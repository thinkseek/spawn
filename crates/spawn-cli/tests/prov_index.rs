//! `spawn prov index`, `who` and `show` (PV2, docs/provenance.md) on a real
//! git repository and a real database (`SPAWN_TEST_DATABASE_URL`, a schema
//! of each case's own), through the compiled `spawn` binary
//! (`CARGO_BIN_EXE_spawn`).
//!
//! The history: an imported first commit; a task's branch (`spawn/agent_a`,
//! task 11) of three checkpoints with a merge of main between them, while
//! main takes a commit of its own; its landing (a merge with the
//! integrator's trailers); and an unlanded branch (`spawn/agent_b`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use postgres::{Client, NoTls};
use serde_json::{Value, json};
use spawn_postgres::PostgresStore;
use spawn_postgres::testing::TestSchema;

/// The source as missions name it: the domain is `git:<SOURCE>`.
const SOURCE: &str = "/srv/prov-test.git";

/// A directory of the test's own, removed when it ends, passed or failed.
struct Scratch(PathBuf);

impl Scratch {
    fn new(case: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let path = std::env::temp_dir().join(format!(
            "spawn-prov-{case}-{}-{nanos:x}",
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

/// The test URL with `schema` as its search path.
fn schema_url(schema: &str) -> String {
    let url = std::env::var("SPAWN_TEST_DATABASE_URL")
        .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip");
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}options=-csearch_path%3D{schema}")
}

/// A work tree whose commits carry the times the test gives.
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

    /// Commit everything with `message`, one second after the last commit.
    fn commit(&mut self, message: &str) -> String {
        self.clock += 1;
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// Merge `branch` into the current branch, with no fast-forward.
    fn merge(&mut self, branch: &str, message: &str) -> String {
        self.clock += 1;
        self.git(&["merge", "-q", "--no-ff", "-m", message, branch]);
        self.git(&["rev-parse", "HEAD"])
    }
}

/// The commits of the history the module doc tells.
struct History {
    import: String,
    a1: String,
    a2: String,
    owner: String,
    a3: String,
    landing: String,
    b1: String,
}

fn history(work: &mut Work) -> History {
    work.write("README.md", "the project\n");
    work.write("Cargo.lock", "# locked\n");
    work.write("tool.sh", "echo tool\n");
    let import = work.commit("Initial import");
    work.git(&["checkout", "-q", "-b", "spawn/agent_a"]);
    work.write("src/a.rs", "one\ntwo\nthree\n");
    let a1 = work.commit("spawn checkpoint exec_a1");
    work.write("src/a.rs", "one\nTWO\nthree\n");
    let a2 = work.commit("spawn checkpoint exec_a2");
    work.git(&["checkout", "-q", "main"]);
    work.write("docs/owner.md", "the owner's own\n");
    let owner = work.commit("An owner's fix");
    work.git(&["checkout", "-q", "spawn/agent_a"]);
    work.merge("main", "spawn: merge main into agent_a");
    work.write("src/a.rs", "one\nTWO\nthree\nfour\n");
    let tool = work.dir.join("tool.sh");
    let mut mode = std::fs::metadata(&tool).expect("tool.sh").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
    std::fs::set_permissions(&tool, mode).expect("tool.sh made executable");
    let a3 = work.commit("spawn checkpoint exec_a3");
    work.git(&["checkout", "-q", "main"]);
    let landing = work.merge(
        "spawn/agent_a",
        &format!(
            "Add a.rs (task 11)\n\nWhat changed\n    Spawn-Task: 99\n\nSpawn-Task: 11\nSpawn-Head: {a3}\n"
        ),
    );
    work.git(&["checkout", "-q", "-b", "spawn/agent_b"]);
    work.write("src/b.rs", "bee\n");
    let b1 = work.commit("spawn checkpoint exec_b1");
    work.git(&["checkout", "-q", "main"]);
    History {
        import,
        a1,
        a2,
        owner,
        a3,
        landing,
        b1,
    }
}

/// The event log and the mission layer of the history: agent_a's three
/// executions (exec_b1 is in no log), task 11 (agent_a, on a preset) and
/// task 12 (agent_b), both made before any commit.
fn facts(client: &mut Client) {
    client
        .batch_execute(
            "insert into mission_missions (id, text, origin, created_at_ms, updated_at_ms)
             overriding system value values (1, 'prov', 'human', 0, 0);
             insert into grading_presets (preset_id, engine_label, model_label, model_profile,
                                          cost_rank, window_start_ms, created_at_ms,
                                          updated_at_ms, last_event)
             values ('opus', 'claude', 'claude-opus-5-5', 'default', 1, 0, 0, 0, 0);
             insert into mission_tasks (id, mission_id, kind, agent_id, session_id, brief,
                                        created_by, created_at_ms, updated_at_ms, preset_id)
             overriding system value
             values (11, 1, 'ephemeral', 'agent_a', 'session_a', 'a', 'human', 1000, 1000, 'opus'),
                    (12, 1, 'ephemeral', 'agent_b', 'session_b', 'b', 'human', 1000, 1000, null);",
        )
        .expect("the mission layer");
    for (seq, execution) in ["exec_a1", "exec_a2", "exec_a3"].into_iter().enumerate() {
        let seq = i64::try_from(seq).expect("small");
        client
            .execute(
                "insert into spawn_events (agent_id, session_id, execution_id, source, kind,
                                           session_seq, recorded_at_ms)
                 values ('agent_a', 'session_a', $1, 'bridge', $2, $3, 1000)",
                &[
                    &execution,
                    &json!({"name": "execution.started", "detail": {}}),
                    &(seq + 1),
                ],
            )
            .expect("an execution");
    }
}

/// `spawn <arguments>` on `schema`.
fn spawn(schema: &str, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spawn"))
        .args(arguments)
        .env("SPAWN_DATABASE_URL", schema_url(schema))
        .output()
        .expect("run spawn")
}

/// `spawn <arguments>`'s JSON, which must succeed.
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

fn index(schema: &str, git_dir: &str) -> Value {
    spawn_json(
        schema,
        &["prov", "index", "--source", SOURCE, "--repo", git_dir],
    )
}

/// Every recorded change's `spawn_prov_canonical`, each entry's `origin`
/// replaced by the `change_id#rev` it names (row ids differ from database
/// to database), keyed by `change_id#rev`.
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

fn count(client: &mut Client, table: &str) -> i64 {
    client
        .query_one(&format!("select count(*) from {table}"), &[])
        .expect("a count")
        .get(0)
}

/// The changes of `show`'s answer.
fn changes(answer: &Value) -> Vec<Value> {
    answer["changes"].as_array().expect("changes").clone()
}

fn entry<'a>(shown: &'a Value, path: &str) -> &'a Value {
    shown["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|entry| entry["path"] == path)
        .unwrap_or_else(|| panic!("no entry for {path}: {shown}"))
}

/// Task 11's edit Change and its landing.
fn check_task(schema: &str, work: &Work, h: &History) {
    // Task 11: one edit Change, a revision per checkpoint, attributed by
    // the log, on the task's preset.
    let task = changes(&spawn_json(schema, &["prov", "show", "11"]));
    let edits: Vec<&Value> = task
        .iter()
        .filter(|shown| shown["change"]["kind"] == "edit")
        .collect();
    assert_eq!(edits.len(), 3, "{task:?}");
    for (rev, (shown, (commit, execution))) in edits
        .iter()
        .zip([(&h.a1, "exec_a1"), (&h.a2, "exec_a2"), (&h.a3, "exec_a3")])
        .enumerate()
    {
        let change = &shown["change"];
        assert_eq!(change["rev"], json!(rev + 1), "{change}");
        assert_eq!(change["result"], format!("git1:{commit}"));
        assert_eq!(change["execution_id"], execution);
        assert_eq!(change["agent_id"], "agent_a");
        assert_eq!(change["attributed"], "host");
        assert_eq!(change["task_id"], 11);
        assert_eq!(change["preset_id"], "opus");
        assert_eq!(change["model_label"], "claude-opus-5-5");
        assert_eq!(change["boundary"], "checkpoint");
        assert_eq!(change["change_id"], edits[0]["change"]["change_id"]);
    }
    // The third checkpoint follows a merge of main: the owner's file came
    // from main and is not its; the mode alone is mechanical.
    let a3 = edits[2];
    assert_eq!(
        a3["change"]["base"],
        json!(format!(
            "git1:{}",
            work.git(&["rev-parse", &format!("{}^1", h.a3)])
        ))
    );
    let paths: Vec<&str> = a3["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .filter_map(|entry| entry["path"].as_str())
        .collect();
    assert_eq!(paths, ["src/a.rs", "tool.sh"]);
    assert_eq!(entry(a3, "tool.sh")["class"], "mechanical");
    assert_eq!(entry(a3, "src/a.rs")["size"], 19);

    // The landing: one includes edge to the task's edit Change, and each
    // path's origin the revision whose content it holds.
    let land: Vec<&Value> = task
        .iter()
        .filter(|shown| shown["change"]["kind"] == "land")
        .collect();
    assert_eq!(land.len(), 1);
    let land = land[0];
    assert_eq!(land["change"]["result"], format!("git1:{}", h.landing));
    assert_eq!(land["change"]["base"], format!("git1:{}", h.owner));
    assert_eq!(land["change"]["agent_id"], "agent_a");
    assert_eq!(land["change"]["boundary"], "land");
    assert_eq!(
        land["edges"],
        json!([{"kind": "includes", "dst": edits[0]["change"]["change_id"],
                "detail": {"select": "all"}, "at_ms": land["change"]["at_ms"]}])
    );
    let a = entry(land, "src/a.rs");
    assert_eq!(a["origin_change_id"], edits[0]["change"]["change_id"]);
    assert_eq!(a["origin_rev"], 3);
    assert_eq!(entry(land, "tool.sh")["origin_rev"], 3);
    assert!(
        land["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .all(|entry| entry["path"] != "docs/owner.md"),
        "main's own commit is not the landing's: {land}"
    );
}

/// The imports and the unlanded branch.
fn check_imports_and_branch(schema: &str, h: &History) {
    // The imports: the first commit (Cargo.lock generated) and the owner's.
    let import = changes(&spawn_json(schema, &["prov", "show", &h.import]));
    assert_eq!(import.len(), 1);
    assert_eq!(import[0]["change"]["kind"], "import");
    assert_eq!(import[0]["change"]["attributed"], "imported");
    assert_eq!(import[0]["change"]["boundary"], "backfill");
    assert_eq!(entry(&import[0], "Cargo.lock")["class"], "generated");
    assert_eq!(entry(&import[0], "README.md")["before"], Value::Null);
    let owner = changes(&spawn_json(schema, &["prov", "show", &h.owner]));
    assert_eq!(owner[0]["change"]["kind"], "import");

    // The unlanded branch: its agent's task at the time, inferred (its
    // execution is in no log).
    let b = changes(&spawn_json(schema, &["prov", "show", &h.b1]));
    assert_eq!(b.len(), 1);
    assert_eq!(b[0]["change"]["task_id"], 12);
    assert_eq!(b[0]["change"]["agent_id"], "agent_b");
    assert_eq!(b[0]["change"]["attributed"], "inferred");
    assert_eq!(b[0]["change"]["execution_id"], "exec_b1");
    assert_eq!(b[0]["change"]["rev"], 1);
}

/// `who`, by line and by file.
fn check_who(schema: &str, git_dir: &str, h: &History) {
    // who: a line's blame names the checkpoint that wrote it; a file names
    // the landing and the revision its content came from.
    let line = spawn_json(
        schema,
        &[
            "prov",
            "who",
            "src/a.rs:2",
            "--source",
            SOURCE,
            "--repo",
            git_dir,
        ],
    );
    assert_eq!(line["commit"], h.a2.as_str(), "{line}");
    assert_eq!(line["granularity"], "line");
    assert_eq!(line["execution_id"], "exec_a2");
    assert_eq!(line["agent_id"], "agent_a");
    assert_eq!(line["task_id"], 11);
    let readme = spawn_json(
        schema,
        &[
            "prov",
            "who",
            "README.md:1",
            "--source",
            SOURCE,
            "--repo",
            git_dir,
        ],
    );
    assert_eq!(readme["attributed"], "imported");
    let file = spawn_json(
        schema,
        &[
            "prov", "who", "src/a.rs", "--source", SOURCE, "--repo", git_dir,
        ],
    );
    assert_eq!(file["granularity"], "file");
    assert_eq!(file["commit"], h.landing.as_str(), "{file}");
    assert_eq!(file["change"]["kind"], "land");
    assert_eq!(file["origin"]["rev"], 3);
    assert_eq!(file["execution_id"], "exec_a3");
}

#[test]
fn the_index_records_checkpoints_landings_and_imports_once_and_who_and_show_read_them() {
    let schema = TestSchema::new("prov_index");
    let store = schema.store();
    let scratch = Scratch::new("index");
    let mut work = Work::init(&scratch.0);
    let h = history(&mut work);
    let git_dir = work.git_dir();
    let mut client = Client::connect(&schema_url(&schema), NoTls).expect("the owner");
    facts(&mut client);

    let first = index(&schema, &git_dir);
    assert_eq!(
        (&first["edit"], &first["land"], &first["import"]),
        (&json!(4), &json!(1), &json!(2)),
        "{first}"
    );
    assert_eq!(first["domain"], format!("git:{SOURCE}"));

    check_task(&schema, &work, &h);
    check_imports_and_branch(&schema, &h);

    // No entry with the same before and after but a mode's change.
    let same: i64 = client
        .query_one(
            "select count(*) from prov_entries where before = after and class <> 'mechanical'",
            &[],
        )
        .expect("a count")
        .get(0);
    assert_eq!(same, 0);

    // A second run records nothing and leaves every canonical value as it
    // was.
    let before = canonical(&mut client);
    let rows = (
        count(&mut client, "prov_changes"),
        count(&mut client, "prov_entries"),
        count(&mut client, "prov_edges"),
    );
    let second = index(&schema, &git_dir);
    assert_eq!(second["recorded"], 0, "{second}");
    assert_eq!(second["main_commits_recorded_before"], 3, "{second}");
    assert_eq!(canonical(&mut client), before);
    assert_eq!(
        rows,
        (
            count(&mut client, "prov_changes"),
            count(&mut client, "prov_entries"),
            count(&mut client, "prov_edges"),
        )
    );

    check_who(&schema, &git_dir, &h);

    // The reader login reads what the routes read.
    let reader = PostgresStore::connect(
        &spawn_postgres::role_url(&schema_url(&schema), "spawn_reader").expect("the reader"),
    )
    .expect("the reader logs in");
    let history = reader
        .prov_path_history(&format!("git:{SOURCE}"), "src/a.rs", Some("src"))
        .expect("the reader reads a path's history");
    assert_eq!(history.len(), 4, "three checkpoints and the landing");
    let by_commit = reader
        .prov_changes_for_result(&format!("git1:{}", h.landing))
        .expect("the reader reads by result");
    assert_eq!(by_commit.len(), 1);
    drop(reader);
    drop(store);
}

#[test]
fn a_commit_the_commit_map_renamed_is_recorded_already_and_shown_by_either_id() {
    let schema = TestSchema::new("prov_commit_map");
    let store = schema.store();
    let scratch = Scratch::new("map");
    let mut work = Work::init(&scratch.0);
    work.write("README.md", "x\n");
    let new = work.commit("Imported after a rewrite");
    let old = "ab".repeat(20);
    let mut client = Client::connect(&schema_url(&schema), NoTls).expect("the owner");
    // The Change recorded before the rewrite, under the old commit.
    let recorded: i64 = client
        .query_one(
            "select spawn_prov_record_change($1, '[]'::jsonb, null)",
            &[&json!({
                "change_id": "cbeforetherewriteaaaaaaaaaa", "rev": 1, "kind": "import",
                "domain": format!("git:{SOURCE}"), "agent_id": "imported",
                "attributed": "imported", "base": "git1:0000",
                "result": format!("git1:{old}"), "boundary": "backfill", "at_ms": 1,
            })],
        )
        .expect("recorded")
        .get(0);
    client
        .query_one("select spawn_commit_map_load($1)", &[&json!([[old, new]])])
        .expect("the map");
    let run = index(&schema, &work.git_dir());
    assert_eq!(run["recorded"], 0, "{run}");
    assert_eq!(run["main_commits_recorded_before"], 1, "{run}");
    for id in [&old, &new] {
        let shown = changes(&spawn_json(&schema, &["prov", "show", id]));
        assert_eq!(shown.len(), 1, "{id}: {shown:?}");
        assert_eq!(shown[0]["change"]["id"], recorded);
    }
    drop(store);
}

/// A repository of `landings` landings, each a task's branch of two
/// checkpoints, and the event log and the mission layer for it.
fn many(work: &mut Work, client: &mut Client, landings: i64) {
    client
        .batch_execute(
            "insert into mission_missions (id, text, origin, created_at_ms, updated_at_ms)
             overriding system value values (1, 'prov', 'human', 0, 0);",
        )
        .expect("a mission");
    work.write("README.md", "start\n");
    work.commit("Initial import");
    for task in 1..=landings {
        let agent = format!("agent_{task}");
        client
            .execute(
                "insert into mission_tasks (id, mission_id, kind, agent_id, session_id, brief,
                                            created_by, created_at_ms, updated_at_ms)
                 overriding system value
                 values ($1, 1, 'ephemeral', $2, $2, 'b', 'human', 1000, 1000)",
                &[&task, &agent],
            )
            .expect("a task");
        let branch = format!("spawn/{agent}");
        work.git(&["checkout", "-q", "-b", &branch]);
        for step in 1..=2 {
            work.write(
                &format!("src/{task}/{step}.rs"),
                &format!("{task} {step}\n"),
            );
            work.write("shared.txt", &format!("{task} {step}\n"));
            work.commit(&format!("spawn checkpoint exec_{task}_{step}"));
        }
        let head = work.git(&["rev-parse", "HEAD"]);
        work.git(&["checkout", "-q", "main"]);
        work.merge(
            &branch,
            &format!("Task {task} (task {task})\n\nSpawn-Task: {task}\nSpawn-Head: {head}\n"),
        );
    }
}

/// Wait (at most a minute) until the server holds no session named
/// `name`: a killed client's backend ends, and its last statement commits
/// or rolls back, only after the client is gone.
fn sessions_gone(client: &mut Client, name: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(1);
    loop {
        let left: i64 = client
            .query_one(
                "select count(*) from pg_stat_activity where application_name = $1",
                &[&name],
            )
            .expect("the sessions")
            .get(0);
        if left == 0 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{left} session(s) of the killed run still open after a minute"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn a_run_killed_midway_is_completed_by_the_next_as_an_uninterrupted_run_would_be() {
    let scratch = Scratch::new("killed");
    let mut work = Work::init(&scratch.0);
    let whole = TestSchema::new("prov_whole");
    let killed = TestSchema::new("prov_killed");
    let whole_store = whole.store();
    let killed_store = killed.store();
    let mut whole_client = Client::connect(&schema_url(&whole), NoTls).expect("the owner");
    let mut killed_client = Client::connect(&schema_url(&killed), NoTls).expect("the owner");
    many(&mut work, &mut whole_client, 30);
    killed_client
        .batch_execute(
            "insert into mission_missions (id, text, origin, created_at_ms, updated_at_ms)
             overriding system value values (1, 'prov', 'human', 0, 0);",
        )
        .expect("a mission");
    for task in 1..=30_i64 {
        let agent = format!("agent_{task}");
        killed_client
            .execute(
                "insert into mission_tasks (id, mission_id, kind, agent_id, session_id, brief,
                                            created_by, created_at_ms, updated_at_ms)
                 overriding system value
                 values ($1, 1, 'ephemeral', $2, $2, 'b', 'human', 1000, 1000)",
                &[&task, &agent],
            )
            .expect("a task");
    }
    let git_dir = work.git_dir();
    let uninterrupted = index(&whole, &git_dir);
    assert_eq!(uninterrupted["land"], 30, "{uninterrupted}");
    assert_eq!(uninterrupted["edit"], 60, "{uninterrupted}");

    // Kill runs, each a little later, until one has recorded something (on
    // this history a kill then lands midway), then let one finish.
    let total = count(&mut whole_client, "prov_changes");
    // The killed runs' sessions carry a name of their own (not the
    // schema's: the guard's own connection carries that), so the test
    // waits until the server has ended every one of them: a statement in
    // flight when the process died may still commit after the kill.
    let child_name = format!("prov_killed_child_{}", std::process::id());
    let child_url = format!("{}&application_name={child_name}", schema_url(&killed));
    let mut delay = std::time::Duration::from_millis(20);
    for _ in 0..40 {
        let mut child = Command::new(env!("CARGO_BIN_EXE_spawn"))
            .args(["prov", "index", "--source", SOURCE, "--repo", &git_dir])
            .env("SPAWN_DATABASE_URL", &child_url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start spawn");
        std::thread::sleep(delay);
        let _ = child.kill();
        let _ = child.wait();
        sessions_gone(&mut killed_client, &child_name);
        if count(&mut killed_client, "prov_changes") > 0 {
            break;
        }
        delay += std::time::Duration::from_millis(20);
    }
    let partial = count(&mut killed_client, "prov_changes");
    let rest = index(&killed, &git_dir);
    assert_eq!(
        i64::try_from(rest["recorded"].as_u64().expect("a count")).expect("small") + partial,
        total,
        "{rest}"
    );
    assert_eq!(canonical(&mut killed_client), canonical(&mut whole_client));
    let again = index(&killed, &git_dir);
    assert_eq!(again["recorded"], 0, "{again}");
    drop(whole_store);
    drop(killed_store);
}

/// PV3a's commands through the binary: `blast` of a task, and the plans,
/// which print and write nothing (no ref moves); a part of one file is
/// refused with `granularity: file`; a plan without `--plan` is refused.
#[test]
fn blast_and_the_plans_answer_through_the_binary_and_write_no_ref() {
    let schema = TestSchema::new("prov_plans");
    let store = schema.store();
    let scratch = Scratch::new("plans");
    let mut work = Work::init(&scratch.0);
    let h = history(&mut work);
    let git_dir = work.git_dir();
    let mut client = Client::connect(&schema_url(&schema), NoTls).expect("the owner");
    facts(&mut client);
    index(&schema, &git_dir);
    let refs = work.git(&["for-each-ref"]);

    let blast = spawn_json(&schema, &["prov", "blast", "11"]);
    assert_eq!(blast["changes"].as_array().map(Vec::len), Some(4), "{blast}");
    let plan = |verb: &str, words: &[&str]| {
        let mut arguments = vec!["prov", verb, "--plan"];
        arguments.extend_from_slice(words);
        arguments.extend_from_slice(&["--source", SOURCE, "--repo", &git_dir]);
        spawn(&schema, &arguments)
    };
    let reverted = plan("revert", &["11"]);
    assert!(reverted.status.success(), "{}", String::from_utf8_lossy(&reverted.stderr));
    let reverted: Value = serde_json::from_slice(&reverted.stdout).expect("JSON");
    assert_eq!(reverted["plan"], "revert");
    assert_eq!(reverted["result"], h.landing.as_str());
    assert!(reverted["tree"].is_string(), "{reverted}");

    let split = plan("split", &["11", "--paths", "src"]);
    assert!(split.status.success(), "{}", String::from_utf8_lossy(&split.stderr));
    let split: Value = serde_json::from_slice(&split.stdout).expect("JSON");
    assert_eq!(split["parts"][0]["paths"], json!(["src/a.rs"]), "{split}");

    let refused = plan("split", &["11", "--paths", "src/a.rs:2"]);
    assert_eq!(refused.status.code(), Some(1));
    let refused: Value = serde_json::from_slice(&refused.stdout).expect("JSON");
    assert_eq!(refused["granularity"], "file", "{refused}");
    assert_eq!(refused["binary"], false);

    let composed = plan("compose", &[&h.a1]);
    assert!(composed.status.success(), "{}", String::from_utf8_lossy(&composed.stderr));
    let composed: Value = serde_json::from_slice(&composed.stdout).expect("JSON");
    assert_eq!(composed["plan"], "compose");

    let unplanned = spawn(
        &schema,
        &["prov", "revert", "11", "--source", SOURCE, "--repo", &git_dir],
    );
    assert!(!unplanned.status.success());
    assert_eq!(work.git(&["for-each-ref"]), refs, "no ref written");
    drop(store);
}
