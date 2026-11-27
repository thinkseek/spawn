//! `RefLedger::record_snapshot` canonical-id semantics on the in-memory ledger,
//! mirroring `spawn_snapshot_record` (`#113`): concurrent writers that record
//! the same Git commit (same `git_oid`, `tree`, `parents`, `project`) under a
//! different digest or timestamp get the canonical existing id back, and a
//! ref move through that id succeeds; real conflicts are still refused.

use spawn_cas::{Digest, GitOid};
use spawn_host::{D6RefKind, InMemoryRefLedger, RefLedger};

fn digest(fill: &str) -> Digest {
    Digest::parse(&format!("sha256:{}", fill.repeat(32))).expect("digest")
}

fn oid(fill: &str) -> GitOid {
    GitOid::parse_hex(&fill.repeat(20)).expect("git oid")
}

#[test]
fn same_git_oid_under_another_id_returns_the_canonical_id() {
    let ledger = InMemoryRefLedger::new();
    let (first, second) = (digest("11"), digest("12"));
    let (tree, parent) = (digest("22"), digest("00"));
    let parents = vec![parent];
    assert_eq!(
        ledger
            .record_snapshot(&first, &oid("aa"), &tree, &parents, "proj", 1_000)
            .expect("first writer"),
        first
    );
    let canonical = ledger
        .record_snapshot(&second, &oid("aa"), &tree, &parents, "proj", 1_007)
        .expect("same commit, other digest and timestamp");
    assert_eq!(canonical, first);
    assert!(ledger.get_snapshot(&second).expect("read").is_none());

    // The canonical id satisfies ref moves (the FK in PostgreSQL).
    ledger
        .cas_ref("proj", "b", D6RefKind::Branch, None, &canonical, "r")
        .expect("cas through canonical id");
}

#[test]
fn same_id_with_another_timestamp_is_idempotent_first_writer_wins() {
    let ledger = InMemoryRefLedger::new();
    let (id, tree) = (digest("31"), digest("32"));
    ledger
        .record_snapshot(&id, &oid("bb"), &tree, &[], "proj", 5_000)
        .expect("first");
    assert_eq!(
        ledger
            .record_snapshot(&id, &oid("bb"), &tree, &[], "proj", 5_999)
            .expect("replay"),
        id
    );
    let row = ledger.get_snapshot(&id).expect("read").expect("row");
    assert_eq!(row.created_at_ms, 5_000);
}

#[test]
fn conflicting_content_is_still_refused() {
    let ledger = InMemoryRefLedger::new();
    let (first, other) = (digest("41"), digest("42"));
    let tree = digest("43");
    ledger
        .record_snapshot(&first, &oid("cc"), &tree, &[], "proj", 1)
        .expect("first");
    // Same git_oid, other id, mismatched tree / project / parents.
    for (t, parents, project) in [
        (digest("44"), vec![], "proj"),
        (tree.clone(), vec![], "other"),
        (tree.clone(), vec![digest("45")], "proj"),
    ] {
        assert!(
            ledger
                .record_snapshot(&other, &oid("cc"), &t, &parents, project, 1)
                .is_err()
        );
    }
    // Same id, other git_oid or tree.
    assert!(
        ledger
            .record_snapshot(&first, &oid("cd"), &tree, &[], "proj", 1)
            .is_err()
    );
    assert!(
        ledger
            .record_snapshot(&first, &oid("cc"), &digest("46"), &[], "proj", 1)
            .is_err()
    );
}
