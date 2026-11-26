//! PV4 (docs/provenance.md) on a real git repository and a real database
//! (`SPAWN_TEST_DATABASE_URL`, a schema of its own), through the compiled
//! `spawn` binary: the `supersedes` edges a landing gets from its task's
//! dispatcher brief, the files over the cap a workspace left out (D6), and
//! the shadow report `spawn prov durability` (D4, B6).
//!
//! The history: an imported first commit; task 11 (preset `opus`) changes
//! `src/a.rs` and adds `src/new.rs`; task 12 (`sonnet`, brief `reverts:
//! task 11`) restores both; task 13 (`sonnet`, brief `fixes: task 12` and
//! `supersedes: task 11 (refactor)`) changes `src/b.rs`, and its landing's
//! message (agent text) says `reverts: task 11`, which must be ignored;
//! task 14 (`haiku`, brief `reverts: task 13`) does not restore `src/b.rs`.
//! Each task lands as a merge with the integrator's trailers.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use postgres::{Client, NoTls};
use serde_json::{Value, json};
use spawn_postgres::testing::TestSchema;

const SOURCE: &str = "/srv/prov-durability.git";

/// A directory of the test's own, removed when it ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(case: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let path = std::env::temp_dir().join(format!(
            "spawn-prov-durability-{case}-{}-{nanos:x}",
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

fn schema_url(schema: &str) -> String {
    let url = std::env::var("SPAWN_TEST_DATABASE_URL")
        .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip");
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}options=-csearch_path%3D{schema}")
}

struct Work {
    dir: PathBuf,
    clock: i64,
}

impl Work {
    fn init(root: &Path) -> Self {
        let dir = root.join("work");
        std::fs::create_dir_all(&dir).expect("the work tree");
        let work = Self {
            dir,
            clock: 1_700_000_000,
        };
        work.git(&["init", "-q", "-b", "main"]);
        work
    }

    fn git_dir(&self) -> String {
        self.dir.join(".git").display().to_string()
    }

    fn git(&self, arguments: &[&str]) -> String {
        let output = spawn_host::test_git_command()
            .arg("-C")
            .arg(&self.dir)
            .args(arguments)
            .env("GIT_AUTHOR_DATE", format!("{} +0000", self.clock))
            .env("GIT_COMMITTER_DATE", format!("{} +0000", self.clock))
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn write(&self, path: &str, text: &str) {
        let file = self.dir.join(path);
        std::fs::create_dir_all(file.parent().expect("a parent")).expect("its directory");
        std::fs::write(file, text).expect("write");
    }

    fn commit(&mut self, message: &str) -> String {
        self.clock += 1;
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// One task's branch `spawn/<agent>` from main: `change` makes its one
    /// checkpoint; then its landing, `body` the message's middle paragraph.
    fn task(&mut self, task: i64, agent: &str, body: &str, change: impl FnOnce(&Self)) -> String {
        self.git(&["checkout", "-q", "-b", &format!("spawn/{agent}"), "main"]);
        change(self);
        let head = self.commit(&format!("spawn checkpoint exec_{task}"));
        self.git(&["checkout", "-q", "main"]);
        self.clock += 1;
        self.git(&[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            &format!(
                "Task {task} (task {task})\n\n{body}\n\nSpawn-Task: {task}\nSpawn-Head: {head}\n"
            ),
            &format!("spawn/{agent}"),
        ]);
        self.git(&["rev-parse", "HEAD"])
    }
}

/// The mission layer and the dispatcher's `task.assigned` records.
fn facts(client: &mut Client) {
    client
        .batch_execute(
            "insert into mission_missions (id, text, origin, created_at_ms, updated_at_ms)
             overriding system value values (1, 'prov', 'human', 0, 0);
             insert into grading_presets (preset_id, engine_label, model_label, model_profile,
                                          cost_rank, window_start_ms, created_at_ms,
                                          updated_at_ms, last_event)
             values ('opus', 'claude', 'claude-opus-5-5', 'default', 1, 0, 0, 0, 0),
                    ('sonnet', 'claude', 'claude-sonnet-5-5', 'default', 2, 0, 0, 0, 0),
                    ('haiku', 'claude', 'claude-haiku-5-5', 'default', 3, 0, 0, 0, 0);
             insert into mission_tasks (id, mission_id, kind, agent_id, session_id, brief,
                                        created_by, created_at_ms, updated_at_ms, preset_id)
             overriding system value
             values (11, 1, 'ephemeral', 'agent_a', 'session_a', 'a', 'human', 1000, 1000, 'opus'),
                    (12, 1, 'ephemeral', 'agent_b', 'session_b', 'b', 'human', 1000, 1000, 'sonnet'),
                    (13, 1, 'ephemeral', 'agent_c', 'session_c', 'c', 'human', 1000, 1000, 'sonnet'),
                    (14, 1, 'ephemeral', 'agent_d', 'session_d', 'd', 'human', 1000, 1000, 'haiku');",
        )
        .expect("the mission layer");
    let briefs = [
        (
            11,
            "Objective 1: add a.rs\n\nreverts: task 99 is not a line of its own here",
        ),
        (12, "Undo task 11\n\nReverts: task 11\n"),
        (
            13,
            "Fix task 12\n\n- fixes: task 12\nsupersedes: task 11 (refactor)\n",
        ),
        (14, "Undo task 13\n\nreverts: task 13\n"),
    ];
    for (seq, (task, brief)) in briefs.into_iter().enumerate() {
        let seq = i64::try_from(seq).expect("small") + 1;
        client
            .execute(
                "insert into spawn_events (agent_id, session_id, source, kind, session_seq,
                                           recorded_at_ms)
                 values ('spawn_supervisor', 'session_control', 'control', $1, $2, 1000)",
                &[
                    &json!({"name": "control.recorded", "detail": {"record": {
                        "type": "task.assigned", "id": task, "mission_id": 1,
                        "brief": brief, "created_by": "spawn_supervisor"}}}),
                    &seq,
                ],
            )
            .expect("a task.assigned record");
    }
}

fn spawn(schema: &str, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spawn"))
        .args(arguments)
        .env("SPAWN_DATABASE_URL", schema_url(schema))
        .env_remove("SPAWN_WORKSPACE_ROOT")
        .output()
        .expect("run spawn")
}

fn spawn_json(schema: &str, arguments: &[&str]) -> Value {
    let output = spawn(schema, arguments);
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

fn count(client: &mut Client, table: &str) -> i64 {
    client
        .query_one(&format!("select count(*) from {table}"), &[])
        .expect("a count")
        .get(0)
}

/// The `land` Change of `task` as `show` prints it.
fn land_of(schema: &str, task: i64) -> Value {
    let shown = spawn_json(schema, &["prov", "show", &task.to_string()]);
    shown["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .find(|change| change["change"]["kind"] == "land")
        .cloned()
        .unwrap_or_else(|| panic!("task {task} has no land Change: {shown}"))
}

/// The `supersedes` edges of a shown Change: `(dst, detail)`, sorted.
fn supersedes(shown: &Value) -> Vec<(String, Value)> {
    let mut edges: Vec<(String, Value)> = shown["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .filter(|edge| edge["kind"] == "supersedes")
        .map(|edge| {
            (
                edge["dst"].as_str().expect("dst").to_string(),
                edge["detail"].clone(),
            )
        })
        .collect();
    edges.sort_by_key(|edge| edge.1.to_string());
    edges
}

fn sha256(bytes: &[u8]) -> String {
    let digest = spawn_cas::sha256_reader(&mut &bytes[..]).expect("hash");
    format!("sha256:{}", digest.hex())
}

/// Write a record of files over the cap for `agent` under `root`, in the
/// format the workspace stores write.
fn oversized(root: &Path, agent: &str, files: &[(&str, &[u8])]) {
    let mut record = Vec::new();
    for (path, bytes) in files {
        record.extend_from_slice(
            format!("told\0{} {}\0{path}\0", bytes.len(), sha256(bytes)).as_bytes(),
        );
    }
    let dir = root.join(".oversized");
    std::fs::create_dir_all(&dir).expect("the record's directory");
    std::fs::write(dir.join(agent), record).expect("the record");
}

#[test]
fn landings_supersede_by_the_dispatchers_brief_and_durability_charges_confirmed_defects() {
    let schema = TestSchema::new("prov_durability");
    // The store makes the schema's tables.
    let store = schema.store();
    let scratch = Scratch::new("brief");
    let mut work = Work::init(&scratch.0);
    work.write("src/a.rs", "one\n");
    work.write("src/b.rs", "bee\n");
    work.commit("Initial import");
    let _l11 = work.task(11, "agent_a", "What changed: a.rs", |w| {
        w.write("src/a.rs", "one\nbroken\n");
        w.write("src/new.rs", "new\n");
    });
    let _l12 = work.task(12, "agent_b", "Restored a.rs", |w| {
        w.write("src/a.rs", "one\n");
        std::fs::remove_file(w.dir.join("src/new.rs")).expect("remove");
    });
    let _l13 = work.task(13, "agent_c", "reverts: task 11\nSpawn-Task-Note", |w| {
        w.write("src/b.rs", "bee\nfixed\n");
    });
    let _l14 = work.task(14, "agent_d", "Tried to undo 13", |w| {
        w.write("src/b.rs", "bee\nnot quite\n");
    });
    let git_dir = work.git_dir();
    let mut client = Client::connect(&schema_url(&schema), NoTls).expect("the owner");
    facts(&mut client);

    // The workspace root: task 14's agent left a file over the cap out of
    // its checkpoints, and so did task 11's (not main's newest landing).
    let root = scratch.0.join("root");
    let big = vec![9u8; 4096];
    oversized(&root, "agent_d", &[("data/model.bin", &big)]);
    oversized(&root, "agent_a", &[("old.bin", b"old")]);
    let root_text = root.display().to_string();
    let index = |schema: &str| {
        spawn_json(
            schema,
            &[
                "prov", "index", "--source", SOURCE, "--repo", &git_dir, "--root", &root_text,
            ],
        )
    };
    let first = index(&schema);
    assert_eq!(first["land"], 4, "{first}");

    check_edges_and_oversized(&schema, &work, &big);

    // A second run records nothing.
    let rows = (
        count(&mut client, "prov_changes"),
        count(&mut client, "prov_entries"),
        count(&mut client, "prov_edges"),
    );
    let second = index(&schema);
    assert_eq!(second["recorded"], 0, "{second}");
    assert_eq!(
        rows,
        (
            count(&mut client, "prov_changes"),
            count(&mut client, "prov_entries"),
            count(&mut client, "prov_edges"),
        )
    );

    check_report(&schema);

    // The report writes nothing.
    assert_eq!(
        rows,
        (
            count(&mut client, "prov_changes"),
            count(&mut client, "prov_entries"),
            count(&mut client, "prov_edges"),
        )
    );
    drop(store);
}

/// The edges the four landings got from their briefs, and the newest
/// landing's file over the cap.
fn check_edges_and_oversized(schema: &str, work: &Work, big: &[u8]) {
    let schema = schema.to_string();
    let l11 = land_of(&schema, 11);
    let l13 = land_of(&schema, 13);
    let id11 = l11["change"]["change_id"]
        .as_str()
        .expect("an id")
        .to_string();
    let id12 = land_of(&schema, 12)["change"]["change_id"]
        .as_str()
        .expect("an id")
        .to_string();
    let id13 = l13["change"]["change_id"]
        .as_str()
        .expect("an id")
        .to_string();
    // Task 11's brief mentions a directive inside prose: none.
    assert!(supersedes(&l11).is_empty(), "{l11}");
    // Task 12 restores task 11's paths: level 1.
    assert_eq!(
        supersedes(&land_of(&schema, 12)),
        [(
            id11.clone(),
            json!({"reason": "defect", "evidence": "level1_revert", "target_task": 11})
        )]
    );
    // Task 13: the brief's two lines; its landing message's "reverts: task
    // 11" (agent text) is no directive.
    assert_eq!(
        supersedes(&l13),
        [
            (
                id12.clone(),
                json!({"reason": "defect", "evidence": "dispatcher_fixes", "target_task": 12})
            ),
            (
                id11.clone(),
                json!({"reason": "refactor", "evidence": "dispatcher_supersedes", "target_task": 11})
            ),
        ]
    );
    // Task 14 does not restore task 13's path: unverified.
    let l14 = land_of(&schema, 14);
    assert_eq!(
        supersedes(&l14),
        [(
            id13.clone(),
            json!({"reason": "defect", "evidence": "unverified_revert", "target_task": 13})
        )]
    );
    // includes edges are still written as before.
    assert!(
        l14["edges"]
            .as_array()
            .expect("edges")
            .iter()
            .any(|edge| edge["kind"] == "includes"),
        "{l14}"
    );

    // D6: the newest landing holds its agent's file over the cap, by
    // digest and size; task 11's landing holds no record (it was not
    // main's newest), and no byte of either is in git.
    let model = l14["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|entry| entry["path"] == "data/model.bin")
        .unwrap_or_else(|| panic!("no oversized entry: {l14}"));
    assert_eq!(model["after"], sha256(big));
    assert_eq!(model["before"], Value::Null);
    assert_eq!(model["size"], 4096);
    assert_eq!(model["class"], "content");
    assert!(
        !l11["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .any(|entry| entry["path"] == "old.bin"),
        "{l11}"
    );
    assert!(
        !work
            .git(&["ls-tree", "-r", "--name-only", "main"])
            .contains("model.bin")
    );
}

/// The shadow report of the history, by flag.
fn check_report(schema: &str) {
    let schema = schema.to_string();
    // The shadow report.
    let report = spawn_json(&schema, &["prov", "durability", "--json"]);
    assert_eq!(report["shadow"], true);
    assert_eq!(report["window"], json!({"landings": 50, "days": 14}));
    assert_eq!(report["landings"], 4);
    let row = |preset: &str| -> Value {
        report["presets"]
            .as_array()
            .expect("presets")
            .iter()
            .find(|row| row["preset_id"] == preset)
            .cloned()
            .unwrap_or_else(|| panic!("no {preset}: {report}"))
    };
    assert_eq!(
        row("opus"),
        json!({"preset_id": "opus", "landed_tasks": 1, "confirmed_defects": 1,
               "reverted_tasks": 1, "fixes_reported": 0})
    );
    assert_eq!(
        row("sonnet"),
        json!({"preset_id": "sonnet", "landed_tasks": 2, "confirmed_defects": 0,
               "reverted_tasks": 0, "fixes_reported": 1}),
        "a fixes: is reported, never a defect; an unverified revert is neither"
    );
    assert_eq!(
        row("haiku"),
        json!({"preset_id": "haiku", "landed_tasks": 1, "confirmed_defects": 0,
               "reverted_tasks": 0, "fixes_reported": 0})
    );
    assert_eq!(report["unattributed"], 0);
    assert_eq!(
        report["not_scored"],
        json!({"refactor": 1, "unverified_revert": 1})
    );
    assert_eq!(report["defects"][0]["charged_task"], 11);
    assert_eq!(report["defects"][0]["by_task"], 12);

    // One source; a window of one landing still holds task 12's revert
    // (the next landing after 11), and drops nothing else that counts.
    let narrow = spawn_json(
        &schema,
        &[
            "prov",
            "durability",
            "--json",
            "--source",
            SOURCE,
            "--window-landings",
            "1",
        ],
    );
    assert_eq!(narrow["domain"], format!("git:{SOURCE}"));
    assert!(
        narrow["not_scored"].get("out_of_window").is_none(),
        "{narrow}"
    );
    assert_eq!(narrow["defects"].as_array().map(Vec::len), Some(1));
    // The text report, and refused flags.
    let shown = spawn(&schema, &["prov", "durability"]);
    assert!(shown.status.success());
    let text = String::from_utf8_lossy(&shown.stdout);
    assert!(
        text.contains("shadow only") && text.contains("opus"),
        "{text}"
    );
    for bad in [
        &["prov", "durability", "--window-landings", "0"][..],
        &["prov", "durability", "--window-days", "x"][..],
        &["prov", "durability", "--json", "yes"][..],
        &["prov", "durability", "--since", "1d"][..],
    ] {
        assert!(!spawn(&schema, bad).status.success(), "{bad:?}");
    }
}
