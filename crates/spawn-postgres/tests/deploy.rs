//! Deploy-on-landing (#54): what deploy/on-landing.sh decides on
//! (`PostgresStore::main_landing`) and how a deploy ended (`deploy.finished`,
//! through the owner-only `spawn_deploy_record`).
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};
use spawn_postgres::{PostgresStore, role_url};

mod common;
use common::{admin, schema_url, store};

const SOURCE: &str = "/tmp/deploy-live.git";
const ELSEWHERE: &str = "/tmp/deploy-other.git";

/// A commit id made of one hex digit.
fn commit(digit: char) -> String {
    digit.to_string().repeat(40)
}

/// A gated mission on `source` with a task its gate accepted.
fn accepted_task(store: &PostgresStore, source: &str) -> i64 {
    let mission = store
        .mission_add("deployed work", &json!([source]), "human")
        .expect("add");
    store
        .mission_configure(mission, Some("/scenarios"), "gate", None, None)
        .expect("configure");
    let task = store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    store.task_delivered(task, 1).expect("delivered");
    assert!(store.task_reported(task, "done").expect("reported"));
    store
        .integrator_record(&json!({"type": "task.gated", "id": task, "revision": 0,
                                    "verdict": "accepted", "base": commit('0'),
                                    "candidate": commit('1')}))
        .expect("accepted");
    task
}

/// A control record as the owner's push or a deploy writes it.
fn control(client: &mut Client, record: &Value) -> i64 {
    client
        .query_one("select spawn_control_record($1)", &[record])
        .expect("a control record")
        .get(0)
}

fn landing(store: &PostgresStore, main: &str, deployed: Option<&str>) -> Value {
    store
        .main_landing(SOURCE, main, deployed)
        .expect("main's landing")
}

/// Main's landing is gate-accepted only as a `task.landed` of a task of a
/// mission on the source or an owner's `source.moved` there with verdict
/// accepted, of exactly that commit; a deploy's own move, a landing on
/// another source and a commit nothing landed are none. The first landing
/// after the deployed commit's is when the max-wait counts from; the hold
/// and the last deploy are read beside them.
#[test]
fn main_is_deployed_only_from_a_gate_accepted_landing_of_it_on_the_source() {
    let (store, schema) = store("deploy_landing");
    let mut client = admin(&schema);
    let task = accepted_task(&store, SOURCE);
    let other_task = accepted_task(&store, ELSEWHERE);

    // Nothing landed yet.
    let read = landing(&store, &commit('a'), None);
    assert_eq!(read["landing"], Value::Null, "{read}");
    assert_eq!(read["first_undeployed_ms"], Value::Null);
    assert_eq!(read["hold"], Value::Null);
    assert_eq!(read["last_deploy"], Value::Null);
    assert!(read["now_ms"].as_i64().is_some(), "{read}");

    // A deploy moved main to d (no verdict: a deploy's move is no landing).
    store
        .source_moved(SOURCE, &commit('0'), &commit('d'))
        .expect("the deploy's move");
    assert_eq!(landing(&store, &commit('d'), None)["landing"], Value::Null);

    // The task's landing of a, and another source's task landing b.
    let landed = store
        .integrator_record(
            &json!({"type": "task.landed", "id": task, "from": commit('d'),
                                    "to": commit('a')}),
        )
        .expect("landed");
    store
        .integrator_record(&json!({"type": "task.landed", "id": other_task,
                                    "from": commit('0'), "to": commit('b')}))
        .expect("landed elsewhere");
    let read = landing(&store, &commit('a'), Some(&commit('d')));
    assert_eq!(read["landing"]["record"], "task.landed", "{read}");
    assert_eq!(read["landing"]["event"], landed);
    assert_eq!(read["landing"]["task"], task);
    assert_eq!(
        read["first_undeployed_ms"], read["landing"]["at_ms"],
        "a is the first landing after d's"
    );
    assert_eq!(
        landing(&store, &commit('b'), None)["landing"],
        Value::Null,
        "another source's landing deploys nothing here"
    );

    // The owner's gated push lands c here and e on the other source; an
    // owner's move without the gate's verdict lands f.
    let pushed = control(
        &mut client,
        &json!({"type": "source.moved", "source": SOURCE, "from": commit('a'), "to": commit('c'),
                "request": 7, "by": "human", "verdict": "accepted"}),
    );
    control(
        &mut client,
        &json!({"type": "source.moved", "source": ELSEWHERE, "from": commit('b'),
                "to": commit('e'), "request": 8, "by": "human", "verdict": "accepted"}),
    );
    control(
        &mut client,
        &json!({"type": "source.moved", "source": SOURCE, "from": commit('c'), "to": commit('f'),
                "request": 9, "by": "human", "verdict": "rejected"}),
    );
    let read = landing(&store, &commit('c'), Some(&commit('d')));
    assert_eq!(read["landing"]["record"], "source.moved", "{read}");
    assert_eq!(read["landing"]["event"], pushed);
    assert_eq!(read["landing"]["request"], 7);
    assert_eq!(
        read["first_undeployed_ms"],
        landing(&store, &commit('a'), None)["landing"]["at_ms"],
        "the max-wait counts from the first landing after the deployed one, a"
    );
    assert_eq!(landing(&store, &commit('e'), None)["landing"], Value::Null);
    assert_eq!(
        landing(&store, &commit('f'), None)["landing"],
        Value::Null,
        "a move without the gate's acceptance"
    );
    let read = landing(&store, &commit('c'), Some(&commit('c')));
    assert_eq!(
        read["first_undeployed_ms"],
        Value::Null,
        "nothing accepted landed after the deployed c"
    );

    // The hold and the last deploy.
    let lock = store.source_hold_lock(SOURCE).expect("the landing lock");
    let held = store
        .source_hold(SOURCE, &commit('c'), 60, Some("deploy-on-landing c"))
        .expect("hold");
    drop(lock);
    store
        .deploy_record(
            &json!({"type": "deploy.finished", "ok": false, "head": commit('c'),
                               "by": "landing", "reason": "the build failed"}),
        )
        .expect("a failed deploy");
    let read = landing(&store, &commit('c'), None);
    assert_eq!(read["hold"]["hold"], held["hold"], "{read}");
    assert_eq!(read["hold"]["reason"], "deploy-on-landing c");
    assert_eq!(read["last_deploy"]["ok"], false);
    assert_eq!(read["last_deploy"]["head"], commit('c'));
    assert_eq!(read["last_deploy"]["by"], "landing");
    store.source_release(SOURCE).expect("release");
    assert_eq!(landing(&store, &commit('c'), None)["hold"], Value::Null);
}

/// A deploy record has exactly its five fields, each checked: nothing from a
/// deploy's environment reaches it but the reason's text, at most 500
/// characters.
#[test]
fn a_deploy_record_has_its_fields_and_nothing_else() {
    let (store, schema) = store("deploy_record");
    let mut client = admin(&schema);
    let good = json!({"type": "deploy.finished", "ok": true, "head": commit('a'),
                      "by": "owner", "reason": ""});
    store.deploy_record(&good).expect("a deploy record");
    for (bad, why) in [
        (json!({"type": "deploy.started", "ok": true}), "the type"),
        (
            json!({"type": "deploy.finished", "ok": true, "head": commit('a'), "by": "owner",
                   "reason": "", "env": "SPAWN_DATABASE_URL=postgres://u:p@h/db"}),
            "a field more",
        ),
        (
            json!({"type": "deploy.finished", "ok": "yes", "head": commit('a'), "by": "owner",
                   "reason": ""}),
            "ok not a boolean",
        ),
        (
            json!({"type": "deploy.finished", "ok": true, "head": "HEAD", "by": "owner",
                   "reason": ""}),
            "head not a commit",
        ),
        (
            json!({"type": "deploy.finished", "ok": true, "head": commit('a'), "by": "agent",
                   "reason": ""}),
            "by neither landing nor owner",
        ),
        (
            json!({"type": "deploy.finished", "ok": false, "head": commit('a'),
                   "by": "landing", "reason": "x".repeat(501)}),
            "a reason too long",
        ),
        (
            json!({"type": "deploy.finished", "ok": false, "head": commit('a'),
                   "by": "landing"}),
            "no reason",
        ),
    ] {
        assert!(store.deploy_record(&bad).is_err(), "{why}: {bad}");
    }
    let count: i64 = client
        .query_one(
            "select count(*) from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' = 'deploy.finished'",
            &[],
        )
        .expect("count")
        .get(0);
    assert_eq!(count, 1, "only the good one was recorded");
    // The fold takes it again (folded into no table).
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
}

/// Only the owner records deploys.
#[test]
fn only_the_owner_records_deploys() {
    let (_store, schema) = store("deploy_owner");
    let login = role_url(&schema_url(&schema), "spawn_supervisor").expect("the login's URL");
    let supervisor = PostgresStore::connect(&login).expect("log in");
    let refused = supervisor
        .deploy_record(
            &json!({"type": "deploy.finished", "ok": true, "head": commit('a'),
                               "by": "owner", "reason": ""}),
        )
        .expect_err("a restricted login records no deploy");
    let message = refused.to_string();
    assert!(
        message.contains("permission denied for function spawn_deploy_record")
            || message.contains("may not record deploys"),
        "{refused}"
    );
}

/// S23: the owner's landing requests with no outcome yet, waiting for their
/// judging or in it, are what `owner_requests` lists, oldest first, each
/// `{request, commit, at_ms}`; a request on another source is not there; an
/// outcome naming one (`source.push_refused`, or a `source.moved` with its
/// `request`) takes it out, and a `source.push_no_verdict` does not. They
/// are the integrator's queue on the source (`source_pushes_pending`, the
/// one SQL). deploy/on-landing.sh waits for the first whose commit main
/// does not hold.
#[test]
fn an_owners_landing_request_without_an_outcome_is_named_until_it_has_one() {
    let (store, schema) = store("deploy_owner_request");
    let mut client = admin(&schema);
    accepted_task(&store, SOURCE);
    accepted_task(&store, ELSEWHERE);
    let ask = |source: &str, digit: char| {
        let commit = commit(digit);
        store
            .source_push_request(
                source,
                &commit,
                &format!("refs/spawn-owner/{commit}"),
                &spawn_postgres::PushRequest {
                    gated: true,
                    urgent: false,
                    reason: None,
                },
            )
            .expect("the owner's request")
    };
    // The requests `owner_requests` lists on `source`, oldest first, after
    // checking they are the integrator's queue there (one SQL).
    let pending = |source: &str, main: &str| -> Vec<Value> {
        let read = store.main_landing(source, main, None).expect("read");
        let listed: Vec<Value> = read["owner_requests"]
            .as_array()
            .unwrap_or_else(|| panic!("a list: {read}"))
            .iter()
            .map(|request| request["request"].clone())
            .collect();
        let queue: Vec<Value> = store
            .source_pushes_pending()
            .expect("the queue")
            .as_array()
            .expect("a list")
            .iter()
            .filter(|request| request["source"] == source)
            .map(|request| request["request"].clone())
            .collect();
        assert_eq!(listed, queue, "the integrator's queue on {source}");
        listed
    };
    assert!(pending(SOURCE, &commit('a')).is_empty());

    let elsewhere = ask(ELSEWHERE, 'e');
    assert!(
        pending(SOURCE, &commit('a')).is_empty(),
        "another source's request holds nothing here"
    );
    let first = ask(SOURCE, 'b');
    let second = ask(SOURCE, 'c');
    assert_eq!(pending(SOURCE, &commit('a')), [json!(first), json!(second)]);
    let read = landing(&store, &commit('a'), None);
    assert_eq!(read["owner_requests"][0]["commit"], commit('b'), "{read}");
    assert!(
        read["owner_requests"][0]["at_ms"].as_i64().is_some(),
        "{read}"
    );

    // A judging that ended with no verdict leaves it pending.
    control(
        &mut client,
        &json!({"type": "source.push_no_verdict", "request": first, "reason": "host fault",
                "judging": "0123456789abcdef"}),
    );
    assert_eq!(pending(SOURCE, &commit('a')), [json!(first), json!(second)]);

    // Refused: the next one is first.
    control(
        &mut client,
        &json!({"type": "source.push_refused", "request": first, "reason": "a conflict"}),
    );
    assert_eq!(pending(SOURCE, &commit('a')), [json!(second)]);
    // Applied: a source.moved naming it; none is left on the source.
    control(
        &mut client,
        &json!({"type": "source.moved", "source": SOURCE, "from": commit('a'), "to": commit('c'),
                "request": second, "by": "human", "verdict": "accepted"}),
    );
    assert!(pending(SOURCE, &commit('c')).is_empty());
    // The other source's request is still its own.
    assert_eq!(pending(ELSEWHERE, &commit('a')), [json!(elsewhere)]);
}

/// `judging` counts the source's tasks between their report and their
/// landing (reported, accepted, landing): a task still at work, another
/// source's task and a landed one are none. deploy/on-landing.sh keeps its
/// quiet spell only while it is above 0.
#[test]
fn judging_counts_the_sources_tasks_between_report_and_landing() {
    let (store, _schema) = store("deploy_judging");
    let judging = || landing(&store, &commit('a'), None)["judging"].clone();
    assert_eq!(judging(), json!(0), "nothing on the source");

    let mission = store
        .mission_add("deployed work", &json!([SOURCE]), "human")
        .expect("add");
    store
        .mission_configure(mission, Some("/scenarios"), "gate", None, None)
        .expect("configure");
    let task = store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    store.task_delivered(task, 1).expect("delivered");
    assert_eq!(judging(), json!(0), "a task at work is not judged yet");
    assert!(store.task_reported(task, "done").expect("reported"));
    assert_eq!(judging(), json!(1), "a reported task waits for its gate");

    accepted_task(&store, ELSEWHERE);
    assert_eq!(judging(), json!(1), "another source's task is not counted");

    let accepted = accepted_task(&store, SOURCE);
    assert_eq!(
        judging(),
        json!(2),
        "an accepted task waits for its landing"
    );

    store
        .integrator_record(&json!({"type": "task.landed", "id": accepted,
                                    "from": commit('0'), "to": commit('a')}))
        .expect("landed");
    assert_eq!(judging(), json!(1), "a landed task is no longer counted");
}
