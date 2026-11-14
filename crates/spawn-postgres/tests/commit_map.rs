//! The commit map of a history rewrite (objective 173): the owner loads git
//! filter-repo's commit-map once (`spawn_commit_map_load`), it is never
//! changed, and the integrator's readers and the folds read a recorded
//! commit through it (`spawn_commit_now`): a request, a baseline, a base, a
//! move's from and to, recorded before the rewrite, are the commits they
//! are now.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};

mod common;
use common::{admin, as_login, schema_url, store};
use spawn_postgres::{PostgresStore, PushRequest, role_url};

const SOURCE: &str = "/tmp/commit-map.git";

/// The history before and after the rewrite: m0 kept, m1 rewritten to m1n,
/// E dropped, X rewritten to xn (on m1n).
struct History {
    m0: String,
    m1: String,
    m1n: String,
    e: String,
    x: String,
    xn: String,
}

fn history() -> History {
    History {
        m0: "a0".repeat(20),
        m1: "b1".repeat(20),
        m1n: "c1".repeat(20),
        e: "d2".repeat(20),
        x: "e3".repeat(20),
        xn: "f3".repeat(20),
    }
}

impl History {
    /// filter-repo's map of it, as `spawn commit-map load` reads it.
    fn pairs(&self) -> Vec<(String, Option<String>)> {
        vec![
            (self.m0.clone(), Some(self.m0.clone())),
            (self.m1.clone(), Some(self.m1n.clone())),
            (self.e.clone(), None),
            (self.x.clone(), Some(self.xn.clone())),
        ]
    }
}

/// A gated mission on [`SOURCE`] whose first verdict left `baseline`, and
/// its task (reported again after that verdict).
fn mission_at(store: &PostgresStore, baseline: &str) -> (i64, i64) {
    let mission = store
        .mission_add("On the source", &json!([SOURCE]), "human")
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
        .integrator_record(
            &json!({"type": "task.gated", "revision": 0, "id": task, "verdict": "aborted",
                    "base": baseline, "reason": "a gate that died"}),
        )
        .expect("a first verdict sets the baseline");
    (mission, task)
}

/// The mission's baseline and halt, as folded.
fn folded(client: &mut Client, mission: i64) -> (Option<String>, Option<String>) {
    let row = client
        .query_one(
            "select baseline, halted from mission_missions where id = $1",
            &[&mission],
        )
        .expect("the mission");
    (row.get(0), row.get(1))
}

fn request(store: &PostgresStore, commit: &str) -> i64 {
    store
        .source_push_request(
            SOURCE,
            commit,
            &format!("refs/spawn-owner/{commit}"),
            &PushRequest::default(),
        )
        .expect("the owner's request")
}

/// The owner's request recorded before the rewrite names the old X: the
/// integrator's queue and deploy-on-landing's read it as X's new commit
/// once the map is loaded, and a request for the dropped E keeps its id
/// (the integrator refuses it). `spawn_commit_now` reads any case, keeps an
/// id the map does not name, and null; the olds the guard reads are those
/// the rewrite changed or dropped, never the kept m0.
#[test]
fn an_owner_request_of_a_pre_rewrite_commit_is_read_as_its_new_commit() {
    let (store, schema) = store("commit_map_request");
    let h = history();
    mission_at(&store, &h.m1);
    let x = request(&store, &h.x);
    let pending = store.source_pushes_pending().expect("pending");
    assert_eq!(
        pending[0]["commit"],
        h.x.as_str(),
        "no map: the recorded id"
    );

    assert_eq!(store.commit_map_size().expect("size"), 0);
    assert_eq!(
        store.commit_map_load(&h.pairs()).expect("loaded"),
        (4, true)
    );
    let pending = store.source_pushes_pending().expect("pending");
    assert_eq!(pending[0]["request"], json!(x));
    assert_eq!(pending[0]["commit"], h.xn.as_str(), "{pending}");
    let landing = store
        .main_landing(SOURCE, &h.m1n, None)
        .expect("deploy-on-landing's read");
    assert_eq!(
        landing["owner_requests"][0]["commit"],
        h.xn.as_str(),
        "{landing}"
    );

    let e = request(&store, &h.e);
    let pending = store.source_pushes_pending().expect("pending");
    let dropped = pending
        .as_array()
        .expect("requests")
        .iter()
        .find(|row| row["request"] == json!(e))
        .expect("the request for E");
    assert_eq!(
        dropped["commit"],
        h.e.as_str(),
        "a dropped commit keeps its id"
    );

    assert_eq!(
        store.commit_now(&h.x.to_uppercase()).expect("now"),
        Some(h.xn.clone())
    );
    assert_eq!(store.commit_now(&h.e).expect("now"), None);
    assert_eq!(store.commit_now(&h.m0).expect("now"), Some(h.m0.clone()));
    let other = "9".repeat(40);
    assert_eq!(store.commit_now(&other).expect("now"), Some(other.clone()));
    let mut client = admin(&schema);
    let null: Option<String> = client
        .query_one("select spawn_commit_now(null)", &[])
        .expect("null")
        .get(0);
    assert_eq!(null, None);

    assert_eq!(
        store.commit_map_entry(&h.m1.to_uppercase()).expect("entry"),
        Some(Some(h.m1n.clone()))
    );
    assert_eq!(store.commit_map_entry(&h.e).expect("entry"), Some(None));
    assert_eq!(store.commit_map_entry(&other).expect("entry"), None);
    let olds = store.commit_map_olds().expect("olds");
    let mut olds: Vec<&str> = olds.iter().map(String::as_str).collect();
    olds.sort_unstable();
    assert_eq!(olds, vec![h.m1.as_str(), h.e.as_str(), h.x.as_str()]);
}

/// The map is loaded once, by the owner: the same pairs again change
/// nothing, a different map, a malformed pair, an old twice and an empty
/// list are refused, a restricted login loads nothing, and no statement
/// updates, deletes or truncates it.
#[test]
fn the_map_is_loaded_once_by_the_owner_and_never_changed() {
    let (store, schema) = store("commit_map_once");
    let h = history();
    let login = role_url(&schema_url(&schema), "spawn_supervisor").expect("the login's URL");
    let supervisor = PostgresStore::connect(&login).expect("log in");
    let refused = supervisor
        .commit_map_load(&h.pairs())
        .expect_err("a restricted login loads no map")
        .to_string();
    assert!(
        refused.contains("permission denied") || refused.contains("only the owner"),
        "{refused}"
    );
    assert_eq!(store.commit_map_size().expect("size"), 0);

    let malformed = |pairs: Vec<(String, Option<String>)>| {
        store
            .commit_map_load(&pairs)
            .expect_err("refused")
            .to_string()
    };
    assert!(malformed(vec![]).contains("non-empty"));
    assert!(
        malformed(vec![("xyz".to_string(), None)]).contains("40 hex"),
        "a malformed old"
    );
    assert!(
        malformed(vec![(h.m1.clone(), Some("12".to_string()))]).contains("40 hex"),
        "a malformed new"
    );
    assert!(
        malformed(vec![
            (h.m1.clone(), Some(h.m1n.clone())),
            (h.m1.to_uppercase(), None)
        ])
        .contains("once"),
        "an old twice"
    );
    assert_eq!(store.commit_map_size().expect("size"), 0, "nothing loaded");

    assert_eq!(
        store.commit_map_load(&h.pairs()).expect("loaded"),
        (4, true)
    );
    let mut reversed = h.pairs();
    reversed.reverse();
    assert_eq!(
        store.commit_map_load(&reversed).expect("the same pairs"),
        (4, false)
    );
    let mut different = h.pairs();
    different[3].1 = Some(h.m1n.clone());
    let refused = store
        .commit_map_load(&different)
        .expect_err("a different map")
        .to_string();
    assert!(
        refused.contains(
            "a different commit map is loaded (4 pairs): a map is loaded once and never changed"
        ),
        "{refused}"
    );
    let fewer = h.pairs()[..3].to_vec();
    assert!(store.commit_map_load(&fewer).is_err(), "fewer pairs");

    let mut client = admin(&schema);
    for statement in [
        "update spawn_commit_map set new = null",
        "delete from spawn_commit_map",
        "truncate spawn_commit_map",
    ] {
        let refused = client
            .batch_execute(statement)
            .expect_err("the map is never changed");
        let error = refused
            .as_db_error()
            .map_or_else(|| refused.to_string(), |error| error.message().to_string());
        assert!(
            error.contains("the commit map is never changed (objective 173)"),
            "{statement}: {error}"
        );
    }
    assert_eq!(store.commit_map_size().expect("size"), 4);
    let public: bool = client
        .query_one(
            "select has_table_privilege('public', 'spawn_commit_map', 'select')",
            &[],
        )
        .expect("privilege")
        .get(0);
    assert!(!public, "the table is revoked from public");
}

/// Objective 190: the translation is fast. `spawn_commit_now` runs with
/// the caller's rights and no SET clause (SECURITY DEFINER with one cost
/// about 5.5 us a call), and `spawn_commit_now_rows`, joined LATERAL, is
/// inlined: no function is called per row, the owner's plan or a
/// restricted login's. The access rule is kept: the supervisor and the
/// reader read recorded commits through both and read the map, and neither
/// writes it.
#[test]
fn the_translation_is_fast_and_the_restricted_logins_read_through_it() {
    let (store, schema) = store("commit_map_fast");
    let h = history();
    store.commit_map_load(&h.pairs()).expect("loaded");
    let plan = |client: &mut Client| -> String {
        client
            .query(
                "explain (verbose, costs off) select b.now, c.now from mission_tasks k \
                 cross join lateral spawn_commit_now_rows(k.base) b \
                 cross join lateral spawn_commit_now_rows(k.candidate) c",
                &[],
            )
            .expect("explain")
            .iter()
            .map(|row| row.get::<_, String>(0))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut owner = admin(&schema);
    let owners = plan(&mut owner);
    assert!(!owners.contains("spawn_commit_now"), "{owners}");
    assert!(owners.contains("spawn_commit_map"), "{owners}");
    let row = owner
        .query_one(
            "select prosecdef, proconfig is null from pg_proc \
             where oid = 'spawn_commit_now(text)'::regprocedure",
            &[],
        )
        .expect("the function");
    assert_eq!(
        (row.get::<_, bool>(0), row.get::<_, bool>(1)),
        (false, true),
        "run with the caller's rights, no SET clause"
    );
    for login in ["spawn_supervisor", "spawn_reader"] {
        let mut client = as_login(&schema, login);
        let theirs = plan(&mut client);
        assert!(!theirs.contains("spawn_commit_now"), "{login}: {theirs}");
        let mut read = |commit: &str| -> Option<String> {
            let row = client
                .query_one(
                    "select spawn_commit_now($1::text), \
                     (select now from spawn_commit_now_rows($1::text))",
                    &[&commit],
                )
                .expect("read through the map");
            let (now, rows): (Option<String>, Option<String>) = (row.get(0), row.get(1));
            assert_eq!(now, rows, "both forms read alike");
            now
        };
        assert_eq!(read(&h.m1.to_uppercase()), Some(h.m1n.clone()), "{login}");
        assert_eq!(read(&h.e), None, "{login}: a dropped commit");
        assert_eq!(
            read(&h.xn),
            Some(h.xn.clone()),
            "{login}: an id the map does not name"
        );
        let size: i64 = client
            .query_one("select count(*) from spawn_commit_map", &[])
            .expect("the map is readable")
            .get(0);
        assert_eq!(size, 4, "{login}");
        let refused = client
            .batch_execute(&format!(
                "insert into spawn_commit_map (old, new) values ('{}', null)",
                "ab".repeat(20)
            ))
            .expect_err("a restricted login writes no map");
        let error = refused
            .as_db_error()
            .map_or_else(|| refused.to_string(), |error| error.message().to_string());
        assert!(error.contains("permission denied"), "{login}: {error}");
    }
    let null: Option<String> = owner
        .query_one("select spawn_commit_now(null)", &[])
        .expect("null")
        .get(0);
    assert_eq!(null, None, "null stays null");
    assert_eq!(store.commit_map_size().expect("size"), 4);
}

/// Objective 190: the map compares its ids in byte order (collation "C"),
/// not through the database's locale, which was most of a lookup's cost:
/// a new map is made so, and a map made before (the default collation) is
/// altered by the next migration, its pairs unchanged.
#[test]
fn the_map_compares_in_byte_order_and_a_map_made_before_is_altered() {
    let (store, schema) = store("commit_map_collation");
    let h = history();
    store.commit_map_load(&h.pairs()).expect("loaded");
    let mut client = admin(&schema);
    let collations = |client: &mut Client| -> Vec<String> {
        client
            .query(
                "select a.attname || '=' || c.collname from pg_attribute a \
                 join pg_collation c on c.oid = a.attcollation \
                 where a.attrelid = 'spawn_commit_map'::regclass \
                   and a.attname in ('old', 'new') order by a.attname",
                &[],
            )
            .expect("collations")
            .iter()
            .map(|row| row.get(0))
            .collect()
    };
    assert_eq!(collations(&mut client), ["new=C", "old=C"], "a new map");
    client
        .batch_execute(
            "alter table spawn_commit_map alter column old type text collate \"default\", \
             alter column new type text collate \"default\"",
        )
        .expect("the map as it was made before");
    assert_eq!(
        collations(&mut client),
        ["new=default", "old=default"],
        "made before"
    );
    // The rows, byte for byte: their count and a checksum of old || new in
    // byte order, the same before the migration and after it.
    let rows = |client: &mut Client| -> (i64, String) {
        let row = client
            .query_one(
                "select count(*), md5(coalesce(string_agg(old || ':' || coalesce(new, '-'), ',' \
                 order by old collate \"C\"), '')) from spawn_commit_map",
                &[],
            )
            .expect("the rows");
        (row.get(0), row.get(1))
    };
    let before = rows(&mut client);
    store.migrate().expect("migrate");
    assert_eq!(collations(&mut client), ["new=C", "old=C"], "altered");
    assert_eq!(rows(&mut client), before, "the rows unchanged");
    assert_eq!(before.0, 4);
    // Still loaded once: the same pairs change nothing, another map is
    // refused.
    assert_eq!(
        store.commit_map_load(&h.pairs()).expect("the same pairs"),
        (4, false)
    );
    let mut different = h.pairs();
    different[1].1 = None;
    assert!(
        store
            .commit_map_load(&different)
            .expect_err("a different map")
            .to_string()
            .contains("a different commit map is loaded (4 pairs)"),
        "refused after the migration"
    );
    assert_eq!(rows(&mut client), before, "nothing loaded");
    for (old, new) in h.pairs() {
        assert_eq!(
            store.commit_map_entry(&old).expect("entry"),
            Some(new),
            "{old}"
        );
    }
    assert_eq!(store.commit_map_size().expect("size"), 4);
}

/// Recorded before the rewrite, read after it: the mission's baseline (m1)
/// and its task's base read as m1n in the integrator's selection; the
/// integrator's `source.moved` from m1n moves the mission at m1, and names
/// it; the owner's latest landing, recorded with the old X, reads as xn (the
/// deploy's and the alert's); and a gating on xn raises the floor of the
/// mission whose baseline names the old X. A refold folds the same.
#[test]
fn the_integrators_readers_and_folds_read_recorded_commits_as_their_new_ones() {
    let (store, schema) = store("commit_map_readers");
    let mut client = admin(&schema);
    let h = history();
    let (mission, task) = mission_at(&store, &h.m1);
    let x = request(&store, &h.x);
    store.commit_map_load(&h.pairs()).expect("loaded");

    let gated = store.gated_missions().expect("gated");
    let row = gated
        .as_array()
        .expect("missions")
        .iter()
        .find(|row| row["id"] == json!(mission))
        .expect("the mission");
    assert_eq!(row["baseline"], h.m1n.as_str(), "{row}");
    let base = &row["tasks"][0]["base"];
    assert!(
        base.is_null() || *base == json!(h.m1n),
        "the task's base: {row}"
    );

    // The integrator applies the request from the rewritten main; its record
    // names the old X as where main went (a record of the old ids).
    let moved = store
        .integrator_record(&json!({"type": "source.moved", "source": SOURCE,
                                   "from": h.m1n, "to": h.x, "request": x, "by": "human"}))
        .expect("moved");
    let record: Value = client
        .query_one(
            "select kind -> 'detail' -> 'record' from spawn_events where id = $1",
            &[&moved],
        )
        .expect("the record")
        .get(0);
    assert_eq!(record["missions"], json!([mission]), "{record}");
    assert_eq!(folded(&mut client, mission), (Some(h.x.clone()), None));
    assert_eq!(
        store.source_owner_landed(SOURCE).expect("landed"),
        Some(h.xn.clone())
    );
    let controllers = store.alert_controllers().expect("controllers");
    let row = controllers["missions"]
        .as_array()
        .expect("missions")
        .iter()
        .find(|row| row["mission"] == json!(mission))
        .expect("the mission waits on a reported task");
    assert_eq!(row["landed"], h.xn.as_str(), "{controllers}");
    let gated = store.gated_missions().expect("gated");
    assert_eq!(gated[0]["baseline"], h.xn.as_str());

    // A gating on xn raises the floor of the mission at the old X.
    store
        .integrator_record(
            &json!({"type": "task.gated", "revision": 0, "id": task, "verdict": "aborted",
                    "base": h.xn, "scenarios": 5, "reason": "a gate that died"}),
        )
        .expect("a gating on xn");
    let floor: i32 = client
        .query_one(
            "select scenario_floor from mission_missions where id = $1",
            &[&mission],
        )
        .expect("the floor")
        .get(0);
    assert_eq!(floor, 5, "the gating was on the mission's baseline");

    let live: Value = client
        .query_one(
            "select coalesce(jsonb_agg(to_jsonb(m) - 'updated_at_ms' order by m.id), '[]')
               from mission_missions m",
            &[],
        )
        .expect("missions")
        .get(0);
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
    let again: Value = client
        .query_one(
            "select coalesce(jsonb_agg(to_jsonb(m) - 'updated_at_ms' order by m.id), '[]')
               from mission_missions m",
            &[],
        )
        .expect("missions")
        .get(0);
    assert_eq!(again, live, "a refold folds the same");
}

/// Before the rewrite the integrator halted the mission: main moved from m1
/// to X with no record. After it, the owner records the move as the commits
/// are now (m1n to xn): the halt was waiting for that record, so the
/// mission moves and is not halted any more.
#[test]
fn an_owners_move_clears_a_halt_recorded_before_the_rewrite() {
    let (store, schema) = store("commit_map_halt");
    let mut client = admin(&schema);
    let h = history();
    let (mission, _) = mission_at(&store, &h.m1);
    store
        .integrator_record(&json!({"type": "mission.halted", "id": mission,
            "reason": format!("the source's main is {}, but the last landing left {}", h.x, h.m1),
            "cause": "main_moved", "source": SOURCE, "from": h.m1, "to": h.x}))
        .expect("halted");
    assert!(folded(&mut client, mission).1.is_some(), "halted");
    store.commit_map_load(&h.pairs()).expect("loaded");

    assert_eq!(
        store.source_moved(SOURCE, &h.m1n, &h.xn).expect("moved"),
        1,
        "the halted mission is the one it moves"
    );
    assert_eq!(folded(&mut client, mission), (Some(h.xn.clone()), None));
}
