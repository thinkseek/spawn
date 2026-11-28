//! D6 Phase 4 Step 1 (`#113` Fine Item 7.2): `refs.journal`, GC Fence,
//! write-reuse mtime refresh under `LOCK_SH`, and 2-round / 24h-grace
//! reachability GC over Merkle trees, blobs, and FastCDC chunked objects.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use spawn_cas::{
    CasError, DEFAULT_PERSISTENT_GC_GRACE, Digest, GC_UNREACH_STATE_FILE, GcFenceBinding, GitOid,
    JournalEntry, NS_ARTIFACTS, NS_BLOB, Quotas, Snapshot, Store, Tree, TreeEntry, bind_gc_fence,
    sha256_of, verify_gc_fence,
};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "spawn-cas-d6-pg-{tag}-{}-{}",
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

    fn now_ms(&self) -> i64 {
        let dur = self
            .0
            .lock()
            .expect("clock")
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("after epoch");
        i64::try_from(dur.as_millis()).expect("ms fits i64")
    }

    fn store(&self, root: &Path) -> Store {
        let clock = self.0.clone();
        Store::open(root)
            .expect("open")
            .with_clock(Arc::new(move || *clock.lock().expect("clock")))
    }
}

fn pseudo_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut x = seed;
    while out.len() < len {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.extend_from_slice(&z.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn binding_a() -> GcFenceBinding {
    GcFenceBinding {
        db_system_id: "7441000000000000001".to_string(),
        db_oid: 16_385,
        schema_name: "public".to_string(),
    }
}

fn journal_entry(
    seq: u64,
    name: &str,
    old_snapshot: Option<Digest>,
    new_snapshot: Digest,
    oid_fill: &str,
    at_ms: i64,
) -> JournalEntry {
    JournalEntry {
        seq,
        project: "spawn".to_string(),
        name: name.to_string(),
        kind: if name == "main" { "main" } else { "branch" }.to_string(),
        old_snapshot,
        new_snapshot,
        git_oid: GitOid::parse_hex(&oid_fill.repeat(20)).expect("oid"),
        at_ms,
    }
}

#[test]
fn journal_appends_tolerates_torn_tail_and_reconciles_strict_fast_forward() {
    let scratch = Scratch::new("journal");
    let store = Store::open(scratch.join("cas")).expect("open");
    assert_eq!(store.latest_seq().expect("empty latest_seq"), 0);
    assert!(store.read_ref_journal().expect("empty read").is_empty());

    let s1 = sha256_of(b"s1");
    let s2 = sha256_of(b"s2");
    let s3 = sha256_of(b"s3");

    let e1 = journal_entry(1, "branch/a", None, s1.clone(), "11", 1000);
    let e2 = journal_entry(2, "branch/a", Some(s1.clone()), s2.clone(), "22", 2000);
    let e3 = journal_entry(3, "branch/a", Some(s2.clone()), s3.clone(), "33", 3000);

    store.append_ref_journal(&e1).expect("append e1");
    store.append_ref_journal(&e2).expect("append e2");
    store.append_ref_journal(&e3).expect("append e3");
    assert_eq!(store.latest_seq().expect("latest_seq"), 3);

    let mode = fs::metadata(store.refs_journal_path())
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);

    // Simulate a crash mid-write leaving an unterminated partial line.
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(store.refs_journal_path())
        .expect("open journal");
    file.write_all(b"{\"seq\":4,\"project\":\"spawn\",\"name\":\"torn")
        .expect("write torn tail");
    drop(file);

    let all = store.read_ref_journal().expect("tolerates torn tail");
    assert_eq!(all, vec![e1.clone(), e2.clone(), e3.clone()]);
    assert_eq!(store.latest_seq().expect("latest_seq after torn"), 3);

    let replay = store.reconcile_entries_after(1).expect("reconcile > 1");
    assert_eq!(replay, vec![e2, e3]);
    assert!(
        store
            .reconcile_entries_after(3)
            .expect("empty suffix")
            .is_empty()
    );

    // A broken fast-forward chain in the replay suffix is refused.
    let bad_scratch = Scratch::new("journal-broken");
    let bad_store = Store::open(bad_scratch.join("cas")).expect("open");
    bad_store
        .append_ref_journal(&journal_entry(1, "branch/a", None, s1.clone(), "11", 100))
        .expect("seq 1");
    bad_store
        .append_ref_journal(&journal_entry(2, "branch/a", Some(s3), s2, "22", 200))
        .expect("seq 2");
    let err = bad_store
        .reconcile_entries_after(0)
        .expect_err("broken chain refused");
    assert!(matches!(err, CasError::BadFormat(_)));

    // Symlink at refs.journal is refused with NotPlain.
    let sym_scratch = Scratch::new("journal-symlink");
    let sym_store = Store::open(sym_scratch.join("cas")).expect("open");
    symlink(sym_scratch.join("target"), sym_store.refs_journal_path()).expect("symlink");
    assert!(matches!(
        sym_store.read_ref_journal(),
        Err(CasError::NotPlain(_))
    ));
    assert!(matches!(
        sym_store.append_ref_journal(&e1),
        Err(CasError::NotPlain(_))
    ));
}

#[test]
fn gc_fence_blocks_unbound_mismatch_and_lagging_pg() {
    let scratch = Scratch::new("gc-fence");
    let clock = ManualClock::new();
    let store = clock.store(&scratch.join("cas"));
    let live = binding_a();

    // Unbound CAS root refuses verification and gc_persistent.
    assert!(matches!(
        verify_gc_fence(&store, &live, 0, 0),
        Err(CasError::GcFenceMissing(_))
    ));
    assert!(matches!(
        store.gc_persistent(&[], &live, 0, DEFAULT_PERSISTENT_GC_GRACE),
        Err(CasError::GcFenceMissing(_))
    ));

    // Bind idempotently.
    bind_gc_fence(&store, &live).expect("bind");
    bind_gc_fence(&store, &live).expect("idempotent rebind");

    // Rebinding to another DB / schema fails.
    let wrong_schema = GcFenceBinding {
        schema_name: "restore_drill".to_string(),
        ..live.clone()
    };
    assert!(matches!(
        bind_gc_fence(&store, &wrong_schema),
        Err(CasError::GcFenceMismatch { .. })
    ));

    // Live DB mismatch fails verify and gc_persistent.
    let wrong_db = GcFenceBinding {
        db_oid: 99_999,
        ..live.clone()
    };
    assert!(matches!(
        verify_gc_fence(&store, &wrong_db, 0, 0),
        Err(CasError::GcFenceMismatch { .. })
    ));
    assert!(matches!(
        store.gc_persistent(&[], &wrong_db, 0, DEFAULT_PERSISTENT_GC_GRACE),
        Err(CasError::GcFenceMismatch { .. })
    ));

    // PG lagging behind refs.journal refuses GC.
    let s1 = sha256_of(b"j1");
    store
        .append_ref_journal(&journal_entry(5, "main", None, s1, "aa", clock.now_ms()))
        .expect("append seq 5");
    assert!(matches!(
        store.gc_persistent(&[], &live, 4, DEFAULT_PERSISTENT_GC_GRACE),
        Err(CasError::GcFenceBehindJournal {
            pg_latest_ref_seq: 4,
            journal_latest_seq: 5,
        })
    ));

    // Once PG catches up to >= journal latest_seq, fence verification succeeds.
    verify_gc_fence(&store, &live, 5, 5).expect("verified");
    store
        .gc_persistent(&[], &live, 5, DEFAULT_PERSISTENT_GC_GRACE)
        .expect("gc proceeds");
}

struct PersistentGcFixture {
    live_snap: Digest,
    live_tree: Digest,
    child_tree: Digest,
    live_small: Digest,
    shared_digest: Digest,
    live_chunked_digest: Digest,
    live_chunked_bytes: Vec<u8>,
    live_chunk_ids: Vec<Digest>,
    dead_snap: Digest,
    dead_tree: Digest,
    dead_small: Digest,
    reused_dead: Digest,
    dead_chunked_digest: Digest,
    dead_chunk_ids: Vec<Digest>,
    pinned_dead: Digest,
    ephem_shared: Digest,
}

fn make_chunked_blob(store: &Store, seed: u64) -> (Digest, u64, Vec<u8>, Vec<Digest>) {
    let bytes = pseudo_bytes(4_500_000, seed);
    let stored = store
        .put_blob(NS_BLOB, &mut bytes.as_slice())
        .expect("put_blob");
    assert!(stored.chunked);
    let chunk_ids = store
        .read_chunk_index(&stored.digest)
        .expect("chunk index")
        .chunks
        .into_iter()
        .map(|chunk| chunk.id)
        .collect();
    (stored.digest, stored.size, bytes, chunk_ids)
}

fn make_snapshot(store: &Store, tree: &Digest, msg: &str) -> Digest {
    store
        .put_merkle_snapshot(&Snapshot {
            tree: tree.clone(),
            parents: Vec::new(),
            commit_body: format!("author a <a@b> 0 +0000\ncommitter a <a@b> 0 +0000\n\n{msg}\n")
                .into_bytes(),
        })
        .expect("put_merkle_snapshot")
}

fn setup_persistent_gc_fixture(store: &Store) -> PersistentGcFixture {
    let shared_bytes = b"shared across live and dead";
    let shared_digest = store.put_bytes(NS_BLOB, shared_bytes).expect("shared");
    let shared_len = u64::try_from(shared_bytes.len()).expect("len");

    let live_small_bytes = b"live child file";
    let live_small = store
        .put_bytes(NS_BLOB, live_small_bytes)
        .expect("live_small");
    let (live_chunked_digest, live_chunked_size, live_chunked_bytes, live_chunk_ids) =
        make_chunked_blob(store, 0x1111_2222_3333_4444);

    let child_tree = store
        .put_tree(
            &Tree::from_entries(vec![TreeEntry::file(
                b"inner.txt",
                &live_small,
                u64::try_from(live_small_bytes.len()).expect("len"),
            )])
            .expect("child tree"),
        )
        .expect("put child tree");
    let live_tree = store
        .put_tree(
            &Tree::from_entries(vec![
                TreeEntry::dir(b"sub", &child_tree),
                TreeEntry::file(b"shared.txt", &shared_digest, shared_len),
                TreeEntry::file(b"big.bin", &live_chunked_digest, live_chunked_size),
            ])
            .expect("live tree"),
        )
        .expect("put live tree");
    let live_snap = make_snapshot(store, &live_tree, "live");

    let dead_small = store
        .put_bytes(NS_BLOB, b"dead standalone blob")
        .expect("dead_small");
    let reused_dead = store
        .put_bytes(NS_BLOB, b"dead blob reused before deletion")
        .expect("reused_dead");
    let (dead_chunked_digest, dead_chunked_size, _, dead_chunk_ids) =
        make_chunked_blob(store, 0xaaaa_bbbb_cccc_dddd);
    let dead_tree = store
        .put_tree(
            &Tree::from_entries(vec![
                TreeEntry::file(b"dead.txt", &dead_small, 20),
                TreeEntry::file(b"shared.txt", &shared_digest, shared_len),
                TreeEntry::file(b"dead-big.bin", &dead_chunked_digest, dead_chunked_size),
            ])
            .expect("dead tree"),
        )
        .expect("put dead tree");
    let dead_snap = make_snapshot(store, &dead_tree, "dead");

    let pinned_dead = store.put_bytes(NS_BLOB, b"pinned dead").expect("pinned");
    store
        .pin("lease", "hold-1", std::slice::from_ref(&pinned_dead))
        .expect("pin");
    let ephem_shared = store
        .put_bytes(NS_ARTIFACTS, b"shared with artifacts")
        .expect("ephem");
    assert_eq!(
        store
            .put_bytes(NS_BLOB, b"shared with artifacts")
            .expect("persist shared"),
        ephem_shared
    );

    PersistentGcFixture {
        live_snap,
        live_tree,
        child_tree,
        live_small,
        shared_digest,
        live_chunked_digest,
        live_chunked_bytes,
        live_chunk_ids,
        dead_snap,
        dead_tree,
        dead_small,
        reused_dead,
        dead_chunked_digest,
        dead_chunk_ids,
        pinned_dead,
        ephem_shared,
    }
}

fn assert_live_closure_intact(store: &Store, fx: &PersistentGcFixture) {
    assert!(store.has(&fx.pinned_dead));
    for alive in [
        &fx.live_snap,
        &fx.live_tree,
        &fx.child_tree,
        &fx.live_small,
        &fx.shared_digest,
    ] {
        assert!(store.has(alive));
    }
    assert!(store.chunk_index_path(&fx.live_chunked_digest).is_file());
    for chunk_id in &fx.live_chunk_ids {
        assert!(store.has(chunk_id));
    }
    assert_eq!(
        store
            .read_blob(&fx.live_chunked_digest)
            .expect("stream live chunked"),
        fx.live_chunked_bytes
    );
}

#[test]
fn persistent_gc_enforces_two_rounds_24h_grace_write_reuse_and_chunked_blobs() {
    let scratch = Scratch::new("persist-gc");
    let clock = ManualClock::new();
    let store = clock.store(&scratch.join("cas"));
    let live = binding_a();
    store.bind_gc_fence(&live).expect("bind");
    let fx = setup_persistent_gc_fixture(&store);

    let r1 = store
        .gc_persistent(
            std::slice::from_ref(&fx.live_snap),
            &live,
            0,
            DEFAULT_PERSISTENT_GC_GRACE,
        )
        .expect("round 1");
    assert!(r1.deleted_objects.is_empty());
    assert!(r1.deleted_chunk_indices.is_empty());
    assert!(r1.deferred_objects >= 4);
    assert_eq!(r1.deferred_chunk_indices, 1);

    clock.advance(Duration::from_hours(12));
    assert_eq!(
        store
            .put_bytes(NS_BLOB, b"dead blob reused before deletion")
            .expect("reuse put_bytes"),
        fx.reused_dead
    );
    let r2 = store
        .gc_persistent(
            std::slice::from_ref(&fx.live_snap),
            &live,
            0,
            DEFAULT_PERSISTENT_GC_GRACE,
        )
        .expect("round 2");
    assert!(r2.deleted_objects.is_empty());
    assert!(r2.deleted_chunk_indices.is_empty());

    let quotas = Quotas::new();
    let eph = store.gc(&quotas).expect("ephemeral gc");
    assert_eq!(eph.stale_tmp, 0);
    assert!(store.gc_unreach_state_path().is_file());

    clock.advance(Duration::from_hours(13));
    let r3 = store
        .gc_persistent(
            std::slice::from_ref(&fx.live_snap),
            &live,
            0,
            DEFAULT_PERSISTENT_GC_GRACE,
        )
        .expect("round 3");

    assert!(r3.deleted_chunk_indices.contains(&fx.dead_chunked_digest));
    assert!(!store.chunk_index_path(&fx.dead_chunked_digest).exists());
    for chunk_id in &fx.dead_chunk_ids {
        assert!(r3.deleted_objects.contains(chunk_id));
        assert!(!store.has(chunk_id));
    }
    for gone in [&fx.dead_snap, &fx.dead_tree, &fx.dead_small] {
        assert!(r3.deleted_objects.contains(gone));
        assert!(!store.has(gone));
    }
    assert!(
        store.has(&fx.reused_dead),
        "write-reuse mtime refresh spared object"
    );
    assert!(r3.deleted_objects.contains(&fx.ephem_shared));
    assert!(store.has(&fx.ephem_shared));
    assert_eq!(
        store.namespaces_of(&fx.ephem_shared).expect("ns"),
        vec![NS_ARTIFACTS]
    );
    assert_live_closure_intact(&store, &fx);

    clock.advance(Duration::from_hours(25));
    let r4 = store
        .gc_persistent(
            std::slice::from_ref(&fx.live_snap),
            &live,
            0,
            DEFAULT_PERSISTENT_GC_GRACE,
        )
        .expect("round 4");
    assert!(r4.deleted_objects.contains(&fx.reused_dead));
    assert!(!store.has(&fx.reused_dead));

    clock.advance(Duration::from_hours(8 * 24));
    store.gc(&quotas).expect("stale tmp sweep");
    assert!(
        store
            .root()
            .join("tmp")
            .join(GC_UNREACH_STATE_FILE)
            .is_file(),
        "ephemeral tmp sweep must preserve persistent GC state"
    );
    assert!(
        store.has(&fx.live_snap),
        "ephemeral GC never evicts NS_TREE"
    );
    assert!(
        store.has(&fx.live_small),
        "ephemeral GC never evicts NS_BLOB"
    );
}

#[test]
fn persistent_gc_resets_rounds_when_object_becomes_reachable_again() {
    let scratch = Scratch::new("persist-revive");
    let clock = ManualClock::new();
    let store = clock.store(&scratch.join("cas"));
    let live = binding_a();
    store.bind_gc_fence(&live).expect("bind");

    let blob = store.put_bytes(NS_BLOB, b"revivable blob").expect("blob");
    let tree = store
        .put_tree(&Tree::from_entries(vec![TreeEntry::file(b"f.txt", &blob, 14)]).expect("tree"))
        .expect("put tree");
    let snap = store
        .put_merkle_snapshot(&Snapshot {
            tree: tree.clone(),
            parents: Vec::new(),
            commit_body: b"author a <a@b> 0 +0000\ncommitter a <a@b> 0 +0000\n\nrevive\n".to_vec(),
        })
        .expect("snap");

    // Round 1 with empty roots: unreachable (round 1).
    let r1 = store
        .gc_persistent(&[], &live, 0, DEFAULT_PERSISTENT_GC_GRACE)
        .expect("r1");
    assert!(r1.deleted_objects.is_empty());

    // Round 2 with snap in roots: reachable again, clears consecutive unreachable count.
    clock.advance(Duration::from_hours(12));
    let r2 = store
        .gc_persistent(
            std::slice::from_ref(&snap),
            &live,
            0,
            DEFAULT_PERSISTENT_GC_GRACE,
        )
        .expect("r2");
    assert!(r2.deleted_objects.is_empty());
    assert_eq!(r2.deferred_objects, 0);

    // Round 3 with empty roots at +24h total: only Round 1 since re-unreachability -> kept!
    clock.advance(Duration::from_hours(24));
    let r3 = store
        .gc_persistent(&[], &live, 0, DEFAULT_PERSISTENT_GC_GRACE)
        .expect("r3");
    assert!(r3.deleted_objects.is_empty());
    assert!(store.has(&blob));

    // Round 4 after 1h (< 24h since Round 3) -> still kept!
    clock.advance(Duration::from_hours(1));
    let r4 = store
        .gc_persistent(&[], &live, 0, DEFAULT_PERSISTENT_GC_GRACE)
        .expect("r4");
    assert!(r4.deleted_objects.is_empty());
    assert!(store.has(&blob));

    // Round 5 after +24h from Round 3 -> deleted!
    clock.advance(Duration::from_hours(24));
    let r5 = store
        .gc_persistent(&[], &live, 0, DEFAULT_PERSISTENT_GC_GRACE)
        .expect("r5");
    assert!(r5.deleted_objects.contains(&blob));
    assert!(r5.deleted_objects.contains(&tree));
    assert!(r5.deleted_objects.contains(&snap));
    assert!(!store.has(&blob));
}

#[test]
fn journal_append_heals_torn_tail_after_committed_entries_and_restores_reconcile() {
    let scratch = Scratch::new("journal-heal-tail");
    let store = Store::open(scratch.join("cas")).expect("open");

    let s1 = sha256_of(b"heal-s1");
    let s2 = sha256_of(b"heal-s2");
    let s3 = sha256_of(b"heal-s3");

    let e1 = journal_entry(1, "branch/a", None, s1.clone(), "11", 1000);
    let e2 = journal_entry(2, "branch/a", Some(s1), s2.clone(), "22", 2000);
    let e3 = journal_entry(3, "branch/a", Some(s2), s3, "33", 3000);

    store.append_ref_journal(&e1).expect("append e1");
    store.append_ref_journal(&e2).expect("append e2");

    // Simulate a crash mid-append leaving an unterminated fragment at EOF.
    let mut raw = fs::OpenOptions::new()
        .append(true)
        .open(store.refs_journal_path())
        .expect("open journal");
    raw.write_all(b"{\"seq\":3,\"project\":\"spawn\",\"name\":\"torn")
        .expect("inject torn tail");
    drop(raw);

    assert_eq!(
        store.read_ref_journal().expect("read with torn tail"),
        vec![e1.clone(), e2.clone()]
    );
    assert_eq!(store.latest_seq().expect("latest_seq before heal"), 2);

    // Production append must heal the torn tail instead of concatenating onto it.
    store
        .append_ref_journal(&e3)
        .expect("append e3 heals torn tail");

    let expected = vec![e1, e2, e3];
    assert_eq!(store.read_ref_journal().expect("read after heal"), expected);
    assert_eq!(store.latest_seq().expect("latest_seq after heal"), 3);
    assert_eq!(
        store
            .reconcile_entries_after(0)
            .expect("reconcile after heal"),
        expected
    );

    let on_disk = fs::read(store.refs_journal_path()).expect("read healed bytes");
    assert!(on_disk.ends_with(b"\n"));
    let on_disk_text = std::str::from_utf8(&on_disk).expect("valid utf-8 journal");
    assert_eq!(on_disk_text.lines().count(), 3);
}

#[test]
fn journal_append_heals_torn_tail_only_file_without_any_newline() {
    let s1 = sha256_of(b"torn-only-s1");
    let e1 = journal_entry(1, "main", None, s1, "44", 4000);

    // Sub-case A: short unterminated fragment (no `\n` anywhere in file).
    let short_scratch = Scratch::new("journal-torn-only-short");
    let short_store = Store::open(short_scratch.join("cas")).expect("open");
    fs::write(
        short_store.refs_journal_path(),
        b"{\"seq\":1,\"torn\xff\xfe",
    )
    .expect("seed short torn-only journal");

    assert!(
        short_store
            .read_ref_journal()
            .expect("empty before heal")
            .is_empty()
    );
    assert_eq!(short_store.latest_seq().expect("seq 0 before heal"), 0);
    short_store
        .append_ref_journal(&e1)
        .expect("append seq 1 heals short torn-only file");
    assert_eq!(
        short_store.read_ref_journal().expect("read healed"),
        vec![e1.clone()]
    );
    assert_eq!(short_store.latest_seq().expect("latest seq 1"), 1);
    assert_eq!(
        short_store.reconcile_entries_after(0).expect("reconcile"),
        vec![e1.clone()]
    );

    // Sub-case B: >16 KiB unterminated fragment crossing multiple backward scan chunks.
    let long_scratch = Scratch::new("journal-torn-only-long");
    let long_store = Store::open(long_scratch.join("cas")).expect("open");
    fs::write(long_store.refs_journal_path(), vec![b'x'; 20_000]).expect("seed multi-chunk torn");

    long_store
        .append_ref_journal(&e1)
        .expect("append seq 1 heals multi-chunk torn-only file");
    assert_eq!(
        long_store
            .read_ref_journal()
            .expect("read multi-chunk healed"),
        vec![e1.clone()]
    );
    assert_eq!(
        long_store.reconcile_entries_after(0).expect("reconcile"),
        vec![e1]
    );
}

#[test]
fn journal_append_preserves_corrupt_newline_terminated_middle_line_fail_closed() {
    let s1 = sha256_of(b"corrupt-mid-s1");
    let s2 = sha256_of(b"corrupt-mid-s2");
    let s3 = sha256_of(b"corrupt-mid-s3");

    let e1 = journal_entry(1, "branch/a", None, s1.clone(), "11", 1000);
    let e2 = journal_entry(2, "branch/a", Some(s1), s2.clone(), "22", 2000);
    let e3 = journal_entry(3, "branch/a", Some(s2), s3, "33", 3000);

    // Case A: valid line + corrupt `\n`-terminated line + valid line, then append.
    let mid_scratch = Scratch::new("journal-corrupt-middle");
    let mid_store = Store::open(mid_scratch.join("cas")).expect("open");
    mid_store.append_ref_journal(&e1).expect("append e1");
    let mut mid_raw = fs::OpenOptions::new()
        .append(true)
        .open(mid_store.refs_journal_path())
        .expect("open mid");
    mid_raw
        .write_all(b"{\"seq\":2,\"corrupt\":\n")
        .expect("write corrupt newline line");
    drop(mid_raw);
    mid_store.append_ref_journal(&e2).expect("append e2");
    mid_store.append_ref_journal(&e3).expect("append e3");

    assert!(matches!(
        mid_store.read_ref_journal(),
        Err(CasError::BadFormat(_))
    ));
    assert!(matches!(
        mid_store.latest_seq(),
        Err(CasError::BadFormat(_))
    ));
    assert!(matches!(
        mid_store.reconcile_entries_after(0),
        Err(CasError::BadFormat(_))
    ));

    // Case B: corrupt `\n`-terminated line immediately before an unterminated torn tail.
    let tail_scratch = Scratch::new("journal-corrupt-before-torn");
    let tail_store = Store::open(tail_scratch.join("cas")).expect("open");
    tail_store.append_ref_journal(&e1).expect("append e1");
    let mut tail_raw = fs::OpenOptions::new()
        .append(true)
        .open(tail_store.refs_journal_path())
        .expect("open tail");
    tail_raw
        .write_all(b"{\"seq\":2,\"corrupt\":\n{\"seq\":3,\"half")
        .expect("write corrupt line plus torn tail");
    drop(tail_raw);

    tail_store
        .append_ref_journal(&e3)
        .expect("append trims torn tail only, not corrupt committed line");
    assert!(matches!(
        tail_store.read_ref_journal(),
        Err(CasError::BadFormat(
            "malformed committed JSON line in refs.journal"
        ))
    ));
    assert!(matches!(
        tail_store.latest_seq(),
        Err(CasError::BadFormat(
            "malformed committed JSON line in refs.journal"
        ))
    ));
    assert!(matches!(
        tail_store.reconcile_entries_after(0),
        Err(CasError::BadFormat(
            "malformed committed JSON line in refs.journal"
        ))
    ));
}

#[test]
fn journal_survives_consecutive_torn_tail_crashes_and_restarts_strictly_monotonic() {
    let scratch = Scratch::new("journal-consecutive-crashes");
    let cas_root = scratch.join("cas");

    let s1 = sha256_of(b"cycle-s1");
    let s2 = sha256_of(b"cycle-s2");
    let s3 = sha256_of(b"cycle-s3");
    let s4 = sha256_of(b"cycle-s4");

    let e1 = journal_entry(1, "main", None, s1.clone(), "aa", 1000);
    let e2 = journal_entry(2, "main", Some(s1), s2.clone(), "bb", 2000);
    let e3 = journal_entry(3, "main", Some(s2), s3.clone(), "cc", 3000);
    let e4 = journal_entry(4, "main", Some(s3), s4, "dd", 4000);

    // Boot 1 -> commit seq 1 -> crash with torn tail #1.
    let boot1 = Store::open(&cas_root).expect("boot 1");
    boot1.append_ref_journal(&e1).expect("append e1");
    fs::OpenOptions::new()
        .append(true)
        .open(boot1.refs_journal_path())
        .expect("open crash 1")
        .write_all(b"{\"seq\":2,\"partial")
        .expect("torn tail 1");
    drop(boot1);

    // Boot 2 -> reopen -> heal & append seq 2 -> crash with large torn tail #2 (>8 KiB).
    let boot2 = Store::open(&cas_root).expect("boot 2");
    assert_eq!(
        boot2.read_ref_journal().expect("read boot 2"),
        vec![e1.clone()]
    );
    boot2.append_ref_journal(&e2).expect("append e2");
    assert_eq!(boot2.latest_seq().expect("seq after boot 2"), 2);
    let mut crash2 = fs::OpenOptions::new()
        .append(true)
        .open(boot2.refs_journal_path())
        .expect("open crash 2");
    crash2.write_all(&vec![b'{'; 10_000]).expect("torn tail 2");
    drop(crash2);
    drop(boot2);

    // Boot 3 -> reopen -> heal & append seq 3 and seq 4 -> full fast-forward reconcile succeeds.
    let boot3 = Store::open(&cas_root).expect("boot 3");
    assert_eq!(
        boot3.read_ref_journal().expect("read boot 3"),
        vec![e1.clone(), e2.clone()]
    );
    boot3.append_ref_journal(&e3).expect("append e3");
    boot3.append_ref_journal(&e4).expect("append e4");

    let expected = vec![e1, e2, e3, e4];
    assert_eq!(boot3.read_ref_journal().expect("all 4 intact"), expected);
    assert_eq!(boot3.latest_seq().expect("latest_seq 4"), 4);
    assert_eq!(
        boot3.reconcile_entries_after(0).expect("reconcile all 4"),
        expected
    );
}
