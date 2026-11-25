//! Universal file provenance (PV1): `prov_changes`, `prov_entries` and
//! `prov_edges` are written only through `spawn_prov_record_change` (owner
//! only, atomic, idempotent on an identical `(change_id, rev)`), never
//! updated or deleted, read back by `(change_id, rev)`, id, path, agent and
//! execution, and walked by `prov_blast_radius` both ways, cycles included.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::json;

mod common;
use common::{admin, as_login, migrate_warnings, schema_url, store};
use spawn_postgres::{
    PostgresStore, ProvAttributionSource, ProvBoundary, ProvChangeInput, ProvChangeKind,
    ProvDirection, ProvEdgeInput, ProvEdgeKind, ProvEntryClass, ProvEntryInput, ProvLink, role_url,
};

const TABLES: [&str; 3] = ["prov_changes", "prov_entries", "prov_edges"];

fn change(change_id: &str, rev: i32, agent: &str, at_ms: i64) -> ProvChangeInput {
    ProvChangeInput {
        change_id: change_id.to_string(),
        rev,
        kind: ProvChangeKind::Edit,
        domain: "code".to_string(),
        agent_id: agent.to_string(),
        execution_id: Some(format!("exec-{agent}")),
        task_id: Some(577),
        preset_id: Some("default".to_string()),
        model_label: Some("model-a".to_string()),
        transcript_ref: Some(format!("transcripts/{change_id}.jsonl")),
        attributed: ProvAttributionSource::Host,
        base: format!("tree-{change_id}-{rev}-base"),
        result: format!("tree-{change_id}-{rev}-result"),
        boundary: ProvBoundary::Checkpoint,
        at_ms,
    }
}

fn entry(path: &str, after: &str) -> ProvEntryInput {
    ProvEntryInput {
        path: path.to_string(),
        before: None,
        after: Some(after.to_string()),
        size: Some(42),
        class: ProvEntryClass::Content,
        origin: None,
    }
}

fn edge(kind: ProvEdgeKind, dst: &str) -> ProvEdgeInput {
    ProvEdgeInput {
        kind,
        dst: dst.to_string(),
        detail: json!({}),
        at_ms: None,
    }
}

/// The server's message of a refused statement (a driver error's own
/// text is only "db error").
fn message(error: &postgres::Error) -> String {
    error
        .as_db_error()
        .map_or_else(|| error.to_string(), |db| db.message().to_string())
}

fn count(client: &mut Client, table: &str) -> i64 {
    client
        .query_one(&format!("select count(*) from {table}"), &[])
        .expect("count")
        .get(0)
}

/// The schema applies over itself (every start runs it), with no new
/// warning; the reader selects the three tables and writes none of them;
/// public and the restricted logins hold nothing on them but the reader's
/// select, and only the owner records; `spawn_control_refold` leaves the
/// provenance as it was.
#[test]
fn the_schema_applies_twice_and_only_the_owner_records() {
    let (store, schema) = store("prov_schema");
    let id = store
        .record_change(
            &change("c-schema", 1, "agent_a", 1_000),
            &[entry("src/a.rs", "blob-a")],
            &[edge(ProvEdgeKind::Used, "c-other")],
        )
        .expect("record");

    // Applied again, twice: idempotent, and the rows stay.
    assert!(migrate_warnings(&schema).is_empty());
    store.migrate().expect("migrate again");
    store.migrate().expect("and again");
    let mut client = admin(&schema);
    for table in TABLES {
        assert_eq!(count(&mut client, table), 1, "{table} keeps its row");
    }
    let triggers: i64 = client
        .query_one(
            "select count(*) from pg_trigger t
              where t.tgrelid in ('prov_changes'::regclass, 'prov_entries'::regclass,
                                  'prov_edges'::regclass) and not t.tgisinternal",
            &[],
        )
        .expect("triggers")
        .get(0);
    assert_eq!(triggers, 6, "one row and one truncate trigger per table");

    // The refold rebuilds the folds from the log; provenance is not one.
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
    for table in TABLES {
        assert_eq!(count(&mut client, table), 1, "{table} survives the refold");
    }

    // Public holds nothing on them; the reader selects, the others do not.
    let reader = common::login(&schema, "spawn_reader");
    let supervisor = common::login(&schema, "spawn_supervisor");
    let human = common::login(&schema, "spawn_human");
    for table in TABLES {
        for (role, select) in [
            ("public", false),
            (reader.as_str(), true),
            (supervisor.as_str(), false),
            (human.as_str(), false),
        ] {
            for privilege in ["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE"] {
                let held: bool = client
                    .query_one(
                        "select has_table_privilege($1::text, $2::text, $3::text)",
                        &[&role, &table, &privilege],
                    )
                    .expect("the privilege")
                    .get(0);
                let want = select && privilege == "SELECT";
                assert_eq!(held, want, "{role} {privilege} on {table}");
            }
        }
    }
    for function in [
        "spawn_prov_record_change(jsonb, jsonb, jsonb)",
        "spawn_prov_canonical(bigint)",
        "spawn_prov_unchanged()",
    ] {
        for role in [
            "public",
            reader.as_str(),
            supervisor.as_str(),
            human.as_str(),
        ] {
            let held: bool = client
                .query_one(
                    "select has_function_privilege($1::text, $2::text, 'execute')",
                    &[&role, &function],
                )
                .expect("the privilege")
                .get(0);
            assert!(!held, "{role} may execute {function}");
        }
    }

    // As the logins themselves: the reader reads, and neither records.
    let mut as_reader = as_login(&schema, "spawn_reader");
    let read: i64 = as_reader
        .query_one("select count(*) from prov_changes where id = $1", &[&id])
        .expect("the reader selects")
        .get(0);
    assert_eq!(read, 1);
    let refused = as_reader
        .execute(
            "insert into prov_edges (kind, src, dst, at_ms) values ('used', $1, 'x', 1)",
            &[&id],
        )
        .expect_err("the reader writes nothing");
    let refused = message(&refused);
    assert!(refused.contains("permission denied"), "{refused}");
    for login in ["spawn_reader", "spawn_supervisor"] {
        let url = role_url(&schema_url(&schema), login).expect("the login's URL");
        let restricted = PostgresStore::connect(&url).expect("log in");
        let refused = restricted
            .record_change(&change("c-login", 1, "agent_a", 2_000), &[], &[])
            .expect_err("a restricted login records nothing")
            .to_string();
        assert!(refused.contains("permission denied"), "{login}: {refused}");
    }
    assert_eq!(count(&mut client, "prov_changes"), 1);
}

/// A change, its entries and edges come back as recorded, by
/// `(change_id, rev)`, by id, by path, by agent and by execution; the same
/// change again returns its id and writes nothing, a different one under
/// the same key is refused and writes nothing.
#[test]
#[allow(clippy::too_many_lines)] // one change's life: record, read every way, record again
fn a_recorded_change_reads_back_every_way() {
    let (store, schema) = store("prov_roundtrip");
    let mut first = change("c-1", 1, "agent_a", 1_000);
    first.attributed = ProvAttributionSource::Inferred;
    first.boundary = ProvBoundary::Submit;
    let entries = [
        entry("src/b.rs", "blob-b1"),
        ProvEntryInput {
            path: "src/a.rs".to_string(),
            before: Some("blob-a0".to_string()),
            after: None,
            size: None,
            class: ProvEntryClass::Mechanical,
            origin: None,
        },
    ];
    let edges = [
        ProvEdgeInput {
            kind: ProvEdgeKind::Used,
            dst: "blob-spec".to_string(),
            detail: json!({"why": "read the spec"}),
            at_ms: Some(900),
        },
        edge(ProvEdgeKind::Includes, "c-0"),
    ];
    let id = store
        .record_change(&first, &entries, &edges)
        .expect("record");

    let got = store.prov_change("c-1", 1).expect("by key").expect("found");
    assert_eq!(got.id, id);
    assert_eq!(got.change_id, "c-1");
    assert_eq!(got.rev, 1);
    assert_eq!(got.kind, ProvChangeKind::Edit);
    assert_eq!(got.domain, "code");
    assert_eq!(got.agent_id, "agent_a");
    assert_eq!(got.execution_id.as_deref(), Some("exec-agent_a"));
    assert_eq!(got.task_id, Some(577));
    assert_eq!(got.preset_id.as_deref(), Some("default"));
    assert_eq!(got.model_label.as_deref(), Some("model-a"));
    assert_eq!(got.transcript_ref.as_deref(), Some("transcripts/c-1.jsonl"));
    assert_eq!(got.attributed, ProvAttributionSource::Inferred);
    assert_eq!(got.base, first.base);
    assert_eq!(got.result, first.result);
    assert_eq!(got.boundary, ProvBoundary::Submit);
    assert_eq!(got.at_ms, 1_000);
    assert_eq!(
        store.prov_change_by_id(id).expect("by id"),
        Some(got.clone())
    );
    assert_eq!(store.prov_change("c-1", 2).expect("by key"), None);
    assert_eq!(store.prov_change_by_id(id + 1_000).expect("by id"), None);

    let stored = store.prov_entries(id).expect("entries");
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].path, "src/a.rs", "by path");
    assert_eq!(stored[0].before.as_deref(), Some("blob-a0"));
    assert_eq!(stored[0].after, None);
    assert_eq!(stored[0].size, None);
    assert_eq!(stored[0].class, ProvEntryClass::Mechanical);
    assert_eq!(stored[1].path, "src/b.rs");
    assert_eq!(stored[1].after.as_deref(), Some("blob-b1"));
    assert_eq!(stored[1].size, Some(42));
    assert!(stored.iter().all(|entry| entry.change == id));

    let stored = store.prov_edges(id).expect("edges");
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].kind, ProvEdgeKind::Used);
    assert_eq!(stored[0].src, id);
    assert_eq!(stored[0].dst, "blob-spec");
    assert_eq!(stored[0].detail, json!({"why": "read the spec"}));
    assert_eq!(stored[0].at_ms, 900);
    assert_eq!(stored[1].kind, ProvEdgeKind::Includes);
    assert_eq!(stored[1].detail, json!({}));
    assert_eq!(
        stored[1].at_ms, 1_000,
        "an edge's time defaults to its change's"
    );

    // A second revision, another agent's change and one with no execution.
    let second_rev = store
        .record_change(
            &change("c-1", 2, "agent_a", 2_000),
            &[entry("src/b.rs", "blob-b2")],
            &[],
        )
        .expect("rev 2");
    let mut other = change("c-2", 1, "agent_b", 1_500);
    other.execution_id = None;
    let other = store
        .record_change(&other, &[entry("src/c.rs", "blob-c")], &[])
        .expect("another");

    let ids = |records: Vec<spawn_postgres::ProvChangeRecord>| -> Vec<i64> {
        records.iter().map(|record| record.id).collect()
    };
    assert_eq!(
        ids(store.prov_changes_for_path("src/b.rs").expect("path")),
        [id, second_rev]
    );
    assert_eq!(
        ids(store.prov_changes_for_path("src/c.rs").expect("path")),
        [other]
    );
    assert!(
        store
            .prov_changes_for_path("src/none.rs")
            .expect("path")
            .is_empty()
    );
    assert_eq!(
        ids(store.prov_changes_for_agent("agent_a").expect("agent")),
        [id, second_rev]
    );
    assert_eq!(
        ids(store.prov_changes_for_agent("agent_b").expect("agent")),
        [other]
    );
    assert_eq!(
        ids(store
            .prov_changes_for_execution("exec-agent_a")
            .expect("execution")),
        [id, second_rev]
    );
    assert!(
        store
            .prov_changes_for_execution("exec-agent_b")
            .expect("execution")
            .is_empty()
    );

    // The same change again (entries in another order): its id, no write.
    let mut client = admin(&schema);
    let reordered = [entries[1].clone(), entries[0].clone()];
    let edges_reordered = [edges[1].clone(), edges[0].clone()];
    assert_eq!(
        store
            .record_change(&first, &reordered, &edges_reordered)
            .expect("the same again"),
        id
    );
    // A different one under the same key: refused, nothing written.
    let before: Vec<i64> = TABLES
        .iter()
        .map(|table| count(&mut client, table))
        .collect();
    let mut changed = first.clone();
    changed.agent_id = "agent_z".to_string();
    for (input, entries, edges) in [
        (&changed, &entries[..], &edges[..]),
        (&first, &entries[..1], &edges[..]),
        (&first, &entries[..], &edges[..1]),
    ] {
        let refused = store
            .record_change(input, entries, edges)
            .expect_err("a different change under a recorded key")
            .to_string();
        assert!(
            refused.contains("recorded already with different content"),
            "{refused}"
        );
    }
    let after: Vec<i64> = TABLES
        .iter()
        .map(|table| count(&mut client, table))
        .collect();
    assert_eq!(before, after);

    // An entry's origin is a recorded change; a path twice is refused whole.
    let mut reverted = entry("src/b.rs", "blob-b1");
    reverted.origin = Some(id);
    let revert = store
        .record_change(&change("c-3", 1, "agent_a", 3_000), &[reverted], &[])
        .expect("an origin");
    assert_eq!(
        store.prov_entries(revert).expect("entries")[0].origin,
        Some(id)
    );
    let mut dangling = entry("src/d.rs", "blob-d");
    dangling.origin = Some(id + 1_000);
    store
        .record_change(&change("c-4", 1, "agent_a", 4_000), &[dangling], &[])
        .expect_err("an origin that names no change");
    store
        .record_change(
            &change("c-5", 1, "agent_a", 5_000),
            &[entry("src/e.rs", "x"), entry("src/e.rs", "y")],
            &[],
        )
        .expect_err("a path twice");
    assert_eq!(store.prov_change("c-4", 1).expect("key"), None);
    assert_eq!(store.prov_change("c-5", 1).expect("key"), None, "atomic");
}

/// No statement updates, deletes or truncates any of the three tables, the
/// owner's included.
#[test]
fn provenance_is_never_changed() {
    let (store, schema) = store("prov_immutable");
    let id = store
        .record_change(
            &change("c-im", 1, "agent_a", 1_000),
            &[entry("src/a.rs", "blob-a")],
            &[edge(ProvEdgeKind::Used, "x")],
        )
        .expect("record");
    let mut client = admin(&schema);
    for statement in [
        "update prov_changes set agent_id = 'other'",
        "delete from prov_changes",
        "truncate prov_changes cascade",
        "update prov_entries set after = 'other'",
        "delete from prov_entries",
        "truncate prov_entries",
        "update prov_edges set dst = 'other'",
        "delete from prov_edges",
        "truncate prov_edges",
    ] {
        let refused = message(&client.batch_execute(statement).expect_err(statement));
        assert!(
            refused.contains("append-only") || refused.contains("provenance"),
            "{statement}: {refused}"
        );
    }
    for table in TABLES {
        assert_eq!(count(&mut client, table), 1, "{table} kept its row");
    }
    assert_eq!(
        store
            .prov_change_by_id(id)
            .expect("read")
            .map(|c| c.agent_id),
        Some("agent_a".to_string())
    );
}

/// Every closed list is checked: a change's kind, attribution and boundary,
/// an entry's class, an edge's kind; and malformed arguments are refused.
/// Each refusal writes nothing.
#[test]
fn the_checks_refuse_values_outside_their_lists() {
    let (store, schema) = store("prov_checks");
    let mut client = admin(&schema);
    let good = json!({
        "change_id": "c-chk", "rev": 1, "kind": "edit", "domain": "code",
        "agent_id": "agent_a", "base": "t0", "result": "t1", "boundary": "job", "at_ms": 1
    });
    let record = |client: &mut Client,
                  change: &serde_json::Value,
                  entries: &serde_json::Value,
                  edges: &serde_json::Value| {
        client
            .query_one(
                "select spawn_prov_record_change($1, $2, $3)",
                &[change, entries, edges],
            )
            .map(|row| row.get::<_, i64>(0))
            .map_err(|error| message(&error))
    };
    let with = |key: &str, value: &str| {
        let mut change = good.clone();
        change[key] = json!(value);
        change
    };
    for (key, value) in [
        ("kind", "rewrite"),
        ("attributed", "guessed"),
        ("boundary", "commit"),
    ] {
        let error = record(&mut client, &with(key, value), &json!([]), &json!([])).expect_err(key);
        assert!(error.contains("check constraint"), "{key}: {error}");
    }
    let error = record(
        &mut client,
        &good,
        &json!([{"path": "a", "class": "binary"}]),
        &json!([]),
    )
    .expect_err("class");
    assert!(error.contains("check constraint"), "class: {error}");
    let error = record(
        &mut client,
        &good,
        &json!([]),
        &json!([{"kind": "copied", "dst": "x"}]),
    )
    .expect_err("edge kind");
    assert!(error.contains("check constraint"), "edge kind: {error}");

    // Malformed arguments.
    let mut missing = good.clone();
    missing.as_object_mut().expect("object").remove("agent_id");
    record(&mut client, &missing, &json!([]), &json!([])).expect_err("a missing column");
    let error = record(&mut client, &with("id", "7"), &json!([]), &json!([]))
        .expect_err("an id is not given");
    assert!(error.contains("unknown provenance keys: id"), "{error}");
    record(&mut client, &json!([]), &json!([]), &json!([])).expect_err("not an object");
    record(&mut client, &good, &json!({}), &json!([])).expect_err("entries not an array");
    record(&mut client, &good, &json!([]), &json!(1)).expect_err("edges not an array");
    record(&mut client, &with("rev", "one"), &json!([]), &json!([])).expect_err("a wrong type");
    for table in TABLES {
        assert_eq!(count(&mut client, table), 0, "{table}: nothing written");
    }

    // The same columns written directly are checked too.
    for statement in [
        "insert into prov_changes (change_id, rev, kind, domain, agent_id, base, result, boundary, at_ms)
         values ('d', 1, 'edit', 'code', 'a', 't0', 't1', 'nowhere', 1)",
        "insert into prov_changes (change_id, rev, kind, domain, agent_id, attributed, base, result,
                                   boundary, at_ms)
         values ('d', 1, 'edit', 'code', 'a', 'nobody', 't0', 't1', 'job', 1)",
    ] {
        let error = message(&client.batch_execute(statement).expect_err(statement));
        assert!(error.contains("check constraint"), "{error}");
    }

    // The good one passes, with null edges and the defaults.
    let id = record(
        &mut client,
        &good,
        &json!([{"path": "a"}]),
        &serde_json::Value::Null,
    )
    .expect("the good one");
    let got = store.prov_change_by_id(id).expect("read").expect("found");
    assert_eq!(got.attributed, ProvAttributionSource::Host);
    assert_eq!(got.boundary, ProvBoundary::Job);
    assert_eq!(
        store.prov_entries(id).expect("entries")[0].class,
        ProvEntryClass::Content
    );
    assert!(store.prov_edges(id).expect("edges").is_empty());
}

/// The blast radius walks forward to what depends on a change and backward
/// to what it depends on, through change ids, tree and content digests and
/// origins; a cycle (A uses B, B uses A, and C uses itself) ends, each
/// change once by its shortest path; the depth bound holds.
#[test]
fn the_blast_radius_walks_both_ways_and_ends_on_cycles() {
    let (store, _schema) = store("prov_blast");
    // A uses B (not recorded yet: an edge names a change id, not a row).
    let ch_a = store
        .record_change(
            &change("A", 1, "agent_a", 1_000),
            &[entry("src/a.rs", "blob-a")],
            &[edge(ProvEdgeKind::Used, "B")],
        )
        .expect("A");
    // B uses A: the cycle A -> B -> A.
    let ch_b = store
        .record_change(
            &change("B", 1, "agent_b", 2_000),
            &[entry("src/b.rs", "blob-b")],
            &[edge(ProvEdgeKind::Used, "A")],
        )
        .expect("B");
    // C includes B's content digest, and itself (a self-loop).
    let ch_c = store
        .record_change(
            &change("C", 1, "agent_c", 3_000),
            &[entry("src/c.rs", "blob-c")],
            &[
                edge(ProvEdgeKind::Includes, "blob-b"),
                edge(ProvEdgeKind::Supersedes, "C"),
            ],
        )
        .expect("C");
    // D reverts to C's content (origin), and E uses D's tree.
    let mut back = entry("src/c.rs", "blob-c");
    back.origin = Some(ch_c);
    let d_change = change("D", 1, "agent_d", 4_000);
    let d_tree = d_change.result.clone();
    let ch_d = store.record_change(&d_change, &[back], &[]).expect("D");
    let ch_e = store
        .record_change(
            &change("E", 1, "agent_e", 5_000),
            &[],
            &[edge(ProvEdgeKind::Redacts, &d_tree)],
        )
        .expect("E");

    let forward = store
        .prov_blast_radius(ch_a, ProvDirection::Forward, 32)
        .expect("forward");
    let walked: Vec<(i64, i32, Vec<i64>, ProvLink)> = forward
        .iter()
        .map(|node| (node.id, node.depth, node.path.clone(), node.via))
        .collect();
    assert_eq!(
        walked,
        [
            (
                ch_b,
                1,
                vec![ch_a, ch_b],
                ProvLink::Edge(ProvEdgeKind::Used)
            ),
            (
                ch_c,
                2,
                vec![ch_a, ch_b, ch_c],
                ProvLink::Edge(ProvEdgeKind::Includes)
            ),
            (ch_d, 3, vec![ch_a, ch_b, ch_c, ch_d], ProvLink::Origin),
            (
                ch_e,
                4,
                vec![ch_a, ch_b, ch_c, ch_d, ch_e],
                ProvLink::Edge(ProvEdgeKind::Redacts)
            ),
        ],
        "A is not reached again through B, nor C through itself"
    );
    assert_eq!(forward[0].change_id, "B");
    assert_eq!(forward[0].rev, 1);
    assert_eq!(forward[0].agent_id, "agent_b");

    let backward = store
        .prov_blast_radius(ch_e, ProvDirection::Backward, 32)
        .expect("backward");
    let walked: Vec<(i64, i32, ProvLink)> = backward
        .iter()
        .map(|node| (node.id, node.depth, node.via))
        .collect();
    assert_eq!(
        walked,
        [
            (ch_d, 1, ProvLink::Edge(ProvEdgeKind::Redacts)),
            (ch_c, 2, ProvLink::Origin),
            (ch_b, 3, ProvLink::Edge(ProvEdgeKind::Includes)),
            (ch_a, 4, ProvLink::Edge(ProvEdgeKind::Used)),
        ]
    );

    // From inside the cycle, each way: the other, once.
    let from_b = store
        .prov_blast_radius(ch_b, ProvDirection::Backward, 32)
        .expect("backward from B");
    assert_eq!(
        from_b.iter().map(|node| node.id).collect::<Vec<_>>(),
        [ch_a]
    );

    // The bound.
    let near = store
        .prov_blast_radius(ch_a, ProvDirection::Forward, 2)
        .expect("bounded");
    assert_eq!(
        near.iter().map(|node| node.id).collect::<Vec<_>>(),
        [ch_b, ch_c]
    );
    assert!(
        store
            .prov_blast_radius(ch_a, ProvDirection::Forward, 0)
            .expect("depth 0")
            .is_empty()
    );
    assert!(
        store
            .prov_blast_radius(ch_a + 1_000, ProvDirection::Forward, 32)
            .expect("unknown")
            .is_empty()
    );
}

/// A dense cyclic graph (every change uses every other, and itself) ends
/// under a deep bound, every change reached once at depth 1.
#[test]
fn the_blast_radius_ends_on_a_dense_cycle() {
    let (store, _schema) = store("prov_dense");
    let names = ["N0", "N1", "N2", "N3", "N4", "N5"];
    let ids: Vec<i64> = names
        .iter()
        .enumerate()
        .map(|(at, name)| {
            let edges: Vec<ProvEdgeInput> = names
                .iter()
                .map(|other| edge(ProvEdgeKind::Used, other))
                .collect();
            store
                .record_change(
                    &change(name, 1, "agent_n", i64::try_from(at).expect("small")),
                    &[],
                    &edges,
                )
                .expect("record")
        })
        .collect();
    for direction in [ProvDirection::Forward, ProvDirection::Backward] {
        let radius = store
            .prov_blast_radius(ids[0], direction, 64)
            .expect("walk");
        assert_eq!(
            radius.iter().map(|node| node.id).collect::<Vec<_>>(),
            ids[1..],
            "{direction:?}"
        );
        assert!(radius.iter().all(|node| node.depth == 1), "{direction:?}");
    }
}
