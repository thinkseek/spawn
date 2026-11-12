//! The store layer without a network or gpg: an in-memory bucket that
//! counts every write per key, and a seal that only marks the bytes.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{
    Bucket, Entry, Head, Keys, Listed, Plan, Put, Source, Store, Verified, hmac_sha256, may_write,
    plan, random_hex, ranges, restore, sources, store_r2,
};
use crate::backup::{hex, md5_hex};

#[derive(Default)]
struct Memory {
    objects: RefCell<BTreeMap<String, (Vec<u8>, String)>>,
    writes: RefCell<BTreeMap<String, u32>>,
}

impl Bucket for Memory {
    fn put_new(&self, key: &str, file: &Path, sha256: &str) -> Result<Put, String> {
        let mut objects = self.objects.borrow_mut();
        if objects.contains_key(key) {
            return Ok(Put::Existed);
        }
        let bytes = fs::read(file).map_err(|error| error.to_string())?;
        objects.insert(key.to_string(), (bytes, sha256.to_string()));
        *self.writes.borrow_mut().entry(key.to_string()).or_default() += 1;
        Ok(Put::Written)
    }
    fn head(&self, key: &str) -> Result<Option<Head>, String> {
        Ok(self.objects.borrow().get(key).map(|(bytes, sha256)| Head {
            length: bytes.len() as u64,
            etag: md5_hex(bytes),
            sha256: sha256.clone(),
        }))
    }
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self
            .objects
            .borrow()
            .get(key)
            .map(|(bytes, _)| bytes.clone()))
    }
    fn list(&self, prefix: &str) -> Result<Vec<Listed>, String> {
        Ok(self
            .objects
            .borrow()
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, (bytes, _))| Listed {
                key: key.clone(),
                size: bytes.len() as u64,
                etag: md5_hex(bytes),
            })
            .collect())
    }
}

const MARK: &[u8] = b"sealed:";

fn seal(input: &Path, out: &Path) -> Result<(), String> {
    let mut bytes = MARK.to_vec();
    bytes.extend(fs::read(input).map_err(|error| error.to_string())?);
    fs::write(out, bytes).map_err(|error| error.to_string())
}

fn open(input: &Path, out: &Path) -> Result<(), String> {
    let bytes = fs::read(input).map_err(|error| error.to_string())?;
    let plain = bytes.strip_prefix(MARK).ok_or("not sealed")?;
    fs::write(out, plain).map_err(|error| error.to_string())
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "spawn-backup-store-test-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.subsec_nanos())
    ));
    fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn keys() -> Keys {
    Keys {
        prefix: "locktest/t/".to_string(),
        area: "transcripts".to_string(),
        host: "h".to_string(),
        account: "a".to_string(),
    }
}

fn store<'a>(bucket: &'a Memory, work: &'a Path) -> Store<'a, Memory> {
    Store {
        bucket,
        keys: keys(),
        name_key: [7; 32],
        seal: &seal,
        open: &open,
        scratch: work,
        nonce: &|| random_hex(4),
    }
}

fn listed(dir: &Path) -> Vec<Source> {
    let mut said = Vec::new();
    sources("transcripts", dir, &mut said).expect("sources")
}

fn entry<'a>(
    store: &Store<'_, Memory>,
    path: &str,
    holder: &'a mut Option<super::Manifest>,
) -> &'a Entry {
    *holder = store
        .newest()
        .expect("newest")
        .map(|(_, manifest)| manifest);
    holder
        .as_ref()
        .and_then(|manifest| manifest.files.iter().find(|entry| entry.path == path))
        .unwrap_or_else(|| panic!("{path} in the newest manifest"))
}

fn set_mtime(path: &Path, seconds: u64) {
    fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| {
            file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds))
        })
        .expect("mtime");
}

#[test]
fn hmac_is_rfc_4231s() {
    // RFC 4231 test case 2.
    assert_eq!(
        hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}

#[test]
fn a_file_is_sent_whole_grown_or_not_at_all() {
    let last = Entry {
        path: "a".to_string(),
        size: 10,
        mtime_ns: 5,
        sha256: "x".to_string(),
        segments: Vec::new(),
    };
    let never = |_: &Entry| -> Result<bool, String> { panic!("not asked") };
    assert_eq!(plan(None, 3, 1, never), Ok(Plan::Whole));
    assert_eq!(plan(Some(&last), 10, 5, never), Ok(Plan::Unchanged));
    assert_eq!(
        plan(Some(&last), 12, 6, |_| Ok(true)),
        Ok(Plan::Append { from: 10 })
    );
    assert_eq!(
        plan(Some(&last), 12, 6, |_| Ok(false)),
        Ok(Plan::Whole),
        "its head changed"
    );
    assert_eq!(
        plan(Some(&last), 10, 6, never),
        Ok(Plan::Whole),
        "same size, another time"
    );
    assert_eq!(plan(Some(&last), 4, 6, never), Ok(Plan::Whole), "shorter");
    assert_eq!(ranges(10, 25, 10), vec![(10, 10), (20, 5)]);
    assert_eq!(ranges(0, 0, 10), Vec::<(u64, u64)>::new());
}

/// The whole allow-list (the lead's rule on rev-g F3/F4), every cell of
/// switch state x bucket x (prefix, area) printed and compared with the
/// cells allowed, named here one by one, so a new cell cannot slip in.
/// Made-up names: the real ones are values of the credential.
#[test]
fn it_writes_only_under_locktest_until_switched_on() {
    let switches = [None, Some("0"), Some("1")];
    let buckets = [
        "b-keep",
        "b-work",
        "b-store",
        "b",
        "b-work-keep",
        "b-keep-work",
        "",
    ];
    let cells = [
        ("", "transcripts"),
        ("", "sessions"),
        ("", "artifacts"),
        ("", ""),
        ("locktest/r/", "transcripts"),
        ("locktest/r/", "sessions"),
        ("locktest/r/", "artifacts"),
        ("probe/r/", "transcripts"),
        ("probe/r/", "sessions"),
        ("locktestx/", "transcripts"),
        ("other/", "transcripts"),
        ("artifacts/", "transcripts"),
        ("transcripts/", "sessions"),
    ];
    let mut allowed = Vec::new();
    for enabled in switches {
        for bucket in buckets {
            for (prefix, area) in cells {
                let answer = may_write(prefix, area, bucket, enabled);
                println!(
                    "{:5} {bucket:12} {prefix:13} {area:12} {}",
                    format!("{enabled:?}"),
                    answer.as_ref().map_or("refused", |()| "ALLOWED")
                );
                match answer {
                    Ok(()) => allowed.push((enabled, bucket, prefix, area)),
                    Err(error) => {
                        assert!(error.starts_with("refused"), "{error}");
                        assert!(bucket.len() < 2 || !error.contains(bucket), "{error}");
                    }
                }
            }
        }
    }
    let mut expected = Vec::new();
    for enabled in switches {
        // The work bucket (and a name ending in -work): test objects of the
        // two areas, in every switch state.
        for bucket in ["b-work", "b-keep-work"] {
            for prefix in ["locktest/r/", "probe/r/"] {
                for area in ["transcripts", "sessions"] {
                    expected.push((enabled, bucket, prefix, area));
                }
            }
        }
    }
    // The keep bucket (and a name ending in -keep), switched on: the store's
    // own keys only.
    for bucket in ["b-keep", "b-work-keep"] {
        for area in ["transcripts", "sessions"] {
            expected.push((Some("1"), bucket, "", area));
        }
    }
    allowed.sort();
    expected.sort();
    assert_eq!(allowed, expected);
}

/// Counted, not timed: what each run writes, and that what comes back is
/// the file.
#[test]
fn a_run_sends_only_what_changed_and_never_writes_a_key_twice() {
    let dir = scratch("src");
    let work = scratch("work");
    let bucket = Memory::default();
    let store = store(&bucket, &work);
    fs::create_dir_all(dir.join("agent_x")).expect("agent");
    let a = dir.join("agent_x/a.jsonl");
    let b = dir.join("agent_x/b.jsonl");
    fs::write(&a, b"line 1\nline 2\n").expect("a");
    fs::write(&b, b"memory v1\n").expect("b");

    let first = store.put(&listed(&dir), 1_000).expect("first put");
    assert_eq!(
        (first.files, first.whole, first.segments_written),
        (2, 2, 2)
    );
    let again = store.put(&listed(&dir), 2_000).expect("again");
    assert_eq!(
        (
            again.unchanged,
            again.segments_written,
            again.segments_found
        ),
        (2, 0, 0),
        "an unchanged run sends nothing but its manifest"
    );
    assert_ne!(again.manifest, first.manifest, "a manifest key per run");

    // Appended: only the new bytes, as one segment.
    fs::write(&a, b"line 1\nline 2\nline 3\n").expect("grow");
    set_mtime(&a, 100);
    let grown = store.put(&listed(&dir), 3_000).expect("grown");
    assert_eq!((grown.appended, grown.segments_written), (1, 1));
    let mut holder = None;
    let entry_a = entry(&store, "agent_x/a.jsonl", &mut holder).clone();
    assert_eq!(entry_a.segments.len(), 2);
    assert_eq!(
        (entry_a.segments[1].offset, entry_a.segments[1].length),
        (14, 7)
    );
    assert_eq!(
        store.rebuild(&entry_a).expect("rebuild"),
        fs::read(&a).expect("a")
    );

    // Rewritten to the same size (a memory file): whole, and right.
    fs::write(&b, b"memory v2\n").expect("rewrite");
    set_mtime(&b, 200);
    let rewritten = store.put(&listed(&dir), 4_000).expect("rewritten");
    assert_eq!((rewritten.whole, rewritten.segments_written), (1, 1));
    let entry_b = entry(&store, "agent_x/b.jsonl", &mut holder).clone();
    assert_eq!(entry_b.segments.len(), 1);
    assert_eq!(store.rebuild(&entry_b).expect("rebuild"), b"memory v2\n");

    // Truncated: whole again.
    fs::write(&a, b"line 1\n").expect("truncate");
    set_mtime(&a, 300);
    let truncated = store.put(&listed(&dir), 5_000).expect("truncated");
    assert_eq!(truncated.whole, 1);
    let entry_a = entry(&store, "agent_x/a.jsonl", &mut holder).clone();
    assert_eq!(store.rebuild(&entry_a).expect("rebuild"), b"line 1\n");

    // The same bytes in a new file: the same name, not written again.
    fs::write(dir.join("agent_x/c.jsonl"), b"memory v2\n").expect("c");
    let same = store.put(&listed(&dir), 6_000).expect("same");
    assert_eq!((same.whole, same.segments_written), (1, 0));

    // No key was ever written twice; every manifest is unique.
    assert!(bucket.writes.borrow().values().all(|count| *count == 1));
    let manifests = bucket
        .list("locktest/t/transcripts/manifests/h/a/")
        .expect("list");
    assert_eq!(manifests.len(), 6);

    // Every segment the newest manifest names is there; older versions'
    // are kept and counted, not named.
    let (_, newest) = store.newest().expect("newest").expect("one");
    // Named: a's and b's (c is b's bytes, the same object); not named:
    // a's two older segments and b's first version. Sampled: a and b (the
    // first two) and c (the largest, as large as b).
    assert_eq!(
        store.verify(&newest).expect("verify"),
        Verified {
            named: 2,
            present: 2,
            missing: 0,
            wrong: 0,
            unnamed: 3,
            hashed: 3,
        }
    );
    // One gone (as a test, by hand: the layer itself never deletes): missing.
    let named = store.keys.segment(&newest.files[0].segments[0].name);
    bucket.objects.borrow_mut().remove(&named);
    let found = store.verify(&newest).expect("verify");
    assert_eq!((found.missing, found.ok()), (1, false));

    // What is stored is sealed; no object's metadata is its plaintext's
    // sha256 (rev-g F1: that would confirm a guessed segment), it is the
    // sealed bytes'; a segment's name is the HMAC of its plaintext.
    for (key, (bytes, sha256)) in bucket.objects.borrow().iter() {
        let plain = bytes.strip_prefix(b"sealed:").expect("sealed");
        assert_ne!(
            sha256,
            &hex(&Sha256::digest(plain)),
            "{key}: its plaintext's sha256"
        );
        assert_eq!(
            sha256,
            &hex(&Sha256::digest(bytes)),
            "{key}: the sealed bytes' sha256"
        );
        if key.contains("/segments/") {
            assert!(key.ends_with(&hex(&hmac_sha256(&[7; 32], plain))), "{key}");
        }
    }
    fs::remove_dir_all(&dir).expect("clean up");
    fs::remove_dir_all(&work).expect("clean up");
}

#[test]
fn sessions_are_their_three_places_and_a_symlink_is_never_followed() {
    let home = scratch("claude");
    fs::create_dir_all(home.join("projects/p")).expect("projects");
    fs::create_dir_all(home.join("file-history/f")).expect("file-history");
    fs::create_dir_all(home.join("other")).expect("other");
    fs::write(home.join("projects/p/s.jsonl"), b"s").expect("s");
    fs::write(home.join("projects/p/MEMORY.md"), b"m").expect("m");
    fs::write(home.join("file-history/f/1"), b"1").expect("1");
    fs::write(home.join("history.jsonl"), b"h").expect("h");
    fs::write(home.join("other/secret"), b"no").expect("other");
    fs::write(home.join(".credentials.json"), b"no").expect("credentials");
    std::os::unix::fs::symlink(home.join("other/secret"), home.join("projects/p/link"))
        .expect("link");
    let mut said = Vec::new();
    let found: Vec<String> = sources("sessions", &home, &mut said)
        .expect("sources")
        .into_iter()
        .map(|source| source.path)
        .collect();
    assert_eq!(
        found,
        [
            "file-history/f/1",
            "history.jsonl",
            "projects/p/MEMORY.md",
            "projects/p/s.jsonl"
        ]
    );
    assert!(
        said.iter()
            .any(|line| line.contains("projects/p/link (a symlink)")),
        "{said:?}"
    );
    let link = scratch("claude-link");
    let linked = link.join(".claude");
    std::os::unix::fs::symlink(&home, &linked).expect("a symlinked .claude");
    assert!(
        sources("sessions", &linked, &mut said)
            .unwrap_err()
            .contains("a symlink is refused")
    );
    fs::remove_dir_all(&home).expect("clean up");
    fs::remove_dir_all(&link).expect("clean up");
}

/// cc-astra A30: every object the manifest names is checked against the
/// size and MD5 it recorded, not only the sampled ones; an object changed
/// under its key (same size, or not) fails verify.
#[test]
fn verify_finds_a_changed_object_it_did_not_sample() {
    let dir = scratch("a30");
    let work = scratch("a30-work");
    let bucket = Memory::default();
    let store = store(&bucket, &work);
    fs::create_dir_all(dir.join("agent_x")).expect("agent");
    // d is the fourth file and the smallest: never in the sample.
    for (name, body) in [
        ("a", "aaaaaaaa\n"),
        ("b", "bbbbbbbbbb\n"),
        ("c", "cccccccccccccc\n"),
        ("d", "ddd\n"),
    ] {
        fs::write(dir.join(format!("agent_x/{name}.jsonl")), body).expect("file");
    }
    store.put(&listed(&dir), 1_000).expect("put");
    let (_, newest) = store.newest().expect("newest").expect("one");
    let good = store.verify(&newest).expect("verify");
    assert_eq!((good.named, good.present, good.hashed), (4, 4, 3));
    assert!(good.ok());
    let d = newest
        .files
        .iter()
        .find(|entry| entry.path == "agent_x/d.jsonl")
        .expect("d");
    let key = store.keys.segment(&d.segments[0].name);
    let original = bucket.objects.borrow()[&key].clone();

    // The same size, other bytes: the MD5 tells.
    let mut same_size = original.0.clone();
    *same_size.last_mut().expect("a byte") ^= 1;
    bucket
        .objects
        .borrow_mut()
        .insert(key.clone(), (same_size, original.1.clone()));
    let changed = store.verify(&newest).expect("verify");
    assert_eq!(
        (changed.wrong, changed.present, changed.ok()),
        (1, 3, false)
    );
    assert_eq!(
        changed.hashed, 0,
        "nothing is called good beside a wrong object"
    );

    // Another size: the size tells.
    let mut longer = original.0.clone();
    longer.push(b'x');
    bucket
        .objects
        .borrow_mut()
        .insert(key.clone(), (longer, original.1.clone()));
    assert_eq!(store.verify(&newest).expect("verify").wrong, 1);

    bucket.objects.borrow_mut().insert(key, original);
    assert!(store.verify(&newest).expect("verify").ok());
    fs::remove_dir_all(&dir).expect("clean up");
    fs::remove_dir_all(&work).expect("clean up");
}

/// cc-astra A32: two runs in one millisecond whose random parts sort the
/// other way: the newest is still the second (its generation), and the
/// third run reads the second's files as its last.
#[test]
fn the_newest_manifest_is_the_last_run_whatever_its_random_part() {
    let dir = scratch("a32");
    let work = scratch("a32-work");
    let bucket = Memory::default();
    let suffixes = RefCell::new(vec!["10000000", "f0000000"]);
    let nonce = || {
        suffixes
            .borrow_mut()
            .pop()
            .map(str::to_string)
            .ok_or_else(|| "no suffix left".to_string())
    };
    let store = Store {
        nonce: &nonce,
        ..store(&bucket, &work)
    };
    fs::create_dir_all(dir.join("agent_x")).expect("agent");
    let a = dir.join("agent_x/a.jsonl");
    fs::write(&a, b"one\n").expect("a");
    let first = store.put(&listed(&dir), 7_000).expect("first");
    fs::write(&a, b"one\ntwo\n").expect("grow");
    set_mtime(&a, 100);
    let second = store
        .put(&listed(&dir), 7_000)
        .expect("second, the same ms");
    assert!(
        first.manifest.ends_with("-f0000000.json.gpg"),
        "{}",
        first.manifest
    );
    assert!(
        second.manifest.ends_with("-10000000.json.gpg"),
        "{}",
        second.manifest
    );
    let (key, newest) = store.newest().expect("newest").expect("one");
    assert_eq!(key, second.manifest);
    assert_eq!((newest.generation, newest.files[0].size), (2, 8));
    // A manifest of the old form (no generation; locktest/ holds some) sorts
    // above every new one, and is never the newest (rev-g F1): it is not
    // even read (its bytes are not a manifest).
    let old = format!("{}20261112T163117Z-abcd.json.gpg", store.keys.manifests());
    assert!(old > second.manifest);
    bucket
        .objects
        .borrow_mut()
        .insert(old, (b"sealed:not a manifest".to_vec(), String::new()));
    let (key, _) = store
        .newest()
        .expect("the old form is skipped")
        .expect("one");
    assert_eq!(key, second.manifest);
    // A manifest under a new-form key but of another version is refused.
    let mut older = newest.clone();
    older.version = 1;
    let mut sealed = b"sealed:".to_vec();
    sealed.extend(serde_json::to_vec(&older).expect("json"));
    bucket.objects.borrow_mut().insert(
        format!("{}000000000009-x-y.json.gpg", store.keys.manifests()),
        (sealed, String::new()),
    );
    let error = store.newest().unwrap_err();
    assert!(error.contains("is of version 1, not 2"), "{error}");
    fs::remove_dir_all(&dir).expect("clean up");
    fs::remove_dir_all(&work).expect("clean up");
}

/// cc-astra A29: get writes under the directory it is given and nowhere
/// else: a symlinked parent, a dangling link at the file, or a file made
/// there by someone else first is refused, and nothing is written through
/// it or over it.
#[test]
fn restore_never_follows_a_link_and_never_writes_over_anything() {
    let into = scratch("a29-into");
    let elsewhere = scratch("a29-elsewhere");

    restore(&into, "projects/p/s.jsonl", b"bytes").expect("a fresh file");
    assert_eq!(
        fs::read(into.join("projects/p/s.jsonl")).expect("written"),
        b"bytes"
    );

    // A parent that is a symlink out of the directory.
    std::os::unix::fs::symlink(&elsewhere, into.join("linked")).expect("link");
    let error = restore(&into, "linked/s.jsonl", b"bytes").unwrap_err();
    assert!(error.starts_with("refused"), "{error}");
    assert!(
        !elsewhere.join("s.jsonl").exists(),
        "written through the link"
    );
    // A deeper parent too.
    fs::create_dir_all(into.join("deep")).expect("deep");
    std::os::unix::fs::symlink(&elsewhere, into.join("deep/linked")).expect("link");
    assert!(restore(&into, "deep/linked/x/s.jsonl", b"bytes").is_err());
    assert!(
        !elsewhere.join("x").exists(),
        "a directory made through the link"
    );

    // A dangling link where the file goes: not created through it.
    std::os::unix::fs::symlink(elsewhere.join("made"), into.join("dangling")).expect("link");
    let error = restore(&into, "dangling", b"bytes").unwrap_err();
    assert!(error.contains("already holds"), "{error}");
    assert!(!elsewhere.join("made").exists(), "created through the link");

    // Someone made the file first (a creator racing the write): left as it is.
    fs::write(into.join("projects/p/raced.jsonl"), b"theirs").expect("theirs");
    let error = restore(&into, "projects/p/raced.jsonl", b"ours").unwrap_err();
    assert!(error.contains("already holds"), "{error}");
    assert_eq!(
        fs::read(into.join("projects/p/raced.jsonl")).expect("theirs"),
        b"theirs"
    );

    // A path out of the directory.
    assert!(restore(&into, "../out", b"bytes").is_err());
    assert!(restore(&into, "/abs", b"bytes").is_err());
    fs::remove_dir_all(&into).expect("clean up");
    fs::remove_dir_all(&elsewhere).expect("clean up");
}

/// cc-astra pass 39: the projects and file-history roots are lstat'ed
/// before the walk; a symlinked root is refused, not walked.
#[test]
fn a_symlinked_projects_or_file_history_root_is_refused() {
    for root in ["projects", "file-history"] {
        let home = scratch(&format!("root-{root}"));
        let elsewhere = scratch(&format!("root-{root}-elsewhere"));
        fs::create_dir_all(elsewhere.join("p")).expect("p");
        fs::write(elsewhere.join("p/secret.jsonl"), b"no").expect("secret");
        fs::write(home.join("history.jsonl"), b"h").expect("h");
        std::os::unix::fs::symlink(&elsewhere, home.join(root)).expect("link");
        let mut said = Vec::new();
        let error = sources("sessions", &home, &mut said).unwrap_err();
        assert!(error.contains("a symlink is refused"), "{root}: {error}");
        fs::remove_dir_all(&home).expect("clean up");
        fs::remove_dir_all(&elsewhere).expect("clean up");
    }
}

/// The lead's pin on A30: an object whose ETag is a multipart one is
/// refused, not recorded, so verify's MD5 check never meets one.
#[test]
fn a_multipart_etag_is_refused() {
    struct Multipart(Memory);
    impl Bucket for Multipart {
        fn put_new(&self, key: &str, file: &Path, sha256: &str) -> Result<Put, String> {
            self.0.put_new(key, file, sha256)
        }
        fn head(&self, key: &str) -> Result<Option<Head>, String> {
            Ok(self.0.head(key)?.map(|head| Head {
                etag: format!("{}-2", head.etag),
                ..head
            }))
        }
        fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
            self.0.get(key)
        }
        fn list(&self, prefix: &str) -> Result<Vec<Listed>, String> {
            self.0.list(prefix)
        }
    }
    let dir = scratch("multipart");
    let work = scratch("multipart-work");
    let bucket = Multipart(Memory::default());
    let store = Store {
        bucket: &bucket,
        keys: keys(),
        name_key: [7; 32],
        seal: &seal,
        open: &open,
        scratch: &work,
        nonce: &|| random_hex(4),
    };
    fs::create_dir_all(dir.join("agent_x")).expect("agent");
    fs::write(dir.join("agent_x/a.jsonl"), b"a\n").expect("a");
    let error = store.put(&listed(&dir), 1_000).unwrap_err();
    assert!(error.contains("not one PUT's MD5"), "{error}");
    fs::remove_dir_all(&dir).expect("clean up");
    fs::remove_dir_all(&work).expect("clean up");
}

/// rev-g F2: an object already at a segment's key (an earlier run's, a
/// racer's) whose ETag is a multipart one is refused on the found path
/// too, never recorded, and nothing is written over it.
#[test]
fn a_found_object_with_a_multipart_etag_is_refused() {
    struct Found(Memory, String);
    impl Bucket for Found {
        fn put_new(&self, key: &str, file: &Path, sha256: &str) -> Result<Put, String> {
            self.0.put_new(key, file, sha256)
        }
        fn head(&self, key: &str) -> Result<Option<Head>, String> {
            Ok(self.0.head(key)?.map(|head| Head {
                etag: if key == self.1 {
                    format!("{}-2", head.etag)
                } else {
                    head.etag
                },
                ..head
            }))
        }
        fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
            self.0.get(key)
        }
        fn list(&self, prefix: &str) -> Result<Vec<Listed>, String> {
            self.0.list(prefix)
        }
    }
    let dir = scratch("found-multipart");
    let work = scratch("found-multipart-work");
    let body = b"a\n";
    let key = keys().segment(&hex(&hmac_sha256(&[7; 32], body)));
    let bucket = Found(Memory::default(), key.clone());
    bucket
        .0
        .objects
        .borrow_mut()
        .insert(key.clone(), (b"sealed:a\n".to_vec(), "theirs".to_string()));
    let store = Store {
        bucket: &bucket,
        keys: keys(),
        name_key: [7; 32],
        seal: &seal,
        open: &open,
        scratch: &work,
        nonce: &|| random_hex(4),
    };
    fs::create_dir_all(dir.join("agent_x")).expect("agent");
    fs::write(dir.join("agent_x/a.jsonl"), body).expect("a");
    let error = store.put(&listed(&dir), 1_000).unwrap_err();
    assert!(error.contains("not one PUT's MD5"), "{error}");
    assert!(error.starts_with(&key), "the found segment's: {error}");
    assert!(bucket.0.writes.borrow().is_empty(), "nothing written");
    fs::remove_dir_all(&dir).expect("clean up");
    fs::remove_dir_all(&work).expect("clean up");
}

/// The store's credential (lead-mac 01:41): its four keys, the bucket one
/// it lists, mode 600; what it refuses names the key, never a value.
#[test]
fn the_store_credential_gives_a_listed_bucket_and_never_says_a_value() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("credential");
    let file = dir.join("r2-store.env");
    let write = |text: &str, mode: u32| {
        fs::write(&file, text).expect("credential");
        fs::set_permissions(&file, fs::Permissions::from_mode(mode)).expect("mode");
    };
    let full = "R2_ENDPOINT=https://made-up.example\nR2_ACCESS_KEY_ID=made-up-id\n\
R2_SECRET_ACCESS_KEY=made-up-secret\nR2_BUCKETS=b-store,b-keep,b-work\n";
    write(full, 0o600);
    let r2 = store_r2(&file, Some("b-work")).expect("a listed bucket");
    assert_eq!(
        (
            r2.endpoint.as_str(),
            r2.bucket.as_str(),
            r2.access_key.as_str(),
            r2.secret_key.as_str()
        ),
        (
            "https://made-up.example",
            "b-work",
            "made-up-id",
            "made-up-secret"
        )
    );
    let refusals = [
        store_r2(&file, Some("b-other")).unwrap_err(),
        store_r2(&file, None).unwrap_err(),
        store_r2(&file, Some(" ")).unwrap_err(),
        {
            write(
                &full.replace("R2_SECRET_ACCESS_KEY=made-up-secret\n", ""),
                0o600,
            );
            store_r2(&file, Some("b-work")).unwrap_err()
        },
        {
            write(full, 0o644);
            store_r2(&file, Some("b-work")).unwrap_err()
        },
    ];
    assert!(refusals[0].contains("does not list"), "{}", refusals[0]);
    assert!(refusals[1].contains("SPAWN_STORE_BUCKET is required"));
    assert!(refusals[2].contains("SPAWN_STORE_BUCKET is required"));
    assert!(
        refusals[3].contains("R2_SECRET_ACCESS_KEY is not set"),
        "{}",
        refusals[3]
    );
    assert!(refusals[4].contains("must be 600"), "{}", refusals[4]);
    for refusal in &refusals {
        for value in [
            "made-up.example",
            "made-up-id",
            "made-up-secret",
            "b-store",
            "b-keep",
            "b-other",
        ] {
            assert!(!refusal.contains(value), "{value} in {refusal}");
        }
    }
    fs::remove_dir_all(&dir).expect("clean up");
}
