//! manifest-v1 and the data snapshot pins (PV3b, docs/provenance.md): the
//! format is the `tree_sha256_v1` line list (the shared `tree/` fixture of
//! spawn-node v1 hashes to its golden digest), symbolic links are never
//! followed and one leaving the root is refused, the pin keeps the three
//! newest snapshots of a ref, and the store stays readable after a write is
//! interrupted or the writer is killed (SIGKILL) at any instant.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use spawn_cas::{
    CasError, Digest, KEEP_SNAPSHOTS, Manifest, ManifestEntry, NS_ARTIFACTS, NS_DATA,
    PROV_PIN_OWNER, Store, sha256_of,
};

/// The golden `tree_sha256_v1` of spawn-node v1's `tree/` fixture.
const TREE_FIXTURE_SHA256: &str =
    "191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707";

/// A fresh directory under the temporary directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "spawn-cas-manifest-{tag}-{}-{}",
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

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    fs::create_dir_all(file.parent().expect("a parent")).expect("its directory");
    fs::write(file, text).expect("write");
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../spawn-node-proto/testdata/node-protocol-v1/tree")
}

#[test]
fn the_shared_tree_fixture_hashes_to_its_golden_digest() {
    let manifest = Manifest::build(&fixture()).expect("the fixture");
    assert_eq!(
        manifest.digest().to_string(),
        format!("sha256:{TREE_FIXTURE_SHA256}")
    );
    let paths: Vec<String> = manifest
        .entries()
        .iter()
        .map(|entry| String::from_utf8(entry.path.clone()).expect("UTF-8"))
        .collect();
    assert_eq!(paths, ["build", "check-tests.sh", "sub/001.sh"]);
    // The bytes are the identity: parsed back, the same manifest (sizes
    // are not part of it).
    let parsed = Manifest::parse(&manifest.text()).expect("parse");
    assert_eq!(parsed.text(), manifest.text());
    assert_eq!(parsed.digest(), manifest.digest());
    assert!(parsed.entries().iter().all(|entry| entry.size.is_none()));
    assert!(manifest.entries().iter().all(|entry| entry.size.is_some()));
    // A file's digest is its bytes' SHA-256.
    let build = fs::read(fixture().join("build")).expect("build");
    assert_eq!(manifest.entries()[0].digest, sha256_of(&build));
}

#[test]
fn manifests_are_sorted_bytewise_and_anything_else_is_refused() {
    let scratch = Scratch::new("sorted");
    let root = scratch.join("data");
    // Bytewise: "B" < "a" < "a.txt" < "a/b" ('.' 0x2e < '/' 0x2f).
    write(&root, "a/b", "1");
    write(&root, "a.txt", "2");
    write(&root, "B", "3");
    let manifest = Manifest::build(&root).expect("build");
    let paths: Vec<&[u8]> = manifest
        .entries()
        .iter()
        .map(|entry| entry.path.as_slice())
        .collect();
    assert_eq!(paths, [&b"B"[..], b"a.txt", b"a/b"]);
    assert_eq!(manifest.bytes(), 3);
    assert_eq!(manifest.subtree(b"a").len(), 1);
    assert_eq!(manifest.subtree(b"a").entries()[0].path, b"b");
    // "a.txt" sorts between "a" and "a/": not under a/.
    assert_eq!(manifest.subtree(b"a.txt").len(), 0);
    assert_eq!(manifest.subtree(b"B").len(), 0);

    let hex = "ab".repeat(32);
    let refused = [
        format!("{hex}  b\n{hex}  a\n"),
        format!("{hex}  a\n{hex}  a\n"),
        format!("{hex}  a"),
        format!("sha256:{hex}  a\n"),
        format!("{}  a\n", "AB".repeat(32)),
        format!("{hex} a\n"),
        format!("{hex}  ../a\n"),
        format!("{hex}  /a\n"),
        format!("{hex}  a//b\n"),
    ];
    for text in refused {
        assert!(
            matches!(
                Manifest::parse(text.as_bytes()),
                Err(CasError::BadManifest { .. })
            ),
            "{text:?} parsed"
        );
    }
    assert!(Manifest::parse(b"").expect("empty").is_empty());
    assert_eq!(
        Manifest::default().digest(),
        sha256_of(b""),
        "the empty snapshot is the SHA-256 of nothing"
    );
    // Unprefixed digests are refused everywhere.
    assert!(Digest::parse(&hex).is_err());
    let twice = ManifestEntry {
        path: b"a".to_vec(),
        digest: sha256_of(b"a"),
        size: None,
    };
    assert!(Manifest::from_entries(vec![twice.clone(), twice]).is_err());
}

#[test]
fn symlinks_are_never_followed_and_one_leaving_the_root_is_refused() {
    let scratch = Scratch::new("links");
    let root = scratch.join("data");
    write(&root, "kept.txt", "kept");
    write(&scratch.0, "outside/secret.txt", "secret");
    symlink("kept.txt", root.join("inside")).expect("an inner link");
    fs::create_dir_all(root.join("sub")).expect("sub");
    symlink("../kept.txt", root.join("sub/up")).expect("an inner link up");
    let manifest = Manifest::build(&root).expect("inner links are left out");
    assert_eq!(manifest.len(), 1, "{manifest:?}");

    for (name, target) in [
        ("abs", scratch.join("outside").display().to_string()),
        ("rel", "../outside/secret.txt".to_string()),
        ("deep", "sub/../../outside".to_string()),
    ] {
        let link = root.join(name);
        symlink(&target, &link).expect("an escaping link");
        match Manifest::build(&root) {
            Err(CasError::SymlinkEscape { link: found, .. }) => {
                assert_eq!(found, PathBuf::from(name));
            }
            other => panic!("{name} -> {target}: {other:?}"),
        }
        fs::remove_file(&link).expect("unlink");
    }
    // Through an inner link to the root itself, `..` leaves it: the text
    // says inside, the kernel says outside.
    symlink(".", root.join("here")).expect("a link to the root");
    symlink("here/../outside/secret.txt", root.join("chained")).expect("chained");
    assert!(matches!(
        Manifest::build(&root),
        Err(CasError::SymlinkEscape { .. })
    ));
    fs::remove_file(root.join("chained")).expect("unlink");
    fs::remove_file(root.join("here")).expect("unlink");
    // A dangling link that would land outside is refused too.
    symlink("../../nowhere", root.join("sub/dangling")).expect("dangling");
    assert!(matches!(
        Manifest::build(&root),
        Err(CasError::SymlinkEscape { .. })
    ));
}

#[test]
fn a_snapshot_is_stored_verified_and_digest_only_stores_its_manifest_alone() {
    let scratch = Scratch::new("put");
    let store = Store::open(scratch.join("cas")).expect("open");
    let root = scratch.join("data");
    write(&root, "x/1", "one");
    write(&root, "x/2", "one");
    write(&root, "y", "two");
    let manifest = Manifest::build(&root).expect("build");
    let snapshot = store.put_snapshot(&root, &manifest, false).expect("put");
    assert_eq!(snapshot, manifest.digest());
    assert_eq!(store.verify_snapshot(&snapshot).expect("verify"), {
        Manifest::parse(&manifest.text()).expect("parsed")
    });
    assert!(
        store
            .snapshot_missing(&snapshot)
            .expect("missing")
            .is_empty()
    );
    assert_eq!(store.namespaces_of(&snapshot).expect("ns"), [NS_DATA]);

    write(&root, "z", "three");
    let second = Manifest::build(&root).expect("build");
    let digest_only = store.put_snapshot(&root, &second, true).expect("put");
    assert!(store.has(&digest_only));
    assert_eq!(
        store.snapshot_missing(&digest_only).expect("missing"),
        [sha256_of(b"three")]
    );

    // A file changed between the manifest and the store is refused.
    let third = Manifest::build(&root).expect("build");
    write(&root, "z", "changed");
    assert!(
        store
            .put_snapshot(&root, &third, false)
            .is_err_and(|error| error.is_mismatch())
    );
}

#[test]
fn the_pin_keeps_the_three_newest_snapshots_and_drops_the_fourth() {
    let scratch = Scratch::new("keep");
    let store = Store::open(scratch.join("cas")).expect("open");
    let root = scratch.join("data");
    write(&root, "shared", "in every snapshot");
    // An object another namespace also records is never removed.
    let artifact = store.put_bytes(NS_ARTIFACTS, b"v1").expect("artifact");
    let mut snapshots: Vec<Digest> = Vec::new();
    for version in 1..=4 {
        write(&root, "version", &format!("v{version}"));
        write(&root, "unique", &format!("only in {version}"));
        let manifest = Manifest::build(&root).expect("build");
        let snapshot = store.put_snapshot(&root, &manifest, false).expect("put");
        snapshots.insert(0, snapshot);
        let report = store.keep_snapshots("clean", &snapshots).expect("keep");
        assert_eq!(report.kept.len(), version.min(KEEP_SNAPSHOTS));
    }
    let kept = store.kept_snapshots("clean").expect("kept");
    assert_eq!(kept, snapshots[..KEEP_SNAPSHOTS]);
    let dropped = &snapshots[KEEP_SNAPSHOTS];
    // The fourth newest loses its file bytes, never its manifest (D3).
    assert!(store.has(dropped), "the fourth snapshot's manifest stays");
    store
        .snapshot_manifest(dropped)
        .expect("the fourth snapshot's manifest reads");
    assert_eq!(
        store.snapshot_missing(dropped).expect("missing"),
        [sha256_of(b"only in 1")],
        "its own bytes are gone; v1 (an artifact too) stays"
    );
    assert!(store.has(&artifact), "v1 is an artifact too");
    assert!(store.has(&sha256_of(b"in every snapshot")));
    for snapshot in &kept {
        assert!(
            store
                .snapshot_missing(snapshot)
                .expect("missing")
                .is_empty()
        );
    }
    let pinned = store.pinned().expect("pinned");
    assert!(pinned.contains(&kept[0]) && pinned.contains(dropped));
    let mut every = snapshots.clone();
    every.sort();
    assert_eq!(store.kept_manifests("clean").expect("manifests"), every);
    // A data quota of 0 evicts every unpinned data object: the manifests
    // all stay, and so do the kept snapshots' bytes.
    let quotas = spawn_cas::Quotas::from([(NS_DATA.to_string(), 0)]);
    store.gc(&quotas).expect("gc");
    for snapshot in &snapshots {
        store.snapshot_manifest(snapshot).expect("pinned forever");
    }
    for snapshot in &kept {
        store.verify_snapshot(snapshot).expect("kept whole");
    }
    let names = store.pin_names(PROV_PIN_OWNER).expect("names");
    assert_eq!(names, ["clean"]);
    // Keeping the same list again changes nothing.
    let again = store.keep_snapshots("clean", &snapshots).expect("keep");
    assert!(again.dropped.is_empty() && again.removed == 0, "{again:?}");
}

#[test]
fn an_interrupted_write_leaves_a_store_that_reads_correctly_with_no_repair() {
    let scratch = Scratch::new("torn");
    let cas = scratch.join("cas");
    let root = scratch.join("data");
    write(&root, "a", "alpha");
    write(&root, "b", "beta");
    let manifest = Manifest::build(&root).expect("build");
    {
        let store = Store::open(&cas).expect("open");
        let snapshot = store.put_snapshot(&root, &manifest, false).expect("put");
        store.keep_snapshots("r", &[snapshot]).expect("keep");
    }
    // What a writer killed midway leaves: half an object in tmp/, a pin
    // temporary half written, and an object whose rename never happened.
    fs::write(cas.join("tmp/put-999-0"), b"alph").expect("a torn temporary");
    fs::write(cas.join("pins/prov/.pin-999-0"), b"# snapshot sha256:ab").expect("torn pin");
    let store = Store::open(&cas).expect("open again, no repair step");
    let kept = store.kept_snapshots("r").expect("the pin reads whole");
    assert_eq!(kept, [manifest.digest()]);
    store
        .verify_snapshot(&kept[0])
        .expect("every byte verifies");
    assert_eq!(store.pin_names(PROV_PIN_OWNER).expect("names"), ["r"]);
    // The next snapshot goes on as if nothing happened.
    write(&root, "c", "gamma");
    let next = Manifest::build(&root).expect("build");
    let snapshot = store.put_snapshot(&root, &next, false).expect("put");
    store
        .keep_snapshots("r", &[snapshot.clone(), manifest.digest()])
        .expect("keep");
    store.verify_snapshot(&snapshot).expect("verifies");
}

/// The child of [`sigkill_at_any_instant_leaves_every_kept_snapshot_whole`]:
/// snapshots its directory again and again, a file changing each time,
/// until it is killed.
fn crash_child(dir: &Path) {
    let store = Store::open(dir.join("cas")).expect("open");
    let root = dir.join("data");
    let mut snapshots: Vec<Digest> = store.kept_snapshots("crash").expect("kept");
    let mut round: u64 = 0;
    loop {
        round += 1;
        let file = format!("shard-{}", round % 20);
        fs::write(root.join(&file), format!("{round} {}", "x".repeat(4096))).expect("write");
        let manifest = Manifest::build(&root).expect("build");
        let snapshot = store.put_snapshot(&root, &manifest, false).expect("put");
        snapshots.insert(0, snapshot);
        snapshots.truncate(8);
        store.keep_snapshots("crash", &snapshots).expect("keep");
    }
}

const CRASH_CHILD: &str = "SPAWN_CAS_CRASH_CHILD";

#[test]
fn sigkill_at_any_instant_leaves_every_kept_snapshot_whole() {
    if let Ok(dir) = std::env::var(CRASH_CHILD) {
        crash_child(Path::new(&dir));
        return;
    }
    let scratch = Scratch::new("sigkill");
    for shard in 0..20 {
        write(&scratch.join("data"), &format!("shard-{shard}"), "start");
    }
    let mut killed = 0;
    for round in 0_u64..16 {
        let mut child = Command::new(std::env::current_exe().expect("the test binary"))
            .args([
                "sigkill_at_any_instant_leaves_every_kept_snapshot_whole",
                "--exact",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(CRASH_CHILD, &scratch.0)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the child");
        // A different instant each round: 20 ms to 170 ms in.
        let deadline = Instant::now() + Duration::from_millis(20 + round * 10);
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().expect("wait") {
                panic!("the child ended by itself: {status}");
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        child.kill().expect("SIGKILL");
        child.wait().expect("reaped");
        killed += 1;

        // No repair step: the store opens and every kept snapshot is whole.
        let store = Store::open(scratch.join("cas")).expect("open after SIGKILL");
        for snapshot in store.kept_snapshots("crash").expect("the pin reads") {
            store
                .verify_snapshot(&snapshot)
                .unwrap_or_else(|error| panic!("round {round}: {snapshot}: {error}"));
        }
        // Every object in place verifies: none is ever torn.
        for object in store.objects(spawn_cas::Algo::Sha256).expect("objects") {
            store
                .verify(&object.digest)
                .unwrap_or_else(|error| panic!("round {round}: {}: {error}", object.digest));
        }
    }
    assert_eq!(killed, 16);
    let store = Store::open(scratch.join("cas")).expect("open");
    assert!(
        !store.kept_snapshots("crash").expect("kept").is_empty(),
        "the child never got as far as one snapshot"
    );
}
