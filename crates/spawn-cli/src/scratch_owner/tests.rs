use std::path::{Path, PathBuf};

use super::{Claim, OWNER_FILE, Parsed, build_path_ok, change, claim, claim_in};

const HOUR_MS: i64 = 3_600_000;

/// A scratch directory, resolved, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let root = crate::storage::tests::scratch(tag);
        std::fs::create_dir_all(&root).expect("mkdir");
        Scratch(std::fs::canonicalize(&root).expect("resolve"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn flags(words: &[&str]) -> Parsed {
    Parsed::of(&words.iter().map(ToString::to_string).collect::<Vec<_>>()).expect("flags")
}

fn claimed(dir: &Path, words: &[&str], now: i64) -> Result<Claim, String> {
    claim(dir, &flags(words), now)
}

fn sample() -> Claim {
    Claim {
        agent: "rev-e.96_x".to_string(),
        objective: "4/140".to_string(),
        created_ms: 1_000,
        lease_until_ms: 1_000 + 24 * HOUR_MS,
        build: vec!["target".to_string(), "a/b/target".to_string()],
    }
}

#[test]
fn an_owner_file_reads_back_its_five_fields_and_nothing_else() {
    let claim = sample();
    let line = claim.line();
    assert!(!line.contains('\n'));
    assert_eq!(Claim::parse(&line), Some(claim.clone()));
    assert_eq!(Claim::parse(&format!("{line}\n")), Some(claim.clone()));
    let value: serde_json::Value = serde_json::from_str(&line).expect("JSON");
    let mut keys: Vec<&String> = value.as_object().expect("an object").keys().collect();
    keys.sort();
    assert_eq!(keys, ["agent", "build", "created_ms", "lease_until_ms", "objective"]);
    let with = |field: &str, to: serde_json::Value| {
        let mut changed = value.clone();
        changed[field] = to;
        Claim::parse(&changed.to_string())
    };
    assert_eq!(with("extra", serde_json::json!(1)), None, "a field more");
    assert_eq!(with("agent", serde_json::json!("")), None);
    assert_eq!(with("agent", serde_json::json!("a b")), None);
    assert_eq!(with("agent", serde_json::json!("x".repeat(65))), None);
    assert_eq!(with("objective", serde_json::json!("4/14")), None, "not <mission>/<NNN>");
    assert_eq!(with("objective", serde_json::json!("0/140")), None);
    assert_eq!(with("build", serde_json::json!(["src"])), None);
    assert_eq!(with("created_ms", serde_json::json!("1")), None);
    let mut without = value.clone();
    without.as_object_mut().expect("an object").remove("build");
    assert_eq!(Claim::parse(&without.to_string()), None, "a field fewer");
    assert_eq!(Claim::parse("4242 123456"), None, "094's pid marker is no claim");
    assert_eq!(claim.pair(), Some((4, 140)));
}

#[test]
fn a_build_path_is_relative_plain_parts_ending_in_target() {
    for good in ["target", "t/target", "a/b-c/target"] {
        assert!(build_path_ok(good), "{good}");
    }
    for bad in ["", "t", "target/x", "/abs/target", "../target", "a/../target", "./target", "a//target", "targets"] {
        assert!(!build_path_ok(bad), "{bad}");
    }
}

#[test]
fn a_claim_is_refused_with_nothing_written() {
    let scratch = Scratch::new("claim-refused");
    let dir = scratch.0.join("c");
    std::fs::create_dir_all(dir.join("t")).expect("mkdir");
    let outside = scratch.0.join("outside");
    std::fs::create_dir_all(outside.join("target")).expect("mkdir");
    std::os::unix::fs::symlink(&outside, dir.join("link")).expect("symlink");
    std::fs::write(dir.join("file"), "x").expect("write");
    std::os::unix::fs::symlink(&dir, scratch.0.join("alias")).expect("symlink");
    let base = ["--agent", "rev-e", "--objective", "1/7"];
    let refused = |dir: &Path, more: &[&str], holds: &str| {
        let words: Vec<&str> = base.iter().chain(more).copied().collect();
        let error = claimed(dir, &words, 5).expect_err("refused");
        assert!(error.contains(holds), "{error} holds {holds}");
    };
    refused(Path::new("relative/c"), &[], "absolute");
    refused(&scratch.0.join("alias"), &[], "a symlink");
    refused(&dir, &["--build", "link/target"], "a symlink");
    refused(&dir, &["--build", "file/target"], "not a directory");
    refused(&dir, &["--build", "t"], "ending in target");
    refused(&dir, &["--lease-hours", "0"], "--lease-hours");
    refused(&dir, &["--lease-hours", "169"], "--lease-hours");
    assert!(std::fs::symlink_metadata(dir.join(OWNER_FILE)).is_err(), "nothing written");
    // An owner file that holds no claim (094's marker) is not taken over.
    std::fs::write(dir.join(OWNER_FILE), "4242 123456\n").expect("marker");
    refused(&dir, &[], "does not hold a claim");
    assert_eq!(
        std::fs::read_to_string(dir.join(OWNER_FILE)).expect("marker"),
        "4242 123456\n"
    );
}

#[test]
fn a_claim_is_written_renewed_released_and_never_taken_over_while_it_runs() {
    let scratch = Scratch::new("claim-lease");
    let dir = scratch.0.join("a");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let now = 1_000_000;
    let made = claimed(
        &dir,
        &["--agent", "rev-e", "--objective", "1/7", "--build", "t/target", "--build", "base/target"],
        now,
    )
    .expect("claimed");
    assert_eq!(made.objective, "1/007");
    assert_eq!(made.lease_until_ms, now + 24 * HOUR_MS);
    assert_eq!(made.build, ["t/target", "base/target"]);
    assert_eq!(claim_in(&dir), Some(made.clone()));
    assert_eq!(
        std::fs::read_to_string(dir.join(OWNER_FILE)).expect("the file"),
        format!("{}\n", made.line())
    );
    let error = claimed(&dir, &["--agent", "rev-b", "--objective", "1/8"], now + 1).expect_err("runs");
    assert!(error.contains("renew or release"), "{error}");
    let renewed = change(&dir, |claim| claim.lease_until_ms = now + 5 * HOUR_MS).expect("renewed");
    assert_eq!(renewed.created_ms, now);
    assert_eq!(claim_in(&dir).map(|claim| claim.lease_until_ms), Some(now + 5 * HOUR_MS));
    change(&dir, |claim| claim.lease_until_ms = now + 2).expect("released");
    let taken = claimed(&dir, &["--agent", "rev-b", "--objective", "1/8"], now + 3).expect("its lease ended");
    assert_eq!(taken.agent, "rev-b");
    let bare = scratch.0.join("bare");
    std::fs::create_dir_all(&bare).expect("mkdir");
    let error = change(&bare, |_| {}).expect_err("no claim");
    assert!(error.contains("no claim"), "{error}");
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .expect("read")
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(leftovers, [OWNER_FILE], "no temporary file is left");
}
