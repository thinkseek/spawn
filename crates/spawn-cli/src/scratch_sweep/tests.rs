use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::{
    Args, EXCUSED, Inputs, Proof, Stage, args, digest, in_use, mapped, snapshot, sweep, under,
};
use crate::scratch_owner::{Claim, OWNER_FILE};

const HOUR_MS: i64 = 3_600_000;
const SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55\n# a test's\n";
/// The fake /proc's boot time, in seconds.
const BTIME: i64 = 1_000;

/// A scratch directory of the test's own, resolved, removed when the test
/// ends. Never under the real home: the sweep removes there.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let root = crate::storage::tests::scratch(tag);
        if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
            assert!(
                !root.starts_with(&home),
                "a sweep test's scratch must not be in the real home: {}",
                root.display()
            );
        }
        let root = std::fs::canonicalize(&root).expect("resolve");
        std::fs::create_dir_all(root.join("proc")).expect("a /proc of its own");
        std::fs::write(root.join("proc/stat"), format!("cpu 0\nbtime {BTIME}\n")).expect("stat");
        Scratch(root)
    }

    fn proc(&self) -> PathBuf {
        self.0.join("proc")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fill(path: &Path, bytes: usize) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    std::fs::write(path, vec![0_u8; bytes]).expect("write");
}

fn tag(dir: &Path) {
    std::fs::create_dir_all(dir).expect("mkdir");
    std::fs::write(dir.join("CACHEDIR.TAG"), SIGNATURE).expect("tag");
}

/// A folder with sources and a tagged, filled build path `build`.
fn folder_with(folder: &Path, build: &str) {
    fill(&folder.join("src/main.rs"), 100);
    fill(&folder.join(build).join("debug/x"), 3000);
    tag(&folder.join(build));
}

fn now() -> i64 {
    crate::verify::now_ms()
}

/// A claim of `objective`, made an hour ago and released (unless `lease`
/// says when it ends), written to `folder`'s owner file.
fn claim(folder: &Path, objective: &str, build: &[&str], lease: Option<i64>) -> Claim {
    let claim = Claim {
        agent: "rev-g".to_string(),
        objective: objective.to_string(),
        created_ms: now() - HOUR_MS,
        lease_until_ms: lease.unwrap_or(now() - 1000),
        build: build.iter().map(ToString::to_string).collect(),
    };
    std::fs::write(folder.join(OWNER_FILE), format!("{}\n", claim.line())).expect("owner file");
    claim
}

/// A fake process under `proc`: `cwd` a symlink to its cwd, or (`None`) a
/// regular file, so that it cannot be read; `start` its stat's start
/// ticks (`None`: a stat that does not parse).
struct Fake<'a> {
    pid: u32,
    uid: u32,
    start: Option<u64>,
    comm: &'a str,
    cwd: Option<&'a Path>,
    fds: &'a [&'a Path],
    maps: &'a [&'a str],
}

impl Fake<'_> {
    fn readable(pid: u32, cwd: &Path) -> Fake<'_> {
        Fake {
            pid,
            uid: uid(),
            start: Some(5),
            comm: "sh",
            cwd: Some(cwd),
            fds: &[],
            maps: &[],
        }
    }

    fn unreadable(pid: u32, comm: &str, start: Option<u64>) -> Fake<'_> {
        Fake {
            pid,
            uid: uid(),
            start,
            comm,
            cwd: None,
            fds: &[],
            maps: &[],
        }
    }

    fn make(&self, proc: &Path) {
        let dir = proc.join(self.pid.to_string());
        std::fs::create_dir_all(dir.join("fd")).expect("mkdir");
        let uid = self.uid;
        std::fs::write(
            dir.join("status"),
            format!("Name:\t{}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n", self.comm),
        )
        .expect("status");
        let stat = self.start.map_or("garbage".to_string(), |start| {
            format!(
                "{} ({}) S{} {start} 0 0\n",
                self.pid,
                self.comm,
                " 0".repeat(18)
            )
        });
        std::fs::write(dir.join("stat"), stat).expect("stat");
        std::fs::write(dir.join("comm"), format!("{}\n", self.comm)).expect("comm");
        match self.cwd {
            Some(cwd) => std::os::unix::fs::symlink(cwd, dir.join("cwd")).expect("cwd"),
            None => std::fs::write(dir.join("cwd"), "").expect("cwd"),
        }
        std::os::unix::fs::symlink("/", dir.join("root")).expect("root");
        for (fd, path) in self.fds.iter().enumerate() {
            std::os::unix::fs::symlink(path, dir.join("fd").join(fd.to_string())).expect("fd");
        }
        let maps: Vec<String> = self
            .maps
            .iter()
            .map(|path| format!("7f00-7f10 r-xp 00000000 fd:01 1234      {path}\n"))
            .collect();
        std::fs::write(dir.join("maps"), maps.concat()).expect("maps");
    }
}

fn uid() -> u32 {
    spawn_host::process::current_uid()
}

/// When a fake process of `start` ticks started, in ms.
fn started(start: u64) -> i64 {
    BTIME * 1000 + i64::try_from(start).expect("ticks") * 10
}

/// One sweep's exit (or refusal) and its lines.
fn run(
    scratch: &Scratch,
    folders: &[(PathBuf, Claim)],
    words: &[&str],
    hook: &dyn Fn(Stage, &Path),
) -> (Result<i32, String>, Vec<String>) {
    let words: Vec<String> = words.iter().map(ToString::to_string).collect();
    let args = args(&words).expect("arguments");
    let proc = scratch.proc();
    let inputs = Inputs {
        folders,
        landed: &[(1, 7), (1, 9)],
        proc_root: &proc,
        uid: uid(),
    };
    let mut lines = Vec::new();
    let result = sweep(&inputs, &args, hook, &mut |line| lines.push(line));
    (result, lines)
}

fn none(_: Stage, _: &Path) {}

fn digest_of(lines: &[String]) -> String {
    lines
        .iter()
        .find_map(|line| line.strip_prefix("digest\t"))
        .expect("a digest")
        .to_string()
}

/// The print-only list, then `--apply` with its digest.
fn listed_then_applied(
    scratch: &Scratch,
    folders: &[(PathBuf, Claim)],
    hook: &dyn Fn(Stage, &Path),
) -> (Vec<String>, Result<i32, String>, Vec<String>) {
    let (result, listed) = run(scratch, folders, &[], &none);
    assert_eq!(result, Ok(0), "{listed:?}");
    let digest = digest_of(&listed);
    let (result, applied) = run(
        scratch,
        folders,
        &["--apply", "--expect-digest", &digest],
        hook,
    );
    (listed, result, applied)
}

fn line_for<'a>(lines: &'a [String], path: &Path) -> &'a str {
    let field = format!("\t{}\t", path.display());
    lines
        .iter()
        .find(|line| line.contains(&field) && !line.starts_with("excused\t"))
        .unwrap_or_else(|| panic!("no line for {}: {lines:?}", path.display()))
}

#[test]
fn each_rule_alone_keeps_its_path() {
    let scratch = Scratch::new("sweep-rules");
    let s = scratch.0.join("s");
    let outside = scratch.0.join("outside");
    fill(&outside.join("target/secret"), 9000);
    tag(&outside.join("target"));
    let mut folders = Vec::new();
    // No tag.
    fill(&s.join("notag/target/debug/x"), 3000);
    std::fs::create_dir_all(s.join("notag")).expect("mkdir");
    folders.push((
        s.join("notag"),
        claim(&s.join("notag"), "1/007", &["target"], None),
    ));
    // A transcript under it.
    folder_with(&s.join("jsonl"), "target");
    fill(&s.join("jsonl/target/debug/session.jsonl"), 10);
    folders.push((
        s.join("jsonl"),
        claim(&s.join("jsonl"), "1/007", &["target"], None),
    ));
    // Through a symlink.
    std::fs::create_dir_all(s.join("link")).expect("mkdir");
    std::os::unix::fs::symlink(&outside, s.join("link/x")).expect("symlink");
    folders.push((
        s.join("link"),
        claim(&s.join("link"), "1/007", &["x/target"], None),
    ));
    // A file named target.
    fill(&s.join("file/target"), 10);
    folders.push((
        s.join("file"),
        claim(&s.join("file"), "1/007", &["target"], None),
    ));
    // Not there.
    std::fs::create_dir_all(s.join("gone")).expect("mkdir");
    folders.push((
        s.join("gone"),
        claim(&s.join("gone"), "1/007", &["target"], None),
    ));
    // Not a build path (a claim no owner file could hold).
    folder_with(&s.join("odd"), "target");
    let mut odd = claim(&s.join("odd"), "1/007", &["target"], None);
    odd.build = vec!["../odd/target".to_string()];
    folders.push((s.join("odd"), odd));
    // In use.
    folder_with(&s.join("used"), "target");
    folders.push((
        s.join("used"),
        claim(&s.join("used"), "1/007", &["target"], None),
    ));
    Fake::readable(4242, &s.join("used/target/debug")).make(&scratch.proc());

    let (listed, result, applied) = listed_then_applied(&scratch, &folders, &none);
    assert_eq!(result, Ok(0), "{applied:?}");
    for lines in [&listed, &applied] {
        let why = |path: &Path| line_for(lines, path).to_string();
        assert!(why(&s.join("notag/target")).ends_with(": no CACHEDIR.TAG"));
        assert!(
            why(&s.join("jsonl/target")).ends_with(&format!(
                ": {} is a transcript",
                s.join("jsonl/target/debug/session.jsonl").display()
            )),
            "{}",
            why(&s.join("jsonl/target"))
        );
        assert!(
            why(&s.join("link/x/target"))
                .ends_with(&format!(": {} is a symlink", s.join("link/x").display()))
        );
        assert!(why(&s.join("file/target")).ends_with(&format!(
            ": {} is not a directory",
            s.join("file/target").display()
        )));
        assert!(why(&s.join("gone/target")).ends_with(": not there"));
        assert!(why(&s.join("odd/../odd/target")).ends_with(": not a build path"));
        assert!(why(&s.join("used/target")).ends_with(": in use: process 4242's cwd"));
        assert!(
            lines.iter().all(|line| line.starts_with("stays\t")
                || !line.contains('\t')
                || line.starts_with("digest\t")),
            "{lines:?}"
        );
    }
    assert_eq!(
        applied.last().map(String::as_str),
        Some("removed 0 build path(s), 0 bytes")
    );
    assert!(outside.join("target/secret").exists());
    for kept in [
        "notag/target/debug/x",
        "jsonl/target/debug/session.jsonl",
        "used/target/debug/x",
        "odd/target/debug/x",
    ] {
        assert!(s.join(kept).exists(), "{kept}");
    }
}

#[test]
fn only_a_landed_claim_with_an_ended_lease_goes_and_print_only_removes_nothing() {
    let scratch = Scratch::new("sweep-landed");
    let s = scratch.0.join("s");
    folder_with(&s.join("go"), "a/target");
    folder_with(&s.join("lease"), "target");
    folder_with(&s.join("notl"), "target");
    let folders = vec![
        (
            s.join("go"),
            claim(&s.join("go"), "1/007", &["a/target"], None),
        ),
        (
            s.join("lease"),
            claim(
                &s.join("lease"),
                "1/009",
                &["target"],
                Some(now() + HOUR_MS),
            ),
        ),
        (
            s.join("notl"),
            claim(&s.join("notl"), "1/008", &["target"], None),
        ),
    ];
    let (listed, result, applied) = listed_then_applied(&scratch, &folders, &none);
    assert!(line_for(&listed, &s.join("go/a/target")).starts_with("would-remove\t"));
    assert!(line_for(&listed, &s.join("lease/target")).ends_with("rev-g 1/009: lease runs"));
    assert!(line_for(&listed, &s.join("notl/target")).ends_with("rev-g 1/008: not landed"));
    let bytes = 3000 + SIGNATURE.len();
    assert!(listed.contains(&format!(
        "would remove (print only; nothing removed) 1 build path(s), {bytes} bytes"
    )));
    assert_eq!(result, Ok(0), "{applied:?}");
    assert_eq!(
        line_for(&applied, &s.join("go/a/target")),
        format!(
            "removed\t{}\t{bytes}\trev-g 1/007",
            s.join("go/a/target").display()
        )
    );
    assert_eq!(
        applied.last().cloned(),
        Some(format!("removed 1 build path(s), {bytes} bytes"))
    );
    assert!(!s.join("go/a/target").exists());
    // The removal keeps the sources, the folder, the build path's parent
    // and the owner file.
    assert!(s.join("go/src/main.rs").exists());
    assert!(s.join("go/a").is_dir());
    assert!(s.join("go").join(OWNER_FILE).exists());
    assert!(s.join("lease/target/debug/x").exists());
    assert!(s.join("notl/target/debug/x").exists());
}

#[test]
fn print_only_removes_nothing() {
    let scratch = Scratch::new("sweep-print");
    let s = scratch.0.join("s");
    folder_with(&s.join("go"), "target");
    let folders = vec![(
        s.join("go"),
        claim(&s.join("go"), "1/007", &["target"], None),
    )];
    let (result, listed) = run(&scratch, &folders, &[], &none);
    assert_eq!(result, Ok(0));
    assert!(line_for(&listed, &s.join("go/target")).starts_with("would-remove\t"));
    assert!(s.join("go/target/debug/x").exists());
    assert!(
        listed
            .last()
            .is_some_and(|line| line.starts_with("digest\t"))
    );
}

#[test]
fn a_claim_changed_between_list_and_removal_stays() {
    type Change = fn(&Path, &Claim);
    let renewed: Change = |folder, claim| {
        let mut renewed = claim.clone();
        renewed.lease_until_ms = now() + HOUR_MS;
        std::fs::write(folder.join(OWNER_FILE), renewed.line()).expect("renew");
    };
    let gone: Change = |folder, _| std::fs::remove_file(folder.join(OWNER_FILE)).expect("gone");
    let made_again: Change = |folder, claim| {
        let mut again = claim.clone();
        again.created_ms += 1;
        std::fs::write(folder.join(OWNER_FILE), again.line()).expect("again");
    };
    for (name, change) in [
        ("renewed", renewed),
        ("gone", gone),
        ("made again", made_again),
    ] {
        let scratch = Scratch::new("sweep-claim");
        let s = scratch.0.join("s");
        folder_with(&s.join("go"), "target");
        let made = claim(&s.join("go"), "1/007", &["target"], None);
        let folders = vec![(s.join("go"), made.clone())];
        let folder = s.join("go");
        let hook = |stage: Stage, _: &Path| {
            if stage == Stage::Digested {
                change(&folder, &made);
            }
        };
        let (_, result, applied) = listed_then_applied(&scratch, &folders, &hook);
        assert_eq!(result, Ok(0), "{name}");
        assert!(
            line_for(&applied, &s.join("go/target")).ends_with(": its claim changed"),
            "{name}: {applied:?}"
        );
        assert!(s.join("go/target/debug/x").exists(), "{name}");
    }
}

/// rev-g's swap test: a component swapped for a symlink to a target
/// outside while the re-judgement's in-use proof runs.
#[test]
fn a_component_swapped_for_a_symlink_during_the_in_use_proof_removes_nothing_outside() {
    let scratch = Scratch::new("sweep-swap");
    let s = scratch.0.join("s");
    let outside = scratch.0.join("outside");
    fill(&outside.join("target/secret"), 3000);
    tag(&outside.join("target"));
    folder_with(&s.join("go"), "x/target");
    let folders = vec![(
        s.join("go"),
        claim(&s.join("go"), "1/007", &["x/target"], None),
    )];
    let go = s.join("go");
    let hook = |stage: Stage, _: &Path| {
        if stage == Stage::InUse {
            std::fs::rename(go.join("x"), go.join("x.judged")).expect("move");
            std::os::unix::fs::symlink(&outside, go.join("x")).expect("symlink");
        }
    };
    let (_, result, applied) = listed_then_applied(&scratch, &folders, &hook);
    assert_eq!(result, Ok(0), "{applied:?}");
    assert!(
        line_for(&applied, &s.join("go/x/target")).ends_with(": its path changed"),
        "{applied:?}"
    );
    assert!(outside.join("target/secret").exists());
    assert!(outside.join("target/CACHEDIR.TAG").exists());
    assert!(go.join("x.judged/target/debug/x").exists());
}

/// cc-astra's A58: the build path's directory replaced by another real one
/// right after the judged one was opened: the replacement is never
/// touched, and the run says the name no longer holds the judged one.
#[test]
fn a_replacement_after_the_judged_directory_is_opened_is_never_removed() {
    let scratch = Scratch::new("sweep-a58");
    let s = scratch.0.join("s");
    folder_with(&s.join("go"), "target");
    let folders = vec![(
        s.join("go"),
        claim(&s.join("go"), "1/007", &["target"], None),
    )];
    let go = s.join("go");
    let replace = || {
        std::fs::rename(go.join("target"), go.join("judged")).expect("move");
        fill(&go.join("target/sentinel"), 3000);
        fill(&go.join("target/session.jsonl"), 10);
    };
    let hook = |stage: Stage, _: &Path| {
        if stage == Stage::Opened {
            replace();
        }
    };
    let (_, result, applied) = listed_then_applied(&scratch, &folders, &hook);
    assert_eq!(result, Ok(1), "{applied:?}");
    let line = line_for(&applied, &go.join("target"));
    assert!(line.starts_with("failed\t"), "{line}");
    assert!(
        line.ends_with(": its contents were removed, but its name no longer holds it: the empty directory is left"),
        "{line}"
    );
    assert!(go.join("target/sentinel").exists());
    assert!(go.join("target/session.jsonl").exists());
    // The judged directory, emptied, under the name it was moved to.
    assert_eq!(
        std::fs::read_dir(go.join("judged")).expect("read").count(),
        0
    );

    // The same replacement after the re-judgement's checks: its path changed.
    let scratch = Scratch::new("sweep-a58b");
    let s = scratch.0.join("s");
    folder_with(&s.join("go"), "target");
    let folders = vec![(
        s.join("go"),
        claim(&s.join("go"), "1/007", &["target"], None),
    )];
    let go = s.join("go");
    let hook = |stage: Stage, _: &Path| {
        if stage == Stage::InUse {
            std::fs::rename(go.join("target"), go.join("judged")).expect("move");
            fill(&go.join("target/sentinel"), 3000);
        }
    };
    let (_, result, applied) = listed_then_applied(&scratch, &folders, &hook);
    assert_eq!(result, Ok(0), "{applied:?}");
    assert!(line_for(&applied, &go.join("target")).ends_with(": its path changed"));
    assert!(go.join("target/sentinel").exists());
    assert!(go.join("judged/debug/x").exists());
}

/// cc-astra's A59: the path grown between --apply's digest check and its
/// re-judgement stays, and the new file is kept.
#[test]
fn a_path_grown_after_the_digest_check_stays_changed_since_the_list() {
    let scratch = Scratch::new("sweep-a59");
    let s = scratch.0.join("s");
    folder_with(&s.join("go"), "target");
    let folders = vec![(
        s.join("go"),
        claim(&s.join("go"), "1/007", &["target"], None),
    )];
    let new = s.join("go/target/debug/new");
    let hook = |stage: Stage, _: &Path| {
        if stage == Stage::Digested {
            fill(&new, 1);
        }
    };
    let (_, result, applied) = listed_then_applied(&scratch, &folders, &hook);
    assert_eq!(result, Ok(0));
    assert!(line_for(&applied, &s.join("go/target")).ends_with(": changed since the list"));
    assert!(new.exists());
}

#[test]
fn rg_bytes_added_after_the_digest_keep_the_path() {
    let scratch = Scratch::new("sweep-rg-bytes");
    let s = scratch.0.join("s");
    folder_with(&s.join("go"), "target");
    let folders = vec![(
        s.join("go"),
        claim(&s.join("go"), "1/007", &["target"], None),
    )];
    let x = s.join("go/target/debug/x");
    let hook = |stage: Stage, _: &Path| {
        if stage == Stage::Digested {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(&x)
                .and_then(|mut file| file.write_all(b"more"))
                .expect("grow");
        }
    };
    let (_, result, applied) = listed_then_applied(&scratch, &folders, &hook);
    assert_eq!(result, Ok(0));
    let line = line_for(&applied, &s.join("go/target"));
    assert!(line.starts_with("stays\t"), "{line}");
    assert!(line.ends_with(": changed since the list"), "{line}");
    assert_eq!(std::fs::metadata(&x).expect("kept").len(), 3004);
}

#[test]
fn a_transcript_put_in_the_judged_directory_during_removal_is_never_removed() {
    let scratch = Scratch::new("sweep-late-jsonl");
    let s = scratch.0.join("s");
    folder_with(&s.join("go"), "target");
    let folders = vec![(
        s.join("go"),
        claim(&s.join("go"), "1/007", &["target"], None),
    )];
    let late = s.join("go/target/debug/late.jsonl");
    let hook = |stage: Stage, _: &Path| {
        if stage == Stage::Opened {
            fill(&late, 10);
        }
    };
    let (_, result, applied) = listed_then_applied(&scratch, &folders, &hook);
    assert_eq!(result, Ok(1), "{applied:?}");
    let line = line_for(&applied, &s.join("go/target"));
    assert!(line.starts_with("failed\t"), "{line}");
    assert!(
        line.ends_with(&format!(
            ": {} is a transcript: it is never removed",
            late.display()
        )),
        "{line}"
    );
    assert!(late.exists());
    assert_eq!(
        applied.last().map(String::as_str),
        Some("removed 0 build path(s), 0 bytes")
    );
}

#[test]
fn excused_processes_are_printed() {
    let scratch = Scratch::new("sweep-excused");
    let s = scratch.0.join("s");
    folder_with(&s.join("go"), "target");
    let folders = vec![(
        s.join("go"),
        claim(&s.join("go"), "1/007", &["target"], None),
    )];
    assert_eq!(EXCUSED, ["(sd-pam)"]);
    Fake::unreadable(7, "(sd-pam)", Some(3)).make(&scratch.proc());
    let (listed, result, applied) = listed_then_applied(&scratch, &folders, &none);
    let excused = format!(
        "excused\t{}\tprocess 7 (sd-pam): cannot be read, started before the claim",
        s.join("go/target").display()
    );
    assert_eq!(
        listed.first().map(String::as_str),
        Some("unreadable\t7:3\t(sd-pam)")
    );
    assert!(listed.contains(&excused), "{listed:?}");
    assert!(line_for(&listed, &s.join("go/target")).starts_with("would-remove\t"));
    assert_eq!(result, Ok(0));
    assert!(applied.contains(&excused), "{applied:?}");
    assert!(!s.join("go/target").exists());
}

/// The in-use proof on a /proc of the test's own.
#[test]
fn the_in_use_proof_reads_a_proc_of_its_own() {
    let scratch = Scratch::new("sweep-proof");
    let target = scratch.0.join("s/go/target");
    let near = scratch.0.join("s/go/target2");
    std::fs::create_dir_all(&target).expect("mkdir");
    std::fs::create_dir_all(&near).expect("mkdir");
    let created = started(100);
    let proof = |fakes: &[Fake], excuses: &[(u32, u64)]| {
        let proc = scratch.0.join(format!("proc-{}", fakes.len()));
        let _ = std::fs::remove_dir_all(&proc);
        std::fs::create_dir_all(&proc).expect("mkdir");
        std::fs::write(proc.join("stat"), format!("btime {BTIME}\n")).expect("stat");
        for fake in fakes {
            fake.make(&proc);
        }
        in_use(
            &target,
            uid(),
            created,
            &snapshot(&proc).expect("snapshot"),
            excuses,
        )
    };
    // A cwd, and a mapping of a deleted file.
    assert_eq!(
        proof(&[Fake::readable(10, &target.join("debug"))], &[]),
        Proof::Used("in use: process 10's cwd".to_string())
    );
    let deleted = format!(
        "{}/debug/deps/libserde_derive.so (deleted)",
        target.display()
    );
    let mapping = Fake {
        maps: &[deleted.as_str()],
        ..Fake::readable(11, Path::new("/"))
    };
    assert_eq!(
        proof(&[mapping], &[]),
        Proof::Used("in use: process 11's mapping".to_string())
    );
    let fd_path = target.join("debug/x");
    let fd = Fake {
        fds: &[&fd_path],
        ..Fake::readable(12, Path::new("/"))
    };
    assert_eq!(
        proof(&[fd], &[]),
        Proof::Used("in use: process 12's open file".to_string())
    );
    // A near name is not under the path.
    let near_file = near.join("x");
    let near_use = Fake {
        fds: &[&near_file],
        maps: &[],
        ..Fake::readable(13, &near)
    };
    assert_eq!(proof(&[near_use], &[]), Proof::Free(Vec::new()));
    // An unreadable process of the owner.
    assert_eq!(
        proof(&[Fake::unreadable(20, "(sd-pam)", Some(50))], &[]),
        Proof::Free(vec![(20, "(sd-pam)".to_string())]),
        "excused by name, started before the claim"
    );
    assert_eq!(
        proof(&[Fake::unreadable(20, "(sd-pam)", Some(150))], &[]),
        Proof::Used(
            "in use unknown: process 20 ((sd-pam)) of its owner cannot be read".to_string()
        ),
        "started since the claim"
    );
    assert_eq!(
        proof(&[Fake::unreadable(21, "gpg-agent", Some(50))], &[]),
        Proof::Used(
            "in use unknown: process 21 (gpg-agent) of its owner cannot be read".to_string()
        ),
        "not listed, started before"
    );
    assert_eq!(
        proof(&[Fake::unreadable(22, "(sd-pam)", None)], &[]),
        Proof::Used(
            "in use unknown: process 22 ((sd-pam)) of its owner cannot be read".to_string()
        ),
        "an unknown start"
    );
    // An exact --excuse pair passes; another start does not.
    assert_eq!(
        proof(&[Fake::unreadable(21, "gpg-agent", Some(50))], &[(21, 50)]),
        Proof::Free(Vec::new())
    );
    assert!(matches!(
        proof(&[Fake::unreadable(21, "gpg-agent", Some(50))], &[(21, 51)]),
        Proof::Used(_)
    ));
    // A definite use ahead of an unknown one, whatever the order.
    assert_eq!(
        proof(
            &[
                Fake::unreadable(5, "python3", Some(50)),
                Fake::readable(90, &target)
            ],
            &[]
        ),
        Proof::Used("in use: process 90's cwd".to_string())
    );
    // Another user's unreadable process is not counted.
    let other = Fake {
        uid: uid() + 1,
        ..Fake::unreadable(30, "python3", Some(50))
    };
    assert_eq!(proof(&[other], &[]), Proof::Free(Vec::new()));
}

#[test]
fn proc_paths_compare_by_component() {
    assert!(under(Path::new("/a/target"), Path::new("/a/target")));
    assert!(under(
        Path::new("/a/target/x (deleted)"),
        Path::new("/a/target")
    ));
    assert!(!under(Path::new("/a/target2"), Path::new("/a/target")));
    assert!(!under(
        Path::new("/a/target2 (deleted)"),
        Path::new("/a/target")
    ));
    assert_eq!(
        mapped(b"7f00-7f10 r-xp 00000000 fd:01 1234      /a/b c.so (deleted)"),
        Some(PathBuf::from("/a/b c.so (deleted)"))
    );
    assert_eq!(mapped(b"7f00-7f10 rw-p 00000000 00:00 0 "), None);
    assert_eq!(mapped(b"7f00-7f10 rw-p 00000000 00:00 0      [heap]"), None);
}

/// The unreadable listing names this uid's processes only, each once,
/// the excused set first; an excuse that names none refuses the run.
#[test]
fn the_unreadable_listing_is_of_this_uid_only() {
    let scratch = Scratch::new("sweep-listing");
    let s = scratch.0.join("s");
    folder_with(&s.join("go"), "target");
    let folders = vec![(
        s.join("go"),
        claim(&s.join("go"), "1/007", &["target"], None),
    )];
    Fake::unreadable(40, "gpg-agent", Some(9)).make(&scratch.proc());
    Fake::unreadable(41, "python3", Some(10)).make(&scratch.proc());
    Fake {
        uid: uid() + 1,
        ..Fake::unreadable(42, "other", Some(11))
    }
    .make(&scratch.proc());
    let (result, listed) = run(&scratch, &folders, &["--excuse", "41:10"], &none);
    assert_eq!(result, Ok(0));
    assert_eq!(listed[0], "excuse\t41:10\tpython3");
    assert_eq!(listed[1], "unreadable\t40:9\tgpg-agent");
    assert!(
        listed.iter().all(|line| !line.contains("other")),
        "{listed:?}"
    );
    assert!(
        line_for(&listed, &s.join("go/target")).contains("in use unknown: process 40 (gpg-agent)")
    );
    let (result, listed) = run(
        &scratch,
        &folders,
        &["--excuse", "41:10", "--excuse", "40:9"],
        &none,
    );
    assert_eq!(result, Ok(0));
    assert!(line_for(&listed, &s.join("go/target")).starts_with("would-remove\t"));
    for stale in ["41:11", "42:11", "43:1"] {
        let (result, lines) = run(&scratch, &folders, &["--excuse", stale], &none);
        let error = result.expect_err(stale);
        assert!(
            error.ends_with("is no unreadable process of yours running now; nothing removed"),
            "{error}"
        );
        assert!(lines.is_empty());
    }
    assert!(s.join("go/target/debug/x").exists());
}

/// The real /proc: nothing uses a fresh directory (every unreadable
/// process of ours excused), and this process's open file does.
#[test]
fn the_real_proc_sees_this_process_open_file() {
    let scratch = Scratch::new("sweep-real-proc");
    let fresh = scratch.0.join("fresh");
    std::fs::create_dir_all(&fresh).expect("mkdir");
    let procs = snapshot(Path::new("/proc")).expect("/proc");
    let excuses: Vec<(u32, u64)> = procs
        .list
        .iter()
        .filter(|process| process.uses.is_none() && process.uid == Some(uid()))
        .filter_map(|process| Some((process.pid, process.start?)))
        .collect();
    let unknown_start = procs.list.iter().any(|process| {
        process.uses.is_none() && process.uid == Some(uid()) && process.start.is_none()
    });
    if !unknown_start {
        assert_eq!(
            in_use(&fresh, uid(), now(), &procs, &excuses),
            Proof::Free(Vec::new())
        );
    }
    let _held = std::fs::File::create(fresh.join("held")).expect("open");
    let procs = snapshot(Path::new("/proc")).expect("/proc");
    assert_eq!(
        in_use(&fresh, uid(), now(), &procs, &excuses),
        Proof::Used(format!(
            "in use: process {}'s open file",
            std::process::id()
        ))
    );
}

/// The digest covers what would go (each row: path, bytes, owner, ids,
/// claim) and the excuses, not what stays.
#[test]
fn the_digest_is_of_what_would_go_and_the_excuses() {
    let scratch = Scratch::new("sweep-digest");
    let s = scratch.0.join("s");
    folder_with(&s.join("go"), "target");
    folder_with(&s.join("notl"), "target");
    let go = (
        s.join("go"),
        claim(&s.join("go"), "1/007", &["target"], None),
    );
    let notl = (
        s.join("notl"),
        claim(&s.join("notl"), "1/008", &["target"], None),
    );
    let (_, alone) = run(&scratch, std::slice::from_ref(&go), &[], &none);
    let (_, both) = run(&scratch, &[go.clone(), notl], &[], &none);
    assert_eq!(
        digest_of(&alone),
        digest_of(&both),
        "what stays is not in it"
    );
    let rows: Vec<String> = alone
        .iter()
        .filter(|line| line.starts_with("would-remove\t"))
        .cloned()
        .collect();
    assert_eq!(rows.len(), 1);
    let metadata = |path: &Path| std::fs::metadata(path).expect("stat");
    let ids = format!(
        "{}:{},{}:{}",
        metadata(&s.join("go")).dev(),
        metadata(&s.join("go")).ino(),
        metadata(&s.join("go/target")).dev(),
        metadata(&s.join("go/target")).ino()
    );
    use sha2::Digest as _;
    let line = std::fs::read_to_string(s.join("go").join(OWNER_FILE)).expect("owner");
    let claim_sha = super::hex(&sha2::Sha256::digest(line.trim_end().as_bytes()));
    assert_eq!(
        rows[0],
        format!(
            "would-remove\t{}\t{}\trev-g 1/007\tids {ids}\tclaim {claim_sha}",
            s.join("go/target").display(),
            3000 + SIGNATURE.len()
        )
    );
    assert_eq!(digest_of(&alone), digest(&rows, &[]));
    assert_ne!(
        digest(&rows, &[]),
        digest(&rows, &[(1, 2)]),
        "the excuses are in it"
    );
    assert_ne!(digest(&rows, &[]), digest(&[], &[]));
    assert_eq!(
        digest(&[], &[]),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    Fake::unreadable(41, "python3", Some(10)).make(&scratch.proc());
    let (_, excused) = run(
        &scratch,
        std::slice::from_ref(&go),
        &["--excuse", "41:10"],
        &none,
    );
    assert_eq!(digest_of(&excused), digest(&rows, &[(41, 10)]));
}

/// cc-astra's A64: another tagged directory of the same size put at the
/// path between two runs, then (put back) the claim made again with the
/// same owner shown: --apply with the first digest is refused both times.
#[test]
fn a_same_size_replacement_or_a_new_claim_between_runs_is_another_digest() {
    let scratch = Scratch::new("sweep-a64");
    let s = scratch.0.join("s");
    folder_with(&s.join("go"), "target");
    let made = claim(&s.join("go"), "1/007", &["target"], None);
    let folders = vec![(s.join("go"), made.clone())];
    let (_, listed) = run(&scratch, &folders, &[], &none);
    let first = digest_of(&listed);
    let go = s.join("go");
    std::fs::rename(go.join("target"), go.join("kept")).expect("move");
    fill(&go.join("target/debug/x"), 3000);
    tag(&go.join("target"));
    let apply = ["--apply", "--expect-digest", first.as_str()];
    let (result, lines) = run(&scratch, &folders, &apply, &none);
    let error = result.expect_err("another directory");
    assert!(
        error.starts_with("the list is not the one read (its digest is now "),
        "{error}"
    );
    assert!(error.ends_with("); nothing removed"));
    assert!(lines.is_empty());
    assert!(go.join("target/debug/x").exists());
    std::fs::remove_dir_all(go.join("target")).expect("the replacement");
    std::fs::rename(go.join("kept"), go.join("target")).expect("back");
    let mut again = made.clone();
    again.created_ms += 1;
    std::fs::write(go.join(OWNER_FILE), again.line()).expect("claimed again");
    let folders = vec![(s.join("go"), again)];
    let (result, _) = run(&scratch, &folders, &apply, &none);
    assert!(
        result
            .expect_err("a new claim")
            .contains("is not the one read")
    );
    assert!(go.join("target/debug/x").exists());
}

#[test]
fn apply_with_a_digest_not_of_the_list_removes_nothing() {
    let scratch = Scratch::new("sweep-wrong-digest");
    let s = scratch.0.join("s");
    folder_with(&s.join("go"), "target");
    let folders = vec![(
        s.join("go"),
        claim(&s.join("go"), "1/007", &["target"], None),
    )];
    let empty = digest(&[], &[]);
    for wrong in ["0000", empty.as_str()] {
        let (result, lines) = run(
            &scratch,
            &folders,
            &["--apply", "--expect-digest", wrong],
            &none,
        );
        assert!(result.expect_err(wrong).contains("is not the one read"));
        assert!(lines.is_empty());
        assert!(s.join("go/target/debug/x").exists());
    }
}

#[test]
fn the_arguments() {
    let parse = |words: &[&str]| args(&words.iter().map(ToString::to_string).collect::<Vec<_>>());
    assert_eq!(parse(&[]), Ok(Args::default()));
    assert!(
        parse(&["--apply"])
            .expect_err("alone")
            .contains("go together")
    );
    assert!(
        parse(&["--expect-digest", "ab"])
            .expect_err("alone")
            .contains("go together")
    );
    assert_eq!(
        parse(&["--apply", "--expect-digest", "ab", "--excuse", "7:3"]),
        Ok(Args {
            excuses: vec![(7, 3)],
            expect: Some("ab".to_string())
        })
    );
    assert_eq!(
        parse(&["--excuse", "9:1", "--excuse", "7:3"]),
        Ok(Args {
            excuses: vec![(7, 3), (9, 1)],
            expect: None
        })
    );
    for bad in ["7", "7:", ":3", "a:3", "7:3:1", "-7:3", "7: 3"] {
        assert!(parse(&["--excuse", bad]).is_err(), "{bad}");
        assert!(
            parse(&["--apply", "--expect-digest", "ab", "--excuse", bad]).is_err(),
            "{bad}"
        );
    }
    assert!(parse(&["--excuse"]).is_err());
    assert!(parse(&["--apply", "--apply", "--expect-digest", "ab"]).is_err());
    assert!(parse(&["--whatever"]).is_err());
}
