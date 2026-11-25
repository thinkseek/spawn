use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use spawn_postgres::PostgresStore;

use super::Integrator;
use crate::sources::test_git as git;

struct Db {
    store: Arc<PostgresStore>,
    url: String,
    schema: spawn_postgres::testing::TestSchema,
}

impl Db {
    fn new(case: &str) -> Self {
        // Dropped after the store (declared after it), when the test passes (P19).
        let schema = spawn_postgres::testing::TestSchema::new(case);
        let url = schema.url().to_string();
        let store = PostgresStore::connect_in_schema(&url, &schema).expect("connect");
        Self {
            store: Arc::new(store),
            url,
            schema,
        }
    }

    /// An owner connection in the schema.
    fn admin(&self) -> postgres::Client {
        let mut client =
            postgres::Client::connect(&self.url, postgres::NoTls).expect("admin connection");
        client
            .batch_execute(&format!("set search_path to {}", self.schema))
            .expect("schema");
        client
    }

    /// A mission its gate alone accepts, on `source`.
    fn gated_mission(&self, source: &str, scenarios: &Path) -> i64 {
        let mission = self
            .store
            .mission_add("gated", &json!([source]), "human")
            .expect("add");
        self.store
            .mission_configure(
                mission,
                Some(&scenarios.to_string_lossy()),
                "gate",
                None,
                None,
            )
            .expect("configure");
        mission
    }

    /// A task of `mission` for `worker`, delivered and reported.
    fn reported_task(&self, mission: i64, worker: &str) -> i64 {
        let task = self
            .store
            .task_assign(mission, "ephemeral", worker, "brief", "default")
            .expect("assign");
        self.store.task_delivered(task, 1).expect("delivered");
        assert!(self.store.task_reported(task, "done").expect("reported"));
        task
    }

    fn record(&self, record: &Value) {
        self.store.integrator_record(record).expect("record");
    }

    fn mission(&self, id: i64) -> Value {
        let board = self.store.mission_board(50).expect("board");
        board["missions"]
            .as_array()
            .expect("missions")
            .iter()
            .find(|mission| mission["id"] == id)
            .cloned()
            .expect("the mission")
    }
}

/// A bare source with one commit on main, under a fresh directory, and
/// an integrator whose workspace root is beside it: `(base, source,
/// scenarios, integrator)`.
fn world(tag: &str, gate: &str) -> (PathBuf, String, PathBuf, Integrator) {
    let base = std::env::temp_dir().join(format!("spawn-db-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let seed = base.join("seed");
    std::fs::create_dir_all(&seed).expect("seed");
    let seed_path = seed.to_string_lossy().to_string();
    git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
    std::fs::write(seed.join("README.md"), "base\n").expect("write");
    super::tests::commit_in(&seed_path, "base");
    let source = base.join("source.git").to_string_lossy().to_string();
    git(&["clone", "-q", "--bare", &seed_path, &source]).expect("bare source");
    let scenarios = base.join("scenarios");
    std::fs::create_dir_all(&scenarios).expect("scenarios");
    let gate_path = base.join("gate.sh");
    std::fs::write(&gate_path, gate).expect("gate");
    let integrator = Integrator::new(
        base.join("host"),
        gate_path,
        test_gate_env(),
        room_to_spare(),
        super::DEFAULT_GATES,
    );
    (base, source, scenarios, integrator)
}

/// The checks before a gate with their defaults, on a host with room to
/// spare: whatever this test host has, the stub gates run.
fn room_to_spare() -> crate::gate::Room {
    crate::gate::Room::from_env(|_| None).measured_by(
        |_| Some(u64::MAX),
        || Some(u64::MAX),
        |_| Ok(()),
    )
}

/// Main moves on the source by one commit, pushed from a clone; the
/// integrator's mirror does not follow. Main's new commit.
fn main_moves(base: &Path, source: &str, file: &str) -> String {
    let clone = base.join(format!("clone-{file}"));
    let clone_path = clone.to_string_lossy().to_string();
    git(&["clone", "-q", source, &clone_path]).expect("clone");
    std::fs::write(clone.join(file), "moved\n").expect("write");
    super::tests::commit_in(&clone_path, "main moves");
    git(&["-C", &clone_path, "push", "-q", "origin", "main"]).expect("push main");
    git(&["--git-dir", source, "rev-parse", "refs/heads/main"]).expect("main")
}

fn source_main(source: &str) -> String {
    git(&["--git-dir", source, "rev-parse", "refs/heads/main"]).expect("main")
}

/// The stub gates' environment: PATH only, for `sleep` and `cat`.
fn test_gate_env() -> Vec<(String, String)> {
    std::env::var("PATH")
        .map(|path| vec![("PATH".to_string(), path)])
        .unwrap_or_default()
}

/// `agent`'s work on `source`'s main, pushed as `spawn/<agent>`: one new
/// file. The pushed commit.
fn push_work(base: &Path, source: &str, agent: &str, file: &str) -> String {
    let clone = base.join(format!("agent-{file}"));
    let clone_path = clone.to_string_lossy().to_string();
    git(&["clone", "-q", source, &clone_path]).expect("clone");
    std::fs::write(clone.join(file), "work\n").expect("write");
    super::tests::commit_in(&clone_path, "work");
    git(&[
        "-C",
        &clone_path,
        "push",
        "-q",
        "origin",
        &format!("HEAD:refs/heads/spawn/{agent}"),
    ])
    .expect("push the agent's branch");
    git(&["-C", &clone_path, "rev-parse", "HEAD"]).expect("work")
}

/// A gate that notes the candidate it judges in `<marks>/started`, then
/// holds until `<marks>/release` exists, and accepts.
fn holding_gate(marks: &Path) -> String {
    let marks = marks.to_string_lossy();
    format!(
        "echo \"$3\" >>'{marks}/started'\n\
             while [ ! -e '{marks}/release' ]; do sleep 0.05; done\n\
             printf '{{\"verdict\":\"accepted\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n"
    )
}

/// Releases the holding gates when a test ends, a failed one too, so no
/// stub gate outlives it.
struct Release(PathBuf);

impl Drop for Release {
    fn drop(&mut self) {
        let _ = std::fs::write(self.0.join("release"), "");
    }
}

/// Stops the gate units of one test's integrators when the test ends, a
/// failed one too: a unit outlives the test that started it (gate v2
/// step 2). Only that test's: the units its judgings under the root
/// name (a unit is named after its private directory,
/// `<root>/judgings/<id>/<unit>`), never every unit of the test process.
/// The suite runs its tests on threads of one process, so the units of
/// tests running beside it share its pid: a pid-wide stop at the end of
/// one test killed the gate another was still waiting on, whose
/// adopter then found the runner gone without a status (P24).
struct StopUnits(PathBuf);

impl Drop for StopUnits {
    fn drop(&mut self) {
        let judgings = self.0.join(super::judging::DIRECTORY);
        for judging in std::fs::read_dir(&judgings).into_iter().flatten().flatten() {
            for private in std::fs::read_dir(judging.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let name = private.file_name().to_string_lossy().to_string();
                if !name.starts_with("spawn-gate-") {
                    continue;
                }
                let _ = std::process::Command::new("systemctl")
                    .args(["--user", "stop", "--", &format!("{name}.service")])
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
    }
}

fn started(marks: &Path) -> Vec<String> {
    std::fs::read_to_string(marks.join("started"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Wait until `count` gates started (a gate starts on its own thread).
fn wait_started(marks: &Path, count: usize) -> Vec<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let started = started(marks);
        if started.len() >= count || std::time::Instant::now() > deadline {
            return started;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// The gate's environment names the shared test server (this test's
/// own), so every gate that gets a test database shares it and
/// `--gates` bounds them.
fn on_the_shared_server(integrator: &mut Integrator) {
    let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect("SPAWN_TEST_DATABASE_URL");
    integrator
        .judge
        .gate_env
        .push(("SPAWN_TEST_DATABASE_URL".to_string(), url));
}

fn agent_of(db: &Db, mission: i64, task: i64) -> String {
    db.mission(mission)["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .find(|row| row["id"] == task)
        .and_then(|row| row["agent_id"].as_str())
        .expect("the task's agent")
        .to_string()
}

/// An empty directory for a holding gate's marks.
fn fresh_marks(name: &str) -> PathBuf {
    let marks = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&marks);
    std::fs::create_dir_all(&marks).expect("marks");
    marks
}

/// The integrator's mirror of `source`.
fn mirror_of(integrator: &Integrator, source: &str) -> String {
    crate::sources::ensure_mirror(&integrator.judge.root, Some(source))
        .expect("mirror")
        .to_string_lossy()
        .to_string()
}

/// A landing commit's parent `n` (1: the main it lands on, 2: the agent's
/// head), read from the first of `repos` that has the commit: a gate is
/// given the landing commit, one new merge per landing.
fn parent(repos: &[&str], commit: &str, n: u8) -> String {
    repos
        .iter()
        .find_map(|repo| git(&["--git-dir", repo, "rev-parse", &format!("{commit}^{n}")]).ok())
        .unwrap_or_else(|| panic!("no parent {n} of {commit} in {repos:?}"))
}

/// Gates run per mission, concurrently: mission A has two ready tasks,
/// B one on a source of its own, and D one on A's source; three gates
/// may run. The first pass starts A's first task and B's, and nothing
/// else through any number of passes although a gate is free: A's second
/// waits for A's first (a mission's tasks stay serial, each judged
/// against the main the one before left), and D for A's (they share the
/// mirror and main). Then D's runs, judged against the main A's first
/// left: it has waited since it was reported, A's second only since A's
/// first ended (the longest wait gets the gate). Then A's second.
#[test]
fn gates_run_per_mission_concurrently() {
    let marks = fresh_marks("spawn-db-gate-marks");
    let _release = Release(marks.clone());
    let (base_a, source_a, scenarios, mut integrator) =
        world("concurrent-a", &holding_gate(&marks));
    integrator.gates = 3;
    let (base_b, source_b, _, _) = world("concurrent-b", "exit 1\n");
    let db = Db::new("concurrent_gates");
    let a = db.gated_mission(&source_a, &scenarios);
    let b = db.gated_mission(&source_b, &scenarios);
    let d = db.gated_mission(&source_a, &scenarios);
    let a_one = db.reported_task(a, "a1");
    let a_two = db.reported_task(a, "a2");
    let b_task = db.reported_task(b, "b1");
    let d_task = db.reported_task(d, "d1");
    let pushed_a_one = push_work(&base_a, &source_a, &agent_of(&db, a, a_one), "a1.txt");
    let pushed_a_two = push_work(&base_a, &source_a, &agent_of(&db, a, a_two), "a2.txt");
    let pushed_b = push_work(&base_b, &source_b, &agent_of(&db, b, b_task), "b1.txt");
    let pushed_d = push_work(&base_a, &source_a, &agent_of(&db, d, d_task), "d1.txt");
    let hold = |count: usize| {
        for _ in 0..3 {
            integrator
                .pass_on(&db.store)
                .expect("a pass while gates run");
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        let now = started(&marks);
        assert_eq!(now.len(), count, "nothing else starts: {now:?}");
        now
    };
    let release = || {
        std::fs::write(marks.join("release"), "").expect("release");
        integrator.settle();
        std::fs::remove_file(marks.join("release")).expect("hold again");
    };

    integrator.pass_on(&db.store).expect("the first pass");
    let (mirror_a, mirror_b) = (
        mirror_of(&integrator, &source_a),
        mirror_of(&integrator, &source_b),
    );
    let repos = [mirror_a.as_str(), mirror_b.as_str()];
    let mut first: Vec<String> = wait_started(&marks, 2)
        .iter()
        .map(|commit| parent(&repos, commit, 2))
        .collect();
    first.sort();
    let mut expected = vec![pushed_a_one.clone(), pushed_b.clone()];
    expected.sort();
    assert_eq!(first, expected, "A's first and B's, at once");
    hold(2);
    assert_eq!(db.mission(d)["tasks"][0]["state"], "reported");
    release();
    assert_eq!(
        parent(&[&source_b], &source_main(&source_b), 2),
        pushed_b,
        "B's landed"
    );
    let a_first_main = source_main(&source_a);
    assert_eq!(
        parent(&[&source_a], &a_first_main, 2),
        pushed_a_one,
        "A's first landed"
    );

    integrator.pass_on(&db.store).expect("the next pass");
    let second = wait_started(&marks, 3);
    assert_eq!(
        parent(&repos, &second[2], 2),
        pushed_d,
        "D's, the longest wait"
    );
    assert_eq!(
        parent(&repos, &second[2], 1),
        a_first_main,
        "judged against the main A's first left"
    );
    hold(3);
    release();
    assert_eq!(db.mission(d)["tasks"][0]["state"], "landed");
    let d_main = source_main(&source_a);

    integrator.pass_on(&db.store).expect("the pass after");
    let third = wait_started(&marks, 4);
    assert_eq!(parent(&repos, &third[3], 2), pushed_a_two, "A's second");
    assert_eq!(
        parent(&repos, &third[3], 1),
        d_main,
        "judged against the main D's left"
    );
    std::fs::write(marks.join("release"), "").expect("release");
    integrator.settle();
    assert!(
        db.mission(a)["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .all(|row| row["state"] == "landed"),
        "{}",
        db.mission(a)
    );
    for base in [base_a, base_b, marks] {
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// The gate goes to the ready task that has waited longest, across
/// missions, not to the lowest mission id: A and B are always ready
/// (two tasks each, reported A1, A2, B1, B2) and there is one gate. A2
/// waits from the end of A1's judging, after B1 was reported, so the
/// missions take turns: A1, B1, A2, B2. Neither waits more than one
/// gate behind the other.
#[test]
fn two_missions_always_ready_take_turns_at_one_gate() {
    let marks = fresh_marks("spawn-db-gate-turns");
    let _release = Release(marks.clone());
    let (base_a, source_a, scenarios, mut integrator) = world("turns-a", &holding_gate(&marks));
    integrator.gates = 1;
    on_the_shared_server(&mut integrator);
    let (base_b, source_b, _, _) = world("turns-b", "exit 1\n");
    let db = Db::new("turns");
    let a = db.gated_mission(&source_a, &scenarios);
    let b = db.gated_mission(&source_b, &scenarios);
    let mut pushed = Vec::new();
    for (mission, base, source, worker) in [
        (a, &base_a, &source_a, "a1"),
        (a, &base_a, &source_a, "a2"),
        (b, &base_b, &source_b, "b1"),
        (b, &base_b, &source_b, "b2"),
    ] {
        let task = db.reported_task(mission, worker);
        pushed.push(push_work(
            base,
            source,
            &agent_of(&db, mission, task),
            &format!("{worker}.txt"),
        ));
        // Reported at distinct times.
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    for turn in 1..=4 {
        integrator.pass_on(&db.store).expect("a pass");
        assert_eq!(
            wait_started(&marks, turn).len(),
            turn,
            "gate {turn} started"
        );
        integrator.pass_on(&db.store).expect("a pass while it runs");
        std::fs::write(marks.join("release"), "").expect("release");
        integrator.settle();
        std::fs::remove_file(marks.join("release")).expect("hold again");
    }
    let (mirror_a, mirror_b) = (
        mirror_of(&integrator, &source_a),
        mirror_of(&integrator, &source_b),
    );
    let repos = [mirror_a.as_str(), mirror_b.as_str()];
    let judged: Vec<String> = started(&marks)
        .iter()
        .map(|commit| parent(&repos, commit, 2))
        .collect();
    let order = |heads: &[&String]| heads.iter().map(|head| (*head).clone()).collect::<Vec<_>>();
    assert_eq!(
        judged,
        order(&[&pushed[0], &pushed[2], &pushed[1], &pushed[3]]),
        "A1, B1, A2, B2"
    );
    for base in [base_a, base_b, marks] {
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// An objective mission with a ready backlog, its resident, and up to
/// `workers` tasks in flight.
fn objective_mission(
    db: &Db,
    base: &Path,
    source: &str,
    scenarios: &Path,
    name: &str,
    workers: u32,
) -> i64 {
    let backlog = base.join(format!("backlog-{name}"));
    std::fs::create_dir_all(&backlog).expect("backlog");
    std::fs::write(
        backlog.join("001.json"),
        r#"{"id": 1, "title": "One", "prompt": "Do one."}"#,
    )
    .expect("objective");
    db.store
        .resident_propose(name, "Pursues objectives")
        .expect("propose");
    db.store.resident_approve(name).expect("approve");
    let mission = db
        .store
        .mission_add(name, &json!([source]), "human")
        .expect("add");
    db.store
        .mission_configure(
            mission,
            Some(&scenarios.to_string_lossy()),
            "gate",
            None,
            Some(
                &json!({"backlog": backlog.to_string_lossy(), "resident": name,
                         "workers": workers}),
            ),
        )
        .expect("configure");
    mission
}

/// A mission with a ready task does not delay another mission's
/// objective assignment: A (the lower id) assigns an objective and has a
/// task to judge, whose gate holds; B, on another source, gets its
/// objective in the same pass, and again while A's gate runs.
#[test]
fn a_ready_mission_does_not_delay_another_missions_assignment() {
    let marks = fresh_marks("spawn-db-gate-assign");
    let _release = Release(marks.clone());
    let (base_a, source_a, scenarios, mut integrator) = world("assign-a", &holding_gate(&marks));
    integrator.gates = 1;
    let (base_b, source_b, _, _) = world("assign-b", "exit 1\n");
    let db = Db::new("assign_not_delayed");
    let a = objective_mission(&db, &base_a, &source_a, &scenarios, "ra", 2);
    let ready = db.reported_task(a, "w");
    let pushed = push_work(&base_a, &source_a, &agent_of(&db, a, ready), "w.txt");
    let b = objective_mission(&db, &base_b, &source_b, &scenarios, "rb", 1);
    let tasks = |mission: i64| db.mission(mission)["tasks"].as_array().map_or(0, Vec::len);
    assert_eq!((tasks(a), tasks(b)), (1, 0));

    integrator.pass_on(&db.store).expect("one pass");
    assert_eq!(tasks(a), 2, "A assigned its objective");
    assert_eq!(tasks(b), 1, "B assigned its objective in the same pass");
    let mirror = crate::sources::ensure_mirror(&integrator.judge.root, Some(&source_a))
        .expect("mirror")
        .to_string_lossy()
        .to_string();
    let started = wait_started(&marks, 1);
    assert_eq!(parent(&[&mirror], &started[0], 2), pushed, "A's gate runs");
    integrator
        .pass_on(&db.store)
        .expect("a pass while A's gate holds");
    assert_eq!(tasks(b), 1, "B's one slot stays filled, nothing doubled");
    std::fs::write(marks.join("release"), "").expect("release");
    integrator.settle();
    for base in [base_a, base_b, marks] {
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// Gates with servers of their own run beside each other whatever the cap.
#[test]
fn gates_with_servers_of_their_own_are_not_held_by_the_cap() {
    // With SPAWN_GATE_PG_BIN every gate runs a PostgreSQL server of its
    // own, so `--gates` (a bound on the shared server) does not hold
    // them: two missions' gates run at once under a cap of one (Supply PA
    // and Spawn were serialized for no reason, 2026-11-09).
    let marks = fresh_marks("spawn-db-gate-own-servers");
    let _release = Release(marks.clone());
    let (base_a, source_a, scenarios, mut integrator) =
        world("own-servers-a", &holding_gate(&marks));
    integrator.gates = 1;
    on_the_shared_server(&mut integrator);
    integrator.judge.gate_env.push((
        "SPAWN_GATE_PG_BIN".to_string(),
        "/usr/lib/postgresql/16/bin".to_string(),
    ));
    let (base_b, source_b, _, _) = world("own-servers-b", "exit 1\n");
    let db = Db::new("own_servers");
    let a = db.gated_mission(&source_a, &scenarios);
    let b = db.gated_mission(&source_b, &scenarios);
    let a_one = db.reported_task(a, "a1");
    let b_one = db.reported_task(b, "b1");
    push_work(&base_a, &source_a, &agent_of(&db, a, a_one), "a1.txt");
    push_work(&base_b, &source_b, &agent_of(&db, b, b_one), "b1.txt");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.pass_on(&db.store).expect("another pass");
    assert_eq!(
        wait_started(&marks, 2).len(),
        2,
        "both missions' gates run at once"
    );
    std::fs::write(marks.join("release"), "").expect("release");
    integrator.settle();
    for base in [base_a, base_b, marks] {
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// A cap of one is one gate at a time across missions, as before.
#[test]
fn a_cap_of_one_gates_one_mission_at_a_time() {
    let marks = fresh_marks("spawn-db-gate-cap");
    let _release = Release(marks.clone());
    let (base_a, source_a, scenarios, mut integrator) = world("cap-one-a", &holding_gate(&marks));
    integrator.gates = 1;
    on_the_shared_server(&mut integrator);
    let (base_b, source_b, _, _) = world("cap-one-b", "exit 1\n");
    let db = Db::new("cap_one");
    let a = db.gated_mission(&source_a, &scenarios);
    let b = db.gated_mission(&source_b, &scenarios);
    let a_one = db.reported_task(a, "a1");
    let b_task = db.reported_task(b, "b1");
    let pushed_a_one = push_work(&base_a, &source_a, &agent_of(&db, a, a_one), "a1.txt");
    let pushed_b = push_work(&base_b, &source_b, &agent_of(&db, b, b_task), "b1.txt");

    integrator.pass_on(&db.store).expect("a pass");
    let (mirror_a, mirror_b) = (
        mirror_of(&integrator, &source_a),
        mirror_of(&integrator, &source_b),
    );
    let repos = [mirror_a.as_str(), mirror_b.as_str()];
    let heads = || -> Vec<String> {
        started(&marks)
            .iter()
            .map(|commit| parent(&repos, commit, 2))
            .collect()
    };
    integrator.pass_on(&db.store).expect("another pass");
    wait_started(&marks, 1);
    assert_eq!(heads(), vec![pushed_a_one.clone()]);
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(heads(), vec![pushed_a_one], "B waits for the one gate");
    std::fs::write(marks.join("release"), "").expect("release");
    integrator.settle();
    integrator.pass_on(&db.store).expect("the pass after");
    integrator.settle();
    assert_eq!(heads().last(), Some(&pushed_b));
    assert_eq!(db.mission(b)["tasks"][0]["state"], "landed");
    for base in [base_a, base_b, marks] {
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// One event of `task`'s session at `at`, written straight into the log.
fn session_event(db: &Db, task: &super::Task, at: i64, name: &str, detail: &Value) {
    db.admin()
        .execute(
            "insert into spawn_events (agent_id, session_id, source, kind, session_seq, recorded_at_ms)
                 values ($1, $2, 'test', jsonb_build_object('name', $3::text, 'detail', $4::jsonb),
                         (select coalesce(max(session_seq), 0) + 1 from spawn_events
                           where session_id = $2),
                         $5)",
            &[&task.agent_id, &task.session_id, &name, detail, &at],
        )
        .expect("write an event");
}

/// Tool calls every 30 s in `task`'s session from `from` to `to`, with
/// the Bridge's screen readings between them.
fn tool_work(db: &Db, task: &super::Task, from: i64, to: i64) {
    let mut at = from;
    let mut call = 0;
    while at < to {
        let id = json!({"tool_use_id": format!("toolu_{}_{call}", task.id)});
        session_event(db, task, at, "tool.started", &id);
        session_event(db, task, at + 10_000, "tool.completed", &id);
        session_event(db, task, at + 20_000, "screen.observed", &json!({}));
        at += 30_000;
        call += 1;
    }
}

/// The integrator's budget pass end to end, on the store: a task that
/// worked 13 minutes, then sat through a five-hour stop of the fleet,
/// is not aborted by the first pass after it (2026-11-07 23:05:46, task
/// 159); one that worked past its budget is, with the worker stopped.
#[test]
fn the_budget_pass_aborts_an_overrun_and_not_a_stop() {
    let db = Db::new("budget_pass");
    let mission = db
        .store
        .mission_add("budget", &json!(["/nowhere.git"]), "human")
        .expect("add");
    db.store
        .mission_configure(
            mission,
            Some("/nowhere/scenarios"),
            "gate",
            Some(&json!({"minutes": 60})),
            None,
        )
        .expect("configure");
    let gated = || -> super::Mission {
        let missions: Vec<super::Mission> =
            serde_json::from_value(db.store.gated_missions().expect("gated")).expect("missions");
        missions
            .into_iter()
            .find(|row| row.id == mission)
            .expect("the mission")
    };
    let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
    let min = 60_000;

    let stopped = db
        .store
        .task_assign(mission, "ephemeral", "worker_stopped", "brief", "default")
        .expect("assign");
    db.store.task_delivered(stopped, 1).expect("delivered");
    let delivered = now - 5 * 60 * min - 13 * min;
    db.admin()
        .execute(
            "update mission_tasks set delivered_at_ms = $2 where id = $1",
            &[&stopped, &delivered],
        )
        .expect("deliver five hours ago");
    let task = ready_task(&db, stopped);
    tool_work(&db, &task, delivered + 5_000, delivered + 13 * min);
    // The fleet stopped with a tool open; the restart's commands.
    session_event(
        &db,
        &task,
        delivered + 13 * min,
        "tool.started",
        &json!({"tool_use_id": "toolu_open"}),
    );
    session_event(&db, &task, now - min, "command.accepted", &json!({}));
    assert_eq!(
        Integrator::enforce_budget(&db.store, &gated()).expect("the pass"),
        Vec::<i64>::new(),
        "five hours of wall clock, about 20 minutes of work"
    );
    assert_eq!(task_state(&db, mission, stopped), "delivered");

    let overran = db
        .store
        .task_assign(mission, "ephemeral", "worker_overran", "brief", "default")
        .expect("assign");
    db.store.task_delivered(overran, 1).expect("delivered");
    let delivered = now - 70 * min;
    db.admin()
        .execute(
            "update mission_tasks set delivered_at_ms = $2 where id = $1",
            &[&overran, &delivered],
        )
        .expect("deliver 70 minutes ago");
    tool_work(&db, &ready_task(&db, overran), delivered + 5_000, now);
    assert_eq!(
        Integrator::enforce_budget(&db.store, &gated()).expect("the pass"),
        vec![overran]
    );
    assert_eq!(
        task_state(&db, mission, stopped),
        "delivered",
        "the stop still counts once"
    );
    assert_eq!(task_state(&db, mission, overran), "cancelled");
    let stops: i64 = db
        .admin()
        .query_one(
            "select count(*) from spawn_commands where idempotency_key = $1",
            &[&format!("integrate-abort-{overran}")],
        )
        .expect("the stop")
        .get(0);
    assert_eq!(stops, 1, "the worker is stopped");
}

/// An objective mission on no real source: a backlog of `objectives`,
/// an approved resident, `workers` slots and `budget`.
fn backlog_mission(
    db: &Db,
    base: &Path,
    name: &str,
    objectives: &[u32],
    workers: u32,
    budget: Option<&Value>,
) -> i64 {
    let backlog = base.join("backlog");
    let scenarios = base.join("scenarios");
    std::fs::create_dir_all(&backlog).expect("backlog");
    std::fs::create_dir_all(&scenarios).expect("scenarios");
    for id in objectives {
        std::fs::write(
            backlog.join(format!("{id:03}.json")),
            json!({"id": id, "title": format!("Objective {id}"), "prompt": "Do it."}).to_string(),
        )
        .expect("objective");
    }
    db.store
        .resident_propose(name, "Pursues objectives")
        .expect("propose");
    db.store.resident_approve(name).expect("approve");
    let mission = db
        .store
        .mission_add(
            name,
            &json!([base.join("source.git").to_string_lossy()]),
            "human",
        )
        .expect("add");
    db.store
        .mission_configure(
            mission,
            Some(&scenarios.to_string_lossy()),
            "gate",
            budget,
            Some(
                &json!({"backlog": backlog.to_string_lossy(), "resident": name,
                         "workers": workers}),
            ),
        )
        .expect("configure");
    mission
}

/// The mission as the integrator reads it in a pass.
fn snapshot(db: &Db, mission: i64) -> super::Mission {
    let missions: Vec<super::Mission> =
        serde_json::from_value(db.store.gated_missions().expect("gated")).expect("missions");
    missions
        .into_iter()
        .find(|row| row.id == mission)
        .expect("the mission")
}

/// `(task, objective)` of the mission's tasks in flight, by task.
fn flying(db: &Db, mission: i64) -> Vec<(i64, u32)> {
    let mut tasks: Vec<(i64, u32)> = snapshot(db, mission)
        .tasks
        .iter()
        .filter_map(|task| Some((task.id, task.objective?)))
        .collect();
    tasks.sort_unstable();
    tasks
}

fn objective_task(db: &Db, mission: i64, objective: u32) -> super::Task {
    snapshot(db, mission)
        .tasks
        .into_iter()
        .find(|task| task.objective == Some(objective))
        .expect("the objective's task")
}

/// `task` delivered, reported, and sent back by `count` gate verdicts
/// `verdict`, from revision `from` on: a rejection, or a conflict of a
/// merge that left markers (main moved: not a failed attempt).
fn sent_back(db: &Db, task: i64, verdict: &str, from: i32, count: i32) {
    for revision in from..from + count {
        db.store.task_delivered(task, 1).expect("delivered");
        assert!(
            db.store
                .task_reported(task, "## Outcome\nhalf of it")
                .expect("reported")
        );
        let record = if verdict == "conflict" {
            json!({"type": "task.gated", "id": task, "revision": revision,
                   "verdict": "conflict", "merge": "marked", "base": "b",
                   "reason": "main conflicts with your branch"})
        } else {
            json!({"type": "task.gated", "id": task, "revision": revision,
                   "verdict": verdict, "base": "b", "note": "a check failed"})
        };
        db.record(&record);
    }
}

/// A checkpoint of `task`'s session at `at`, written straight into the log.
fn checkpoint(db: &Db, task: &super::Task, at: i64, commit: &str, reason: &str) {
    db.admin()
        .execute(
            "insert into spawn_events (agent_id, session_id, source, kind, session_seq,
                                           recorded_at_ms)
                 values ($1, $2, 'bridge',
                         jsonb_build_object('name', 'artifact.committed', 'detail',
                           jsonb_build_object('reason', $3::text, 'artifact',
                             jsonb_build_object('branch', $1::text, 'commit', $4::text,
                                                'project', 'missions'))),
                         (select coalesce(max(session_seq), 0) + 1 from spawn_events
                           where session_id = $2), $5)",
            &[&task.agent_id, &task.session_id, &reason, &commit, &at],
        )
        .expect("a checkpoint");
}

/// The integrator cancels only an objective's task being worked, to
/// yield: not without `yielded`, not `reported` (an objective's task
/// waiting for its gate), not a task of a mission without objectives.
fn integrator_refuses_other_cancels(db: &Db, base: &Path, objective_task: i64) {
    let cancel = |task: i64, yielded: bool| {
        db.store
            .integrator_record(&json!({"type": "task.cancelled", "id": task,
                                       "yielded": yielded}))
    };
    assert!(cancel(objective_task, false).is_err(), "it does not yield");
    db.store
        .task_delivered(objective_task, 1)
        .expect("delivered");
    assert!(
        db.store
            .task_reported(objective_task, "again")
            .expect("reported")
    );
    assert!(
        cancel(objective_task, true).is_err(),
        "it waits for its gate"
    );
    let plain = db.gated_mission("/nowhere.git", &base.join("scenarios"));
    let plain_task = db
        .store
        .task_assign(plain, "ephemeral", "w", "brief", "default")
        .expect("assign");
    assert!(cancel(plain_task, true).is_err(), "no objective");
}

/// `task.cancelled` records by spawn-integrate for `tasks` alone, each
/// with its worker stopped once.
fn yields_recorded(db: &Db, tasks: &[i64]) {
    let mut admin = db.admin();
    let yielded: Vec<(i64, String)> = admin
        .query(
            "select (kind -> 'detail' -> 'record' ->> 'id')::bigint,
                        kind -> 'detail' -> 'record' ->> 'by'
                   from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.cancelled'
                  order by session_seq",
            &[],
        )
        .expect("the yields")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    let expected: Vec<(i64, String)> = tasks
        .iter()
        .map(|task| (*task, "spawn-integrate".to_string()))
        .collect();
    assert_eq!(yielded, expected);
    let keys: Vec<String> = tasks
        .iter()
        .map(|task| format!("integrate-yield-{task}"))
        .collect();
    let stops: i64 = admin
        .query_one(
            "select count(*) from spawn_commands where idempotency_key = any($1)",
            &[&keys],
        )
        .expect("the stops")
        .get(0);
    assert_eq!(
        stops,
        i64::try_from(tasks.len()).expect("few"),
        "each worker stopped"
    );
}

/// Mission 4's stall (2026-11-07), on the store: three slots, two of
/// them held by objectives at their limit of failed attempts, one by
/// an objective under it. The pass yields the two (worker stopped,
/// `task.cancelled` with `yielded` by spawn-integrate, the supervisor
/// told once), leaves the third, and fills the slots fewest attempts
/// first: 025, then 019 again, whose brief names the yielded task's
/// branch and last checkpoint (not its set-aside). The store refuses
/// any other cancel by the integrator.
#[test]
fn objectives_at_their_limit_yield_and_the_next_get_the_slots() {
    let base = std::env::temp_dir().join(format!("spawn-db-yield-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let db = Db::new("objective_yield");
    let mission = backlog_mission(&db, &base, "ry", &[19, 21, 23, 25], 3, None);
    let none = std::collections::BTreeSet::new();
    let pass = || {
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("a pass")
    };
    pass();
    let first = flying(&db, mission);
    assert_eq!(
        first.iter().map(|(_, o)| *o).collect::<Vec<_>>(),
        [19, 21, 23]
    );
    let task_19 = objective_task(&db, mission, 19);
    let (t19, t21, t23) = (first[0].0, first[1].0, first[2].0);
    sent_back(&db, t19, "rejected", 0, 3);
    sent_back(&db, t21, "rejected", 0, 3);
    // 023: two rejections, then three conflicts (main moving), which do
    // not count: it stays under the limit.
    sent_back(&db, t23, "rejected", 0, 2);
    sent_back(&db, t23, "conflict", 2, 3);
    let assigned_at: i64 = db
        .admin()
        .query_one(
            "select created_at_ms from mission_tasks where id = $1",
            &[&t19],
        )
        .expect("the task")
        .get(0);
    let work = "1111111111111111111111111111111111111111";
    let before = "0000000000000000000000000000000000000000";
    checkpoint(&db, &task_19, assigned_at - 1, before, "tool_completed");
    checkpoint(&db, &task_19, assigned_at + 1, before, "tool_completed");
    checkpoint(&db, &task_19, assigned_at + 2, work, "tool_completed");
    integrator_refuses_other_cancels(&db, &base, t23);

    assert_eq!(pass().len(), 2);
    assert_eq!(task_state(&db, mission, t19), "cancelled");
    assert_eq!(task_state(&db, mission, t21), "cancelled");
    assert_eq!(
        task_state(&db, mission, t23),
        "reported",
        "under its limit, untouched"
    );
    let now_flying: Vec<u32> = flying(&db, mission)
        .iter()
        .filter(|(task, _)| *task != t19 && *task != t21)
        .map(|(_, objective)| *objective)
        .collect();
    assert_eq!(
        now_flying,
        [23, 25, 19],
        "by task: 023 kept, then 025, and 019 again"
    );
    yields_recorded(&db, &[t19, t21]);
    let told = db.store.objective_yields().expect("the yields to tell");
    assert_eq!(told.as_array().map(Vec::len), Some(2));

    let again = objective_task(&db, mission, 19).brief;
    for part in [
        "Their reports below say what each did and found".to_string(),
        format!("- task {t19}: yielded (objective 19 had 3 failed attempts"),
        format!("its last checkpoint {work}"),
        "> half of it".to_string(),
    ] {
        assert!(again.contains(&part), "{part}: {again}");
    }
    // The set-aside the dispatcher records after the cancel is not the work.
    checkpoint(&db, &task_19, assigned_at + 3, &"2".repeat(40), "set_aside");
    let prior = db
        .store
        .objective_prior_work(mission, 19, 3)
        .expect("prior work");
    assert_eq!(prior[0]["commit"], work);
    // Objective 177: after a history rewrite, the prior work by its new commit.
    let rewritten = "3".repeat(40);
    db.store
        .commit_map_load(&[(work.to_string(), Some(rewritten.clone()))])
        .expect("the map");
    assert_eq!(
        db.store
            .objective_prior_work(mission, 19, 3)
            .expect("prior work")[0]["commit"],
        rewritten.as_str()
    );
    let fresh = objective_task(&db, mission, 25).brief;
    assert!(!fresh.contains("Earlier tasks"), "a first task has none");

    assert!(pass().is_empty(), "nothing more yields, no slot is free");
    let _ = std::fs::remove_dir_all(&base);
}

/// The run of gate rejections as the store folds it: its length and the
/// objective all of it was (`gate_rejection_objective`), or none.
fn rejection_run(db: &Db, mission: i64) -> (u32, Option<u32>) {
    let row = snapshot(db, mission);
    (row.gate_rejections, row.gate_rejection_objective)
}

/// The `task.cancelled` record of `task`, as the store keeps it.
fn cancel_record(db: &Db, task: i64) -> Value {
    db.admin()
        .query_one(
            "select kind -> 'detail' -> 'record' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.cancelled'
                    and kind -> 'detail' -> 'record' ->> 'id' = $1::bigint::text",
            &[&task],
        )
        .expect("the cancel")
        .get(0)
}

/// Batch 11 (design-halt-per-objective.md), on the store: three gate
/// rejections in a row, all objective 19's task, halt nothing. The pass
/// yields 19 (worker stopped, `task.cancelled` with `yielded` and
/// `rejections` 3, the reason saying why) and gives the slot to 21; the
/// yield ends the run. 21's rejections then start a run of their own,
/// and a `stale` verdict between two of them neither counts nor names
/// another objective: the run is 2, still 21's.
#[test]
fn three_rejections_of_one_objective_yield_it_and_halt_nothing() {
    let base = std::env::temp_dir().join(format!("spawn-db-halt-obj-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let db = Db::new("halt_per_objective");
    let mission = backlog_mission(&db, &base, "rh", &[19, 21], 1, None);
    let none = std::collections::BTreeSet::new();
    let pass = || {
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("a pass")
    };
    pass();
    let t19 = objective_task(&db, mission, 19).id;
    sent_back(&db, t19, "rejected", 0, 2);
    assert_eq!(rejection_run(&db, mission), (2, Some(19)));
    sent_back(&db, t19, "rejected", 2, 1);
    assert_eq!(rejection_run(&db, mission), (3, Some(19)));
    assert_eq!(
        super::halt_reason(&snapshot(&db, mission), 0),
        None,
        "one objective's rejections: no halt"
    );

    assert_eq!(pass().len(), 1, "21 takes the slot");
    assert_eq!(task_state(&db, mission, t19), "cancelled");
    yields_recorded(&db, &[t19]);
    let cancel = cancel_record(&db, t19);
    assert_eq!(cancel["rejections"], 3, "{cancel}");
    assert_eq!(cancel["yielded"], true);
    assert_eq!(
        cancel["reason"].as_str(),
        Some(
            format!(
                "3 gate rejections in a row, all objective 19's tasks: task #{t19} yields \
                     its slot to the next objective instead of halting the mission, its work \
                     set aside"
            )
            .as_str()
        )
    );
    assert_eq!(
        rejection_run(&db, mission),
        (0, None),
        "the yield ends the run"
    );
    assert_eq!(db.mission(mission)["halted"], Value::Null);

    let t21 = objective_task(&db, mission, 21).id;
    sent_back(&db, t21, "rejected", 0, 1);
    assert_eq!(
        rejection_run(&db, mission),
        (1, Some(21)),
        "a run of its own"
    );
    db.store.task_delivered(t21, 2).expect("delivered");
    assert!(db.store.task_reported(t21, "again").expect("reported"));
    db.record(&json!({"type": "task.gated", "id": t21, "revision": 1,
                      "verdict": "stale", "reason": "behind"}));
    assert_eq!(
        rejection_run(&db, mission),
        (1, Some(21)),
        "stale: counted nowhere"
    );
    db.record(&json!({"type": "task.gated", "id": t21, "revision": 1,
                      "verdict": "rejected", "base": "b", "note": "a check failed"}));
    assert_eq!(rejection_run(&db, mission), (2, Some(21)));
    assert_eq!(super::halt_reason(&snapshot(&db, mission), 0), None);
    let _ = std::fs::remove_dir_all(&base);
}

/// rev-e on 67877e62, on the store: three rejections of objective 19,
/// then the owner cancels 19's task directly (no yield). The cancel ends
/// the run as a yield does, so the next rejection, of 21, is a run of
/// its own (1, 21) and halts nothing: the run is never mixed with one
/// whose objective is gone.
#[test]
fn an_owners_cancel_of_the_runs_objective_ends_the_run() {
    let base = std::env::temp_dir().join(format!("spawn-db-halt-cancel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let db = Db::new("halt_owner_cancel");
    let mission = backlog_mission(&db, &base, "rc", &[19, 21], 2, None);
    let none = std::collections::BTreeSet::new();
    crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("a pass");
    let (t19, t21) = (
        objective_task(&db, mission, 19).id,
        objective_task(&db, mission, 21).id,
    );
    sent_back(&db, t19, "rejected", 0, 3);
    assert_eq!(rejection_run(&db, mission), (3, Some(19)));
    db.store.task_cancel(t19).expect("the owner cancels");
    assert_eq!(task_state(&db, mission, t19), "cancelled");
    assert_eq!(
        rejection_run(&db, mission),
        (0, None),
        "the cancel ends the run"
    );
    sent_back(&db, t21, "rejected", 0, 1);
    assert_eq!(rejection_run(&db, mission), (1, Some(21)));
    assert_eq!(super::halt_reason(&snapshot(&db, mission), 0), None);
    assert_eq!(db.mission(mission)["halted"], Value::Null);
    let _ = std::fs::remove_dir_all(&base);
}

/// On the store: rejections of 19, 21, 19 span two objectives, so the
/// run names none and halts the mission as before; a yield of an
/// objective that is not the run's leaves the run; an acceptance ends
/// it; and a resume clears it.
#[test]
fn rejections_across_objectives_still_halt() {
    let base = std::env::temp_dir().join(format!("spawn-db-halt-mixed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let db = Db::new("halt_mixed");
    let mission = backlog_mission(&db, &base, "rm", &[19, 21], 2, None);
    let none = std::collections::BTreeSet::new();
    crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("a pass");
    let (t19, t21) = (
        objective_task(&db, mission, 19).id,
        objective_task(&db, mission, 21).id,
    );
    sent_back(&db, t19, "rejected", 0, 1);
    assert_eq!(rejection_run(&db, mission), (1, Some(19)));
    sent_back(&db, t21, "rejected", 0, 1);
    assert_eq!(rejection_run(&db, mission), (2, None), "two objectives");
    sent_back(&db, t19, "rejected", 1, 1);
    assert_eq!(rejection_run(&db, mission), (3, None), "none stays none");
    let row = snapshot(&db, mission);
    assert!(
        super::halt_reason(&row, 0).is_some_and(|r| r == "3 gate rejections in a row"),
        "{:?}",
        super::halt_reason(&row, 0)
    );
    let attempts = std::collections::BTreeMap::new();
    assert!(
        crate::objectives::yields(&row, &attempts, 99, &none).is_empty(),
        "a mixed run yields nothing"
    );
    db.record(&json!({"type": "mission.halted", "id": mission,
                      "reason": "3 gate rejections in a row"}));
    db.store.mission_resume(mission).expect("resume");
    assert_eq!(rejection_run(&db, mission), (0, None), "resume clears it");

    // A run of 21's; 19 yields (an objective's own rule): the run stays.
    sent_back(&db, t21, "rejected", 1, 2);
    assert_eq!(rejection_run(&db, mission), (2, Some(21)));
    db.record(
        &json!({"type": "task.cancelled", "id": t19, "yielded": true,
                      "objective": 19, "failures": 2, "limit": 3, "reason": "a test"}),
    );
    assert_eq!(
        rejection_run(&db, mission),
        (2, Some(21)),
        "another objective's yield"
    );
    // An acceptance ends it.
    db.store.task_delivered(t21, 4).expect("delivered");
    assert!(db.store.task_reported(t21, "again").expect("reported"));
    db.record(&json!({"type": "task.gated", "id": t21, "revision": 3,
                      "verdict": "accepted", "base": "b"}));
    assert_eq!(rejection_run(&db, mission), (0, None));
    let _ = std::fs::remove_dir_all(&base);
}

/// On the store: a mission without objectives (its tasks have none)
/// halts on three rejections as before; the run names no objective.
#[test]
fn three_rejections_of_a_plain_mission_halt() {
    let (base, source, scenarios, _) = world("halt-plain", "exit 1\n");
    let db = Db::new("halt_plain");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    for revision in 0..3 {
        if revision > 0 {
            db.store
                .task_delivered(task, revision + 1)
                .expect("delivered");
            assert!(db.store.task_reported(task, "again").expect("reported"));
        }
        db.record(
            &json!({"type": "task.gated", "id": task, "revision": revision,
                          "verdict": "rejected", "base": "b0", "note": "fails"}),
        );
    }
    let after = gated(&db, mission);
    assert_eq!(
        (after.gate_rejections, after.gate_rejection_objective),
        (3, None)
    );
    assert!(super::halt_reason(&after, 0).is_some_and(|r| r.contains("rejections")));
    let _ = std::fs::remove_dir_all(&base);
}

/// On the store: objective 023 has two failed attempts when its task
/// goes over budget. An overrun is its own record, no abort and no
/// yield: nothing is told to the supervisor as a yield, and the
/// objective's next task continues the work instead (task 174, "A task
/// over its budget keeps its work"). The record names the measure that
/// made it, `active`.
#[test]
fn an_overrun_at_the_limit_is_no_yield() {
    let base = std::env::temp_dir().join(format!("spawn-db-overrun-yield-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let db = Db::new("overrun_yield");
    let mission = backlog_mission(
        &db,
        &base,
        "ra",
        &[23, 25],
        2,
        Some(&json!({"minutes": 60})),
    );
    let none = std::collections::BTreeSet::new();
    let pass = || {
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("a pass")
    };
    pass();
    let first = objective_task(&db, mission, 23);
    let other = objective_task(&db, mission, 25);
    sent_back(&db, first.id, "rejected", 0, 2);
    let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
    let overrun = |task: &super::Task| {
        db.store.task_delivered(task.id, 1).expect("delivered");
        let delivered = now - 70 * 60_000;
        db.admin()
            .execute(
                "update mission_tasks set delivered_at_ms = $2 where id = $1",
                &[&task.id, &delivered],
            )
            .expect("deliver 70 minutes ago");
        tool_work(&db, task, delivered + 5_000, now);
    };
    overrun(&first);
    overrun(&other);
    let over = Integrator::enforce_budget(&db.store, &snapshot(&db, mission)).expect("the budget");
    assert_eq!(over, vec![first.id, other.id]);
    let record = |task: i64| -> Value {
        db.admin()
            .query_one(
                "select kind -> 'detail' -> 'record' from spawn_events
                      where session_id = 'session_control'
                        and kind -> 'detail' -> 'record' ->> 'type' = 'task.over_budget'
                        and kind -> 'detail' -> 'record' ->> 'id' = $1",
                &[&task.to_string()],
            )
            .expect("the overrun")
            .get(0)
    };
    // An overrun is its own record, at the attempt limit too: no abort,
    // no yield, no counter.
    for task in [first.id, other.id] {
        let overrun = record(task);
        assert!(overrun["tool_calls"].as_i64().unwrap_or_default() > 0);
        assert_eq!(overrun["budget"], json!({"minutes": 60}));
        assert_eq!(
            overrun["objective"].as_u64(),
            objective_of(&db, task).and_then(|objective| u64::try_from(objective).ok())
        );
    }
    let aborts: i64 = db
        .admin()
        .query_one(
            "select count(*) from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.aborted'",
            &[],
        )
        .expect("the aborts")
        .get(0);
    assert_eq!(aborts, 0, "an overrun is no abort");
    // The two rejections stand; the overrun adds no gate verdict.
    assert_eq!(
        gate_counters(&db, mission),
        (0, 2),
        "an overrun is no gate abort"
    );
    let told = db.store.objective_yields().expect("the yields to tell");
    assert!(
        told.as_array().expect("a list").is_empty(),
        "an overrun yields nothing"
    );

    // The next pass assigns 023's continuation of the overrun, and 025
    // gets its slot when one is free.
    pass();
    let mut flying: Vec<u32> = flying(&db, mission)
        .iter()
        .filter(|(task, _)| *task != first.id && *task != other.id)
        .map(|(_, objective)| *objective)
        .collect();
    flying.sort_unstable();
    assert_eq!(flying, [23, 25], "the continuation of the overrun, and 025");
    let again = objective_task(&db, mission, 23);
    assert_eq!(
        continues_of(&db, again.id),
        Some(first.id),
        "023's task continues the overrun"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// The pass that aborts a task over budget does not re-issue its
/// objective (task 113 aborted at 23:05:46, task 160 for objective 023
/// at 23:05:51): its slot frees from the next pass on, and goes to the
/// objective with the fewest attempts, 025, not 023.
#[test]
fn an_aborted_objective_is_not_reissued_at_once() {
    let base = std::env::temp_dir().join(format!("spawn-db-reissue-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let db = Db::new("objective_reissue");
    let mission = backlog_mission(
        &db,
        &base,
        "rr",
        &[23, 25],
        1,
        Some(&json!({"minutes": 60})),
    );
    let none = std::collections::BTreeSet::new();
    crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("first fill");
    let task = objective_task(&db, mission, 23);
    db.store.task_delivered(task.id, 1).expect("delivered");
    let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
    let delivered = now - 70 * 60_000;
    db.admin()
        .execute(
            "update mission_tasks set delivered_at_ms = $2 where id = $1",
            &[&task.id, &delivered],
        )
        .expect("deliver 70 minutes ago");
    tool_work(&db, &task, delivered + 5_000, now);
    let pass = snapshot(&db, mission);
    let over = Integrator::enforce_budget(&db.store, &pass).expect("the budget");
    assert_eq!(over, vec![task.id]);
    // The overrun names the active-time measure, so it is the
    // objective's failed attempt (an abort without one, the old
    // budget's, is not).
    let raw = db.store.objective_outcomes(mission).expect("outcomes");
    assert_eq!(raw[0]["measure"], "active", "{raw}");
    let settled = over.into_iter().collect();
    assert!(
        crate::objectives::assign_next(&db.store, &pass, &settled)
            .expect("the same pass")
            .is_empty(),
        "nothing in the pass that overran"
    );
    // The next pass assigns the overrun's continuation, and it says so:
    // the work is continued, not redone.
    crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none)
        .expect("the next pass");
    let next: Vec<(i64, u32)> = flying(&db, mission)
        .into_iter()
        .filter(|(id, _)| *id != task.id)
        .collect();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].1, 23, "the overrun's objective, continued");
    let again = objective_task(&db, mission, 23);
    assert_eq!(continues_of(&db, again.id), Some(task.id));
    let brief = again.brief;
    assert!(
        brief.contains(&format!("continues task #{}", task.id)),
        "{brief}"
    );
    assert!(
        brief.contains(&format!("- task {}: cancelled", task.id)),
        "the overrun task is the work the brief says ended: {brief}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A mission's run of gate aborts and rejections.
fn gate_counters(db: &Db, mission: i64) -> (i32, i32) {
    let row = db
        .admin()
        .query_one(
            "select gate_aborts, gate_rejections from mission_missions where id = $1",
            &[&mission],
        )
        .expect("the mission");
    (row.get(0), row.get(1))
}

/// The objective a task pursues, as the store folded it.
fn objective_of(db: &Db, task: i64) -> Option<i64> {
    db.admin()
        .query_one(
            "select objective from mission_tasks where id = $1",
            &[&task],
        )
        .expect("the task")
        .get::<_, Option<i32>>(0)
        .map(i64::from)
}

/// The task a continuation continues, as the store folded it.
fn continues_of(db: &Db, task: i64) -> Option<i64> {
    db.admin()
        .query_one(
            "select continues from mission_tasks where id = $1",
            &[&task],
        )
        .expect("the task")
        .get(0)
}

/// An overrun counts toward no halt: three in a row leave the mission's
/// counters alone, and each task leaves flight.
#[test]
fn three_overruns_halt_nothing() {
    let db = Db::new("overrun_halt");
    let (base, source, scenarios, _integrator) = world("overrun-halt", "#!/bin/sh\nexit 0\n");
    let mission = db.gated_mission(&source, &scenarios);
    let mut tasks = Vec::new();
    for worker in ["a", "b", "c"] {
        let task = db
            .store
            .task_assign(mission, "ephemeral", worker, "brief", "default")
            .expect("assign");
        db.store.task_delivered(task, 1).expect("delivered");
        db.record(&json!({"type": "task.over_budget", "id": task,
                          "reason": "the task made more than 5 tool calls",
                          "tool_calls": 6, "elapsed_ms": 1_000,
                          "budget": {"tool_calls": 5}}));
        tasks.push(task);
    }
    let row = snapshot(&db, mission);
    assert_eq!((row.gate_aborts, row.gate_rejections), (0, 0));
    assert_eq!(
        super::halt_reason(&row, 0),
        None,
        "no halt counts an overrun"
    );
    for task in tasks {
        let state: String = db
            .admin()
            .query_one("select state from mission_tasks where id = $1", &[&task])
            .expect("the task")
            .get(0);
        assert_eq!(state, "cancelled", "an overrun leaves flight");
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// The work an overrun keeps: its branch's commit and that commit's
/// diff stat against main; waited for while the push is pending, and
/// lost (said so) when it never reaches the source.
#[test]
fn an_overrun_keeps_the_work_its_branch_holds() {
    let db = Db::new("overrun_kept");
    let (base, source, scenarios, _integrator) = world("overrun-kept", "#!/bin/sh\nexit 0\n");
    let mission = db.gated_mission(&source, &scenarios);
    let task = ready_task(&db, db.reported_task(mission, "kept"));
    let clone = base.join("kept-clone");
    let clone_path = clone.to_string_lossy().to_string();
    git(&["clone", "-q", &source, &clone_path]).expect("clone");
    std::fs::write(clone.join("big.txt"), "line\n").expect("write");
    super::tests::commit_in(&clone_path, "the big work");
    let commit = git(&["-C", &clone_path, "rev-parse", "HEAD"]).expect("head");
    git(&[
        "-C",
        &clone_path,
        "push",
        "-q",
        "origin",
        &format!("{commit}:refs/heads/spawn/{}", task.agent_id),
    ])
    .expect("push the branch");
    let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
    checkpoint(&db, &task, now, &commit, "tool_completed");
    let row = snapshot(&db, mission);
    let kept = super::kept_work(&db.store, &row, &task).expect("the work");
    let super::Kept::Work {
        branch,
        commit: kept_commit,
        diff,
    } = kept
    else {
        panic!("the branch holds it: {kept:?}");
    };
    assert_eq!(branch, format!("spawn/{}", task.agent_id));
    assert_eq!(kept_commit, commit);
    assert!(
        diff.contains("big.txt") && diff.contains("insertion"),
        "{diff}"
    );
    // A checkpoint the pusher has not made it there yet: wait, until
    // the push has had its time, then say it was lost.
    let unpushed = "0123456789abcdef0123456789abcdef01234567";
    checkpoint(&db, &task, now, unpushed, "tool_completed");
    assert_eq!(
        super::kept_work(&db.store, &row, &task).expect("kept"),
        super::Kept::Waiting
    );
    checkpoint(&db, &task, now - 120_000, unpushed, "tool_completed");
    assert!(
        matches!(
            super::kept_work(&db.store, &row, &task).expect("kept"),
            super::Kept::Lost { .. }
        ),
        "a push that never came is lost"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// The task as the integrator reads it.
fn ready_task(db: &Db, task: i64) -> super::Task {
    let missions: Vec<super::Mission> =
        serde_json::from_value(db.store.gated_missions().expect("gated")).expect("missions");
    missions
        .into_iter()
        .flat_map(|mission| mission.tasks)
        .find(|row| row.id == task)
        .expect("the task")
}

fn verdicts(db: &Db, task: i64) -> Vec<String> {
    db.admin()
        .query(
            "select kind -> 'detail' -> 'record' ->> 'verdict' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                    and kind -> 'detail' -> 'record' ->> 'id' = $1
                  order by session_seq",
            &[&task.to_string()],
        )
        .expect("verdicts")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

/// A judging that panics before any record of its own records an
/// abort, so the three-aborts halt bounds a bug; one that returns an
/// error records nothing (objective 008: an environment error is shown,
/// not judged); one that recorded something gets no abort on top of it.
#[test]
fn a_judging_that_panics_before_its_verdict_is_recorded_aborted() {
    let (base, source, scenarios, integrator) = world("judging-fails", "exit 1\n");
    let db = Db::new("judging_fails");
    let main = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");

    super::judged_or_aborted(&db.store, &ready_task(&db, task), &main, mission, || {
        Err("fetch the pushed branch: no such repository".to_string())
    });
    assert!(verdicts(&db, task).is_empty(), "an error is no verdict");
    super::judged_or_aborted(&db.store, &ready_task(&db, task), &main, mission, || {
        panic!("a bug in the judging")
    });
    super::judged_or_aborted(&db.store, &ready_task(&db, task), &main, mission, || {
        panic!("another bug, {}", "formatted")
    });
    assert_eq!(verdicts(&db, task), vec!["aborted", "aborted"]);
    let reasons: Vec<String> = db
        .admin()
        .query(
            "select kind -> 'detail' -> 'record' ->> 'reason' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'verdict' = 'aborted'
                  order by session_seq",
            &[],
        )
        .expect("reasons")
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert!(reasons[0].contains("a bug in the judging"), "{reasons:?}");
    assert!(reasons[1].contains("another bug, formatted"), "{reasons:?}");

    // One that recorded its verdict, then failed: no abort on top.
    let judged = ready_task(&db, task);
    super::judged_or_aborted(&db.store, &judged, &main, mission, || {
        db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                          "verdict": "stale", "reason": "behind"}));
        db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                          "verdict": "rejected", "base": main, "note": "no"}));
        Err("then the landing failed".to_string())
    });
    assert_eq!(
        verdicts(&db, task),
        vec!["aborted", "aborted", "stale", "rejected"]
    );

    // The rejection reset the count: three aborts in a row from here
    // halt the mission.
    let other = db.reported_task(mission, "x");
    for _ in 0..3 {
        super::judged_or_aborted(&db.store, &ready_task(&db, other), &main, mission, || {
            panic!("again")
        });
    }
    assert_eq!(verdicts(&db, other), vec!["aborted"; 3]);
    integrator.pass_on(&db.store).expect("a pass");
    let halted = db.mission(mission)["halted"].clone();
    assert!(
        halted
            .as_str()
            .is_some_and(|reason| reason.contains("gate aborts in a row")),
        "halted: {halted}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Audit #17: a landing's push moved main, and the integrator stopped
/// before it recorded `task.landed`. A lower-id mission on the same
/// source runs first in the next pass: it must not read the moved main
/// as an unexplained move (a halt) while the landing is unresolved. The
/// landing then resolves (main holds its candidate), and its record
/// moves the waiting mission's baseline with it.
#[test]
fn an_unrecorded_landing_holds_the_other_missions_on_its_source() {
    let (base, source, scenarios, integrator) = world("unrecorded-landing", "exit 1\n");
    let db = Db::new("unrecorded_landing");
    let b0 = source_main(&source);
    let waiting = db.gated_mission(&source, &scenarios);
    let landing = db.gated_mission(&source, &scenarios);
    let waiting_task = db.reported_task(waiting, "w");
    let landing_task = db.reported_task(landing, "l");
    db.record(
        &json!({"type": "task.gated", "id": waiting_task, "revision": 0,
                      "verdict": "aborted", "base": b0, "reason": "a gate that died"}),
    );
    let c1 = main_moves(&base, &source, "landed.txt");
    db.record(
        &json!({"type": "task.gated", "id": landing_task, "revision": 0,
                      "verdict": "accepted", "base": b0, "candidate": c1}),
    );
    // The mirror exists, as it did when the push went out.
    crate::sources::ensure_mirror(&integrator.judge.root, Some(&source)).expect("mirror");

    integrator.pass_on(&db.store).expect("a pass");
    let waited = db.mission(waiting);
    assert!(
        waited["halted"].is_null(),
        "the waiting mission halted: {}",
        waited["halted"]
    );
    let landed = db.mission(landing);
    assert_eq!(landed["tasks"][0]["state"], "landed");
    assert_eq!(landed["baseline"], c1.as_str());
    assert_eq!(
        db.mission(waiting)["baseline"],
        c1.as_str(),
        "the landing names the mission that waited at its from"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Audit #17, the other half: main moved past the landing's candidate
/// (another landing went on top of it before the record), so the
/// compare-and-swap push fails and main is not the candidate. The
/// candidate is in main's history: it landed, and is recorded so,
/// instead of judging landed work again.
#[test]
fn a_landing_already_under_main_is_recorded_landed() {
    let (base, source, scenarios, integrator) = world("landing-under-main", "exit 1\n");
    let db = Db::new("landing_under_main");
    let b0 = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "l");
    let c1 = main_moves(&base, &source, "landed.txt");
    let c2 = main_moves(&base, &source, "on-top.txt");
    db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                      "verdict": "accepted", "base": b0, "candidate": c1}));
    crate::sources::ensure_mirror(&integrator.judge.root, Some(&source)).expect("mirror");

    integrator.pass_on(&db.store).expect("a pass");
    let after = db.mission(mission);
    assert_eq!(after["tasks"][0]["state"], "landed", "{after}");
    assert_eq!(after["baseline"], c1.as_str());
    assert_eq!(source_main(&source), c2, "main is left where it is");
    let _ = std::fs::remove_dir_all(&base);
}

/// Audit #13: the owner moved main F -> T and recorded it (the baseline
/// is T), but the integrator's fetch fails, so its mirror still says F.
/// That is no reading of the source's main: the mission is skipped this
/// pass, not halted.
#[test]
fn a_failed_fetch_halts_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let (base, source, scenarios, integrator) = world("failed-fetch", "exit 1\n");
    let db = Db::new("failed_fetch");
    crate::sources::ensure_mirror(&integrator.judge.root, Some(&source)).expect("mirror at F");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let t = main_moves(&base, &source, "deploy.txt");
    db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                      "verdict": "aborted", "base": t, "reason": "a gate that died"}));
    let permissions = |mode| {
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(mode))
            .expect("permissions");
    };
    permissions(0o000);
    let pass = integrator.pass_on(&db.store);
    permissions(0o755);
    pass.expect("a pass");
    let after = db.mission(mission);
    assert!(after["halted"].is_null(), "halted: {}", after["halted"]);
    assert_eq!(after["baseline"], t.as_str());
    let _ = std::fs::remove_dir_all(&base);
}

/// Audit #14: only a directory on this host receives agents' branches.
/// A gated mission on a remote source (configured before the gate was
/// refused on one) would be gated `stale` on every pass, forever: it is
/// halted with the reason instead.
#[test]
fn a_gated_mission_on_a_remote_source_is_halted() {
    let db = Db::new("remote_source");
    let source = "https://example.invalid/widget.git";
    let mission = db
        .store
        .mission_add("remote", &json!([source]), "human")
        .expect("add");
    db.admin()
        .execute(
            "select spawn_control_record($1)",
            &[&json!({"type": "mission.configured", "id": mission,
                      "gate": "/controller/rsi/scenarios", "accept": "gate"})],
        )
        .expect("a gate configured before the check");
    db.reported_task(mission, "w");
    let base = std::env::temp_dir().join(format!("spawn-db-remote-{}", std::process::id()));
    let integrator = Integrator::new(
        base.join("host"),
        base.join("gate.sh"),
        Vec::new(),
        room_to_spare(),
        super::DEFAULT_GATES,
    );
    integrator.pass_on(&db.store).expect("a pass");
    let halted = db.mission(mission)["halted"].clone();
    assert!(
        halted
            .as_str()
            .is_some_and(|reason| reason.contains("never lands")),
        "halted: {halted}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

fn gated(db: &Db, mission: i64) -> super::Mission {
    let missions: Vec<super::Mission> =
        serde_json::from_value(db.store.gated_missions().expect("gated")).expect("missions");
    missions
        .into_iter()
        .find(|found| found.id == mission)
        .expect("the mission")
}

/// The fleet view's gate room (`fleet_health`) shows `mission`'s run of
/// `no_room` as the mission row folds it.
fn assert_gate_room_shown(db: &Db, mission: i64) {
    let health = db.store.fleet_health().expect("health");
    assert!(
        health.get("attention").is_none(),
        "who needs a human is /api/attention's, not a count of judge flags"
    );
    let row = health["gate_room"]
        .as_array()
        .and_then(|missions| missions.iter().find(|row| row["mission"] == mission))
        .expect("the mission's gate room");
    let fold = gated(db, mission);
    assert_eq!(row["task"].as_i64(), fold.gate_room_task);
    assert_eq!(row["waits"].as_u64(), Some(u64::from(fold.gate_room_waits)));
    assert_eq!(row["escalated"].as_bool(), Some(fold.gate_room_escalated));
    assert_eq!(row["since_ms"].as_i64(), fold.gate_room_since_ms);
    assert_eq!(row["last_ms"].as_i64(), fold.gate_room_last_ms);
}

/// The dispatcher's plan over the gate-room records the store gives now,
/// for a supervisor whose digests told it `tag`.
fn gate_room_plan(db: &Db, tag: &str) -> crate::dispatch::Plan {
    let records = db.store.gate_room_escalations().expect("escalations");
    let items =
        crate::disk::gate_room_escalations(records.as_array().map_or(&[][..], Vec::as_slice));
    let supervisor: crate::dispatch::Facts = serde_json::from_value(json!({
        "session_id": "session_supervisor", "agent_id": "agent_supervisor", "told": [tag],
    }))
    .expect("facts");
    let work = crate::deliver::Work::default();
    crate::dispatch::decide_with(&[supervisor], &work, &items, false, 0, "spawn")
}

/// Task 40: the fold. `no_room` keeps the task where it was, names the
/// free space, moves neither the abort nor the rejection count, and
/// folds the mission's run of them; a gate that judged ends the run, and
/// a real abort still counts. The fleet view's health carries the run,
/// and a told escalation stays told and unresolved while the run goes on.
#[test]
fn no_room_counts_toward_no_halt_and_a_real_abort_still_does() {
    let (base, source, scenarios, _) = world("no-room-fold", "exit 1\n");
    let db = Db::new("no_room_fold");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let no_room = |escalate: bool| {
        json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "no_room",
               "reason": "no room to judge now", "free_kb": 1024, "min_gb": 10,
               "tmpdir": "/data/tmp", "log": "! gate: short", "escalate": escalate})
    };
    db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                      "verdict": "aborted", "reason": "a gate that died"}));
    for _ in 0..4 {
        db.record(&no_room(false));
    }
    db.record(&no_room(true));
    let after = gated(&db, mission);
    assert_eq!(after.gate_aborts, 1, "no_room moves no abort count");
    assert_eq!(after.gate_rejections, 0);
    assert_eq!(after.gate_room_waits, 5);
    assert!(after.gate_room_escalated);
    assert!(after.gate_room_since_ms.is_some() && after.gate_room_last_ms.is_some());
    assert_eq!(
        after.tasks[0].state,
        spawn_core::mission::TaskState::Reported
    );
    assert!(after.halted.is_none());
    assert_eq!(after.gate_room_task, Some(task));
    assert_gate_room_shown(&db, mission);
    let escalations = db.store.gate_room_escalations().expect("escalations");
    assert_eq!(
        escalations.as_array().map(Vec::len),
        Some(1),
        "{escalations}"
    );
    assert_eq!(escalations[0]["mission"], mission);
    assert_eq!(escalations[0]["free_kb"], 1024);
    // Told is a record: a digest that reached the supervisor with the
    // item's tag, however long ago. Until then it stays, whatever its age.
    let tag = format!("host gate-room-{}", escalations[0]["event_id"]);
    let digest = |key: &str, succeeded: bool| {
        db.admin()
            .execute(
                "insert into spawn_commands (principal, idempotency_key, agent_id, session_id,
                                                 action, state, succeeded, created_at_ms)
                     values ('spawn-dispatch', $1, 'agent_supervisor', 'session_supervisor',
                             jsonb_build_object('type', 'input', 'text', 'digest',
                                                'tags', jsonb_build_array($2::text)),
                             'finished', $3, 0)",
                &[&key, &tag, &succeeded],
            )
            .expect("a digest");
    };
    digest("dispatch-digest-failed", false);
    assert_eq!(
        db.store
            .gate_room_escalations()
            .expect("escalations")
            .as_array()
            .map(Vec::len),
        Some(1),
        "a digest that failed told nothing"
    );
    digest("dispatch-digest-told", true);
    // Told once, by its record: the dispatcher's told set has its tag,
    // so it is never pending again. Its run goes on, so the plan holds
    // it as told and unresolved.
    let plan = gate_room_plan(&db, &tag);
    assert!(plan.pending.is_empty(), "told once: {:?}", plan.pending);
    let told: Vec<String> = plan
        .told
        .iter()
        .map(crate::dispatch::Escalation::tag)
        .collect();
    assert_eq!(told, [tag.as_str()], "told, and the gate still has no room");

    // A no_room that names no free space or TMPDIR is refused.
    assert!(
        db.store
            .integrator_record(&json!({"type": "task.gated", "id": task, "revision": 0,
                                       "verdict": "no_room", "reason": "?"}))
            .is_err()
    );

    // Two more real aborts make three in a row: the halt still fires.
    for _ in 0..2 {
        db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                          "verdict": "aborted", "reason": "bind dir missing"}));
    }
    let after = gated(&db, mission);
    assert_eq!(after.gate_aborts, 3);
    assert_eq!(after.gate_room_waits, 0, "a gate that ran ends the run");
    assert!(after.gate_room_since_ms.is_none() && !after.gate_room_escalated);
    let plan = gate_room_plan(&db, &tag);
    assert!(
        plan.told.is_empty() && plan.pending.is_empty(),
        "told, and its run is over: resolved"
    );
    assert_gate_room_shown(&db, mission);
    assert!(super::halt_reason(&after, 0).is_some_and(|reason| reason.contains("aborts")));
    let _ = std::fs::remove_dir_all(&base);
}

/// Task 40 review: a run of `no_room` is one task's. The task leaving
/// the gate (cancelled, aborted over budget) ends it, and a later task's
/// first `no_room` starts a new run: no inherited age, backoff or
/// escalation.
#[test]
fn a_task_leaving_the_gate_ends_its_run_of_no_room() {
    let (base, source, scenarios, _) = world("no-room-leaves", "exit 1\n");
    let db = Db::new("no_room_leaves");
    let mission = db.gated_mission(&source, &scenarios);
    let no_room = |task: i64, escalate: bool| {
        json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "no_room",
               "reason": "no room", "free_kb": 0, "min_gb": 10, "tmpdir": "/t",
               "escalate": escalate})
    };
    let first = db.reported_task(mission, "a");
    for escalate in [false, false, true] {
        db.record(&no_room(first, escalate));
    }
    let before = gated(&db, mission);
    assert_eq!(
        (
            before.gate_room_task,
            before.gate_room_waits,
            before.gate_room_escalated
        ),
        (Some(first), 3, true)
    );
    db.store.task_cancel(first).expect("cancel");
    let after = gated(&db, mission);
    assert_eq!(
        (
            after.gate_room_task,
            after.gate_room_waits,
            after.gate_room_escalated
        ),
        (None, 0, false),
        "cancelled: the run ends"
    );
    assert!(after.gate_room_since_ms.is_none() && after.gate_room_last_ms.is_none());

    // A no_room on another task, while the run's task still waits (or
    // left without a record), starts a new run too.
    let second = db.reported_task(mission, "b");
    db.record(&no_room(second, true));
    let third = db.reported_task(mission, "c");
    db.record(&no_room(third, false));
    let after = gated(&db, mission);
    assert_eq!(
        (
            after.gate_room_task,
            after.gate_room_waits,
            after.gate_room_escalated
        ),
        (Some(third), 1, false),
        "another task's no_room starts a new run"
    );
    assert_eq!(after.gate_room_since_ms, after.gate_room_last_ms);
    assert_eq!(super::room_wait(&after, second, i64::MAX), None);
    let _ = std::fs::remove_dir_all(&base);
}

/// Task 40, end to end: a gate that exits 5 with its no-room fact is
/// recorded `no_room` with the free space, the task stays reported, and
/// the next pass waits out the backoff instead of running the gate.
#[test]
fn a_gate_without_room_is_recorded_no_room_and_retried_later() {
    let gate = "echo 'gate: 0.0 GiB free' >&2\n\
                    printf '{\"no_room\":true,\"free_kb\":2048,\"min_gb\":10,\"tmpdir\":\"/data/tmp\"}\\n' \
                    >\"$SPAWN_GATE_VERDICT\"\n\
                    echo ran >>\"$(dirname \"$0\")/runs\"\n\
                    exit 5\n";
    let (base, source, scenarios, integrator) = world("no-room-pass", gate);
    let db = Db::new("no_room_pass");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent_id = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    let clone = base.join("agent");
    let clone_path = clone.to_string_lossy().to_string();
    git(&["clone", "-q", &source, &clone_path]).expect("clone");
    std::fs::write(clone.join("work.txt"), "work\n").expect("write");
    super::tests::commit_in(&clone_path, "work");
    git(&[
        "-C",
        &clone_path,
        "push",
        "-q",
        "origin",
        &format!("HEAD:refs/heads/spawn/{agent_id}"),
    ])
    .expect("push the agent's branch");

    // Judgings run on threads: settle each pass's before the next, as
    // `--once` does.
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    integrator.pass_on(&db.store).expect("a second pass");
    integrator.settle();
    let runs = std::fs::read_to_string(base.join("runs")).expect("the gate ran");
    assert_eq!(
        runs.lines().count(),
        1,
        "the second pass waits out the backoff"
    );
    let after = gated(&db, mission);
    assert_eq!(
        after.tasks[0].state,
        spawn_core::mission::TaskState::Reported
    );
    assert_eq!((after.gate_aborts, after.gate_rejections), (0, 0));
    assert_eq!(after.gate_room_waits, 1);
    let record: Value = db
        .admin()
        .query_one(
            "select kind -> 'detail' -> 'record' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                  order by session_seq desc limit 1",
            &[],
        )
        .expect("the verdict")
        .get(0);
    assert_eq!(record["id"], task);
    assert_eq!(record["verdict"], "no_room");
    assert_eq!(record["free_kb"], 2048);
    assert_eq!(record["tmpdir"], "/data/tmp");
    assert_eq!(record["escalate"], false);
    // The gate's own floor is the disk check, named as the
    // integrator's checks name theirs.
    assert_eq!(
        (&record["check"], &record["measured"], &record["threshold"]),
        (&json!("disk"), &json!(2048), &json!(10 * 1_048_576))
    );
    assert!(
        record["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("0.0 GiB free in /data/tmp")),
        "{record}"
    );
    assert!(
        record["log"]
            .as_str()
            .is_some_and(|log| log.contains("! gate: 0.0 GiB free"))
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// The stub gate that notes each run beside itself and accepts.
const NOTING_GATE: &str = "echo ran >>\"$(dirname \"$0\")/runs\"\n\
                               printf '{\"verdict\":\"accepted\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";

/// How many times the stub gate in `base` ran.
fn gate_runs(base: &Path) -> usize {
    std::fs::read_to_string(base.join("runs")).map_or(0, |runs| runs.lines().count())
}

/// The latest `task.gated` record.
fn last_gated(db: &Db) -> Value {
    db.admin()
        .query_one(
            "select kind -> 'detail' -> 'record' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                  order by session_seq desc limit 1",
            &[],
        )
        .expect("the verdict")
        .get(0)
}

/// One judging of a reported task with pushed work, on a host whose
/// room `room` measures: the mission, the task, and the world's base.
fn judged_on(tag: &str, room: crate::gate::Room) -> (Db, i64, i64, PathBuf) {
    judged_on_with(tag, room, false)
}

/// [`judged_on`], the mission's scenarios marked
/// [`super::NO_TEST_DATABASE`] when `no_test_database`.
fn judged_on_with(
    tag: &str,
    room: crate::gate::Room,
    no_test_database: bool,
) -> (Db, i64, i64, PathBuf) {
    let (base, source, scenarios, mut integrator) = world(tag, NOTING_GATE);
    if no_test_database {
        std::fs::write(scenarios.join(super::NO_TEST_DATABASE), "").expect("the marker");
    }
    integrator.judge.room = room;
    let db = Db::new(&tag.replace('-', "_"));
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    push_work(&base, &source, &agent_of(&db, mission, task), "work.txt");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    (db, mission, task, base)
}

/// A room with these integrator variables, measuring `free_kb`,
/// `available_mb` and `probe`.
fn measured_room(
    integrator: &[(&'static str, &'static str)],
    free_kb: u64,
    available_mb: u64,
    probe: Result<(), spawn_postgres::ProbeFailure>,
) -> crate::gate::Room {
    let integrator = integrator.to_vec();
    crate::gate::Room::from_env(move |name| {
        integrator
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_string())
    })
    .measured_by(
        move |_| Some(free_kb),
        move || Some(available_mb),
        move |_| probe.clone(),
    )
}

/// Objective 018: the host's room is measured before the gate starts.
/// A check that falls short (disk, memory, the test database whose
/// parallel query cannot get its shared memory) runs no gate and is
/// recorded `no_room` with its check, measure and threshold: the task
/// stays reported, neither count moves, and the mission's run of
/// no-room names the task, so it is judged again after the backoff. No
/// password reaches the record.
#[test]
fn a_check_short_before_the_gate_records_no_room_and_runs_no_gate() {
    const URL: &str = "postgres://gate:hunter2@%2Fvar%2Frun%2Ftest-pg:5433/spawn_test";
    let shared_memory = spawn_postgres::ProbeFailure {
        probe: "a parallel query within 10 s".to_string(),
        error: "ERROR: could not resize shared memory segment \"/PostgreSQL.2\" to 8388608 \
                    bytes: No space left on device (53100) for gate:hunter2"
            .to_string(),
    };
    let plenty = u64::MAX / 2;
    let cases = [
        (
            "room-short-disk",
            measured_room(&[], 1024, plenty, Ok(())),
            "disk",
            json!(1024),
            json!(10 * 1_048_576),
            "SPAWN_GATE_MIN_FREE_GB=10",
        ),
        (
            "room-short-memory",
            measured_room(&[], plenty, 512, Ok(())),
            "memory",
            json!(512),
            json!(2048),
            "512 MiB available",
        ),
        (
            "room-short-database",
            measured_room(
                &[("SPAWN_TEST_DATABASE_URL", URL)],
                plenty,
                plenty,
                Err(shared_memory),
            ),
            "database",
            json!(
                "ERROR: could not resize shared memory segment \"/PostgreSQL.2\" to 8388608 \
                     bytes: No space left on device (53100) for gate:***"
            ),
            json!("a parallel query within 10 s"),
            "the test database (/var/run/test-pg:5433/spawn_test)",
        ),
    ];
    for (tag, room, check, measured, threshold, named) in cases {
        let (db, mission, task, base) = judged_on(tag, room);
        assert_eq!(gate_runs(&base), 0, "{check}: the gate never ran");
        let record = last_gated(&db);
        assert_eq!(
            (&record["id"], &record["verdict"], &record["check"]),
            (&json!(task), &json!("no_room"), &json!(check)),
            "{record}"
        );
        assert_eq!(
            (&record["measured"], &record["threshold"]),
            (&measured, &threshold),
            "{record}"
        );
        let reason = record["reason"].as_str().expect("a reason");
        assert!(
            reason.contains(named) && reason.ends_with("not a verdict, judged again later"),
            "{reason}"
        );
        assert!(
            record["log"]
                .as_str()
                .is_some_and(|log| log.contains("the gate did not run")),
            "{record}"
        );
        assert!(!record.to_string().contains("hunter2"), "{record}");
        if check == "disk" {
            assert_eq!(
                (&record["free_kb"], &record["min_gb"], &record["tmpdir"]),
                (&json!(1024), &json!(10), &json!("/tmp"))
            );
        }
        let after = gated(&db, mission);
        assert_eq!(
            after.tasks[0].state,
            spawn_core::mission::TaskState::Reported
        );
        assert_eq!((after.gate_aborts, after.gate_rejections), (0, 0));
        assert_eq!(
            (after.gate_room_task, after.gate_room_waits),
            (Some(task), 1)
        );
        assert!(
            super::room_wait(&after, task, after.now_ms.expect("the database's clock")).is_some(),
            "judged again after the backoff"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// A mission whose gate needs no test database (`no-test-database`)
/// is not held back by a test server that fails its probe: its gate
/// never gets the URL, so the probe is not made and the gate runs.
#[test]
fn a_mission_without_a_test_database_is_not_held_back_by_its_probe() {
    let room = measured_room(
        &[(
            "SPAWN_TEST_DATABASE_URL",
            "postgres://t:pw@%2Frun%2Fpg/spawn_test",
        )],
        u64::MAX / 2,
        u64::MAX / 2,
        Err(spawn_postgres::ProbeFailure {
            probe: "connect within 10 s".to_string(),
            error: "connection refused".to_string(),
        }),
    );
    let (db, mission, task, base) = judged_on_with("room-no-db", room, true);
    assert_eq!(gate_runs(&base), 1, "the gate ran");
    assert_eq!(verdicts(&db, task), vec!["accepted"]);
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
    let _ = std::fs::remove_dir_all(&base);
}

/// Every check met: the gate runs exactly as before, the test database
/// probed by the integrator's URL; a threshold that does not parse is an
/// abort, and runs no gate either.
#[test]
fn with_room_the_gate_runs_and_a_bad_threshold_is_an_abort() {
    let probed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let noted = Arc::clone(&probed);
    let room = measured_room(
        &[(
            "SPAWN_TEST_DATABASE_URL",
            "postgres://t:pw@%2Frun%2Fpg/spawn_test",
        )],
        u64::MAX / 2,
        u64::MAX / 2,
        Ok(()),
    )
    .measured_by(
        |_| Some(u64::MAX / 2),
        || Some(u64::MAX / 2),
        move |url| {
            noted.lock().expect("probed").push(url.to_string());
            Ok(())
        },
    );
    let (db, mission, task, base) = judged_on("room-met", room);
    assert_eq!(gate_runs(&base), 1, "the gate ran once");
    assert_eq!(
        *probed.lock().expect("probed"),
        ["postgres://t:pw@%2Frun%2Fpg/spawn_test"]
    );
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
    assert_eq!(verdicts(&db, task), vec!["accepted"]);
    let _ = std::fs::remove_dir_all(&base);

    let room = measured_room(&[("SPAWN_GATE_MIN_FREE_MEMORY_MB", "lots")], 0, 0, Ok(()));
    let (db, mission, task, base) = judged_on("room-bad", room);
    assert_eq!(gate_runs(&base), 0);
    let record = last_gated(&db);
    assert_eq!(
        (&record["id"], &record["verdict"]),
        (&json!(task), &json!("aborted"))
    );
    assert!(
        record["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("SPAWN_GATE_MIN_FREE_MEMORY_MB")),
        "{record}"
    );
    assert_eq!(gated(&db, mission).gate_aborts, 1, "an abort counts");
    let _ = std::fs::remove_dir_all(&base);
}

/// The base warm after a landing (warm.rs): a task whose gate accepts
/// lands in its judging's thread, and the pass after it starts one
/// `gate.sh --base-only` of the new main, on the source's mirror, with
/// the mission's scenarios — in the background, the landing never
/// waiting for it.
#[test]
fn a_landing_starts_a_base_warm_of_the_new_main() {
    let dir = std::env::temp_dir().join(format!("spawn-db-warm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let args = dir.join("warm-args");
    // The warm's arguments appear whole (written aside, then moved): the
    // test waits for the file and reads it at once, and a `>` redirect
    // creates it empty before printf writes (check-tests flaked on that,
    // 0 lines read, judging 01a1cb516f).
    let gate = format!(
        "if [ \"$1\" = --base-only ]; then printf '%s\\n' \"$@\" >'{0}.tmp' && mv '{0}.tmp' '{0}'; exit 0; fi\n\
             printf '{{\"verdict\":\"accepted\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
             echo '{{\"verdict\":\"accepted\"}}'\n",
        args.display()
    );
    let (base, source, scenarios, integrator) = world("warm", &gate);
    let integrator = integrator.with_warm(dir.join("gate-warm"));
    let db = Db::new("landing_warm");
    let mission = db.gated_mission(&source, &scenarios);
    db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");

    integrator.pass_on(&db.store).expect("the judging's pass");
    integrator.settle();
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
    let main = source_main(&source);
    // The landing was made in the judging's thread, after that pass
    // looked for warms: the next pass starts it.
    integrator.pass_on(&db.store).expect("the next pass");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !args.exists() {
        assert!(std::time::Instant::now() < deadline, "no warm started");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let said = std::fs::read_to_string(&args).expect("the warm's arguments");
    let words: Vec<&str> = said.lines().collect();
    assert_eq!(words.len(), 4, "{said}");
    assert_eq!(words[0], "--base-only");
    assert!(words[1].contains(".sources"), "the source's mirror: {said}");
    assert_eq!(words[2], main, "the new main");
    assert_eq!(
        Path::new(words[3]),
        super::pinned(&scenarios),
        "the mission's scenarios, as its gates read them"
    );
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A warm a predecessor's restart cut off (warm.rs, `Warms::take_over`):
/// its pid file, naming a process gone, the source's main, the source and
/// the mission's scenarios, is found by the next integrator, which warms
/// that main again at its first pass, but not while it drains, and not
/// while every mission on the source is halted.
#[test]
fn a_warm_cut_off_by_a_restart_is_warmed_again_at_the_first_pass() {
    // `case`: "live", "drain" or "halted". The warm's arguments, if any.
    fn first_pass(case: &str) -> Option<Vec<String>> {
        let dir = std::env::temp_dir().join(format!(
            "spawn-warm-again-dir-{case}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("gate-warm")).expect("dir");
        let args = dir.join("warm-args");
        // Whole when it appears, as above.
        let gate = format!(
            "if [ \"$1\" = --base-only ]; then printf '%s\\n' \"$@\" >'{0}.tmp' && mv '{0}.tmp' '{0}'; exit 0; fi\n\
             exit 1\n",
            args.display()
        );
        let (base, source, scenarios, integrator) = world(&format!("warm-again-{case}"), &gate);
        let db = Db::new(&format!("warm_again_{case}"));
        let mission = db.gated_mission(&source, &scenarios);
        let main = source_main(&source);
        let mut gone = std::process::Command::new("true").spawn().expect("true");
        let pid = gone.id();
        gone.wait().expect("reaped");
        std::fs::write(
            dir.join("gate-warm").join(super::warm::PID_FILE),
            format!(
                "pid {pid}\nstart 1\ncommit {main}\nsource {source}\nscenarios {}\n",
                super::pinned(&scenarios).display()
            ),
        )
        .expect("pid file");
        let integrator = integrator.with_warm(dir.join("gate-warm"));
        match case {
            "drain" => integrator.begin_drain(Some(&db.store), 7200),
            "halted" => {
                db.record(&json!({"type": "mission.halted", "id": mission, "reason": "a test"}));
            }
            _ => {}
        }
        integrator.pass_on(&db.store).expect("the first pass");
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(if case == "live" { 10 } else { 2 });
        while !args.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let said = std::fs::read_to_string(&args)
            .ok()
            .map(|said| said.lines().map(str::to_string).collect::<Vec<_>>());
        if case != "live" {
            // Nor at a later pass: a halted source's lost warm is dropped,
            // and a drain starts no warm.
            integrator.pass_on(&db.store).expect("the next pass");
            std::thread::sleep(std::time::Duration::from_millis(500));
            assert!(!args.exists(), "{case}: no warm at a later pass either");
        }
        if let Some(words) = &said {
            assert_eq!(words.len(), 4, "{words:?}");
            assert_eq!(words[0], "--base-only");
            assert!(
                words[1].contains(".sources"),
                "the source's mirror: {words:?}"
            );
            assert_eq!(words[2], main, "the source's main as it is");
            assert_eq!(Path::new(&words[3]), super::pinned(&scenarios));
        }
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&dir);
        said
    }
    assert!(
        first_pass("live").is_some(),
        "warmed again at the first pass"
    );
    assert_eq!(first_pass("drain"), None, "a drain starts no warm");
    assert_eq!(
        first_pass("halted"),
        None,
        "a source whose missions are all halted is warmed for no gate"
    );
}

/// Gate v2 step 2 (integrate/judging.rs): a judging's gate runs in the
/// judging's directory under the workspace root, beside `judging.json`
/// (what it judges), its verdict and landing name that judging, and the
/// directory is gone once they are recorded.
#[test]
fn a_judgings_outcomes_name_it_and_its_directory_goes_once_they_are_recorded() {
    let dir = std::env::temp_dir().join(format!("spawn-judging-dir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let seen = dir.join("seen");
    let gate = format!(
        "if [ \"$1\" = --base-only ]; then exit 0; fi\n\
             private=$(dirname \"$SPAWN_GATE_VERDICT\")\n\
             printf '%s\\n' \"$private\" >'{seen}'\n\
             cp \"$(dirname \"$private\")/judging.json\" '{seen}.json'\n\
             printf '{{\"verdict\":\"accepted\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
             echo '{{\"verdict\":\"accepted\"}}'\n",
        seen = seen.display()
    );
    let (base, source, scenarios, integrator) = world("judging-dir", &gate);
    let db = Db::new("judging_dir");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");

    integrator.pass_on(&db.store).expect("the judging's pass");
    integrator.settle();
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
    let gated = control_records(&db, "task.gated");
    let landed = control_records(&db, "task.landed");
    assert_eq!((gated.len(), landed.len()), (1, 1), "{gated:?} {landed:?}");
    let id = gated[0]["judging"]
        .as_str()
        .expect("the verdict names its judging");
    assert_eq!(id.len(), 28, "{id}");
    assert_eq!(
        landed[0]["judging"],
        json!(id),
        "the landing names the same judging"
    );
    // The gate ran in a private directory of that judging's directory.
    let judgings = base.join("host").join(super::judging::DIRECTORY);
    let private = std::fs::read_to_string(&seen).expect("the gate's directory");
    let private = Path::new(private.trim());
    assert_eq!(private.parent(), Some(judgings.join(id).as_path()));
    let described: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("seen.json")).expect("judging.json"),
    )
    .expect("json");
    assert_eq!(described["version"], json!(super::judging::VERSION));
    assert_eq!(described["id"], json!(id));
    assert_eq!(described["kind"], json!("task"));
    assert_eq!(described["task"], json!(task));
    assert_eq!(described["mission"], json!(mission));
    assert_eq!(described["source"], json!(source));
    // What deploy/controller.sh keeps while the judging lasts: the gate
    // and the scenarios it runs, each as pinned (physical).
    assert_eq!(
        described["gate"],
        json!(super::pinned(&integrator.judge.gate))
    );
    assert_eq!(described["scenarios"], json!(super::pinned(&scenarios)));
    // Gone once recorded; the parent stays, empty.
    assert!(
        !judgings.join(id).exists(),
        "the judging's directory is removed"
    );
    assert_eq!(
        std::fs::read_dir(&judgings).expect("judgings").count(),
        0,
        "nothing left under {}",
        judgings.display()
    );
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Gate v2 step 2 (6a): with gates in units, a judged task's gate runs
/// in a transient unit of the user's manager (its cgroup says so), and
/// the task lands as from a child's gate (skipped where no manager
/// answers).
#[test]
fn a_gate_in_a_unit_judges_and_lands_as_a_childs_does() {
    if !crate::gate::units_available() {
        eprintln!("no systemd user manager answers here: not tried");
        return;
    }
    let gate = "if [ \"$1\" = --base-only ]; then exit 0; fi\n\
                    grep -q 'spawn-gate-' /proc/self/cgroup || { echo 'not in a unit' >&2; exit 3; }\n\
                    printf '{\"verdict\":\"accepted\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
                    echo '{\"verdict\":\"accepted\"}'\n";
    let (base, source, scenarios, integrator) = world("unit-gate", gate);
    let integrator = integrator.with_units(true);
    let db = Db::new("unit_gate");
    let mission = db.gated_mission(&source, &scenarios);
    db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    integrator.pass_on(&db.store).expect("the judging's pass");
    integrator.settle();
    let after = db.mission(mission);
    assert_eq!(after["tasks"][0]["state"], "landed", "{after}");
    let _ = std::fs::remove_dir_all(&base);
}

/// Gate v2 step 2 (6b, D5 and D4): a second integrator on the same root
/// adopts the judging the first one runs, as a restarted integrator
/// adopts what its predecessor left: it starts no gate of its own and
/// takes over the running one. Released, the gate's outcome is
/// recorded once (the store returns the first to the other reaper) and
/// lands once, and nothing of the judging is left. Skipped where no
/// systemd user manager answers.
#[test]
fn a_gate_left_running_is_adopted_judged_once_and_lands_once() {
    if !crate::gate::units_available() {
        eprintln!("no systemd user manager answers here: not tried");
        return;
    }
    let marks = std::env::temp_dir().join(format!("spawn-adopt-marks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&marks);
    std::fs::create_dir_all(&marks).expect("marks");
    let _release = Release(marks.clone());
    let (base, source, scenarios, first) = world("adopt", &holding_gate(&marks));
    let units = StopUnits(base.join("host"));
    let first = first.with_units(true);
    let second = Integrator::new(
        base.join("host"),
        first.judge.gate.clone(),
        test_gate_env(),
        room_to_spare(),
        super::DEFAULT_GATES,
    )
    .with_units(true);
    let db = Db::new("adopt_gate");
    let mission = db.gated_mission(&source, &scenarios);
    db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");

    first
        .pass_on(&db.store)
        .expect("the first integrator's pass");
    wait_started(&marks, 1);
    // The other integrator's passes take the judging over and start no
    // gate of their own.
    for _ in 0..3 {
        second
            .pass_on(&db.store)
            .expect("the second integrator's pass");
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(started(&marks).len(), 1, "no second gate");
    assert_eq!(
        second.running.borrow().len(),
        1,
        "the second runs the adopted judging"
    );
    assert_eq!(
        second.running.borrow()[0].judging,
        first.running.borrow()[0].judging,
        "the same judging"
    );
    std::fs::write(marks.join("release"), "").expect("release");
    first.settle();
    second.settle();
    let after = db.mission(mission);
    assert_eq!(after["tasks"][0]["state"], "landed", "{after}");
    let records = control_records(&db, "task.gated");
    let landed = control_records(&db, "task.landed");
    assert_eq!(
        (records.len(), landed.len()),
        (1, 1),
        "{records:?} {landed:?}"
    );
    assert_eq!(records[0]["verdict"], "accepted");
    assert_eq!(
        records
            .iter()
            .filter(|record| record["verdict"] == "aborted")
            .count(),
        0,
        "the starting integrator's outcome is not duplicated as an abort"
    );
    assert_eq!(gated(&db, mission).gate_aborts, 0);
    assert_eq!(landed[0]["judging"], records[0]["judging"]);
    let judgings = base.join("host").join(super::judging::DIRECTORY);
    assert_eq!(
        std::fs::read_dir(&judgings).expect("judgings").count(),
        0,
        "nothing left"
    );
    assert_eq!(started(&marks).len(), 1, "the gate ran once");
    // The units go before the scratch their judgings are in.
    drop(units);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// A reaped adopted gate normally has the starting integrator's outcome, and
/// the store returns it without appending another. With no original outcome,
/// the adopter records `aborted` under the same judging id instead. Three such
/// no-outcome reaps hit the existing abort halt; the next pass starts no gate.
#[test]
fn a_reaped_gate_without_an_original_outcome_records_an_abort() {
    let marks = fresh_marks("spawn-reaped-no-outcome");
    let gate = format!(
        "echo started >>'{}/started'\nexit 1\n",
        marks.to_string_lossy()
    );
    let (base, source, scenarios, integrator) = world("reaped-no-outcome", &gate);
    let db = Db::new("reaped_no_outcome");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let pushed = push_work(&base, &source, &agent_of(&db, mission, task), "work.txt");
    let main = source_main(&source);

    for attempt in 1..=3 {
        let left = left_judging(
            &base.join("host"),
            &db,
            mission,
            task,
            &source,
            &pushed,
            true,
        );
        let private = left.join(format!("spawn-gate-{}-left-1", std::process::id()));
        // Keep a real runner alive while the adopter starts waiting, then
        // remove its directory and end it before wait_unit can finish. No
        // starting integrator exists to record an outcome.
        let mut runner = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("runner");
        std::fs::write(
            private.join("runner.json"),
            json!({"unit": format!("spawn-gate-{}-left-1.service", std::process::id()),
                   "runner": spawn_host::process::ProcessId::of(runner.id()).record(),
                   "started_ms": super::now_ms(), "limit_s": 7200})
            .to_string(),
        )
        .expect("live runner");
        integrator.pass_on(&db.store).expect("adopt the gate");
        assert_eq!(
            integrator.running.borrow().len(),
            1,
            "the left gate is adopted"
        );
        // The live runner keeps wait_unit waiting once it has read runner.json.
        // Give that read a moment before taking the directory away.
        std::thread::sleep(std::time::Duration::from_millis(500));
        std::fs::remove_dir_all(&left).expect("reap the judging directory");
        runner.kill().expect("end the runner");
        runner.wait().expect("reap the runner process");
        integrator.settle();

        let records = control_records(&db, "task.gated");
        assert_eq!(records.len(), attempt, "one outcome per reap: {records:?}");
        let record = &records[attempt - 1];
        assert_eq!(record["id"], task);
        assert_eq!(record["verdict"], "aborted");
        assert!(
            record["reason"].as_str().is_some_and(
                |reason| reason.contains("the integrator that started the gate reaped it")
            ),
            "the abort names the reaping integrator: {record}"
        );
        assert_eq!(
            record["judging"],
            json!(
                left.file_name()
                    .and_then(|name| name.to_str())
                    .expect("judging id")
            ),
            "deduplication keeps the adopted judging's id"
        );
        assert_eq!(
            gated(&db, mission).gate_aborts,
            u32::try_from(attempt).expect("three attempts")
        );
        assert_eq!(db.mission(mission)["tasks"][0]["state"], "reported");
        assert_eq!(source_main(&source), main, "nothing landed");
    }

    // The next pass turns the third abort into the halt before ready work can
    // start another gate. The stub would leave a mark if a fourth gate ran.
    integrator.pass_on(&db.store).expect("the bounded pass");
    integrator.settle();
    let after = gated(&db, mission);
    assert_eq!(after.gate_aborts, 3);
    assert!(
        after
            .halted
            .as_deref()
            .is_some_and(|reason| reason.contains("3 gate aborts")),
        "the no-outcome reaps are bounded: {:?}",
        after.halted
    );
    assert!(started(&marks).is_empty(), "no fourth gate started");
    assert_eq!(control_records(&db, "task.gated").len(), 3);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// A judging an earlier integrator left, made by hand as it leaves one:
/// its description, and (`gate`) a private directory whose runner is a
/// process gone now and that wrote no status.
fn left_judging(
    root: &Path,
    db: &Db,
    mission: i64,
    task: i64,
    source: &str,
    candidate: &str,
    gate: bool,
) -> PathBuf {
    std::fs::create_dir_all(root).expect("the root");
    let judging = super::judging::Judging::create(
        root,
        &json!({"kind": "task", "mission": mission, "task": task, "revision": 0,
                "source": source, "base": source_main(source), "count": 0,
                "shares_server": false, "jobs": null, "gate": "/nonexistent/gate.sh",
                "scenarios": db.mission(mission)["gate"], "probe": false,
                "candidate": candidate, "branch": candidate}),
        true,
    )
    .expect("a judging");
    let directory = judging.directory().to_path_buf();
    if gate {
        let private = directory.join(format!("spawn-gate-{}-left-1", std::process::id()));
        std::fs::create_dir(&private).expect("private");
        let mut child = std::process::Command::new("true").spawn().expect("true");
        let gone = spawn_host::process::ProcessId::of(child.id());
        child.wait().expect("gone");
        std::fs::write(
            private.join("runner.json"),
            json!({"unit": format!("spawn-gate-{}-left-1.service", std::process::id()),
                   "runner": gone.record(), "started_ms": super::now_ms(),
                   "limit_s": 7200})
            .to_string(),
        )
        .expect("runner.json");
        std::fs::write(private.join("stdout"), "| the gate ran a while\n").expect("stdout");
    }
    directory
}

/// [`left_judging`] whose gate ended while no integrator ran: its
/// verdict accepted and its status written.
fn left_accepted(
    root: &Path,
    db: &Db,
    mission: i64,
    task: i64,
    source: &str,
    candidate: &str,
) -> PathBuf {
    let directory = left_judging(root, db, mission, task, source, candidate, true);
    let private = directory.join(format!("spawn-gate-{}-left-1", std::process::id()));
    std::fs::write(
        private.join("verdict"),
        "{\"v\":1,\"verdict\":\"accepted\"}\n",
    )
    .expect("verdict");
    std::fs::write(private.join("status"), "0\n").expect("status");
    directory
}

/// Gate v2 step 2 (6b, D5): main moved while no integrator ran (an
/// owner's push). The left gate accepted the candidate on the old main:
/// adopted, its acceptance is recorded and its landing finds main moved,
/// `stale`, counted nowhere, the task judged again, and nothing lands;
/// on a main that did not move, the same left gate lands.
#[test]
fn an_adopted_verdict_on_a_main_that_moved_is_stale_and_lands_nothing() {
    let (base, source, scenarios, integrator) = world("adopt-moved", "exit 1\n");
    let db = Db::new("adopt_moved");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    let pushed = push_work(&base, &source, &agent, "work.txt");
    let left = left_accepted(&base.join("host"), &db, mission, task, &source, &pushed);
    // The owner's push while no integrator ran.
    let owner = push_work(&base, &source, "owner", "owner.txt");
    git(&[
        "--git-dir",
        &source,
        "update-ref",
        "refs/heads/main",
        &owner,
    ])
    .expect("main moves");
    integrator
        .pass_on(&db.store)
        .expect("the pass that adopts it");
    integrator.settle();
    // Accepted as the gate found it, then stale when its landing finds
    // main elsewhere (an acceptance keeps the landing's compare-and-swap
    // as its guard), both naming the judging.
    let gated = control_records(&db, "task.gated");
    let verdicts: Vec<&str> = gated.iter().filter_map(|r| r["verdict"].as_str()).collect();
    assert_eq!(verdicts, ["accepted", "stale"], "{gated:?}");
    assert_eq!(gated[1]["judging"], gated[0]["judging"]);
    assert_eq!(
        db.mission(mission)["tasks"][0]["state"],
        "reported",
        "judged again"
    );
    assert_eq!(
        source_main(&source),
        owner,
        "nothing landed over the owner's push"
    );
    assert!(control_records(&db, "task.landed").is_empty());
    assert!(!left.exists());
    let _ = std::fs::remove_dir_all(&base);

    // The same, main untouched: it lands, once, naming the judging.
    let (base, source, scenarios, integrator) = world("adopt-lands", "exit 1\n");
    let db = Db::new("adopt_lands");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    let pushed = push_work(&base, &source, &agent, "work.txt");
    let left = left_accepted(&base.join("host"), &db, mission, task, &source, &pushed);
    integrator
        .pass_on(&db.store)
        .expect("the pass that adopts it");
    integrator.settle();
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
    let landed = control_records(&db, "task.landed");
    assert_eq!(landed.len(), 1);
    assert_eq!(
        landed[0]["judging"],
        json!(left.file_name().and_then(|name| name.to_str()).expect("id"))
    );
    assert_eq!(source_main(&source), pushed, "the adopted candidate landed");
    let _ = std::fs::remove_dir_all(&base);
}

/// Gate v2 step 2 (6b, D6): a left judging whose gate's runner is gone
/// without the gate's status (the host rebooted, the unit was killed) is
/// adopted and recorded as a host fault, evidence `abandoned`, naming
/// it; nothing lands, no gate starts, the task waits for its backoff,
/// and the judging's directory goes.
#[test]
fn an_adopted_gate_whose_runner_is_gone_is_abandoned_not_judged() {
    let marks = std::env::temp_dir().join(format!("spawn-abandon-marks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&marks);
    std::fs::create_dir_all(&marks).expect("marks");
    let _release = Release(marks.clone());
    let (base, source, scenarios, integrator) = world("abandon", &holding_gate(&marks));
    let db = Db::new("abandon_gate");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    let pushed = push_work(&base, &source, &agent, "work.txt");
    let main = source_main(&source);
    let left = left_judging(
        &base.join("host"),
        &db,
        mission,
        task,
        &source,
        &pushed,
        true,
    );
    integrator
        .pass_on(&db.store)
        .expect("the pass that adopts it");
    integrator.settle();
    let gated = control_records(&db, "task.gated");
    assert_eq!(gated.len(), 1, "{gated:?}");
    assert_eq!(gated[0]["verdict"], "host_fault", "{}", gated[0]);
    assert_eq!(gated[0]["evidence"], crate::gate::ABANDONED);
    assert_eq!(
        gated[0]["judging"],
        json!(left.file_name().and_then(|name| name.to_str()).expect("id"))
    );
    assert!(
        gated[0]["log"]
            .as_str()
            .is_some_and(|log| log.contains("the gate ran a while")),
        "the gate's output is its log: {}",
        gated[0]
    );
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "reported");
    assert_eq!(source_main(&source), main, "nothing landed");
    assert!(started(&marks).is_empty(), "no gate started");
    assert!(!left.exists(), "the judging's directory is gone");
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// Gate v2 step 2 (6b): a left judging with no gate in it (its
/// integrator stopped before the gate started, or after its records) is
/// removed, and its task is judged as any other.
#[test]
fn a_left_judging_without_a_gate_is_removed_and_its_task_judged_anew() {
    let gate = "printf '{\"verdict\":\"accepted\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
                    echo '{\"verdict\":\"accepted\"}'\n";
    let (base, source, scenarios, integrator) = world("left-empty", gate);
    let db = Db::new("left_empty");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    let pushed = push_work(&base, &source, &agent, "work.txt");
    let left = left_judging(
        &base.join("host"),
        &db,
        mission,
        task,
        &source,
        &pushed,
        false,
    );
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    assert!(!left.exists(), "removed");
    let gated = control_records(&db, "task.gated");
    assert_eq!(gated.len(), 1, "{gated:?}");
    assert_ne!(
        gated[0]["judging"],
        json!(left.file_name().and_then(|name| name.to_str()).expect("id")),
        "judged by a judging of its own"
    );
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
    let _ = std::fs::remove_dir_all(&base);
}

/// Audit #10: the integrator's verdict names the revision it judged. A
/// task sent back once by the gate is reported again (revision 1); the
/// stub gate accepts, and the verdict for revision 1 lands it.
#[test]
fn a_verdict_names_the_revision_it_judged_and_lands() {
    let gate = "printf '{\"verdict\":\"accepted\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
                    echo '{\"verdict\":\"accepted\"}'\n";
    let (base, source, scenarios, integrator) = world("verdict-revision", gate);
    let db = Db::new("verdict_revision");
    let b0 = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent_id = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                      "verdict": "rejected", "base": b0, "note": "again"}));
    db.store.task_delivered(task, 2).expect("delivered again");
    assert!(
        db.store
            .task_reported(task, "again")
            .expect("reported again")
    );
    // The agent's work, pushed as spawn/<agent> (no checkpoint recorded,
    // so the push is what lands).
    let clone = base.join("agent");
    let clone_path = clone.to_string_lossy().to_string();
    git(&["clone", "-q", &source, &clone_path]).expect("clone");
    std::fs::write(clone.join("work.txt"), "work\n").expect("write");
    super::tests::commit_in(&clone_path, "work");
    git(&[
        "-C",
        &clone_path,
        "push",
        "-q",
        "origin",
        &format!("HEAD:refs/heads/spawn/{agent_id}"),
    ])
    .expect("push the agent's branch");
    let work = git(&["-C", &clone_path, "rev-parse", "HEAD"]).expect("work");

    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    let after = db.mission(mission);
    assert_eq!(after["tasks"][0]["state"], "landed", "{after}");
    // One landing commit on top of the base, the work its second parent,
    // its message from the record (the brief and the report).
    let main = source_main(&source);
    let rev = |reference: String| {
        git(&["--git-dir", &source, "rev-parse", &reference]).expect("rev-parse")
    };
    assert_eq!(rev(format!("{main}^1")), b0);
    assert_eq!(rev(format!("{main}^2")), work);
    let message =
        git(&["--git-dir", &source, "show", "-s", "--format=%B", &main]).expect("message");
    assert!(
        message.starts_with(&format!(
            "brief (task {task})\n\nagain\n\nSpawn-Task: {task}\n"
        )),
        "{message}"
    );
    assert_eq!(after["baseline"], main.as_str());
    let revisions: Vec<Option<String>> = db
        .admin()
        .query(
            "select kind -> 'detail' -> 'record' ->> 'revision' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                  order by session_seq",
            &[],
        )
        .expect("verdicts")
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(
        revisions,
        vec![Some("0".to_string()), Some("1".to_string())]
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Issue #8: a reported task whose agent's latest recorded checkpoint
/// the source never got (its push was lost with the process that queued
/// it) is not judged `stale` on every pass forever: the stale check
/// pushes that checkpoint again from this host's mirror, and the same
/// check finds it fresh.
#[test]
fn a_recorded_checkpoint_whose_push_was_lost_is_pushed_again_and_fresh() {
    use spawn_core::event::{ArtifactRef, CheckpointReason, EventKind, NewEvent, Source};
    use spawn_core::id::{AgentId, BranchName, CommitId, SessionId};
    use spawn_core::port::{Commit, EventLog, WorkspaceStore};
    let db = Db::new("lost_push");
    let (base, source, scenarios, _integrator) = world("lost-push", "#!/bin/sh\nexit 0\n");
    let mission = db.gated_mission(&source, &scenarios);
    let task = ready_task(&db, db.reported_task(mission, "lost"));
    let root = base.join("host");
    let mirror = crate::sources::ensure_mirror(&root, Some(&source)).expect("mirror");
    let host = spawn_host::GitWorkspace::new(&mirror, &root);
    let branch = BranchName::new(task.agent_id.as_str()).expect("branch");
    host.ensure_branch(&branch, None).expect("branch");
    let workspace = host.mount(&branch).expect("mount");
    std::fs::write(Path::new(workspace.path()).join("work.txt"), "work\n").expect("write");
    let Commit::Created(commit) = host.commit(&workspace, "work").expect("commit") else {
        panic!("a checkpoint");
    };
    db.store
        .append(NewEvent {
            agent_id: AgentId::new(task.agent_id.as_str()).expect("agent"),
            session_id: SessionId::new(task.session_id.as_str()).expect("session"),
            execution_id: None,
            source: Source::Bridge,
            source_event_id: Some(format!("commit:{commit}")),
            kind: EventKind::ArtifactCommitted {
                reason: CheckpointReason::BaseMerged,
                artifact: ArtifactRef {
                    project: "missions".to_string(),
                    branch: branch.clone(),
                    commit: CommitId::new(commit.as_str()).expect("commit"),
                },
                files: vec!["work.txt".to_string()],
            },
        })
        .expect("recorded, never pushed");
    assert_eq!(
        super::Judge::pushed_candidate(&db.store, &mirror, &source, &task).expect("judged"),
        super::Freshness::Fresh(commit.as_str().to_string())
    );
    host.unmount(&workspace).expect("unmount");
    let _ = std::fs::remove_dir_all(&base);
}

/// A `host_fault` record as the integrator writes it, by its failures.
fn host_fault_record(task: i64, revision: i32, faults: u32) -> Value {
    json!({"type": "task.gated", "id": task, "revision": revision, "verdict": "host_fault",
           "base": "b0", "scenarios": 0,
           "reason": "host fault: every failure check-tests.sh printed (1) is a connection \
                          error to the test database; not a verdict on the work, judged again \
                          in 5 min",
           "log": "| FAILED api::tests::one: Connection refused", "evidence": "failures",
           "failures": [{"test": "api::tests::one", "message": "Connection refused",
                         "class": "connection"}],
           "host_faults": faults, "backoff_ms": 300_000, "escalate": faults == 2})
}

/// The integrator-down alerts on this store, as `spawn alerts` has them:
/// `(key, state)`, oldest first.
fn integrator_alerts(db: &Db) -> Vec<(String, String)> {
    db.store
        .alerts(0)
        .expect("the alerts")
        .as_array()
        .expect("alerts")
        .iter()
        .filter(|row| row["kind"] == "integrator_down")
        .map(|row| {
            (
                row["key"].as_str().unwrap_or_default().to_string(),
                row["state"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// Batch 11 (3), on the store: a gated mission's reported task waits
/// while no integrator holds its lock. The loop's alert step fires
/// `integrator_down` once the lock has been free at every reading for
/// the bound, once (not again while it is open), and clears it at the
/// first reading that finds the lock held. A source an owner holds (a
/// deploy restarting the integrator) and a halted mission are no work
/// waiting: nothing fires for them, however long.
#[test]
fn the_integrator_down_alert_fires_once_and_clears_when_it_runs_again() {
    let (base, source, scenarios, _) = world("integrator-down", "exit 0\n");
    let db = Db::new("integrator_down");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let (mut settings, _) = crate::alerts::Settings::from_env();
    settings.check_ms = 1;
    settings.integrator_down_ms = 300;
    settings.memory_low_mb = 0;
    settings.tmpfs_high_mb = 0;
    settings.disk_low_gb = 0;
    settings.test_log = None;
    settings.live_log = None;
    let mut monitor = crate::alerts::Monitor::with(settings);
    let mut check = || {
        std::thread::sleep(std::time::Duration::from_millis(5));
        monitor.check(&db.store).expect("a check");
    };
    let past_the_bound = || std::thread::sleep(std::time::Duration::from_millis(400));

    // An owner holds the source: no work waits, however long.
    db.admin()
        .execute(
            "insert into mission_source_holds (source, hold, main, by, reason, held_at_ms,
                                                   expires_ms)
                 values ($1, 1, $2, 'deploy', 'restart', spawn_now_ms(),
                         spawn_now_ms() + 600000)",
            &[&source, &"a".repeat(40)],
        )
        .expect("a hold");
    check();
    past_the_bound();
    check();
    assert_eq!(integrator_alerts(&db), [], "a held source");
    db.admin()
        .execute("delete from mission_source_holds", &[])
        .expect("released");
    // A halted mission: the same.
    db.record(&json!({"type": "mission.halted", "id": mission, "reason": "a test"}));
    check();
    past_the_bound();
    check();
    assert_eq!(integrator_alerts(&db), [], "a halted mission");
    db.store.mission_resume(mission).expect("resume");

    check();
    assert_eq!(integrator_alerts(&db), [], "not yet: the bound starts");
    past_the_bound();
    check();
    let fired = integrator_alerts(&db);
    assert_eq!(fired.len(), 1, "{fired:?}");
    assert_eq!(fired[0].1, "open");
    let row = db
        .store
        .alerts(0)
        .expect("the alerts")
        .as_array()
        .expect("alerts")
        .iter()
        .find(|row| row["kind"] == "integrator_down")
        .cloned()
        .expect("the alert");
    assert_eq!(row["subject"], "spawn-integrate");
    assert_eq!(
        row["evidence"]["waiting"],
        json!([{"mission": mission, "task": task}])
    );
    let text = row["text"].as_str().unwrap_or_default();
    assert!(
        text.starts_with("spawn-integrate is not running: no integrator has held its lock")
            && text.contains(&format!(
                "1 reported task waits for a gate (task #{task} of mission #{mission})"
            )),
        "{text}"
    );
    past_the_bound();
    check();
    assert_eq!(integrator_alerts(&db), fired, "once while it is open");

    // It runs again: the lock is held, the alert clears.
    assert!(db.store.integrator_try_lock().expect("the lock"));
    check();
    let cleared = integrator_alerts(&db);
    assert_eq!(cleared, [(fired[0].0.clone(), "cleared".to_string())]);
    past_the_bound();
    check();
    assert_eq!(integrator_alerts(&db).len(), 1, "held: nothing fires");
    let _ = std::fs::remove_dir_all(&base);
}

/// Batch 11 (2), on the store: the host fault's backoff ("the gate's
/// last run on task N was a host fault … judged again in Ns") is said
/// once, when the task starts waiting, not on every pass it goes on
/// waiting; once the task has had a gate, its next wait is said again.
#[test]
fn a_host_faults_backoff_is_said_once_per_wait() {
    let (base, source, scenarios, integrator) = world("host-fault-said", "exit 1\n");
    let db = Db::new("host_fault_said");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    db.record(&host_fault_record(task, 0, 1));
    let waiting = gated(&db, mission);
    let said = || integrator.backs_off(&waiting, &waiting.tasks[0]);
    assert!(
        super::gate_wait(&waiting, &waiting.tasks[0])
            .is_some_and(|(_, line)| line.contains("was a host fault (1 in a row)")),
        "it waits out the recorded due time"
    );
    assert_eq!(said(), Some(super::Said::Now), "it starts waiting: said");
    assert_eq!(said(), Some(super::Said::Before), "the next pass: quiet");
    assert_eq!(said(), Some(super::Said::Before));
    // Another task's wait is its own.
    assert_eq!(integrator.waiting.borrow().len(), 1);
    // It has its gate, which faults again: the new wait is said.
    integrator.wait_over(task);
    db.record(&host_fault_record(task, 0, 2));
    let again = gated(&db, mission);
    assert_eq!(
        integrator.backs_off(&again, &again.tasks[0]),
        Some(super::Said::Now)
    );
    assert_eq!(
        integrator.backs_off(&again, &again.tasks[0]),
        Some(super::Said::Before)
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Objective 20: a host fault between two rejections leaves the
/// rejection run as it was (two in a row, not reset, not three), and
/// between two aborts the abort run; the task stays where it was.
#[test]
fn a_host_fault_between_two_rejections_leaves_the_rejection_run() {
    let (base, source, scenarios, _) = world("host-fault-run", "exit 1\n");
    let db = Db::new("host_fault_run");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let rejected = |revision: i32| {
        json!({"type": "task.gated", "id": task, "revision": revision,
               "verdict": "rejected", "base": "b0", "note": "fails"})
    };
    db.record(&rejected(0));
    db.store.task_delivered(task, 2).expect("delivered again");
    assert!(db.store.task_reported(task, "again").expect("reported"));
    db.record(&host_fault_record(task, 1, 1));
    let after = gated(&db, mission);
    assert_eq!(after.gate_rejections, 1, "neither grown nor reset");
    assert_eq!(after.gate_host_faults, 1);
    assert_eq!(
        (after.tasks[0].state, after.tasks[0].revisions),
        (spawn_core::mission::TaskState::Reported, 1),
        "nothing went back to the worker"
    );
    db.record(&rejected(1));
    let after = gated(&db, mission);
    assert_eq!(
        after.gate_rejections, 2,
        "the run went on from where it was"
    );
    assert_eq!(
        after.gate_host_faults, 0,
        "a real verdict ends the host faults"
    );
    assert_eq!(super::halt_reason(&after, 0), None);

    // The abort run, the same.
    db.store.task_delivered(task, 3).expect("delivered again");
    assert!(db.store.task_reported(task, "again").expect("reported"));
    let aborted = json!({"type": "task.gated", "id": task, "revision": 2,
                         "verdict": "aborted", "reason": "a gate that died"});
    db.record(&aborted);
    db.record(&host_fault_record(task, 2, 1));
    db.record(&host_fault_record(task, 2, 2));
    let after = gated(&db, mission);
    assert_eq!((after.gate_aborts, after.gate_rejections), (1, 2));
    assert_eq!(after.gate_host_faults, 2);
    db.record(&aborted);
    let after = gated(&db, mission);
    assert_eq!((after.gate_aborts, after.gate_host_faults), (2, 0));
    assert_eq!(super::halt_reason(&after, 0), None);
    let _ = std::fs::remove_dir_all(&base);
}

/// Objective 29, end to end: its task scenario exits 6 (the gate cannot
/// judge it here: no systemd user manager in the sandbox) and the gate
/// states its `cannot_judge` fact. The task is gated `host_fault` with
/// the evidence `cannot_judge`: never rejected, never aborted, nothing
/// back to the worker, not judged again before its `due_ms`, escalating
/// at the second in a row as any host fault — and never capped into an
/// abort, however many in a row (`SPAWN_GATE_HOST_FAULT_CAP` counts no
/// `cannot_judge`).
#[test]
fn a_scenario_the_host_cannot_judge_is_a_host_fault_never_a_rejection() {
    let dir = std::env::temp_dir().join(format!("spawn-db-cannot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let runs = dir.join("runs");
    let runs_path = runs.display();
    let gate = format!(
        "n=$(cat '{runs_path}' 2>/dev/null || echo 0); n=$((n + 1)); echo \"$n\" >'{runs_path}'\n\
             echo '| == candidate: 029.sh must pass'\n\
             echo '029: cannot be judged here: no systemd user manager is reachable' >&2\n\
             printf '{{\"cannot_judge\":true,\"scenario\":\"029.sh\",\"runs\":\"6\"}}\n' \
             >\"$SPAWN_GATE_VERDICT\"\n\
             exit 6\n"
    );
    let (base, source, scenarios, mut integrator) = world("cannot-judge", &gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_millis(1),
        cap: 2,
    };
    let db = Db::new("cannot_judge");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    let count =
        || std::fs::read_to_string(&runs).map_or(0, |text| text.trim().parse::<u32>().unwrap_or(0));
    let pass = || {
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
    };

    pass();
    assert_eq!(count(), 1);
    let (record, _, at) = last_verdict(&db);
    assert_eq!(record["verdict"], "host_fault", "{record}");
    assert_eq!(record["evidence"], "cannot_judge");
    assert_eq!(record["scenario"], "029.sh");
    assert_eq!(record["host_faults"], 1);
    assert_eq!(record["escalate"], false);
    assert_eq!(record["backoff_ms"], 1);
    assert_eq!(record["due_ms"], at + 1, "its own time plus the backoff");
    assert!(record["candidate"].is_string());
    assert!(
        record["reason"].as_str().is_some_and(|reason| {
            reason.starts_with("host fault: this host cannot judge 029.sh")
                && reason.contains(
                    "it says: 029: cannot be judged here: no systemd user manager is reachable",
                )
        }),
        "the scenario's own line, quoted: {record}"
    );
    assert_eq!(
        record["cause"],
        "029: cannot be judged here: no systemd user manager is reachable"
    );
    assert!(
        record["log"]
            .as_str()
            .is_some_and(|log| log.contains("029: cannot be judged here")),
        "the gate's log is kept"
    );
    let after = gated(&db, mission);
    assert_eq!(
        (after.gate_host_faults, after.gate_host_task),
        (1, Some(task))
    );
    assert_eq!((after.gate_rejections, after.gate_aborts), (0, 0));
    assert_eq!(
        (after.tasks[0].state, after.tasks[0].revisions),
        (spawn_core::mission::TaskState::Reported, 0),
        "nothing went back to the worker"
    );
    assert!(after.halted.is_none());

    // Judged again after its due time (this test's backoff is 1 ms; the
    // hold itself is every host fault's, tested above): the second in a
    // row escalates, as any.
    past_due(&db);
    pass();
    assert_eq!(count(), 2);
    let record = last_verdict(&db).0;
    assert_eq!(record["verdict"], "host_fault", "{record}");
    assert_eq!(record["evidence"], "cannot_judge");
    assert_eq!(record["host_faults"], 2);
    assert_eq!(record["escalate"], true);

    // The cap (2 here) never reaches it: four faults in a row stay host
    // faults, and `gate_aborts` never moves.
    for _ in 0..2 {
        past_due(&db);
        pass();
    }
    assert_eq!(count(), 4);
    let record = last_verdict(&db).0;
    assert_eq!(record["verdict"], "host_fault", "{record}");
    assert_eq!(record["host_faults"], 4);
    let after = gated(&db, mission);
    assert_eq!((after.gate_aborts, after.gate_rejections), (0, 0));
    assert!(after.halted.is_none(), "no halt counts a host fault");
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Wait until the database's clock is past the latest record's `due_ms`:
/// a pass judges a task again only then, and the tests' backoffs of a few
/// milliseconds otherwise race a pass started at once after the last
/// (one short in the count, now and then).
fn past_due(db: &Db) {
    let due = last_verdict(db).0["due_ms"].as_i64().expect("a due time");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while db
        .admin()
        .query_one("select spawn_now_ms()", &[])
        .expect("the database's clock")
        .get::<_, i64>(0)
        <= due
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the clock never passed {due}"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// The task's latest `task.gated` record and its event's id and time.
fn last_verdict(db: &Db) -> (Value, i64, i64) {
    let row = db
        .admin()
        .query_one(
            "select kind -> 'detail' -> 'record', id, recorded_at_ms from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                  order by session_seq desc limit 1",
            &[],
        )
        .expect("the verdict");
    (row.get(0), row.get(1), row.get(2))
}

#[test]
fn an_empty_base_is_an_abort_before_pregate_git_work() {
    let (base, source, scenarios, integrator) = world("empty-base-pregate", "exit 3\n");
    let db = Db::new("empty_base_pregate");
    for empty_base in ["", " ", "\u{3000}"] {
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let row = snapshot(&db, mission);
        let ran = integrator
            .judge
            .judge(
                &db.store,
                Path::new("/nonexistent/mirror.git"),
                "/nonexistent/source.git",
                empty_base,
                &row,
                0,
                &ready_task(&db, task),
                None,
            )
            .expect("an empty base becomes a parameter abort before fetching a candidate");
        assert!(!ran, "a parameter error runs no gate");
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["id"], task, "{record}");
        assert_eq!(record["verdict"], "aborted", "{record}");
        assert_eq!(record["evidence"], "parameter_error", "{record}");
        assert!(
            record["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("base revision is empty")),
            "base={empty_base:?}: {record}"
        );
        assert_eq!(gated(&db, mission).gate_aborts, 1);
    }
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn an_empty_candidate_is_an_abort_before_pregate_git_work() {
    let (base, source, scenarios, integrator) = world("empty-candidate-pregate", "exit 3\n");
    let db = Db::new("empty_candidate_pregate");
    let mirror = mirror_of(&integrator, &source);
    let main = source_main(&source);
    for (index, empty_candidate) in ["", " ", "\u{3000}"].into_iter().enumerate() {
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent = agent_of(&db, mission, task);
        let pushed = push_work(&base, &source, &agent, &format!("work-{index}.txt"));
        git(&[
            "--git-dir",
            &mirror,
            "fetch",
            "-q",
            &source,
            &format!("+refs/heads/spawn/{agent}:refs/spawn-integrate/{agent}"),
        ])
        .expect("fetch the candidate");
        let row = snapshot(&db, mission);
        integrator
            .judge
            .judge_pushed_using(
                &db.store,
                Path::new(&mirror),
                &source,
                &main,
                &row,
                0,
                &ready_task(&db, task),
                Some(&pushed),
                None,
                |_, _, _, _| {
                    if index == 2 {
                        main_moves(&base, &source, "main-moves-empty-candidate.txt");
                    }
                    Ok(Some(empty_candidate.to_string()))
                },
            )
            .expect("an empty candidate becomes a parameter abort");
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["id"], task, "{record}");
        assert_eq!(record["verdict"], "aborted", "{record}");
        assert_eq!(record["evidence"], "parameter_error", "{record}");
        assert!(
            record["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("candidate revision is empty")),
            "candidate={empty_candidate:?}: {record}"
        );
        assert_eq!(gated(&db, mission).gate_aborts, 1);
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// An adopted judging (gate v2 step 2, D5) judges the candidate its
/// `judging.json` names: one that is empty or whitespace is a parameter
/// abort before any gate run is bound (no `env`), counted toward the abort
/// halt, as a candidate the landing merge returned empty is.
#[test]
fn an_empty_adopted_candidate_is_an_abort_before_the_gate() {
    let (base, source, scenarios, integrator) = world("empty-adopted-candidate", "exit 3\n");
    let db = Db::new("empty_adopted_candidate");
    let mirror = mirror_of(&integrator, &source);
    let main = source_main(&source);
    for (index, empty_candidate) in ["", " ", "\u{3000}"].into_iter().enumerate() {
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent = agent_of(&db, mission, task);
        let pushed = push_work(&base, &source, &agent, &format!("adopted-{index}.txt"));
        git(&[
            "--git-dir",
            &mirror,
            "fetch",
            "-q",
            &source,
            &format!("+refs/heads/spawn/{agent}:refs/spawn-integrate/{agent}"),
        ])
        .expect("fetch the branch");
        let mut judge = integrator.judge.clone();
        judge.adopted = Some(super::Adopted {
            candidate: empty_candidate.to_string(),
            branch: pushed,
            started_ms: super::now_ms(),
        });
        let row = snapshot(&db, mission);
        let ran = judge
            .judge(
                &db.store,
                Path::new(&mirror),
                &source,
                &main,
                &row,
                0,
                &ready_task(&db, task),
                None,
            )
            .expect("an empty adopted candidate becomes a parameter abort");
        assert!(ran, "the adopted judging is judged");
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["id"], task, "{record}");
        assert_eq!(record["verdict"], "aborted", "{record}");
        assert_eq!(record["evidence"], "parameter_error", "{record}");
        assert!(
            record["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("candidate revision is empty")),
            "candidate={empty_candidate:?}: {record}"
        );
        assert!(
            record["env"].is_null(),
            "refused before a gate run was bound: {record}"
        );
        assert_eq!(gated(&db, mission).gate_aborts, 1);
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// Objective 20, end to end: a gate that rejects for check-tests.sh
/// with connection errors only is recorded `host_fault` (by its
/// failures), the task stays reported and is not judged again before the
/// record's `due_ms`; the next, with database errors while the test
/// database's log gains a crash during the gate, is a host fault by the
/// log, the second in a row: escalated, its backoff doubled; the third
/// run accepts and the work lands, ending the run.
#[test]
#[allow(clippy::too_many_lines)]
fn a_host_fault_is_held_until_due_then_judged_again() {
    let dir = std::env::temp_dir().join(format!("spawn-db-host-fault-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let runs = dir.join("runs");
    let server_log = dir.join("postgres.log");
    std::fs::write(
        &server_log,
        "2026-01-01 00:00:00.000 UTC [1] LOG:  server process (PID 1) was terminated by \
             signal 9: Killed\n",
    )
    .expect("an old crash");
    let (runs_path, log_path) = (runs.display(), server_log.display());
    let gate = format!(
        "n=$(cat '{runs_path}' 2>/dev/null || echo 0); n=$((n + 1)); echo \"$n\" >'{runs_path}'\n\
             reject() {{\n\
               echo '| stage gate rejected 5ms regression: check-tests.sh must still pass'\n\
               printf '{{\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the candidate\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
               exit 0\n\
             }}\n\
             echo '| == regression: check-tests.sh must still pass'\n\
             if [ \"$n\" -eq 1 ]; then\n\
               echo '| FAILED api::tests::claims_in_order: error connecting to server: Connection refused (os error 111)'\n\
               echo '| FAILED stalls::tests::delivery: db error: FATAL: the database system is in recovery mode'\n\
               reject\n\
             elif [ \"$n\" -eq 2 ]; then\n\
               printf '%s [1] LOG:  server process (PID 4242) was terminated by signal 11: Segmentation fault\\n' \"$(date -u '+%Y-%m-%d %H:%M:%S.%3N UTC')\" >>'{log_path}'\n\
               echo '| FAILED merges::tests::a_conflict: db error: ERROR: cache lookup failed for relation 16388'\n\
               reject\n\
             fi\n\
             printf '{{\"verdict\":\"accepted\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n"
    );
    let (base, source, scenarios, mut integrator) = world("host-fault-pass", &gate);
    integrator.judge.host = super::HostSettings {
        server_log: Some(server_log.clone()),
        backoff: std::time::Duration::from_secs(2),
        cap: super::HOST_FAULT_CAP,
    };
    let db = Db::new("host_fault_pass");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    let count =
        || std::fs::read_to_string(&runs).map_or(0, |text| text.trim().parse::<u32>().unwrap_or(0));
    let pass = || {
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
    };
    let wait_until = |due: i64| {
        let left = due - super::now_ms();
        if left > 0 {
            std::thread::sleep(std::time::Duration::from_millis(
                u64::try_from(left).unwrap_or(0) + 50,
            ));
        }
    };

    pass();
    assert_eq!(count(), 1);
    let (record, event, at) = last_verdict(&db);
    assert_eq!(record["verdict"], "host_fault", "{record}");
    assert_eq!(record["evidence"], "failures");
    assert_eq!(record["failures"].as_array().map(Vec::len), Some(2));
    assert_eq!(record["failures"][1]["test"], "stalls::tests::delivery");
    assert_eq!(record["failures"][1]["class"], "connection");
    assert_eq!(record["host_faults"], 1);
    assert_eq!(record["escalate"], false);
    assert_eq!(record["backoff_ms"], 2000);
    assert_eq!(record["due_ms"], at + 2000, "its own time plus the backoff");
    assert!(record["candidate"].is_string() && record.get("crash").is_none());
    assert!(
        record["log"]
            .as_str()
            .is_some_and(|log| log.contains("| FAILED api::tests::claims_in_order"))
    );
    let after = gated(&db, mission);
    assert_eq!(
        (
            after.gate_host_faults,
            after.gate_host_task,
            after.gate_host_due_ms
        ),
        (1, Some(task), Some(at + 2000))
    );
    assert_eq!((after.gate_rejections, after.gate_aborts), (0, 0));
    assert_eq!(
        (after.tasks[0].state, after.tasks[0].revisions),
        (spawn_core::mission::TaskState::Reported, 0)
    );
    assert!(after.halted.is_none());

    // Held until its due time.
    pass();
    assert_eq!(count(), 1, "not judged again before due_ms");
    assert_eq!(last_verdict(&db).1, event);
    wait_until(at + 2000);

    pass();
    assert_eq!(count(), 2);
    let (record, event, at) = last_verdict(&db);
    assert_eq!(record["verdict"], "host_fault", "{record}");
    assert_eq!(record["evidence"], "server_log");
    assert_eq!(record["failures"][0]["class"], "database");
    let crash = record["crash"].as_array().expect("the crash lines");
    assert_eq!(
        crash.len(),
        1,
        "the old crash is outside the window: {record}"
    );
    assert!(
        crash[0]
            .as_str()
            .is_some_and(|line| line.contains("(PID 4242) was terminated by signal 11"))
    );
    assert_eq!(record["host_faults"], 2);
    assert_eq!(record["escalate"], true);
    assert_eq!(record["backoff_ms"], 4000);
    assert_eq!(record["due_ms"], at + 4000);
    let escalations = db.store.gate_host_fault_escalations().expect("escalations");
    assert_eq!(
        escalations.as_array().map(Vec::len),
        Some(1),
        "{escalations}"
    );
    assert_eq!(escalations[0]["event_id"], event);
    assert_eq!(escalations[0]["mission"], mission);
    assert!(
        escalations[0].get("log").is_none(),
        "the gate's log stays out"
    );
    assert_eq!(gated(&db, mission).gate_host_faults, 2);

    pass();
    assert_eq!(count(), 2, "held again, for the doubled backoff");
    wait_until(at + 4000);
    pass();
    assert_eq!(count(), 3);
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
    let after = db.mission(mission);
    assert!(after["halted"].is_null());
    let missions: Vec<super::Mission> =
        serde_json::from_value(db.store.gated_missions().expect("gated")).expect("missions");
    let after = missions
        .into_iter()
        .find(|found| found.id == mission)
        .expect("the mission");
    assert_eq!(
        (
            after.gate_host_faults,
            after.gate_host_task,
            after.gate_host_due_ms
        ),
        (0, None, None),
        "the landing ends the run"
    );
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sandbox that never started its command is recorded with its own
/// evidence, `sandbox_never_started`, beside the gate's fact, so a
/// record names the fault without its text being read.
#[test]
fn a_sandbox_that_never_started_has_its_own_evidence() {
    let failure = crate::gate::GateFailure::HostFault {
        scenario: "sh build".to_string(),
        on: "sandbox".to_string(),
        runs: "never started".to_string(),
        reason: "sandbox: sh build in /w/candidate never started (bwrap exited 1: bwrap: \
                     Can't chdir to /w/candidate: Permission denied)"
            .to_string(),
        tree: Some(crate::gate::SandboxTree::Candidate),
        log: String::new(),
    };
    let record = super::gate_host_fault_fields(&failure, 1, std::time::Duration::from_mins(1));
    assert_eq!(record["evidence"], crate::gate::SANDBOX_NEVER_STARTED);
    assert_eq!(record["fact"]["on"], "sandbox");
    assert_eq!(record["fact"]["tree"], "candidate");
    let reason = record["reason"].as_str().expect("a reason");
    assert!(
        reason.starts_with("host fault: the gate's sandbox never started sh build (sandbox: ")
            && reason.contains("Can't chdir"),
        "{reason}"
    );
}

/// A gate whose runner ended without its status (gate v2 step 2, D6:
/// gate.rs `wait_unit`) is recorded with evidence `abandoned`, through
/// the store; it is a host fault the candidate may cause (an OOM), so it
/// counts toward the cap, never a verdict.
#[test]
fn an_abandoned_gate_is_a_host_fault_that_counts_toward_the_cap() {
    let failure = crate::gate::GateFailure::HostFault {
        scenario: "gate".to_string(),
        on: "runner".to_string(),
        runs: "no status".to_string(),
        reason: "the gate's runner (spawn-gate-1-2-3.service) ended without writing the \
                     gate's status: a reboot, an OOM kill or a stop from outside"
            .to_string(),
        tree: None,
        log: "its last words\n".to_string(),
    };
    let fields = super::gate_host_fault_fields(&failure, 1, std::time::Duration::from_mins(1));
    assert_eq!(fields["evidence"], crate::gate::ABANDONED);
    assert_eq!(fields["fact"]["on"], "runner");
    let reason = fields["reason"].as_str().expect("a reason");
    assert!(
        reason.starts_with("host fault: the gate was abandoned: "),
        "{reason}"
    );
    assert!(!super::base_only_fault(&fields), "counted toward the cap");
    let db = Db::new("abandoned_gate");
    let mission = db.gated_mission("/srv/abandoned", Path::new("/scenarios"));
    let task = db.reported_task(mission, "w");
    let mut record = json!({"type": "task.gated", "id": task, "revision": 0,
                            "verdict": "host_fault", "base": "b0"});
    if let (Some(target), Some(extra)) = (record.as_object_mut(), fields.as_object()) {
        target.extend(extra.clone());
    }
    db.record(&record);
    assert_eq!(
        db.mission(mission)["tasks"][0]["state"],
        "reported",
        "no verdict"
    );
}

/// The gate's own host faults (exit 6 with its fact, gate.rs
/// `HOST_FAULT_EXIT`): a scenario's harness error on the base (012.sh
/// exited 1, then 0: Supply PA's gate aborted twice on that, 2026-11-08)
/// and a check failing on the candidate and on the base (task 127's
/// e2e) are each `host_fault` by the gate's fact: `gate_aborts` does not
/// move, the task stays reported and is held until its due time, the
/// second in a row escalates with its backoff doubled, and the next run
/// that accepts lands it.
#[test]
#[allow(clippy::too_many_lines)]
fn the_gates_own_host_faults_are_held_and_never_counted() {
    let dir = std::env::temp_dir().join(format!("spawn-db-gate-fault-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let runs = dir.join("runs");
    let runs_path = runs.display();
    let gate = format!(
        "n=$(cat '{runs_path}' 2>/dev/null || echo 0); n=$((n + 1)); echo \"$n\" >'{runs_path}'\n\
             if [ \"$n\" -eq 1 ]; then\n\
               echo '| == base: 012.sh'\n\
               echo '| 012.sh exited 1, then 0 on the base: a harness error, not a verdict on the candidate'\n\
               printf '{{\"host_fault\":true,\"scenario\":\"012.sh\",\"on\":\"base\",\"runs\":\"1, then 0\",\"reason\":\"012.sh exited 1, then 0 on the base\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
               exit 6\n\
             elif [ \"$n\" -eq 2 ]; then\n\
               echo '| == regression: check-e2e.sh must still pass'\n\
               printf '{{\"host_fault\":true,\"scenario\":\"check-e2e.sh\",\"on\":\"candidate and base\",\"runs\":\"1, then 1 on the candidate; 1 on the base\",\"reason\":\"check-e2e.sh failed twice on the candidate and on the base\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
               exit 6\n\
             fi\n\
             printf '{{\"verdict\":\"accepted\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n"
    );
    let (base, source, scenarios, mut integrator) = world("gate-fault", &gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_secs(2),
        cap: super::HOST_FAULT_CAP,
    };
    let db = Db::new("gate_host_fault");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    let count =
        || std::fs::read_to_string(&runs).map_or(0, |text| text.trim().parse::<u32>().unwrap_or(0));
    let pass = || {
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
    };
    let wait_until = |due: i64| {
        let left = due - super::now_ms();
        if left > 0 {
            std::thread::sleep(std::time::Duration::from_millis(
                u64::try_from(left).unwrap_or(0) + 50,
            ));
        }
    };

    // A scenario's harness error on the base.
    pass();
    assert_eq!(count(), 1);
    let (record, event, at) = last_verdict(&db);
    assert_eq!(record["verdict"], "host_fault", "{record}");
    assert_eq!(record["evidence"], "gate");
    assert_eq!(
        record["fact"],
        json!({"scenario": "012.sh", "on": "base", "runs": "1, then 0",
               "reason": "012.sh exited 1, then 0 on the base"})
    );
    assert!(
        record["reason"].as_str().is_some_and(
            |reason| reason.starts_with("host fault: 012.sh exited 1, then 0 on the base")
        ),
        "{record}"
    );
    assert!(
        record["log"]
            .as_str()
            .is_some_and(|log| log.contains("| == base: 012.sh"))
    );
    assert_eq!(
        (
            &record["host_faults"],
            &record["escalate"],
            &record["backoff_ms"]
        ),
        (&json!(1), &json!(false), &json!(2000))
    );
    assert_eq!(record["due_ms"], at + 2000);
    assert!(record["candidate"].is_string());
    let after = gated(&db, mission);
    assert_eq!(
        (
            after.gate_host_faults,
            after.gate_host_task,
            after.gate_aborts,
            after.gate_rejections
        ),
        (1, Some(task), 0, 0),
        "a host fault, not an abort"
    );
    assert_eq!(
        (after.tasks[0].state, after.tasks[0].revisions),
        (spawn_core::mission::TaskState::Reported, 0)
    );
    pass();
    assert_eq!(count(), 1, "not judged again before due_ms");
    assert_eq!(last_verdict(&db).1, event);
    wait_until(at + 2000);

    // A check failing on the candidate and on the base.
    pass();
    assert_eq!(count(), 2);
    let (record, event, at) = last_verdict(&db);
    assert_eq!(record["verdict"], "host_fault", "{record}");
    assert_eq!(record["fact"]["scenario"], "check-e2e.sh");
    assert_eq!(record["fact"]["on"], "candidate and base");
    assert_eq!(
        (
            &record["host_faults"],
            &record["escalate"],
            &record["backoff_ms"]
        ),
        (&json!(2), &json!(true), &json!(4000))
    );
    let escalations = db.store.gate_host_fault_escalations().expect("escalations");
    assert_eq!(escalations[0]["event_id"], event, "{escalations}");
    let after = gated(&db, mission);
    assert_eq!((after.gate_host_faults, after.gate_aborts), (2, 0));
    assert_eq!(super::halt_reason(&after, 0), None);

    pass();
    assert_eq!(count(), 2, "held again, for the doubled backoff");
    wait_until(at + 4000);
    pass();
    assert_eq!(count(), 3);
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
    assert_eq!(gated(&db, mission).gate_aborts, 0);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sandbox that never started its command, recorded through the
/// store for every tree the gate names (rev-e on 81dd42f9: the store's
/// host_fault check refused the evidence, and no test wrote one): each
/// is `host_fault` with evidence `sandbox_never_started` and its tree.
/// The base's and the gate's own come before any candidate code ran, as
/// a fault on the base does: neither counts toward the host-fault cap
/// nor is ever capped. With the cap at 1, the candidate's first is a
/// host fault, a base one after it still is, and the candidate's second
/// is capped, recorded `aborted`.
#[test]
#[allow(clippy::too_many_lines)]
fn a_sandbox_that_never_started_is_recorded_for_each_tree_and_capped_only_on_the_candidate() {
    let dir = std::env::temp_dir().join(format!("spawn-db-never-started-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let runs = dir.join("runs");
    let runs_path = runs.display();
    let fact = |scenario: &str, tree: &str| {
        format!(
            "printf '{{\"host_fault\":true,\"scenario\":\"{scenario}\",\"on\":\"sandbox\",\
                 \"runs\":\"never started\",\"reason\":\"sandbox: {scenario} in /w/{tree} never \
                 started (bwrap exited 1: bwrap: cannot mount tmpfs on /run/x)\",\
                 \"evidence\":\"sandbox_never_started\",\"tree\":\"{tree}\"}}\\n' \
                 >\"$SPAWN_GATE_VERDICT\"\nexit 6\n"
        )
    };
    let gate = format!(
        "n=$(cat '{runs_path}' 2>/dev/null || echo 0); n=$((n + 1)); echo \"$n\" >'{runs_path}'\n\
             case \"$n\" in\n\
             1) {}\n;;\n\
             2) {}\n;;\n\
             3) {}\n;;\n\
             4) {}\n;;\n\
             5) {}\n;;\n\
             esac\n\
             printf '{{\"verdict\":\"accepted\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n",
        fact("sh build", "base"),
        fact("socket-scan", "gate"),
        fact("sh build", "candidate"),
        fact("sh 012.sh", "base"),
        fact("sh check-e2e.sh", "candidate"),
    );
    let (base, source, scenarios, mut integrator) = world("never-started", &gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_millis(300),
        cap: 1,
    };
    let db = Db::new("gate_never_started");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    let count =
        || std::fs::read_to_string(&runs).map_or(0, |text| text.trim().parse::<u32>().unwrap_or(0));
    // One judging: wait out the last record's due time, then pass.
    let judged = |n: u32| -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while count() < n {
            assert!(std::time::Instant::now() < deadline, "run {n} never came");
            integrator.pass_on(&db.store).expect("a pass");
            integrator.settle();
            if count() < n {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        last_verdict(&db).0
    };
    for (n, tree) in [(1, "base"), (2, "gate"), (3, "candidate"), (4, "base")] {
        let record = judged(n);
        assert_eq!(record["verdict"], "host_fault", "run {n}: {record}");
        assert_eq!(
            record["evidence"],
            crate::gate::SANDBOX_NEVER_STARTED,
            "{record}"
        );
        assert_eq!(record["fact"]["on"], "sandbox", "{record}");
        assert_eq!(record["fact"]["tree"], tree, "{record}");
        assert_eq!(record["fact"]["runs"], "never started", "{record}");
        assert!(
            record["reason"].as_str().is_some_and(
                |reason| reason.starts_with("host fault: the gate's sandbox never started")
            ),
            "{record}"
        );
    }
    // Only the candidate's counted: one, at the cap, so the next
    // candidate fault is capped.
    assert_eq!(
        db.store
            .task_host_faults(task, 0, &source_main(&source))
            .expect("the count"),
        1,
        "the base's and the gate's own do not count"
    );
    let record = judged(5);
    assert_eq!(record["verdict"], "aborted", "{record}");
    assert_eq!(
        record["host_fault"]["evidence"],
        crate::gate::SANDBOX_NEVER_STARTED,
        "{record}"
    );
    assert_eq!(
        record["host_fault"]["fact"]["tree"], "candidate",
        "{record}"
    );
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The gate's own test server that did not start (gate.sh
/// `start_server`), recorded through the store for both starts: the
/// first (`gate`) and the candidate phase's fresh one (`candidate`).
/// Each is `host_fault` with evidence `test_server_never_started` and
/// its start. Neither is the candidate's doing: with the cap at 1, both
/// are recorded as host faults, neither counts toward the task, and the
/// next judging lands.
#[test]
fn a_test_server_that_never_started_is_recorded_for_both_starts_and_never_counted() {
    let dir = std::env::temp_dir().join(format!(
        "spawn-db-server-never-started-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let runs = dir.join("runs");
    let runs_path = runs.display();
    let fact = |tree: &str, what: &str| {
        format!(
            "printf '{{\"host_fault\":true,\"scenario\":\"test server\",\"on\":\"test server\",\
                 \"runs\":\"never started\",\"reason\":\"{what} did not start: FATAL:  could not \
                 create shared memory segment: a host fault, not a verdict on the candidate\",\
                 \"evidence\":\"test_server_never_started\",\"tree\":\"{tree}\"}}\\n' \
                 >\"$SPAWN_GATE_VERDICT\"\nexit 6\n"
        )
    };
    let gate = format!(
        "n=$(cat '{runs_path}' 2>/dev/null || echo 0); n=$((n + 1)); echo \"$n\" >'{runs_path}'\n\
             case \"$n\" in\n\
             1) {}\n;;\n\
             2) {}\n;;\n\
             esac\n\
             printf '{{\"verdict\":\"accepted\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n",
        // No apostrophe: the fact is in a single-quoted printf.
        fact("gate", "the first test server"),
        fact("candidate", "the fresh test server of the candidate phase"),
    );
    let (base, source, scenarios, mut integrator) = world("server-never-started", &gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_millis(300),
        cap: 1,
    };
    let db = Db::new("gate_server_never_started");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    let count =
        || std::fs::read_to_string(&runs).map_or(0, |text| text.trim().parse::<u32>().unwrap_or(0));
    let judged = |n: u32| -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while count() < n {
            assert!(std::time::Instant::now() < deadline, "run {n} never came");
            integrator.pass_on(&db.store).expect("a pass");
            integrator.settle();
            if count() < n {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        last_verdict(&db).0
    };
    for (n, tree) in [(1, "gate"), (2, "candidate")] {
        let record = judged(n);
        assert_eq!(record["verdict"], "host_fault", "run {n}: {record}");
        assert_eq!(
            record["evidence"],
            crate::gate::TEST_SERVER_NEVER_STARTED,
            "{record}"
        );
        assert_eq!(record["fact"]["on"], "test server", "{record}");
        assert_eq!(record["fact"]["tree"], tree, "{record}");
        assert_eq!(record["fact"]["runs"], "never started", "{record}");
        assert!(
            record["reason"]
                .as_str()
                .is_some_and(|reason| reason
                    .starts_with("host fault: the gate's own test server never started")),
            "{record}"
        );
    }
    assert_eq!(
        db.store
            .task_host_faults(task, 0, &source_main(&source))
            .expect("the count"),
        0,
        "no server start counts toward the task"
    );
    let record = judged(3);
    assert_eq!(record["verdict"], "accepted", "{record}");
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The host-fault cap (rev-e on gate-check-flakes): one report revision
/// of a task has at most `SPAWN_GATE_HOST_FAULT_CAP` uncounted host
/// faults. With the cap at 2, a gate that reports the same host fault on
/// every run is `host_fault` twice (no count), and the third run is
/// recorded `aborted` with a reason that says why, the fault kept under
/// `host_fault`: it counts toward the abort halt, as the next one does.
#[test]
fn host_faults_past_the_cap_on_one_revision_count_as_aborts() {
    // A host fault the cap counts: check-tests.sh's failures, every one
    // a connection error to the test database (evidence `failures`).
    let gate = "echo '| == regression: check-tests.sh must still pass'\n\
                    echo '| FAILED api::tests::one: error connecting to server: Connection refused (os error 111)'\n\
                    echo '| stage gate rejected 5ms regression: check-tests.sh must still pass'\n\
                    printf '{\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the candidate\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
    let (base, source, scenarios, mut integrator) = world("gate-fault-cap", gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_secs(1),
        cap: 2,
    };
    let db = Db::new("gate_host_fault_cap");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    // One judging: a pass, its gate settled, after the last record's
    // due time (a host fault's backoff) has come.
    let judge_once = |due: Option<i64>| {
        if let Some(due) = due {
            let left = due - super::now_ms();
            if left > 0 {
                std::thread::sleep(std::time::Duration::from_millis(
                    u64::try_from(left).unwrap_or(0) + 50,
                ));
            }
        }
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        last_verdict(&db).0
    };

    let first = judge_once(None);
    assert_eq!(first["verdict"], "host_fault", "{first}");
    let second = judge_once(first["due_ms"].as_i64());
    assert_eq!(second["verdict"], "host_fault", "the cap is 2: {second}");
    assert_eq!(gated(&db, mission).gate_aborts, 0, "no count yet");

    let third = judge_once(second["due_ms"].as_i64());
    assert_eq!(third["verdict"], "aborted", "past the cap: {third}");
    let reason = third["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains(&format!(
            "task {task} has had 2 at report revision 0 on main {}",
            &source_main(&source)[..12]
        )) && reason.contains("SPAWN_GATE_HOST_FAULT_CAP allows is 2")
            && reason.contains("counts as an abort")
            && reason.contains("is a connection error to the test database")
            && !reason.contains("judged again"),
        "{reason}"
    );
    assert_eq!(third["host_fault"]["evidence"], "failures", "{third}");
    assert_eq!(
        third["host_fault"]["failures"][0]["test"],
        "api::tests::one"
    );
    assert!(third["log"].is_string(), "{third}");
    let after = gated(&db, mission);
    assert_eq!(
        (after.gate_aborts, after.gate_host_faults),
        (1, 0),
        "an abort, which ends the run of host faults"
    );
    assert_eq!(
        after.tasks[0].state,
        spawn_core::mission::TaskState::Reported
    );

    // The same revision's next host fault counts too.
    let fourth = judge_once(None);
    assert_eq!(fourth["verdict"], "aborted", "{fourth}");
    assert_eq!(gated(&db, mission).gate_aborts, 2);
    assert_eq!(
        db.store
            .task_host_faults(task, 0, &source_main(&source))
            .expect("count"),
        2
    );

    assert_eq!(super::host_fault_cap(None), 3);
    assert_eq!(super::host_fault_cap(Some(" 5 ")), 5);
    assert_eq!(super::host_fault_cap(Some("0")), 3);
    assert_eq!(super::host_fault_cap(Some("x")), 3);
    let _ = std::fs::remove_dir_all(&base);
}

/// A gate that notes its candidate in `<marks>/started`, holds until
/// `<marks>/release` exists, then writes `verdict` and exits `exit`:
/// main moves while it holds.
fn gate_holding_then(marks: &Path, lines: &str, verdict: &str, exit: i32) -> String {
    let marks = marks.to_string_lossy();
    format!(
        "echo \"$3\" >>'{marks}/started'\n\
             while [ ! -e '{marks}/release' ]; do sleep 0.05; done\n\
             {lines}\
             printf '%s\\n' '{verdict}' >\"$SPAWN_GATE_VERDICT\"\n\
             exit {exit}\n"
    )
}

/// One judging whose gate holds while main moves on the source: the
/// pass starts the gate on `m0`, main moves (the owner's push, recorded
/// `source.moved` as `spawn mission moved` does, so no pass halts on a
/// main no record explains), the gate is released and settled. Main's
/// new commit.
fn judged_while_main_moves(
    integrator: &Integrator,
    db: &Db,
    base: &Path,
    source: &str,
    marks: &Path,
    file: &str,
) -> String {
    let _ = std::fs::remove_file(marks.join("release"));
    let before = started(marks).len();
    integrator.pass_on(&db.store).expect("a pass");
    assert_eq!(
        wait_started(marks, before + 1).len(),
        before + 1,
        "the gate started"
    );
    let was = source_main(source);
    let now = main_moves(base, source, file);
    db.store.source_moved(source, &was, &now).expect("moved");
    std::fs::write(marks.join("release"), "").expect("release");
    integrator.settle();
    now
}

/// Main moved while the gate ran (design-stale-on-main-move.md): every
/// verdict but `accepted` (whose landing's compare-and-swap is its own
/// guard) is `stale`, judged again on the new main, with what the gate
/// found (`found`), main then (`base`) and now (`main_now`), and its
/// reason; nothing is counted: no rejection, no abort, no run of host
/// faults, no room count, and the task is ready again.
#[test]
fn a_verdict_on_a_main_that_moved_while_the_gate_ran_is_stale() {
    let cases = [
        (
            "host_fault",
            "echo '| == regression: check-tests.sh must still pass'\n\
                 echo '| FAILED api::tests::one: error connecting to server: Connection refused (os error 111)'\n",
            "{\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the candidate\"}",
            0,
        ),
        (
            "rejected",
            "",
            "{\"verdict\":\"rejected\",\"reason\":\"regression: 012.sh must still pass\"}",
            0,
        ),
        (
            "no_room",
            "",
            "{\"no_room\":true,\"free_kb\":2048,\"min_gb\":10,\"tmpdir\":\"/data/tmp\"}",
            5,
        ),
        ("aborted", "echo 'gate: a harness error' >&2\n", "", 3),
    ];
    for (found, lines, verdict, exit) in cases {
        let marks = fresh_marks(&format!("spawn-stale-move-{found}"));
        let release = Release(marks.clone());
        let (base, source, scenarios, mut integrator) = world(
            &format!("stale-move-{found}"),
            &gate_holding_then(&marks, lines, verdict, exit),
        );
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_secs(1),
            cap: 3,
        };
        let db = Db::new(&format!("stale_move_{found}"));
        let m0 = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent = agent_of(&db, mission, task);
        push_work(&base, &source, &agent, "work.txt");

        let m1 = judged_while_main_moves(&integrator, &db, &base, &source, &marks, "main.txt");
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["verdict"], "stale", "{found}: {record}");
        assert_eq!(record["found"], found, "{found}: {record}");
        assert_eq!(record["base"], m0.as_str(), "{found}");
        assert_eq!(record["main_now"], m1.as_str(), "{found}");
        let reason = record["reason"].as_str().unwrap_or_default();
        assert!(
            reason.starts_with(&format!(
                "main moved from {} to {} while the gate ran: its {found}",
                &m0[..12],
                &m1[..12]
            )) && reason.ends_with("is no verdict on the new main; judged again"),
            "{found}: {reason}"
        );
        assert!(record.get("backoff_ms").is_none(), "{found}: {record}");
        let after = gated(&db, mission);
        assert_eq!(
            (
                after.gate_rejections,
                after.gate_aborts,
                after.gate_host_faults
            ),
            (0, 0, 0),
            "{found}: nothing counted"
        );
        assert_eq!(
            after.tasks[0].state,
            spawn_core::mission::TaskState::Reported,
            "{found}: ready to be judged again"
        );
        assert!(after.halted.is_none(), "{found}");
        assert_eq!(
            db.store.task_host_faults(task, 0, &m0).expect("count"),
            0,
            "{found}: no host fault for the cap"
        );
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&marks);
    }
}

/// Supply PA's mission 6, 114516 (2026-11-09): two gate aborts in a row,
/// a task at the host-fault cap, and a gate started on main A whose
/// verdict lands, after main moved to B, as a host fault the cap would
/// make the third abort and halt the mission. It is `stale` instead,
/// found `host_fault`: `gate_aborts` stays 2, and no halt.
#[test]
fn a_capped_host_fault_on_a_main_that_moved_is_stale_and_halts_nothing() {
    let marks = fresh_marks("spawn-stale-114516");
    let release = Release(marks.clone());
    let gate = gate_holding_then(
        &marks,
        "echo '| == regression: check-tests.sh must still pass'\n\
             echo '| FAILED api::tests::one: error connecting to server: Connection refused (os error 111)'\n",
        "{\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the candidate\"}",
        0,
    );
    let (base, source, scenarios, mut integrator) = world("stale-114516", &gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_millis(1),
        cap: 1,
    };
    let db = Db::new("stale_114516");
    let a = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    for _ in 0..2 {
        db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                          "verdict": "aborted", "base": a, "reason": "a harness error"}));
    }
    let agent = agent_of(&db, mission, task);
    push_work(&base, &source, &agent, "work.txt");
    // The task's first host fault, on A: the cap (1) is reached.
    std::fs::write(marks.join("release"), "").expect("release");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    let first = last_verdict(&db).0;
    assert_eq!(first["verdict"], "host_fault", "{first}");
    assert_eq!(db.store.task_host_faults(task, 0, &a).expect("count"), 1);
    assert_eq!(gated(&db, mission).gate_aborts, 2);
    past_due(&db);

    let b = judged_while_main_moves(&integrator, &db, &base, &source, &marks, "main.txt");
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["verdict"], "stale", "never capped: {record}");
    assert_eq!(record["found"], "host_fault");
    assert_eq!(
        (record["base"].as_str(), record["main_now"].as_str()),
        (Some(a.as_str()), Some(b.as_str()))
    );
    assert!(record.get("host_fault").is_none(), "{record}");
    let after = gated(&db, mission);
    assert_eq!(after.gate_aborts, 2, "gate_aborts unchanged");
    assert_eq!(super::halt_reason(&after, 0), None, "no halt");
    assert!(after.halted.is_none());
    drop(release);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// An owner's landing requested while a gate of its source runs moves
/// main only after that gate (design-owner-push-between-gates.md, D1):
/// a pass during the gate applies nothing, the task's verdict is its
/// own (`accepted`, landed on the main it was judged against, never
/// `stale`), and the next pass lands the owner's commit on the task's
/// landing. On a step 0 that applies every request at once, main moved
/// under the gate and the verdict came back `stale` (Supply PA tasks
/// 214, 218, 220, 222, 223, 226 and 227, 2026-11-09/10).
#[test]
fn an_owner_push_requested_while_a_gate_runs_moves_main_only_after_it() {
    let marks = fresh_marks("spawn-owner-between");
    let release = Release(marks.clone());
    let gate = gate_holding_then(&marks, "", "{\"verdict\":\"accepted\"}", 0);
    let (base, source, scenarios, integrator) = world("owner-between", &gate);
    let db = Db::new("owner_between");
    let m0 = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = agent_of(&db, mission, task);
    push_work(&base, &source, &agent, "work.txt");
    let _ = std::fs::remove_file(marks.join("release"));
    integrator.pass_on(&db.store).expect("a pass");
    assert_eq!(wait_started(&marks, 1).len(), 1, "the task's gate started");

    let commit = owner_commit(&base, &source, "CONTRACT.md", "v1\n", "owner: the contract");
    let request = owner_request(&db, &source, &commit);
    integrator
        .pass_on(&db.store)
        .expect("a pass while the gate runs");
    assert_eq!(
        source_main(&source),
        m0,
        "no owner's landing moves main under a gate of its source"
    );
    assert!(control_records(&db, "source.moved").is_empty());

    std::fs::write(marks.join("release"), "").expect("release");
    integrator.settle();
    let (record, _, _) = last_verdict(&db);
    assert_eq!(
        record["verdict"], "accepted",
        "the task's own verdict: {record}"
    );
    let landed = control_records(&db, "task.landed");
    assert_eq!(landed.len(), 1, "{landed:?}");
    let landing = landed[0]["to"].as_str().expect("the landing").to_string();
    assert_eq!(landed[0]["from"], m0.as_str());
    assert_eq!(source_main(&source), landing);

    integrator
        .pass_on(&db.store)
        .expect("the pass after the gate");
    let moved = control_records(&db, "source.moved");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["request"], json!(request));
    assert_eq!(moved[0]["from"], landing.as_str());
    let now = source_main(&source);
    assert_eq!(
        git(&["-C", &source, "rev-parse", &format!("{now}^1")]).expect("first parent"),
        landing,
        "the owner's landing is on the task's"
    );
    assert_eq!(
        db.mission(mission)["baseline"],
        now.as_str(),
        "the mission's baseline follows the owner's landing"
    );
    assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));
    drop(release);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// The owner's commit with `files` (paths under the tree, directories
/// made), put in the source at its owner's ref, no branch moved.
fn owner_commit_files(base: &Path, source: &str, files: &[(&str, &str)], subject: &str) -> String {
    let clone = base.join(format!("owner-files-{}", subject.len()));
    let clone_path = clone.to_string_lossy().to_string();
    let _ = std::fs::remove_dir_all(&clone);
    git(&["clone", "-q", source, &clone_path]).expect("the owner's clone");
    for (file, text) in files {
        let path = clone.join(file);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("directories");
        std::fs::write(path, text).expect("write");
    }
    super::tests::commit_in(&clone_path, subject);
    let commit = git(&["-C", &clone_path, "rev-parse", "HEAD"]).expect("the commit");
    let reference = super::owner::owner_ref(&commit);
    git(&[
        "-C",
        &clone_path,
        "push",
        "-q",
        "origin",
        &format!("{commit}:{reference}"),
    ])
    .expect("into the source, no branch");
    commit
}

/// A gated owner's request, as `spawn mission push --gated` records it.
fn gated_request(db: &Db, source: &str, commit: &str, reason: &str) -> i64 {
    db.store
        .source_push_request(
            source,
            commit,
            &super::owner::owner_ref(commit),
            &spawn_postgres::PushRequest {
                gated: true,
                reason: Some(reason.to_string()),
                ..spawn_postgres::PushRequest::default()
            },
        )
        .expect("the owner's gated request")
}

/// A gated owner's push (design-owner-push-between-gates.md) is a
/// judging of its source: requested while a task's gate runs, it starts
/// only after that gate, and ahead of the source's next ready task. Its
/// gate judges the owner's landing merge on the task's landing, with
/// the candidate's own scenarios and an empty protected list (D3), and
/// its acceptance moves main to that merge, `source.moved` naming the
/// verdict. The next task is judged on the new main after it.
#[test]
#[allow(clippy::too_many_lines)]
fn a_gated_owner_push_is_judged_after_the_running_gate_and_before_the_next_task() {
    let marks = fresh_marks("spawn-owner-gated");
    let release = Release(marks.clone());
    let seen = marks.to_string_lossy().to_string();
    let gate = gate_holding_then(
        &marks,
        &format!(
            "echo \"$4\" >>'{seen}/scenarios-given'\n\
                 cat \"$4/protected\" >>'{seen}/protected-seen'\n"
        ),
        "{\"verdict\":\"accepted\"}",
        0,
    );
    let (base, source, scenarios, integrator) = world("owner-gated", &gate);
    let db = Db::new("owner_gated");
    let m0 = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = agent_of(&db, mission, task);
    push_work(&base, &source, &agent, "work.txt");
    let _ = std::fs::remove_file(marks.join("release"));
    integrator.pass_on(&db.store).expect("a pass");
    assert_eq!(wait_started(&marks, 1).len(), 1, "the task's gate started");

    let commit = owner_commit_files(
        &base,
        &source,
        &[
            ("scenarios/protected", "scenarios/\n"),
            ("scenarios/check-contract.sh", "exit 0\n"),
            ("CONTRACT.md", "v1\n"),
        ],
        "owner: the contract",
    );
    let request = gated_request(&db, &source, &commit, "the contract, v1");
    let second = db.reported_task(mission, "w2");
    let second_agent = agent_of(&db, mission, second);
    push_work(&base, &source, &second_agent, "second.txt");
    integrator
        .pass_on(&db.store)
        .expect("a pass while the gate runs");
    assert_eq!(
        source_main(&source),
        m0,
        "nothing moves under the task's gate"
    );
    assert_eq!(
        started(&marks).len(),
        1,
        "no other judging of the source starts"
    );

    std::fs::write(marks.join("release"), "").expect("release");
    integrator.settle();
    let landed = control_records(&db, "task.landed");
    assert_eq!(landed.len(), 1, "{landed:?}");
    let landing = landed[0]["to"].as_str().expect("the landing").to_string();

    integrator
        .pass_on(&db.store)
        .expect("the pass after the gate");
    assert_eq!(
        integrator
            .running
            .borrow()
            .iter()
            .map(|gate| gate.owner)
            .collect::<Vec<_>>(),
        vec![Some(request)],
        "the owner's judging takes the source, not the second task's"
    );
    integrator.settle();
    let judged = started(&marks);
    assert_eq!(judged.len(), 2, "{judged:?}");
    let moved = control_records(&db, "source.moved");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["request"], json!(request));
    assert_eq!(moved[0]["from"], landing.as_str());
    assert_eq!(moved[0]["verdict"], "accepted");
    let now = source_main(&source);
    assert_eq!(moved[0]["to"], now.as_str());
    assert_eq!(judged[1], now, "the owner's gate judged what landed");
    assert_eq!(
        git(&["--git-dir", &source, "rev-parse", &format!("{now}^1")]).expect("first parent"),
        landing,
        "the owner's landing merge is on the task's landing"
    );
    assert_eq!(
        git(&["--git-dir", &source, "rev-parse", &format!("{now}^2")]).expect("second parent"),
        commit
    );
    let given = std::fs::read_to_string(marks.join("scenarios-given")).expect("given");
    let given: Vec<&str> = given.lines().collect();
    assert_eq!(given.len(), 2, "{given:?}");
    assert_ne!(
        given[1],
        scenarios.to_string_lossy(),
        "the candidate's own scenarios"
    );
    assert!(Path::new(given[1]).ends_with("scenarios"), "{given:?}");
    assert_eq!(
        std::fs::read_to_string(marks.join("protected-seen")).expect("protected"),
        "",
        "the copy's protected list is empty"
    );
    assert!(
        !Path::new(given[1]).exists(),
        "the candidate's tree is removed after its gate"
    );
    assert!(verdicts(&db, second).is_empty(), "the second task waited");

    integrator.pass_on(&db.store).expect("the next pass");
    integrator.settle();
    assert_eq!(verdicts(&db, second), vec!["accepted"]);
    assert_eq!(started(&marks)[2].len(), 40);
    assert_eq!(
        git(&[
            "--git-dir",
            &source,
            "rev-parse",
            &format!("{}^1", source_main(&source))
        ])
        .expect("first parent"),
        now,
        "the second task landed on the owner's landing"
    );
    drop(release);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// A gated owner's push its gate rejects records `source.push_refused`
/// with the verdict and the gate's log, and moves nothing; no mission
/// counts it.
#[test]
fn a_rejected_owner_judging_is_refused_and_moves_nothing() {
    let gate = "echo '| == regression: 046.sh must still pass'\n\
                    printf '{\"verdict\":\"rejected\",\"reason\":\"regression: 046.sh fails on the candidate\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
                    echo '{\"verdict\":\"rejected\",\"reason\":\"regression: 046.sh fails on the candidate\"}'\n";
    let (base, source, scenarios, integrator) = world("owner-rejected", gate);
    let db = Db::new("owner_rejected");
    let m0 = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    let before = gated(&db, mission);
    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("BROKEN.md", "x\n")],
        "owner: breaks 046",
    );
    let request = gated_request(&db, &source, &commit, "a change");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    assert_eq!(source_main(&source), m0, "nothing moves");
    assert!(control_records(&db, "source.moved").is_empty());
    let refused = control_records(&db, "source.push_refused");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0]["request"], json!(request));
    assert_eq!(refused[0]["verdict"], "rejected");
    assert_eq!(
        refused[0]["reason"],
        "the owner's gate rejected it: regression: 046.sh fails on the candidate"
    );
    assert!(
        refused[0]["log"]
            .as_str()
            .is_some_and(|log| log.contains("046.sh must still pass")),
        "{}",
        refused[0]
    );
    let after = gated(&db, mission);
    assert_eq!(
        after.gate_rejections, before.gate_rejections,
        "no mission counts it"
    );
    assert_eq!(after.gate_aborts, before.gate_aborts);
    integrator.pass_on(&db.store).expect("the next pass");
    integrator.settle();
    assert_eq!(
        control_records(&db, "source.push_refused").len(),
        1,
        "judged once"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// An owner judging whose gate still ends `main_red` (a gate.sh without
/// SPAWN_GATE_RED_UNSOLVED, which otherwise judges the red check as
/// unsolved on the base and goes on) gets no verdict, not a refusal
/// (the lead, 2026-11-10): the push neither broke nor fixed main,
/// nothing is recorded, nothing moves, and it is judged again after its
/// backoff.
#[test]
fn an_owner_judging_on_a_red_main_it_does_not_fix_is_no_verdict() {
    let gate = "printf '%s\\n' '{\"host_fault\":true,\"scenario\":\"check-tests.sh\",\"on\":\"main\",\"runs\":\"1, then 1 on the candidate; 1, then 1 on the base\",\"reason\":\"r\",\"failures\":[{\"test\":\"api::slice_11\",\"message\":\"expected 23 to be 21\"}]}' >\"$SPAWN_GATE_VERDICT\"\nexit 6\n";
    let (base, source, scenarios, mut integrator) = world("owner-red", gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_mins(10),
        cap: 3,
    };
    let db = Db::new("owner_red");
    let m0 = source_main(&source);
    db.gated_mission(&source, &scenarios);
    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("B.md", "b\n")],
        "owner: unrelated to the red check",
    );
    let request = gated_request(&db, &source, &commit, "b");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    assert_eq!(source_main(&source), m0, "nothing moves");
    assert!(
        control_records(&db, "source.push_refused").is_empty(),
        "not refused for a red it did not cause"
    );
    assert!(control_records(&db, "source.moved").is_empty());
    let backoff = Arc::clone(&integrator.owner.borrow().backoff);
    assert_eq!(
        backoff
            .lock()
            .expect("backoff")
            .get(&request)
            .map(|(faults, _)| *faults),
        Some(1),
        "judged again after its backoff"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Every judging keeps its gates' files (P26, lead-mac): an accepted
/// task's gate leaves its stdout (the per-scenario lines at its end),
/// stderr and verdict under `<root>/gate-logs/<judging>/<gate>/` after
/// the judging's own directory is removed, and the task's `task.gated`
/// and `task.landed` name that directory (`gate_logs`). The environment
/// file is not kept.
#[test]
fn an_accepted_task_judging_keeps_its_gates_files_and_names_them() {
    let gate = "if [ \"$1\" = --base-only ]; then exit 0; fi\n\
                    echo '| stage gate ok 6095ms regression: check-web.sh must still pass'\n\
                    echo 'a note on stderr' >&2\n\
                    printf '{\"verdict\":\"accepted\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
                    echo '{\"verdict\":\"accepted\"}'\n";
    let (base, source, scenarios, integrator) = world("kept-gates", gate);
    let db = Db::new("kept_gates");
    let mission = db.gated_mission(&source, &scenarios);
    db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    integrator.pass_on(&db.store).expect("the judging's pass");
    integrator.settle();
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
    let gated = control_records(&db, "task.gated");
    assert_eq!(gated.len(), 1, "{gated:?}");
    let judging = gated[0]["judging"].as_str().expect("names its judging");
    let kept = base
        .join("host")
        .join(super::retained::GATE_LOGS)
        .join(judging);
    assert_eq!(gated[0]["gate_logs"], json!(kept.to_string_lossy()));
    let landed = control_records(&db, "task.landed");
    assert_eq!(landed[0]["gate_logs"], gated[0]["gate_logs"]);
    let dirs: Vec<PathBuf> = std::fs::read_dir(&kept)
        .expect("the kept judging")
        .map(|entry| entry.expect("entry").path())
        .collect();
    assert!(!dirs.is_empty(), "at least the candidate's gate");
    let stdout: String = dirs
        .iter()
        .map(|gate| std::fs::read_to_string(gate.join("stdout")).unwrap_or_default())
        .collect();
    assert!(
        stdout.contains("| stage gate ok 6095ms regression: check-web.sh must still pass"),
        "{stdout}"
    );
    assert!(dirs.iter().any(|gate| {
        std::fs::read_to_string(gate.join("stderr"))
            .is_ok_and(|text| text.contains("a note on stderr"))
    }));
    assert!(
        dirs.iter().all(|gate| !gate.join("env").exists()),
        "no env file kept"
    );
    assert!(
        !base
            .join("host")
            .join(super::judging::DIRECTORY)
            .join(judging)
            .exists(),
        "the judging's own directory is removed"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// An owner judging with no verdict keeps what its gate printed (P26):
/// a host fault and an abort each leave a readable log under the
/// integrator's root, stdout then stderr marked `! `, and a
/// `source.push_no_verdict` naming the request, the judging and the
/// log's path. Still no outcome: nothing moves, nothing is refused, and
/// the request is judged again after its backoff.
#[test]
fn an_owner_no_verdict_keeps_its_gates_log_and_names_it() {
    let gate = "echo '| == base: the line the owner needs'\n\
                    echo 'a harness complaint' >&2\n\
                    printf '%s\\n' '{\"host_fault\":true,\"scenario\":\"check-tests.sh\",\"on\":\"base\",\"runs\":\"1, then 1 on the base\",\"reason\":\"r\"}' >\"$SPAWN_GATE_VERDICT\"\n\
                    exit 6\n";
    let (base, source, scenarios, mut integrator) = world("owner-kept", gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_mins(10),
        cap: 3,
    };
    let db = Db::new("owner_kept");
    let m0 = source_main(&source);
    db.gated_mission(&source, &scenarios);
    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("K.md", "k\n")],
        "owner: k",
    );
    let request = gated_request(&db, &source, &commit, "k");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    assert_eq!(source_main(&source), m0, "nothing moves");
    assert!(control_records(&db, "source.moved").is_empty());
    assert!(control_records(&db, "source.push_refused").is_empty());
    let kept = control_records(&db, "source.push_no_verdict");
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0]["request"], json!(request));
    let judging = kept[0]["judging"].as_str().expect("names its judging");
    assert!(
        kept[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("the host's fault on check-tests.sh")),
        "{kept:?}"
    );
    let path = std::path::PathBuf::from(kept[0]["log"].as_str().expect("names its log"));
    assert_eq!(
        path.parent(),
        Some(base.join("host").join(super::retained::DIRECTORY).as_path())
    );
    assert!(
        path.to_string_lossy()
            .ends_with(&format!("-request-{request}-{judging}.log")),
        "{}",
        path.display()
    );
    let log = std::fs::read_to_string(&path).expect("the kept log reads");
    assert!(log.contains("| == base: the line the owner needs"), "{log}");
    assert!(log.contains("! a harness complaint"), "{log}");
    let gates = base
        .join("host")
        .join(super::retained::GATE_LOGS)
        .join(judging);
    assert_eq!(kept[0]["gate_logs"], json!(gates.to_string_lossy()));
    assert!(
        !base
            .join("host")
            .join(super::judging::DIRECTORY)
            .join(judging)
            .exists(),
        "the judging's own directory is still removed"
    );
    let backoff = Arc::clone(&integrator.owner.borrow().backoff);
    assert_eq!(
        backoff
            .lock()
            .expect("backoff")
            .get(&request)
            .map(|(faults, _)| *faults),
        Some(1),
        "judged again after its backoff"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// An owner judging whose gate aborts (a non-zero exit with no verdict
/// to believe: a harness error) records nothing and is judged again
/// after its backoff, as the host's other failures are (the lead,
/// 2026-11-10, rev-b's N1): never `source.push_refused`.
#[test]
fn an_aborted_owner_gate_is_no_verdict() {
    let (base, source, scenarios, integrator) =
        world("owner-aborted", "echo 'half a run'\nexit 1\n");
    let db = Db::new("owner_aborted");
    let m0 = source_main(&source);
    db.gated_mission(&source, &scenarios);
    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("C.md", "c\n")],
        "owner: c",
    );
    let request = gated_request(&db, &source, &commit, "c");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    assert_eq!(source_main(&source), m0, "nothing moves");
    assert!(
        control_records(&db, "source.push_refused").is_empty(),
        "an abort is no refusal"
    );
    assert!(control_records(&db, "source.moved").is_empty());
    let kept = control_records(&db, "source.push_no_verdict");
    assert_eq!(kept.len(), 1, "{kept:?}");
    let log = std::fs::read_to_string(kept[0]["log"].as_str().expect("names its log"))
        .expect("the abort's log reads");
    assert!(log.contains("half a run"), "{log}");
    let backoff = Arc::clone(&integrator.owner.borrow().backoff);
    assert_eq!(
        backoff
            .lock()
            .expect("backoff")
            .get(&request)
            .map(|(faults, _)| *faults),
        Some(1),
        "judged again after its backoff"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// An owner's gate runs with SPAWN_GATE_RED_UNSOLVED=1 (the lead,
/// 2026-11-10, rev-b's N2): a check main is red on is unsolved on the
/// base, so the push is judged on the rest (test-gate.sh 5a'), never
/// held or refused for a red it did not cause. A task's gate runs
/// without it.
#[test]
fn an_owner_gate_judges_a_red_check_as_unsolved_on_the_base_and_a_task_gate_does_not() {
    let marks = fresh_marks("spawn-owner-red-unsolved");
    let seen = marks.to_string_lossy().to_string();
    let gate = format!(
        "echo \"${{SPAWN_GATE_RED_UNSOLVED:-unset}}\" >>'{seen}/red-unsolved'\n\
             printf '%s\\n' '{{\"verdict\":\"accepted\"}}' >\"$SPAWN_GATE_VERDICT\"\n"
    );
    let (base, source, scenarios, integrator) = world("owner-red-unsolved", &gate);
    let db = Db::new("owner_red_unsolved");
    let mission = db.gated_mission(&source, &scenarios);
    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("D.md", "d\n")],
        "owner: d",
    );
    gated_request(&db, &source, &commit, "d");
    integrator.pass_on(&db.store).expect("the owner's judging");
    integrator.settle();
    assert_eq!(control_records(&db, "source.moved").len(), 1);
    let task = db.reported_task(mission, "w");
    let agent = agent_of(&db, mission, task);
    push_work(&base, &source, &agent, "work.txt");
    integrator.pass_on(&db.store).expect("the task's judging");
    integrator.settle();
    assert_eq!(verdicts(&db, task), vec!["accepted"]);
    assert_eq!(
        std::fs::read_to_string(marks.join("red-unsolved")).expect("seen"),
        "1\nunset\n",
        "the owner's gate only"
    );
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// An owner's gate runs with SPAWN_GATE_OWNER_CANDIDATE=1 and a task's
/// without it (review-fix-gate.md): the owner's accepted landing names
/// the validators the gate judged it without, failing on the base too,
/// and those the owner's list kept from running, on its `source.moved`;
/// a task's gate that ends on a validator defect is a host fault with
/// its own evidence, `validator_defect`, naming them, never capped.
#[test]
fn an_owner_gate_is_told_it_is_the_owners_and_a_validator_defect_is_named() {
    let marks = fresh_marks("spawn-owner-candidate");
    let seen = marks.to_string_lossy().to_string();
    let gate = format!(
        "echo \"${{SPAWN_GATE_OWNER_CANDIDATE:-unset}}\" >>'{seen}/owner'\n\
             if [ \"${{SPAWN_GATE_OWNER_CANDIDATE-}}\" = 1 ]; then\n\
             printf '%s\\n' '{{\"v\":1,\"verdict\":\"accepted\",\"validator_defects\":[\"028.sh\",\"../x.sh\"],\"quarantined\":[\"031.sh\"]}}' >\"$SPAWN_GATE_VERDICT\"\n\
             exit 0\nfi\n\
             printf '%s\\n' '{{\"v\":1,\"host_fault\":true,\"scenario\":\"028.sh\",\"on\":\"candidate and base\",\"runs\":\"1, then 1 on the candidate; 1 on the base, run now\",\"reason\":\"validator defect: 028.sh fails on the base too\",\"evidence\":\"validator_defect\",\"validator_defects\":[\"028.sh\"]}}' >\"$SPAWN_GATE_VERDICT\"\n\
             exit 6\n"
    );
    let (base, source, scenarios, integrator) = world("owner-candidate", &gate);
    let db = Db::new("owner_candidate");
    let mission = db.gated_mission(&source, &scenarios);
    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("D.md", "d\n")],
        "owner: d",
    );
    gated_request(&db, &source, &commit, "d");
    integrator.pass_on(&db.store).expect("the owner's judging");
    integrator.settle();
    let moved = control_records(&db, "source.moved");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["verdict"], "accepted");
    assert_eq!(
        moved[0]["validator_defects"],
        json!(["028.sh"]),
        "{}",
        moved[0]
    );
    assert_eq!(moved[0]["quarantined"], json!(["031.sh"]), "{}", moved[0]);
    let task = db.reported_task(mission, "w");
    let agent = agent_of(&db, mission, task);
    push_work(&base, &source, &agent, "work.txt");
    integrator.pass_on(&db.store).expect("the task's judging");
    integrator.settle();
    assert_eq!(verdicts(&db, task), vec!["host_fault"]);
    let gated = control_records(&db, "task.gated");
    let fault = gated.last().expect("the task's record");
    assert_eq!(fault["evidence"], "validator_defect", "{fault}");
    assert_eq!(fault["validator_defects"], json!(["028.sh"]), "{fault}");
    assert_eq!(fault["fact"]["on"], "candidate and base", "{fault}");
    assert!(
        fault["reason"]
            .as_str()
            .is_some_and(|reason| reason.starts_with("validator defect: 028.sh fails")),
        "{fault}"
    );
    assert_eq!(
        std::fs::read_to_string(marks.join("owner")).expect("seen"),
        "1\nunset\n",
        "the owner's gate only"
    );
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// A gated owner's push whose candidate has no scenarios directory
/// where the mission's is cannot be judged by it: refused, verdict
/// `no_scenarios`, the owner's to fix; nothing moves.
#[test]
fn a_gated_owner_push_without_the_scenarios_directory_is_refused() {
    let (base, source, scenarios, integrator) = world("owner-no-scenarios", "exit 0\n");
    let db = Db::new("owner_no_scenarios");
    let m0 = source_main(&source);
    db.gated_mission(&source, &scenarios);
    let commit = owner_commit_files(&base, &source, &[("E.md", "e\n")], "owner: e");
    let request = gated_request(&db, &source, &commit, "e");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    assert_eq!(source_main(&source), m0);
    let refused = control_records(&db, "source.push_refused");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0]["request"], json!(request));
    assert_eq!(refused[0]["verdict"], "no_scenarios");
    let _ = std::fs::remove_dir_all(&base);
}

/// An owner judging's tree gets `web/node_modules` before its gate starts
/// (git holds none, and the gate binds that directory only when it is
/// there at its setup), so a check that builds the UI judges the owner's
/// push instead of failing on both sides as a host fault (lead-mac,
/// 2026-11-10). The installed controller's set when the candidate names
/// the dependencies it was installed from (web/pnpm-lock.yaml): copied,
/// and pnpm not run. Other dependencies (objective 131, K2): the
/// candidate's own set, installed by pnpm with exactly `INSTALL_ARGS` and
/// an environment of HOME, LC_ALL and PATH alone, judged and landed. A set
/// the host's store lacks: no gate, and the `NOT_IN_STORE` no-verdict.
/// The controller's own set is never written.
#[test]
fn an_owner_judgings_tree_gets_the_controllers_web_node_modules() {
    let (base, source, scenarios, mut integrator) = world("owner-modules", "");
    let seen = modules_seen_gate(&base);
    let pnpm = stub_pnpm(&base, &mut integrator);
    let modules = base.join("web/node_modules");
    std::fs::create_dir_all(modules.join(".pnpm")).expect("modules");
    std::fs::create_dir_all(modules.join("pkg")).expect("modules");
    std::fs::write(modules.join(".pnpm/lock.yaml"), "lock v1\n").expect("lock");
    std::fs::write(modules.join("pkg/index.js"), "x\n").expect("pkg");
    let db = Db::new("owner_modules");
    db.gated_mission(&source, &scenarios);
    let seen_lines = || {
        std::fs::read_to_string(&seen)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };

    let commit = owner_commit_files(
        &base,
        &source,
        &[
            ("scenarios/protected", ""),
            ("web/package.json", "{}\n"),
            ("web/pnpm-lock.yaml", "lock v1\n"),
        ],
        "owner: the ui",
    );
    gated_request(&db, &source, &commit, "the ui");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    assert_eq!(
        seen_lines(),
        vec!["copied"],
        "the gate found the copy at its start"
    );
    assert!(pnpm.runs().is_empty(), "the controller's set: pnpm not run");
    assert_eq!(
        control_records(&db, "source.moved").len(),
        1,
        "judged, and landed"
    );

    // Other dependencies than the installed ones: the candidate's own set,
    // installed.
    let commit = owner_commit_files(
        &base,
        &source,
        &[("web/pnpm-lock.yaml", "lock v2, other dependencies\n")],
        "owner: new dependencies",
    );
    gated_request(&db, &source, &commit, "new dependencies");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    assert_eq!(
        pnpm.runs(),
        vec![super::owner::INSTALL_ARGS.join(" ")],
        "pnpm ran once, with exactly the install's arguments"
    );
    assert_eq!(
        pnpm.names(),
        "HOME LC_ALL PATH",
        "pnpm's environment names HOME, LC_ALL and PATH alone"
    );
    assert_eq!(
        seen_lines(),
        vec!["copied", "installed"],
        "the gate found the installed set at its start"
    );
    assert_eq!(
        control_records(&db, "source.moved").len(),
        2,
        "judged, and landed"
    );

    // A set the host's store lacks: no gate, and the no-verdict says so.
    let commit = owner_commit_files(
        &base,
        &source,
        &[("web/pnpm-lock.yaml", "lock v3, made-up-absent\n")],
        "owner: absent dependencies",
    );
    let request = gated_request(&db, &source, &commit, "absent dependencies");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    assert_eq!(pnpm.runs().len(), 2, "pnpm ran for it");
    assert_eq!(seen_lines(), vec!["copied", "installed"], "no gate ran");
    let reasons: Vec<String> = no_verdict_records(&db, request)
        .into_iter()
        .map(|(reason, _)| reason)
        .collect();
    assert_eq!(reasons, vec![super::owner::NOT_IN_STORE.to_string()]);
    assert_eq!(control_records(&db, "source.moved").len(), 2);

    assert!(
        std::fs::symlink_metadata(modules.join("pkg/index.js")).is_ok()
            && std::fs::symlink_metadata(modules.join("pkg2")).is_err(),
        "the controller's own is untouched"
    );
    assert_eq!(
        std::fs::read_to_string(modules.join(".pnpm/lock.yaml")).expect("lock"),
        "lock v1\n"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A gate that notes, in the file it returns, what the tree's
/// `web/node_modules` held at its start: `copied` (the controller's `pkg`),
/// `installed` (the stub pnpm's `pkg2`) or `none`; and accepts.
fn modules_seen_gate(base: &Path) -> PathBuf {
    let seen = base.join("modules-seen");
    let gate = format!(
        "m=\"$4/../web/node_modules\"\n\
             if [ -d \"$m\" ] && [ ! -L \"$m\" ] && [ -f \"$m/pkg/index.js\" ]; then echo copied; \
             elif [ -d \"$m\" ] && [ ! -L \"$m\" ] && [ -f \"$m/pkg2/index.js\" ]; then \
             echo installed; else echo none; fi >>'{seen}'\n\
             printf '%s\\n' '{{\"verdict\":\"accepted\"}}' >\"$SPAWN_GATE_VERDICT\"\n",
        seen = seen.display()
    );
    std::fs::write(base.join("gate.sh"), gate).expect("gate");
    seen
}

/// A case of web/ configuration an owner judging refuses: the commit's
/// subject, its files, and what the no-verdict names.
type WebCase = (
    &'static str,
    &'static [(&'static str, &'static str)],
    &'static str,
);

/// An owner judging's candidate whose `web/` holds pnpm configuration that
/// could run code in pnpm or reach outside the tree (objective 131, rev-g's
/// F1 and F2): a web/.pnpmfile.cjs, a pnpm-workspace.yaml with modulesDir,
/// a package.json with a `file:` dependency. Each gets the `WEB_CONFIG`
/// no-verdict naming it; pnpm never starts and no gate runs.
#[test]
fn an_owner_judging_refuses_web_config_that_could_run_code() {
    let (base, source, scenarios, mut integrator) = world("owner-web-config", "");
    let ran = base.join("gate-ran");
    std::fs::write(
        base.join("gate.sh"),
        format!(
            "echo ran >>'{}'\n\
             printf '%s\\n' '{{\"verdict\":\"accepted\"}}' >\"$SPAWN_GATE_VERDICT\"\n",
            ran.display()
        ),
    )
    .expect("gate");
    let pnpm = stub_pnpm(&base, &mut integrator);
    let modules = base.join("web/node_modules");
    std::fs::create_dir_all(modules.join(".pnpm")).expect("modules");
    std::fs::write(modules.join(".pnpm/lock.yaml"), "lock v1\n").expect("lock");
    let db = Db::new("owner_web_config");
    db.gated_mission(&source, &scenarios);
    let cases: [WebCase; 3] = [
        (
            "owner: a pnpmfile",
            &[
                ("web/.pnpmfile.cjs", "module.exports = {};\n"),
                ("web/package.json", "{}\n"),
            ],
            "web has .pnpmfile.cjs",
        ),
        (
            "owner: a modules directory outside the tree",
            &[
                ("web/pnpm-workspace.yaml", "modulesDir: ../../outside\n"),
                ("web/package.json", "{}\n"),
            ],
            "web/pnpm-workspace.yaml sets modulesDir",
        ),
        (
            "owner: a dependency on a host directory, by file:",
            &[(
                "web/package.json",
                "{\"dependencies\": {\"x\": \"file:/etc\"}}\n",
            )],
            "web/package.json has a dependency that is not a registry range (x)",
        ),
    ];
    for (subject, files, found) in cases {
        let mut files = files.to_vec();
        files.push(("scenarios/protected", ""));
        files.push(("web/pnpm-lock.yaml", "lock v2\n"));
        let commit = owner_commit_files(&base, &source, &files, subject);
        let request = gated_request(&db, &source, &commit, subject);
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        let reasons: Vec<String> = no_verdict_records(&db, request)
            .into_iter()
            .map(|(reason, _)| reason)
            .collect();
        assert_eq!(
            reasons,
            vec![format!("{}: {found}", super::owner::WEB_CONFIG)],
            "{subject}"
        );
        super::owner::withdraw(&db.store, request).expect("withdrawn");
    }
    assert!(pnpm.runs().is_empty(), "pnpm never started");
    assert!(std::fs::symlink_metadata(&ran).is_err(), "no gate ran");
    assert!(std::fs::symlink_metadata(base.join("outside")).is_err());
    let _ = std::fs::remove_dir_all(&base);
}

/// A stub pnpm, first on the gate environment's PATH (HOME beside it): it
/// notes each run's arguments and the names (never the values) of the
/// variables it was given, fails as pnpm 12 does on a set the store lacks
/// (a lockfile naming made-up-absent: ERR_PNPM_META_FETCH_FAIL, then
/// ERR_PNPM_NO_OFFLINE_META), and otherwise installs `pkg2`, with the
/// lockfile as its `.pnpm/lock.yaml`.
fn stub_pnpm(base: &Path, integrator: &mut Integrator) -> StubPnpm {
    let bin = base.join("stub-bin");
    std::fs::create_dir_all(&bin).expect("stub bin");
    let runs = base.join("pnpm-runs");
    let names = base.join("pnpm-names");
    let install = bin.join("install.sh");
    // awk, not a shell, reads what pnpm was given: a shell exports PWD of
    // its own before its first line runs. gawk also adds AWKPATH and
    // AWKLIBPATH to ENVIRON; the stub deletes those two interpreter defaults.
    let pnpm = format!(
        "#!/usr/bin/awk -f\n\
         BEGIN {{\n\
         line = \"\"\n\
         for (i = 1; i < ARGC; i++) line = line (i > 1 ? \" \" : \"\") ARGV[i]\n\
         print line >> \"{runs}\"\n\
         sorted = \"sort | tr '\\\\n' ' ' >'{names}'\"\n\
         delete ENVIRON[\"AWKPATH\"]\n\
         delete ENVIRON[\"AWKLIBPATH\"]\n\
         for (name in ENVIRON) print name | sorted\n\
         close(sorted)\n\
         exit system(\"sh '{install}'\")\n\
         }}\n",
        runs = runs.display(),
        names = names.display(),
        install = install.display()
    );
    let script = "#!/bin/sh\n\
         if grep -q made-up-absent pnpm-lock.yaml; then\n\
         echo ' ERR_PNPM_META_FETCH_FAIL  GET http://127.0.0.1/made-up-absent: offline'\n\
         echo ' ERR_PNPM_NO_OFFLINE_META  Failed to resolve made-up-absent@1.0.0 in the store' >&2\n\
         exit 1\n\
         fi\n\
         mkdir -p node_modules/.pnpm node_modules/pkg2\n\
         cp pnpm-lock.yaml node_modules/.pnpm/lock.yaml\n\
         echo x >node_modules/pkg2/index.js\n";
    std::fs::write(&install, script).expect("the stub's install");
    let stub = bin.join("pnpm");
    std::fs::write(&stub, pnpm).expect("stub pnpm");
    std::fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("executable");
    let path = std::env::var("PATH").unwrap_or_default();
    let env = &mut integrator.judge.gate_env;
    env.retain(|(name, _)| name != "PATH" && name != "HOME");
    env.push(("PATH".to_string(), format!("{}:{path}", bin.display())));
    env.push(("HOME".to_string(), base.join("home").display().to_string()));
    StubPnpm { runs, names }
}

/// What the stub pnpm noted.
struct StubPnpm {
    runs: PathBuf,
    names: PathBuf,
}

impl StubPnpm {
    /// Each run's arguments, a line a run.
    fn runs(&self) -> Vec<String> {
        std::fs::read_to_string(&self.runs)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The last run's variable names, sorted, joined by spaces.
    fn names(&self) -> String {
        std::fs::read_to_string(&self.names)
            .unwrap_or_default()
            .trim()
            .to_string()
    }
}

/// An owner judging's description names the gate and the scenarios it
/// runs, as pinned, like a task's: deploy/controller.sh keeps the
/// controller either lies in while the judging lasts (rev-e C1 on commit
/// 7: a Supply push cleans the supply-pa controller a running Supply gate
/// reads its scenarios from).
#[test]
fn an_owner_judging_names_its_gate_and_scenarios() {
    let (base, source, scenarios, integrator) = world("owner-described", "");
    let seen = base.join("described.json");
    let gate = format!(
        "cp \"$(dirname \"$(dirname \"$SPAWN_GATE_VERDICT\")\")/judging.json\" '{seen}'\n\
             printf '%s\\n' '{{\"verdict\":\"accepted\"}}' >\"$SPAWN_GATE_VERDICT\"\n",
        seen = seen.display()
    );
    std::fs::write(base.join("gate.sh"), gate).expect("gate");
    let db = Db::new("owner_described");
    db.gated_mission(&source, &scenarios);
    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", "")],
        "owner: described",
    );
    gated_request(&db, &source, &commit, "described");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    let described: Value =
        serde_json::from_str(&std::fs::read_to_string(&seen).expect("judging.json")).expect("json");
    assert_eq!(described["kind"], json!("owner"));
    assert_eq!(
        described["gate"],
        json!(super::pinned(&integrator.judge.gate))
    );
    assert_eq!(described["scenarios"], json!(super::pinned(&scenarios)));
    let _ = std::fs::remove_dir_all(&base);
}

/// A gate that notes its run in `runs` beside it and ends with the
/// host's fault on check-tests.sh, the same at every run.
fn gate_with_one_host_fault() -> String {
    format!(
        "echo \"$3\" >>\"$(dirname \"$0\")/runs\"\n\
             printf '%s\\n' '{}' >\"$SPAWN_GATE_VERDICT\"\nexit 6\n",
        "{\"host_fault\":true,\"scenario\":\"check-tests.sh\",\"on\":\"base\",\"runs\":\"1, then 1 on the base\",\"reason\":\"r\"}"
    )
}

/// How many owner judgings of `request` in a row found no verdict, as
/// its backoff counts them; None without a backoff.
fn faults_of(integrator: &Integrator, request: i64) -> Option<u32> {
    let backoff = Arc::clone(&integrator.owner.borrow().backoff);
    let kept = backoff.lock().expect("backoff");
    kept.get(&request).map(|(faults, _)| *faults)
}

/// The `source.push_no_verdict` records of `request`, oldest first: each
/// (reason, kept log's path or empty).
fn no_verdict_records(db: &Db, request: i64) -> Vec<(String, String)> {
    control_records(db, "source.push_no_verdict")
        .iter()
        .filter(|record| record["request"] == json!(request))
        .map(|record| {
            let text = |name: &str| record[name].as_str().unwrap_or_default().to_string();
            (text("reason"), text("log"))
        })
        .collect()
}

/// The backoff of `request`'s owner judging is over now (its count
/// kept).
fn end_backoff(integrator: &Integrator, request: i64) {
    let backoff = Arc::clone(&integrator.owner.borrow().backoff);
    if let Some(kept) = backoff.lock().expect("backoff").get_mut(&request) {
        kept.1 = std::time::Instant::now();
    }
}

/// The refusal of a request that found no verdict twice for `why`.
fn twice(why: &str) -> String {
    format!(
        "no verdict twice for the same reason ({why}): classify it (candidate failure, test \
             defect or infrastructure) before requesting again"
    )
}

/// An owner judging that finds no verdict (the host's fault) records no
/// outcome: the request stays pending, is not judged again before its
/// backoff, and is judged again after it. No verdict again for another
/// reason (the fault on another scenario) is still no refusal: the
/// backoff grows.
#[test]
fn an_owner_judging_with_no_verdict_stays_pending_until_its_backoff() {
    let gate = "echo \"$3\" >>\"$(dirname \"$0\")/runs\"\n\
                    n=$(($(wc -l <\"$(dirname \"$0\")/runs\")))\n\
                    printf '{\"host_fault\":true,\"scenario\":\"check-%s.sh\",\"on\":\"base\",\"runs\":\"1, then 1 on the base\",\"reason\":\"r\"}\\n' \"$n\" >\"$SPAWN_GATE_VERDICT\"\n\
                    exit 6\n";
    let (base, source, scenarios, mut integrator) = world("owner-no-verdict", gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_mins(10),
        cap: 3,
    };
    let db = Db::new("owner_no_verdict");
    let m0 = source_main(&source);
    db.gated_mission(&source, &scenarios);
    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("A.md", "a\n")],
        "owner: a",
    );
    let request = gated_request(&db, &source, &commit, "a");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    assert_eq!(gate_runs(&base), 1);
    assert_eq!(source_main(&source), m0);
    assert!(control_records(&db, "source.moved").is_empty());
    assert!(control_records(&db, "source.push_refused").is_empty());
    assert_eq!(faults_of(&integrator, request), Some(1));
    integrator
        .pass_on(&db.store)
        .expect("a pass in its backoff");
    integrator.settle();
    assert_eq!(gate_runs(&base), 1, "not judged again inside its backoff");
    end_backoff(&integrator, request);
    integrator
        .pass_on(&db.store)
        .expect("a pass after its backoff");
    integrator.settle();
    assert_eq!(gate_runs(&base), 2, "judged again after it");
    assert_eq!(
        faults_of(&integrator, request),
        Some(2),
        "the backoff grows"
    );
    let found = no_verdict_records(&db, request);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[0].0.contains("check-1.sh"), "{found:?}");
    assert!(found[1].0.contains("check-2.sh"), "{found:?}");
    assert!(
        control_records(&db, "source.push_refused").is_empty(),
        "another reason is no refusal"
    );
    assert_eq!(source_main(&source), m0);
    let _ = std::fs::remove_dir_all(&base);
}

/// An owner judging that finds no verdict twice in a row for the same
/// reason (the same failure on the same scenario) is refused, not
/// judged a third time (request 131083, 2026-11-11: three judgings for
/// one deterministic fault, its source held behind it). The reason is
/// the one its `source.push_no_verdict` records carry, apart from the
/// kept log's path, which differs at every judging (P26). The refusal
/// names the reason and the second log, nothing moves, and the
/// source's next request is judged.
#[test]
fn an_owner_judging_with_no_verdict_twice_for_one_reason_is_refused() {
    let (base, source, scenarios, mut integrator) =
        world("owner-no-verdict-twice", &gate_with_one_host_fault());
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_mins(10),
        cap: 3,
    };
    let db = Db::new("owner_no_verdict_twice");
    let m0 = source_main(&source);
    db.gated_mission(&source, &scenarios);
    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("A.md", "a\n")],
        "owner: a",
    );
    let request = gated_request(&db, &source, &commit, "a");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    assert_eq!(gate_runs(&base), 1);
    assert!(control_records(&db, "source.push_refused").is_empty());
    assert_eq!(faults_of(&integrator, request), Some(1));
    end_backoff(&integrator, request);
    integrator
        .pass_on(&db.store)
        .expect("a pass after its backoff");
    integrator.settle();
    assert_eq!(gate_runs(&base), 2, "judged a second time");
    let found = no_verdict_records(&db, request);
    assert_eq!(found.len(), 2, "one per judging: {found:?}");
    let why = found[0].0.clone();
    assert!(why.contains("check-tests.sh"), "{why}");
    assert_eq!(found[1].0, why, "the same reason");
    assert!(
        !found[0].1.is_empty() && !found[1].1.is_empty() && found[0].1 != found[1].1,
        "two kept logs, two paths: {found:?}"
    );
    let refused = control_records(&db, "source.push_refused");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0]["request"], json!(request));
    assert_eq!(refused[0]["verdict"], "no_verdict");
    assert_eq!(refused[0]["reason"], twice(&why));
    assert_eq!(refused[0]["kept_log"], found[1].1.as_str());
    assert_eq!(source_main(&source), m0, "nothing moves");
    assert!(control_records(&db, "source.moved").is_empty());
    assert_eq!(faults_of(&integrator, request), None, "no backoff left");
    assert_eq!(
        db.store.source_pushes_pending().expect("pending"),
        json!([])
    );
    integrator.pass_on(&db.store).expect("the next pass");
    integrator.settle();
    assert_eq!(gate_runs(&base), 2, "never a third time");
    let next = gated_request(&db, &source, &commit, "a, again");
    integrator.pass_on(&db.store).expect("the next request");
    integrator.settle();
    assert_eq!(gate_runs(&base), 3, "the source's next request is judged");
    assert_eq!(
        faults_of(&integrator, next),
        Some(1),
        "counted from its own first: another request's records are not its"
    );
    assert_eq!(control_records(&db, "source.push_refused").len(), 1);
    let _ = std::fs::remove_dir_all(&base);
}

/// The rule reads the store's records, not the integrator's memory: an
/// integrator that starts between the two judgings (a deploy) refuses
/// the request at its own first judging of it, the request's second.
#[test]
fn no_verdict_twice_for_one_reason_is_refused_across_an_integrator_restart() {
    let (base, source, scenarios, mut first) =
        world("owner-no-verdict-restart", &gate_with_one_host_fault());
    let host = || super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_mins(10),
        cap: 3,
    };
    first.judge.host = host();
    let db = Db::new("owner_no_verdict_restart");
    let m0 = source_main(&source);
    db.gated_mission(&source, &scenarios);
    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("A.md", "a\n")],
        "owner: a",
    );
    let request = gated_request(&db, &source, &commit, "a");
    first.pass_on(&db.store).expect("a pass");
    first.settle();
    assert_eq!(gate_runs(&base), 1);
    assert_eq!(faults_of(&first, request), Some(1));
    let gate = first.judge.gate.clone();
    drop(first);
    let mut second = Integrator::new(
        base.join("host"),
        gate,
        test_gate_env(),
        room_to_spare(),
        super::DEFAULT_GATES,
    );
    second.judge.host = host();
    assert_eq!(faults_of(&second, request), None, "it remembers nothing");
    second
        .pass_on(&db.store)
        .expect("the next integrator's pass");
    second.settle();
    assert_eq!(gate_runs(&base), 2, "judged a second time, at once");
    let found = no_verdict_records(&db, request);
    assert_eq!(found.len(), 2, "{found:?}");
    let refused = control_records(&db, "source.push_refused");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0]["request"], json!(request));
    assert_eq!(refused[0]["reason"], twice(&found[0].0));
    assert_eq!(source_main(&source), m0);
    second.pass_on(&db.store).expect("a pass after it");
    second.settle();
    assert_eq!(gate_runs(&base), 2, "never a third time");
    let _ = std::fs::remove_dir_all(&base);
}

/// No room is ordinary waiting, not a fault to classify: an owner
/// judging that finds no room twice in a row stays pending and is
/// judged again after its backoff, as before, although each records
/// the same reason. Nor is it "the same reason" for a fault: the rule
/// compares a judging with the request's judging just before it, so a
/// fault, no room, then that fault again is judged once more, and the
/// fault twice in a row after it is refused.
#[test]
fn an_owner_judging_without_room_twice_stays_pending_and_is_judged_again() {
    let gate = "echo ran >>\"$(dirname \"$0\")/runs\"\n\
                    case $(($(wc -l <\"$(dirname \"$0\")/runs\"))) in\n\
                    3|5|6) printf '%s\\n' '{\"host_fault\":true,\"scenario\":\"check-tests.sh\",\"on\":\"base\",\"runs\":\"1, then 1 on the base\",\"reason\":\"r\"}' >\"$SPAWN_GATE_VERDICT\"\n\
                    exit 6;;\n\
                    esac\n\
                    printf '%s\\n' '{\"no_room\":true,\"free_kb\":2048,\"min_gb\":10,\"tmpdir\":\"/data/tmp\"}' >\"$SPAWN_GATE_VERDICT\"\n\
                    exit 5\n";
    let (base, source, scenarios, mut integrator) = world("owner-no-room-twice", gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_mins(10),
        cap: 3,
    };
    let db = Db::new("owner_no_room_twice");
    let m0 = source_main(&source);
    db.gated_mission(&source, &scenarios);
    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("A.md", "a\n")],
        "owner: a",
    );
    let request = gated_request(&db, &source, &commit, "a");
    let judged_again = |runs: usize| {
        end_backoff(&integrator, request);
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        assert_eq!(gate_runs(&base), runs, "judged again");
    };
    let pending = |faults: u32| {
        assert!(
            control_records(&db, "source.push_refused").is_empty(),
            "not refused"
        );
        let pending = db.store.source_pushes_pending().expect("pending");
        assert_eq!(pending[0]["request"], json!(request), "{pending}");
        assert_eq!(faults_of(&integrator, request), Some(faults));
    };
    judged_again(1);
    pending(1);
    judged_again(2);
    pending(2);
    let found = no_verdict_records(&db, request);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[0].0.starts_with("no room to judge"), "{found:?}");
    assert_eq!(found[0].0, found[1].0, "no room twice, the same reason");
    integrator
        .pass_on(&db.store)
        .expect("a pass in its backoff");
    integrator.settle();
    assert_eq!(gate_runs(&base), 2, "not judged inside its backoff");
    judged_again(3); // the fault
    pending(3);
    judged_again(4); // no room
    pending(4);
    judged_again(5); // the fault again, after no room: once more
    pending(5);
    judged_again(6); // the fault twice in a row
    let found = no_verdict_records(&db, request);
    let refused = control_records(&db, "source.push_refused");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0]["reason"], twice(&found[5].0));
    assert!(found[5].0.contains("check-tests.sh"), "{found:?}");
    assert_eq!(source_main(&source), m0);
    let _ = std::fs::remove_dir_all(&base);
}

/// A gate that could not start, a gate another integrator reaped and a
/// wait for room are no fault of the request, however often in a row:
/// none may be "the same reason twice". A reaped gate records nothing,
/// and the same text twice at a judging's end is only backed off: the
/// request stays pending.
#[test]
fn a_gate_that_never_started_or_was_reaped_twice_stays_pending() {
    use crate::gate::{GateFailure, Shortfall};
    let no_room = GateFailure::NoRoom {
        short: Shortfall::Disk {
            free_kb: 1,
            min_gb: 10,
            tmpdir: "/t".to_string(),
        },
        log: String::new(),
    };
    assert!(!super::owner::repeats(&no_room));
    assert!(!super::owner::repeats(&GateFailure::Failed(
        "x".to_string()
    )));
    assert!(!super::owner::repeats(&GateFailure::Reaped));
    assert!(!super::owner::repeats(&GateFailure::Parameter(
        "gate base revision is empty or whitespace".to_string(),
    )));
    assert!(super::owner::repeats(&GateFailure::Timeout));
    assert!(super::owner::repeats(&GateFailure::Aborted {
        reason: "r".to_string(),
        log: String::new(),
    }));

    let (base, source, scenarios, integrator) = world("owner-reaped", "exit 0\n");
    let db = Db::new("owner_reaped");
    db.gated_mission(&source, &scenarios);
    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("A.md", "a\n")],
        "owner: a",
    );
    let request = gated_request(&db, &source, &commit, "a");
    // Reaped, twice: what the judging's thread ends with.
    let backoff = Arc::clone(&integrator.owner.borrow().backoff);
    for _ in 0..2 {
        super::owner::judged(
            &db.store,
            request,
            "the request",
            super::owner::Judged::NoVerdict(
                "the integrator that started the gate reaped it".to_string(),
            ),
            &backoff,
            std::time::Duration::from_mins(10),
        );
    }
    assert_eq!(faults_of(&integrator, request), Some(2), "backed off");
    assert!(
        no_verdict_records(&db, request).is_empty(),
        "nothing recorded"
    );
    assert!(control_records(&db, "source.push_refused").is_empty());
    let pending = db.store.source_pushes_pending().expect("pending");
    assert_eq!(pending[0]["request"], json!(request), "still pending");
    let _ = std::fs::remove_dir_all(&base);
}

/// A request whose commit main already holds is not withdrawn: its
/// landing went out without its record (the integrator stopped between
/// the two), and a refusal would leave main holding a commit its request
/// calls refused. The withdrawal is an error, nothing is recorded, and
/// the next pass records the landing.
#[test]
fn a_request_main_already_holds_is_not_withdrawn() {
    let (base, source, scenarios, integrator) = world("owner-withdraw-landed", "exit 0\n");
    let db = Db::new("owner_withdraw_landed");
    let m0 = source_main(&source);
    db.gated_mission(&source, &scenarios);
    let commit = owner_commit(&base, &source, "L.md", "l\n", "owner: l");
    let request = plain_request(&db, &source, &commit);
    // The landing's push, with no record of it.
    git(&[
        "--git-dir",
        &source,
        "update-ref",
        "refs/heads/main",
        &commit,
        &m0,
    ])
    .expect("main moves");
    let refused = super::owner::withdraw(&db.store, request).expect_err("main holds it");
    assert_eq!(
        refused,
        format!(
            "the owner landing request {request}: main holds it already: its landing is \
                 recorded at the next pass; nothing is withdrawn"
        )
    );
    assert!(control_records(&db, "source.push_refused").is_empty());
    assert_eq!(
        db.store.source_push_outcome(request).expect("outcome"),
        None,
        "still pending"
    );
    integrator.pass_on(&db.store).expect("the next pass");
    let moved = control_records(&db, "source.moved");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["request"], json!(request));
    assert_eq!(moved[0]["to"], commit.as_str());
    assert_eq!(source_main(&source), commit);
    assert!(control_records(&db, "source.push_refused").is_empty());
    let _ = std::fs::remove_dir_all(&base);
}

/// A withdrawal and the two waits of a task do not touch each other
/// (gate v2 step 5b, P22). While the nightly is red on the source its
/// task is held and the owner's gated request is still judged; that
/// request withdrawn mid-judging has its gate stopped, lands nothing,
/// and leaves the task held as it was, never judged. It is no landing
/// of the owner's either (`source.moved` is, P22's fact), so no task
/// waits for a deploy because of it. A green nightly then releases the
/// task, which is judged and lands.
#[test]
fn a_withdrawn_owner_request_leaves_a_task_held_by_the_nightly_as_it_was() {
    let marks = fresh_marks("spawn-withdrawn-nightly");
    let release = Release(marks.clone());
    let gate = gate_holding_then(&marks, "", "{\"verdict\":\"accepted\"}", 0);
    let (base, source, scenarios, integrator) = world("withdrawn-nightly", &gate);
    let db = Db::new("withdrawn_nightly");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = agent_of(&db, mission, task);
    push_work(&base, &source, &agent, "work.txt");
    let facts = integrator.judge.root.join("nightly");
    std::fs::create_dir_all(&facts).expect("facts");
    let m0 = source_main(&source);
    let fact = |red_since: Option<&str>| {
        let state = if red_since.is_some() { "red" } else { "green" };
        std::fs::write(
            facts.join("source.json"),
            json!({"v": 1, "main": m0, "at": 1, "state": state, "failed": [],
                   "last_green": null, "red_since": red_since})
            .to_string(),
        )
        .expect("the fact");
    };
    fact(Some(&m0));
    let held = |what: &str| {
        assert!(
            verdicts(&db, task).is_empty(),
            "{what}: the task is not judged"
        );
        assert!(
            integrator
                .waiting
                .borrow()
                .contains(&(task, super::Wait::NightlyRed)),
            "{what}: held by the nightly's red"
        );
        assert!(
            !integrator
                .waiting
                .borrow()
                .contains(&(task, super::Wait::Controller)),
            "{what}: not waiting for a deploy"
        );
    };
    let _ = std::fs::remove_file(marks.join("release"));
    integrator.pass_on(&db.store).expect("a pass while red");
    held("before the request");
    assert!(started(&marks).is_empty());

    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("A.md", "a\n")],
        "owner: a",
    );
    let request = gated_request(&db, &source, &commit, "a");
    integrator.pass_on(&db.store).expect("the owner's pass");
    assert_eq!(wait_started(&marks, 1).len(), 1, "the owner's gate runs");
    held("while the owner's gate runs");
    super::owner::withdraw(&db.store, request).expect("withdrawn while judged");
    // A gate that is not stopped is released after 300 s (a guard
    // against a hang only): the test then fails on `release`.
    let watchdog = marks.join("release");
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_mins(5));
        let _ = std::fs::write(watchdog, "");
    });
    integrator
        .pass_on(&db.store)
        .expect("the pass that stops it");
    integrator.settle();
    assert!(!marks.join("release").exists(), "never released");
    integrator.pass_on(&db.store).expect("a pass after it");
    integrator.pass_on(&db.store).expect("another");
    held("after the withdrawal");
    assert_eq!(started(&marks).len(), 1, "no gate but the owner's ran");
    assert_eq!(source_main(&source), m0, "nothing moved");
    assert!(control_records(&db, "source.moved").is_empty());
    assert_eq!(
        db.store.source_owner_landed(&source).expect("landed"),
        None,
        "a withdrawal is no landing of the owner's"
    );
    assert_withdrawn_only(&db, request);

    fact(None);
    std::fs::write(marks.join("release"), "").expect("release");
    integrator
        .pass_on(&db.store)
        .expect("the pass after the green night");
    integrator.settle();
    assert_eq!(verdicts(&db, task), vec!["accepted"]);
    integrator.pass_on(&db.store).expect("the landing's pass");
    assert_eq!(control_records(&db, "task.landed").len(), 1);
    assert_withdrawn_only(&db, request);
    drop(release);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// A request as `spawn mission push` records it, not gated.
fn plain_request(db: &Db, source: &str, commit: &str) -> i64 {
    db.store
        .source_push_request(
            source,
            commit,
            &super::owner::owner_ref(commit),
            &spawn_postgres::PushRequest::default(),
        )
        .expect("the owner's request")
}

/// The one refusal recorded is the withdrawal of `request`.
fn assert_withdrawn_only(db: &Db, request: i64) {
    let refused = control_records(db, "source.push_refused");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0]["request"], json!(request));
    assert_eq!(refused[0]["reason"], super::owner::WITHDRAWN);
}

/// `spawn mission push --withdraw` on a request that waits: it ends
/// refused, "withdrawn by the owner", as `spawn mission pushes` then
/// says, and no pass applies it, not even one that read it pending
/// before the withdrawal. A request that ended (withdrawn, or applied)
/// or that does not exist is an error, and nothing changes.
#[test]
fn a_waiting_owner_request_is_withdrawn_and_an_ended_one_is_not() {
    let (base, source, scenarios, integrator) = world("owner-withdrawn", "exit 0\n");
    let db = Db::new("owner_withdrawn");
    let m0 = source_main(&source);
    db.gated_mission(&source, &scenarios);
    let commit = owner_commit(&base, &source, "W.md", "w\n", "owner: w");
    let request = plain_request(&db, &source, &commit);
    let outcome = super::owner::withdraw(&db.store, request).expect("withdrawn");
    assert_eq!(outcome["type"], "source.push_refused", "{outcome}");
    assert_eq!(outcome["request"], json!(request));
    assert_eq!(outcome["reason"], "withdrawn by the owner");
    // Step 0 read it pending, then it was withdrawn: not applied.
    let read_before = super::owner::Request {
        id: request,
        source: source.clone(),
        commit: commit.clone(),
        gated: false,
        urgent: false,
        reason: None,
    };
    assert_eq!(
        super::owner::apply(&db.store, &read_before).expect("applied"),
        super::owner::Outcome::Refused("withdrawn by the owner".to_string())
    );
    assert_eq!(source_main(&source), m0, "nothing moves");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    assert_eq!(source_main(&source), m0, "nothing moves");
    assert!(control_records(&db, "source.moved").is_empty());
    assert_withdrawn_only(&db, request);
    let listed = db.store.source_pushes(&source, None, 20).expect("pushes");
    assert_eq!(listed[0]["request"], json!(request), "{listed}");
    assert_eq!(listed[0]["state"], "refused", "{listed}");
    assert_eq!(listed[0]["outcome"]["reason"], "withdrawn by the owner");

    assert_eq!(
        super::owner::withdraw(&db.store, request).expect_err("ended"),
        format!(
            "the owner landing request {request} ended already (refused: withdrawn by the \
                 owner): nothing is withdrawn"
        )
    );
    assert_eq!(
        super::owner::withdraw(&db.store, request + 1000).expect_err("unknown"),
        format!(
            "no owner landing request {}: nothing is withdrawn",
            request + 1000
        )
    );
    let applied = plain_request(&db, &source, &commit);
    integrator
        .pass_on(&db.store)
        .expect("the pass that applies");
    assert_eq!(source_main(&source), commit, "the next request lands");
    assert_eq!(
        super::owner::withdraw(&db.store, applied).expect_err("applied"),
        format!(
            "the owner landing request {applied} ended already (applied): nothing is withdrawn"
        )
    );
    assert_eq!(source_main(&source), commit, "and stays landed");
    assert_eq!(control_records(&db, "source.moved").len(), 1);
    assert_withdrawn_only(&db, request);
    let _ = std::fs::remove_dir_all(&base);
}

/// A request withdrawn in the backoff of its owner judging is refused
/// and never judged again; the source's next request is judged at
/// once, not behind that backoff.
#[test]
fn an_owner_request_withdrawn_in_its_backoff_is_not_judged_again() {
    let (base, source, scenarios, mut integrator) =
        world("owner-withdrawn-backoff", &gate_with_one_host_fault());
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_mins(10),
        cap: 3,
    };
    let db = Db::new("owner_withdrawn_backoff");
    let m0 = source_main(&source);
    db.gated_mission(&source, &scenarios);
    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("A.md", "a\n")],
        "owner: a",
    );
    let request = gated_request(&db, &source, &commit, "a");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    assert_eq!(gate_runs(&base), 1);
    assert_eq!(faults_of(&integrator, request), Some(1), "in its backoff");
    super::owner::withdraw(&db.store, request).expect("withdrawn");
    assert_withdrawn_only(&db, request);
    end_backoff(&integrator, request);
    integrator.pass_on(&db.store).expect("a pass after it");
    integrator.settle();
    assert_eq!(gate_runs(&base), 1, "not judged again");
    assert_eq!(faults_of(&integrator, request), None, "its backoff is gone");
    assert_eq!(source_main(&source), m0);

    let next = gated_request(&db, &source, &commit, "a, again");
    integrator.pass_on(&db.store).expect("the next request");
    integrator.settle();
    assert_eq!(gate_runs(&base), 2, "the source's next request is judged");
    assert_eq!(faults_of(&integrator, next), Some(1));
    assert_withdrawn_only(&db, request);
    let _ = std::fs::remove_dir_all(&base);
}

/// A request withdrawn while its owner judging runs never lands: the
/// gate, left to end `accepted` (no pass in between stops it), finds
/// the request ended under the landing hold and moves nothing. The
/// source's next request is judged and lands.
#[test]
fn a_withdrawn_owner_request_whose_gate_then_accepts_does_not_land() {
    let marks = fresh_marks("spawn-owner-withdrawn-accepted");
    let release = Release(marks.clone());
    let gate = gate_holding_then(&marks, "", "{\"verdict\":\"accepted\"}", 0);
    let (base, source, scenarios, integrator) = world("owner-withdrawn-accepted", &gate);
    let db = Db::new("owner_withdrawn_accepted");
    let m0 = source_main(&source);
    db.gated_mission(&source, &scenarios);
    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("A.md", "a\n")],
        "owner: a",
    );
    let request = gated_request(&db, &source, &commit, "a");
    let _ = std::fs::remove_file(marks.join("release"));
    integrator.pass_on(&db.store).expect("a pass");
    assert_eq!(wait_started(&marks, 1).len(), 1, "its gate started");
    super::owner::withdraw(&db.store, request).expect("withdrawn while judged");
    std::fs::write(marks.join("release"), "").expect("release");
    integrator.settle();
    assert_eq!(source_main(&source), m0, "an accepted gate lands nothing");
    assert!(control_records(&db, "source.moved").is_empty());
    assert_withdrawn_only(&db, request);
    assert_eq!(faults_of(&integrator, request), None);

    let next = gated_request(&db, &source, &commit, "a, again");
    integrator.pass_on(&db.store).expect("the next request");
    integrator.settle();
    let moved = control_records(&db, "source.moved");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["request"], json!(next));
    assert_eq!(source_main(&source), commit);
    drop(release);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// The pass after a withdrawal stops the owner judging in flight for
/// the request: its gate is killed, not waited for, its result is
/// discarded, and the source is free at the pass that reaps it.
#[test]
fn a_withdrawal_stops_the_owner_judging_in_flight() {
    let marks = fresh_marks("spawn-owner-withdrawn-stopped");
    let release = Release(marks.clone());
    let gate = gate_holding_then(&marks, "", "{\"verdict\":\"accepted\"}", 0);
    let (base, source, scenarios, integrator) = world("owner-withdrawn-stopped", &gate);
    let db = Db::new("owner_withdrawn_stopped");
    let m0 = source_main(&source);
    db.gated_mission(&source, &scenarios);
    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("A.md", "a\n")],
        "owner: a",
    );
    let request = gated_request(&db, &source, &commit, "a");
    let _ = std::fs::remove_file(marks.join("release"));
    integrator.pass_on(&db.store).expect("a pass");
    assert_eq!(wait_started(&marks, 1).len(), 1, "its gate started");
    integrator
        .pass_on(&db.store)
        .expect("a pass while it is judged");
    assert!(
        integrator
            .running
            .borrow()
            .iter()
            .all(|gate| gate.stop.by().is_none()),
        "a pending request's judging is not stopped"
    );
    super::owner::withdraw(&db.store, request).expect("withdrawn while judged");
    // A gate that is not stopped is released after 300 s (a guard
    // against a hang only): the test then fails on `release`.
    let watchdog = marks.join("release");
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_mins(5));
        let _ = std::fs::write(watchdog, "");
    });
    integrator
        .pass_on(&db.store)
        .expect("the pass that stops it");
    integrator.settle();
    assert!(!marks.join("release").exists(), "never released");
    assert_eq!(source_main(&source), m0, "nothing moves");
    assert!(control_records(&db, "source.moved").is_empty());
    assert_withdrawn_only(&db, request);
    assert_eq!(faults_of(&integrator, request), None, "no backoff");

    std::fs::write(marks.join("release"), "").expect("release");
    let next = gated_request(&db, &source, &commit, "a, again");
    integrator.pass_on(&db.store).expect("the next request");
    integrator.settle();
    let moved = control_records(&db, "source.moved");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["request"], json!(next));
    assert_eq!(started(&marks).len(), 2, "judged at once");
    drop(release);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// An urgent owner's request (`spawn mission push --urgent`,
/// design-owner-push-between-gates.md, D2) stops the task's gate in
/// flight on its source: the gate is killed, not waited for, and the
/// task's judging records itself discarded by the request (`stale`,
/// found `discarded`, the request and the owner's reason), counted
/// nowhere. The next pass applies the request, and the task is judged
/// again on the owner's main. A request that is not urgent waits for
/// the gate (D1, above).
#[test]
fn an_urgent_owner_push_stops_the_gate_in_flight_and_discards_it() {
    let marks = fresh_marks("spawn-owner-urgent");
    let release = Release(marks.clone());
    let gate = gate_holding_then(&marks, "", "{\"verdict\":\"accepted\"}", 0);
    let (base, source, scenarios, integrator) = world("owner-urgent", &gate);
    let db = Db::new("owner_urgent");
    let m0 = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    let before = gated(&db, mission);
    let task = db.reported_task(mission, "w");
    let agent = agent_of(&db, mission, task);
    push_work(&base, &source, &agent, "work.txt");
    let _ = std::fs::remove_file(marks.join("release"));
    integrator.pass_on(&db.store).expect("a pass");
    assert_eq!(wait_started(&marks, 1).len(), 1, "the task's gate started");

    let commit = owner_commit(&base, &source, "FIX.md", "fix\n", "owner: fix 046");
    let request = db
        .store
        .source_push_request(
            &source,
            &commit,
            &super::owner::owner_ref(&commit),
            &spawn_postgres::PushRequest {
                urgent: true,
                reason: Some("046 is red on main".to_string()),
                ..spawn_postgres::PushRequest::default()
            },
        )
        .expect("the urgent request");
    // A gate that is not stopped is released after 300 s (a guard
    // against a hang only): it then lands `accepted`, and the test fails.
    let watchdog = marks.join("release");
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_mins(5));
        let _ = std::fs::write(watchdog, "");
    });
    integrator
        .pass_on(&db.store)
        .expect("the pass that stops it");
    assert_eq!(source_main(&source), m0, "nothing moves under the gate");
    integrator.settle();
    assert!(!marks.join("release").exists(), "never released");
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["verdict"], "stale", "{record}");
    assert_eq!(record["found"], "discarded");
    assert_eq!(record["request"], json!(request));
    assert_eq!(
        record["reason"],
        format!("discarded by the owner's urgent landing (request {request}): 046 is red on main")
    );
    let after = gated(&db, mission);
    assert_eq!(after.gate_aborts, before.gate_aborts, "counted nowhere");
    assert_eq!(after.gate_rejections, before.gate_rejections);
    assert!(control_records(&db, "source.moved").is_empty());

    std::fs::write(marks.join("release"), "").expect("release");
    integrator.pass_on(&db.store).expect("the pass after it");
    let moved = control_records(&db, "source.moved");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["request"], json!(request));
    assert_eq!(moved[0]["from"], m0.as_str());
    let owner_main = source_main(&source);
    integrator
        .pass_on(&db.store)
        .expect("the pass that judges again");
    integrator.settle();
    assert_eq!(verdicts(&db, task), vec!["stale", "accepted"]);
    let judged = started(&marks);
    assert_eq!(judged.len(), 2, "{judged:?}");
    assert_eq!(
        git(&[
            "--git-dir",
            &source,
            "rev-parse",
            &format!("{}^1", source_main(&source))
        ])
        .expect("first parent"),
        owner_main,
        "judged again and landed on the owner's main"
    );
    drop(release);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// A red main (design-main-red.md) is a fact about the main the gate
/// judged against: when main moved while the gate ran, no
/// `source.main_red` is written for a main that is no longer main, and
/// the task's verdict is `stale`, found `host_fault`.
#[test]
fn no_red_main_is_recorded_for_a_main_that_moved_while_the_gate_ran() {
    let marks = fresh_marks("spawn-stale-red");
    let release = Release(marks.clone());
    let gate = gate_holding_then(
        &marks,
        "echo '| == regression: check-tests.sh must still pass'\n",
        "{\"host_fault\":true,\"scenario\":\"check-tests.sh\",\"on\":\"main\",\"runs\":\"1, then 1 on the candidate; 1, then 1 on the base\",\"reason\":\"r\",\"failures\":[{\"test\":\"api::slice_11\",\"message\":\"expected 23 to be 21\"}]}",
        6,
    );
    let (base, source, scenarios, mut integrator) = world("stale-red", &gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_secs(1),
        cap: 3,
    };
    let db = Db::new("stale_red");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = agent_of(&db, mission, task);
    push_work(&base, &source, &agent, "work.txt");
    judged_while_main_moves(&integrator, &db, &base, &source, &marks, "main.txt");
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["verdict"], "stale", "{record}");
    assert_eq!(record["found"], "host_fault");
    assert!(control_records(&db, "source.main_red").is_empty());
    assert!(
        db.mission(mission)["red"].is_null(),
        "{}",
        db.mission(mission)
    );
    drop(release);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// A judging that panics after main moved (judged_or_aborted): no abort
/// counted toward the halt for a main that is no longer main, `stale`
/// with found `aborted` and the panic's reason. Three of them halt
/// nothing.
#[test]
fn a_judging_that_panics_after_main_moved_is_stale() {
    let (base, source, scenarios, integrator) = world("panic-moved", "exit 1\n");
    let db = Db::new("panic_moved");
    let m0 = source_main(&source);
    let mission = mission_at_baseline(&db, &source, &scenarios, &m0);
    let aborts = gated(&db, mission).gate_aborts;
    let task = db.reported_task(mission, "w");
    let m1 = main_moves(&base, &source, "main.txt");
    db.store.source_moved(&source, &m0, &m1).expect("moved");
    for _ in 0..3 {
        super::judged_or_aborted(&db.store, &ready_task(&db, task), &m0, mission, || {
            panic!("a bug in the judging")
        });
    }
    assert_eq!(verdicts(&db, task), vec!["stale"; 3]);
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["found"], "aborted", "{record}");
    assert_eq!(record["main_now"], m1.as_str());
    assert!(
        record["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("a bug in the judging")),
        "{record}"
    );
    assert_eq!(gated(&db, mission).gate_aborts, aborts, "none counted");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    let board = db.mission(mission);
    assert!(board["halted"].is_null(), "{board}");
    let _ = std::fs::remove_dir_all(&base);
}

/// Main cannot be read when the gate ends (the source is gone for the
/// moment): the verdict is recorded as found, as before the stale check,
/// with the read's error as `main_read`, so the record says main was
/// never checked. The same for a judging that panicked.
#[test]
fn a_verdict_whose_main_cannot_be_read_is_recorded_as_found_with_main_read() {
    let marks = fresh_marks("spawn-main-unread");
    let release = Release(marks.clone());
    let gate = gate_holding_then(
        &marks,
        "",
        "{\"verdict\":\"rejected\",\"reason\":\"regression: 012.sh must still pass\"}",
        0,
    );
    let (base, source, scenarios, integrator) = world("main-unread", &gate);
    let db = Db::new("main_unread");
    let m0 = source_main(&source);
    let mission = mission_at_baseline(&db, &source, &scenarios, &m0);
    let task = db.reported_task(mission, "w");
    let agent = agent_of(&db, mission, task);
    push_work(&base, &source, &agent, "work.txt");
    let away = base.join("source-away.git");
    integrator.pass_on(&db.store).expect("a pass");
    assert_eq!(wait_started(&marks, 1).len(), 1, "the gate started");
    std::fs::rename(&source, &away).expect("the source goes away");
    std::fs::write(marks.join("release"), "").expect("release");
    integrator.settle();
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["verdict"], "rejected", "as found: {record}");
    assert_eq!(record["base"], m0.as_str());
    assert!(
        record["main_read"]
            .as_str()
            .is_some_and(|error| !error.is_empty()),
        "{record}"
    );
    assert!(record.get("main_now").is_none(), "{record}");

    // A judging that panics while the source is away: aborted, as
    // before, and it says main was not read.
    let other = db.reported_task(mission, "x");
    super::judged_or_aborted(&db.store, &ready_task(&db, other), &m0, mission, || {
        panic!("a bug in the judging")
    });
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["verdict"], "aborted", "{record}");
    assert!(record["main_read"].is_string(), "{record}");
    std::fs::rename(&away, &source).expect("the source is back");
    drop(release);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// The cap counts per main: only the host faults judged against the main
/// being judged now. Supply PA's task 207 (2026-11-09): its host fault of
/// 01:45 on fcf5e1e (a red main) still counted at 08:32, on a main that
/// had moved since. With the cap at 1, a host fault on A reaches it; main
/// moves to B (the owner's push, recorded); the next host fault, on B, is
/// `host_fault` again, the first on B, never capped into an abort.
#[test]
fn host_faults_on_an_older_main_count_nothing_toward_the_cap_on_this_one() {
    let gate = "echo '| == regression: check-tests.sh must still pass'\n\
                    echo '| FAILED api::tests::one: error connecting to server: Connection refused (os error 111)'\n\
                    printf '{\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the candidate\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
    let (base, source, scenarios, mut integrator) = world("cap-per-main", gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_millis(1),
        cap: 1,
    };
    let db = Db::new("cap_per_main");
    let a = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = agent_of(&db, mission, task);
    push_work(&base, &source, &agent, "work.txt");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    let first = last_verdict(&db).0;
    assert_eq!(first["verdict"], "host_fault", "{first}");
    assert_eq!(first["base"], a.as_str());
    assert_eq!(db.store.task_host_faults(task, 0, &a).expect("count"), 1);

    let b = main_moves(&base, &source, "main.txt");
    db.store.source_moved(&source, &a, &b).expect("moved");
    assert_eq!(
        db.store.task_host_faults(task, 0, &b).expect("count"),
        0,
        "none on B yet"
    );
    past_due(&db);
    integrator.pass_on(&db.store).expect("a pass on B");
    integrator.settle();
    let second = last_verdict(&db).0;
    assert_eq!(
        second["verdict"], "host_fault",
        "A's fault is not B's: {second}"
    );
    assert_eq!(second["base"], b.as_str());
    assert_eq!(db.store.task_host_faults(task, 0, &b).expect("count"), 1);
    assert_eq!(gated(&db, mission).gate_aborts, 0);

    // On B, the cap holds as before: the next one is an abort.
    past_due(&db);
    integrator.pass_on(&db.store).expect("a third pass");
    integrator.settle();
    let third = last_verdict(&db).0;
    assert_eq!(third["verdict"], "aborted", "{third}");
    assert!(
        third["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains(&format!(
                "has had 1 at report revision 0 on main {}",
                &b[..12]
            ))),
        "{third}"
    );
    assert_eq!(gated(&db, mission).gate_aborts, 1);
    let _ = std::fs::remove_dir_all(&base);
}

/// A gate verdict's `flakes` (checks that failed once and passed again)
/// are on the task's `task.gated` record; a verdict without them leaves
/// the record without the key, as every record before it.
#[test]
fn a_verdicts_flakes_are_on_its_record() {
    let gate = "printf '{\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the candidate\",\"flakes\":[\"check-e2e.sh\"]}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
    let (base, source, scenarios, integrator) = world("gate-flakes", gate);
    let db = Db::new("gate_flakes");
    let mission = db.gated_mission(&source, &scenarios);
    let _task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["flakes"], json!(["check-e2e.sh"]), "{record}");
    let _ = std::fs::remove_dir_all(&base);
}

/// A gate verdict's `main_red_scenarios` (scenarios solved on an
/// ancestor of the base and unsolved on it) are on the task's
/// `task.gated` record, whatever the verdict; a verdict without them
/// leaves the record without the key.
#[test]
fn a_verdicts_red_scenarios_are_on_its_record() {
    let gate = "printf '{\"verdict\":\"accepted\",\"main_red_scenarios\":[\"046.sh\"]}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
    let (base, source, scenarios, integrator) = world("gate-red-scenarios", gate);
    let db = Db::new("gate_red_scenarios");
    let mission = db.gated_mission(&source, &scenarios);
    let _task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["main_red_scenarios"], json!(["046.sh"]), "{record}");
    let _ = std::fs::remove_dir_all(&base);
}

/// A gate verdict's `quarantined` (the regressions the owner's list
/// kept from running) is on the task's `task.gated` record.
#[test]
fn a_verdicts_quarantined_are_on_its_record() {
    let gate = "printf '{\"verdict\":\"accepted\",\"quarantined\":[\"028.sh\"]}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
    let (base, source, scenarios, integrator) = world("gate-quarantined", gate);
    let db = Db::new("gate_quarantined");
    let mission = db.gated_mission(&source, &scenarios);
    let _task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["quarantined"], json!(["028.sh"]), "{record}");
    let _ = std::fs::remove_dir_all(&base);
}

/// Objective 061 (a): an owner judging's records name the gate's wall
/// clock (`gate_ms`) and the mission whose gate judged it (`mission`):
/// the `source.moved` of an accepted push and the `source.push_refused`
/// of a rejected one.
#[test]
fn an_owner_judgings_records_carry_its_gate_time_and_mission() {
    let marks = fresh_marks("spawn-owner-gate-ms");
    let seen = marks.to_string_lossy().to_string();
    let gate = format!(
        "n=$(cat '{seen}/n' 2>/dev/null || echo 0); n=$((n + 1)); echo \"$n\" >'{seen}/n'\n\
             sleep 0.2\n\
             if [ \"$n\" = 1 ]; then\n\
             printf '%s\\n' '{{\"v\":1,\"verdict\":\"accepted\"}}' >\"$SPAWN_GATE_VERDICT\"\n\
             else\n\
             printf '%s\\n' '{{\"v\":1,\"verdict\":\"rejected\",\"reason\":\"a scenario fails\"}}' >\"$SPAWN_GATE_VERDICT\"\n\
             fi\n"
    );
    let (base, source, scenarios, integrator) = world("owner-gate-ms", &gate);
    let db = Db::new("owner_gate_ms");
    let mission = db.gated_mission(&source, &scenarios);
    let first = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("F.md", "f\n")],
        "owner: f",
    );
    gated_request(&db, &source, &first, "f");
    integrator.pass_on(&db.store).expect("the first judging");
    integrator.settle();
    let second = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("G.md", "g\n")],
        "owner: g",
    );
    gated_request(&db, &source, &second, "g");
    integrator.pass_on(&db.store).expect("the second judging");
    integrator.settle();
    let moved = control_records(&db, "source.moved");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["mission"], mission, "{}", moved[0]);
    assert!(
        moved[0]["gate_ms"].as_i64().is_some_and(|ms| ms >= 200),
        "the gate's wall clock: {}",
        moved[0]
    );
    let refused = control_records(&db, "source.push_refused");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0]["verdict"], "rejected", "{}", refused[0]);
    assert_eq!(refused[0]["mission"], mission, "{}", refused[0]);
    assert!(
        refused[0]["gate_ms"].as_i64().is_some_and(|ms| ms >= 200),
        "{}",
        refused[0]
    );
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// Objective 056 (a): a verdict's `passed` and `reused` (the regressions
/// that passed in the gate, run or taken from the base's cache) reach
/// the records written from it: an owner landing's `source.moved` and a
/// task's `task.gated`, accepted or rejected; a verdict without them
/// leaves both keys off.
#[test]
fn a_verdicts_passed_and_reused_reach_both_records() {
    let gate = "if [ \"${SPAWN_GATE_OWNER_CANDIDATE-}\" = 1 ]; then\n\
             printf '%s\\n' '{\"v\":1,\"verdict\":\"accepted\",\"validator_defects\":[\"028.sh\"],\"passed\":[\"001.sh\",\"check-web.sh\",\"../x.sh\"],\"reused\":[\"002.sh\"]}' >\"$SPAWN_GATE_VERDICT\"\n\
             exit 0\nfi\n\
             printf '%s\\n' '{\"v\":1,\"verdict\":\"rejected\",\"reason\":\"r\",\"passed\":[\"003.sh\"],\"reused\":[\"004.sh\",\"005.sh\"]}' >\"$SPAWN_GATE_VERDICT\"\n";
    let (base, source, scenarios, integrator) = world("gate-passed-reused", gate);
    let db = Db::new("gate_passed_reused");
    let mission = db.gated_mission(&source, &scenarios);
    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("E.md", "e\n")],
        "owner: e",
    );
    gated_request(&db, &source, &commit, "e");
    integrator.pass_on(&db.store).expect("the owner's judging");
    integrator.settle();
    let moved = control_records(&db, "source.moved");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(
        moved[0]["passed"],
        json!(["001.sh", "check-web.sh"]),
        "{}",
        moved[0]
    );
    assert_eq!(moved[0]["reused"], json!(["002.sh"]), "{}", moved[0]);
    let task = db.reported_task(mission, "w");
    let agent = agent_of(&db, mission, task);
    push_work(&base, &source, &agent, "work.txt");
    integrator.pass_on(&db.store).expect("the task's judging");
    integrator.settle();
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["verdict"], "rejected", "{record}");
    assert_eq!(record["passed"], json!(["003.sh"]), "{record}");
    assert_eq!(record["reused"], json!(["004.sh", "005.sh"]), "{record}");
    let _ = std::fs::remove_dir_all(&base);
}

/// A gate verdict without `main_red_scenarios` (every gate before it)
/// leaves the record without the key.
#[test]
fn a_verdict_without_red_scenarios_has_none_on_its_record() {
    let gate = "printf '{\"verdict\":\"rejected\",\"reason\":\"r\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
    let (base, source, scenarios, integrator) = world("gate-no-red-scenarios", gate);
    let db = Db::new("gate_no_red_scenarios");
    let mission = db.gated_mission(&source, &scenarios);
    let _task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    let (record, _, _) = last_verdict(&db);
    assert!(record.get("main_red_scenarios").is_none(), "{record}");
    assert!(
        record.get("passed").is_none() && record.get("reused").is_none(),
        "{record}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A base-only host fault (the gate's fact `on` `base`, before any
/// candidate code ran) counts toward no halt, so it never counts toward
/// the cap and is never capped: with the cap at 2, a gate that keeps
/// reporting a base harness error is `host_fault` every time, and the
/// task never aborts.
#[test]
fn base_only_host_faults_never_reach_the_cap() {
    let gate = "echo '| == base: 012.sh'\n\
                    printf '{\"host_fault\":true,\"scenario\":\"012.sh\",\"on\":\"base\",\"runs\":\"1, then 0\",\"reason\":\"012.sh exited 1, then 0 on the base\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
                    exit 6\n";
    let (base, source, scenarios, mut integrator) = world("gate-fault-base-cap", gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_secs(1),
        cap: 2,
    };
    let db = Db::new("gate_host_fault_base_cap");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    let mut due = None;
    for round in 0..4 {
        if let Some(due) = due {
            let left = due - super::now_ms();
            if left > 0 {
                std::thread::sleep(std::time::Duration::from_millis(
                    u64::try_from(left).unwrap_or(0) + 50,
                ));
            }
        }
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        let record = last_verdict(&db).0;
        assert_eq!(record["verdict"], "host_fault", "round {round}: {record}");
        assert_eq!(record["fact"]["on"], "base", "{record}");
        due = record["due_ms"].as_i64();
    }
    assert_eq!(
        db.store
            .task_host_faults(task, 0, &source_main(&source))
            .expect("count"),
        0,
        "base-only faults are not counted"
    );
    assert_eq!(gated(&db, mission).gate_aborts, 0);
    assert_eq!(gated(&db, mission).gate_host_faults, 4, "each one a run");
    let _ = std::fs::remove_dir_all(&base);
}

/// A check that failed on the candidate and on the base (the gate's fact
/// `on` `candidate and base`) fails on main without the candidate, so it
/// counts toward no halt, never counts toward the cap and is never
/// capped: with the cap at 2, a gate that keeps reporting it (a red main:
/// Supply PA's check-tests, 2026-11-09 00:48-03:42) is `host_fault` every
/// time, and the task never aborts.
#[test]
fn candidate_and_base_host_faults_never_reach_the_cap() {
    let gate = "echo '| == regression: check-e2e.sh must still pass'\n\
                    printf '{\"host_fault\":true,\"scenario\":\"check-e2e.sh\",\"on\":\"candidate and base\",\"runs\":\"1, then 1 on the candidate; 1 on the base\",\"reason\":\"check-e2e.sh failed twice on the candidate and on the base\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
                    exit 6\n";
    let (base, source, scenarios, mut integrator) = world("gate-fault-both-cap", gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_secs(1),
        cap: 2,
    };
    let db = Db::new("gate_host_fault_both_cap");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    let mut due = None;
    for round in 0..4 {
        if let Some(due) = due {
            let left = due - super::now_ms();
            if left > 0 {
                std::thread::sleep(std::time::Duration::from_millis(
                    u64::try_from(left).unwrap_or(0) + 50,
                ));
            }
        }
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        let record = last_verdict(&db).0;
        assert_eq!(record["verdict"], "host_fault", "round {round}: {record}");
        assert_eq!(record["fact"]["on"], "candidate and base", "{record}");
        due = record["due_ms"].as_i64();
    }
    assert_eq!(
        db.store
            .task_host_faults(task, 0, &source_main(&source))
            .expect("count"),
        0,
        "faults on the candidate and the base are not counted"
    );
    assert_eq!(gated(&db, mission).gate_aborts, 0);
    assert_eq!(gated(&db, mission).gate_host_faults, 4, "each one a run");
    let _ = std::fs::remove_dir_all(&base);
}

/// A gate that rejects a merge (`$3^2` exists) with check-tests.sh failing
/// on it, and gives the head alone, judged against the base it was built
/// on, the host fault `head_fault` (a verdict file's JSON) with exit 6.
fn head_alone_gate(head_fault: &str) -> String {
    format!(
        "if git --git-dir=\"$1\" rev-parse -q --verify \"$3^2\" >/dev/null; then\n\
               echo ' FAIL  test/health.test.ts > shifts what the server stamps'\n\
               printf '{{\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the candidate\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
               exit 0\n\
             fi\n\
             echo '| == the head alone'\n\
             printf '%s\\n' '{head_fault}' >\"$SPAWN_GATE_VERDICT\"\n\
             exit 6\n"
    )
}

/// The task's work, made on `on` (an older main) and pushed as
/// `spawn/<agent>`: one new file. The pushed commit.
fn push_work_on(base: &Path, source: &str, agent: &str, on: &str, file: &str) -> String {
    let clone = base.join(format!("agent-on-{file}"));
    let clone_path = clone.to_string_lossy().to_string();
    git(&["clone", "-q", source, &clone_path]).expect("clone");
    git(&["-C", &clone_path, "checkout", "-q", on]).expect("checkout the older main");
    std::fs::write(clone.join(file), "work\n").expect("write");
    super::tests::commit_in(&clone_path, "work");
    git(&[
        "-C",
        &clone_path,
        "push",
        "-q",
        "origin",
        &format!("HEAD:refs/heads/spawn/{agent}"),
    ])
    .expect("push the agent's branch");
    git(&["-C", &clone_path, "rev-parse", "HEAD"]).expect("work")
}

/// A judging's description says its candidate was judged once the
/// candidate's gate has ended, before any later gate of it starts (a
/// rejection's head re-judging), so an integrator that finds the later
/// gate left running never adopts it as the candidate's (rev-e G1,
/// [`judging::left`]).
#[test]
fn a_judging_says_its_candidate_was_judged_before_its_head_is_judged_alone() {
    let marks = fresh_marks("spawn-candidate-judged");
    let seen = marks.to_string_lossy().to_string();
    let gate = format!(
        "described=\"$(dirname \"$(dirname \"$SPAWN_GATE_VERDICT\")\")/judging.json\"\n\
             if grep -q '\"candidate_judged\":true' \"$described\"; then said=judged; else said=not; fi\n\
             if git --git-dir=\"$1\" rev-parse -q --verify \"$3^2\" >/dev/null; then\n\
               echo \"merge $said\" >>'{seen}/said'\n\
               printf '{{\"verdict\":\"rejected\",\"reason\":\"regression: 011.sh fails\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
               exit 0\n\
             fi\n\
             echo \"head $said\" >>'{seen}/said'\n\
             printf '{{\"verdict\":\"rejected\",\"reason\":\"regression: 011.sh fails\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n"
    );
    let (base, source, scenarios, integrator) = world("candidate-judged", &gate);
    let db = Db::new("candidate_judged");
    let m0 = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    main_moves(&base, &source, "main.txt");
    push_work_on(&base, &source, &agent, &m0, "work.txt");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    assert_eq!(
        std::fs::read_to_string(marks.join("said")).expect("said"),
        "merge not\nhead judged\n",
        "the merge is the candidate's gate; the head's comes after it is said"
    );
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// Supply PA's task 207 (2026-11-09): a rejected merge's head, judged
/// alone against the base it was built on, gave no verdict (a host
/// fault there). The record says so through the store: its reason names
/// the merge's rejection, the head's base and that the fault is the head's
/// run there, never a fault of main; `head` and `head_base` are on it; and
/// the supervisor's digest item, at the second in a row, says it was the
/// work alone on that base.
#[test]
fn a_head_judged_alone_without_a_verdict_says_it_was_the_head_on_its_base() {
    let fault = "{\"host_fault\":true,\"scenario\":\"011.sh\",\"on\":\"base\",\"runs\":\"2\",\"reason\":\"a harness error\"}";
    let (base, source, scenarios, mut integrator) =
        world("head-alone-fault", &head_alone_gate(fault));
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_secs(1),
        cap: 3,
    };
    let db = Db::new("head_alone_fault");
    let m0 = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    let m1 = main_moves(&base, &source, "main.txt");
    let head = push_work_on(&base, &source, &agent, &m0, "work.txt");
    let mut due = None;
    for _ in 0..2 {
        if let Some(due) = due {
            let left = due - super::now_ms();
            if left > 0 {
                std::thread::sleep(std::time::Duration::from_millis(
                    u64::try_from(left).unwrap_or(0) + 50,
                ));
            }
        }
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        due = last_verdict(&db).0["due_ms"].as_i64();
    }
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["verdict"], "host_fault", "{record}");
    assert_eq!(record["head"], head.as_str(), "{record}");
    assert_eq!(record["head_base"], m0.as_str(), "{record}");
    assert_eq!(record["base"], m1.as_str(), "the merge's base, as before");
    let reason = record["reason"].as_str().expect("a reason");
    for wanted in [
        &format!("the merge with main {} was rejected", &m1[..12]),
        "regression: check-tests.sh fails on the candidate",
        &format!(
            "judging the work alone against the base it was built on, {}, gave no verdict",
            &m0[..12]
        ),
        "011.sh exited 2 on the base",
    ] {
        assert!(reason.contains(wanted), "{wanted:?} in {reason}");
    }
    assert_eq!(record["escalate"], true, "the second in a row: {record}");
    let items = crate::dispatch::digest::host_fault_escalations(
        db.store
            .gate_host_fault_escalations()
            .expect("escalations")
            .as_array()
            .expect("records"),
    );
    assert_eq!(items.len(), 1, "{items:?}");
    assert!(
        items[0].line.contains(&format!(
            "judging the work alone against the base it was built on ({}), after main \
                 rejected the merge",
            &m0[..12]
        )),
        "{}",
        items[0].line
    );
    let _ = std::fs::remove_dir_all(&base);
}

// --- objective 035: every verdict binds what it judged and where -------

/// A controller tree under `base` (`<base>/ctl`, its gate directory
/// `<base>/ctl/rsi/scenarios` holding one check), its `.controller`
/// naming `commit`: the gate directory.
fn controller_tree(base: &Path, commit: &str) -> PathBuf {
    let scenarios = base.join("ctl/rsi/scenarios");
    std::fs::create_dir_all(&scenarios).expect("the gate directory");
    std::fs::write(scenarios.join("check-ok.sh"), "exit 0\n").expect("a check");
    std::fs::write(base.join("ctl/.controller"), format!("{commit}\n")).expect("the marker");
    scenarios
}

/// The gate's arguments, run by run: `(base, candidate)`.
fn noted_args(base: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(base.join("args"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            Some((words.next()?.to_string(), words.next()?.to_string()))
        })
        .collect()
}

/// Notes `$2 $3` beside itself, then: its first run prints its setup's
/// toolchain line and rejects; every later one ends before any setup
/// (exit 3, an abort).
const BINDING_GATE: &str = "dir=$(dirname \"$0\")\n\
        printf '%s %s\\n' \"$2\" \"$3\" >>\"$dir/args\"\n\
        if [ \"$(wc -l <\"$dir/args\")\" -eq 1 ]; then\n\
          echo '| == setup'\n\
          echo '| base cache: toolchain rustc 1.95.0-test'\n\
          printf '{\"verdict\":\"rejected\",\"reason\":\"fails\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
          exit 0\n\
        fi\n\
        echo 'gate: a bad setting' >&2\n\
        exit 3\n";

/// Every verdict has its class: four about the code, three about the
/// infrastructure (every host fault's evidence alike), `stale`
/// superseded; nothing else gets one.
#[test]
fn every_verdict_has_its_class() {
    for (verdict, class) in [
        ("accepted", "code"),
        ("rejected", "code"),
        ("conflict", "code"),
        ("incompatible", "code"),
        ("aborted", "infrastructure"),
        ("host_fault", "infrastructure"),
        ("no_room", "infrastructure"),
        ("stale", "superseded"),
    ] {
        assert_eq!(super::verdict_class(verdict), Some(class), "{verdict}");
        let bound = super::bound(&json!({"type": "task.gated", "verdict": verdict}));
        assert_eq!(bound["class"], class, "{bound}");
    }
    assert_eq!(super::verdict_class("solved"), None);
    assert_eq!(super::verdict_class(""), None);
    // A record no gate ran for: what it knows, no environment.
    let stale = super::bound(&json!({"type": "task.gated", "verdict": "stale", "base": "b"}));
    assert!(
        stale["env"].is_null() && stale.get("env").is_some(),
        "{stale}"
    );
    assert_eq!(stale["inputs"]["base"], "b");
    for name in ["candidate", "controller", "scenarios_sha256", "scenario"] {
        assert!(stale["inputs"][name].is_null(), "{name}: {stale}");
    }
    // A record that says what it judged keeps it; others go as they are.
    let gated = super::bound(&json!({"type": "task.gated", "verdict": "accepted",
                                      "inputs": {"base": "x"}, "env": {"host": "h"}}));
    assert_eq!(
        (&gated["inputs"]["base"], &gated["env"]["host"]),
        (&json!("x"), &json!("h"))
    );
    let landed = json!({"type": "task.landed", "id": 1});
    assert_eq!(super::bound(&landed), landed);
}

/// Two judgings by one integrator: each verdict's `inputs` are exactly
/// the gate's arguments, with the controller and the scenario set read
/// when that gate started (a marker changed and a file added between
/// them are seen); its `env` names this host and the toolchain its run
/// printed, null for a run that printed none; the rejection is `code`,
/// the abort `infrastructure`, and the counts move as before.
#[test]
fn each_verdict_binds_the_inputs_and_environment_of_its_own_run() {
    let (base, source, _, mut integrator) = world("binding-runs", BINDING_GATE);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_secs(1),
        cap: 3,
    };
    let first = "0123456789abcdef0123456789abcdef01234567";
    let second = "fedcba9876543210fedcba9876543210fedcba98";
    let scenarios = controller_tree(&base, first);
    let db = Db::new("binding_runs");
    let mission = db.gated_mission(&source, &scenarios);
    let one = db.reported_task(mission, "w1");
    push_work(&base, &source, &agent_of(&db, mission, one), "one.txt");
    let digest_one = super::inputs::scenarios_sha256(&scenarios).expect("a digest");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    let rejected = last_gated(&db);
    let runs = noted_args(&base);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(rejected["verdict"], "rejected", "{rejected}");
    assert_eq!(rejected["class"], "code");
    assert_eq!(rejected["inputs"]["base"], runs[0].0.as_str());
    assert_eq!(rejected["inputs"]["candidate"], runs[0].1.as_str());
    assert_eq!(rejected["inputs"]["base"], rejected["base"], "{rejected}");
    assert_eq!(rejected["inputs"]["controller"], first);
    assert_eq!(rejected["inputs"]["scenarios_sha256"], digest_one.as_str());
    assert!(rejected["inputs"]["scenario"].is_null(), "{rejected}");
    assert_eq!(rejected["env"]["host"], json!(super::inputs::host_name()));
    assert_eq!(rejected["env"]["toolchain"], "rustc 1.95.0-test");
    assert!(rejected["env"]["runner"].is_null(), "{rejected}");
    assert!(
        rejected.get("head_inputs").is_none(),
        "no head run: {rejected}"
    );

    // Between the runs: the marker names another commit, a file joins.
    std::fs::write(base.join("ctl/.controller"), format!("{second}\n")).expect("the marker");
    std::fs::write(scenarios.join("check-two.sh"), "exit 0\n").expect("another check");
    let digest_two = super::inputs::scenarios_sha256(&scenarios).expect("a digest");
    assert_ne!(digest_two, digest_one);
    let two = db.reported_task(mission, "w2");
    push_work(&base, &source, &agent_of(&db, mission, two), "two.txt");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    let aborted = last_gated(&db);
    let runs = noted_args(&base);
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(
        (&aborted["id"], &aborted["verdict"]),
        (&json!(two), &json!("aborted")),
        "{aborted}"
    );
    assert_eq!(aborted["class"], "infrastructure");
    assert_eq!(aborted["inputs"]["base"], runs[1].0.as_str());
    assert_eq!(aborted["inputs"]["candidate"], runs[1].1.as_str());
    assert_eq!(
        aborted["inputs"]["controller"], second,
        "read per run, not remembered"
    );
    assert_eq!(aborted["inputs"]["scenarios_sha256"], digest_two.as_str());
    assert!(
        aborted["env"]["toolchain"].is_null(),
        "printed none: {aborted}"
    );
    assert_eq!(aborted["env"]["host"], json!(super::inputs::host_name()));
    let after = gated(&db, mission);
    assert_eq!((after.gate_rejections, after.gate_aborts), (1, 1));

    // A marker gone, then a malformed one: null, never the last commit.
    for marker in [None, Some("not a commit\n")] {
        match marker {
            None => std::fs::remove_file(base.join("ctl/.controller")).expect("remove"),
            Some(text) => std::fs::write(base.join("ctl/.controller"), text).expect("write"),
        }
        assert_eq!(
            super::inputs::controller_of(&scenarios)
                .and_then(|dir| super::inputs::controller_commit(&dir)),
            None,
            "{marker:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// A merge the gate rejects whose head passes alone is `incompatible`:
/// its record binds the merge's run (`inputs`, `env`) and the head's
/// (`head_inputs`, based on `head_base`).
#[test]
fn an_incompatible_verdict_binds_the_head_run_too() {
    let gate = "printf '%s %s\\n' \"$2\" \"$3\" >>\"$(dirname \"$0\")/args\"\n\
                    echo '| base cache: toolchain rustc 1.95.0-test'\n\
                    if git --git-dir=\"$1\" rev-parse -q --verify \"$3^2\" >/dev/null; then\n\
                      printf '{\"verdict\":\"rejected\",\"reason\":\"fails merged\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
                      exit 0\n\
                    fi\n\
                    printf '{\"verdict\":\"accepted\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
    let (base, source, _, integrator) = world("binding-head", gate);
    let commit = "0123456789abcdef0123456789abcdef01234567";
    let scenarios = controller_tree(&base, commit);
    let db = Db::new("binding_head");
    let m0 = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = agent_of(&db, mission, task);
    let m1 = main_moves(&base, &source, "main.txt");
    let head = push_work_on(&base, &source, &agent, &m0, "work.txt");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    let record = last_gated(&db);
    let runs = noted_args(&base);
    assert_eq!(record["verdict"], "incompatible", "{record}");
    assert_eq!(record["class"], "code");
    assert_eq!(runs.len(), 2, "the merge, then the head alone: {runs:?}");
    assert_eq!(record["inputs"]["base"], m1.as_str());
    assert_eq!(record["inputs"]["candidate"], runs[0].1.as_str());
    assert_eq!(
        record["inputs"]["candidate"], record["candidate"],
        "{record}"
    );
    assert_eq!(record["inputs"]["controller"], commit);
    assert_eq!(
        record["head_inputs"]["base"],
        m0.as_str(),
        "its base is head_base"
    );
    assert_eq!(
        record["head_inputs"]["base"], record["head_base"],
        "{record}"
    );
    assert_eq!(record["head_inputs"]["candidate"], head.as_str());
    assert_eq!(
        (runs[1].0.as_str(), runs[1].1.as_str()),
        (m0.as_str(), head.as_str())
    );
    assert_eq!(record["head_inputs"]["controller"], commit);
    assert_eq!(
        record["head_inputs"]["scenarios_sha256"],
        record["inputs"]["scenarios_sha256"]
    );
    assert_eq!(record["env"]["toolchain"], "rustc 1.95.0-test");
    let _ = std::fs::remove_dir_all(&base);
}

/// A `stale` verdict judged nothing: its record, as the integrator
/// writes it ([`super::record`]), has no environment and says only what
/// is known; the store takes it and its class.
#[test]
fn a_stale_verdict_has_no_environment() {
    let (base, source, _, _) = world("binding-stale", NOTING_GATE);
    let scenarios = controller_tree(&base, "0123456789abcdef0123456789abcdef01234567");
    let db = Db::new("binding_stale");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let main = source_main(&source);
    super::record(
        &db.store,
        &json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "stale",
                "reason": "behind", "inputs": super::inputs::known(Some(&main), None),
                "env": null}),
    )
    .expect("recorded");
    let stored = last_gated(&db);
    assert_eq!(stored["class"], "superseded");
    assert!(
        stored["env"].is_null() && stored.get("env").is_some(),
        "{stored}"
    );
    assert_eq!(stored["inputs"]["base"], main.as_str());
    for name in ["candidate", "controller", "scenarios_sha256", "scenario"] {
        assert!(stored["inputs"][name].is_null(), "{name}: {stored}");
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// The other half of task 207: the head alone fails the check on its own
/// base too (that base was a red main), so it cannot be judged there. The
/// merge's rejection stands, through the store: `rejected`, counted, with
/// the merge's reason and log (the failing test), `head_verdict`
/// `unjudged` and the reason, and a note that says so; no host fault, no
/// backoff, no run of host faults. Both shapes the gate gives a check
/// failing on a base: `candidate and base`, and a red main.
#[test]
fn a_head_whose_own_base_fails_the_check_leaves_the_merges_rejection_standing() {
    for (tag, fault) in [
        (
            "both",
            "{\"host_fault\":true,\"scenario\":\"check-tests.sh\",\"on\":\"candidate and base\",\"runs\":\"1, then 1 on the candidate; 1 on the base\",\"reason\":\"r\"}",
        ),
        (
            "red",
            "{\"host_fault\":true,\"scenario\":\"check-tests.sh\",\"on\":\"main\",\"runs\":\"1, then 1 on the candidate; 1, then 1 on the base\",\"reason\":\"r\",\"failures\":[{\"test\":\"api::slice_11\",\"message\":\"expected 23 to be 21\"}]}",
        ),
    ] {
        let (base, source, scenarios, mut integrator) =
            world(&format!("head-unjudged-{tag}"), &head_alone_gate(fault));
        integrator.judge.host = super::HostSettings {
            server_log: None,
            backoff: std::time::Duration::from_secs(1),
            cap: 3,
        };
        let db = Db::new(&format!("head_unjudged_{tag}"));
        let m0 = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        db.reported_task(mission, "w");
        let agent = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        main_moves(&base, &source, "main.txt");
        let head = push_work_on(&base, &source, &agent, &m0, "work.txt");
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        let (record, _, _) = last_verdict(&db);
        assert_eq!(record["verdict"], "rejected", "{tag}: {record}");
        assert_eq!(
            record["reason"], "regression: check-tests.sh fails on the candidate",
            "{tag}: the merge's reason"
        );
        assert!(
            record["log"]
                .as_str()
                .is_some_and(|log| log.contains("shifts what the server stamps")),
            "{tag}: the merge's log, with its failing test: {record}"
        );
        assert_eq!(record["head"], head.as_str());
        assert_eq!(record["head_base"], m0.as_str());
        assert_eq!(record["head_verdict"], "unjudged", "{tag}: {record}");
        let why = record["head_reason"].as_str().expect("why");
        assert!(
            why.contains(&m0[..12]) && why.contains("check-tests.sh fails there too"),
            "{tag}: {why}"
        );
        let note = record["note"].as_str().expect("a note");
        assert!(
            note.starts_with("The gate rejected this work: regression: check-tests.sh")
                && note.contains("shifts what the server stamps")
                && note.contains("so the gate's rejection of the merge with main stands"),
            "{tag}: {note}"
        );
        assert!(
            record.get("backoff_ms").is_none(),
            "{tag}: no backoff: {record}"
        );
        let after = gated(&db, mission);
        assert_eq!(after.gate_host_faults, 0, "{tag}: no run of host faults");
        assert_eq!(after.gate_aborts, 0);
        assert_eq!(after.gate_rejections, 1, "{tag}: a rejection, counted");
        let board = db.mission(mission);
        assert_eq!(
            board["tasks"][0]["state"], "revise",
            "{tag}: sent back: {board}"
        );
        assert!(
            control_records(&db, "source.main_red").is_empty(),
            "{tag}: the head's old base is not the source's main"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// Main is red on a check (design-main-red.md), through the store. A gate
/// that finds check-tests.sh failing twice on the base with the same test
/// (an assertion: the work's class) gives the source one red,
/// `source.main_red` with `alert`, and the task a `host_fault` with
/// evidence `main_red` that starts no run and is never capped (the cap
/// here is 1). The red main holds the source's gates: the next pass
/// starts none. Once the probe interval has passed, one gate runs anyway,
/// a probe (`source.main_probe`, the record's `probe`), with the red check
/// given to the gate (SPAWN_GATE_BASE_CHECK); the store takes one probe
/// per interval. When the red check passes on the base, the source is
/// green again (`source.main_green`, its red's `since_ms`), the work is
/// judged as usual, and the supervisor is told of the red once and of the
/// green once.
#[test]
#[allow(clippy::too_many_lines)]
fn a_red_main_holds_its_sources_gates_until_a_probe_finds_it_green() {
    let dir = std::env::temp_dir().join(format!("spawn-db-main-red-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let (runs, green) = (dir.join("runs"), dir.join("green"));
    let (runs_path, green_path) = (runs.display(), green.display());
    let gate = format!(
        "echo \"base_check=${{SPAWN_GATE_BASE_CHECK-}}\" >>'{runs_path}'\n\
             if [ -e '{green_path}' ]; then\n\
               echo '| base check: check-tests.sh passes on the base: main is green on it again'\n\
               if grep -q fault '{green_path}'; then\n\
                 printf '{{\"host_fault\":true,\"scenario\":\"check-e2e.sh\",\"on\":\"candidate and base\",\"runs\":\"1, then 1 on the candidate; 1, then 0 on the base\",\"reason\":\"r\",\"base_check\":\"check-tests.sh\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
                 exit 6\n\
               fi\n\
               printf '{{\"verdict\":\"accepted\",\"base_check\":\"check-tests.sh\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
               exit 0\n\
             fi\n\
             echo '| == regression: check-tests.sh must still pass'\n\
             printf '{{\"host_fault\":true,\"scenario\":\"check-tests.sh\",\"on\":\"main\",\"runs\":\"1, then 1 on the candidate; 1, then 1 on the base\",\"reason\":\"r\",\"failures\":[{{\"test\":\"api::tests::adds\",\"message\":\"assertion `left == right` failed\"}}]}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
             exit 6\n"
    );
    let (base, source, scenarios, mut integrator) = world("main-red", &gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_secs(1),
        cap: 1,
    };
    let db = Db::new("main_red");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    let main = source_main(&source);
    let gates = || {
        std::fs::read_to_string(&runs)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect::<Vec<String>>()
    };
    let pass = |integrator: &Integrator| {
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
    };

    // The first gate finds main red: one red for the source, the task's
    // host_fault with no run, no backoff, no count.
    pass(&integrator);
    assert_eq!(gates(), ["base_check="], "no red yet, so no base check");
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["verdict"], "host_fault", "{record}");
    assert_eq!(record["evidence"], super::MAIN_RED, "{record}");
    assert_eq!(record["fact"]["on"], "main");
    assert_eq!(record["fact"]["main"], main.as_str());
    assert_eq!(record["backoff_ms"], 0);
    assert!(record.get("probe").is_none(), "{record}");
    let reds = control_records(&db, "source.main_red");
    assert_eq!(reds.len(), 1);
    assert_eq!(reds[0]["alert"], true, "the first of the red: {}", reds[0]);
    assert_eq!(reds[0]["scenario"], "check-tests.sh");
    assert_eq!(reds[0]["failures"][0]["test"], "api::tests::adds");
    assert_eq!(reds[0]["failures"][0]["class"], "work");
    let after = gated(&db, mission);
    let red = after.red.clone().expect("the source is red");
    assert_eq!(
        (red.main.as_str(), red.scenario.as_str()),
        (main.as_str(), "check-tests.sh")
    );
    assert_eq!(
        (after.gate_host_faults, after.gate_aborts),
        (0, 0),
        "no run, no count"
    );
    assert_eq!(
        db.store
            .task_host_faults(task, 0, &source_main(&source))
            .expect("count"),
        0
    );

    // Told once: the red's alert.
    let news = crate::dispatch::digest::main_red_news(
        db.store
            .main_red_news()
            .expect("news")
            .as_array()
            .expect("records"),
    );
    assert_eq!(news.len(), 1, "{news:?}");
    assert_eq!(news[0].why, crate::dispatch::Why::MainRed);
    assert!(
        news[0].line.contains("is red on check-tests.sh")
            && news[0].line.contains("api::tests::adds"),
        "{}",
        news[0].line
    );

    // The red main holds the source's gates: no gate, no record.
    let verdicts = control_records(&db, "task.gated").len();
    pass(&integrator);
    assert_eq!(gates().len(), 1, "held: no gate ran");
    assert_eq!(control_records(&db, "task.gated").len(), verdicts);

    // Past the probe interval: one gate anyway, a probe, given the red
    // check; still red, so a second red record, no alert, still no count.
    integrator.red_probe_ms = 1;
    std::thread::sleep(std::time::Duration::from_millis(5));
    // A probe whose gate does not start this pass (no CPU room: another
    // source's gate holds the cores) is not spent: no probe record, no
    // gate.
    let (release, waiting) = std::sync::mpsc::channel::<()>();
    integrator.running.borrow_mut().push(super::Running {
        mission: 0,
        source: "another source".to_string(),
        task: 0,
        shares_server: false,
        jobs: 2,
        owner: None,
        stop: std::sync::Arc::new(super::Stop::default()),
        halt: std::sync::Arc::new(crate::gate::Halt::default()),
        judging: String::new(),
        handle: std::thread::spawn(move || {
            let _ = waiting.recv();
        }),
    });
    integrator.cpu = Some(crate::gate::Cpu {
        cores: 2,
        reserved: 1,
        jobs: Some(1),
    });
    // No settle: it would wait for the holding thread.
    integrator.pass_on(&db.store).expect("a pass");
    assert_eq!(gates().len(), 1, "no CPU room: no probe gate");
    assert!(
        control_records(&db, "source.main_probe").is_empty(),
        "no probe spent"
    );
    integrator.cpu = None;
    drop(release);
    integrator.settle();
    pass(&integrator);
    assert_eq!(gates(), ["base_check=", "base_check=check-tests.sh"]);
    assert_eq!(control_records(&db, "source.main_probe").len(), 1);
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["probe"], true, "{record}");
    assert_eq!(
        record["verdict"], "host_fault",
        "the cap of 1 never caps it: {record}"
    );
    let reds = control_records(&db, "source.main_red");
    assert_eq!(reds.len(), 2);
    assert_eq!(reds[1]["alert"], false, "{}", reds[1]);
    assert_eq!(gated(&db, mission).gate_aborts, 0);
    // The store takes one probe per interval: none is due an hour after
    // the last.
    assert!(
        db.store
            .integrator_record(&json!({"type": "source.main_probe", "source": source,
                                       "main": main, "task": task, "probe_ms": 3_600_000}))
            .is_err()
    );
    // A red that names no failure is refused.
    assert!(
        db.store
            .integrator_record(&json!({"type": "source.main_red", "source": source,
                                       "main": main, "scenario": "check-tests.sh",
                                       "failures": [], "task": task}))
            .is_err()
    );

    // The red check passes on the base, and the gate then ends with a
    // host fault on another check: green again all the same (the gate
    // says it in any fact), and the fault recorded as usual.
    std::fs::write(&green, "fault").expect("green");
    std::thread::sleep(std::time::Duration::from_millis(5));
    pass(&integrator);
    assert_eq!(gates().len(), 3);
    let greens = control_records(&db, "source.main_green");
    assert_eq!(greens.len(), 1);
    assert_eq!(greens[0]["scenario"], "check-tests.sh");
    assert!(
        greens[0]["since_ms"].is_i64(),
        "the red's since: {}",
        greens[0]
    );
    assert!(gated(&db, mission).red.is_none(), "green: no red");
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["verdict"], "host_fault", "{record}");
    assert_eq!(record["fact"]["scenario"], "check-e2e.sh", "{record}");
    assert_eq!(
        record["probe"], true,
        "the gate that found it green was a probe"
    );
    // Green, the source's gates run as usual: after the fault's backoff,
    // the work is judged and accepted, no probe and no base check.
    std::fs::write(&green, "").expect("green");
    if let Some(due) = record["due_ms"].as_i64() {
        let left = due - super::now_ms();
        if left > 0 {
            std::thread::sleep(std::time::Duration::from_millis(
                u64::try_from(left).unwrap_or(0) + 50,
            ));
        }
    }
    pass(&integrator);
    assert_eq!(
        gates().last().map(String::as_str),
        Some("base_check="),
        "{:?}",
        gates()
    );
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["verdict"], "accepted", "{record}");
    assert!(record.get("probe").is_none(), "{record}");
    let news = crate::dispatch::digest::main_red_news(
        db.store
            .main_red_news()
            .expect("news")
            .as_array()
            .expect("records"),
    );
    assert!(
        news.iter()
            .any(|item| item.why == crate::dispatch::Why::MainGreen
                && item.line.contains("passes check-tests.sh again")),
        "{news:?}"
    );
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Objective 033's evidence beside the cap (this objective's): a base
/// that disagreed with itself is a base-only fault, before any candidate
/// code, so it counts toward no halt and never reaches
/// `SPAWN_GATE_HOST_FAULT_CAP`: with the cap at 2, four disagreements in
/// a row are `host_fault` every time, `base_disagrees` every time, and
/// the task never aborts (the store's count leaves them out too).
#[test]
fn base_disagreements_never_reach_the_cap() {
    let gate = "echo '| == base: 011.sh'\n\
                    echo '| 011.sh exited 1, then 0 on the base: a harness error, not a verdict; the gate aborts'\n\
                    echo '| stage gate aborted 12003ms base: 011.sh again (unsolved needs two runs that agree)'\n\
                    exit 4\n";
    let (base, source, scenarios, mut integrator) = world("base-disagrees-cap", gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_secs(1),
        cap: 2,
    };
    let db = Db::new("base_disagrees_cap");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    let mut due = None;
    for round in 0..4 {
        if let Some(due) = due {
            let left = due - super::now_ms();
            if left > 0 {
                std::thread::sleep(std::time::Duration::from_millis(
                    u64::try_from(left).unwrap_or(0) + 50,
                ));
            }
        }
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        let record = last_verdict(&db).0;
        assert_eq!(record["verdict"], "host_fault", "round {round}: {record}");
        assert_eq!(record["evidence"], "base_disagrees", "{record}");
        due = record["due_ms"].as_i64();
    }
    assert_eq!(
        db.store
            .task_host_faults(task, 0, &source_main(&source))
            .expect("count"),
        0,
        "a base's disagreement is not counted"
    );
    let after = gated(&db, mission);
    assert_eq!(after.gate_aborts, 0);
    assert_eq!(after.gate_host_faults, 4, "each one a run");
    assert_eq!(after.gate_base_disagrees, 4, "each one a disagreement");
    let _ = std::fs::remove_dir_all(&base);
}

/// Objective 20 beside main's `no-test-database`: a mission whose gate
/// needs no test database has no host faults. Its check-tests.sh
/// failures, connection errors though they are, stay a rejection.
#[test]
fn a_mission_without_the_test_database_has_no_host_faults() {
    let gate = "echo '| == regression: check-tests.sh must still pass'\n\
                    echo '| FAILED api::tests::one: error connecting to server: Connection refused (os error 111)'\n\
                    echo '| stage gate rejected 5ms regression: check-tests.sh must still pass'\n\
                    printf '{\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the candidate\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
    let (base, source, scenarios, integrator) = world("host-fault-no-db", gate);
    std::fs::write(scenarios.join(super::NO_TEST_DATABASE), "").expect("the marker");
    let db = Db::new("host_fault_no_db");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["id"], task);
    assert_eq!(record["verdict"], "rejected", "{record}");
    let after = gated(&db, mission);
    assert_eq!((after.gate_rejections, after.gate_host_faults), (1, 0));
    let _ = std::fs::remove_dir_all(&base);
}

/// Objective 33, end to end: a gate that exits 4 after its own pair of
/// lines (a scenario of the base exiting 1 and then 0) is recorded
/// `host_fault` with the evidence `base_disagrees`, the gate's own
/// scenario (not a nested gate's 102.sh), both exits and its setup
/// seconds; the task stays reported and is not judged again before the
/// record's `due_ms`; three in a row double the backoff, share 020's run,
/// never escalate and carry the alert at the third; a base run that
/// exits 2, or `1, then 2`, is an abort and resets both runs; the
/// accepted run lands and the mission is never halted.
#[test]
#[allow(clippy::too_many_lines)]
fn a_base_that_disagrees_with_itself_is_a_host_fault_judged_again() {
    let dir = std::env::temp_dir().join(format!("spawn-db-base-disagrees-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let runs = dir.join("runs");
    let runs_path = runs.display();
    let template = r#"
n=$(cat 'RUNS' 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" >'RUNS'
disagrees() {
  echo '| == base: 011.sh'
  echo '| stage gate exit 1 74210ms base: 011.sh'
  echo '| == base: 011.sh again (unsolved needs two runs that agree)'
  if [ "$2" = nested ]; then
    echo '| | 102.sh exited 1, then 0 on the base: a harness error, not a verdict; the gate aborts'
    echo '| | stage gate aborted 5ms base: 102.sh again (unsolved needs two runs that agree)'
  fi
  echo '| 011.sh exited 1, then 0 on the base: a harness error, not a verdict; the gate aborts'
  echo '| stage gate aborted 66871ms base: 011.sh again (unsolved needs two runs that agree)'
  echo "| gate time: base 141 s (2 scenario runs, 19 results from the cache), candidate 0 s, setup $1 s (exports, socket scan, static checks), total 385 s"
  exit 4
}
case "$n" in
1) disagrees 244 nested ;;
2) disagrees 31 ;;
3) disagrees 29 ;;
4)
  echo '| == base: 011.sh'
  echo '| 011.sh exited 2 on the base: a harness error, not a verdict; the gate aborts'
  echo '| stage gate aborted 5120ms base: 011.sh again (unsolved needs two runs that agree)'
  exit 4
  ;;
5)
  echo '| == base: 011.sh'
  echo '| 011.sh exited 1, then 2 on the base: a harness error, not a verdict; the gate aborts'
  echo '| stage gate aborted 12003ms base: 011.sh again (unsolved needs two runs that agree)'
  exit 4
  ;;
esac
printf '{"verdict":"accepted"}\n' >"$SPAWN_GATE_VERDICT"
"#;
    let gate = template.replace("RUNS", &runs_path.to_string());
    let (base, source, scenarios, mut integrator) = world("base-disagrees-pass", &gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_secs(2),
        cap: super::HOST_FAULT_CAP,
    };
    let db = Db::new("base_disagrees_pass");
    let mission = db.gated_mission(&source, &scenarios);
    let _task = db.reported_task(mission, "w");
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    push_work(&base, &source, &agent, "work.txt");
    let count =
        || std::fs::read_to_string(&runs).map_or(0, |text| text.trim().parse::<u32>().unwrap_or(0));
    let pass = || {
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
    };
    let wait_until = |due: i64| {
        let left = due - super::now_ms();
        if left > 0 {
            std::thread::sleep(std::time::Duration::from_millis(
                u64::try_from(left).unwrap_or(0) + 50,
            ));
        }
    };
    let state = |db: &Db| {
        let after = gated(db, mission);
        (after.tasks[0].state, after.tasks[0].revisions)
    };

    // (1) Run 1: the base disagreed with itself.
    pass();
    assert_eq!(count(), 1);
    let (record, event, at) = last_verdict(&db);
    assert_eq!(record["verdict"], "host_fault", "{record}");
    assert_eq!(record["evidence"], "base_disagrees");
    assert_eq!(
        record["scenario"], "011.sh",
        "the gate's own line, not 102.sh"
    );
    assert_eq!(record["exits"], json!([1, 0]));
    assert_eq!(record["setup_s"], 244);
    assert_eq!(record["host_faults"], 1);
    assert_eq!(record["base_disagrees"], 1);
    assert_eq!(record["escalate"], false);
    assert_eq!(record["alert"], false);
    assert_eq!(record["backoff_ms"], 2000);
    assert_eq!(record["due_ms"], at + 2000, "its own time plus the backoff");
    assert!(record["candidate"].is_string());
    assert!(
        record["log"].as_str().is_some_and(|log| {
            log.contains(
                "| 011.sh exited 1, then 0 on the base: a harness error, not a verdict; the \
                     gate aborts",
            ) && log.contains("| | 102.sh exited 1, then 0")
        }),
        "{record}"
    );
    assert!(record["reason"].as_str().is_some_and(|reason| {
        reason.starts_with("host fault: the base disagrees with itself: 011.sh exited 1, then 0")
            && reason.contains("244 s")
            && reason.ends_with("not a verdict, judged again later")
    }));
    let after = gated(&db, mission);
    assert_eq!((after.gate_host_faults, after.gate_base_disagrees), (1, 1));
    assert_eq!((after.gate_rejections, after.gate_aborts), (0, 0));
    assert_eq!(
        state(&db),
        (spawn_core::mission::TaskState::Reported, 0),
        "nothing went back to the worker"
    );
    assert!(after.halted.is_none());
    assert_eq!(
        db.store
            .gate_base_disagrees_alerts()
            .expect("alerts")
            .as_array()
            .map(Vec::len),
        Some(0),
        "the first in a row is no alert"
    );

    // (2) Held until its due time, then judged again.
    pass();
    assert_eq!(count(), 1, "not judged again before due_ms");
    assert_eq!(last_verdict(&db).1, event);
    wait_until(at + 2000);

    // (3) Run 2: the second in a row, the backoff doubled, no escalation.
    pass();
    assert_eq!(count(), 2);
    let (record, _, at) = last_verdict(&db);
    assert_eq!(record["verdict"], "host_fault", "{record}");
    assert_eq!(record["host_faults"], 2);
    assert_eq!(record["base_disagrees"], 2);
    assert_eq!(record["setup_s"], 31);
    assert_eq!(record["backoff_ms"], 4000);
    assert_eq!(record["due_ms"], at + 4000);
    assert_eq!(record["escalate"], false, "never, whatever the count");
    assert_eq!(record["alert"], false);
    assert_eq!(gated(&db, mission).gate_aborts, 0);
    assert_eq!(
        db.store
            .gate_host_fault_escalations()
            .expect("escalations")
            .as_array()
            .map(Vec::len),
        Some(0),
        "a base disagreement is no escalation"
    );

    // (4) Run 3: three in a row, the alert, and no halt.
    pass();
    assert_eq!(count(), 2, "held again, for the doubled backoff");
    wait_until(at + 4000);
    pass();
    let (record, alert_event, at) = last_verdict(&db);
    assert_eq!(record["verdict"], "host_fault", "{record}");
    assert_eq!(record["host_faults"], 3);
    assert_eq!(record["base_disagrees"], 3);
    assert_eq!(record["backoff_ms"], 8000);
    assert_eq!(record["escalate"], false);
    assert_eq!(record["alert"], true, "three in a row raise one alert");
    let after = gated(&db, mission);
    assert_eq!((after.gate_host_faults, after.gate_base_disagrees), (3, 3));
    assert!(after.halted.is_none(), "no halt counts a host fault");
    assert_eq!(after.gate_aborts, 0);
    let alerts = db.store.gate_base_disagrees_alerts().expect("alerts");
    assert_eq!(alerts.as_array().map(Vec::len), Some(1), "{alerts}");
    assert_eq!(alerts[0]["event_id"], alert_event);
    assert_eq!(alerts[0]["scenario"], "011.sh");

    // (5) Control: a real harness error on the base is an abort, counts,
    // and ends both runs.
    wait_until(at + 8000);
    pass();
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["verdict"], "aborted", "{record}");
    let after = gated(&db, mission);
    assert_eq!(
        (
            after.gate_aborts,
            after.gate_host_faults,
            after.gate_base_disagrees
        ),
        (1, 0, 0),
        "a base run that exited 2 is an abort and resets both runs"
    );
    pass();
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["verdict"], "aborted", "1, then 2 is an abort too");
    assert_eq!(gated(&db, mission).gate_aborts, 2);
    assert!(gated(&db, mission).halted.is_none());

    // Run 6 accepts and lands, ending both runs.
    pass();
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["verdict"], "accepted", "{record}");
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
    let after = gated(&db, mission);
    assert_eq!(
        (
            after.gate_host_faults,
            after.gate_base_disagrees,
            after.gate_aborts
        ),
        (0, 0, 0)
    );
    assert!(after.halted.is_none());
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&dir);
}

// --- the owner's hold (objective 034) --------------------------------

/// A stub gate that accepts every candidate.
const ACCEPTING_GATE: &str = "printf '{\"verdict\":\"accepted\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";

/// Hold `source` for `bound` seconds as `spawn mission hold` does: the
/// hold's event id.
fn hold(db: &Db, source: &str, bound: i32) -> i64 {
    crate::missions::hold_source(&db.store, source, bound, Some("owner gate test"))
        .expect("the hold")["hold"]
        .as_i64()
        .expect("the hold's event id")
}

/// The event ids of the control records of type `kind` naming task
/// `task` (`id`), oldest first.
fn task_records(db: &Db, kind: &str, task: i64) -> Vec<i64> {
    db.admin()
        .query(
            "select id from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = $1
                    and kind -> 'detail' -> 'record' ->> 'id' = $2
                  order by session_seq",
            &[&kind, &task.to_string()],
        )
        .expect("records")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

/// The control records of type `kind` (`source.%`), oldest first, as
/// `(event id, record)`.
fn source_records(db: &Db, kind: &str) -> Vec<(i64, Value)> {
    db.admin()
        .query(
            "select id, kind -> 'detail' -> 'record' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = $1
                  order by session_seq",
            &[&kind],
        )
        .expect("records")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

fn task_state(db: &Db, mission: i64, task: i64) -> String {
    db.mission(mission)["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .find(|row| row["id"] == task)
        .and_then(|row| row["state"].as_str())
        .expect("the task's state")
        .to_string()
}

/// `land()` on a held source pushes nothing: it records one `task.held`
/// naming the hold (a second call, and the passes after, record none)
/// and leaves the task `landing`.
#[test]
fn a_landing_on_a_held_source_pushes_nothing_and_waits_once_per_hold() {
    let (base, source, scenarios, integrator) = world("held-landing", "exit 1\n");
    let db = Db::new("held_landing");
    let b0 = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "l");
    let work = push_work(&base, &source, &agent_of(&db, mission, task), "w.txt");
    db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                      "verdict": "accepted", "base": b0, "candidate": work}));
    let held = hold(&db, &source, 600);
    let mirror =
        crate::sources::ensure_mirror(&integrator.judge.root, Some(&source)).expect("mirror");
    let landing = ready_task(&db, task);
    for _ in 0..2 {
        assert_eq!(
            super::land(&db.store, &mirror, &source, &landing),
            Ok(super::Landing::Held(held))
        );
    }
    integrator.pass_on(&db.store).expect("a pass");
    assert_eq!(source_main(&source), b0, "nothing was pushed");
    assert_eq!(
        task_records(&db, "task.held", task).len(),
        1,
        "once per task and hold"
    );
    assert!(task_records(&db, "task.landed", task).is_empty());
    assert_eq!(task_state(&db, mission, task), "landing");
    assert!(db.mission(mission)["halted"].is_null());
    assert_eq!(db.mission(mission)["hold"], held);
    let _ = std::fs::remove_dir_all(&base);
}

/// A candidate already in the source's main (a push from before the hold
/// whose record was lost) landed before the hold: recorded `task.landed`
/// under the hold, as without one, and main is left where it is.
#[test]
fn a_landing_already_under_main_is_recorded_landed_under_a_hold() {
    let (base, source, scenarios, integrator) = world("held-under-main", "exit 1\n");
    let db = Db::new("held_under_main");
    let b0 = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "l");
    let c1 = main_moves(&base, &source, "landed.txt");
    let c2 = main_moves(&base, &source, "on-top.txt");
    db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                      "verdict": "accepted", "base": b0, "candidate": c1}));
    hold(&db, &source, 600);
    crate::sources::ensure_mirror(&integrator.judge.root, Some(&source)).expect("mirror");

    integrator.pass_on(&db.store).expect("a pass");
    assert_eq!(task_state(&db, mission, task), "landed");
    assert!(task_records(&db, "task.held", task).is_empty());
    assert_eq!(db.mission(mission)["baseline"], c1.as_str());
    assert_eq!(source_main(&source), c2, "main is left where it is");
    let _ = std::fs::remove_dir_all(&base);
}

/// The hold is taken under the landing lock: while a landing holds it
/// (its compare-and-swap push begun), the hold waits for the landing's
/// record, then names the main that landing left.
#[test]
fn a_hold_taken_while_a_landing_holds_the_lock_waits_for_its_record() {
    let (base, source, scenarios, _integrator) = world("hold-waits", "exit 1\n");
    let db = Db::new("hold_waits");
    let b0 = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "l");
    db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                      "verdict": "accepted", "base": b0, "candidate": "c"}));
    let owner = PostgresStore::connect_in_schema(&db.url, &db.schema).expect("the owner's CLI");
    let lock = db
        .store
        .landing_hold(task)
        .expect("the landing lock")
        .expect("still landing");
    let held_source = source.clone();
    let holder =
        std::thread::spawn(move || crate::missions::hold_source(&owner, &held_source, 600, None));
    std::thread::sleep(std::time::Duration::from_millis(700));
    assert!(
        !holder.is_finished(),
        "the hold waits for the landing's lock"
    );
    let c1 = main_moves(&base, &source, "landed.txt");
    db.record(&json!({"type": "task.landed", "id": task, "from": b0, "to": c1}));
    drop(lock);
    let held = holder.join().expect("the holder").expect("the hold");
    assert_eq!(held["main"], c1.as_str(), "the main the landing left");
    let landed = task_records(&db, "task.landed", task);
    assert!(
        held["hold"].as_i64() > landed.first().copied(),
        "the landing's record comes first: {held} after {landed:?}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A task waiting behind the hold ends nothing early: its mission's step
/// goes on to assign its next objective, and its source is not counted
/// as moved. Once its `task.held` names the active hold, another mission
/// on the held source is judged as usual (its gate runs and records its
/// verdict), and its accepted task waits behind the hold too.
#[test]
fn a_task_waiting_behind_a_hold_ends_nothing_early() {
    let marks = fresh_marks("spawn-db-held-step");
    let _release = Release(marks.clone());
    let (base, source, scenarios, integrator) = world("held-step", &holding_gate(&marks));
    let db = Db::new("held_step");
    let b0 = source_main(&source);
    let a = objective_mission(&db, &base, &source, &scenarios, "rh", 2);
    integrator
        .pass_on(&db.store)
        .expect("objective 1 is assigned");
    let tasks = |mission: i64| db.mission(mission)["tasks"].as_array().map_or(0, Vec::len);
    assert_eq!(tasks(a), 1);
    let first = db.mission(a)["tasks"][0]["id"].as_i64().expect("its task");
    db.store.task_delivered(first, 1).expect("delivered");
    assert!(db.store.task_reported(first, "done").expect("reported"));
    let work = push_work(&base, &source, &agent_of(&db, a, first), "a.txt");
    db.record(&json!({"type": "task.gated", "id": first, "revision": 0,
                      "verdict": "accepted", "base": b0, "candidate": work}));
    let c = db.gated_mission(&source, &scenarios);
    let ready = db.reported_task(c, "c");
    let pushed = push_work(&base, &source, &agent_of(&db, c, ready), "c.txt");
    let held = hold(&db, &source, 600);
    std::fs::write(
        base.join("backlog-rh").join("002.json"),
        r#"{"id": 2, "title": "Two", "prompt": "Do two."}"#,
    )
    .expect("objective 2");

    integrator
        .pass_on(&db.store)
        .expect("a pass under the hold");
    assert_eq!(
        tasks(a),
        2,
        "objective 2 is assigned while the source is held"
    );
    assert_eq!(task_records(&db, "task.held", first).len(), 1);
    assert!(
        started(&marks).is_empty(),
        "C waited for A's task, not held yet when read"
    );
    integrator.pass_on(&db.store).expect("the next pass");
    let started = wait_started(&marks, 1);
    let mirror = mirror_of(&integrator, &source);
    assert_eq!(
        parent(&[&mirror], &started[0], 2),
        pushed,
        "C's gate runs on the held source: A's waiting task is no landing in flight and \
             its source was not counted as moved"
    );
    std::fs::write(marks.join("release"), "").expect("release");
    integrator.settle();
    assert_eq!(
        verdicts(&db, ready),
        ["accepted"],
        "the gate on a held source recorded"
    );
    assert_eq!(task_state(&db, c, ready), "landing");
    assert_eq!(
        task_records(&db, "task.held", ready).len(),
        1,
        "its judging's land()"
    );
    assert_eq!(task_state(&db, a, first), "landing");
    assert_eq!(
        source_main(&source),
        b0,
        "nothing landed on the held source"
    );
    for mission in [a, c] {
        assert!(db.mission(mission)["halted"].is_null());
        assert_eq!(db.mission(mission)["hold"], held);
    }
    for base in [base, marks] {
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// After the release a waiting task lands at the next pass, after the
/// release's record. When main moved meanwhile (an owner deploy that
/// recorded `source.moved`), its landing is `stale` and it is judged
/// again against the new main: never discarded, never counted, no halt.
#[test]
fn after_the_release_a_waiting_task_lands_or_is_judged_again_on_a_moved_main() {
    let (base, source, scenarios, integrator) = world("held-release", ACCEPTING_GATE);
    let db = Db::new("held_release");
    let b0 = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    let first = db.reported_task(mission, "one");
    push_work(&base, &source, &agent_of(&db, mission, first), "one.txt");
    hold(&db, &source, 600);
    integrator
        .pass_on(&db.store)
        .expect("judged under the hold");
    integrator.settle();
    assert_eq!(verdicts(&db, first), ["accepted"]);
    integrator
        .pass_on(&db.store)
        .expect("a pass under the hold");
    assert_eq!(task_state(&db, mission, first), "landing");
    assert_eq!(task_records(&db, "task.held", first).len(), 1);
    assert_eq!(source_main(&source), b0);
    let released = db.store.source_release(&source).expect("the release");
    assert_eq!(released["waiting"], json!([first]));
    integrator
        .pass_on(&db.store)
        .expect("the pass after the release");
    assert_eq!(task_state(&db, mission, first), "landed");
    let (release, _) = source_records(&db, "source.released")[0].clone();
    assert!(task_records(&db, "task.landed", first)[0] > release);
    let landed = source_main(&source);
    assert_ne!(landed, b0);

    // Held again; the owner moves main and records it; then the release.
    let second = db.reported_task(mission, "two");
    push_work(&base, &source, &agent_of(&db, mission, second), "two.txt");
    hold(&db, &source, 600);
    integrator
        .pass_on(&db.store)
        .expect("judged under the hold");
    integrator.settle();
    assert_eq!(task_state(&db, mission, second), "landing");
    let deployed = main_moves(&base, &source, "deploy.txt");
    assert_eq!(
        db.store
            .source_moved(&source, &landed, &deployed)
            .expect("moved"),
        1
    );
    db.store.source_release(&source).expect("the release");
    integrator
        .pass_on(&db.store)
        .expect("its landing finds main moved");
    assert_eq!(verdicts(&db, second), ["accepted", "stale"]);
    assert_eq!(
        task_state(&db, mission, second),
        "reported",
        "judged again, not discarded"
    );
    integrator
        .pass_on(&db.store)
        .expect("judged again on the new main");
    integrator.settle();
    assert_eq!(verdicts(&db, second), ["accepted", "stale", "accepted"]);
    assert_eq!(task_state(&db, mission, second), "landed");
    assert_eq!(parent(&[&source], &source_main(&source), 1), deployed);
    let after = gated(&db, mission);
    assert_eq!(after.halted, None);
    assert_eq!(
        (after.gate_rejections, after.gate_aborts),
        (0, 0),
        "nothing counted"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A hold past its bound is recorded expired at the start of a pass,
/// before anything lands, once; the task waiting behind it is named in
/// the record and lands after it.
#[test]
fn an_expired_hold_is_recorded_before_anything_lands_once() {
    let (base, source, scenarios, integrator) = world("held-expired", "exit 1\n");
    let db = Db::new("held_expired");
    let b0 = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "l");
    let work = push_work(&base, &source, &agent_of(&db, mission, task), "w.txt");
    db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                      "verdict": "accepted", "base": b0, "candidate": work}));
    let held = hold(&db, &source, 1);
    integrator
        .pass_on(&db.store)
        .expect("a pass under the hold");
    assert_eq!(task_state(&db, mission, task), "landing");
    std::thread::sleep(std::time::Duration::from_millis(1200));
    integrator
        .pass_on(&db.store)
        .expect("the pass after its bound");
    let expired = source_records(&db, "source.hold_expired");
    assert_eq!(expired.len(), 1);
    let (event, record) = &expired[0];
    assert_eq!(record["hold"], held);
    assert_eq!(record["source"], source.as_str());
    assert_eq!(record["waiting"], json!([task]));
    assert_eq!(task_state(&db, mission, task), "landed");
    assert!(
        task_records(&db, "task.landed", task)[0] > *event,
        "the expiry first"
    );
    assert_eq!(source_main(&source), work);
    integrator.pass_on(&db.store).expect("another pass");
    assert_eq!(source_records(&db, "source.hold_expired").len(), 1, "once");
    let _ = std::fs::remove_dir_all(&base);
}

// Objective 21: the owner's landings (`spawn mission push`), step 0.

/// A gated mission on `source` whose baseline is `baseline` (its first
/// verdict's base), with nothing left to judge.
fn mission_at_baseline(db: &Db, source: &str, scenarios: &Path, baseline: &str) -> i64 {
    let mission = db.gated_mission(source, scenarios);
    let task = db.reported_task(mission, "w");
    db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                      "verdict": "aborted", "base": baseline, "reason": "a gate that died"}));
    db.store.task_cancel(task).expect("nothing left to judge");
    assert_eq!(db.mission(mission)["baseline"], baseline);
    mission
}

/// The owner's commit on `source`'s main as it is now (`file` holding
/// `text`, subject `subject`), put in the source at
/// `refs/spawn-owner/<commit>` as `spawn mission push` does: no branch
/// moves. The commit.
fn owner_commit(base: &Path, source: &str, file: &str, text: &str, subject: &str) -> String {
    let clone = base.join(format!("owner-{file}-{}", text.len()));
    let clone_path = clone.to_string_lossy().to_string();
    let _ = std::fs::remove_dir_all(&clone);
    git(&["clone", "-q", source, &clone_path]).expect("the owner's clone");
    std::fs::write(clone.join(file), text).expect("write");
    super::tests::commit_in(&clone_path, subject);
    let commit = git(&["-C", &clone_path, "rev-parse", "HEAD"]).expect("the commit");
    let reference = super::owner::owner_ref(&commit);
    git(&[
        "-C",
        &clone_path,
        "push",
        "-q",
        "origin",
        &format!("{commit}:{reference}"),
    ])
    .expect("into the source, no branch");
    commit
}

/// The owner's request for `commit` on `source`: its event id.
fn owner_request(db: &Db, source: &str, commit: &str) -> i64 {
    db.store
        .source_push_request(
            source,
            commit,
            &super::owner::owner_ref(commit),
            &spawn_postgres::PushRequest::default(),
        )
        .expect("the owner's request")
}

/// The control records of `kind`, oldest first.
fn control_records(db: &Db, kind: &str) -> Vec<Value> {
    db.admin()
        .query(
            "select kind -> 'detail' -> 'record' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = $1
                  order by session_seq",
            &[&kind],
        )
        .expect("records")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

fn halted(db: &Db, mission: i64) -> Value {
    db.mission(mission)["halted"].clone()
}

/// A pending request is applied before the baseline check: main moves
/// from where the last landing left it to the owner's commit (a
/// fast-forward), one `source.moved` names the request with `from` and
/// `to`, the mission's baseline follows, and no pass halts it, although
/// the integrator's mirror still holds the old main when the pass begins.
#[test]
fn an_owner_landing_is_applied_before_the_baseline_check() {
    let (base, source, scenarios, integrator) = world("owner-ff", "exit 1\n");
    let db = Db::new("owner_ff");
    let b0 = source_main(&source);
    let mirror = mirror_of(&integrator, &source);
    let mission = mission_at_baseline(&db, &source, &scenarios, &b0);
    let commit = owner_commit(&base, &source, "CONTRACT.md", "v1\n", "owner: the contract");
    assert_eq!(source_main(&source), b0, "the request moves no branch");
    let request = owner_request(&db, &source, &commit);

    integrator.pass_on(&db.store).expect("a pass");
    assert_eq!(source_main(&source), commit, "a fast-forward to the commit");
    let moved = control_records(&db, "source.moved");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["request"], json!(request));
    assert_eq!(moved[0]["source"], source.as_str());
    assert_eq!(moved[0]["from"], b0.as_str());
    assert_eq!(moved[0]["to"], commit.as_str());
    assert_eq!(moved[0]["by"], "human");
    assert_eq!(moved[0]["missions"], json!([mission]));
    assert_eq!(db.mission(mission)["baseline"], commit.as_str());
    assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));
    assert_eq!(
        git(&["--git-dir", &mirror, "rev-parse", "refs/heads/main"]).expect("mirror main"),
        commit,
        "the mission's step read the new main after it moved"
    );
    assert_eq!(
        db.store.source_pushes_pending().expect("pending"),
        json!([])
    );

    for _ in 0..2 {
        integrator.pass_on(&db.store).expect("another pass");
    }
    assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));
    assert_eq!(
        control_records(&db, "source.moved").len(),
        1,
        "applied once"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Main moved (a recorded move) since the owner's commit's base: the
/// landing is a merge commit, first parent main, second the commit, by
/// the landing identity, its message naming the owner's landing, the
/// request and the commit's subject; main moves to it, and the
/// `source.moved` naming the request moves the baseline. Nothing halts.
#[test]
fn an_owner_landing_on_a_main_that_moved_is_a_merge() {
    let (base, source, scenarios, integrator) = world("owner-merge", "exit 1\n");
    let db = Db::new("owner_merge");
    let b0 = source_main(&source);
    let mission = mission_at_baseline(&db, &source, &scenarios, &b0);
    let commit = owner_commit(&base, &source, "CONTRACT.md", "v1\n", "owner: the contract");
    let b1 = main_moves(&base, &source, "landed.txt");
    assert_eq!(db.store.source_moved(&source, &b0, &b1).expect("moved"), 1);
    let request = owner_request(&db, &source, &commit);

    integrator.pass_on(&db.store).expect("a pass");
    let main = source_main(&source);
    assert_eq!(parent(&[&source], &main, 1), b1, "first parent: main");
    assert_eq!(parent(&[&source], &main, 2), commit, "second: the commit");
    for file in ["CONTRACT.md", "landed.txt", "README.md"] {
        git(&[
            "--git-dir",
            &source,
            "cat-file",
            "-e",
            &format!("{main}:{file}"),
        ])
        .unwrap_or_else(|error| panic!("{file} in the merge: {error}"));
    }
    let message = git(&["--git-dir", &source, "log", "-1", "--format=%B", &main]).expect("log");
    assert!(
        message.starts_with(&format!(
            "Owner landing (request {request}): owner: the contract"
        )),
        "{message}"
    );
    assert!(
        message.contains(&format!("Spawn-Owner-Request: {request}")),
        "{message}"
    );
    let who = git(&[
        "--git-dir",
        &source,
        "log",
        "-1",
        "--format=%an <%ae> %cn <%ce>",
        &main,
    ])
    .expect("log");
    assert_eq!(
        who,
        "spawn-integrate <integrate@localhost> spawn-integrate <integrate@localhost>"
    );
    let moved: Vec<Value> = control_records(&db, "source.moved")
        .into_iter()
        .filter(|record| record["request"] == json!(request))
        .collect();
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["from"], b1.as_str());
    assert_eq!(moved[0]["to"], main.as_str());
    assert_eq!(db.mission(mission)["baseline"], main.as_str());
    integrator.pass_on(&db.store).expect("another pass");
    assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));
    let _ = std::fs::remove_dir_all(&base);
}

/// A commit that conflicts with main is refused: `source.push_refused`
/// names the request and the reason, main and the baseline stay, no
/// `source.moved` names it, nothing halts, and the request is no longer
/// pending. So is a commit the source does not have.
#[test]
fn a_conflicting_owner_landing_is_refused_and_moves_nothing() {
    let (base, source, scenarios, integrator) = world("owner-conflict", "exit 1\n");
    let db = Db::new("owner_conflict");
    let b0 = source_main(&source);
    let mission = mission_at_baseline(&db, &source, &scenarios, &b0);
    let commit = owner_commit(
        &base,
        &source,
        "README.md",
        "the owner's\n",
        "owner: readme",
    );
    let b1 = main_moves(&base, &source, "README.md");
    assert_eq!(db.store.source_moved(&source, &b0, &b1).expect("moved"), 1);
    let request = owner_request(&db, &source, &commit);
    let missing = "0123456789abcdef0123456789abcdef01234567";
    let absent = owner_request(&db, &source, missing);

    integrator.pass_on(&db.store).expect("a pass");
    assert_eq!(source_main(&source), b1, "main stays");
    let refused = control_records(&db, "source.push_refused");
    assert_eq!(refused.len(), 2, "{refused:?}");
    assert_eq!(refused[0]["request"], json!(request));
    assert!(
        refused[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("conflicts")),
        "{}",
        refused[0]
    );
    assert_eq!(refused[1]["request"], json!(absent));
    assert!(
        refused[1]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("not in the source")),
        "{}",
        refused[1]
    );
    assert!(
        control_records(&db, "source.moved")
            .iter()
            .all(|record| record["request"].is_null()),
        "no move names a refused request"
    );
    assert_eq!(db.mission(mission)["baseline"], b1.as_str());
    integrator.pass_on(&db.store).expect("another pass");
    assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));
    assert_eq!(
        db.store.source_pushes_pending().expect("pending"),
        json!([])
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A pass that finds a request pending for a source (recorded after step
/// 0 read the requests, or one whose landing failed after main moved)
/// holds every halt by baseline for that source, and for no other:
/// both mains moved behind Spawn's back, only B halts. Then the request
/// is applied, and the move behind Spawn's back, which no record
/// explains, still halts A.
#[test]
fn a_pending_owner_landing_holds_the_baseline_halt_of_its_source_only() {
    let (base_a, source_a, scenarios, integrator) = world("owner-hold-a", "exit 1\n");
    let (base_b, source_b, _, _) = world("owner-hold-b", "exit 1\n");
    let db = Db::new("owner_hold");
    let a = mission_at_baseline(&db, &source_a, &scenarios, &source_main(&source_a));
    let b = mission_at_baseline(&db, &source_b, &scenarios, &source_main(&source_b));
    main_moves(&base_a, &source_a, "behind-a.txt");
    main_moves(&base_b, &source_b, "behind-b.txt");
    let commit = owner_commit(&base_a, &source_a, "CONTRACT.md", "v1\n", "owner: contract");
    let request = owner_request(&db, &source_a, &commit);

    let held = super::owner::pending_by_source(&db.store).expect("pending");
    assert_eq!(held.get(&source_a), Some(&request));
    assert_eq!(held.len(), 1);
    integrator
        .steps(&db.store, &held, &[], Vec::new())
        .expect("the steps");
    assert!(halted(&db, a).is_null(), "held: {}", halted(&db, a));
    assert!(
        halted(&db, b)
            .as_str()
            .is_some_and(|reason| reason.contains("but the last landing left")),
        "another source is not held: {}",
        halted(&db, b)
    );

    integrator.pass_on(&db.store).expect("a pass");
    assert_eq!(source_main(&source_a), commit, "applied");
    let moved = control_records(&db, "source.moved");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["missions"], json!([]), "A's baseline was not main");
    assert!(
        halted(&db, a)
            .as_str()
            .is_some_and(|reason| reason.contains("but the last landing left")),
        "a move no record explains still halts: {}",
        halted(&db, a)
    );
    for base in [base_a, base_b] {
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// Mission 4, 2026-11-08 09:23: task 182 (objective 34) was rejected
/// three times — "the work changes nothing on main" twice, then "scenario
/// already passes on the base" — because the owner batch had landed the
/// behaviour already, and the third halted the mission. A gate that says
/// the objective's scenario passes on the base is a fact about the
/// objective: it is recorded solved there, the task ends as a cancel,
/// no rejection is counted, and no task for it is assigned again.
#[test]
fn an_objective_solved_on_the_base_ends_its_task_without_a_rejection() {
    let gate = "echo '| == base: 023.sh'\n\
                    echo '| verdict rejected scenario already passes on the base'\n\
                    printf '{\"verdict\":\"rejected\",\"reason\":\"scenario already passes on the base\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
    let (base, source, scenarios, integrator) = world("solved-base", gate);
    let db = Db::new("solved_base");
    let mission = backlog_mission(&db, &base, "solved", &[23], 1, None);
    std::fs::write(scenarios.join("023.sh"), "exit 0\n").expect("the scenario");
    let none = std::collections::BTreeSet::new();
    crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("assign");
    let task = objective_task(&db, mission, 23);
    db.store.task_delivered(task.id, 1).expect("delivered");
    assert!(db.store.task_reported(task.id, "done").expect("reported"));
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    let base_commit = source_main(&source);
    push_work(&base, &source, &agent, "work.txt");

    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    let solved = control_records(&db, "objective.solved");
    assert_eq!(solved.len(), 1, "{solved:?}");
    assert_eq!(solved[0]["objective"], json!(23));
    assert_eq!(solved[0]["task"], json!(task.id));
    assert_eq!(solved[0]["base"], json!(base_commit));
    assert!(
        solved[0]["scenario"]
            .as_str()
            .is_some_and(|scenario| scenario.ends_with("023.sh")),
        "{}",
        solved[0]
    );
    let cancelled = control_records(&db, "task.cancelled");
    assert_eq!(cancelled.len(), 1, "{cancelled:?}");
    assert_eq!(cancelled[0]["id"], json!(task.id));
    assert!(cancelled[0]["solved"].is_number(), "{}", cancelled[0]);
    assert!(
        control_records(&db, "task.gated").is_empty(),
        "no rejection is recorded"
    );
    let after = gated(&db, mission);
    assert_eq!((after.gate_rejections, after.gate_aborts), (0, 0));
    assert!(db.mission(mission)["halted"].is_null());
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "cancelled");
    // No task for it again: the objective is a landing.
    crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none)
        .expect("the next pass");
    assert!(
        flying(&db, mission).is_empty(),
        "no task for a solved objective"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Objective 210 (GitHub #65): "scenario already passes on the base" ends
/// an objective's task as solved on the base whatever the backlog says:
/// the objective gone from it (renumbered), a malformed file in it (it
/// cannot be read at all), or the backlog gone. The one exception is an
/// objective the backlog positively reads as recurring: its task is
/// rejected and sent back, as before.
#[test]
fn an_objective_solved_on_the_base_is_solved_unless_the_backlog_reads_it_recurring() {
    let gate = "echo '| == base: 023.sh'\n\
                    echo '| verdict rejected scenario already passes on the base'\n\
                    printf '{\"verdict\":\"rejected\",\"reason\":\"scenario already passes on the base\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
    for case in ["absent", "malformed", "missing", "recurring"] {
        let (base, source, scenarios, integrator) = world(&format!("solved-backlog-{case}"), gate);
        let db = Db::new(&format!("solved_backlog_{case}"));
        let mission = backlog_mission(&db, &base, "solved", &[23], 1, None);
        std::fs::write(scenarios.join("023.sh"), "exit 0\n").expect("the scenario");
        let none = std::collections::BTreeSet::new();
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("assign");
        let task = objective_task(&db, mission, 23);
        db.store.task_delivered(task.id, 1).expect("delivered");
        assert!(db.store.task_reported(task.id, "done").expect("reported"));
        let agent = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        push_work(&base, &source, &agent, "work.txt");
        let backlog = base.join("backlog");
        match case {
            "absent" => std::fs::remove_file(backlog.join("023.json")).expect("renumbered"),
            "malformed" => std::fs::write(backlog.join("x.json"), "{\n").expect("malformed"),
            "missing" => std::fs::remove_dir_all(&backlog).expect("gone"),
            _ => std::fs::write(
                backlog.join("023.json"),
                json!({"id": 23, "title": "Objective 23", "prompt": "Do it.",
                       "recurring": true, "metric": "made_up_ms", "target": 100,
                       "direction": "lower"})
                .to_string(),
            )
            .expect("recurring"),
        }

        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        let solved = control_records(&db, "objective.solved");
        let rejected = control_records(&db, "task.gated");
        let after = gated(&db, mission);
        let state = db.mission(mission)["tasks"][0]["state"].clone();
        if case == "recurring" {
            assert!(solved.is_empty(), "{case}: {solved:?}");
            assert_eq!(rejected.len(), 1, "{case}: {rejected:?}");
            assert_eq!(rejected[0]["verdict"], "rejected", "{case}");
            assert_eq!(after.gate_rejections, 1, "{case}");
            assert_eq!(state, "revise", "{case}");
        } else {
            assert_eq!(solved.len(), 1, "{case}: {solved:?}");
            assert_eq!(solved[0]["objective"], json!(23), "{case}");
            assert_eq!(solved[0]["task"], json!(task.id), "{case}");
            assert!(rejected.is_empty(), "{case}: {rejected:?}");
            assert_eq!(after.gate_rejections, 0, "{case}");
            assert_eq!(state, "cancelled", "{case}");
        }
        assert!(db.mission(mission)["halted"].is_null(), "{case}");
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// The same for the other wording mission 4's task 182 hit: a candidate
/// whose tree is main's is not rejected at once in an objective mission,
/// but judged — the gate's base check answers "solved on the base"
/// before any candidate build, so the objective is recorded solved
/// instead of the task rejected ("the work changes nothing on main").
#[test]
fn an_objective_task_whose_work_changes_nothing_is_judged_not_rejected() {
    let gate = "echo '| verdict rejected scenario already passes on the base'\n\
                    printf '{\"verdict\":\"rejected\",\"reason\":\"scenario already passes on the base\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
    let (base, source, scenarios, integrator) = world("solved-base-empty", gate);
    let db = Db::new("solved_base_empty");
    let mission = backlog_mission(&db, &base, "solved", &[23], 1, None);
    std::fs::write(scenarios.join("023.sh"), "exit 0\n").expect("the scenario");
    let none = std::collections::BTreeSet::new();
    crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("assign");
    let task = objective_task(&db, mission, 23);
    db.store.task_delivered(task.id, 1).expect("delivered");
    assert!(db.store.task_reported(task.id, "done").expect("reported"));
    let agent = db.mission(mission)["tasks"][0]["agent_id"]
        .as_str()
        .expect("the task's agent")
        .to_string();
    // A branch with a commit that changes no file: main's tree.
    let clone = base.join("empty-work");
    let clone_path = clone.to_string_lossy().to_string();
    git(&["clone", "-q", &source, &clone_path]).expect("clone");
    git(&[
        "-C",
        &clone_path,
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "nothing",
    ])
    .expect("empty");
    git(&[
        "-C",
        &clone_path,
        "push",
        "-q",
        "origin",
        &format!("HEAD:refs/heads/spawn/{agent}"),
    ])
    .expect("push the agent's branch");

    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    assert!(
        control_records(&db, "task.gated").is_empty(),
        "not rejected at once: {:?}",
        control_records(&db, "task.gated")
    );
    assert_eq!(control_records(&db, "objective.solved").len(), 1);
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "cancelled");
    let _ = std::fs::remove_dir_all(&base);
}

/// The owner's hold says the owner is moving main. Mission 6, 2026-11-08
/// 06:36: the owner held the source, pushed main behind Spawn's back and
/// recorded the move after; a pass between the push and the record
/// halted the mission, and `spawn mission moved` then matched no mission
/// (it skipped the halted ones), so the halt stood until a human resumed
/// it. The real order: hold, push, a pass, then moved and release.
#[test]
fn the_owners_hold_keeps_a_moved_main_from_halting() {
    let (base, source, scenarios, integrator) = world("owner-hold-move", "exit 1\n");
    let db = Db::new("owner_hold_move");
    let b0 = source_main(&source);
    let mission = mission_at_baseline(&db, &source, &scenarios, &b0);
    let lock = db
        .store
        .source_hold_lock(&source)
        .expect("the landing lock");
    db.store
        .source_hold(&source, &b0, 3600, Some("the owner's deploy"))
        .expect("held");
    drop(lock);
    let moved = main_moves(&base, &source, "owner-push.txt");

    integrator.pass_on(&db.store).expect("a pass");
    assert!(
        halted(&db, mission).is_null(),
        "no halt while held: {}",
        halted(&db, mission)
    );
    let after = db.mission(mission);
    assert_eq!(
        after["baseline"],
        b0.as_str(),
        "nothing is judged against it"
    );

    // Then the move is recorded, as `spawn mission moved` does, and the
    // hold released: the mission takes the owner's main.
    assert_eq!(
        db.store
            .source_moved(&source, &b0, &moved)
            .expect("the owner's move"),
        1
    );
    db.store.source_release(&source).expect("released");
    let after = db.mission(mission);
    assert_eq!(after["baseline"], moved.as_str());
    assert!(after["halted"].is_null(), "halted: {}", after["halted"]);
    integrator.pass_on(&db.store).expect("another pass");
    assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));
    let _ = std::fs::remove_dir_all(&base);
}

/// A mission the integrator halted on a moved main is moved and cleared
/// by the owner's record of that very move: same source, from and to.
/// A halt for another reason at the same baseline keeps it.
#[test]
fn a_main_move_is_recorded_over_a_halt_that_says_so() {
    let (base, source, scenarios, integrator) = world("owner-move-halted", "exit 1\n");
    let db = Db::new("owner_move_halted");
    let b0 = source_main(&source);
    let mission = mission_at_baseline(&db, &source, &scenarios, &b0);
    let other = mission_at_baseline(&db, &source, &scenarios, &b0);
    db.record(&json!({"type": "mission.halted", "id": other,
                      "reason": "three gate rejections in a row"}));
    let moved = main_moves(&base, &source, "owner-push.txt");

    integrator.pass_on(&db.store).expect("a pass");
    assert!(
        halted(&db, mission)
            .as_str()
            .is_some_and(|reason| reason.contains("but the last landing left")),
        "{}",
        halted(&db, mission)
    );
    // The move the halt names: applied.
    assert_eq!(
        db.store
            .source_moved(&source, &b0, &moved)
            .expect("the owner's move"),
        1
    );
    let after = db.mission(mission);
    assert!(after["halted"].is_null(), "halted: {}", after["halted"]);
    assert_eq!(after["baseline"], moved.as_str());
    let kept = db.mission(other);
    assert_eq!(
        kept["halted"],
        json!("three gate rejections in a row"),
        "another halt keeps its baseline"
    );
    assert_eq!(kept["baseline"], b0.as_str());
    integrator.pass_on(&db.store).expect("another pass");
    assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));
    let _ = std::fs::remove_dir_all(&base);
}

/// The integrator stopped between an owner landing's move of main and
/// its record: the request is still pending and main already holds the
/// commit. The next pass records the move from where it began (the
/// missions' baseline), so nothing halts; a commit main held already
/// moves nothing.
#[test]
fn an_owner_landing_whose_record_was_lost_is_recorded_from_where_it_began() {
    let (base, source, scenarios, integrator) = world("owner-lost", "exit 1\n");
    let db = Db::new("owner_lost");
    let b0 = source_main(&source);
    let mission = mission_at_baseline(&db, &source, &scenarios, &b0);
    let commit = owner_commit(&base, &source, "CONTRACT.md", "v1\n", "owner: the contract");
    let request = owner_request(&db, &source, &commit);
    // The move went out; its record did not.
    git(&[
        "--git-dir",
        &source,
        "update-ref",
        "refs/heads/main",
        &commit,
        &b0,
    ])
    .expect("moved");

    integrator.pass_on(&db.store).expect("a pass");
    let moved = control_records(&db, "source.moved");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["request"], json!(request));
    assert_eq!(moved[0]["from"], b0.as_str());
    assert_eq!(moved[0]["to"], commit.as_str());
    assert_eq!(db.mission(mission)["baseline"], commit.as_str());
    assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));

    // The same commit again: main holds it, nothing moves.
    let again = owner_request(&db, &source, &commit);
    integrator.pass_on(&db.store).expect("another pass");
    let moved = control_records(&db, "source.moved");
    assert_eq!(moved.len(), 2, "{moved:?}");
    assert_eq!(moved[1]["request"], json!(again));
    assert_eq!(moved[1]["from"], commit.as_str());
    assert_eq!(moved[1]["to"], commit.as_str());
    assert_eq!(source_main(&source), commit);
    assert!(halted(&db, mission).is_null(), "{}", halted(&db, mission));
    let _ = std::fs::remove_dir_all(&base);
}

/// Objective 036, through the store: an accepted verdict a gate run gave
/// carries what it judged (`inputs`, `env`), lands, and is judged again
/// beside it on the runner the controller's dispatcher names. The runner
/// rejects it: the task stays landed, nothing is counted, the record
/// says the two disagree on equal inputs, and the supervisor's digest
/// has one item for it until a delivered digest tells it. A verdict on
/// another scenario set is recorded without a comparison and is no news.
#[test]
#[allow(clippy::too_many_lines)]
fn a_shadow_judges_a_copy_beside_the_verdict_and_decides_nothing() {
    let gate = "echo '| == setup'\n\
                    echo '| base cache: toolchain primary-tc'\n\
                    printf '{\"verdict\":\"accepted\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n";
    let (base, source, _, integrator) = world("shadow", gate);
    let ctl = base.join("ctl");
    let scenarios = ctl.join("rsi").join("scenarios");
    std::fs::create_dir_all(&scenarios).expect("scenarios");
    std::fs::create_dir_all(ctl.join("deploy")).expect("deploy");
    std::fs::write(scenarios.join("check-ok.sh"), "exit 0\n").expect("a check");
    let marker = "0123456789abcdef0123456789abcdef01234567";
    std::fs::write(ctl.join(".controller"), format!("{marker}\n")).expect("marker");
    // The runner's dispatcher: it notes its arguments, places a
    // rejection naming the runner's toolchain, and exits 0.
    let notes = base.join("dispatched");
    std::fs::write(
        ctl.join("deploy/gate-runner.sh"),
        format!(
            "printf '%s\\n' \"$@\" >>'{notes}'\n\
                 job=\"$6/$(printf %s \"$4\" | cut -c1-12)-x\"\n\
                 mkdir -p \"$job\"\n\
                 printf '{{\"result\":\"verdict\",\"host\":\"runner-host\"}}\\n' >\"$job/record.json\"\n\
                 printf '{{\"verdict\":\"rejected\"}}\\n' >\"$job/verdict.json\"\n\
                 printf '| base cache: toolchain runner-tc\\n' >\"$job/gate.log\"\n",
            notes = notes.display()
        ),
    )
    .expect("dispatcher");
    let shadow_dir = base.join("shadow");
    let integrator = integrator.with_shadow(
        super::shadow::Settings {
            runner: "bb-runner".to_string(),
            every: 1,
        },
        shadow_dir.clone(),
    );
    let db = Db::new("shadow");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    push_work(&base, &source, &agent_of(&db, mission, task), "work.txt");
    integrator.pass_on(&db.store).expect("a pass");
    integrator.settle();
    let (gated_record, _, _) = last_verdict(&db);
    assert_eq!(gated_record["verdict"], "accepted", "{gated_record}");
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
    let given = &gated_record["inputs"];
    assert_eq!(given["base"], gated_record["base"]);
    assert_eq!(given["candidate"], gated_record["candidate"]);
    assert_eq!(given["controller"], marker);
    assert_eq!(
        given["scenarios_sha256"],
        json!(super::inputs::scenarios_sha256(&scenarios))
    );
    assert_eq!(given["scenario"], Value::Null);
    assert_eq!(gated_record["env"]["toolchain"], "primary-tc");
    assert_eq!(gated_record["env"]["runner"], Value::Null);

    let shadows = || control_records(&db, "task.shadow_gated");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while shadows().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "no shadow record within 30 s"
        );
        integrator.pass_on(&db.store).expect("a pass");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let record = shadows()[0].clone();
    assert_eq!(record["id"], task);
    assert_eq!(record["runner"], "bb-runner");
    assert_eq!(record["result"], "verdict");
    assert_eq!(record["verdict"], "rejected");
    assert_eq!(record["primary"], "accepted");
    assert_eq!(record["exit"], 0);
    assert_eq!(record["inputs"], *given, "{record}");
    assert_eq!(record["unequal"], json!([]));
    assert_eq!(record["agrees"], json!(false));
    assert_eq!(record["env"]["toolchain"], "runner-tc");
    assert_eq!(record["env"]["host"], "runner-host");
    assert_eq!(record["primary_env"], gated_record["env"]);
    let dispatched = std::fs::read_to_string(&notes).expect("the dispatcher ran");
    let arguments: Vec<&str> = dispatched.lines().collect();
    assert_eq!(arguments[0], "bb-runner");
    assert_eq!(Some(arguments[2]), given["base"].as_str());
    assert_eq!(Some(arguments[3]), given["candidate"].as_str());
    // The shadow decided nothing, and its directory is gone.
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
    let after = gated(&db, mission);
    assert_eq!(after.gate_rejections, 0);
    assert_eq!(after.gate_aborts, 0);
    let left: Vec<_> = std::fs::read_dir(&shadow_dir)
        .expect("the shadow directory")
        .flatten()
        .collect();
    assert!(left.is_empty(), "{left:?}");
    // Told once: an item of its own until a digest carrying its tag is
    // delivered.
    let items = crate::dispatch::digest::shadow_disagreements(
        db.store
            .shadow_disagreements()
            .expect("disagreements")
            .as_array()
            .expect("a list"),
    );
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].why, crate::dispatch::Why::ShadowDisagrees);
    assert!(items[0].why.is_news());
    let line = &items[0].line;
    assert!(
        line.contains(&format!("#{task} "))
            && line.contains("shadow")
            && line.contains("bb-runner")
            && line.contains("accepted")
            && line.contains("rejected")
            && line.contains("primary-tc")
            && line.contains("runner-tc"),
        "{line}"
    );
    // More passes record nothing more.
    integrator.pass_on(&db.store).expect("a pass");
    assert_eq!(shadows().len(), 1);

    // A record that compared nothing is never news, and the store takes
    // no `agrees` on unequal inputs.
    let mut unequal = record.clone();
    unequal["unequal"] = json!(["scenarios_sha256"]);
    unequal["agrees"] = Value::Null;
    db.record(&unequal);
    unequal["agrees"] = json!(false);
    assert!(db.store.integrator_record(&unequal).is_err());
    assert_eq!(
        db.store
            .shadow_disagreements()
            .expect("disagreements")
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
    let _ = std::fs::remove_dir_all(&base);
}

/// The types of the integrator's records after `after` (an event id),
/// in log order, with the task each names (its `id`).
fn record_types_after(db: &Db, after: i64) -> Vec<(String, Option<i64>)> {
    db.admin()
        .query(
            "select kind -> 'detail' -> 'record' ->> 'type',
                        (kind -> 'detail' -> 'record' ->> 'id')::bigint
                   from spawn_events
                  where session_id = 'session_control' and id > $1
                    and event_name = 'control.recorded'
                  order by session_seq",
            &[&after],
        )
        .expect("records")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

fn last_event(db: &Db) -> i64 {
    db.admin()
        .query_one("select coalesce(max(id), 0) from spawn_events", &[])
        .expect("last event")
        .get(0)
}

/// A drain (objective 37): task A's gate runs when it is asked;
/// `integrator.draining` names [A] and the bound. Task B, reported on
/// another source meanwhile, gets no gate through any number of passes,
/// although a gate is free, and the drain is not over while A's runs.
/// Released, A is accepted and lands, and the pass after records
/// `integrator.drained`, after the landing; B is still `reported`.
#[test]
fn a_drain_lets_the_running_gate_land_and_starts_none() {
    let marks = fresh_marks("spawn-db-drain-marks");
    let _release = Release(marks.clone());
    let (base_a, source_a, scenarios, mut integrator) = world("drain-a", &holding_gate(&marks));
    integrator.gates = 2;
    let (base_b, source_b, _, _) = world("drain-b", "exit 1\n");
    let db = Db::new("drain");
    let a = db.gated_mission(&source_a, &scenarios);
    let b = db.gated_mission(&source_b, &scenarios);
    let a_task = db.reported_task(a, "a1");
    push_work(&base_a, &source_a, &agent_of(&db, a, a_task), "a1.txt");
    integrator.pass_on(&db.store).expect("the first pass");
    assert_eq!(wait_started(&marks, 1).len(), 1, "A's gate runs");
    let mark = last_event(&db);

    integrator.begin_drain(Some(&db.store), 7200);
    assert!(integrator.draining());
    assert_eq!(
        control_records(&db, "integrator.draining"),
        vec![json!({"type": "integrator.draining", "gates": [a_task], "bound_s": 7200})]
    );
    let b_task = db.reported_task(b, "b1");
    push_work(&base_b, &source_b, &agent_of(&db, b, b_task), "b1.txt");
    for _ in 0..3 {
        integrator
            .pass_on(&db.store)
            .expect("a pass while draining");
        assert!(
            !integrator.drained(Some(&db.store), true),
            "not drained while A's gate runs"
        );
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(started(&marks).len(), 1, "no gate starts while draining");

    std::fs::write(marks.join("release"), "").expect("release");
    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(1);
    loop {
        integrator.reap();
        integrator.pass_on(&db.store).expect("a pass");
        if integrator.drained(Some(&db.store), true) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "never drained: {:?}",
            record_types_after(&db, mark)
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let records = record_types_after(&db, mark);
    let at = |kind: &str, task: Option<i64>| {
        records
            .iter()
            .position(|(seen, id)| seen == kind && *id == task)
            .unwrap_or_else(|| panic!("no {kind} in {records:?}"))
    };
    assert!(at("task.landed", Some(a_task)) < at("integrator.drained", None));
    assert!(
        !records
            .iter()
            .any(|(kind, id)| *id == Some(b_task) && kind.starts_with("task.gated")),
        "no verdict on B: {records:?}"
    );
    assert_eq!(db.mission(a)["tasks"][0]["state"], "landed");
    assert_eq!(db.mission(b)["tasks"][0]["state"], "reported");
    assert_eq!(started(&marks).len(), 1, "B never had a gate");
    assert_eq!(
        control_records(&db, "integrator.drained"),
        vec![json!({"type": "integrator.drained", "bound_s": 7200})]
    );
    for base in [base_a, base_b, marks] {
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// A drain at its bound: the gate still running is stopped (its process
/// gone), `integrator.drain_expired` names its task and the bound, and
/// nothing is recorded for the stopped gate: no verdict, the task still
/// `reported` for the next integrator.
#[test]
fn a_drain_at_its_bound_stops_the_gate_and_records_nothing_for_it() {
    let marks = fresh_marks("spawn-db-drain-bound-marks");
    let _release = Release(marks.clone());
    let gate = format!(
        "echo $$ >'{}/pid'\n{}",
        marks.to_string_lossy(),
        holding_gate(&marks)
    );
    let (base, source, scenarios, integrator) = world("drain-bound", &gate);
    let db = Db::new("drain_bound");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w1");
    push_work(&base, &source, &agent_of(&db, mission, task), "w1.txt");
    integrator.pass_on(&db.store).expect("the first pass");
    assert_eq!(wait_started(&marks, 1).len(), 1, "the gate runs");
    let pid: u32 = std::fs::read_to_string(marks.join("pid"))
        .expect("the gate's pid")
        .trim()
        .parse()
        .expect("a pid");

    integrator.begin_drain(Some(&db.store), 1);
    let now = std::time::Instant::now();
    assert!(!integrator.drain_expired(now));
    assert!(integrator.drain_expired(now + std::time::Duration::from_secs(2)));
    integrator.expire_drain(Some(&db.store));
    assert_eq!(
        control_records(&db, "integrator.drain_expired"),
        vec![json!({"type": "integrator.drain_expired", "gates": [task], "bound_s": 1})]
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !super::warm::exited_unreaped(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(super::warm::exited_unreaped(pid), "the gate {pid} is gone");
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert!(control_records(&db, "task.gated").is_empty(), "no verdict");
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "reported");
    assert!(control_records(&db, "integrator.drained").is_empty());
    for path in [base, marks] {
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// A drain at its bound with gates in units of their own (gate v2 step
/// 2, commit 7): the halt stops none of them, `integrator.drain_expired`
/// says so (`left_running`), and the integrator that starts next adopts
/// the gate and records it once. Skipped where no systemd user manager
/// answers.
#[test]
fn a_drain_at_its_bound_leaves_a_units_gate_for_the_next_integrator() {
    if !crate::gate::units_available() {
        eprintln!("no systemd user manager answers here: not tried");
        return;
    }
    let marks = fresh_marks("spawn-db-drain-units-marks");
    let _release = Release(marks.clone());
    let (base, source, scenarios, first) = world("drain-units", &holding_gate(&marks));
    let units = StopUnits(base.join("host"));
    let first = first.with_units(true);
    let db = Db::new("drain_units");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w1");
    push_work(&base, &source, &agent_of(&db, mission, task), "w1.txt");
    first.pass_on(&db.store).expect("the first pass");
    assert_eq!(wait_started(&marks, 1).len(), 1, "the gate runs");
    first.begin_drain(Some(&db.store), 1);
    first.expire_drain(Some(&db.store));
    assert_eq!(
        control_records(&db, "integrator.drain_expired"),
        vec![
            json!({"type": "integrator.drain_expired", "gates": [task], "bound_s": 1,
                    "left_running": true})
        ]
    );
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        control_records(&db, "task.gated").is_empty(),
        "no verdict yet"
    );
    let second = Integrator::new(
        base.join("host"),
        first.judge.gate.clone(),
        test_gate_env(),
        room_to_spare(),
        super::DEFAULT_GATES,
    )
    .with_units(true);
    second
        .pass_on(&db.store)
        .expect("the next integrator's pass");
    assert_eq!(second.running.borrow().len(), 1, "it adopted the gate");
    std::fs::write(marks.join("release"), "").expect("release");
    second.settle();
    first.settle();
    assert_eq!(started(&marks).len(), 1, "the gate ran once");
    assert_eq!(db.mission(mission)["tasks"][0]["state"], "landed");
    assert_eq!(control_records(&db, "task.gated").len(), 1);
    // The units go before the scratch their judgings are in.
    drop(units);
    for path in [base, marks] {
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// The store takes the integrator's drain records only as they are
/// specified: a bound from 1 to 86400 s, and the gates (but drained's)
/// as task numbers.
#[test]
fn the_store_takes_drain_records_as_specified() {
    let db = Db::new("drain_records");
    for good in [
        json!({"type": "integrator.draining", "gates": [], "bound_s": 7200}),
        json!({"type": "integrator.drained", "bound_s": 1}),
        json!({"type": "integrator.drain_expired", "gates": [4, 9], "bound_s": 86400}),
    ] {
        db.store.integrator_record(&good).expect("taken");
    }
    for bad in [
        json!({"type": "integrator.draining", "gates": ["4"], "bound_s": 7200}),
        json!({"type": "integrator.draining", "bound_s": 7200}),
        json!({"type": "integrator.drained", "bound_s": 0}),
        json!({"type": "integrator.drain_expired", "gates": [4], "bound_s": 86401}),
        json!({"type": "integrator.drain_expired", "gates": [4]}),
        json!({"type": "integrator.stopped", "gates": [], "bound_s": 5}),
    ] {
        assert!(db.store.integrator_record(&bad).is_err(), "refused: {bad}");
    }
}

/// The 12:39 sequence (P22; task 259, 2026-11-11): an owner's landing
/// moves main while the mission's gate is the deployed controller's,
/// built from the main before it. A task judged then would judge the
/// new main with the old controller's scenarios: it waits, said once,
/// starting no gate, until the deploy switches the controller to the
/// landing; then it is judged on the new main by the new controller.
/// Its own landing then moves main past the controller with no deploy
/// to come, and holds the next task not at all.
#[test]
fn a_task_waits_for_the_controller_of_the_owners_landing() {
    let marks = fresh_marks("spawn-controller-behind");
    let release = Release(marks.clone());
    let seen = marks.to_string_lossy().to_string();
    let gate = format!(
        "echo \"$4\" >>'{seen}/scenarios-given'\n{}",
        holding_gate(&marks)
    );
    let (base, source, _, integrator) = world("controller-behind", &gate);
    let db = Db::new("controller_behind");
    let m0 = source_main(&source);
    // deploy/controller.sh's layout: controllers/<name>-<commit>, and
    // controllers/<name>, the symlink the mission's gate names.
    let controllers = base.join("controllers");
    let deploy = |commit: &str| {
        std::fs::create_dir_all(controllers.join(format!("spawn-{commit}/rsi/scenarios")))
            .expect("a controller");
        let next = controllers.join("spawn.next");
        let _ = std::fs::remove_file(&next);
        std::os::unix::fs::symlink(format!("spawn-{commit}"), &next).expect("link");
        std::fs::rename(&next, controllers.join("spawn")).expect("switch");
    };
    deploy(&m0);
    let mission = db.gated_mission(&source, &controllers.join("spawn/rsi/scenarios"));
    let task = db.reported_task(mission, "w");
    let agent = agent_of(&db, mission, task);
    push_work(&base, &source, &agent, "work.txt");
    let commit = owner_commit(&base, &source, "CONTRACT.md", "v1\n", "owner: the contract");
    owner_request(&db, &source, &commit);

    integrator.pass_on(&db.store).expect("the landing's pass");
    assert_eq!(
        source_main(&source),
        commit,
        "the owner's landing moved main"
    );
    integrator
        .pass_on(&db.store)
        .expect("a pass before the deploy");
    integrator.pass_on(&db.store).expect("another");
    assert!(
        integrator.running.borrow().is_empty() && started(&marks).is_empty(),
        "no gate judges the new main with the old controller: {:?}",
        started(&marks)
    );
    assert!(verdicts(&db, task).is_empty());
    assert!(
        integrator
            .waiting
            .borrow()
            .contains(&(task, super::Wait::Controller)),
        "the wait is said (once: the set holds it)"
    );

    deploy(&commit);
    integrator
        .pass_on(&db.store)
        .expect("the pass after the deploy");
    let judged = wait_started(&marks, 1);
    assert_eq!(judged.len(), 1, "{judged:?}");
    std::fs::write(marks.join("release"), "").expect("release");
    integrator.settle();
    assert_eq!(verdicts(&db, task), vec!["accepted"]);
    let given = std::fs::read_to_string(marks.join("scenarios-given")).expect("given");
    assert!(
        given
            .trim_end()
            .ends_with(&format!("controllers/spawn-{commit}/rsi/scenarios")),
        "the new controller's scenarios: {given}"
    );
    // The task's landing moves main past the controller too, but needs
    // no deploy: the next task is judged on it, not held.
    assert_eq!(control_records(&db, "task.landed").len(), 1);
    let landed = source_main(&source);
    assert_ne!(landed, commit);
    let next = db.reported_task(mission, "w2");
    let next_agent = agent_of(&db, mission, next);
    push_work(&base, &source, &next_agent, "next.txt");
    integrator
        .pass_on(&db.store)
        .expect("the pass after the landing");
    integrator.settle();
    assert_eq!(verdicts(&db, next), vec!["accepted"], "judged, not held");
    assert!(
        !integrator
            .waiting
            .borrow()
            .contains(&(next, super::Wait::Controller))
    );
    drop(release);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// P22, on the store: a gated mission's reported task waits while the
/// owner's latest landing on its source is newer than the controller
/// its gate is in. The loop's alert step fires `controller_behind` once
/// that has held at every reading for the bound, once, and clears it at
/// the first reading after the deploy switches the controller to the
/// landing. Main moved by anything but the owner (a task's landing)
/// fires nothing.
#[test]
fn the_controller_behind_alert_fires_once_and_clears_after_the_deploy() {
    let (base, source, _, integrator) = world("controller-alert", "exit 0\n");
    let db = Db::new("controller_alert");
    let m0 = source_main(&source);
    let controllers = base.join("controllers");
    let deploy = |commit: &str| {
        std::fs::create_dir_all(controllers.join(format!("spawn-{commit}/rsi/scenarios")))
            .expect("a controller");
        let next = controllers.join("spawn.next");
        let _ = std::fs::remove_file(&next);
        std::os::unix::fs::symlink(format!("spawn-{commit}"), &next).expect("link");
        std::fs::rename(&next, controllers.join("spawn")).expect("switch");
    };
    deploy(&m0);
    let mission = db.gated_mission(&source, &controllers.join("spawn/rsi/scenarios"));
    db.reported_task(mission, "w");
    let (mut settings, _) = crate::alerts::Settings::from_env();
    settings.check_ms = 1;
    settings.integrator_down_ms = 0;
    settings.controller_behind_ms = 300;
    settings.memory_low_mb = 0;
    settings.tmpfs_high_mb = 0;
    settings.disk_low_gb = 0;
    settings.test_log = None;
    settings.live_log = None;
    let mut monitor = crate::alerts::Monitor::with(settings);
    let mut check = || {
        std::thread::sleep(std::time::Duration::from_millis(5));
        monitor.check(&db.store).expect("a check");
    };
    let past_the_bound = || std::thread::sleep(std::time::Duration::from_millis(400));
    let alerts = || -> Vec<Value> {
        db.store
            .alerts(0)
            .expect("the alerts")
            .as_array()
            .expect("alerts")
            .iter()
            .filter(|row| row["kind"] == "controller_behind")
            .cloned()
            .collect()
    };

    // Main is the controller's commit, then past it by a push that is
    // no owner's landing: nothing, however long.
    check();
    main_moves(&base, &source, "pushed.txt");
    check();
    past_the_bound();
    check();
    assert!(alerts().is_empty(), "{:?}", alerts());
    let m1 = owner_commit(&base, &source, "CONTRACT.md", "v1\n", "owner: the contract");
    owner_request(&db, &source, &m1);
    integrator.pass_on(&db.store).expect("the landing's pass");
    assert_eq!(source_main(&source), m1, "the owner's landing moved main");
    check();
    assert!(alerts().is_empty(), "not yet: the bound starts");
    past_the_bound();
    check();
    check();
    let fired = alerts();
    assert_eq!(fired.len(), 1, "once: {fired:?}");
    assert_eq!(fired[0]["state"], "open");
    assert_eq!(fired[0]["subject"], format!("mission #{mission}"));
    assert_eq!(fired[0]["evidence"]["controller"], m0.as_str());
    assert_eq!(fired[0]["evidence"]["landed"], m1.as_str());

    deploy(&m1);
    check();
    let cleared = alerts();
    assert_eq!(cleared.len(), 1, "{cleared:?}");
    assert_eq!(cleared[0]["state"], "cleared", "{cleared:?}");
    let _ = std::fs::remove_dir_all(&base);
}

/// Objective 178, through the real store and `spawn_commit_map`: the
/// controller deployed before a history rewrite is `controllers/spawn-<old>`,
/// an id the rewritten source does not hold; the commit map reads it as m0.
/// The owner lands past m0: the task waits for the deploy (read through
/// the map, the controller is behind), starting no gate. Switched to a
/// controller whose commit the rewrite dropped, the task is judged (an
/// unknown controller is never a wait) and accepted.
#[test]
fn a_controller_from_before_the_rewrite_holds_the_task_through_the_map() {
    let marks = fresh_marks("spawn-controller-rewrite");
    let release = Release(marks.clone());
    let (base, source, _, integrator) = world("controller-rewrite", &holding_gate(&marks));
    let db = Db::new("controller_rewrite");
    let m0 = source_main(&source);
    let old = "a1".repeat(20);
    let dropped = "d2".repeat(20);
    db.store
        .commit_map_load(&[(old.clone(), Some(m0.clone())), (dropped.clone(), None)])
        .expect("the map");
    let controllers = base.join("controllers");
    let deploy = |commit: &str| {
        std::fs::create_dir_all(controllers.join(format!("spawn-{commit}/rsi/scenarios")))
            .expect("a controller");
        let next = controllers.join("spawn.next");
        let _ = std::fs::remove_file(&next);
        std::os::unix::fs::symlink(format!("spawn-{commit}"), &next).expect("link");
        std::fs::rename(&next, controllers.join("spawn")).expect("switch");
    };
    deploy(&old);
    let mission = db.gated_mission(&source, &controllers.join("spawn/rsi/scenarios"));
    let task = db.reported_task(mission, "w");
    let agent = agent_of(&db, mission, task);
    push_work(&base, &source, &agent, "work.txt");
    let commit = owner_commit(&base, &source, "CONTRACT.md", "v1\n", "owner: the contract");
    owner_request(&db, &source, &commit);

    integrator.pass_on(&db.store).expect("the landing's pass");
    assert_eq!(
        source_main(&source),
        commit,
        "the owner's landing moved main"
    );
    integrator
        .pass_on(&db.store)
        .expect("a pass before the deploy");
    assert!(
        integrator.running.borrow().is_empty() && started(&marks).is_empty(),
        "no gate judges past the controller built before the rewrite: {:?}",
        started(&marks)
    );
    assert!(verdicts(&db, task).is_empty());
    assert!(
        integrator
            .waiting
            .borrow()
            .contains(&(task, super::Wait::Controller)),
        "the controller, read as m0, is behind the owner's landing"
    );

    deploy(&dropped);
    integrator
        .pass_on(&db.store)
        .expect("the pass with a dropped controller");
    let judged = wait_started(&marks, 1);
    assert_eq!(judged.len(), 1, "judged, never a wait: {judged:?}");
    std::fs::write(marks.join("release"), "").expect("release");
    integrator.settle();
    assert_eq!(verdicts(&db, task), vec!["accepted"]);
    drop(release);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// Objective 178, the alert: the loop's Monitor reads the controller
/// through the commit map. A controller built before the rewrite
/// (`spawn-<old>`, mapped to m0) behind the owner's landing fires
/// `controller_behind` after the bound, its evidence the new id; switched
/// to a controller whose commit the rewrite dropped, it clears (never
/// behind).
#[test]
fn the_controller_behind_alert_reads_the_controller_through_the_map() {
    let (base, source, _, integrator) = world("controller-alert-map", "exit 0\n");
    let db = Db::new("controller_alert_map");
    let m0 = source_main(&source);
    let old = "a1".repeat(20);
    let dropped = "d2".repeat(20);
    db.store
        .commit_map_load(&[(old.clone(), Some(m0.clone())), (dropped.clone(), None)])
        .expect("the map");
    let controllers = base.join("controllers");
    let deploy = |commit: &str| {
        std::fs::create_dir_all(controllers.join(format!("spawn-{commit}/rsi/scenarios")))
            .expect("a controller");
        let next = controllers.join("spawn.next");
        let _ = std::fs::remove_file(&next);
        std::os::unix::fs::symlink(format!("spawn-{commit}"), &next).expect("link");
        std::fs::rename(&next, controllers.join("spawn")).expect("switch");
    };
    deploy(&old);
    let mission = db.gated_mission(&source, &controllers.join("spawn/rsi/scenarios"));
    db.reported_task(mission, "w");
    let (mut settings, _) = crate::alerts::Settings::from_env();
    settings.check_ms = 1;
    settings.integrator_down_ms = 0;
    settings.controller_behind_ms = 300;
    settings.memory_low_mb = 0;
    settings.tmpfs_high_mb = 0;
    settings.disk_low_gb = 0;
    settings.test_log = None;
    settings.live_log = None;
    let mut monitor = crate::alerts::Monitor::with(settings);
    let mut check = || {
        std::thread::sleep(std::time::Duration::from_millis(5));
        monitor.check(&db.store).expect("a check");
    };
    let past_the_bound = || std::thread::sleep(std::time::Duration::from_millis(400));
    let alerts = || -> Vec<Value> {
        db.store
            .alerts(0)
            .expect("the alerts")
            .as_array()
            .expect("alerts")
            .iter()
            .filter(|row| row["kind"] == "controller_behind")
            .cloned()
            .collect()
    };

    let m1 = owner_commit(&base, &source, "CONTRACT.md", "v1\n", "owner: the contract");
    owner_request(&db, &source, &m1);
    integrator.pass_on(&db.store).expect("the landing's pass");
    assert_eq!(source_main(&source), m1, "the owner's landing moved main");
    check();
    assert!(alerts().is_empty(), "not yet: the bound starts");
    past_the_bound();
    check();
    let fired = alerts();
    assert_eq!(fired.len(), 1, "once: {fired:?}");
    assert_eq!(fired[0]["state"], "open");
    assert_eq!(
        fired[0]["evidence"]["controller"],
        m0.as_str(),
        "the new id"
    );
    assert_eq!(fired[0]["evidence"]["landed"], m1.as_str());

    deploy(&dropped);
    check();
    let cleared = alerts();
    assert_eq!(cleared.len(), 1, "{cleared:?}");
    assert_eq!(
        cleared[0]["state"], "cleared",
        "a dropped controller is never behind"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Gate v2 step 5b, on the store: while the nightly's fact for the
/// source has a red (`red_since`), the mission's reported task is not
/// judged (held, said once), so nothing of it lands; the owner's push
/// on the red source still lands; a green nightly (`red_since` null)
/// releases the task, which is then judged and lands.
#[test]
fn a_red_nightly_holds_task_landings_and_not_the_owners() {
    let marks = fresh_marks("spawn-nightly-red");
    let release = Release(marks.clone());
    let (base, source, scenarios, integrator) = world("nightly-red", &holding_gate(&marks));
    let db = Db::new("nightly_red");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "w");
    let agent = agent_of(&db, mission, task);
    push_work(&base, &source, &agent, "work.txt");
    // nightly.sh's fact for $SPAWN_LIVE/missions/source.git: red.
    let facts = integrator.judge.root.join("nightly");
    std::fs::create_dir_all(&facts).expect("facts");
    let m0 = source_main(&source);
    let fact = |red_since: Option<&str>| {
        let state = if red_since.is_some() { "red" } else { "green" };
        let failed: &[&str] = if red_since.is_some() { &["023"] } else { &[] };
        std::fs::write(
            facts.join("source.json"),
            json!({"v": 1, "main": m0, "at": 1, "state": state, "failed": failed,
                   "last_green": null, "red_since": red_since})
            .to_string(),
        )
        .expect("the fact");
    };
    fact(Some(&m0));

    integrator.pass_on(&db.store).expect("a pass while red");
    integrator.pass_on(&db.store).expect("another");
    assert!(
        integrator.running.borrow().is_empty() && started(&marks).is_empty(),
        "no task is judged while the nightly is red: {:?}",
        started(&marks)
    );
    assert!(verdicts(&db, task).is_empty());
    assert!(
        integrator
            .waiting
            .borrow()
            .contains(&(task, super::Wait::NightlyRed)),
        "the hold is said (once: the set holds it)"
    );

    // The owner's push lands on the red source: a fix or a revert is one.
    let commit = owner_commit(&base, &source, "FIX.md", "fix\n", "owner: the fix");
    owner_request(&db, &source, &commit);
    integrator.pass_on(&db.store).expect("the owner's pass");
    assert_eq!(source_main(&source), commit, "the owner's push landed");
    assert!(started(&marks).is_empty(), "the task is still held");

    // A green nightly ends the red: the task is judged and lands.
    fact(None);
    std::fs::write(marks.join("release"), "").expect("release");
    integrator
        .pass_on(&db.store)
        .expect("the pass after the green night");
    integrator.settle();
    assert_eq!(verdicts(&db, task), vec!["accepted"]);
    integrator.pass_on(&db.store).expect("the landing's pass");
    assert_eq!(control_records(&db, "task.landed").len(), 1);
    drop(release);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&marks);
}

/// The load-sensitive bound (sync/design-load-sensitive-bound.md; the
/// lead, 11-13: any scenario, the same pushed commit). gate.sh found a
/// regression load-sensitive (failed at load, passed alone as the base
/// does): `host_fault` with evidence `load_sensitive`, judged again after
/// the backoff, counted toward no cap. The second time for the same pushed
/// commit, by another scenario here, parks it (`load_sensitive_parked`):
/// both scenarios named, each with its root-fix owner from
/// `scenario-owners`, escalated once; the mission's run of host faults
/// ends, so its other task is judged, and the parked one is not, pass after
/// pass, until a person releases it (`task.released`), when the count
/// starts again; parked again, a new commit pushed on its branch releases
/// it (by the integrator) and is judged.
#[test]
#[allow(clippy::too_many_lines)]
fn a_commit_load_sensitive_twice_is_parked_and_its_mission_goes_on() {
    let dir = std::env::temp_dir().join(format!("spawn-db-parked-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let runs = dir.join("runs");
    let runs_path = runs.display();
    // Odd runs find 099.sh load-sensitive, even runs 046.sh.
    let gate = format!(
        "n=$(cat '{runs_path}' 2>/dev/null || echo 0); n=$((n + 1)); echo \"$n\" >'{runs_path}'\n\
             if [ $((n % 2)) = 1 ]; then s=099.sh; else s=046.sh; fi\n\
             echo \"| $s: load-sensitive: fails at 4 at a time (1, then 1), passes alone (0) as the base does (0, then 0): not this candidate's failure; root fix wanted\"\n\
             printf '{{\"v\":1,\"cannot_judge\":true,\"scenario\":\"%s\",\"runs\":\"1, then 1 at 4 at a time, then 0 alone on the candidate; 0, then 0 on the base, run now\",\"reason\":\"load-sensitive on the candidate\",\"load_sensitive\":[{{\"scenario\":\"%s\",\"runs\":\"r\",\"base\":\"b\",\"solo_ms\":1,\"base_ms\":[1,1]}}]}}\\n' \"$s\" \"$s\" >\"$SPAWN_GATE_VERDICT\"\n\
             exit 6\n"
    );
    let (base, source, scenarios, mut integrator) = world("load-sensitive-parked", &gate);
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_millis(1),
        cap: 2,
    };
    std::fs::create_dir_all(base.join("host")).expect("root");
    std::fs::write(
        base.join("host").join(super::SCENARIO_OWNERS),
        "# scenario  root-fix owner\n099.sh rev-f\n",
    )
    .expect("owners");
    let db = Db::new("load_sensitive_parked");
    let mission = db.gated_mission(&source, &scenarios);
    let first = db.reported_task(mission, "w1");
    let agent = |index: usize| {
        db.mission(mission)["tasks"][index]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string()
    };
    let pushed = push_work(&base, &source, &agent(0), "work.txt");
    let count =
        || std::fs::read_to_string(&runs).map_or(0, |text| text.trim().parse::<u32>().unwrap_or(0));
    let pass = || {
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
    };
    let task_of = |id: i64| {
        gated(&db, mission)
            .tasks
            .into_iter()
            .find(|task| task.id == id)
            .expect("the task")
    };

    // The first: load_sensitive, a host fault as any, with its backoff.
    let source_main_before = source_main(&source);
    pass();
    assert_eq!(count(), 1);
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["id"], first);
    assert_eq!(record["verdict"], "host_fault", "{record}");
    assert_eq!(record["evidence"], "load_sensitive");
    assert_eq!(record["pushed"], pushed.as_str());
    assert_eq!(record["sensitive"], json!(["099.sh"]));
    assert_eq!(record["host_faults"], 1);
    assert_eq!(record["escalate"], false);
    let task = task_of(first);
    assert_eq!(
        task.gate_load_sensitive_pushed.as_deref(),
        Some(pushed.as_str())
    );
    assert_eq!(task.gate_parked_pushed, None);

    // An owner's landing between the two moves main (the gate's merge
    // candidate) but not the task's pushed commit: the key holds (rev-e N1).
    // Recorded `source.moved`, as `spawn mission moved` does.
    let moved = main_moves(&base, &source, "owner.txt");
    db.store
        .source_moved(&source, &source_main_before, &moved)
        .expect("moved");

    // The second on the same commit, another scenario: parked.
    past_due(&db);
    pass();
    assert_eq!(count(), 2);
    let (record, _, _) = last_verdict(&db);
    assert_eq!(record["id"], first);
    assert_eq!(record["verdict"], "host_fault", "{record}");
    assert_eq!(record["evidence"], "load_sensitive_parked");
    assert_eq!(record["parked"], true);
    assert_eq!(record["escalate"], true);
    assert_eq!(record["sensitive"], json!(["099.sh", "046.sh"]));
    assert_eq!(
        record["owners"],
        json!([{"scenario": "099.sh", "owner": "rev-f"}, {"scenario": "046.sh", "owner": null}])
    );
    let after = gated(&db, mission);
    assert_eq!(
        (after.gate_host_faults, after.gate_host_task),
        (0, None),
        "the run ends: nothing waits behind a parked task"
    );
    assert_eq!((after.gate_rejections, after.gate_aborts), (0, 0));
    let task = task_of(first);
    assert_eq!(task.gate_parked_pushed.as_deref(), Some(pushed.as_str()));
    assert_eq!(
        (task.state, task.revisions),
        (spawn_core::mission::TaskState::Reported, 0),
        "neither rejected nor sent back"
    );
    assert_eq!(
        db.store
            .task_host_faults(first, 0, &source_main(&source))
            .expect("count"),
        0,
        "neither evidence counts toward the cap"
    );
    let items = crate::dispatch::digest::host_fault_escalations(
        db.store
            .gate_host_fault_escalations()
            .expect("escalations")
            .as_array()
            .expect("records"),
    );
    assert_eq!(items.len(), 1, "{items:?}");
    for wanted in [
        format!("task #{first} of mission {mission} is parked"),
        "099.sh (root fix: rev-f)".to_string(),
        "046.sh (no root-fix owner named in scenario-owners)".to_string(),
        format!("spawn task release --id {first}"),
    ] {
        assert!(
            items[0].line.contains(&wanted),
            "{wanted:?} in {}",
            items[0].line
        );
    }

    // The mission's other task is judged; the parked one is not.
    let second = db.reported_task(mission, "w2");
    push_work(&base, &source, &agent(1), "other.txt");
    pass();
    assert_eq!(count(), 3);
    assert_eq!(last_verdict(&db).0["id"], second);
    past_due(&db);
    pass();
    assert_eq!(count(), 4);
    assert_eq!(
        last_verdict(&db).0["id"],
        second,
        "the parked task is not judged"
    );

    // A person releases it: judged again, its count from the start.
    db.record(&json!({"type": "task.released", "id": first, "by": "test",
                      "reason": "the root fix landed"}));
    let task = task_of(first);
    assert_eq!(
        (task.gate_parked_pushed, task.gate_load_sensitive_pushed),
        (None, None)
    );
    past_due(&db);
    pass();
    assert_eq!(count(), 5);
    let record = last_verdict(&db).0;
    assert_eq!(record["id"], first, "{record}");
    assert_eq!(
        record["evidence"], "load_sensitive",
        "the count starts again"
    );

    // Parked again; then a new commit on its branch releases it, by the
    // integrator, and the new commit is judged.
    past_due(&db);
    pass();
    let record = last_verdict(&db).0;
    assert_eq!(record["id"], first, "{record}");
    assert_eq!(record["evidence"], "load_sensitive_parked", "{record}");
    let newer = push_work_on(&base, &source, &agent(0), &pushed, "more.txt");
    pass();
    let released: Value = db
        .admin()
        .query_one(
            "select kind -> 'detail' -> 'record' from spawn_events
              where kind -> 'detail' -> 'record' ->> 'type' = 'task.released'
              order by session_seq desc limit 1",
            &[],
        )
        .expect("the release")
        .get(0);
    assert_eq!(released["id"], first);
    assert_eq!(released["by"], "spawn-integrate");
    assert!(
        released["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains(&newer)),
        "{released}"
    );
    pass();
    let record = last_verdict(&db).0;
    assert_eq!(record["id"], first, "{record}");
    assert_eq!(record["pushed"], newer.as_str());
    assert_eq!(
        record["evidence"], "load_sensitive",
        "a new commit counts from the start"
    );
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Objective 173: a gate-accepted candidate that does not descend from its
/// base (after a history rewrite its base reads as the new commit, and the
/// candidate was merged on the old history) is never pushed, although main
/// is at its base: its verdict is `stale`, naming why, and the task is
/// judged again.
#[test]
fn a_landing_candidate_that_does_not_descend_from_its_base_is_never_pushed() {
    let (base, source, scenarios, integrator) = world("foreign-candidate", "exit 1\n");
    let db = Db::new("foreign_candidate");
    let b0 = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "f");
    let c1 = main_moves(&base, &source, "moved.txt");
    let mirror = mirror_of(&integrator, &source);
    // The candidate: a merge on b0, in the mirror (where landings are made).
    let tree = git(&["--git-dir", &mirror, "rev-parse", &format!("{b0}^{{tree}}")]).expect("tree");
    let candidate = git(&[
        "--git-dir",
        &mirror,
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit-tree",
        &tree,
        "-p",
        &b0,
        "-m",
        "merged on b0",
    ])
    .expect("candidate");
    db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                      "verdict": "accepted", "base": c1, "candidate": candidate}));

    integrator.pass_on(&db.store).expect("a pass");
    assert_eq!(source_main(&source), c1, "main is left at the base");
    let after = db.mission(mission);
    assert_ne!(after["tasks"][0]["state"], "landed", "{after}");
    let gated: Value = db
        .admin()
        .query_one(
            "select kind -> 'detail' -> 'record' from spawn_events
              where kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                and kind -> 'detail' -> 'record' ->> 'id' = $1
              order by id desc limit 1",
            &[&task.to_string()],
        )
        .expect("the latest verdict")
        .get(0);
    assert_eq!(gated["verdict"], "stale", "{gated}");
    assert_eq!(
        gated["reason"],
        format!("the candidate {candidate} does not descend from its base {c1}").as_str(),
        "{gated}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Objective 177: a parked task's pushed commit and a red main, recorded
/// before a history rewrite (shaped as the integrator records them:
/// `load_sensitive_parked_fields`, `source.main_red`), are read by their new
/// commits once the commit map is loaded, so the parked task is not released
/// as if on a new commit, and a probe of the rewritten red main is due as
/// before.
#[test]
fn a_parked_task_and_a_red_main_are_read_by_their_new_commits_after_a_rewrite() {
    let (base, source, scenarios, _integrator) = world("parked-rewrite", "exit 1\n");
    let db = Db::new("parked_rewrite");
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "p");
    let (pushed, pushed_new) = ("a1".repeat(20), "b1".repeat(20));
    let (main, main_new) = ("c2".repeat(20), "d2".repeat(20));
    let mut parked = super::load_sensitive_parked_fields(
        &pushed,
        &["003.sh".to_string()],
        &[("003.sh".to_string(), None)],
        "log",
    );
    for (field, value) in [
        ("type", json!("task.gated")),
        ("id", json!(task)),
        ("revision", json!(0)),
        ("verdict", json!("host_fault")),
        ("base", json!(main)),
    ] {
        parked[field] = value;
    }
    db.record(&parked);
    db.record(
        &json!({"type": "source.main_red", "source": source, "main": main,
                      "scenario": "check-tests.sh", "runs": 2,
                      "failures": [{"test": "t", "message": "m", "class": "regression"}],
                      "task": task, "mission": mission}),
    );
    let before = gated(&db, mission);
    assert_eq!(
        before.tasks[0].gate_parked_pushed.as_deref(),
        Some(pushed.as_str())
    );

    db.store
        .commit_map_load(&[
            (pushed.clone(), Some(pushed_new.clone())),
            (main.clone(), Some(main_new.clone())),
        ])
        .expect("the map");
    let after = gated(&db, mission);
    assert_eq!(
        after.tasks[0].gate_parked_pushed.as_deref(),
        Some(pushed_new.as_str())
    );
    assert_eq!(
        after.tasks[0].gate_load_sensitive_pushed.as_deref(),
        Some(pushed_new.as_str())
    );
    assert_eq!(
        after.red.as_ref().map(|red| red.main.as_str()),
        Some(main_new.as_str())
    );

    std::thread::sleep(std::time::Duration::from_millis(5));
    assert!(
        db.store
            .integrator_record(&json!({"type": "source.main_probe", "source": source,
                                       "main": "e3".repeat(20), "task": task, "probe_ms": 1}))
            .is_err(),
        "another main's probe is not due"
    );
    db.store
        .integrator_record(&json!({"type": "source.main_probe", "source": source,
                                   "main": main_new, "task": task, "probe_ms": 1}))
        .expect("a probe of the rewritten red main is due");
    let _ = std::fs::remove_dir_all(&base);
}

/// Objective 177: the re-push of a lost checkpoint refuses one from before
/// a history rewrite. The agent's recorded checkpoint, the head of its
/// branch in this host's mirror, which the source lacks, is one the commit
/// map names dropped: it is never pushed (the source has no spawn/<agent>),
/// the task is stale, and the refusal is recorded once however many passes
/// try.
#[test]
fn a_checkpoint_from_before_the_rewrite_is_never_pushed_again() {
    use spawn_core::event::{ArtifactRef, CheckpointReason, EventKind, NewEvent, Source};
    use spawn_core::id::{AgentId, BranchName, CommitId, SessionId};
    use spawn_core::port::{Commit, EventLog, WorkspaceStore};
    let db = Db::new("dropped_push");
    let (base, source, scenarios, _integrator) = world("dropped-push", "#!/bin/sh\nexit 0\n");
    let mission = db.gated_mission(&source, &scenarios);
    let task = ready_task(&db, db.reported_task(mission, "dropped"));
    let root = base.join("host");
    let mirror = crate::sources::ensure_mirror(&root, Some(&source)).expect("mirror");
    let host = spawn_host::GitWorkspace::new(&mirror, &root);
    let branch = BranchName::new(task.agent_id.as_str()).expect("branch");
    host.ensure_branch(&branch, None).expect("branch");
    let workspace = host.mount(&branch).expect("mount");
    std::fs::write(Path::new(workspace.path()).join("work.txt"), "work\n").expect("write");
    let Commit::Created(commit) = host.commit(&workspace, "work").expect("commit") else {
        panic!("a checkpoint");
    };
    host.unmount(&workspace).expect("unmount");
    db.store
        .append(NewEvent {
            agent_id: AgentId::new(task.agent_id.as_str()).expect("agent"),
            session_id: SessionId::new(task.session_id.as_str()).expect("session"),
            execution_id: None,
            source: Source::Bridge,
            source_event_id: Some(format!("commit:{commit}")),
            kind: EventKind::ArtifactCommitted {
                reason: CheckpointReason::TurnStopped,
                artifact: ArtifactRef {
                    project: "missions".to_string(),
                    branch: branch.clone(),
                    commit: CommitId::new(commit.as_str()).expect("commit"),
                },
                files: vec!["work.txt".to_string()],
            },
        })
        .expect("recorded, never pushed");
    db.store
        .commit_map_load(&[(commit.as_str().to_string(), None)])
        .expect("the map drops it");

    for _ in 0..2 {
        let verdict =
            super::Judge::pushed_candidate(&db.store, &mirror, &source, &task).expect("judged");
        assert!(matches!(verdict, super::Freshness::Stale(_)), "{verdict:?}");
    }
    assert!(
        git(&[
            "--git-dir",
            &source,
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/spawn/{}", task.agent_id),
        ])
        .is_err(),
        "nothing pushed"
    );
    let alerts: Vec<(String, Value)> = db
        .admin()
        .query(
            "select key, evidence from spawn_alerts where kind = 'rewrite_refused'",
            &[],
        )
        .expect("alerts")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert_eq!(
        alerts[0].0,
        format!("rewrite_refused:push-again:{}:{commit}", task.id)
    );
    assert_eq!(alerts[0].1["writer"], "push-again");
    assert_eq!(alerts[0].1["commit"], commit.as_str());
    let _ = std::fs::remove_dir_all(&base);
}

/// A gate that gives no verdict: a host fault on check-tests.sh whose reason
/// is `reason` (no quote or backslash in it).
fn gate_with_host_fault_reason(reason: &str) -> String {
    let verdict = format!(
        "{{\"host_fault\":true,\"scenario\":\"check-tests.sh\",\"on\":\"base\",\
         \"runs\":\"1, then 1 on the base\",\"reason\":\"{reason}\"}}"
    );
    format!(
        "echo \"$3\" >>\"$(dirname \"$0\")/runs\"\n\
             printf '%s\\n' '{verdict}' >\"$SPAWN_GATE_VERDICT\"\nexit 6\n"
    )
}

/// An owner request judged with no verdict until it is refused: two
/// judgings, the refusal at the second, never a third. Its no-verdict
/// reasons and its refusal, as stored.
fn judged_twice_then_refused(tag: &str, reason: &str) -> (Vec<(String, String)>, Value) {
    let (base, source, scenarios, mut integrator) =
        world(tag, &gate_with_host_fault_reason(reason));
    integrator.judge.host = super::HostSettings {
        server_log: None,
        backoff: std::time::Duration::from_mins(10),
        cap: 3,
    };
    let db = Db::new(&tag.replace('-', "_"));
    let m0 = source_main(&source);
    db.gated_mission(&source, &scenarios);
    let commit = owner_commit_files(
        &base,
        &source,
        &[("scenarios/protected", ""), ("A.md", "a\n")],
        "owner: a",
    );
    let request = gated_request(&db, &source, &commit, "a");
    for _ in 0..3 {
        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        end_backoff(&integrator, request);
    }
    assert_eq!(gate_runs(&base), 2, "judged twice, never a third time");
    let refused = control_records(&db, "source.push_refused");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0]["request"], json!(request));
    assert_eq!(source_main(&source), m0, "nothing moves");
    let found = no_verdict_records(&db, request);
    assert_eq!(found.len(), 2, "{found:?}");
    let _ = std::fs::remove_dir_all(&base);
    (found, refused[0].clone())
}

/// Objective 170: the rule "no verdict twice for the same reason" compares
/// the stored reason, masked, with what the store keeps of the new one: a
/// fault reason holding a PGPASSWORD= value is refused at the second
/// judging, and the stored reasons and the refusal are masked.
#[test]
fn no_verdict_twice_for_one_masked_reason_is_refused() {
    let (found, refused) = judged_twice_then_refused(
        "owner-no-verdict-masked",
        "connect: PGPASSWORD=s3cretvalue refused",
    );
    for (reason, _) in &found {
        assert!(reason.contains("[redacted]"), "{reason}");
        assert!(!reason.contains("s3cretvalue"), "{reason}");
    }
    assert_eq!(found[0].0, found[1].0, "the same reason, as stored");
    let said = refused["reason"].as_str().expect("the refusal's reason");
    assert!(!said.contains("s3cretvalue"), "{said}");
    assert!(said.contains("[redacted]"), "{said}");
}

/// Objective 170 R3 (cc-astra's A73): the formatted reason is exactly 500
/// characters and ends in " PWD=x", whose mask is longer. Only the fields a
/// store check limits are cut, so the stored reason is kept whole (past 500,
/// masked) and is the same reason at the second judging: refused then, never
/// judged a third time.
#[test]
fn no_verdict_twice_at_the_500_char_boundary_is_refused() {
    let prefix = "the host's fault on check-tests.sh: ";
    let tail = " PWD=x";
    let reason = format!("{}{tail}", "r".repeat(500 - prefix.len() - tail.len()));
    assert_eq!(prefix.len() + reason.len(), 500);
    let (found, _) = judged_twice_then_refused("owner-no-verdict-boundary", &reason);
    for (stored, _) in &found {
        assert!(stored.chars().count() > 500, "never cut: {stored}");
        assert!(!stored.contains("PWD=x"), "{stored}");
        assert!(stored.starts_with(prefix), "{stored}");
    }
}

/// A test's world, removed when the test ends.
struct Gone(PathBuf);

impl Drop for Gone {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An integrator whose connections are made in `db`'s schema (objective
/// 221: `pass` takes and keeps the lock on a connection of its own), and
/// its world, removed when dropped.
fn locking_integrator(db: &Db, tag: &str) -> (Integrator, Gone) {
    let (base, _, _, integrator) = world(tag, "exit 0\n");
    let (url, schema) = (db.url.clone(), db.schema.name().to_string());
    let integrator = integrator.with_connect(Box::new(move || {
        PostgresStore::connect_in_schema(&url, &schema).map_err(|error| error.to_string())
    }));
    (integrator, Gone(base))
}

/// A connection in `db`'s schema.
fn connection(db: &Db) -> PostgresStore {
    PostgresStore::connect_in_schema(&db.url, db.schema.name()).expect("connect")
}

/// Objective 221: an idle integrator keeps its lock between passes, so a
/// second one is refused while the first is idle, and after a pass of the
/// first that failed on a live connection too; a start finds it held and,
/// past its wait, is refused naming the holder.
#[test]
fn a_second_integrator_is_refused_while_the_first_is_idle() {
    let db = Db::new("lock_idle");
    let (first, _first_world) = locking_integrator(&db, "lock-idle-a");
    let (second, _second_world) = locking_integrator(&db, "lock-idle-b");
    first.pass().expect("the first pass");
    assert!(first.held.borrow().is_some(), "the lock is kept while idle");
    first.pass().expect("an idle pass");
    second.pass().expect("the second's pass does nothing");
    assert!(second.held.borrow().is_none(), "the second is refused");
    assert!(
        second.stopped.borrow().is_none(),
        "it never held it: no stop"
    );

    // A pass that fails on a live connection keeps the lock.
    let mut admin = db.admin();
    admin
        .batch_execute("alter function spawn_now_ms() rename to spawn_now_ms_k")
        .expect("rename");
    let failed = first.pass();
    admin
        .batch_execute("alter function spawn_now_ms_k() rename to spawn_now_ms")
        .expect("rename back");
    assert!(failed.is_err(), "the pass fails");
    assert!(first.held.borrow().is_some(), "and keeps the lock");
    second.pass().expect("the second's pass does nothing");
    assert!(second.held.borrow().is_none(), "still refused");

    let started = std::time::Instant::now();
    let refused = super::take_lock_at_start(
        &|| Ok(connection(&db)),
        std::time::Duration::from_millis(1200),
    )
    .err()
    .expect("a start is refused past its wait");
    assert!(started.elapsed() >= std::time::Duration::from_millis(1200));
    assert!(
        refused.starts_with(&format!(
            "integrate: another integrator holds this database's lock (spawn integrate pid={}, \
             connected at ",
            std::process::id()
        )) && refused.ends_with(
            " UTC); this one does not start. Stop that one first, or drain it with `spawn \
             integrate --drain --root <its root>`."
        ),
        "{refused}"
    );
}

/// Objective 221: a start that finds the lock held takes it once it is
/// freed within its wait (a deploy's restart).
#[test]
fn a_start_takes_a_lock_freed_within_its_wait() {
    let db = Db::new("lock_restart");
    let old = connection(&db);
    assert!(old.integrator_lock("spawn integrate pid=1").expect("lock"));
    let freed = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(800));
        drop(old);
    });
    let (store, pid) =
        super::take_lock_at_start(&|| Ok(connection(&db)), std::time::Duration::from_secs(10))
            .expect("the lock, once freed");
    freed.join().expect("freed");
    assert!(store.integrator_holds_lock().expect("holds"));
    assert_eq!(store.backend_pid().expect("pid"), pid);
}

/// Objective 221: an integrator holds its lock for life; a connection that
/// ended is dropped after the pass it failed, and the next pass takes the
/// lock again on a new one. Lost to another integrator by then, it stops,
/// naming the holder.
#[test]
fn a_lock_held_for_life_is_lost_only_to_another_integrator() {
    let db = Db::new("lock_lost");
    let (integrator, _world) = locking_integrator(&db, "lock-lost");
    let end = |pid: i32| {
        db.admin()
            .execute("select pg_terminate_backend($1)", &[&pid])
            .expect("end the connection");
    };
    integrator.pass().expect("the first pass");
    end(integrator.lock_pid.get().expect("its connection"));
    assert!(integrator.pass().is_err(), "a pass on an ended connection");
    assert!(
        integrator.held.borrow().is_none(),
        "the connection is dropped"
    );
    integrator.pass().expect("the lock taken again");
    assert!(integrator.held.borrow().is_some());
    assert!(integrator.stopped.borrow().is_none());

    end(integrator.lock_pid.get().expect("its connection"));
    assert!(integrator.pass().is_err());
    let other = connection(&db);
    let mut taken = false;
    for _ in 0..50 {
        taken = other
            .integrator_lock("spawn integrate pid=999")
            .expect("lock");
        if taken {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(taken, "another integrator takes the freed lock");
    let stopped = integrator.pass().expect_err("it stops");
    assert!(
        stopped.starts_with(
            "integrate: this integrator's connection to the database ended and another \
             integrator holds the lock now (spawn integrate pid=999, connected at "
        ) && stopped.ends_with(" UTC); this one stops"),
        "{stopped}"
    );
    assert_eq!(
        integrator.stopped.borrow().as_deref(),
        Some(stopped.as_str())
    );
}

/// Objective 234: the store takes `integrator.started`, full or with every
/// unreadable value null, as written, and refuses each field's bad shape
/// or absence alone (22023).
#[test]
fn the_store_takes_an_integrator_start_and_refuses_each_bad_field() {
    let db = Db::new("integrator_started");
    let full = json!({
        "type": "integrator.started", "pid": 60, "ppid": 50, "parent_comm": "st) 1 (234",
        "ancestors": [{"pid": 50, "comm": "st) 1 (234"}, {"pid": 1, "comm": "systemd"}],
        "cwd": "/w", "cgroup": "/user.slice/spawn-integrate.service",
        "gate": "/w/scripts/rsi/gate.sh", "gate_from_env": true, "root": "/w/ws", "once": false,
    });
    let bare = json!({
        "type": "integrator.started", "pid": 61, "ppid": 0, "parent_comm": null,
        "ancestors": [], "cwd": null, "cgroup": null,
        "gate": "/w/g.sh", "gate_from_env": false, "root": "/w/ws", "once": true,
    });
    for record in [&full, &bare] {
        let event = db.store.integrator_record(record).expect("taken");
        let read: Value = db
            .admin()
            .query_one(
                "select kind -> 'detail' -> 'record' from spawn_events where id = $1",
                &[&event],
            )
            .expect("read back")
            .get(0);
        assert_eq!(&read, record);
    }
    let with = |field: &str, value: Option<Value>| {
        let mut record = full.clone();
        match value {
            Some(value) => record[field] = value,
            None => {
                record.as_object_mut().expect("an object").remove(field);
            }
        }
        record
    };
    for (field, value) in [
        ("pid", Some(json!(0))),
        ("pid", Some(json!("60"))),
        ("pid", None),
        ("ppid", Some(json!(-1))),
        ("ppid", None),
        ("gate", Some(json!(""))),
        ("gate", Some(json!(null))),
        ("gate", None),
        ("root", Some(json!(""))),
        ("root", None),
        ("gate_from_env", Some(json!("true"))),
        ("gate_from_env", None),
        ("once", Some(json!(1))),
        ("once", None),
        ("parent_comm", Some(json!(5))),
        ("parent_comm", None),
        ("cwd", Some(json!([]))),
        ("cwd", None),
        ("cgroup", Some(json!({}))),
        ("cgroup", None),
        ("ancestors", Some(json!(null))),
        ("ancestors", Some(json!([{"pid": "1", "comm": "init"}]))),
        ("ancestors", Some(json!([{"pid": 1}]))),
        ("ancestors", Some(json!([1]))),
        ("ancestors", None),
        ("extra", Some(json!(1))),
    ] {
        let record = with(field, value.clone());
        let error = db
            .store
            .integrator_record(&record)
            .expect_err(&format!("{field} {value:?} refused"));
        let text = format!("{error:?}");
        assert!(
            text.contains("integrator.started"),
            "{field} {value:?}: {text}"
        );
        assert!(text.contains("22023"), "{field} {value:?}: {text}");
    }
    let count: i64 = db
        .admin()
        .query_one(
            "select count(*) from spawn_events
              where kind -> 'detail' -> 'record' ->> 'type' = 'integrator.started'",
            &[],
        )
        .expect("count")
        .get(0);
    assert_eq!(count, 2);
}

/// A graded preset in the catalog, onboarded active at `grade`.
fn graded_preset(db: &Db, id: &str, model: &str, rank: u32, grade: &str) {
    db.store
        .grading_record(&json!({"type": "preset.registered", "preset_id": id,
                                "engine_label": "claude", "model_label": id,
                                "model_profile": model, "cost_rank": rank}))
        .expect("register");
    db.store
        .grading_record(&json!({"type": "preset.onboarded", "preset_id": id,
                                "from_state": "unexamined", "to_state": "active",
                                "to_grade": grade, "passed": 1, "total": 1,
                                "failed_cases": [], "suite": "s"}))
        .expect("onboard");
}

/// An objective mission whose backlog holds `objectives` (id, prompt).
fn prompted_mission(db: &Db, base: &Path, name: &str, objectives: &[(u32, &str)]) -> i64 {
    let ids: Vec<u32> = objectives.iter().map(|(id, _)| *id).collect();
    let workers = u32::try_from(objectives.len()).expect("few");
    let mission = backlog_mission(db, base, name, &ids, workers, None);
    for (id, prompt) in objectives {
        std::fs::write(
            base.join("backlog").join(format!("{id:03}.json")),
            json!({"id": id, "title": format!("Objective {id}"), "prompt": prompt}).to_string(),
        )
        .expect("objective");
    }
    mission
}

/// An objective task's grading as the store keeps it: (difficulty,
/// preset, first preset, model, kind).
fn grading_of(db: &Db, task: i64) -> (i16, Option<String>, Option<String>, String, String) {
    let row = db
        .admin()
        .query_one(
            "select difficulty, preset_id, first_preset_id, model, kind
               from mission_tasks where id = $1",
            &[&task],
        )
        .expect("the task");
    (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4))
}

/// The `task.assigned` record of `task`.
fn assigned_record(db: &Db, task: i64) -> Value {
    db.admin()
        .query_one(
            "select kind -> 'detail' -> 'record' from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' = 'task.assigned'
                and kind -> 'detail' -> 'record' ->> 'id' = $1::bigint::text",
            &[&task],
        )
        .expect("the record")
        .get(0)
}

/// Issue #99 item 4: with the grading catalog empty an objective's task
/// is assigned as before agent grading (default model, `l2`, no preset,
/// a record naming none of them); with a catalog that covers neither its
/// classified difficulty nor `l2` it falls back the same way, never
/// failing the assignment.
#[test]
fn an_objective_without_a_catalog_that_takes_it_is_assigned_as_before() {
    let base = std::env::temp_dir().join(format!("spawn-db-obj-ungraded-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let db = Db::new("objective_ungraded");
    let mission = prompted_mission(&db, &base, "ru", &[(1, "Do it."), (2, "Do that.")]);
    let none = std::collections::BTreeSet::new();
    let first =
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("a pass");
    assert_eq!(first.len(), 2, "both assigned on an empty catalog");
    for task in &first {
        let record = assigned_record(&db, *task);
        for key in [
            "model",
            "difficulty",
            "preset_id",
            "execution_profile",
            "classification",
        ] {
            assert!(record.get(key).is_none(), "{key}: {record}");
        }
        let (difficulty, preset, first_preset, model, _) = grading_of(&db, *task);
        assert_eq!(
            (difficulty, preset, first_preset, model.as_str()),
            (2, None, None, "default")
        );
    }

    // A catalog that takes only l1: an l2 objective falls back, assigned.
    let base_l1 = base.join("l1");
    let db = Db::new("objective_uncovered");
    graded_preset(&db, "small", "cheap", 0, "l1");
    let mission = prompted_mission(&db, &base_l1, "rv", &[(3, "Do it.")]);
    let assigned =
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("a pass");
    assert_eq!(assigned.len(), 1);
    assert_eq!(
        grading_of(&db, assigned[0]),
        (2, None, None, "default".to_string(), "resident".to_string())
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Issue #99 item 4: an objective's task is classified from the
/// objective's own text and routed through the catalog, resident and
/// per-task worker alike: a typo fix to the cheap `l1` preset, a security
/// fix to the `l3` one, each with its model, difficulty, preset and
/// classification recorded; an `l2` preset's starved twin gets the next
/// `l2` objective once (the anti-starvation re-probe), then routing is as
/// before.
#[test]
#[allow(clippy::too_many_lines)]
fn an_objective_is_classified_and_routed_through_the_catalog() {
    let base = std::env::temp_dir().join(format!("spawn-db-obj-graded-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let db = Db::new("objective_graded");
    graded_preset(&db, "small", "cheap", 0, "l1");
    graded_preset(&db, "mid", "default", 1, "l2");
    graded_preset(&db, "mid-dear", "default", 3, "l2");
    graded_preset(&db, "strong", "default", 2, "l3");
    let mission = prompted_mission(
        &db,
        &base,
        "rg",
        &[
            (1, "Fix the typo in README.md"),
            (2, "Harden authentication: stop the SQL injection"),
        ],
    );
    let none = std::collections::BTreeSet::new();
    let assigned =
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("a pass");
    assert_eq!(assigned.len(), 2);
    let (typo, security) = (
        objective_task(&db, mission, 1).id,
        objective_task(&db, mission, 2).id,
    );
    assert_eq!(
        grading_of(&db, typo),
        (
            1,
            Some("small".to_string()),
            Some("small".to_string()),
            "cheap".to_string(),
            "resident".to_string()
        )
    );
    assert_eq!(
        grading_of(&db, security),
        (
            3,
            Some("strong".to_string()),
            Some("strong".to_string()),
            "default".to_string(),
            "ephemeral".to_string()
        )
    );
    // The board the dispatcher delivers from names each task's model.
    let board = db.mission(mission);
    let model_of = |task: i64| {
        board["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|row| row["id"] == task)
            .map(|row| row["model"].clone())
    };
    assert_eq!(model_of(typo), Some(json!("cheap")));
    assert_eq!(model_of(security), Some(json!("default")));
    let record = assigned_record(&db, typo);
    assert_eq!(record["model"], json!("cheap"));
    assert_eq!(record["difficulty"], json!("l1"));
    assert_eq!(record["preset_id"], json!("small"));
    assert_eq!(record["classification"]["difficulty"], json!("l1"));
    // Classified from the objective's text alone: the brief's standing
    // rules are in the brief, not in the evidence.
    assert!(
        record["brief"]
            .as_str()
            .is_some_and(|brief| brief.contains("Work only inside this workspace")),
        "{record}"
    );
    let evidence = record["classification"]["signals"].to_string();
    assert!(!evidence.contains("Cargo.toml"), "{evidence}");
    assert!(!evidence.contains("e2e"), "{evidence}");
    let record = assigned_record(&db, security);
    assert_eq!(record["difficulty"], json!("l3"));
    assert_eq!(record["classification"]["difficulty"], json!("l3"));
    assert!(
        record.get("model").is_none(),
        "default is not named: {record}"
    );

    // The re-probe: every preset's window long past, the next l2
    // objective goes once to mid-dear (mid is the cheapest), then to mid.
    db.admin()
        .batch_execute("update grading_presets set window_start_ms = 0")
        .expect("starve the presets");
    let base_l2 = base.join("l2");
    let mission = prompted_mission(&db, &base_l2, "rp", &[(5, "Do it.")]);
    let probed =
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("a pass");
    assert_eq!(
        grading_of(&db, probed[0]).1.as_deref(),
        Some("mid-dear"),
        "the starved l2 preset is re-probed"
    );
    db.store.task_cancel(probed[0]).expect("cancel");
    let next =
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("a pass");
    assert_eq!(
        grading_of(&db, next[0]).1.as_deref(),
        Some("mid"),
        "re-probed once, then the minimum-sufficient preset"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Issue #99 item 4: a routed objective task the gate rejects steps its
/// rework up in the verdict's own statement (`record_gated`), as any
/// routed task's does: the `l1` typo fix on `small` is rerouted to `mid`
/// (`l2`, the default model), its first preset kept; the board's model,
/// which the dispatcher runs the rework on, moves with it.
#[test]
fn a_routed_objectives_gate_rejection_steps_its_rework_up() {
    let base = std::env::temp_dir().join(format!("spawn-db-obj-stepup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let db = Db::new("objective_step_up");
    graded_preset(&db, "small", "cheap", 0, "l1");
    graded_preset(&db, "mid", "default", 1, "l2");
    graded_preset(&db, "strong", "default", 2, "l3");
    let mission = prompted_mission(&db, &base, "rs", &[(1, "Fix the typo in README.md")]);
    let none = std::collections::BTreeSet::new();
    let assigned =
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("a pass");
    let task = assigned[0];
    assert_eq!(grading_of(&db, task).1.as_deref(), Some("small"));
    db.store.task_delivered(task, 1).expect("delivered");
    assert!(db.store.task_reported(task, "done").expect("reported"));
    let rerouted = super::record_gated(
        &db.store,
        &json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "rejected",
                "base": "b", "note": "a check failed"}),
    )
    .expect("recorded")
    .expect("rerouted");
    assert_eq!(
        rerouted,
        json!({"type": "task.rerouted", "id": task, "preset_id": "mid", "model": "default",
               "from_preset_id": "small", "revision": 1, "routed_difficulty": "l2"})
    );
    assert_eq!(task_state(&db, mission, task), "revise");
    assert_eq!(
        grading_of(&db, task),
        (
            1,
            Some("mid".to_string()),
            Some("small".to_string()),
            "default".to_string(),
            "resident".to_string()
        )
    );
    let board = db.mission(mission);
    let row = board["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .find(|row| row["id"] == task)
        .cloned()
        .expect("the task");
    assert_eq!(row["model"], json!("default"));
    // A third rejection (two more) steps it up to l3.
    for revision in 1..3 {
        db.store.task_delivered(task, 1).expect("delivered");
        assert!(db.store.task_reported(task, "done").expect("reported"));
        let _ = super::record_gated(
            &db.store,
            &json!({"type": "task.gated", "id": task, "revision": revision,
                    "verdict": "rejected", "base": "b", "note": "a check failed"}),
        )
        .expect("recorded");
    }
    assert_eq!(grading_of(&db, task).1.as_deref(), Some("strong"));
    let _ = std::fs::remove_dir_all(&base);
}

/// `task`, delivered and reported at `revision`, rejected by the gate as
/// the integrator records it (`record_gated`, its step-up included).
fn gate_rejected(db: &Db, task: i64, revision: i32) {
    db.store.task_delivered(task, 1).expect("delivered");
    assert!(db.store.task_reported(task, "done").expect("reported"));
    super::record_gated(
        &db.store,
        &json!({"type": "task.gated", "id": task, "revision": revision,
                "verdict": "rejected", "base": "b", "note": "a check failed"}),
    )
    .expect("recorded");
}

/// Issue #99 item 4, review F1: a routed objective task stepped up by the
/// gate (`small` to `mid`, one quality rejection) that then overruns its
/// budget continues on `mid` (the default model) at its own `l1`, its
/// first preset `small` and its rejection carried: never routed afresh
/// onto `small` with its rejections forgotten.
#[test]
fn an_overrun_continuation_keeps_the_stepped_up_grading() {
    let base = std::env::temp_dir().join(format!("spawn-db-obj-carry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let db = Db::new("objective_carry");
    graded_preset(&db, "small", "cheap", 0, "l1");
    graded_preset(&db, "mid", "default", 1, "l2");
    graded_preset(&db, "strong", "default", 2, "l3");
    let mission = prompted_mission(&db, &base, "rc", &[(1, "Fix the typo in README.md")]);
    let none = std::collections::BTreeSet::new();
    let first = crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none)
        .expect("a pass")[0];
    gate_rejected(&db, first, 0);
    assert_eq!(grading_of(&db, first).1.as_deref(), Some("mid"));
    db.store.task_delivered(first, 2).expect("delivered again");
    db.record(&json!({"type": "task.over_budget", "id": first,
                      "reason": "the task made more than 5 tool calls",
                      "tool_calls": 6, "elapsed_ms": 1_000, "budget": {"tool_calls": 5}}));
    let next =
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("a pass");
    assert_eq!(next.len(), 1);
    let continuation = next[0];
    assert_eq!(continues_of(&db, continuation), Some(first));
    assert_eq!(
        grading_of(&db, continuation),
        (
            1,
            Some("mid".to_string()),
            Some("small".to_string()),
            "default".to_string(),
            "ephemeral".to_string()
        )
    );
    let rejections: i32 = db
        .admin()
        .query_one(
            "select quality_rejections from mission_tasks where id = $1",
            &[&continuation],
        )
        .expect("the task")
        .get(0);
    assert_eq!(rejections, 1);
    let record = assigned_record(&db, continuation);
    assert!(record.get("classification").is_none(), "{record}");
    assert_eq!(record["quality_rejections"], json!(1));
    // Its next rejection steps it on as the first task's would have: l3.
    gate_rejected(&db, continuation, 0);
    assert_eq!(grading_of(&db, continuation).1.as_deref(), Some("mid"));
    gate_rejected(&db, continuation, 1);
    assert_eq!(grading_of(&db, continuation).1.as_deref(), Some("strong"));
    let _ = std::fs::remove_dir_all(&base);
}

/// Issue #99 item 4, review F1: an objective whose task yields after three
/// gate rejections (stepped up to `strong` on the way) gets a fresh task,
/// classified and routed afresh: back on `small` at `l1`, no rejection
/// carried. At the default limit of three attempts an `l1` objective never
/// runs at `l3` across a yield.
#[test]
fn a_fresh_task_after_a_yield_is_routed_afresh() {
    let base = std::env::temp_dir().join(format!("spawn-db-obj-yield-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let db = Db::new("objective_routed_yield");
    graded_preset(&db, "small", "cheap", 0, "l1");
    graded_preset(&db, "mid", "default", 1, "l2");
    graded_preset(&db, "strong", "default", 2, "l3");
    let mission = prompted_mission(&db, &base, "ry", &[(1, "Fix the typo in README.md")]);
    let none = std::collections::BTreeSet::new();
    let first = crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none)
        .expect("a pass")[0];
    for revision in 0..3 {
        gate_rejected(&db, first, revision);
    }
    assert_eq!(grading_of(&db, first).1.as_deref(), Some("strong"));
    let next =
        crate::objectives::assign_next(&db.store, &snapshot(&db, mission), &none).expect("a pass");
    assert_eq!(task_state(&db, mission, first), "cancelled", "yielded");
    assert_eq!(next.len(), 1);
    let fresh = next[0];
    assert_ne!(fresh, first);
    assert_eq!(continues_of(&db, fresh), None);
    assert_eq!(
        grading_of(&db, fresh),
        (
            1,
            Some("small".to_string()),
            Some("small".to_string()),
            "cheap".to_string(),
            "resident".to_string()
        )
    );
    let record = assigned_record(&db, fresh);
    assert_eq!(record["classification"]["difficulty"], json!("l1"));
    assert!(record.get("quality_rejections").is_none(), "{record}");
    let _ = std::fs::remove_dir_all(&base);
}

/// `agent`'s checkpoint on `source`'s main, pushed as `spawn/<agent>` and
/// fetched into `mirror`: the pushed commit.
fn push_checkpoint(base: &Path, source: &str, mirror: &Path, agent: &str) -> String {
    let clone = base.join(format!("checkpoint-{agent}"));
    let clone_path = clone.to_string_lossy().to_string();
    git(&["clone", "-q", source, &clone_path]).expect("clone");
    std::fs::write(clone.join("work.txt"), "work\n").expect("write");
    super::tests::commit_in(&clone_path, "spawn checkpoint exec_p1");
    let branch = format!("refs/heads/spawn/{agent}");
    git(&["-C", &clone_path, "push", "-q", "origin", &format!("HEAD:{branch}")])
        .expect("push the agent's branch");
    git(&[
        "--git-dir",
        &mirror.to_string_lossy(),
        "fetch",
        "-q",
        source,
        &format!("+{branch}:refs/prov-test/head"),
    ])
    .expect("fetch into the mirror");
    git(&["-C", &clone_path, "rev-parse", "HEAD"]).expect("work")
}

/// A landing of task work whose source's main is indexed, through
/// `land()`: its candidate's last trailer is its `Spawn-Change`, and with
/// `hook_down` the database is unreachable at the landing hook. Returns
/// the candidate, after checking the landing succeeded.
fn land_with_provenance(tag: &str, hook_down: bool) -> (Db, String, PathBuf, String) {
    let (base, source, scenarios, integrator) = world(tag, "exit 1\n");
    let db = Db::new(&tag.replace('-', "_"));
    let b0 = source_main(&source);
    let mission = db.gated_mission(&source, &scenarios);
    let task = db.reported_task(mission, "l");
    let mirror =
        crate::sources::ensure_mirror(&integrator.judge.root, Some(&source)).expect("mirror");
    let domain = crate::provenance::index::domain_of(&source);
    let mut repo =
        crate::provenance::index::Repo::open(&mirror.to_string_lossy()).expect("the mirror");
    crate::provenance::index::index(&db.store, &mut repo, &domain).expect("main indexed");
    drop(repo);
    let head = push_checkpoint(&base, &source, &mirror, &agent_of(&db, mission, task));
    let message = super::landing_message(&ready_task(&db, task), &head);
    let candidate = super::landing_candidate(&mirror.to_string_lossy(), &b0, &head, &message)
        .expect("a landing")
        .expect("no conflict");
    let written = git(&[
        "--git-dir",
        &mirror.to_string_lossy(),
        "log",
        "-1",
        "--format=%B",
        &candidate,
    ])
    .expect("the message");
    assert_eq!(
        written.lines().last(),
        Some(format!("Spawn-Change: {}", crate::provenance::source_change(&source, task)).as_str()),
        "{written}"
    );
    db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                      "verdict": "accepted", "base": b0, "candidate": candidate}));
    let landing = ready_task(&db, task);
    crate::provenance::HOOK_UNREACHABLE.with(|down| down.set(hook_down));
    let landed = super::land(&db.store, &mirror, &source, &landing);
    crate::provenance::HOOK_UNREACHABLE.with(|down| down.set(false));
    assert_eq!(landed, Ok(super::Landing::Landed));
    assert_eq!(source_main(&source), candidate, "the push moved main");
    assert_eq!(task_records(&db, "task.landed", task).len(), 1);
    (db, candidate, base, domain)
}

/// Decisions B4 (verification 1, in a test): the landing's `land` Change
/// is in `spawn prov show` as soon as `land()` returns, with no indexer
/// run, including the Change its `Spawn-Change` names; a following index
/// run records nothing.
#[test]
fn a_landing_records_its_land_change_right_after_the_push() {
    let (db, candidate, base, domain) = land_with_provenance("prov-land-hook", false);
    let shown = crate::provenance::show(
        &db.store,
        &crate::provenance::Target::Commit(candidate.clone()),
    )
    .expect("show");
    let change = &shown["changes"][0];
    assert_eq!(change["change"]["kind"], "land", "{shown:#}");
    let message = git(&["--git-dir", &db_mirror(&base), "log", "-1", "--format=%B", &candidate])
        .expect("the landed message");
    assert_eq!(change["edges"][0]["kind"], "includes");
    assert_eq!(
        json!(crate::provenance::spawn_changes(&message)),
        json!([change["edges"][0]["dst"]]),
        "the trailer names the included Change"
    );
    let mut repo = crate::provenance::index::Repo::open(&db_mirror(&base)).expect("the source");
    let again = crate::provenance::index::index(&db.store, &mut repo, &domain).expect("index");
    assert_eq!(again.recorded(), 0, "{again:?}");
    let _ = std::fs::remove_dir_all(&base);
}

/// The source's bare repository under a test's `base`.
fn db_mirror(base: &Path) -> String {
    format!("{}/source.git", base.display())
}

/// Decisions B4 (verification 2): with the database unreachable at the
/// hook, the landing still succeeds (pushed, recorded `task.landed`), and
/// a later `spawn prov index` records the landing and its checkpoint.
#[test]
fn a_landing_whose_provenance_hook_fails_still_lands_and_the_indexer_records_it() {
    let (db, candidate, base, domain) = land_with_provenance("prov-land-down", true);
    let shown = crate::provenance::show(
        &db.store,
        &crate::provenance::Target::Commit(candidate.clone()),
    )
    .expect("show");
    assert_eq!(shown["changes"], json!([]), "the hook recorded nothing");
    let mut repo = crate::provenance::index::Repo::open(&db_mirror(&base)).expect("the source");
    let later = crate::provenance::index::index(&db.store, &mut repo, &domain).expect("index");
    assert_eq!((later.edits, later.lands), (1, 1), "{later:?}");
    let shown =
        crate::provenance::show(&db.store, &crate::provenance::Target::Commit(candidate))
            .expect("show");
    assert_eq!(shown["changes"][0]["change"]["kind"], "land");
    let _ = std::fs::remove_dir_all(&base);
}
