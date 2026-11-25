//! PV3a on a real git repository: the landing hook writes the rows
//! `spawn prov index` writes, and a hook that fails leaves them to it
//! (a test database, `SPAWN_TEST_DATABASE_URL`, a schema of each case's
//! own); `blast`; `who` through a squashed landing's trailers; and the
//! plans, whose printed commands, run by hand in a clone, make the very
//! tree they print, while the repository planned on is left unwritten.
//!
//! The history: an imported first commit (`README.md`, a binary
//! `bin.dat`); task 11's branch `agent_a` (not under `spawn/`, so no walk
//! meets it before its landing) of two checkpoints; its landing, a merge
//! with the integrator's trailers and its `Spawn-Change`; then a commit of
//! main's own on the same file.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use postgres::{Client, NoTls};
use serde_json::{Value, json};
use spawn_postgres::PostgresStore;
use spawn_postgres::testing::TestSchema;

use super::index::{self, Repo};
use super::plan::{self, Pick, View, Wanted};
use super::{Landed, Target, blast, record_land, source_change, who};

const SOURCE: &str = "/srv/prov-land.git";

/// A directory of the test's own, removed when it ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(case: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let path = std::env::temp_dir().join(format!(
            "spawn-prov-land-{case}-{}-{nanos:x}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("a scratch directory");
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// git in `dir`, with fixed times and identity; its trimmed stdout.
fn git_in(dir: &Path, clock: i64, arguments: &[&str]) -> String {
    let date = format!("{clock} +0000");
    let output = spawn_host::test_git_command()
        .arg("-C")
        .arg(dir)
        .args(arguments)
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

struct Work {
    dir: PathBuf,
    clock: i64,
}

impl Work {
    fn git(&self, arguments: &[&str]) -> String {
        git_in(&self.dir, self.clock, arguments)
    }

    fn write(&self, path: &str, bytes: &[u8]) {
        let path = self.dir.join(path);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("directories");
        std::fs::write(path, bytes).expect("write");
    }

    fn commit(&mut self, message: &str) -> String {
        self.clock += 1;
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn git_dir(&self) -> String {
        self.dir.join(".git").display().to_string()
    }
}

struct History {
    import: String,
    a1: String,
    a2: String,
    landing: String,
    message: String,
}

/// The history up to the branch (no landing yet).
fn branch(root: &Path) -> (Work, String, String, String) {
    let dir = root.join("work");
    std::fs::create_dir_all(&dir).expect("the work tree");
    let mut work = Work {
        dir,
        clock: 1_700_000_000,
    };
    work.git(&["init", "-q", "-b", "main"]);
    work.write("README.md", b"the project\n");
    work.write("bin.dat", b"\x00\x01\x02binary\x00");
    let import = work.commit("Initial import");
    work.git(&["checkout", "-q", "-b", "agent_a"]);
    work.write("src/a.txt", b"one\ntwo\n");
    let a1 = work.commit("spawn checkpoint exec_a1");
    work.write("src/a.txt", b"one\nTWO\n");
    work.write("src/b.txt", b"bee\n");
    work.write("bin.dat", b"\x00\x03binary again\x00");
    let a2 = work.commit("spawn checkpoint exec_a2");
    work.git(&["checkout", "-q", "main"]);
    (work, import, a1, a2)
}

/// The landing of task 11, as the integrator writes it (its trailers, the
/// `Spawn-Change` last).
fn land(work: &mut Work, a2: &str) -> (String, String) {
    let message = format!(
        "Add a and b (task 11)\n\nWhat changed\n    Spawn-Task: 99\n\nSpawn-Task: 11\n\
         Spawn-Head: {a2}\nSpawn-Change: {}\n",
        source_change(SOURCE, 11)
    );
    work.clock += 1;
    work.git(&["merge", "-q", "--no-ff", "-m", &message, "agent_a"]);
    (work.git(&["rev-parse", "HEAD"]), message)
}

/// The event log and the mission layer: task 11 (agent_a) and its two
/// executions.
fn facts(client: &mut Client) {
    client
        .batch_execute(
            "insert into mission_missions (id, text, origin, created_at_ms, updated_at_ms)
             overriding system value values (1, 'prov', 'human', 0, 0);
             insert into mission_tasks (id, mission_id, kind, agent_id, session_id, brief,
                                        created_by, created_at_ms, updated_at_ms)
             overriding system value
             values (11, 1, 'ephemeral', 'agent_a', 'session_a', 'a', 'human', 1000, 1000);",
        )
        .expect("the mission layer");
    for (seq, execution) in [(1_i64, "exec_a1"), (2, "exec_a2")] {
        client
            .execute(
                "insert into spawn_events (agent_id, session_id, execution_id, source, kind,
                                           session_seq, recorded_at_ms)
                 values ('agent_a', 'session_a', $1, 'bridge', $2, $3, 1000)",
                &[
                    &execution,
                    &json!({"name": "execution.started", "detail": {}}),
                    &seq,
                ],
            )
            .expect("an execution");
    }
}

fn owner(schema: &TestSchema) -> Client {
    let mut client = Client::connect(schema.url(), NoTls).expect("the owner");
    client
        .batch_execute(&format!("set search_path to {}", schema.name()))
        .expect("the schema");
    client
}

/// Every recorded change's `spawn_prov_canonical`, origins by the
/// `change_id#rev` they name, keyed by `change_id#rev`.
fn canonical(client: &mut Client) -> BTreeMap<String, Value> {
    let names: HashMap<i64, String> = client
        .query("select id, change_id || '#' || rev from prov_changes", &[])
        .expect("the changes")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    client
        .query(
            "select change_id || '#' || rev, spawn_prov_canonical(id) from prov_changes",
            &[],
        )
        .expect("the canonical values")
        .iter()
        .map(|row| {
            let mut value: Value = row.get(1);
            for entry in value["entries"].as_array_mut().into_iter().flatten() {
                if let Some(origin) = entry["origin"].as_i64() {
                    entry["origin"] = json!(names[&origin]);
                }
            }
            (row.get(0), value)
        })
        .collect()
}

fn domain() -> String {
    index::domain_of(SOURCE)
}

fn index_now(store: &PostgresStore, git_dir: &str) -> index::Summary {
    let mut repo = Repo::open(git_dir).expect("the repository");
    index::index(store, &mut repo, &domain()).expect("an index run")
}

/// Decisions B4, verification 2: the hook records, right after the push,
/// exactly the rows a later `spawn prov index` writes when the hook could
/// not (its database unreachable); after the hook the indexer records
/// nothing; the landing's `includes` edge names its `Spawn-Change`.
#[test]
fn the_landing_hook_writes_the_rows_the_indexer_writes_and_a_failed_hook_leaves_them_to_it() {
    let scratch = Scratch::new("hook");
    let (mut work, _import, a1, a2) = branch(&scratch.0);
    let git_dir = work.git_dir();
    let hooked = TestSchema::new("prov_land_hook");
    let failed = TestSchema::new("prov_land_failed");
    let hooked_store = hooked.store();
    let failed_store = failed.store();
    let mut hooked_client = owner(&hooked);
    let mut failed_client = owner(&failed);
    facts(&mut hooked_client);
    facts(&mut failed_client);
    // Main's history before the landing is indexed in both.
    assert_eq!(index_now(&hooked_store, &git_dir).imports, 1);
    assert_eq!(index_now(&failed_store, &git_dir).imports, 1);
    let (landing, _) = land(&mut work, &a2);

    // Not indexed history: the hook leaves it all to the indexer.
    let fresh = TestSchema::new("prov_land_fresh");
    assert_eq!(
        record_land(&fresh.store(), Path::new(&git_dir), SOURCE, &landing),
        Ok(Landed::NotIndexed)
    );

    let recorded = record_land(&hooked_store, Path::new(&git_dir), SOURCE, &landing);
    let Ok(Landed::Recorded(summary)) = recorded else {
        panic!("the hook records: {recorded:?}");
    };
    assert_eq!((summary.edits, summary.lands), (2, 1));
    assert_eq!(
        record_land(&hooked_store, Path::new(&git_dir), SOURCE, &landing),
        Ok(Landed::Already)
    );
    let after_hook = canonical(&mut hooked_client);
    assert_eq!(index_now(&hooked_store, &git_dir).recorded(), 0);
    assert_eq!(canonical(&mut hooked_client), after_hook);

    super::HOOK_UNREACHABLE.with(|down| down.set(true));
    let refused = record_land(&failed_store, Path::new(&git_dir), SOURCE, &landing);
    super::HOOK_UNREACHABLE.with(|down| down.set(false));
    assert!(refused.is_err(), "{refused:?}");
    assert!(
        canonical(&mut failed_client)
            .keys()
            .all(|key| !key.starts_with(&source_change(SOURCE, 11)))
    );
    let later = index_now(&failed_store, &git_dir);
    assert_eq!((later.edits, later.lands), (2, 1));
    assert_eq!(canonical(&mut failed_client), after_hook);

    // The landing includes exactly the Change its trailer names.
    let shown = super::show(&hooked_store, &Target::Commit(landing.clone())).expect("show");
    let change = &shown["changes"][0];
    assert_eq!(change["change"]["kind"], "land");
    assert_eq!(
        change["edges"],
        json!([{"kind": "includes", "dst": source_change(SOURCE, 11),
                "detail": {"select": "all"}, "at_ms": change["change"]["at_ms"]}])
    );
    let checkpoint = super::show(&hooked_store, &Target::Commit(a1)).expect("show");
    assert_eq!(
        checkpoint["changes"][0]["change"]["change_id"],
        source_change(SOURCE, 11)
    );
    assert_eq!(checkpoint["changes"][0]["change"]["attributed"], "host");
}

/// `blast`: the later Changes on a checkpoint's paths, the landing that
/// includes it apart, and the radius; and `who` of a squashed landing
/// (its second parent gone) answers the landing's Change through its
/// trailers, for every path (decisions B4, verification 3).
#[test]
fn blast_names_later_changes_and_who_follows_a_squashed_landings_trailers() {
    let scratch = Scratch::new("blast");
    let (mut work, import, a1, a2) = branch(&scratch.0);
    let git_dir = work.git_dir();
    let schema = TestSchema::new("prov_land_blast");
    let store = schema.store();
    let mut client = owner(&schema);
    facts(&mut client);
    let (landing, message) = land(&mut work, &a2);
    work.write("src/a.txt", b"one\nTWO\nthree\n");
    let own = work.commit("main's own");
    let summary = index_now(&store, &git_dir);
    assert_eq!((summary.edits, summary.lands, summary.imports), (2, 1, 2));

    let answer = blast(&store, &Target::Commit(a1.clone())).expect("blast");
    assert_eq!(answer["indexed"], true);
    let results = |key: &str| -> Vec<String> {
        answer[key]
            .as_array()
            .expect("a list")
            .iter()
            .map(|row| row["change"]["result"].as_str().expect("result").to_string())
            .collect()
    };
    assert_eq!(
        results("later"),
        vec![index::git1(&a2), index::git1(&own)],
        "{answer:#}"
    );
    assert_eq!(results("landed_by"), vec![index::git1(&landing)]);
    assert_eq!(answer["later"][1]["paths"], json!(["src/a.txt"]));
    let radius = answer["radius"].as_array().expect("radius");
    assert!(
        radius
            .iter()
            .any(|node| node["via"] == "includes" && node["depth"] == 1),
        "{answer:#}"
    );
    assert_eq!(answer["used"], json!([]), "no used edge in the Git domain");
    let task = blast(&store, &Target::Task(11)).expect("blast of a task");
    assert_eq!(task["changes"].as_array().map(Vec::len), Some(3));
    let none = blast(&store, &Target::Task(404)).expect("blast of nothing");
    assert_eq!(none["indexed"], false);

    // Squash the landing in a clone: one parent, the same tree and message.
    let clone = scratch.0.join("squashed.git");
    let clone_path = clone.display().to_string();
    git_in(
        &scratch.0,
        1,
        &["clone", "-q", "--bare", &git_dir, &clone_path],
    );
    let tree = git_in(&clone, 1, &["rev-parse", &format!("{landing}^{{tree}}")]);
    let squashed = git_in(
        &clone,
        1_800_000_000,
        &["commit-tree", &tree, "-p", &import, "-m", &message],
    );
    git_in(&clone, 1, &["update-ref", "refs/heads/main", &squashed]);
    let repo = Repo::open(&clone_path).expect("the clone");
    let land_change = super::show(&store, &Target::Commit(landing)).expect("show")["changes"][0]
        ["change"]
        .clone();
    for path in ["src/b.txt", "bin.dat"] {
        let said = who(&store, &repo, &domain(), path).expect("who");
        assert_eq!(said["commit"], squashed.as_str(), "{said:#}");
        assert_eq!(said["via"], "trailers", "{said:#}");
        assert_eq!(said["change"], land_change, "{path}: {said:#}");
        assert_eq!(said["spawn_change"], json!([source_change(SOURCE, 11)]));
        assert_eq!(said["task_id"], 11);
    }
    let line = who(&store, &repo, &domain(), "src/b.txt:1").expect("who of a line");
    assert_eq!(line["change"], land_change);
}

/// Every command of a plan, run in `clone` (a work tree): each must
/// succeed, and so the plan's `test "$(git write-tree)" = <tree>` holds.
fn run_commands(clone: &Path, commands: &Value) {
    for command in commands.as_array().expect("commands") {
        let command = command.as_str().expect("a command");
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(clone)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", clone)
            .output()
            .expect("run sh");
        assert!(
            output.status.success(),
            "{command}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// What a repository holds: its refs and its objects.
fn holdings(git_dir: &str) -> (String, String) {
    let dir = Path::new(git_dir);
    (
        git_in(dir, 1, &["--git-dir", git_dir, "for-each-ref"]),
        git_in(
            dir,
            1,
            &[
                "--git-dir",
                git_dir,
                "cat-file",
                "--batch-all-objects",
                "--batch-check",
            ],
        ),
    )
}

/// Decisions B4, verifications 4 and 5: `revert --plan` is what `git
/// revert -m 1 --no-commit` does by hand, on a landing modified since;
/// `split --plan`'s first tree is the one a hand split builds; `compose
/// --plan` is the cherry-picks; each plan's commands, run in a clone, make
/// its tree; nothing is written in the repository planned on; a part of a
/// file is refused, binary or text, with `granularity: file`.
#[test]
fn plans_print_the_tree_their_commands_make_and_write_nothing() {
    let scratch = Scratch::new("plan");
    let (mut work, import, a1, a2) = branch(&scratch.0);
    let (landing, _) = land(&mut work, &a2);
    work.write("src/a.txt", b"zero\none\nTWO\n");
    let own = work.commit("main's own");
    let git_dir = work.git_dir();
    let before = holdings(&git_dir);
    let view = View::of(&git_dir).expect("a view");
    let landed = Pick {
        label: "landing".to_string(),
        base: import.clone(),
        result: landing.clone(),
        merge: true,
    };

    let reverted = plan::revert(&view, &own, &landed).expect("revert");
    assert_eq!(reverted["conflicts"], json!([]), "{reverted:#}");
    let hand = scratch.0.join("hand");
    let hand_path = hand.display().to_string();
    git_in(&scratch.0, 1, &["clone", "-q", &git_dir, &hand_path]);
    git_in(&hand, 1, &["switch", "-q", "--detach", &own]);
    git_in(&hand, 1, &["revert", "--no-commit", "-m", "1", &landing]);
    assert_eq!(git_in(&hand, 1, &["write-tree"]), reverted["tree"]);
    git_in(&hand, 1, &["reset", "-q", "--hard"]);
    run_commands(&hand, &reverted["commands"]);

    let split = plan::split(&view, &landed, &["src".to_string()]).expect("split");
    let first = &split["parts"][0];
    assert_eq!(first["paths"], json!(["src/a.txt", "src/b.txt"]));
    assert_eq!(split["parts"][1]["paths"], json!(["bin.dat"]));
    git_in(&hand, 1, &["switch", "-q", "--detach", &import]);
    git_in(&hand, 1, &["checkout", &landing, "--", "src"]);
    assert_eq!(git_in(&hand, 1, &["write-tree"]), first["tree"]);
    git_in(&hand, 1, &["reset", "-q", "--hard"]);
    run_commands(&hand, &first["commands"]);
    run_commands(&hand, &split["parts"][1]["commands"]);
    assert_eq!(
        split["parts"][1]["tree"],
        git_in(&hand, 1, &["rev-parse", &format!("{landing}^{{tree}}")])
    );
    assert!(plan::split(&view, &landed, &["nowhere".to_string()]).is_err());
    assert!(
        plan::split(
            &view,
            &landed,
            &["src".to_string(), "bin.dat".to_string()]
        )
        .is_err(),
        "every path: nothing to split"
    );

    let picks = [
        Pick {
            label: "a1".to_string(),
            base: import.clone(),
            result: a1.clone(),
            merge: false,
        },
        Pick {
            label: "a2".to_string(),
            base: a1.clone(),
            result: a2.clone(),
            merge: false,
        },
    ];
    let composed = plan::compose(&view, &import, &picks).expect("compose");
    assert_eq!(
        composed["tree"],
        git_in(&hand, 1, &["rev-parse", &format!("{a2}^{{tree}}")])
    );
    run_commands(&hand, &composed["commands"]);

    for (word, binary) in [("bin.dat:1-2", true), ("src/a.txt:2", false)] {
        let wanted = plan::wanted_paths(word);
        let [Wanted::Part { path, lines }] = wanted.as_slice() else {
            panic!("{word} is a part of a file");
        };
        let refused = plan::refusal(&view, "split", &landed, path, lines);
        assert_eq!(refused["refused"], true);
        assert_eq!(refused["granularity"], "file");
        assert_eq!(refused["binary"], binary, "{word}: {refused:#}");
    }

    let path = view.path();
    assert!(plan::exists(&view));
    drop(view);
    assert!(!Path::new(&path).exists(), "the view is removed");
    assert_eq!(holdings(&git_dir), before, "no ref and no object written");
}

#[test]
fn a_paths_value_is_whole_files_or_parts_of_one() {
    assert_eq!(
        plan::wanted_paths(" src/, a.rs:3 ,b.rs:10-20,c:d.rs,e.rs:x,, "),
        vec![
            Wanted::Whole("src".to_string()),
            Wanted::Part {
                path: "a.rs".to_string(),
                lines: "3".to_string()
            },
            Wanted::Part {
                path: "b.rs".to_string(),
                lines: "10-20".to_string()
            },
            Wanted::Whole("c:d.rs".to_string()),
            Wanted::Whole("e.rs:x".to_string()),
        ]
    );
    assert_eq!(plan::shell_quoted("a/b.rs"), "a/b.rs");
    assert_eq!(plan::shell_quoted("it's"), r"'it'\''s'");
}

#[test]
fn a_messages_spawn_change_lines_are_read_from_its_last_paragraph() {
    let id = source_change(SOURCE, 11);
    let message = format!(
        "T\n\n    Spawn-Change: {id}\n\nSpawn-Task: 11\nSpawn-Head: {}\nSpawn-Change: {id}\n",
        "a".repeat(40)
    );
    assert_eq!(super::spawn_changes(&message), vec![id.clone()]);
    assert_eq!(super::spawn_changes(&format!("Spawn-Change: {id}")), Vec::<String>::new());
    assert_eq!(index::trailers(&message).task, Some(11), "the other trailers still read");
    assert_eq!(id, index::change_id(&domain(), "edit:task:11"));
}
