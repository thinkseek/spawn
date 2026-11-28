//! Golden fixtures, canonical encoding, dual `V1`/`V2` `CommitObject`
//! decoding, AI lineage, persistent GC walk, and fail-closed validation tests
//! for `CommitV2` (`Wave-1 S1` Round-2).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use spawn_cas::{
    CHUNK_MAX, COMMIT_V2_MAGIC, CasError, CommitActor, CommitIntent, CommitObject, CommitOrigin,
    CommitV2, DEFAULT_PERSISTENT_GC_GRACE, Digest, GcFenceBinding, NS_BLOB, NS_TREE, Snapshot,
    Store, Tree, TreeEntry, WorkBoundary, WorkChange, bind_gc_fence, empty_tree_digest, sha256_of,
};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "spawn-cas-commit-v2-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch");
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct ManualClock(Arc<Mutex<SystemTime>>);

impl ManualClock {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(
            SystemTime::UNIX_EPOCH + Duration::from_hours(500_000),
        )))
    }

    fn advance(&self, by: Duration) {
        *self.0.lock().expect("clock") += by;
    }

    fn store(&self, root: &Path) -> Store {
        let clock = self.0.clone();
        Store::open(root)
            .expect("open")
            .with_clock(Arc::new(move || *clock.lock().expect("clock")))
    }
}

fn fixtures_v1_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/d6-cas-v1")
}

fn fixtures_v2_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/d6-cas-v2")
}

fn digest_n(byte: u8) -> Digest {
    Digest::from_raw_sha256([byte; 32])
}

fn sample_commit() -> CommitV2 {
    CommitV2 {
        tree: digest_n(0x11),
        parents: vec![digest_n(0x22)],
        at_ms: 1_795_700_000_000,
        actor: CommitActor::Agent {
            id: "agent-alpha".to_string(),
            execution_id: Some("exec_123".to_string()),
            model_label: Some("model-v1".to_string()),
            preset_id: Some("preset-default".to_string()),
        },
        intent: CommitIntent {
            origin: CommitOrigin::Work {
                change: WorkChange::Edit,
                boundary: WorkBoundary::Checkpoint,
            },
            task_id: Some(77),
            transcript_ref: Some("transcript://exec_123/turn-1".to_string()),
            message: "checkpoint commit".to_string(),
        },
        git_compat_body: None,
    }
}

fn verify_v2_golden_fixture(
    dir: &Path,
    filename: &str,
    len_key: &str,
    sha_key: &str,
    manifest: &serde_json::Value,
    expected: &CommitV2,
) -> Vec<u8> {
    let raw = fs::read(dir.join(filename)).expect("read v2 golden bin");
    assert_eq!(
        u64::try_from(raw.len()).expect("len"),
        manifest[len_key].as_u64().expect("manifest len")
    );
    assert_eq!(
        sha256_of(&raw).to_string(),
        manifest[sha_key].as_str().expect("manifest sha")
    );
    assert!(raw.starts_with(COMMIT_V2_MAGIC));

    let obj = CommitObject::decode(&raw).expect("decode golden obj");
    assert_eq!(obj.as_v2(), Some(expected));
    assert_eq!(CommitV2::decode(&raw).expect("decode golden v2"), *expected);
    assert_eq!(expected.encode().expect("reencode golden"), raw);
    assert_eq!(expected.digest().expect("digest"), sha256_of(&raw));
    raw
}

fn store_single_file_tree(store: &Store, name: &str, payload: &[u8]) -> (Digest, Digest) {
    let blob = store
        .put_blob(NS_BLOB, &mut std::io::Cursor::new(payload))
        .expect("put blob");
    let tree = store
        .put_tree(
            &Tree::from_entries(vec![TreeEntry::file(
                name,
                &blob.digest,
                u64::try_from(payload.len()).expect("len"),
            )])
            .expect("tree"),
        )
        .expect("put tree");
    (tree, blob.digest)
}

fn read_v2_manifest(dir_v2: &Path) -> (serde_json::Value, Digest, Digest, Digest) {
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(dir_v2.join("manifest.json")).expect("read v2 manifest"))
            .expect("parse v2 manifest");
    assert_eq!(manifest["v"], 2);
    assert_eq!(
        empty_tree_digest().to_string(),
        manifest["empty_tree_sha256"].as_str().expect("empty_tree")
    );
    let v1_tree = Digest::parse(manifest["v1_tree_sha256"].as_str().expect("v1_tree")).expect("d");
    let v1_snap =
        Digest::parse(manifest["v1_snapshot_sha256"].as_str().expect("v1_snap")).expect("d");
    let v1_parent = Digest::parse(
        manifest["v1_parent_snapshot_sha256"]
            .as_str()
            .expect("v1_parent"),
    )
    .expect("d");
    (manifest, v1_tree, v1_snap, v1_parent)
}

#[test]
fn golden_d6_cas_v2_native_and_minimal_vectors() {
    let dir_v2 = fixtures_v2_dir();
    let (manifest, v1_tree, v1_snap, _v1_parent) = read_v2_manifest(&dir_v2);

    let expected_native = CommitV2 {
        tree: v1_tree,
        parents: vec![v1_snap],
        at_ms: 1_795_700_123_456,
        actor: CommitActor::Agent {
            id: "agent-s1-coder".to_string(),
            execution_id: Some("exec_01928374".to_string()),
            model_label: Some("claude-opus-4.5".to_string()),
            preset_id: Some("preset-architect".to_string()),
        },
        intent: CommitIntent {
            origin: CommitOrigin::Work {
                change: WorkChange::Edit,
                boundary: WorkBoundary::Checkpoint,
            },
            task_id: Some(42),
            transcript_ref: Some("transcript://exec_01928374/turn-3".to_string()),
            message: "feat(cas): native spawn commit-v2\n\nIncludes UTF-8 — résumé\tand literal:\ngit_compat 3\nabc".to_string(),
        },
        git_compat_body: None,
    };
    let native_raw = verify_v2_golden_fixture(
        &dir_v2,
        "commit_native.bin",
        "commit_native_len",
        "commit_native_sha256",
        &manifest,
        &expected_native,
    );
    let native_str = std::str::from_utf8(&native_raw).expect("utf8");
    assert!(!native_str.contains("\nauthor "));
    assert!(!native_str.contains("\ncommitter "));

    let expected_minimal = CommitV2 {
        tree: empty_tree_digest(),
        parents: Vec::new(),
        at_ms: 0,
        actor: CommitActor::Human {
            id: "owner".to_string(),
        },
        intent: CommitIntent {
            origin: CommitOrigin::Import,
            task_id: None,
            transcript_ref: None,
            message: "initial import".to_string(),
        },
        git_compat_body: None,
    };
    let minimal_raw = verify_v2_golden_fixture(
        &dir_v2,
        "commit_minimal.bin",
        "commit_minimal_len",
        "commit_minimal_sha256",
        &manifest,
        &expected_minimal,
    );
    let minimal_str = std::str::from_utf8(&minimal_raw).expect("utf8");
    assert!(!minimal_str.contains("git_compat "));
    assert!(!minimal_str.contains("execution_id "));
    assert!(!minimal_str.contains("human:owner"));
}

#[test]
fn golden_d6_cas_v2_compat_and_v1_coexistence() {
    let dir_v2 = fixtures_v2_dir();
    let dir_v1 = fixtures_v1_dir();
    let (manifest, v1_tree, v1_snap, v1_parent) = read_v2_manifest(&dir_v2);

    let expected_compat = CommitV2 {
        tree: v1_tree,
        parents: vec![v1_parent, v1_snap.clone()],
        at_ms: -86_400_000,
        actor: CommitActor::System {
            id: "integrator-1".to_string(),
        },
        intent: CommitIntent {
            origin: CommitOrigin::Land,
            task_id: None,
            transcript_ref: None,
            message: "merge landing from feature branch".to_string(),
        },
        git_compat_body: Some(
            b"author Spawn <spawn@thinkseek.dev> 1795700000 +0000\ncommitter Spawn <spawn@thinkseek.dev> 1795700000 +0000\n\nmerge landing from feature branch\n"
                .to_vec(),
        ),
    };
    verify_v2_golden_fixture(
        &dir_v2,
        "commit_git_compat.bin",
        "commit_git_compat_len",
        "commit_git_compat_sha256",
        &manifest,
        &expected_compat,
    );

    // Untouched v1 snapshot still decodes via CommitObject::V1 cleanly.
    let v1_raw = fs::read(dir_v1.join("snapshot.bin")).expect("v1 snapshot.bin");
    assert_eq!(sha256_of(&v1_raw), v1_snap);
    let decoded_v1 = CommitObject::decode(&v1_raw).expect("decode v1");
    assert!(decoded_v1.as_v1().is_some());
    assert!(decoded_v1.as_v2().is_none());
    assert_eq!(decoded_v1.encode().expect("reencode v1"), v1_raw);
}

#[test]
fn deterministic_encoding_covers_all_11_origins_and_all_actors() {
    assert_eq!(CommitOrigin::ALL.len(), 11);
    let mut origin_wires = BTreeSet::new();
    let mut origin_digests = BTreeSet::new();
    let edge_times = [0_i64, -1, 1, i64::MIN, i64::MAX];

    for change in [WorkChange::Edit, WorkChange::Derive, WorkChange::Revert] {
        let json = serde_json::to_string(&change).expect("ser change");
        let back: WorkChange = serde_json::from_str(&json).expect("de change");
        assert_eq!(back, change);
        assert_eq!(WorkChange::parse(change.as_str()).expect("parse"), change);
        assert_eq!(
            change.to_string().parse::<WorkChange>().expect("str"),
            change
        );
    }
    for boundary in [
        WorkBoundary::Checkpoint,
        WorkBoundary::Submit,
        WorkBoundary::Job,
    ] {
        let json = serde_json::to_string(&boundary).expect("ser boundary");
        let back: WorkBoundary = serde_json::from_str(&json).expect("de boundary");
        assert_eq!(back, boundary);
        assert_eq!(
            WorkBoundary::parse(boundary.as_str()).expect("parse"),
            boundary
        );
        assert_eq!(
            boundary.to_string().parse::<WorkBoundary>().expect("str"),
            boundary
        );
    }

    for (idx, &origin) in CommitOrigin::ALL.iter().enumerate() {
        assert!(origin_wires.insert(origin.as_str()));
        let o_json = serde_json::to_string(&origin).expect("serialize origin");
        let o_back: CommitOrigin = serde_json::from_str(&o_json).expect("deserialize origin");
        assert_eq!(o_back, origin);
        assert_eq!(CommitOrigin::parse(origin.as_str()).expect("parse"), origin);
        assert_eq!(
            origin
                .to_string()
                .parse::<CommitOrigin>()
                .expect("from_str"),
            origin
        );

        let at_ms = edge_times[idx % edge_times.len()];
        let mut commit = sample_commit();
        commit.at_ms = at_ms;
        commit.intent.origin = origin;
        commit.intent.task_id = Some(at_ms);
        commit.intent.message = format!("line 1\t({origin})\n\ngit_compat 9\nfake_body");

        let enc_a = commit.encode().expect("encode a");
        let enc_b = commit.encode().expect("encode b");
        assert_eq!(enc_a, enc_b);
        assert!(origin_digests.insert(sha256_of(&enc_a)));
        assert_eq!(
            CommitObject::decode(&enc_a).expect("decode"),
            CommitObject::V2(commit)
        );
    }
    assert_eq!(origin_wires.len(), 11);
    assert_eq!(origin_digests.len(), 11);

    let actors = [
        CommitActor::Agent {
            id: "agent-full".to_string(),
            execution_id: Some("exec_1".to_string()),
            model_label: Some("claude-opus-4.5".to_string()),
            preset_id: Some("preset-1".to_string()),
        },
        CommitActor::Agent {
            id: "agent-bare".to_string(),
            execution_id: None,
            model_label: None,
            preset_id: None,
        },
        CommitActor::Human {
            id: "owner".to_string(),
        },
        CommitActor::System {
            id: "integrator".to_string(),
        },
    ];
    let mut actor_digests = BTreeSet::new();
    for actor in actors {
        let a_json = serde_json::to_string(&actor).expect("serialize actor");
        let a_back: CommitActor = serde_json::from_str(&a_json).expect("deserialize actor");
        assert_eq!(a_back, actor);
        let mut c = sample_commit();
        c.actor = actor;
        let wire = c.encode().expect("encode actor commit");
        assert!(actor_digests.insert(sha256_of(&wire)));
        assert_eq!(CommitV2::decode(&wire).expect("decode actor commit"), c);
    }
    assert_eq!(actor_digests.len(), 4);
}

#[test]
fn store_dual_commit_object_and_tamper_detection() {
    let scratch = Scratch::new("store-tamper");
    let store = Store::open(scratch.join("cas")).expect("open");

    let (tree_digest, _blob) = store_single_file_tree(&store, "live.txt", b"live payload");
    let v1_snap = Snapshot {
        tree: tree_digest.clone(),
        parents: Vec::new(),
        commit_body: b"author A <a@b> 1 +0000\ncommitter A <a@b> 1 +0000\n\nv1 base\n".to_vec(),
    };
    let v1_digest = store.put_merkle_snapshot(&v1_snap).expect("put v1");

    let mut v2_live = sample_commit();
    v2_live.tree = tree_digest.clone();
    v2_live.parents = vec![v1_digest.clone()];
    let v2_digest = store.put_commit_v2(&v2_live).expect("put v2");
    assert_eq!(
        store.put_commit_v2(&v2_live).expect("idempotent put"),
        v2_digest
    );

    assert_eq!(
        store.get_commit_object(&v1_digest).expect("get v1"),
        CommitObject::V1(v1_snap)
    );
    assert_eq!(
        store.get_commit_object(&v2_digest).expect("get v2"),
        CommitObject::V2(v2_live)
    );
    assert!(matches!(
        store.get_commit_object(&tree_digest),
        Err(CasError::BadFormat(_))
    ));
    let missing = digest_n(0xFE);
    assert!(
        store
            .get_commit_object(&missing)
            .is_err_and(|err| err.is_not_found())
    );

    // Flip 1 byte on disk and verify fail-closed Mismatch + automatic removal.
    let obj_path = store.object_path(&v2_digest);
    let mut raw = fs::read(&obj_path).expect("read object bytes");
    let last_idx = raw.len() - 1;
    raw[last_idx] ^= 0x01;
    fs::set_permissions(&obj_path, fs::Permissions::from_mode(0o600)).expect("chmod rw");
    fs::write(&obj_path, &raw).expect("corrupt write");
    fs::set_permissions(&obj_path, fs::Permissions::from_mode(0o444)).expect("chmod ro");
    let tamper_err = store
        .get_commit_object(&v2_digest)
        .expect_err("tampered object must fail");
    assert!(
        tamper_err.is_mismatch(),
        "expected Mismatch, got {tamper_err:?}"
    );
    assert!(!store.has(&v2_digest));
    let _ = NS_TREE;
}

#[test]
fn store_gc_persistent_retains_mixed_v2_to_v1_graph() {
    let scratch = Scratch::new("store-gc");
    let clock = ManualClock::new();
    let store = clock.store(&scratch.join("cas"));

    let (v1_tree, live_blob) = store_single_file_tree(&store, "live.txt", b"live payload");
    let v1_snap = Snapshot {
        tree: v1_tree.clone(),
        parents: Vec::new(),
        commit_body: b"author A <a@b> 1 +0000\ncommitter A <a@b> 1 +0000\n\nv1 base\n".to_vec(),
    };
    let v1_digest = store.put_merkle_snapshot(&v1_snap).expect("put v1");

    let mut v2_live = sample_commit();
    v2_live.tree = v1_tree.clone();
    v2_live.parents = vec![v1_digest.clone()];
    let v2_digest = store.put_commit_v2(&v2_live).expect("put v2");

    let (dead_tree, dead_blob) = store_single_file_tree(&store, "dead.txt", b"dead payload");
    let mut v2_dead = sample_commit();
    v2_dead.tree = dead_tree.clone();
    v2_dead.parents = Vec::new();
    v2_dead.intent.message = "unreachable dead commit".to_string();
    let v2_dead_digest = store.put_commit_v2(&v2_dead).expect("put dead v2");

    let binding = GcFenceBinding {
        db_system_id: "7441000000000000099".to_string(),
        db_oid: 20_480,
        schema_name: "public".to_string(),
    };
    bind_gc_fence(&store, &binding).expect("bind fence");

    let report1 = store
        .gc_persistent(
            std::slice::from_ref(&v2_digest),
            &binding,
            0,
            DEFAULT_PERSISTENT_GC_GRACE,
        )
        .expect("gc pass 1");
    assert!(report1.deleted_objects.is_empty());

    clock.advance(Duration::from_hours(25));
    let report2 = store
        .gc_persistent(
            std::slice::from_ref(&v2_digest),
            &binding,
            0,
            DEFAULT_PERSISTENT_GC_GRACE,
        )
        .expect("gc pass 2");
    assert!(report2.deleted_objects.contains(&v2_dead_digest));
    assert!(report2.deleted_objects.contains(&dead_tree));
    assert!(report2.deleted_objects.contains(&dead_blob));
    assert!(store.has(&v2_digest));
    assert!(store.has(&v1_digest));
    assert!(store.has(&v1_tree));
    assert!(store.has(&live_blob));
}

#[test]
fn commit_v2_exposes_full_ai_lineage_from_single_root_hash() {
    let scratch = Scratch::new("lineage");
    let store = Store::open(scratch.join("cas")).expect("open");

    let mut commit = sample_commit();
    commit.parents = vec![digest_n(0x31), digest_n(0x32)];
    commit.intent.message = "feat: converge provenance\n\nBody paragraph 1".to_string();
    let root = store.put_commit_v2(&commit).expect("put v2");

    let fetched = store.get_commit_object(&root).expect("get commit object");
    let CommitObject::V2(decoded) = fetched else {
        panic!("expected CommitObject::V2");
    };
    assert_eq!(decoded.tree, commit.tree);
    assert_eq!(decoded.parents, commit.parents);
    assert_eq!(decoded.at_ms, commit.at_ms);
    assert_eq!(decoded.actor.kind_str(), "agent");
    assert_eq!(decoded.actor.id(), "agent-alpha");
    assert_eq!(decoded.actor.execution_id(), Some("exec_123"));
    assert_eq!(decoded.actor.model_label(), Some("model-v1"));
    assert_eq!(decoded.actor.preset_id(), Some("preset-default"));
    assert_eq!(
        decoded.intent.origin,
        CommitOrigin::Work {
            change: WorkChange::Edit,
            boundary: WorkBoundary::Checkpoint,
        }
    );
    assert_eq!(decoded.intent.task_id, Some(77));
    assert_eq!(
        decoded.intent.transcript_ref.as_deref(),
        Some("transcript://exec_123/turn-1")
    );
    assert_eq!(decoded.message_subject(), "feat: converge provenance");
    assert_eq!(
        decoded.intent.message,
        "feat: converge provenance\n\nBody paragraph 1"
    );
    assert!(decoded.git_compat_body.is_none());

    let human = CommitActor::Human {
        id: "alice".to_string(),
    };
    assert_eq!(human.kind_str(), "human");
    assert_eq!(human.id(), "alice");
    assert_eq!(human.execution_id(), None);
    assert_eq!(human.model_label(), None);
    assert_eq!(human.preset_id(), None);
}

#[test]
fn fail_closed_encode_rejects_illegal_actor_and_headers() {
    for bad in [
        "",
        " ",
        "\t\n",
        " leading",
        "trailing ",
        "a\0b",
        "a\nb",
        "a\rb",
        "a\tb",
        "a\x07b",
        "a\x7fb",
        "a\u{0085}b",
    ] {
        for actor in [
            CommitActor::Agent {
                id: bad.to_string(),
                execution_id: None,
                model_label: None,
                preset_id: None,
            },
            CommitActor::Human {
                id: bad.to_string(),
            },
            CommitActor::System {
                id: bad.to_string(),
            },
        ] {
            let mut c = sample_commit();
            c.actor = actor;
            assert!(matches!(c.encode(), Err(CasError::BadFormat(_))));
        }

        let mut c = sample_commit();
        c.actor = CommitActor::Agent {
            id: "agent-alpha".to_string(),
            execution_id: Some(bad.to_string()),
            model_label: None,
            preset_id: None,
        };
        assert!(matches!(c.encode(), Err(CasError::BadFormat(_))));

        let mut c = sample_commit();
        c.actor = CommitActor::Agent {
            id: "agent-alpha".to_string(),
            execution_id: None,
            model_label: Some(bad.to_string()),
            preset_id: None,
        };
        assert!(matches!(c.encode(), Err(CasError::BadFormat(_))));

        let mut c = sample_commit();
        c.actor = CommitActor::Agent {
            id: "agent-alpha".to_string(),
            execution_id: None,
            model_label: None,
            preset_id: Some(bad.to_string()),
        };
        assert!(matches!(c.encode(), Err(CasError::BadFormat(_))));

        let mut c = sample_commit();
        c.intent.transcript_ref = Some(bad.to_string());
        assert!(matches!(c.encode(), Err(CasError::BadFormat(_))));
    }

    let mut overlong = sample_commit();
    overlong.actor = CommitActor::Human {
        id: "a".repeat(1_025),
    };
    assert!(matches!(overlong.encode(), Err(CasError::BadFormat(_))));
}

#[test]
fn fail_closed_encode_rejects_illegal_messages_parents_and_oversize() {
    for bad_msg in [
        "",
        "   ",
        "\n\t  ",
        "bad\0msg",
        "bad\rmsg",
        "bad\x07msg",
        "bad\x7fmsg",
        "bad\u{0085}msg",
    ] {
        let mut c = sample_commit();
        c.intent.message = bad_msg.to_string();
        assert!(matches!(c.encode(), Err(CasError::BadFormat(_))));
    }

    let mut empty_compat = sample_commit();
    empty_compat.git_compat_body = Some(Vec::new());
    assert!(matches!(empty_compat.encode(), Err(CasError::BadFormat(_))));

    let mut nul_compat = sample_commit();
    nul_compat.git_compat_body = Some(b"author bad\0body".to_vec());
    assert!(matches!(nul_compat.encode(), Err(CasError::BadFormat(_))));

    let mut dup_parents = sample_commit();
    dup_parents.parents = vec![digest_n(0x22), digest_n(0x22)];
    assert!(matches!(dup_parents.encode(), Err(CasError::BadFormat(_))));

    let mut too_many_parents = sample_commit();
    too_many_parents.parents = (0_u16..1_025)
        .map(|i| {
            let mut raw = [0_u8; 32];
            raw[..2].copy_from_slice(&i.to_be_bytes());
            Digest::from_raw_sha256(raw)
        })
        .collect();
    assert!(matches!(
        too_many_parents.encode(),
        Err(CasError::BadFormat(_))
    ));

    let mut too_big = sample_commit();
    too_big.intent.message = "x".repeat(CHUNK_MAX + 1);
    assert!(matches!(
        too_big.encode(),
        Err(CasError::TooLarge { kind: "commit", .. })
    ));
}

#[test]
fn fail_closed_decode_rejects_contradictory_origins_and_bad_actors() {
    let base_tree = digest_n(0x11);

    let bad_origins = [
        "kind edit\nboundary checkpoint",
        "origin land/checkpoint",
        "origin import/submit",
        "origin edit/land",
        "origin land/backfill",
        "origin work/land/checkpoint",
        "origin work/import/submit",
        "origin work/edit",
        "origin work/edit/land",
        "origin work/edit/backfill",
        "origin work/edit/checkpoint/extra",
        "origin backfill",
        "origin checkpoint",
        "origin edit",
        "origin Land",
        "origin merge",
    ];
    for raw_origin in bad_origins {
        let wire = format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms 1\nactor agent a\n{raw_origin}\nmessage 2\nhi"
        );
        assert!(
            CommitV2::decode(wire.as_bytes()).is_err(),
            "unexpectedly accepted {raw_origin:?}"
        );
        assert!(CommitOrigin::parse(raw_origin.trim_start_matches("origin ")).is_err());
    }

    let bad_actors = [
        "agent_id a",
        "actor bot x",
        "actor human",
        "actor human ",
        "actor human  owner",
        "actor agent agent-1\nmodel_label m\nexecution_id e",
        "actor agent agent-1\nexecution_id e1\nexecution_id e2",
        "actor human owner\nexecution_id e1",
        "actor human owner\nmodel_label m1",
        "actor human owner\npreset_id p1",
        "actor system sys\nexecution_id e1",
        "actor system sys\nmodel_label m1",
        "actor system sys\npreset_id p1",
        "actor agent a\tb",
        "actor agent  leading",
    ];
    for raw_actor in bad_actors {
        let wire = format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms 1\n{raw_actor}\norigin work/edit/checkpoint\nmessage 2\nhi"
        );
        assert!(
            CommitV2::decode(wire.as_bytes()).is_err(),
            "unexpectedly accepted actor {raw_actor:?}"
        );
    }

    assert!(WorkChange::parse("land").is_err());
    assert!(WorkChange::parse("import").is_err());
    assert!(WorkBoundary::parse("land").is_err());
    assert!(WorkBoundary::parse("backfill").is_err());
}

#[test]
fn fail_closed_decode_rejects_header_and_number_violations() {
    let base_tree = digest_n(0x11);
    let base_p1 = digest_n(0x22);

    let bad_wires: Vec<Vec<u8>> = vec![
        b"spawn-commit-v1\ntree sha256:0000000000000000000000000000000000000000000000000000000000000000\n".to_vec(),
        b"spawn-tree-v1\n\x00\x00\x00\x00".to_vec(),
        b"spawn-commit-v2".to_vec(),
        format!(
            "spawn-commit-v2\ntree {base_tree}\nunknown foo\nat_ms 1\nactor agent a\norigin work/edit/checkpoint\nmessage 2\nhi"
        )
        .into_bytes(),
        format!(
            "spawn-commit-v2\ntree {base_tree}\nparent {base_p1}\nparent {base_p1}\nat_ms 1\nactor agent a\norigin work/edit/checkpoint\nmessage 2\nhi"
        )
        .into_bytes(),
        format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms +1\nactor agent a\norigin work/edit/checkpoint\nmessage 2\nhi"
        )
        .into_bytes(),
        format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms 01\nactor agent a\norigin work/edit/checkpoint\nmessage 2\nhi"
        )
        .into_bytes(),
        format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms -0\nactor agent a\norigin work/edit/checkpoint\nmessage 2\nhi"
        )
        .into_bytes(),
        format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms 9223372036854775808\nactor agent a\norigin work/edit/checkpoint\nmessage 2\nhi"
        )
        .into_bytes(),
        format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms 1\nactor agent a\norigin work/edit/checkpoint\ntask_id 07\nmessage 2\nhi"
        )
        .into_bytes(),
        format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms 1\nactor agent a\norigin work/edit/checkpoint\ntranscript_ref t\ntask_id 7\nmessage 2\nhi"
        )
        .into_bytes(),
    ];

    for (idx, wire) in bad_wires.iter().enumerate() {
        assert!(
            CommitV2::decode(wire).is_err(),
            "bad_wire[{idx}] unexpectedly decoded via CommitV2::decode"
        );
        assert!(
            CommitObject::decode(wire).is_err(),
            "bad_wire[{idx}] unexpectedly decoded via CommitObject::decode"
        );
    }
}

#[test]
fn fail_closed_decode_rejects_payload_lengths_limits_and_trailers() {
    let base_tree = digest_n(0x11);
    let base_p2 = digest_n(0x33);

    let mut non_utf8_msg = format!(
        "spawn-commit-v2\ntree {base_tree}\nat_ms 1\nactor agent a\norigin work/edit/checkpoint\nmessage 2\n"
    )
    .into_bytes();
    non_utf8_msg.extend_from_slice(&[0xFF, 0xFE]);

    let bad_payloads: Vec<Vec<u8>> = vec![
        format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms 1\nactor agent a\norigin work/edit/checkpoint\nmessage 0\n"
        )
        .into_bytes(),
        format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms 1\nactor agent a\norigin work/edit/checkpoint\nmessage 5\nhi"
        )
        .into_bytes(),
        format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms 1\nactor agent a\norigin work/edit/checkpoint\nmessage 99999999999999999999999\nhi"
        )
        .into_bytes(),
        non_utf8_msg,
        format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms 1\nactor agent a\norigin work/edit/checkpoint\nmessage 4\nhi\r!"
        )
        .into_bytes(),
        format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms 1\nactor agent a\norigin work/edit/checkpoint\nmessage 2\nhi!"
        )
        .into_bytes(),
        format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms 1\nactor agent a\norigin work/edit/checkpoint\nmessage 2\nhi\nunknown 1\nx"
        )
        .into_bytes(),
        format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms 1\nactor agent a\norigin work/edit/checkpoint\nmessage 2\nhi\ngit_compat 0\n"
        )
        .into_bytes(),
        format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms 1\nactor agent a\norigin work/edit/checkpoint\nmessage 2\nhi\ngit_compat 4\nabc"
        )
        .into_bytes(),
        format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms 1\nactor agent a\norigin work/edit/checkpoint\nmessage 2\nhi\ngit_compat 3\nabcd"
        )
        .into_bytes(),
        format!(
            "spawn-commit-v2\ntree {base_tree}\nat_ms 1\nactor agent a\norigin work/edit/checkpoint\nmessage 2\nhi\ngit_compat 3\na\0c"
        )
        .into_bytes(),
    ];

    for (idx, wire) in bad_payloads.iter().enumerate() {
        assert!(
            CommitV2::decode(wire).is_err(),
            "bad_payload[{idx}] unexpectedly decoded"
        );
        assert!(
            CommitObject::decode(wire).is_err(),
            "bad_payload[{idx}] unexpectedly decoded via CommitObject"
        );
    }

    let mut wire_too_many_parents = format!("spawn-commit-v2\ntree {base_tree}\n");
    for i in 0_u16..1_025 {
        let mut raw = [0_u8; 32];
        raw[..2].copy_from_slice(&i.to_be_bytes());
        let _ = writeln!(
            wire_too_many_parents,
            "parent {}",
            Digest::from_raw_sha256(raw)
        );
    }
    wire_too_many_parents
        .push_str("at_ms 1\nactor agent a\norigin work/edit/checkpoint\nmessage 2\nhi");
    assert!(matches!(
        CommitV2::decode(wire_too_many_parents.as_bytes()),
        Err(CasError::BadFormat(_))
    ));

    let mut oversized_wire = Vec::with_capacity(CHUNK_MAX + 64);
    oversized_wire.extend_from_slice(COMMIT_V2_MAGIC);
    oversized_wire.extend_from_slice(format!("tree {base_p2}\n").as_bytes());
    oversized_wire.resize(CHUNK_MAX + 1, b'x');
    assert!(matches!(
        CommitV2::decode(&oversized_wire),
        Err(CasError::TooLarge { kind: "commit", .. })
    ));
}
