//! `spawn prov snapshot`, `refs`, `rollback` and `blast` on data snapshots
//! (PV3b, docs/provenance.md), through the compiled `spawn` binary
//! (`CARGO_BIN_EXE_spawn`), on a real database (`SPAWN_TEST_DATABASE_URL`,
//! a schema of each case's own) and a real spawn-cas store.
//!
//! The toy pipeline: 100 raw shards are cleaned one by one (`clean`, derived
//! from `raw`) and merged ten by ten (`merge-0` .. `merge-9`, each derived
//! from the ten clean shards it read, by their digests). One clean shard is
//! corrupted: `blast` names exactly the merge output that read it, the
//! rollback moves `clean` back, the affected shard and merge are rerun, and
//! the validator passes again.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use postgres::{Client, NoTls};
use serde_json::{Value, json};
use spawn_cas::{Manifest, Store, sha256_of};
use spawn_postgres::testing::TestSchema;

/// The golden `tree_sha256_v1` of spawn-node v1's `tree/` fixture.
const TREE_FIXTURE_SHA256: &str =
    "191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707";

/// The commit the pipeline's code is at (no repository needs to hold it).
const CODE: &str = "0123456789abcdef0123456789abcdef01234567";

/// A directory of the test's own, removed when it ends, passed or failed.
struct Scratch(PathBuf);

impl Scratch {
    fn new(case: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let path = std::env::temp_dir().join(format!(
            "spawn-prov-data-{case}-{}-{nanos:x}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("a scratch directory");
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn cas(&self) -> String {
        self.join("cas").display().to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The test URL with `schema` as its search path.
fn schema_url(schema: &str) -> String {
    let url = std::env::var("SPAWN_TEST_DATABASE_URL")
        .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip");
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}options=-csearch_path%3D{schema}")
}

/// `spawn <arguments>` on `schema`, with no workspace or data root set
/// unless `env` sets one.
fn spawn_env(schema: &str, arguments: &[&str], env: &[(&str, &Path)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_spawn"));
    command
        .args(arguments)
        .env("SPAWN_DATABASE_URL", schema_url(schema))
        .env_remove("SPAWN_WORKSPACE_ROOT")
        .env_remove("SPAWN_DATA_ROOT");
    for (name, value) in env {
        command.env(name, value);
    }
    command
}

fn spawn(schema: &str, arguments: &[&str]) -> Output {
    spawn_env(schema, arguments, &[])
        .output()
        .expect("run spawn")
}

fn json_of(arguments: &[&str], output: &Output) -> Value {
    assert!(
        output.status.success(),
        "spawn {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "spawn {arguments:?} printed no JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// `spawn <arguments>`'s JSON, which must succeed.
fn spawn_json(schema: &str, arguments: &[&str]) -> Value {
    json_of(arguments, &spawn(schema, arguments))
}

/// `spawn prov snapshot <dir> --ref <name> --cas <cas> <more>...`.
fn snapshot(schema: &str, scratch: &Scratch, dir: &Path, name: &str, more: &[&str]) -> Value {
    let dir = dir.display().to_string();
    let cas = scratch.cas();
    let mut arguments = vec!["prov", "snapshot", &dir, "--ref", name, "--cas", &cas];
    arguments.extend_from_slice(more);
    spawn_json(schema, &arguments)
}

/// A refused command: it fails, and says why on stderr.
fn refused(output: &Output) -> String {
    assert!(
        !output.status.success(),
        "not refused: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn count(client: &mut Client, table: &str) -> i64 {
    client
        .query_one(&format!("select count(*) from {table}"), &[])
        .expect("a count")
        .get(0)
}

fn rows(client: &mut Client) -> (i64, i64, i64) {
    (
        count(client, "prov_changes"),
        count(client, "prov_entries"),
        count(client, "prov_edges"),
    )
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("its directory");
    std::fs::write(path, text).expect("write");
}

/// The pipeline's directories.
struct Pipeline {
    raw: PathBuf,
    clean: PathBuf,
    merge: PathBuf,
    config: PathBuf,
}

const SHARDS: usize = 100;
const GROUP: usize = 10;

fn shard(n: usize) -> String {
    format!("shard-{n:03}")
}

impl Pipeline {
    fn new(scratch: &Scratch) -> Self {
        let pipeline = Self {
            raw: scratch.join("data/raw"),
            clean: scratch.join("data/clean"),
            merge: scratch.join("data/merge"),
            config: scratch.join("pipeline.toml"),
        };
        write(&pipeline.config, "clean = \"uppercase\"\n");
        for n in 0..SHARDS {
            write(
                &pipeline.raw.join(shard(n)),
                &format!("record {n} of the raw set\n"),
            );
        }
        pipeline
    }

    /// Clean one shard: its raw text in capitals.
    fn clean_shard(&self, n: usize) {
        let raw = std::fs::read_to_string(self.raw.join(shard(n))).expect("raw");
        write(&self.clean.join(shard(n)), &raw.to_uppercase());
    }

    fn group_dir(&self, group: usize) -> PathBuf {
        self.merge.join(format!("g{group}"))
    }

    /// Merge one group: its ten clean shards, in order.
    fn merge_group(&self, group: usize) {
        let text: String = (group * GROUP..(group + 1) * GROUP)
            .map(|n| std::fs::read_to_string(self.clean.join(shard(n))).expect("clean"))
            .collect();
        write(&self.group_dir(group).join("merged.txt"), &text);
    }

    /// The `--input`s of a group's merge: the digests of the clean shards
    /// it read.
    fn group_inputs(&self, group: usize) -> Vec<String> {
        (group * GROUP..(group + 1) * GROUP)
            .flat_map(|n| {
                let bytes = std::fs::read(self.clean.join(shard(n))).expect("clean");
                ["--input".to_string(), sha256_of(&bytes).to_string()]
            })
            .collect()
    }

    /// The validator: every merge output holds its raw shards in capitals.
    fn invalid_groups(&self) -> Vec<usize> {
        (0..SHARDS / GROUP)
            .filter(|&group| {
                let want: String = (group * GROUP..(group + 1) * GROUP)
                    .map(|n| {
                        std::fs::read_to_string(self.raw.join(shard(n)))
                            .expect("raw")
                            .to_uppercase()
                    })
                    .collect();
                std::fs::read_to_string(self.group_dir(group).join("merged.txt")).ok() != Some(want)
            })
            .collect()
    }
}

fn snapshot_group(schema: &str, scratch: &Scratch, pipeline: &Pipeline, group: usize) -> Value {
    let inputs = pipeline.group_inputs(group);
    let mut more: Vec<&str> = inputs.iter().map(String::as_str).collect();
    more.extend_from_slice(&["--code", CODE]);
    snapshot(
        schema,
        scratch,
        &pipeline.group_dir(group),
        &format!("merge-{group}"),
        &more,
    )
}

fn snapshot_clean(schema: &str, scratch: &Scratch, pipeline: &Pipeline) -> Value {
    let config = pipeline.config.display().to_string();
    snapshot(
        schema,
        scratch,
        &pipeline.clean,
        "clean",
        &["--input", "raw", "--code", CODE, "--config", &config],
    )
}

fn refs_of(schema: &str, scratch: &Scratch) -> Value {
    spawn_json(schema, &["prov", "refs", "--cas", &scratch.cas()])
}

fn head_of(refs: &Value, name: &str) -> Value {
    refs["refs"]
        .as_array()
        .expect("refs")
        .iter()
        .find(|row| row["ref"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("no ref {name}: {refs}"))
}

/// The blast of the corrupted snapshot names exactly the merge output that
/// read it; the code commit reaches every data snapshot made with it.
fn check_blast(schema: &str, bad: &str) {
    // blast of the bad snapshot: exactly merge-3's output that read it.
    let blast = spawn_json(schema, &["prov", "blast", bad]);
    let snapshots = blast["snapshots"].as_array().expect("snapshots");
    let named: Vec<(String, i64)> = snapshots
        .iter()
        .map(|row| {
            (
                row["ref"].as_str().expect("ref").to_string(),
                row["rev"].as_i64().expect("rev"),
            )
        })
        .collect();
    assert_eq!(
        named,
        [("clean".to_string(), 2), ("merge-3".to_string(), 2)],
        "{blast}"
    );
    assert_eq!(snapshots[1]["via"], "used");
    let used: Vec<&Value> = blast["used"].as_array().expect("used").iter().collect();
    assert_eq!(used.len(), 1, "{blast}");
    // From the code commit (indexed in no Git domain): every data snapshot
    // made with it.
    let from_code = spawn_json(schema, &["prov", "blast", CODE]);
    let reached: BTreeSet<String> = from_code["snapshots"]
        .as_array()
        .expect("snapshots")
        .iter()
        .map(|row| row["ref"].as_str().expect("ref").to_string())
        .collect();
    assert_eq!(reached.len(), 1 + SHARDS / GROUP, "{from_code}");
    assert!(reached.contains("clean") && reached.contains("merge-9"));
}

/// After the rollback: the affected shard and merge rerun, and the
/// validator passes.
fn rerun_and_validate(schema: &str, scratch: &Scratch, pipeline: &Pipeline, good: &str) {
    // Rerun the affected shard: the directory is the head again, so the
    // snapshot records nothing; rerun the affected merge: it records.
    pipeline.clean_shard(37);
    let rerun = snapshot_clean(schema, scratch, pipeline);
    assert_eq!(rerun["recorded"], false, "{rerun}");
    assert_eq!(rerun["snapshot"], good);
    pipeline.merge_group(3);
    let merged = snapshot_group(schema, scratch, pipeline, 3);
    assert_eq!(
        (&merged["recorded"], &merged["rev"]),
        (&json!(true), &json!(3))
    );
    assert!(pipeline.invalid_groups().is_empty(), "the validator passes");
    let refs = refs_of(schema, scratch);
    assert_eq!(
        refs["refs"].as_array().map(Vec::len),
        Some(2 + SHARDS / GROUP)
    );
    // Its third revision holds what its first held: two snapshots, each
    // kept once.
    let merge_three = head_of(&refs, "merge-3");
    assert_eq!(
        merge_three["kept"].as_array().map(Vec::len),
        Some(2),
        "{merge_three}"
    );
    assert_eq!(merge_three["revisions"], 3);
    let cas = Store::open(scratch.join("cas")).expect("the store");
    cas.verify_snapshot(&spawn_cas::Digest::parse(good).expect("digest"))
        .expect("the good snapshot is whole");
}

/// A kept snapshot rolls back; one never the ref's, or stored digest-only,
/// is refused.
fn other_rollbacks(schema: &str, scratch: &Scratch, data: &Path, snapshots: &[String]) {
    let cas_dir = scratch.cas();
    // A kept one rolls back.
    let moved = spawn_json(
        schema,
        &[
            "prov",
            "rollback",
            "model",
            &snapshots[1],
            "--cas",
            &cas_dir,
        ],
    );
    assert_eq!(
        (&moved["recorded"], &moved["rev"]),
        (&json!(true), &json!(5))
    );
    // Never a snapshot of the ref: refused too.
    let other = sha256_of(b"something else").to_string();
    assert!(
        refused(&spawn(
            schema,
            &["prov", "rollback", "model", &other, "--cas", &cas_dir]
        ))
        .contains("never a snapshot")
    );
    // A digest-only snapshot never stored its bytes: a rollback to it is
    // refused while it is kept.
    write(&data.join("weights.bin"), "version 5");
    let light = snapshot(schema, scratch, data, "model", &["--digest-only"]);
    write(&data.join("weights.bin"), "version 6");
    snapshot(schema, scratch, data, "model", &[]);
    let light = light["snapshot"].as_str().expect("snapshot");
    assert!(
        refused(&spawn(
            schema,
            &["prov", "rollback", "model", light, "--cas", &cas_dir]
        ))
        .contains("digest-only")
    );
}

#[test]
fn the_toy_pipeline_blast_names_the_affected_merge_and_rollback_repairs_it() {
    let schema = TestSchema::new("prov_data_pipeline");
    let store = schema.store();
    let mut client = Client::connect(&schema_url(&schema), NoTls).expect("the owner");
    let scratch = Scratch::new("pipeline");
    let pipeline = Pipeline::new(&scratch);

    // The raw set: an import, every shard an entry.
    let raw = snapshot(&schema, &scratch, &pipeline.raw, "raw", &[]);
    assert_eq!(
        (&raw["recorded"], &raw["kind"], &raw["rev"], &raw["entries"]),
        (&json!(true), &json!("import"), &json!(1), &json!(SHARDS)),
        "{raw}"
    );

    // Clean, then merge: a derive Change each, with their used edges.
    for n in 0..SHARDS {
        pipeline.clean_shard(n);
    }
    let clean = snapshot_clean(&schema, &scratch, &pipeline);
    assert_eq!(clean["kind"], "derive", "{clean}");
    assert_eq!(clean["used"].as_array().map(Vec::len), Some(3), "{clean}");
    assert_eq!(clean["used"][0]["dst"], raw["snapshot"]);
    assert_eq!(clean["used"][1]["dst"], format!("git1:{CODE}"));
    for group in 0..SHARDS / GROUP {
        pipeline.merge_group(group);
        let merged = snapshot_group(&schema, &scratch, &pipeline, group);
        assert_eq!(merged["used"].as_array().map(Vec::len), Some(GROUP + 1));
    }
    assert!(pipeline.invalid_groups().is_empty());
    let good = clean["snapshot"].as_str().expect("a snapshot").to_string();
    let show = spawn_json(&schema, &["prov", "show", "data:clean"]);
    assert_eq!(show["changes"][0]["change"]["result"], good.as_str());
    assert_eq!(show["changes"][0]["change"]["domain"], "data:clean");

    // A clean shard goes bad: the next snapshot of clean has that one path.
    write(&pipeline.clean.join(shard(37)), "garbage\n");
    let corrupt = snapshot_clean(&schema, &scratch, &pipeline);
    assert_eq!(
        (&corrupt["rev"], &corrupt["entries"]),
        (&json!(2), &json!(1))
    );
    let bad = corrupt["snapshot"]
        .as_str()
        .expect("a snapshot")
        .to_string();
    // The naive rerun merges every group: only group 3 read the bad shard,
    // and every other output is unchanged, so it records nothing.
    for group in 0..SHARDS / GROUP {
        pipeline.merge_group(group);
        let merged = snapshot_group(&schema, &scratch, &pipeline, group);
        assert_eq!(
            merged["recorded"],
            json!(group == 3),
            "group {group}: {merged}"
        );
    }
    assert_eq!(pipeline.invalid_groups(), [3]);

    check_blast(&schema, &bad);

    // The plan writes nothing.
    let before = rows(&mut client);
    let plan = spawn_json(
        &schema,
        &[
            "prov",
            "rollback",
            "--plan",
            "clean",
            &good,
            "--cas",
            &scratch.cas(),
        ],
    );
    assert_eq!(plan["plan"], "rollback");
    assert_eq!((&plan["from"], &plan["to"]), (&json!(bad), &json!(good)));
    assert_eq!(plan["kind"], "revert");
    assert_eq!(plan["refused"], Value::Null);
    assert_eq!(plan["entries"].as_array().map(Vec::len), Some(1));
    assert_eq!(plan["entries"][0]["path"], shard(37));
    assert_eq!(rows(&mut client), before, "a plan writes nothing");
    assert_eq!(
        head_of(&refs_of(&schema, &scratch), "clean")["head"],
        bad.as_str()
    );

    // The rollback moves the ref, as a revert revision.
    let moved = spawn_json(
        &schema,
        &["prov", "rollback", "clean", &good, "--cas", &scratch.cas()],
    );
    assert_eq!(moved["recorded"], true, "{moved}");
    let head = head_of(&refs_of(&schema, &scratch), "clean");
    assert_eq!(
        (&head["head"], &head["rev"], &head["kind"]),
        (&json!(good), &json!(3), &json!("revert"))
    );
    let again = spawn_json(
        &schema,
        &["prov", "rollback", "clean", &good, "--cas", &scratch.cas()],
    );
    assert_eq!(again["recorded"], false, "the head already: {again}");

    rerun_and_validate(&schema, &scratch, &pipeline, &good);
    drop(store);
}

#[test]
fn an_unchanged_directory_records_nothing_and_the_tree_fixture_is_its_golden_digest() {
    let schema = TestSchema::new("prov_data_fixture");
    let store = schema.store();
    let mut client = Client::connect(&schema_url(&schema), NoTls).expect("the owner");
    let scratch = Scratch::new("fixture");
    // The shared fixture, copied out of the repository (a checkout may sit
    // under a workspace root, which a data root never is).
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../spawn-node-proto/testdata/node-protocol-v1/tree");
    let tree = scratch.join("data/tree");
    for path in ["build", "check-tests.sh", "sub/001.sh"] {
        let bytes = std::fs::read(fixture.join(path)).expect("the fixture");
        std::fs::create_dir_all(tree.join(path).parent().expect("parent")).expect("dir");
        std::fs::write(tree.join(path), bytes).expect("copy");
    }
    let first = snapshot(&schema, &scratch, &tree, "tree", &[]);
    assert_eq!(first["snapshot"], format!("sha256:{TREE_FIXTURE_SHA256}"));
    assert_eq!(
        (&first["recorded"], &first["entries"]),
        (&json!(true), &json!(3))
    );
    let shown = spawn_json(&schema, &["prov", "show", "data:tree"]);
    let change = &shown["changes"][0]["change"];
    assert_eq!(
        change["base"],
        sha256_of(b"").to_string(),
        "the empty snapshot"
    );
    assert_eq!(change["kind"], "import");
    let entry = &shown["changes"][0]["entries"][0];
    assert_eq!(entry["path"], "build");
    assert!(
        entry["after"]
            .as_str()
            .is_some_and(|after| after.starts_with("sha256:"))
    );
    assert!(entry["size"].as_i64().is_some(), "{entry}");

    let before = rows(&mut client);
    let second = snapshot(&schema, &scratch, &tree, "tree", &["--input", "tree"]);
    assert_eq!(second["recorded"], false, "{second}");
    assert_eq!(
        rows(&mut client),
        before,
        "an unchanged directory records nothing"
    );

    // Unprefixed digests are refused everywhere.
    let hex = TREE_FIXTURE_SHA256;
    let tree_dir = tree.display().to_string();
    let cas = scratch.cas();
    for arguments in [
        vec![
            "prov", "snapshot", &tree_dir, "--ref", "x", "--cas", &cas, "--input", hex,
        ],
        vec!["prov", "rollback", "tree", hex, "--cas", &cas],
        vec!["prov", "blast", hex],
    ] {
        assert!(refused(&spawn(&schema, &arguments)).contains("unprefixed"));
    }
    assert_eq!(rows(&mut client), before);
    drop(store);
}

#[test]
fn over_ten_thousand_entries_fold_into_one_tree_row_per_top_level_directory() {
    let schema = TestSchema::new("prov_data_fold");
    let store = schema.store();
    let scratch = Scratch::new("fold");
    let data = scratch.join("data/big");
    for n in 0..10_001 {
        let top = if n % 2 == 0 { "even" } else { "odd" };
        write(&data.join(format!("{top}/{n:05}")), &n.to_string());
    }
    write(&data.join("README"), "top-level files stay rows\n");
    let big = snapshot(&schema, &scratch, &data, "big", &["--digest-only"]);
    assert_eq!(
        (&big["entries"], &big["folded"]),
        (&json!(3), &json!(true)),
        "{big}"
    );
    let manifest = Manifest::build(&data).expect("the manifest");
    let shown = spawn_json(&schema, &["prov", "show", "data:big"]);
    let entries = shown["changes"][0]["entries"].as_array().expect("entries");
    let paths: Vec<(&str, &str)> = entries
        .iter()
        .map(|entry| {
            (
                entry["path"].as_str().expect("path"),
                entry["class"].as_str().expect("class"),
            )
        })
        .collect();
    assert_eq!(
        paths,
        [("README", "content"), ("even", "tree"), ("odd", "tree")]
    );
    assert_eq!(
        entries[1]["after"],
        manifest.subtree(b"even").digest().to_string()
    );
    // One file more under one directory: one entry, unfolded.
    write(&data.join("odd/extra"), "x");
    let next = snapshot(&schema, &scratch, &data, "big", &["--digest-only"]);
    assert_eq!(
        (&next["entries"], &next["folded"]),
        (&json!(1), &json!(false))
    );
    drop(store);
}

#[test]
fn the_pin_keeps_three_snapshots_and_a_rollback_to_the_fourth_is_refused() {
    let schema = TestSchema::new("prov_data_keep");
    let store = schema.store();
    let mut client = Client::connect(&schema_url(&schema), NoTls).expect("the owner");
    let scratch = Scratch::new("keep");
    let data = scratch.join("data/model");
    write(&data.join("shared.bin"), "the same in every version");
    let mut snapshots = Vec::new();
    for version in 1..=4 {
        write(&data.join("weights.bin"), &format!("version {version}"));
        let shot = snapshot(&schema, &scratch, &data, "model", &[]);
        snapshots.push(shot["snapshot"].as_str().expect("snapshot").to_string());
        if version == 4 {
            assert_eq!(shot["dropped"], json!([snapshots[0]]), "{shot}");
        }
    }
    let cas = Store::open(scratch.join("cas")).expect("the store");
    let kept: Vec<String> = cas
        .kept_snapshots("model")
        .expect("kept")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        kept,
        [
            snapshots[3].clone(),
            snapshots[2].clone(),
            snapshots[1].clone()
        ]
    );
    let first = spawn_cas::Digest::parse(&snapshots[0]).expect("digest");
    assert!(!cas.has(&first), "the fourth newest is dropped");
    assert!(!cas.has(&sha256_of(b"version 1")));
    assert!(cas.has(&sha256_of(b"the same in every version")));

    let before = rows(&mut client);
    let cas_dir = scratch.cas();
    let plan = spawn(
        &schema,
        &[
            "prov",
            "rollback",
            "--plan",
            "model",
            &snapshots[0],
            "--cas",
            &cas_dir,
        ],
    );
    assert_eq!(plan.status.code(), Some(1));
    let plan: Value = serde_json::from_slice(&plan.stdout).expect("JSON");
    assert!(
        plan["refused"]
            .as_str()
            .is_some_and(|why| why.contains("no longer kept"))
    );
    let why = refused(&spawn(
        &schema,
        &[
            "prov",
            "rollback",
            "model",
            &snapshots[0],
            "--cas",
            &cas_dir,
        ],
    ));
    assert!(
        why.contains("rollback refused") && why.contains("no longer kept"),
        "{why}"
    );
    assert_eq!(
        rows(&mut client),
        before,
        "a refused rollback records nothing"
    );
    other_rollbacks(&schema, &scratch, &data, &snapshots);
    drop(store);
}

#[test]
fn a_data_root_inside_a_workspace_holding_transcripts_or_with_an_escaping_link_is_refused() {
    let schema = TestSchema::new("prov_data_refusals");
    let store = schema.store();
    let mut client = Client::connect(&schema_url(&schema), NoTls).expect("the owner");
    let scratch = Scratch::new("refusals");
    let cas = scratch.join("cas");
    let attempt = |dir: &Path, env: &[(&str, &Path)]| -> String {
        let dir = dir.display().to_string();
        let cas = cas.display().to_string();
        let arguments = ["prov", "snapshot", &dir, "--ref", "d", "--cas", &cas];
        refused(
            &spawn_env(&schema, &arguments, env)
                .output()
                .expect("run spawn"),
        )
    };

    // Inside the workspace root.
    let workspace = scratch.join("ws");
    let inside = workspace.join("agent_x/data");
    write(&inside.join("a"), "a");
    let why = attempt(&inside, &[("SPAWN_WORKSPACE_ROOT", &workspace)]);
    assert!(why.contains("inside the workspace root"), "{why}");
    // A directory above that holds the transcript archive is a workspace
    // root, whatever the environment says.
    std::fs::create_dir_all(workspace.join(".transcripts/agent_x")).expect("transcripts");
    let why = attempt(&inside, &[]);
    assert!(why.contains("inside a workspace root"), "{why}");
    // A root holding a transcript directory, at the top or below.
    let holder = scratch.join("data/holder");
    write(&holder.join(".transcripts/agent/native.jsonl"), "{}\n");
    let why = attempt(&holder, &[]);
    assert!(why.contains("transcript directory"), "{why}");
    let nested = scratch.join("data/nested");
    write(&nested.join("deep/.transcripts/agent/native.jsonl"), "{}\n");
    let why = attempt(&nested, &[]);
    assert!(why.contains("transcript directory"), "{why}");
    // A link that leaves the root.
    let linked = scratch.join("data/linked");
    write(&linked.join("a"), "a");
    write(&scratch.join("secret/key"), "never read");
    std::os::unix::fs::symlink("../../secret/key", linked.join("key")).expect("a link");
    let why = attempt(&linked, &[]);
    assert!(why.contains("points outside"), "{why}");
    // Outside SPAWN_DATA_ROOT.
    let plain = scratch.join("elsewhere/plain");
    write(&plain.join("a"), "a");
    let why = attempt(&plain, &[("SPAWN_DATA_ROOT", &scratch.join("data"))]);
    assert!(why.contains("outside SPAWN_DATA_ROOT"), "{why}");
    // The store inside the data root.
    let selfish = scratch.join("data/selfish");
    write(&selfish.join("a"), "a");
    let dir = selfish.display().to_string();
    let inner = selfish.join("cas").display().to_string();
    let why = refused(&spawn(
        &schema,
        &["prov", "snapshot", &dir, "--ref", "d", "--cas", &inner],
    ));
    assert!(why.contains("inside it"), "{why}");

    assert_eq!(rows(&mut client), (0, 0, 0), "nothing was recorded");
    let objects = Store::open(&cas)
        .and_then(|store| store.objects(spawn_cas::Algo::Sha256))
        .expect("objects");
    assert!(objects.is_empty(), "nothing was stored");
    // Next to all of it, a plain data root is fine.
    let fine = scratch.join("data/fine");
    write(&fine.join("a"), "a");
    let answer = snapshot(&schema, &scratch, &fine, "d", &[]);
    assert_eq!(answer["recorded"], true);
    drop(store);
}

#[test]
fn a_snapshot_killed_at_any_instant_is_completed_by_running_it_again() {
    let schema = TestSchema::new("prov_data_sigkill");
    let store = schema.store();
    let mut client = Client::connect(&schema_url(&schema), NoTls).expect("the owner");
    let scratch = Scratch::new("sigkill");
    let data = scratch.join("data/set");
    for n in 0..1500 {
        write(
            &data.join(format!("part-{:02}/{n:04}", n % 30)),
            &format!("row {n}\n"),
        );
    }
    let dir = data.display().to_string();
    let cas = scratch.cas();
    let arguments = ["prov", "snapshot", &dir, "--ref", "set", "--cas", &cas];
    for delay in [5_u64, 15, 30, 60, 100, 160, 250] {
        let mut child = spawn_env(&schema, &arguments, &[])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let deadline = Instant::now() + Duration::from_millis(delay);
        while Instant::now() < deadline && child.try_wait().expect("wait").is_none() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let _ = child.kill();
        child.wait().expect("reaped");
    }
    let done = spawn_json(&schema, &arguments);
    let manifest = Manifest::build(&data).expect("the manifest");
    assert_eq!(done["snapshot"], manifest.digest().to_string());
    // One revision, whichever run recorded it, and the pin keeps it whole.
    let revisions: i64 = client
        .query_one(
            "select count(*) from prov_changes where domain = 'data:set'",
            &[],
        )
        .expect("count")
        .get(0);
    assert_eq!(revisions, 1);
    let cas = Store::open(scratch.join("cas")).expect("the store");
    assert_eq!(
        cas.kept_snapshots("set").expect("kept"),
        [manifest.digest()]
    );
    cas.verify_snapshot(&manifest.digest()).expect("whole");
    let again = spawn_json(&schema, &arguments);
    assert_eq!(again["recorded"], false);
    drop(store);
}
