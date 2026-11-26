//! The primary remote gate (spawn-node v1 PR S5, design §12.2 S5, §12.4
//! M3a/M3b, §6.1, §6.2): a task's candidate gate queued as a `gate` job,
//! waited for, and a node's verdict believed or judged again here, against
//! a real ledger (`SPAWN_TEST_DATABASE_URL`, a schema of each test's own)
//! with nodes simulated as the gateway would drive them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use spawn_node_proto::result::{ResultMeta, Signal};
use spawn_postgres::{FaultNext, PostgresStore, RemoteJobKind, RemoteJobState};

use super::remote::tests::{self as fixtures, Ledger, Scratch};
use super::remote::{
    self, GateJob, Look, Primary, PrimaryOutcome, PrimaryRequest, PrimarySettings,
};
use super::{Integrator, shadow};

/// The boot id the ledger fixture enrolls its nodes with.
const BOOT: &str = "00000000000000018796a5b4c3d2e1f0";

/// A node as the gateway drives it on the ledger: its beats (each with the
/// next sequence number) and its results.
struct NodeSim {
    store: Arc<PostgresStore>,
    node: String,
    seq: i64,
}

impl NodeSim {
    /// One beat, holding nothing: the grants it got.
    fn beat(&mut self) -> Vec<Value> {
        self.seq += 1;
        let body = json!({"v": 1, "cell_id": "c0", "state": "idle",
                          "spec_sha256": fixtures::SPEC_SHA,
                          "capacities": {"gate_sandbox": {"free": 4, "total": 4}},
                          "tmpfs_budget_mb": {"free": 262_144, "total": 262_144},
                          "leases": [], "lease_results_pending": [],
                          "warm_bases": [], "edge": "timer"});
        let answer = self
            .store
            .node_gw_beat(&self.node, BOOT, self.seq, &body, |_| true)
            .expect("a beat")
            .answer;
        answer["grants"].as_array().cloned().unwrap_or_default()
    }

    /// Beat until a grant comes, for up to `within`.
    fn lease(&mut self, within: Duration) -> Option<Value> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Some(grant) = self.beat().into_iter().next() {
                return Some(grant);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    /// The node's result for `grant`: the gate's `exit` and verdict file,
    /// its stdout, judged on the grant's own inputs, changed by `tweak`;
    /// stored and completed as the gateway does it.
    fn finish(
        &self,
        results: &Path,
        grant: &Value,
        exit: Option<i64>,
        verdict: Option<&str>,
        stdout: &str,
        tweak: impl FnOnce(&mut ResultMeta),
    ) {
        let job = grant["job_id"].as_str().expect("a job id");
        let epoch = grant["lease_epoch"].as_i64().expect("an epoch");
        let given = granted_inputs(grant);
        let (mut meta, log) =
            fixtures::meta(job, epoch, exit, verdict, stdout, "warn\n", Some(&given));
        tweak(&mut meta);
        fixtures::store_result(results, &meta, &log);
        let line = meta.to_bytes();
        let mut result = serde_json::to_value(&meta).expect("the result");
        result["result_sha256"] = json!(spawn_node_proto::codec::sha256_hex(&line));
        result["log_bytes"] = json!(meta.log_bytes.to_string());
        let done = self
            .store
            .node_gw_result(&self.node, job, epoch, &result, |_| true)
            .expect("a result");
        assert!(done.answer, "{job} completes");
    }
}

/// The five inputs a grant carries (its `repo` left out).
fn granted_inputs(grant: &Value) -> Value {
    let mut given = grant["spec"]["inputs"].clone();
    if let Some(object) = given.as_object_mut() {
        object.remove("repo");
    }
    given
}

/// A verdict file on the grant's own commits.
fn verdict_for(word: &str, grant: &Value) -> String {
    let given = granted_inputs(grant);
    fixtures::verdict_on(
        word,
        given["base"].as_str().unwrap_or_default(),
        given["candidate"].as_str().unwrap_or_default(),
    )
}

/// A cargo home with a registry in it.
fn cargo_home(dir: &Path) -> PathBuf {
    let home = dir.join("cargo");
    std::fs::create_dir_all(home.join("registry/index/x")).expect("a registry");
    std::fs::write(home.join("registry/index/x/crate"), "crate\n").expect("a crate");
    home
}

/// The primary path's settings for a test: artifacts, results and cargo
/// home under `dir`, a lease awaited `wait`, looked at every 20 ms.
fn settings(dir: &Path, believe_reject: bool, wait: Duration) -> PrimarySettings {
    PrimarySettings {
        believe_reject,
        artifacts: dir.join("artifacts"),
        results: dir.join("results"),
        cargo_home: cargo_home(dir),
        queue_wait: wait,
        every: Duration::from_millis(20),
        limit: Duration::from_mins(2),
    }
}

/// What a node must hold for a gate of `controller` with the registry of
/// `cargo`: what the primary path requires.
fn holds(controller: &Path, cargo: &Path) -> Value {
    json!({
        "registry": remote::registry_sha256(cargo).expect("the registry's digest"),
        "pg": remote::PG_MAJOR,
        "web_lock": remote::web_lock(controller).expect("a web lock"),
        "toolchain": remote::toolchain_channel(controller).expect("a toolchain"),
    })
}

/// A ledger, a repository with a base and a candidate, a controller and
/// the primary path.
struct Fixture {
    ledger: Ledger,
    dir: Scratch,
    repo: PathBuf,
    base: String,
    candidate: String,
    controller: PathBuf,
    scenarios: PathBuf,
    primary: Primary,
    seed: std::cell::Cell<u8>,
    /// The request's [`PrimaryRequest::test_database`] (false unless set).
    test_database: bool,
}

impl Fixture {
    fn new(case: &str, believe_reject: bool, wait: Duration) -> Self {
        let ledger = Ledger::new(&format!("primary_{case}"));
        let dir = Scratch::new(&format!("primary-{}", case.replace('_', "-")));
        let (repo, base, candidate) = fixtures::repository(&dir.0);
        let controller = fixtures::controller(&dir.0);
        let primary = Primary::new(settings(&dir.0, believe_reject, wait), dir.0.join("work"));
        let scenarios = controller.join("rsi/scenarios");
        Self {
            ledger,
            dir,
            repo,
            base,
            candidate,
            controller,
            scenarios,
            primary,
            seed: std::cell::Cell::new(10),
            test_database: false,
        }
    }

    fn request(&self) -> PrimaryRequest<'_> {
        PrimaryRequest {
            task: 7,
            judging_id: Some("0123456789ab0123456789abcdef".to_string()),
            repo: &self.repo,
            base: &self.base,
            candidate: &self.candidate,
            scenarios: &self.scenarios,
            scenario: None,
            gate_env: &[],
            test_database: self.test_database,
        }
    }

    fn results(&self) -> PathBuf {
        self.primary.settings.results.clone()
    }

    /// A node enrolled holding what the gate requires.
    fn node(&self, name: &str) -> NodeSim {
        let seed = self.seed.get();
        self.seed.set(seed + 1);
        let node = self.ledger.enroll(
            name,
            seed,
            &holds(&self.controller, &self.primary.settings.cargo_home),
        );
        NodeSim {
            store: Arc::clone(&self.ledger.store),
            node,
            seq: 0,
        }
    }

    /// The one `gate` job of the ledger.
    fn job(&self) -> spawn_postgres::RemoteJob {
        let id: String = self
            .ledger
            .admin()
            .query_one(
                "select job_id from spawn_remote_jobs where kind = 'gate' order by job_id",
                &[],
            )
            .expect("a gate job")
            .get(0);
        self.ledger
            .store
            .remote_job(&id)
            .expect("read")
            .expect("the job")
    }

    /// No task record of any kind came of the remote runs.
    fn no_task_record(&self) {
        for kind in [
            "task.gated",
            "task.halted",
            "task.aborted",
            "task.shadow_gated",
            "mission.halted",
        ] {
            assert!(self.ledger.records(kind).is_empty(), "{kind}");
        }
    }
}

/// The node's verdict the primary path believed.
fn believed(outcome: PrimaryOutcome) -> remote::Believed {
    match outcome {
        PrimaryOutcome::Believed(believed) => *believed,
        other => panic!("not believed: {other:?}"),
    }
}

fn local_why(outcome: &PrimaryOutcome) -> &str {
    match outcome {
        PrimaryOutcome::Local { why } => why,
        other => panic!("not judged locally: {other:?}"),
    }
}

/// Inert by default: unset, empty and `local` are this host's gate; an
/// unknown value is refused (and the integrator judges locally); `node`
/// needs its directories; M3b is off unless set to 1.
#[test]
fn the_primary_path_is_off_unless_node_and_refuses_what_it_does_not_know() {
    let env = |pairs: &[(&str, &str)]| {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    };
    assert_eq!(remote::primary_settings(env(&[])), Ok(None));
    assert_eq!(
        remote::primary_settings(env(&[("SPAWN_GATE_REMOTE", "")])),
        Ok(None)
    );
    assert_eq!(
        remote::primary_settings(env(&[
            ("SPAWN_GATE_REMOTE", " local "),
            ("SPAWN_GATE_REMOTE_BELIEVE_REJECT", "1"),
            ("SPAWN_NODE_ARTIFACTS", "/a"),
            ("SPAWN_NODE_RESULTS", "/r"),
        ])),
        Ok(None),
        "M3b alone switches nothing on"
    );
    for unknown in ["nodes", "remote", "1", "NODE"] {
        let refused = remote::primary_settings(env(&[
            ("SPAWN_GATE_REMOTE", unknown),
            ("SPAWN_NODE_ARTIFACTS", "/a"),
            ("SPAWN_NODE_RESULTS", "/r"),
            ("CARGO_HOME", "/c"),
        ]))
        .expect_err(unknown);
        assert!(refused.contains("SPAWN_GATE_REMOTE"), "{refused}");
    }
}

/// `node` needs its directories; M3b is off unless set to 1, and refused
/// when it is anything but 1 or 0.
#[test]
fn the_primary_path_on_needs_its_directories_and_m3b_is_its_own_switch() {
    let env = |pairs: &[(&str, &str)]| {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    };
    // `node` without its directories, or with relative ones.
    assert!(remote::primary_settings(env(&[("SPAWN_GATE_REMOTE", "node")])).is_err());
    assert!(
        remote::primary_settings(env(&[
            ("SPAWN_GATE_REMOTE", "node"),
            ("SPAWN_NODE_ARTIFACTS", "a"),
            ("SPAWN_NODE_RESULTS", "/r"),
            ("CARGO_HOME", "/c"),
        ]))
        .is_err()
    );
    let on = remote::primary_settings(env(&[
        ("SPAWN_GATE_REMOTE", "node"),
        ("SPAWN_NODE_ARTIFACTS", "/a"),
        ("SPAWN_NODE_RESULTS", "/r"),
        ("HOME", "/h"),
    ]))
    .expect("settings")
    .expect("on");
    assert!(!on.believe_reject, "M3a by default");
    assert_eq!(on.queue_wait, Duration::from_secs(30));
    assert_eq!(on.cargo_home, PathBuf::from("/h/.cargo"));
    assert_eq!(on.artifacts, PathBuf::from("/a"));
    assert_eq!(on.results, PathBuf::from("/r"));
    let m3b = remote::primary_settings(env(&[
        ("SPAWN_GATE_REMOTE", "node"),
        ("SPAWN_GATE_REMOTE_BELIEVE_REJECT", "1"),
        ("SPAWN_REMOTE_QUEUE_WAIT_SECS", "5"),
        ("SPAWN_NODE_ARTIFACTS", "/a"),
        ("SPAWN_NODE_RESULTS", "/r"),
        ("CARGO_HOME", "/c"),
    ]))
    .expect("settings")
    .expect("on");
    assert!(m3b.believe_reject);
    assert_eq!(m3b.queue_wait, Duration::from_secs(5));
    for off in ["0", ""] {
        assert!(
            !remote::primary_settings(env(&[
                ("SPAWN_GATE_REMOTE", "node"),
                ("SPAWN_GATE_REMOTE_BELIEVE_REJECT", off),
                ("SPAWN_NODE_ARTIFACTS", "/a"),
                ("SPAWN_NODE_RESULTS", "/r"),
                ("CARGO_HOME", "/c"),
            ]))
            .expect("settings")
            .expect("on")
            .believe_reject
        );
    }
    assert!(
        remote::primary_settings(env(&[
            ("SPAWN_GATE_REMOTE", "node"),
            ("SPAWN_GATE_REMOTE_BELIEVE_REJECT", "yes"),
            ("SPAWN_NODE_ARTIFACTS", "/a"),
            ("SPAWN_NODE_RESULTS", "/r"),
            ("CARGO_HOME", "/c"),
        ]))
        .is_err()
    );
    // An integrator is made with the primary path off.
    let dir = Scratch::new("primary-off");
    let integrator = Integrator::new(
        dir.0.join("host"),
        dir.0.join("gate.sh"),
        Vec::new(),
        room_to_spare(),
        super::DEFAULT_GATES,
    );
    assert!(integrator.judge.remote.is_none());
}

/// M3a: exit 0 `accepted` on the job's own five inputs is believed: its
/// verdict, what the node judged and where. The job published its
/// artifacts (delta, baseline, controller pack) and the registry's
/// manifest, and requires what the node holds; no strike, and nothing of
/// it is recorded for the task by this path.
#[test]
fn m3a_an_acceptance_on_the_jobs_own_inputs_is_believed() {
    let world = Fixture::new("m3a_accepts", false, Duration::from_secs(30));
    let mut node = world.node("n1");
    let results = world.results();
    let ran = std::thread::spawn(move || {
        let grant = node.lease(Duration::from_secs(30)).expect("a grant");
        let verdict = verdict_for("accepted", &grant);
        node.finish(
            &results,
            &grant,
            Some(0),
            Some(&verdict),
            "| base cache: toolchain rustc 1.95.0\n",
            |_| {},
        );
        grant
    });
    let outcome = world
        .primary
        .judge(&world.ledger.store, &world.request(), &|| false);
    let grant = ran.join().expect("the node");
    let remote::Believed {
        verdict,
        given,
        env,
        belief,
        job_id,
    } = believed(outcome);
    assert!(verdict.accepted);
    assert_eq!(belief, "M3a");
    assert_eq!(job_id, grant["job_id"]);
    assert_eq!(given, granted_inputs(&grant));
    assert_eq!(given["base"], world.base.as_str());
    assert_eq!(given["candidate"], world.candidate.as_str());
    assert_eq!(given["controller"], "3".repeat(40));
    assert_eq!(
        given["scenarios_sha256"].as_str(),
        super::inputs::scenarios_sha256(&world.scenarios).as_deref()
    );
    assert_eq!(env["runner"], "node:n1");
    assert_eq!(env["host"], "n252-host");
    assert_eq!(world.ledger.strikes("n1"), (0, false));
    let row = world.job();
    assert_eq!(row.kind, RemoteJobKind::Gate);
    assert_eq!(row.state, RemoteJobState::Completed);
    assert_eq!(row.task_id, Some(7));
    assert_eq!(
        row.judging_id.as_deref(),
        Some("0123456789ab0123456789abcdef")
    );
    assert_eq!(
        row.requires,
        holds(&world.controller, &world.primary.settings.cargo_home)
    );
    // Every artifact the grant names is published, whole.
    let roles: Vec<&str> = grant["spec"]["artifact_urls"]
        .as_array()
        .expect("artifacts")
        .iter()
        .map(|url| url["role"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        roles,
        ["delta_bundle", "baseline_bundle", "controller_pack"]
    );
    let artifacts = remote::Artifacts::new(world.primary.settings.artifacts.clone());
    for url in grant["spec"]["artifact_urls"].as_array().expect("urls") {
        let digest = spawn_node_proto::ContentDigest::parse(url["digest"].as_str().unwrap())
            .expect("a digest");
        let bytes = std::fs::read(artifacts.path_of(&digest)).expect("published");
        digest.verify(&bytes).expect("whole");
    }
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(
            world
                .primary
                .settings
                .artifacts
                .join(remote::REGISTRY_MANIFEST),
        )
        .expect("the registry's manifest"),
    )
    .expect("json");
    assert_eq!(manifest["registry"], row.requires["registry"]);
    // The grant's environment names no database.
    assert!(!grant["spec"]["env"].to_string().contains("postgres"));
    world.no_task_record();
    assert!(world.ledger.records("remote_job.host_fault").is_empty());
}

/// M3a: exit 0 `rejected` on the job's own inputs is a valid run (no
/// strike, no fault on the ledger), judged again here; nothing recorded.
/// M3b (`SPAWN_GATE_REMOTE_BELIEVE_REJECT=1`): the same rejection is
/// believed.
#[test]
fn m3a_judges_a_rejection_again_here_and_m3b_believes_it() {
    for believe_reject in [false, true] {
        let case = if believe_reject {
            "m3b_rejects"
        } else {
            "m3a_rejects"
        };
        let world = Fixture::new(case, believe_reject, Duration::from_secs(30));
        let mut node = world.node("n1");
        let results = world.results();
        let ran = std::thread::spawn(move || {
            let grant = node.lease(Duration::from_secs(30)).expect("a grant");
            let verdict = verdict_for("rejected", &grant);
            node.finish(&results, &grant, Some(0), Some(&verdict), "| no\n", |_| {});
        });
        let outcome = world
            .primary
            .judge(&world.ledger.store, &world.request(), &|| false);
        ran.join().expect("the node");
        if believe_reject {
            let remote::Believed {
                verdict, belief, ..
            } = believed(outcome);
            assert!(!verdict.accepted);
            assert_eq!(belief, "M3b");
        } else {
            let why = local_why(&outcome);
            assert!(why.contains("M3a"), "{why}");
            assert!(why.contains("valid run"), "{why}");
        }
        assert_eq!(world.ledger.strikes("n1"), (0, false), "{case}");
        assert_eq!(world.job().state, RemoteJobState::Completed, "{case}");
        assert!(world.job().excluded_nodes.is_empty(), "{case}");
        assert!(world.ledger.records("remote_job.host_fault").is_empty());
        world.no_task_record();
    }
}

/// A node's valid-looking run for [`valid_run_on_two_strikes`]: exit 0,
/// a verdict `stated` with `reason`, its `stdout`, self-fenced or not, on a
/// node whose `(strikes, degraded)` are `start` once it holds its lease,
/// judged with `test_database` as given.
struct ValidRun {
    case: &'static str,
    believe_reject: bool,
    stated: &'static str,
    reason: &'static str,
    stdout: &'static str,
    self_fenced: bool,
    start: (i32, bool),
    test_database: bool,
}

impl ValidRun {
    fn new(case: &'static str, believe_reject: bool, stated: &'static str) -> Self {
        Self {
            case,
            believe_reject,
            stated,
            reason: "r",
            stdout: "| no\n",
            self_fenced: false,
            start: (2, false),
            test_database: false,
        }
    }
}

/// What gate.sh prints when check-tests.sh fails on the candidate with
/// only test-database connection errors (no failure of the work's).
const DB_ONLY_STDOUT: &str = "| == regression: check-tests.sh must still pass\n\
     | FAILED api::tests::one: error connecting to server: Connection refused (os error 111)\n\
     | stage gate rejected 1200ms regression: check-tests.sh must still pass\n";

/// The same failure, with a failure of the work's beside it.
const WORK_STDOUT: &str = "| == regression: check-tests.sh must still pass\n\
     | FAILED api::tests::one: error connecting to server: Connection refused (os error 111)\n\
     | FAILED api::tests::two: assertion `left == right` failed: the landing moved main\n\
     | stage gate rejected 1200ms regression: check-tests.sh must still pass\n";

/// A node (2 strikes, `degraded` as `run.start` says, set once it holds
/// its lease) finishes the primary path's job as `run` says: the outcome, the
/// node's strikes and `degraded` after it, and the job's row.
fn valid_run_on_two_strikes(
    run: &ValidRun,
) -> (PrimaryOutcome, (i32, bool), spawn_postgres::RemoteJob) {
    let mut world = Fixture::new(run.case, run.believe_reject, Duration::from_secs(30));
    world.test_database = run.test_database;
    let mut node = world.node("n1");
    let results = world.results();
    let mut admin = world.ledger.admin();
    let (stated, reason, stdout, self_fenced, (strikes, degraded)) = (
        run.stated,
        run.reason,
        run.stdout,
        run.self_fenced,
        run.start,
    );
    let ran = std::thread::spawn(move || {
        let grant = node.lease(Duration::from_secs(30)).expect("a grant");
        admin
            .execute(
                "update spawn_node_state set host_fault_strikes = $1, degraded = $2
                  where name = 'n1'",
                &[&strikes, &degraded],
            )
            .expect("two strikes");
        let given = granted_inputs(&grant);
        let verdict = json!({"v": 1, "verdict": stated, "reason": reason,
                             "base": given["base"], "candidate": given["candidate"]})
        .to_string();
        node.finish(&results, &grant, Some(0), Some(&verdict), stdout, |meta| {
            meta.self_fenced = self_fenced;
        });
    });
    let outcome = world
        .primary
        .judge(&world.ledger.store, &world.request(), &|| false);
    ran.join().expect("the node");
    (outcome, world.ledger.strikes("n1"), world.job())
}

/// Any valid run puts a node's strikes back to 0 (`spawn_remote_job_valid`):
/// an M3a acceptance, an M3a rejection judged again here, an M3b believed
/// rejection; a degraded node stays degraded (only an undrain clears it).
/// A self-fenced run (no verdict) leaves the strikes as they were.
#[test]
fn a_valid_run_resets_the_nodes_strikes_and_a_self_fenced_one_does_not() {
    // (a) M3a: an acceptance, believed.
    let (outcome, strikes, row) =
        valid_run_on_two_strikes(&ValidRun::new("valid_m3a_accepts", false, "accepted"));
    assert_eq!(believed(outcome).belief, "M3a");
    assert_eq!(strikes, (0, false), "an M3a acceptance resets the strikes");
    assert_eq!(row.state, RemoteJobState::Completed);
    assert_eq!(
        row.lease_epoch, 2,
        "the completion is judged: its epoch moved"
    );
    // (b) M3a: a rejection, a valid run judged again here.
    let (outcome, strikes, _) =
        valid_run_on_two_strikes(&ValidRun::new("valid_m3a_rejects", false, "rejected"));
    assert!(local_why(&outcome).contains("valid run"));
    assert_eq!(
        strikes,
        (0, false),
        "an M3a valid rejection resets the strikes"
    );
    // (c) M3b: a rejection, believed.
    let (outcome, strikes, _) =
        valid_run_on_two_strikes(&ValidRun::new("valid_m3b_rejects", true, "rejected"));
    let believed_rejection = believed(outcome);
    assert_eq!(believed_rejection.belief, "M3b");
    assert!(!believed_rejection.verdict.accepted);
    assert_eq!(
        strikes,
        (0, false),
        "an M3b believed rejection resets the strikes"
    );
    // A degraded node: its strikes go, `degraded` stays.
    let (outcome, strikes, _) = valid_run_on_two_strikes(&ValidRun {
        start: (2, true),
        ..ValidRun::new("valid_degraded", true, "accepted")
    });
    assert!(believed(outcome).verdict.accepted);
    assert_eq!(strikes, (0, true), "only an undrain clears degraded");
    // Self-fenced: no verdict, nothing reset.
    let (outcome, strikes, row) = valid_run_on_two_strikes(&ValidRun {
        self_fenced: true,
        ..ValidRun::new("valid_self_fenced", false, "accepted")
    });
    local_why(&outcome);
    assert_eq!(strikes, (2, false), "a self-fenced run resets nothing");
    assert_eq!(row.lease_epoch, 1, "nothing judged it");
}

/// A rejection on a test database naming only its connection errors may
/// be the node's own test server crashing: no valid run, so under M3a and
/// M3b alike the node keeps its 2 strikes and the job its epoch (neither
/// struck nor reset). The same rejection with a failure of the work's, or
/// on a gate without a test database, is a valid run: strikes back to 0.
#[test]
fn a_rejection_naming_only_test_database_errors_resets_no_strikes() {
    for believe_reject in [false, true] {
        let db_only = |case, stdout, test_database| ValidRun {
            reason: crate::integrate::host::CHECK_TESTS_REGRESSION,
            stdout,
            test_database,
            ..ValidRun::new(case, believe_reject, "rejected")
        };
        let (cases, rule) = if believe_reject {
            (["db_only_m3b", "work_m3b", "no_db_m3b"], "M3b")
        } else {
            (["db_only_m3a", "work_m3a", "no_db_m3a"], "M3a")
        };
        let (_, strikes, row) = valid_run_on_two_strikes(&db_only(cases[0], DB_ONLY_STDOUT, true));
        assert_eq!(strikes, (2, false), "{rule}: a DB-error-only rejection");
        assert_eq!(row.lease_epoch, 1, "{rule}: the completion is not judged");
        assert_eq!(row.state, RemoteJobState::Completed);
        let (_, strikes, row) = valid_run_on_two_strikes(&db_only(cases[1], WORK_STDOUT, true));
        assert_eq!(strikes, (0, false), "{rule}: a failure of the work's");
        assert_eq!(row.lease_epoch, 2);
        let (_, strikes, _) = valid_run_on_two_strikes(&db_only(cases[2], DB_ONLY_STDOUT, false));
        assert_eq!(strikes, (0, false), "{rule}: no test database, a valid run");
    }
}

/// A verdict on other commits or inputs is the node's fault: struck,
/// excluded, and the job goes to another node, whose verdict on the
/// job's own inputs is believed.
#[test]
fn unequal_inputs_strike_and_the_job_goes_to_another_node() {
    let world = Fixture::new("unequal", false, Duration::from_secs(30));
    let mut first = world.node("na");
    let mut second = world.node("nb");
    let results = world.results();
    let ran = std::thread::spawn(move || {
        let grant = first.lease(Duration::from_secs(30)).expect("a grant");
        let given = granted_inputs(&grant);
        let other =
            fixtures::verdict_on("accepted", given["base"].as_str().unwrap(), &"9".repeat(40));
        first.finish(&results, &grant, Some(0), Some(&other), "", |_| {});
        let again = second
            .lease(Duration::from_secs(30))
            .expect("granted again");
        assert_eq!(again["job_id"], grant["job_id"]);
        let verdict = verdict_for("accepted", &again);
        second.finish(&results, &again, Some(0), Some(&verdict), "", |_| {});
        // The first node is never granted it again.
        assert!(first.beat().is_empty());
    });
    let outcome = world
        .primary
        .judge(&world.ledger.store, &world.request(), &|| false);
    ran.join().expect("the nodes");
    let env = believed(outcome).env;
    assert_eq!(env["runner"], "node:nb");
    assert_eq!(world.ledger.strikes("na"), (1, false));
    assert_eq!(world.ledger.strikes("nb"), (0, false));
    let row = world.job();
    assert_eq!(row.fault_nodes.len(), 1);
    assert_eq!(row.excluded_nodes, row.fault_nodes);
    let faults = world.ledger.records("remote_job.host_fault");
    assert_eq!(faults.len(), 1);
    assert_eq!(faults[0]["evidence"], "unequal_inputs");
    assert_eq!(faults[0]["next"], "queued");
    world.no_task_record();
}

/// Section 6.2, row by row, through the primary path on the ledger: where
/// the job goes, the node's strikes, and that the task is judged here and
/// never charged an abort, a host fault or a `no_room` (no task record at
/// all). A job sent to another node with none to take it is taken back
/// once its wait is over (the only node is excluded from it).
#[test]
#[allow(clippy::type_complexity, clippy::too_many_lines)]
fn every_fault_row_is_settled_on_the_ledger_and_judged_here() {
    let world = Fixture::new("fault_rows", false, Duration::from_millis(400));
    let red = r#"{"v":1,"host_fault":true,"on":"main","scenario":"check-tests.sh","runs":"1, 1","failures":[{"test":"a::b","message":"m"}]}"#;
    let cannot = r#"{"v":1,"cannot_judge":true,"scenario":"t029.sh","reason":"no user scopes"}"#;
    let disk =
        r#"{"v":1,"no_room":true,"free_kb":10,"min_gb":8,"tmpdir":"/data01/spawn-node/w/0/t"}"#;
    let base =
        r#"{"v":1,"host_fault":true,"scenario":"check-a.sh","on":"base","runs":"2","reason":"r"}"#;
    let pair = "| check-x.sh exited 1, then 0 on the base: a harness error, not a verdict; \
                the gate aborts\n| stage gate aborted 12ms base: check-x.sh again \
                (unsolved needs two runs that agree)\n";
    // (row, exit, verdict file, stdout, tweak, strikes after, the job's
    // last fault's `next`, its state at the end)
    let rows: Vec<(
        &str,
        Option<i64>,
        Option<&str>,
        &str,
        Box<dyn FnOnce(&mut ResultMeta) + Send>,
        i32,
        Option<&str>,
        RemoteJobState,
    )> = vec![
        (
            "cannot_judge",
            Some(6),
            Some(cannot),
            "",
            Box::new(|_| {}),
            0,
            Some("fallback_local"),
            RemoteJobState::FallbackLocal,
        ),
        (
            "main_red",
            Some(6),
            Some(red),
            "",
            Box::new(|_| {}),
            0,
            Some("fallback_local"),
            RemoteJobState::FallbackLocal,
        ),
        (
            "host_fault",
            Some(6),
            Some(base),
            "",
            Box::new(|_| {}),
            1,
            Some("fallback_local"),
            RemoteJobState::FallbackLocal,
        ),
        (
            "aborted",
            Some(2),
            None,
            "",
            Box::new(|_| {}),
            1,
            Some("queued"),
            RemoteJobState::FallbackLocal,
        ),
        (
            "aborted",
            Some(3),
            None,
            "",
            Box::new(|_| {}),
            1,
            Some("fallback_local"),
            RemoteJobState::FallbackLocal,
        ),
        (
            "base_disagrees",
            Some(4),
            None,
            pair,
            Box::new(|_| {}),
            1,
            Some("queued"),
            RemoteJobState::FallbackLocal,
        ),
        (
            "aborted",
            Some(4),
            None,
            "| nothing\n",
            Box::new(|_| {}),
            1,
            Some("queued"),
            RemoteJobState::FallbackLocal,
        ),
        (
            "no_room",
            Some(5),
            Some(disk),
            "",
            Box::new(|_| {}),
            1,
            Some("queued"),
            RemoteJobState::FallbackLocal,
        ),
        // NB9: exit 5 without its fact, nothing measured here.
        (
            "aborted",
            Some(5),
            None,
            "",
            Box::new(|_| {}),
            1,
            Some("fallback_local"),
            RemoteJobState::FallbackLocal,
        ),
        (
            "aborted",
            Some(6),
            None,
            "",
            Box::new(|_| {}),
            1,
            Some("fallback_local"),
            RemoteJobState::FallbackLocal,
        ),
        (
            "wrapper_error",
            None,
            None,
            "",
            Box::new(|meta| meta.wrapper_error = Some("digest_mismatch".into())),
            1,
            Some("queued"),
            RemoteJobState::FallbackLocal,
        ),
        (
            "signal",
            None,
            None,
            "",
            Box::new(|meta| meta.signal = Some(Signal::Number(9))),
            1,
            Some("queued"),
            RemoteJobState::FallbackLocal,
        ),
        (
            "signal",
            Some(137),
            None,
            "",
            Box::new(|_| {}),
            1,
            Some("queued"),
            RemoteJobState::FallbackLocal,
        ),
        (
            "timed_out",
            None,
            None,
            "",
            Box::new(|meta| meta.timed_out = true),
            1,
            Some("fallback_local"),
            RemoteJobState::FallbackLocal,
        ),
        (
            "unread_version",
            Some(0),
            Some(r#"{"v":2,"verdict":"accepted"}"#),
            "",
            Box::new(|_| {}),
            0,
            Some("fallback_local"),
            RemoteJobState::FallbackLocal,
        ),
        // A self-fenced run is judged by nothing: the row stays, no strike.
        (
            "self_fenced",
            Some(0),
            Some("ACCEPTED"),
            "",
            Box::new(|meta| meta.self_fenced = true),
            0,
            None,
            RemoteJobState::Completed,
        ),
    ];
    for (index, (evidence, exit, fact, stdout, tweak, strikes, next, state)) in
        rows.into_iter().enumerate()
    {
        let name = format!("row{index}");
        let mut node = world.node(&name);
        let results = world.results();
        let fact = fact.map(str::to_string);
        let stdout = stdout.to_string();
        let ran = std::thread::spawn(move || {
            let grant = node.lease(Duration::from_secs(30)).expect("a grant");
            let verdict = fact.map(|fact| {
                if fact == "ACCEPTED" {
                    verdict_for("accepted", &grant)
                } else {
                    fact
                }
            });
            node.finish(&results, &grant, exit, verdict.as_deref(), &stdout, tweak);
            grant["job_id"].as_str().unwrap().to_string()
        });
        let outcome = world
            .primary
            .judge(&world.ledger.store, &world.request(), &|| false);
        let job = ran.join().expect("the node");
        let why = local_why(&outcome).to_string();
        let row = world
            .ledger
            .store
            .remote_job(&job)
            .expect("read")
            .expect("the job");
        assert_eq!(row.state, state, "{name} {evidence}: {why}");
        assert_eq!(
            world.ledger.strikes(&name),
            (strikes, false),
            "{name} {evidence}"
        );
        let faults: Vec<Value> = world
            .ledger
            .records("remote_job.host_fault")
            .into_iter()
            .filter(|fault| fault["job_id"] == job.as_str())
            .collect();
        match next {
            Some(next) => {
                assert_eq!(faults.len(), 1, "{name} {evidence}: {faults:?}");
                assert_eq!(faults[0]["evidence"], evidence, "{name}");
                assert_eq!(faults[0]["next"], next, "{name}");
                assert_eq!(faults[0]["strike"], strikes == 1, "{name}");
                assert!(faults[0].get("judging").is_none(), "F11: {name}");
            }
            None => assert!(faults.is_empty(), "{name} {evidence}: {faults:?}"),
        }
        if evidence == "aborted" && exit == Some(5) {
            assert!(!why.contains("measured"), "NB9: {why}");
        }
    }
    world.no_task_record();
}

/// Poison pill: a job that faults on two nodes is the cause; the first
/// node's strike is given back and the task is judged here.
#[test]
fn a_poison_pill_gives_the_first_node_its_strike_back_and_is_judged_here() {
    let world = Fixture::new("poison", false, Duration::from_secs(30));
    let mut first = world.node("na");
    let mut second = world.node("nb");
    let results = world.results();
    let ran = std::thread::spawn(move || {
        let grant = first.lease(Duration::from_secs(30)).expect("a grant");
        first.finish(&results, &grant, Some(2), None, "", |_| {});
        let again = second
            .lease(Duration::from_secs(30))
            .expect("granted again");
        second.finish(&results, &again, Some(137), None, "", |_| {});
    });
    let outcome = world
        .primary
        .judge(&world.ledger.store, &world.request(), &|| false);
    ran.join().expect("the nodes");
    local_why(&outcome);
    assert_eq!(world.ledger.strikes("na"), (0, false));
    assert_eq!(world.ledger.strikes("nb"), (0, false));
    let row = world.job();
    assert_eq!(row.state, RemoteJobState::FallbackLocal);
    assert_eq!(row.fault_nodes.len(), 2);
    let faults = world.ledger.records("remote_job.host_fault");
    assert_eq!(faults.last().unwrap()["poison"], true);
    world.no_task_record();
}

/// No node leases the job within its wait: the primary takes it back (the
/// fenced compare-and-set wins) and the task is judged here, after the
/// whole wait and not before.
#[test]
fn a_job_no_node_leases_in_its_wait_is_taken_back_and_judged_here() {
    let world = Fixture::new("no_lease", false, Duration::from_millis(800));
    let started = Instant::now();
    let outcome = world
        .primary
        .judge(&world.ledger.store, &world.request(), &|| false);
    assert!(started.elapsed() >= Duration::from_millis(800));
    let why = local_why(&outcome);
    assert!(why.contains("taken back"), "{why}");
    let row = world.job();
    assert_eq!(row.state, RemoteJobState::FallbackLocal);
    // A node beating afterwards is granted nothing.
    let mut late = world.node("late");
    assert!(late.beat().is_empty());
    world.no_task_record();
    // The default wait is 30 s.
    assert_eq!(remote::queue_wait(None), Duration::from_secs(30));
}

/// The take-back races a node's grant: the node wins, so the take-back
/// fails ([`Look::Raced`], never a local judging) and the primary waits
/// for the node's result, which it believes.
#[test]
fn a_take_back_that_loses_to_a_lease_waits_for_the_nodes_result() {
    let world = Fixture::new("race", false, Duration::ZERO);
    let job = world.primary.publish(&world.request()).expect("published");
    let job_id = remote::enqueue(&world.ledger.store, &job).expect("queued");
    // The integrator read it queued, past its wait ...
    let stale = world
        .ledger
        .store
        .remote_job(&job_id)
        .expect("read")
        .expect("the job");
    assert_eq!(stale.state, RemoteJobState::Queued);
    // ... and a node was granted it before the take-back.
    let mut node = world.node("n1");
    let grant = node.beat().into_iter().next().expect("a grant");
    assert_eq!(
        remote::look(&world.ledger.store, &stale, Instant::now(), Duration::ZERO),
        Ok(Look::Raced)
    );
    let row = world.job();
    assert_eq!(row.state, RemoteJobState::Leased);
    // Its result is waited for, whatever the queue wait.
    let results = world.results();
    let ran = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        let verdict = verdict_for("accepted", &grant);
        node.finish(&results, &grant, Some(0), Some(&verdict), "", |_| {});
    });
    let outcome = follow(&world, &job);
    ran.join().expect("the node");
    assert!(
        matches!(&outcome, PrimaryOutcome::Believed(believed) if believed.verdict.accepted),
        "{outcome:?}"
    );
}

fn follow(world: &Fixture, job: &GateJob) -> PrimaryOutcome {
    world.primary.follow(
        &world.ledger.store,
        7,
        &job.job_id,
        &job.inputs,
        false,
        &|| false,
    )
}

/// An owner's urgent landing stops the judging while it waits: a queued
/// job is taken back (no node runs it for nothing), a leased one is left
/// to its node and its result never read.
#[test]
fn a_judging_stopped_while_it_waits_takes_its_queued_job_back() {
    let world = Fixture::new("stopped", false, Duration::from_secs(30));
    let job = world.primary.publish(&world.request()).expect("published");
    remote::enqueue(&world.ledger.store, &job).expect("queued");
    let since = Instant::now();
    let stop = || since.elapsed() > Duration::from_millis(200);
    let outcome = world.primary.follow(
        &world.ledger.store,
        7,
        &job.job_id,
        &job.inputs,
        false,
        &stop,
    );
    assert!(matches!(outcome, PrimaryOutcome::Stopped), "{outcome:?}");
    assert!(
        since.elapsed() < Duration::from_secs(10),
        "not the whole wait"
    );
    assert_eq!(world.job().state, RemoteJobState::FallbackLocal);

    let leased = Fixture::new("stopped_leased", false, Duration::from_secs(30));
    let job = leased
        .primary
        .publish(&leased.request())
        .expect("published");
    remote::enqueue(&leased.ledger.store, &job).expect("queued");
    let mut node = leased.node("n1");
    assert_eq!(node.beat().len(), 1);
    let since = Instant::now();
    let stop = || since.elapsed() > Duration::from_millis(200);
    let outcome = leased.primary.follow(
        &leased.ledger.store,
        7,
        &job.job_id,
        &job.inputs,
        false,
        &stop,
    );
    assert!(matches!(outcome, PrimaryOutcome::Stopped), "{outcome:?}");
    assert_eq!(leased.job().state, RemoteJobState::Leased);
    leased.no_task_record();
}

/// A lease the sweep fences back to the queue gets a whole wait again,
/// and goes to another node.
#[test]
fn a_lease_fenced_back_to_the_queue_is_waited_for_again() {
    let world = Fixture::new("fenced", false, Duration::from_millis(1500));
    let job = world.primary.publish(&world.request()).expect("published");
    remote::enqueue(&world.ledger.store, &job).expect("queued");
    let mut first = world.node("na");
    assert_eq!(first.beat().len(), 1);
    let mut second = world.node("nb");
    let results = world.results();
    let admin = world.ledger.admin();
    let id = job.job_id.clone();
    let ran = std::thread::spawn(move || {
        let mut admin = admin;
        std::thread::sleep(Duration::from_millis(400));
        // Fenced back to the queue (as the sweep does a silent lease).
        admin
            .execute(
                "update spawn_remote_jobs set state = 'queued', lease_epoch = lease_epoch + 1,
                        node_id = null, boot_id = null, grant_seq = null
                  where job_id = $1",
                &[&id],
            )
            .expect("fenced");
        std::thread::sleep(Duration::from_millis(700));
        let grant = second.lease(Duration::from_secs(30)).expect("a grant");
        let verdict = verdict_for("accepted", &grant);
        second.finish(&results, &grant, Some(0), Some(&verdict), "", |_| {});
    });
    let outcome = follow(&world, &job);
    ran.join().expect("the node");
    let env = believed(outcome).env;
    assert_eq!(env["runner"], "node:nb");
}

/// A queued `gate` job a predecessor left is taken back at the first pass
/// of an integrator with the primary path on; never by one with it off.
#[test]
fn a_predecessors_queued_gate_job_is_taken_back_at_the_first_pass() {
    let world = Fixture::new("orphans", false, Duration::from_secs(30));
    let job = world.primary.publish(&world.request()).expect("published");
    let id = remote::enqueue(&world.ledger.store, &job).expect("queued");
    let off = Integrator::new(
        world.dir.0.join("host-off"),
        world.dir.0.join("gate.sh"),
        Vec::new(),
        room_to_spare(),
        super::DEFAULT_GATES,
    );
    off.pass_on(&world.ledger.store).expect("a pass");
    assert_eq!(world.job().state, RemoteJobState::Queued);
    let on = Integrator::new(
        world.dir.0.join("host"),
        world.dir.0.join("gate.sh"),
        Vec::new(),
        room_to_spare(),
        super::DEFAULT_GATES,
    )
    .with_remote_primary(
        settings(&world.dir.0, false, Duration::from_secs(30)),
        world.dir.0.join("work-on"),
    );
    on.pass_on(&world.ledger.store).expect("a pass");
    let row = world
        .ledger
        .store
        .remote_job(&id)
        .expect("read")
        .expect("the job");
    assert_eq!(row.state, RemoteJobState::FallbackLocal);
    // Once: a job queued later is this integrator's own business.
    let later = remote::enqueue(
        &world.ledger.store,
        &world.primary.publish(&world.request()).expect("published"),
    )
    .expect("queued");
    on.pass_on(&world.ledger.store).expect("the next pass");
    assert_eq!(
        world
            .ledger
            .store
            .remote_job(&later)
            .expect("read")
            .expect("the job")
            .state,
        RemoteJobState::Queued
    );
    assert_eq!(
        remote::next_for(RemoteJobKind::Gate, FaultNext::FallbackLocal),
        FaultNext::FallbackLocal
    );
}

// ---------------------------------------------------------------------
// End to end: the integrator's pass, a mission, a task and its work.
// ---------------------------------------------------------------------

/// The checks before a gate with their defaults, on a host with room to
/// spare.
fn room_to_spare() -> crate::gate::Room {
    crate::gate::Room::from_env(|_| None).measured_by(
        |_| Some(u64::MAX),
        || Some(u64::MAX),
        |_| Ok(()),
    )
}

/// A local stub gate that notes each run in `runs` beside it and gives
/// `verdict`.
fn noting_gate(verdict: &str) -> String {
    format!(
        "echo ran >>\"$(dirname \"$0\")/runs\"\n\
         printf '{{\"verdict\":\"{verdict}\",\"reason\":\"stub\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n"
    )
}

/// A source with one commit on main, a controller whose scenarios are the
/// mission's gate directory, a reported task with its work pushed, and an
/// integrator judging with `gate` here and, when `remote` is given, on a
/// node first.
struct World {
    ledger: Ledger,
    dir: Scratch,
    source: String,
    controller: PathBuf,
    cargo: PathBuf,
    results: PathBuf,
    integrator: Integrator,
    mission: i64,
    task: i64,
    seed: std::cell::Cell<u8>,
}

impl World {
    fn new(case: &str, gate: &str, remote: Option<(bool, Duration)>) -> Self {
        let git = crate::sources::test_git;
        let ledger = Ledger::new(&format!("primary_e2e_{case}"));
        let dir = Scratch::new(&format!("primary-e2e-{}", case.replace('_', "-")));
        let seed = dir.0.join("seed");
        std::fs::create_dir_all(&seed).expect("seed");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        super::tests::commit_in(&seed_path, "base");
        let source = dir.0.join("source.git").to_string_lossy().to_string();
        git(&["clone", "-q", "--bare", &seed_path, &source]).expect("bare source");
        let controller = fixtures::controller(&dir.0);
        let scenarios = controller.join("rsi/scenarios");
        let gate_path = dir.0.join("gate.sh");
        std::fs::write(&gate_path, gate).expect("gate");
        let path_env = std::env::var("PATH")
            .map(|path| vec![("PATH".to_string(), path)])
            .unwrap_or_default();
        let settings = settings(&dir.0, false, Duration::from_secs(30));
        let (cargo, results) = (settings.cargo_home.clone(), settings.results.clone());
        let mut integrator = Integrator::new(
            dir.0.join("host"),
            gate_path,
            path_env,
            room_to_spare(),
            super::DEFAULT_GATES,
        );
        if let Some((believe_reject, wait)) = remote {
            integrator = integrator.with_remote_primary(
                PrimarySettings {
                    believe_reject,
                    queue_wait: wait,
                    ..settings
                },
                dir.0.join("host/.remote-node"),
            );
        }
        let store = &ledger.store;
        let mission = store
            .mission_add("gated", &json!([source]), "human")
            .expect("add");
        store
            .mission_configure(
                mission,
                Some(&scenarios.to_string_lossy()),
                "gate",
                None,
                None,
            )
            .expect("configure");
        let task = store
            .task_assign(mission, "ephemeral", "w", "brief", "default")
            .expect("assign");
        store.task_delivered(task, 1).expect("delivered");
        assert!(store.task_reported(task, "done").expect("reported"));
        let world = Self {
            ledger,
            dir,
            source,
            controller,
            cargo,
            results,
            integrator,
            mission,
            task,
            seed: std::cell::Cell::new(40),
        };
        world.push_work();
        world
    }

    fn agent(&self) -> String {
        let board = self.ledger.store.mission_board(50).expect("board");
        board["missions"]
            .as_array()
            .expect("missions")
            .iter()
            .find(|mission| mission["id"] == self.mission)
            .and_then(|mission| mission["tasks"].as_array())
            .and_then(|tasks| tasks.iter().find(|task| task["id"] == self.task))
            .and_then(|task| task["agent_id"].as_str())
            .expect("the task's agent")
            .to_string()
    }

    fn push_work(&self) {
        let git = crate::sources::test_git;
        let clone = self.dir.0.join("agent");
        let clone_path = clone.to_string_lossy().to_string();
        git(&["clone", "-q", &self.source, &clone_path]).expect("clone");
        std::fs::write(clone.join("work.txt"), "work\n").expect("write");
        super::tests::commit_in(&clone_path, "work");
        git(&[
            "-C",
            &clone_path,
            "push",
            "-q",
            "origin",
            &format!("HEAD:refs/heads/spawn/{}", self.agent()),
        ])
        .expect("push the agent's branch");
    }

    fn node(&self, name: &str) -> NodeSim {
        let seed = self.seed.get();
        self.seed.set(seed + 1);
        let node = self
            .ledger
            .enroll(name, seed, &holds(&self.controller, &self.cargo));
        NodeSim {
            store: Arc::clone(&self.ledger.store),
            node,
            seq: 0,
        }
    }

    /// One pass and the judgings it started, to their records.
    fn pass(&self) {
        self.integrator.pass_on(&self.ledger.store).expect("a pass");
        self.integrator.settle();
    }

    fn local_runs(&self) -> usize {
        std::fs::read_to_string(self.dir.0.join("runs")).map_or(0, |runs| runs.lines().count())
    }

    fn gated(&self) -> Vec<Value> {
        self.ledger.records("task.gated")
    }

    /// The mission's counts toward its halts: aborts, rejections and host
    /// faults in a row.
    fn counts(&self) -> (i32, i32, i32) {
        let row = self
            .ledger
            .admin()
            .query_one(
                "select gate_aborts, gate_rejections, gate_host_faults
                   from mission_missions where id = $1",
                &[&self.mission],
            )
            .expect("the mission");
        (row.get(0), row.get(1), row.get(2))
    }

    fn gate_jobs(&self) -> Vec<(String, String)> {
        self.ledger
            .admin()
            .query(
                "select job_id, state from spawn_remote_jobs where kind = 'gate' order by job_id",
                &[],
            )
            .expect("jobs")
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect()
    }
}

/// A node that accepts the job it is granted, on its own inputs.
fn accepting(
    mut node: NodeSim,
    results: PathBuf,
    word: &'static str,
) -> std::thread::JoinHandle<Value> {
    std::thread::spawn(move || {
        let grant = node.lease(Duration::from_mins(1)).expect("a grant");
        let verdict = verdict_for(word, &grant);
        node.finish(
            &results,
            &grant,
            Some(0),
            Some(&verdict),
            "| node gate\n",
            |_| {},
        );
        grant
    })
}

/// Off (the default): the pass judges on this host exactly as before and
/// queues nothing for a node.
#[test]
fn off_the_pass_judges_here_and_queues_nothing() {
    let world = World::new("off", &noting_gate("accepted"), None);
    world.pass();
    assert_eq!(world.local_runs(), 1);
    let gated = world.gated();
    assert_eq!(gated.len(), 1, "{gated:?}");
    assert_eq!(gated[0]["verdict"], "accepted");
    assert_eq!(gated[0]["env"]["runner"], Value::Null);
    assert!(world.gate_jobs().is_empty());
    assert_eq!(world.ledger.records("task.landed").len(), 1);
}

/// M3a end to end: the node's acceptance is the task's verdict (no local
/// gate runs), recorded with what the node judged and where, and the task
/// lands.
#[test]
fn m3a_the_nodes_acceptance_is_the_tasks_verdict_and_it_lands() {
    let world = World::new(
        "m3a_lands",
        &noting_gate("rejected"),
        Some((false, Duration::from_secs(30))),
    );
    let node = accepting(world.node("n1"), world.results.clone(), "accepted");
    world.pass();
    let grant = node.join().expect("the node");
    assert_eq!(world.local_runs(), 0, "no local gate");
    let gated = world.gated();
    assert_eq!(gated.len(), 1, "{gated:?}");
    assert_eq!(gated[0]["verdict"], "accepted");
    assert_eq!(gated[0]["env"]["runner"], "node:n1");
    assert_eq!(gated[0]["inputs"], granted_inputs(&grant));
    assert_eq!(gated[0]["candidate"], granted_inputs(&grant)["candidate"]);
    assert_eq!(world.ledger.records("task.landed").len(), 1);
    assert_eq!(world.ledger.strikes("n1"), (0, false));
    assert_eq!(world.counts(), (0, 0, 0));
    // The job names its judging `judging_id` (F11) and the task.
    let row = world
        .ledger
        .store
        .remote_job(grant["job_id"].as_str().unwrap())
        .expect("read")
        .expect("the job");
    assert_eq!(row.task_id, Some(world.task));
    assert_eq!(row.judging_id.as_deref(), gated[0]["judging"].as_str());
}

/// M3a end to end: the node's rejection is judged again here (the local
/// gate accepts: the task's only verdict is that one); M3b believes it
/// (no local gate, the task rejected).
#[test]
fn a_nodes_rejection_is_judged_again_under_m3a_and_believed_under_m3b() {
    let m3a = World::new(
        "m3a_rejudges",
        &noting_gate("accepted"),
        Some((false, Duration::from_secs(30))),
    );
    let node = accepting(m3a.node("n1"), m3a.results.clone(), "rejected");
    m3a.pass();
    node.join().expect("the node");
    assert_eq!(m3a.local_runs(), 1);
    let gated = m3a.gated();
    assert_eq!(gated.len(), 1, "{gated:?}");
    assert_eq!(gated[0]["verdict"], "accepted");
    assert_eq!(gated[0]["env"]["runner"], Value::Null, "judged here");
    assert_eq!(m3a.ledger.strikes("n1"), (0, false));
    assert_eq!(m3a.counts(), (0, 0, 0));

    let m3b = World::new(
        "m3b_believes",
        &noting_gate("accepted"),
        Some((true, Duration::from_secs(30))),
    );
    let node = accepting(m3b.node("n1"), m3b.results.clone(), "rejected");
    m3b.pass();
    node.join().expect("the node");
    assert_eq!(m3b.local_runs(), 0, "no local gate");
    let gated = m3b.gated();
    assert_eq!(gated.len(), 1, "{gated:?}");
    assert_eq!(gated[0]["verdict"], "rejected");
    assert_eq!(gated[0]["env"]["runner"], "node:n1");
    assert_eq!(m3b.counts().1, 1, "a believed rejection counts as one");
}

/// No node within the wait: taken back, judged here; the node's faults
/// (an exit 5 without its fact, then a signal on a requeue) are settled on
/// the ledger and the task's only record is the local verdict: no abort,
/// host fault or `no_room`, nothing toward a halt.
#[test]
fn a_nodes_trouble_spends_nothing_of_the_missions_budget() {
    let idle = World::new(
        "no_node",
        &noting_gate("accepted"),
        Some((false, Duration::from_millis(500))),
    );
    idle.pass();
    assert_eq!(idle.local_runs(), 1);
    let jobs = idle.gate_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].1, "fallback_local");
    assert_eq!(idle.gated().len(), 1);
    assert_eq!(idle.gated()[0]["verdict"], "accepted");

    for (case, exit) in [("exit_5", 5), ("exit_2", 2), ("exit_6", 6)] {
        let world = World::new(
            case,
            &noting_gate("accepted"),
            Some((false, Duration::from_millis(500))),
        );
        let mut node = world.node("n1");
        let results = world.results.clone();
        let ran = std::thread::spawn(move || {
            let grant = node.lease(Duration::from_mins(1)).expect("a grant");
            node.finish(&results, &grant, Some(exit), None, "| boom\n", |_| {});
        });
        world.pass();
        ran.join().expect("the node");
        assert_eq!(world.local_runs(), 1, "{case}");
        let gated = world.gated();
        assert_eq!(gated.len(), 1, "{case}: {gated:?}");
        assert_eq!(gated[0]["verdict"], "accepted", "{case}");
        assert_eq!(world.counts(), (0, 0, 0), "{case}");
        assert_eq!(world.ledger.strikes("n1"), (1, false), "{case}");
        assert_eq!(world.gate_jobs()[0].1, "fallback_local", "{case}");
        for kind in ["task.halted", "mission.halted", "task.aborted"] {
            assert!(world.ledger.records(kind).is_empty(), "{case} {kind}");
        }
    }
}

/// An owner's urgent landing while the judging waits for a node: the
/// judging is discarded (`stale`, found `discarded`), counted nowhere, its
/// queued job taken back and no gate run here.
#[test]
fn an_urgent_landing_discards_a_judging_waiting_for_a_node() {
    let world = World::new(
        "urgent",
        &noting_gate("accepted"),
        Some((false, Duration::from_mins(1))),
    );
    world
        .integrator
        .pass_on(&world.ledger.store)
        .expect("a pass");
    let deadline = Instant::now() + Duration::from_secs(30);
    while world.gate_jobs().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(world.gate_jobs().len(), 1, "queued for a node");
    for running in world.integrator.running.borrow().iter() {
        assert!(running.stop.stop(4242, Some("hotfix".to_string())));
    }
    world.integrator.settle();
    assert_eq!(world.local_runs(), 0);
    let gated = world.gated();
    assert_eq!(gated.len(), 1, "{gated:?}");
    assert_eq!(gated[0]["verdict"], "stale");
    assert_eq!(gated[0]["found"], "discarded");
    assert_eq!(world.counts(), (0, 0, 0));
    assert_eq!(world.gate_jobs()[0].1, "fallback_local");
}

/// The node shadow stays its own switch beside the primary path: its
/// directories are its own and its settings are untouched by it.
#[test]
fn the_node_shadow_is_orthogonal_to_the_primary_path() {
    let lookup = |name: &str| match name {
        "SPAWN_GATE_REMOTE" => Some("node".to_string()),
        "SPAWN_NODE_ARTIFACTS" => Some("/a".to_string()),
        "SPAWN_NODE_RESULTS" => Some("/r".to_string()),
        "CARGO_HOME" => Some("/c".to_string()),
        _ => None,
    };
    assert_eq!(shadow::node_settings(lookup), Ok(None));
    assert!(
        remote::primary_settings(lookup)
            .expect("settings")
            .is_some()
    );
    let both = |name: &str| {
        if name == "SPAWN_GATE_SHADOW_NODE" {
            Some("1".to_string())
        } else {
            lookup(name)
        }
    };
    assert!(shadow::node_settings(both).expect("shadow").is_some());
    assert!(remote::primary_settings(both).expect("primary").is_some());
}

/// A red main: its gate runs the red check on the base first
/// (`SPAWN_GATE_BASE_CHECK`), which no grant can carry, so a probe of a red
/// main with the primary path on queues no job: the gate runs here, given
/// the check, and its pass on the base records `source.main_green`.
#[test]
fn a_gate_on_a_red_main_is_judged_here_and_finds_it_green() {
    let gate = "echo \"base_check=${SPAWN_GATE_BASE_CHECK-}\" >>\"$(dirname \"$0\")/runs\"\n\
                printf '{\"verdict\":\"accepted\",\"base_check\":\"check-tests.sh\"}\\n' \
                >\"$SPAWN_GATE_VERDICT\"\n";
    let mut world = World::new("red_main", gate, Some((false, Duration::from_secs(30))));
    let main =
        crate::sources::test_git(&["--git-dir", &world.source, "rev-parse", "refs/heads/main"])
            .expect("main")
            .trim()
            .to_string();
    world
        .ledger
        .store
        .integrator_record(&json!({
            "type": "source.main_red", "source": world.source, "main": main,
            "scenario": "check-tests.sh", "runs": "1, 1",
            "failures": [{"test": "api::tests::adds", "message": "m", "class": "work"}],
            "task": world.task, "mission": world.mission,
        }))
        .expect("main is red");
    // The probe is due at once.
    world.integrator.red_probe_ms = 1;
    std::thread::sleep(Duration::from_millis(5));
    // A node that would take any job is beating beside the pass.
    let mut node = world.node("n1");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let beating = std::thread::spawn(move || {
        let mut granted = Vec::new();
        while !stopped.load(std::sync::atomic::Ordering::SeqCst) {
            granted.extend(node.beat());
            std::thread::sleep(Duration::from_millis(20));
        }
        granted
    });
    world.pass();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(
        beating.join().expect("the node").is_empty(),
        "nothing granted"
    );
    assert!(world.gate_jobs().is_empty(), "no gate job queued");
    let runs = std::fs::read_to_string(world.dir.0.join("runs")).unwrap_or_default();
    assert_eq!(
        runs.lines().collect::<Vec<_>>(),
        ["base_check=check-tests.sh"],
        "the local gate, given the red check"
    );
    let greens = world.ledger.records("source.main_green");
    assert_eq!(greens.len(), 1, "{greens:?}");
    assert_eq!(greens[0]["scenario"], "check-tests.sh");
    let gated = world.gated();
    assert_eq!(gated.len(), 1, "{gated:?}");
    assert_eq!(gated[0]["verdict"], "accepted");
    assert_eq!(gated[0]["probe"], true);
    assert_eq!(gated[0]["env"]["runner"], Value::Null, "judged here");
}
