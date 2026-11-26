//! spawn-cas on disk: the layout shared with `spawn-node`'s Go `cas`
//! package (its `cas_test.go`, case for case), verification, pins, the LRU
//! GC and the adoption of `cas_dedup`'s legacy shards.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use spawn_cas::{
    Algo, CAS_MARKER, CasError, Digest, NS_ARTIFACTS, NS_BACKUP, NS_BASE_CACHE, NS_DATA, Quotas,
    Store, TMP_MAX_AGE, default_quotas, is_valid_name, sha256_of,
};

/// A fresh directory under the temporary directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "spawn-cas-test-{tag}-{}-{}",
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

/// A clock tests move by hand.
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

fn hex_of(byte: &str) -> String {
    byte.repeat(32)
}

#[test]
fn digests_parse_only_prefixed_lowercase_hex() {
    let hex = hex_of("ab");
    let good = format!("sha256:{hex}");
    let digest = Digest::parse(&good).expect(&good);
    assert_eq!(digest.to_string(), good);
    assert_eq!(good.parse::<Digest>().expect("from_str"), digest);
    for bad in [
        hex.clone(),
        format!("sha256:{}", hex.to_uppercase()),
        format!("blake3:{hex}"),
        format!("md5:{hex}"),
        format!("SHA256:{hex}"),
        format!("sha256:{}", &hex[..63]),
        format!("sha256:{hex}0"),
        "sha256:".to_string(),
        format!(":{hex}"),
        format!("sha256:{}zz", &hex[..62]),
        format!("sha256: {}", &hex[..63]),
    ] {
        assert!(
            matches!(Digest::parse(&bad), Err(CasError::BadDigest { .. })),
            "{bad:?} accepted"
        );
    }
    assert_eq!(
        sha256_of(b"abc").to_string(),
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn put_lays_out_objects_and_verifies_them() {
    let scratch = Scratch::new("layout");
    let store = Store::open(scratch.join("cas")).expect("open");
    let digest = store.put_bytes(NS_ARTIFACTS, b"hello").expect("put");
    let path = store.object_path(&digest);
    assert_eq!(
        path,
        scratch
            .join("cas")
            .join("objects/sha256")
            .join(&digest.hex()[..2])
            .join(digest.hex())
    );
    let meta = fs::metadata(&path).expect("object");
    assert_eq!(meta.permissions().mode() & 0o777, 0o444);
    assert!(scratch.join("cas").join(CAS_MARKER).is_file());
    for directory in ["objects", "chunk-index", "pins", "access", "tmp"] {
        assert!(scratch.join("cas").join(directory).is_dir(), "{directory}");
    }
    assert!(
        store
            .access_path(NS_ARTIFACTS, &digest)
            .ends_with(format!("access/artifacts/sha256/{}", digest.hex()))
    );
    assert!(store.access_path(NS_ARTIFACTS, &digest).is_file());
    store.verify(&digest).expect("verify");
    assert_eq!(store.read(&digest).expect("read"), b"hello");

    // Bytes that do not hash to the address are refused and removed.
    let other = sha256_of(b"other");
    let refused = store.put(NS_ARTIFACTS, &mut &b"not other"[..], &other);
    assert!(refused.is_err_and(|error| error.is_mismatch()));
    assert!(!store.has(&other), "a mismatching object was stored");
    assert_eq!(fs::read_dir(store.tmp_dir()).expect("tmp").count(), 0);

    // Damage on disk is caught on read; read drops the damaged object.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
    fs::write(&path, "HELLO").expect("damage");
    assert!(
        store
            .verify(&digest)
            .is_err_and(|error| error.is_mismatch())
    );
    assert!(store.has(&digest), "verify only reports");
    assert!(store.read(&digest).is_err_and(|error| error.is_mismatch()));
    assert!(!store.has(&digest), "read removed the damaged object");
    assert!(store.read(&digest).is_err_and(|error| error.is_not_found()));
}

#[test]
fn commit_verifies_and_removes_a_mismatching_temporary() {
    let scratch = Scratch::new("commit");
    let store = Store::open(scratch.join("cas")).expect("open");
    let (mut file, staged) = store.temp_file("fetch").expect("temp");
    assert!(staged.starts_with(store.tmp_dir()));
    file.write_all(b"payload").expect("write");
    drop(file);
    let wrong = sha256_of(b"something else");
    assert!(
        store
            .commit(NS_ARTIFACTS, &staged, &wrong)
            .is_err_and(|error| error.is_mismatch())
    );
    assert!(!staged.exists(), "the mismatching temporary was removed");

    let (mut file, staged) = store.temp_file("fetch").expect("temp");
    file.write_all(b"payload").expect("write");
    drop(file);
    let right = sha256_of(b"payload");
    let placed = store.commit(NS_BACKUP, &staged, &right).expect("commit");
    assert_eq!(placed, store.object_path(&right));
    assert!(!staged.exists());
    assert!(store.access_path(NS_BACKUP, &right).is_file());
    // A namespace outside the name rule is refused before anything moves.
    assert!(matches!(
        store.touch("../escape", &right),
        Err(CasError::BadName { .. })
    ));
    assert!(
        store
            .partial_path(&right)
            .ends_with(format!("tmp/sha256-{}.part", right.hex()))
    );
}

#[test]
fn symlinks_and_special_files_are_never_objects() {
    let scratch = Scratch::new("symlink");
    let store = Store::open(scratch.join("cas")).expect("open");
    let digest = sha256_of(b"outside");
    let outside = scratch.join("outside");
    fs::write(&outside, "outside").expect("write");
    let path = store.object_path(&digest);
    fs::create_dir_all(path.parent().expect("shard")).expect("mkdir");
    std::os::unix::fs::symlink(&outside, &path).expect("symlink");
    assert!(!store.has(&digest));
    assert!(matches!(
        store.open_object(&digest),
        Err(CasError::NotPlain(_))
    ));
    assert!(matches!(store.read(&digest), Err(CasError::NotPlain(_))));
    assert!(store.objects(Algo::Sha256).expect("list").is_empty());
    // A symlink staged for a commit is refused (and only the link removed).
    let staged = store.tmp_dir().join("link");
    std::os::unix::fs::symlink(&outside, &staged).expect("symlink");
    assert!(store.commit(NS_ARTIFACTS, &staged, &digest).is_err());
    assert!(fs::symlink_metadata(&staged).is_err());
    assert_eq!(fs::read(&outside).expect("outside"), b"outside");
}

#[test]
fn a_foreign_directory_is_never_a_store() {
    let scratch = Scratch::new("foreign");
    let root = scratch.join("project");
    fs::create_dir_all(&root).expect("mkdir");
    fs::write(root.join("notes.txt"), "mine").expect("write");
    assert!(matches!(Store::open(&root), Err(CasError::NotCas(_))));
    assert!(!root.join(CAS_MARKER).exists());
    assert!(!root.join("objects").exists());
    // An empty directory is claimed, a missing one made.
    fs::create_dir_all(scratch.join("empty")).expect("mkdir");
    Store::open(scratch.join("empty")).expect("empty");
    Store::open(scratch.join("missing/deeper")).expect("missing");
    assert!(scratch.join("missing/deeper").join(CAS_MARKER).is_file());
}

#[test]
fn pins_are_whole_files_of_prefixed_digests_with_checked_names() {
    let scratch = Scratch::new("pins");
    let store = Store::open(scratch.join("cas")).expect("open");
    let one = store.put_bytes(NS_ARTIFACTS, b"one").expect("put");
    let two = store.put_bytes(NS_BASE_CACHE, b"two").expect("put");
    store
        .pin("lease", "gate-aaaaaaaa.1", &[one.clone(), two.clone()])
        .expect("pin");
    let text = fs::read_to_string(scratch.join("cas/pins/lease/gate-aaaaaaaa.1")).expect("pin");
    assert_eq!(text, format!("{one}\n{two}\n"));
    assert_eq!(
        store.pin_names("lease").expect("names"),
        ["gate-aaaaaaaa.1"]
    );
    assert_eq!(
        store
            .pinned()
            .expect("pinned")
            .into_iter()
            .collect::<Vec<_>>(),
        {
            let mut both = vec![one.clone(), two.clone()];
            both.sort();
            both
        }
    );
    // No temporary is left beside it.
    let names: Vec<_> = fs::read_dir(scratch.join("cas/pins/lease"))
        .expect("dir")
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
    for bad in [
        "",
        ".hidden",
        "-dash",
        "a/b",
        "..",
        "sp ace",
        &"a".repeat(129),
    ] {
        assert!(!is_valid_name(bad), "{bad:?}");
        assert!(matches!(
            store.pin(bad, "x", std::slice::from_ref(&one)),
            Err(CasError::BadName { .. })
        ));
        assert!(matches!(
            store.pin("x", bad, std::slice::from_ref(&one)),
            Err(CasError::BadName { .. })
        ));
    }
    for good in ["a", "_x", "gate-1.2:3_4", &"a".repeat(128)] {
        assert!(is_valid_name(good), "{good:?}");
    }
    store.unpin("lease", "gate-aaaaaaaa.1").expect("unpin");
    store
        .unpin("lease", "gate-aaaaaaaa.1")
        .expect("unpin twice");
    assert!(store.pinned().expect("pinned").is_empty());
}

/// Go `TestLRUGCKeepsPinned`: pinned objects survive LRU eviction; unpinned
/// old objects go first; a namespace under its quota is left alone.
#[test]
fn lru_gc_keeps_pinned_objects() {
    let scratch = Scratch::new("lru");
    let clock = ManualClock::new();
    let store = clock.store(&scratch.join("cas"));
    let mut digests = Vec::new();
    for at in 0..6_u8 {
        digests.push(
            store
                .put_bytes(NS_ARTIFACTS, &[b'a' + at; 100])
                .expect("put"),
        );
        clock.advance(Duration::from_mins(1));
    }
    let base = store.put_bytes(NS_BASE_CACHE, &[b'z'; 1000]).expect("put");
    // The two oldest are pinned (a live lease, a warm base).
    store
        .pin(
            "lease",
            "gate-aaaaaaaa.1",
            std::slice::from_ref(&digests[0]),
        )
        .expect("pin");
    store
        .pin("warm", &"a".repeat(40), std::slice::from_ref(&digests[1]))
        .expect("pin");
    // Using digests[2] again makes it recent.
    clock.advance(Duration::from_mins(1));
    store.touch(NS_ARTIFACTS, &digests[2]).expect("touch");
    let quotas = Quotas::from([
        (NS_ARTIFACTS.to_string(), 300),
        (NS_BASE_CACHE.to_string(), 5000),
    ]);
    let report = store.gc(&quotas).expect("gc");
    // 600 bytes, quota 300: oldest unpinned first: 3, 4; then 400 left
    // (0, 1 pinned; 2, 5): 5 is older than 2's new use.
    for (at, want) in [true, true, true, false, false, false]
        .into_iter()
        .enumerate()
    {
        assert_eq!(store.has(&digests[at]), want, "digests[{at}]");
        // Object first, record second: neither is left for an evicted one.
        assert_eq!(store.access_path(NS_ARTIFACTS, &digests[at]).exists(), want);
    }
    assert!(store.has(&base));
    assert_eq!(report.namespace_use[NS_ARTIFACTS], 300, "{report:?}");
    assert_eq!(report.namespace_use[NS_BASE_CACHE], 1000, "{report:?}");
    assert!(report.kept_pinned >= 1, "{report:?}");
    assert_eq!(
        report.evicted,
        [digests[3].clone(), digests[4].clone(), digests[5].clone()]
    );
    assert_eq!(report.freed_bytes, 300);
    // Unpinning makes them collectable, in LRU order.
    store.unpin("lease", "gate-aaaaaaaa.1").expect("unpin");
    store.unpin("warm", &"a".repeat(40)).expect("unpin");
    store
        .gc(&Quotas::from([(NS_ARTIFACTS.to_string(), 100)]))
        .expect("gc");
    assert!(!store.has(&digests[0]) && !store.has(&digests[1]) && store.has(&digests[2]));
}

#[test]
fn namespaces_without_a_quota_are_never_evicted_and_quotas_are_configurable() {
    let scratch = Scratch::new("quotas");
    let store = Store::open(scratch.join("cas")).expect("open");
    let quotas = default_quotas();
    assert_eq!(quotas[NS_ARTIFACTS], 20 << 30);
    assert_eq!(quotas[NS_BASE_CACHE], 50 << 30);
    assert!(!quotas.contains_key(NS_BACKUP) && !quotas.contains_key(NS_DATA));
    let backup = store.put_bytes(NS_BACKUP, &[1; 64]).expect("put");
    let data = store.put_bytes(NS_DATA, &[2; 64]).expect("put");
    let mut tight = Quotas::from([
        (NS_ARTIFACTS.to_string(), 0),
        (NS_BASE_CACHE.to_string(), 0),
    ]);
    let report = store.gc(&tight).expect("gc");
    assert!(store.has(&backup) && store.has(&data), "{report:?}");
    assert_eq!(report.namespace_use[NS_BACKUP], 64);
    tight.insert(NS_DATA.to_string(), 0);
    store.gc(&tight).expect("gc");
    assert!(store.has(&backup) && !store.has(&data));
}

/// Go `TestGCCrashOnly`: an access record whose object is gone, and a torn
/// pin file, leave a store the next pass reads correctly.
#[test]
fn gc_is_crash_only() {
    let scratch = Scratch::new("crash");
    let store = Store::open(scratch.join("cas")).expect("open");
    let digest = store.put_bytes(NS_ARTIFACTS, b"x").expect("put");
    fs::remove_file(store.object_path(&digest)).expect("remove"); // a crash between the two
    fs::write(scratch.join("cas/pins/torn"), "sha256:abc\nnot a digest").expect("torn");
    store.gc(&default_quotas()).expect("gc");
    assert!(
        !store.access_path(NS_ARTIFACTS, &digest).exists(),
        "dangling access record kept"
    );
    // The whole store removed mid-life: the next open needs no repair.
    fs::remove_dir_all(scratch.join("cas/objects")).expect("rm");
    fs::remove_dir_all(scratch.join("cas/access")).expect("rm");
    let store = Store::open(scratch.join("cas")).expect("reopen");
    store.put_bytes(NS_ARTIFACTS, b"again").expect("put");
}

#[test]
fn gc_purges_stale_temporaries_by_the_store_clock() {
    let scratch = Scratch::new("tmp");
    let clock = ManualClock::new();
    // The files' mtimes are real; the store's clock starts at the present.
    *clock.0.lock().expect("clock") = SystemTime::now();
    let store = clock.store(&scratch.join("cas"));
    let (_, fresh) = store.temp_file("put").expect("temp");
    fs::create_dir_all(store.tmp_dir().join("unpacked/inner")).expect("mkdir");
    let digest = sha256_of(b"held");
    store
        .pin("owner", "name", std::slice::from_ref(&digest))
        .expect("pin");
    fs::write(scratch.join("cas/pins/owner/.pin-1-1"), "torn").expect("torn temp");
    assert_eq!(store.gc(&default_quotas()).expect("gc").stale_tmp, 0);
    assert!(fresh.exists());
    clock.advance(TMP_MAX_AGE + Duration::from_mins(1));
    let report = store.gc(&default_quotas()).expect("gc");
    assert_eq!(report.stale_tmp, 3, "{report:?}");
    assert_eq!(fs::read_dir(store.tmp_dir()).expect("tmp").count(), 0);
    assert!(!scratch.join("cas/pins/owner/.pin-1-1").exists());
    assert!(
        scratch.join("cas/pins/owner/name").exists(),
        "a pin is no temporary"
    );
}

/// The adoption of `cas_dedup`'s `<cas>/<2 hex>/<64 hex>`: renamed, never
/// rehashed (a blob whose bytes do not match its name is adopted as it
/// is), and nothing foreign is touched.
#[test]
fn legacy_shards_are_adopted_in_place_without_rehashing() {
    let scratch = Scratch::new("legacy");
    let root = scratch.join("cas");
    let real = sha256_of(b"legacy blob");
    let unhashed = format!("ab{}", "0".repeat(62)); // its bytes are not its digest
    fs::create_dir_all(root.join(&real.hex()[..2])).expect("mkdir");
    fs::create_dir_all(root.join("ab")).expect("mkdir");
    fs::create_dir_all(root.join("cd")).expect("mkdir");
    fs::create_dir_all(root.join("tmp")).expect("mkdir");
    fs::write(root.join(CAS_MARKER), "spawn content-addressed store 1\n").expect("marker");
    let legacy_real = root.join(&real.hex()[..2]).join(real.hex());
    fs::write(&legacy_real, "legacy blob").expect("write");
    fs::set_permissions(&legacy_real, fs::Permissions::from_mode(0o644)).expect("chmod");
    fs::write(root.join("ab").join(&unhashed), "not its digest").expect("write");
    let foreign = [
        root.join("ab/session.jsonl"),
        root.join("ab").join(format!("{unhashed}.jsonl")),
        root.join("cd").join(&unhashed),
        root.join("cd.jsonl"),
        root.join("index"),
        root.join("tmp/notes.txt"),
    ];
    for path in &foreign {
        fs::write(path, "foreign").expect("write");
    }
    std::os::unix::fs::symlink(
        &legacy_real,
        root.join("ab").join(format!("ab{}", "1".repeat(62))),
    )
    .expect("symlink");

    let store = Store::open(&root).expect("open");
    let report = store.adopt_legacy_shards().expect("adopt");
    assert_eq!(report.adopted, 2, "{report:?}");
    assert!(!legacy_real.exists());
    let adopted = store.object_path(&real);
    assert_eq!(fs::read(&adopted).expect("adopted"), b"legacy blob");
    assert_eq!(
        fs::metadata(&adopted).expect("meta").permissions().mode() & 0o777,
        0o444
    );
    let unhashed = Digest::from_parts(Algo::Sha256, &unhashed).expect("digest");
    assert_eq!(
        fs::read(store.object_path(&unhashed)).expect("adopted"),
        b"not its digest",
        "adopted by name, not rehashed"
    );
    assert!(
        store
            .verify(&unhashed)
            .is_err_and(|error| error.is_mismatch())
    );
    for path in &foreign {
        assert_eq!(
            fs::read(path).expect("kept"),
            b"foreign",
            "{}",
            path.display()
        );
    }
    assert!(
        fs::symlink_metadata(root.join("ab").join(format!("ab{}", "1".repeat(62))))
            .expect("link")
            .is_symlink()
    );
    assert!(
        !root.join(&real.hex()[..2]).exists(),
        "an emptied shard goes"
    );
    // Once is enough: a second run finds nothing to do.
    assert_eq!(store.adopt_legacy_shards().expect("again").adopted, 0);

    // A legacy duplicate of an existing object: its name goes, the object
    // and its emptied shard are as after a clean adoption.
    fs::create_dir_all(root.join(&real.hex()[..2])).expect("mkdir");
    fs::write(&legacy_real, "legacy blob").expect("write");
    let report = store.adopt_legacy_shards().expect("again");
    assert_eq!(report.duplicates, std::slice::from_ref(&legacy_real));
    assert_eq!(report.shards_removed, 1, "{report:?}");
    assert!(!legacy_real.exists());
    assert!(!root.join(&real.hex()[..2]).exists());
    assert_eq!(fs::read(&adopted).expect("object"), b"legacy blob");
}

/// A crash between the link and the unlink of an adoption leaves the same
/// inode at the legacy name and at the object: the next adoption removes
/// the legacy name and keeps the object.
#[test]
fn a_crash_mid_adoption_is_finished_by_the_next_one() {
    let scratch = Scratch::new("legacy-crash");
    let root = scratch.join("cas");
    let store = Store::open(&root).expect("open");
    let digest = sha256_of(b"half adopted");
    let object = store.object_path(&digest);
    fs::create_dir_all(object.parent().expect("shard")).expect("mkdir");
    fs::write(&object, "half adopted").expect("write");
    let legacy = root.join(digest.shard()).join(digest.hex());
    fs::create_dir_all(legacy.parent().expect("shard")).expect("mkdir");
    fs::hard_link(&object, &legacy).expect("link");
    let report = store.adopt_legacy_shards().expect("adopt");
    assert_eq!(report.adopted, 0, "{report:?}");
    assert_eq!(report.duplicates, std::slice::from_ref(&legacy));
    assert_eq!(report.shards_removed, 1, "{report:?}");
    assert!(!legacy.exists());
    assert!(!root.join(digest.shard()).exists());
    assert_eq!(fs::read(&object).expect("object"), b"half adopted");
    store.verify(&digest).expect("verify");
}

/// Namespace membership: the namespaces holding an access record of an
/// object, and removing the object file alone leaves every record.
#[test]
fn namespaces_of_lists_access_records_and_remove_object_file_keeps_them() {
    let scratch = Scratch::new("namespaces");
    let store = Store::open(scratch.join("cas")).expect("open");
    let digest = store.put_bytes(NS_DATA, b"snapshot").expect("put");
    store.touch(NS_BACKUP, &digest).expect("touch");
    assert_eq!(
        store.namespaces_of(&digest).expect("namespaces"),
        [NS_BACKUP, NS_DATA]
    );
    let unknown = sha256_of(b"nobody");
    assert!(store.namespaces_of(&unknown).expect("none").is_empty());
    assert!(store.remove_object_file(&digest).expect("remove"));
    assert!(!store.has(&digest));
    assert!(store.access_path(NS_DATA, &digest).exists());
    assert!(store.access_path(NS_BACKUP, &digest).exists());
}

#[test]
fn remove_object_and_listing_take_only_well_formed_objects() {
    let scratch = Scratch::new("list");
    let store = Store::open(scratch.join("cas")).expect("open");
    let digest = store.put_bytes(NS_ARTIFACTS, b"listed").expect("put");
    store.touch(NS_BASE_CACHE, &digest).expect("touch");
    let shard = scratch.join("cas/objects/sha256").join(&digest.hex()[..2]);
    fs::write(shard.join("notes.jsonl"), "x").expect("write");
    fs::create_dir_all(scratch.join("cas/objects/sha256/zz")).expect("mkdir");
    let listed = store.objects(Algo::Sha256).expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].digest, digest);
    assert!(store.remove_object(&digest).expect("remove"));
    assert!(!store.remove_object(&digest).expect("remove twice"));
    assert!(!store.access_path(NS_ARTIFACTS, &digest).exists());
    assert!(!store.access_path(NS_BASE_CACHE, &digest).exists());
    assert!(shard.join("notes.jsonl").exists());
}
