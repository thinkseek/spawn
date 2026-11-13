//! The store layer without a network or gpg: an in-memory bucket that
//! counts every write per key, and a seal that only marks the bytes.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{
    Bucket, Entry, Head, Keys, Listed, Plan, Put, Reader, Source, Store, Verified, create_list,
    group, hmac_sha256, may_write, plan, plan_lines, plan_summary, put_summary, random_hex, ranges,
    restore, restore_with, sources, store_r2, write_list,
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
}

impl Reader for Memory {
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
#[cfg(target_os = "linux")] // a restore needs O_TMPFILE
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
    }

    impl Reader for Multipart {
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
    }

    impl Reader for Found {
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

/// cc-astra A30 adoption: an object already at a segment's key enters the
/// manifest only when its contents are that segment's; one of the right
/// size and a valid ETag but other bytes, at the key of a file verify would
/// not sample, is an error naming the key, and nothing is written over it.
#[test]
fn a_found_object_is_adopted_only_on_its_contents() {
    let body: &[u8] = b"ddd\n";
    let key = keys().segment(&hex(&hmac_sha256(&[7; 32], body)));
    for (seeded, adopted) in [
        (b"sealed:ddd\n".to_vec(), true),
        (b"sealed:eee\n".to_vec(), false),
    ] {
        let dir = scratch("adopt");
        let work = scratch("adopt-work");
        let bucket = Memory::default();
        let store = store(&bucket, &work);
        fs::create_dir_all(dir.join("agent_x")).expect("agent");
        for (name, text) in [
            ("a", &b"aaaaaaaa\n"[..]),
            ("b", b"bbbbbbbbbb\n"),
            ("c", b"cccccccccccccc\n"),
            ("d", body),
        ] {
            fs::write(dir.join(format!("agent_x/{name}.jsonl")), text).expect("file");
        }
        bucket
            .objects
            .borrow_mut()
            .insert(key.clone(), (seeded.clone(), "theirs".to_string()));
        let result = store.put(&listed(&dir), 1_000);
        if adopted {
            let counts = result.expect("adopted on its contents");
            assert_eq!((counts.segments_found, counts.segments_written), (1, 3));
        } else {
            let error = result.unwrap_err();
            assert!(error.starts_with(&key), "{error}");
            assert!(error.contains("not this segment"), "{error}");
            assert!(store.newest().expect("newest").is_none(), "no manifest");
        }
        assert_eq!(
            bucket.objects.borrow()[&key].0,
            seeded,
            "never written over"
        );
        assert!(!bucket.writes.borrow().contains_key(&key));
        fs::remove_dir_all(&dir).expect("clean up");
        fs::remove_dir_all(&work).expect("clean up");
    }
}

/// cc-astra A37: a restore never deletes. A write that fails midway leaves
/// the bytes so far at the partial name, named in the error; nothing in
/// the directory is removed, and a restore into the same place refuses on
/// the partial file rather than writing over it.
#[test]
#[cfg(target_os = "linux")] // a restore needs O_TMPFILE
fn a_failed_restore_leaves_its_partial_file_and_removes_nothing() {
    use std::io::Write as _;
    let into = scratch("a37");
    fs::create_dir_all(into.join("p")).expect("p");
    fs::write(into.join("p/other.jsonl"), b"theirs").expect("theirs");
    let listing = |dir: &Path| -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("list")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    };
    let error = restore_with(&into, "p/s.jsonl", b"whole", |file| {
        file.write_all(b"half")?;
        Err(std::io::Error::other("the disk filled"))
    })
    .unwrap_err();
    let partial = into.join("p/s.jsonl.spawn-store-get.partial");
    assert!(error.contains(&partial.display().to_string()), "{error}");
    assert!(error.contains("the disk filled"), "{error}");
    assert_eq!(fs::read(&partial).expect("the partial file"), b"half");
    assert_eq!(
        listing(&into.join("p")),
        ["other.jsonl", "s.jsonl.spawn-store-get.partial"],
        "nothing removed, the file not made"
    );
    assert_eq!(
        fs::read(into.join("p/other.jsonl")).expect("theirs"),
        b"theirs"
    );
    let error = restore(&into, "p/s.jsonl", b"whole").unwrap_err();
    assert!(error.contains("already holds"), "{error}");
    assert!(error.contains("s.jsonl.spawn-store-get.partial"), "{error}");
    assert_eq!(
        fs::read(&partial).expect("still"),
        b"half",
        "never written over"
    );
    // A creator racing the write: the file's name taken while the partial
    // is written. The rename never replaces it; ours stays the partial.
    let raced = into.join("p/r.jsonl");
    let error = restore_with(&into, "p/r.jsonl", b"ours", |file| {
        fs::write(&raced, b"theirs")?;
        file.write_all(b"ours")
    })
    .unwrap_err();
    assert!(error.contains("r.jsonl.spawn-store-get.partial"), "{error}");
    assert_eq!(
        fs::read(&raced).expect("theirs"),
        b"theirs",
        "never replaced"
    );
    assert_eq!(
        fs::read(into.join("p/r.jsonl.spawn-store-get.partial")).expect("ours"),
        b"ours"
    );
    // A whole restore elsewhere names its file and leaves no partial.
    restore(&into, "p/t.jsonl", b"whole").expect("whole");
    assert_eq!(fs::read(into.join("p/t.jsonl")).expect("t"), b"whole");
    assert!(!into.join("p/t.jsonl.spawn-store-get.partial").exists());
    fs::remove_dir_all(&into).expect("clean up");
}

/// The names in `dir`, sorted.
#[cfg(target_os = "linux")]
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("list")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// cc-astra A38: a restore that succeeds names the very file it wrote and
/// read back, whatever is put at its partial name meanwhile. A file and a
/// symlink put there while it is written are never moved, named or
/// followed; the file's name holds the verified bytes, a regular file of
/// its own.
#[test]
#[cfg(target_os = "linux")] // a restore needs O_TMPFILE
fn a_restore_names_the_file_it_wrote_whatever_is_put_at_its_partial_name() {
    use std::io::Write as _;
    use std::os::unix::fs::MetadataExt as _;
    let into = scratch("a38-swap");
    let elsewhere = scratch("a38-swap-elsewhere");
    fs::create_dir_all(into.join("p")).expect("p");
    let file_partial = into.join("p/f.jsonl.spawn-store-get.partial");
    restore_with(&into, "p/f.jsonl", b"verified", |file| {
        file.write_all(b"verified")?;
        fs::write(&file_partial, b"substitute")
    })
    .expect("its own file named");
    let link_partial = into.join("p/l.jsonl.spawn-store-get.partial");
    restore_with(&into, "p/l.jsonl", b"verified", |file| {
        file.write_all(b"verified")?;
        std::os::unix::fs::symlink(elsewhere.join("target"), &link_partial)
    })
    .expect("its own file named");
    for name in ["p/f.jsonl", "p/l.jsonl"] {
        let named = fs::symlink_metadata(into.join(name)).expect("named");
        assert!(named.file_type().is_file(), "{name}: {named:?}");
        assert_eq!(fs::read(into.join(name)).expect("named"), b"verified");
    }
    assert_eq!(fs::read(&file_partial).expect("left"), b"substitute");
    assert_ne!(
        fs::symlink_metadata(&file_partial).expect("left").ino(),
        fs::symlink_metadata(into.join("p/f.jsonl"))
            .expect("named")
            .ino(),
        "the substitute is not the file named"
    );
    assert_eq!(
        fs::read_link(&link_partial).expect("still a link"),
        elsewhere.join("target")
    );
    assert!(!elsewhere.join("target").exists(), "followed");
    assert_eq!(
        names(&into.join("p")),
        [
            "f.jsonl",
            "f.jsonl.spawn-store-get.partial",
            "l.jsonl",
            "l.jsonl.spawn-store-get.partial"
        ],
        "nothing moved or removed"
    );
    fs::remove_dir_all(&into).expect("clean up");
    fs::remove_dir_all(&elsewhere).expect("clean up");
}

/// cc-astra A38: a file changed while it is written (through its own
/// descriptor, the one way left to reach a file with no name) fails the
/// read back: nothing is named at the file's name, and the bytes are left
/// at the partial name. A failed write whose partial name was taken
/// meanwhile names nothing at all and follows nothing.
#[test]
#[cfg(target_os = "linux")] // a restore needs O_TMPFILE
fn a_restore_whose_file_changed_or_whose_partial_name_was_taken_names_nothing() {
    use std::io::Write as _;
    use std::os::fd::AsRawFd as _;
    let into = scratch("a38-changed");
    let elsewhere = scratch("a38-changed-elsewhere");
    fs::create_dir_all(into.join("p")).expect("p");
    let error = restore_with(&into, "p/c.jsonl", b"verified", |file| {
        file.write_all(b"verified")?;
        let own = format!("/proc/self/fd/{}", file.as_raw_fd());
        fs::OpenOptions::new()
            .write(true)
            .open(own)?
            .write_all(b"VERIFIED")
    })
    .unwrap_err();
    assert!(error.contains("changed while written"), "{error}");
    assert!(error.contains("c.jsonl.spawn-store-get.partial"), "{error}");
    assert!(!into.join("p/c.jsonl").exists(), "named");
    assert_eq!(
        fs::read(into.join("p/c.jsonl.spawn-store-get.partial")).expect("left"),
        b"VERIFIED"
    );
    // Longer than verified: seen too.
    let error = restore_with(&into, "p/g.jsonl", b"verified", |file| {
        file.write_all(b"verified!")
    })
    .unwrap_err();
    assert!(error.contains("9 bytes, not the 8"), "{error}");
    assert!(!into.join("p/g.jsonl").exists(), "named");
    let taken = into.join("p/u.jsonl.spawn-store-get.partial");
    let error = restore_with(&into, "p/u.jsonl", b"verified", |file| {
        file.write_all(b"ver")?;
        std::os::unix::fs::symlink(elsewhere.join("target"), &taken)?;
        Err(std::io::Error::other("the disk filled"))
    })
    .unwrap_err();
    assert!(error.contains("have no name"), "{error}");
    assert!(error.contains("the disk filled"), "{error}");
    assert_eq!(
        fs::read_link(&taken).expect("still a link"),
        elsewhere.join("target")
    );
    assert!(!elsewhere.join("target").exists(), "followed");
    assert_eq!(
        names(&into.join("p")),
        [
            "c.jsonl.spawn-store-get.partial",
            "g.jsonl.spawn-store-get.partial",
            "u.jsonl.spawn-store-get.partial"
        ],
        "nothing named at a file's name, nothing removed"
    );
    fs::remove_dir_all(&into).expect("clean up");
    fs::remove_dir_all(&elsewhere).expect("clean up");
}

/// cc-astra A30's second branch: the key is free at the HEAD, and another
/// writer's object lands there before the conditional PUT, which answers
/// "already there" (412). The object found is adopted only on its contents:
/// one of the right size but other bytes is refused, no manifest is made,
/// and nothing is written over it.
#[test]
fn an_object_found_by_the_conditional_put_is_adopted_only_on_its_contents() {
    struct Raced {
        inner: Memory,
        key: String,
        theirs: Vec<u8>,
    }
    impl Bucket for Raced {
        fn put_new(&self, key: &str, file: &Path, sha256: &str) -> Result<Put, String> {
            if key == self.key {
                // Theirs lands between our HEAD and our PUT.
                self.inner
                    .objects
                    .borrow_mut()
                    .entry(key.to_string())
                    .or_insert_with(|| (self.theirs.clone(), "theirs".to_string()));
            }
            self.inner.put_new(key, file, sha256)
        }
    }

    impl Reader for Raced {
        fn head(&self, key: &str) -> Result<Option<Head>, String> {
            self.inner.head(key)
        }
        fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
            self.inner.get(key)
        }
        fn list(&self, prefix: &str) -> Result<Vec<Listed>, String> {
            self.inner.list(prefix)
        }
    }
    let body: &[u8] = b"ddd\n";
    let key = keys().segment(&hex(&hmac_sha256(&[7; 32], body)));
    for (theirs, adopted) in [
        (b"sealed:ddd\n".to_vec(), true),
        (b"sealed:eee\n".to_vec(), false),
    ] {
        let dir = scratch("adopt-412");
        let work = scratch("adopt-412-work");
        let bucket = Raced {
            inner: Memory::default(),
            key: key.clone(),
            theirs: theirs.clone(),
        };
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
        for (name, text) in [
            ("a", &b"aaaaaaaa\n"[..]),
            ("b", b"bbbbbbbbbb\n"),
            ("c", b"cccccccccccccc\n"),
            ("d", body),
        ] {
            fs::write(dir.join(format!("agent_x/{name}.jsonl")), text).expect("file");
        }
        assert!(
            bucket.head(&key).expect("head").is_none(),
            "free at the HEAD"
        );
        let result = store.put(&listed(&dir), 1_000);
        if adopted {
            let counts = result.expect("adopted on its contents");
            assert_eq!((counts.segments_found, counts.segments_written), (1, 3));
        } else {
            let error = result.unwrap_err();
            assert!(error.starts_with(&key), "{error}");
            assert!(error.contains("not this segment"), "{error}");
            assert!(store.newest().expect("newest").is_none(), "no manifest");
        }
        assert_eq!(
            bucket.inner.objects.borrow()[&key].0,
            theirs,
            "never written over"
        );
        assert!(!bucket.inner.writes.borrow().contains_key(&key));
        fs::remove_dir_all(&dir).expect("clean up");
        fs::remove_dir_all(&work).expect("clean up");
    }
}

/// A bucket that panics at any PUT: a plan run on it proves it sent
/// nothing (the plan's own type, [`super::ReadOnly`], has no PUT at all).
struct Refusing<'a>(&'a Memory);

impl Bucket for Refusing<'_> {
    fn put_new(&self, key: &str, _: &Path, _: &str) -> Result<Put, String> {
        panic!("a plan tried to write {key}");
    }
}

impl Reader for Refusing<'_> {
    fn head(&self, key: &str) -> Result<Option<Head>, String> {
        self.0.head(key)
    }
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.0.get(key)
    }
    fn list(&self, prefix: &str) -> Result<Vec<Listed>, String> {
        self.0.list(prefix)
    }
}

fn planner<'a>(bucket: &'a Refusing<'a>, work: &'a Path) -> Store<'a, Refusing<'a>> {
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

/// lead-mac's go, step 1: `plan` says what `put` would send, and sends
/// nothing. After a put, one file grows, one is rewritten, one is new and
/// one is untouched: the plan counts them as the next put will, with the
/// bytes it would send, by top directory, over a bucket that panics at a
/// PUT; the bucket's objects are as they were.
#[test]
fn a_plan_says_what_a_put_would_send_and_sends_nothing() {
    let dir = scratch("plan");
    let work = scratch("plan-work");
    let bucket = Memory::default();
    fs::create_dir_all(dir.join("agent_x")).expect("agent");
    fs::create_dir_all(dir.join("agent_y")).expect("agent");
    fs::write(dir.join("agent_x/a.jsonl"), b"aaaa\n").expect("a");
    fs::write(dir.join("agent_x/b.jsonl"), b"bbbb\n").expect("b");
    fs::write(dir.join("agent_y/c.jsonl"), b"cccc\n").expect("c");
    store(&bucket, &work)
        .put(&listed(&dir), 1_000)
        .expect("the first put");
    let before = bucket.objects.borrow().clone();
    let writes = bucket.writes.borrow().clone();
    // a grows; b is rewritten (another size, other bytes); d is new.
    fs::write(dir.join("agent_x/a.jsonl"), b"aaaa\nmore\n").expect("a");
    set_mtime(&dir.join("agent_x/a.jsonl"), 2_000_000_000);
    fs::write(dir.join("agent_x/b.jsonl"), b"BB\n").expect("b");
    set_mtime(&dir.join("agent_x/b.jsonl"), 2_000_000_000);
    fs::write(dir.join("agent_y/d.jsonl"), b"dddddd\n").expect("d");
    let refusing = Refusing(&bucket);
    let counts = planner(&refusing, &work)
        .plan(&listed(&dir))
        .expect("the plan");
    assert_eq!(
        (
            counts.files,
            counts.unchanged,
            counts.appended,
            counts.whole
        ),
        (4, 1, 1, 2)
    );
    assert_eq!(counts.bytes, 10 + 3 + 5 + 7);
    assert_eq!(counts.send, 5 + 3 + 7, "a's new bytes, b and d whole");
    assert_eq!(counts.by_group["agent_x"], (2, 13, 8));
    assert_eq!(counts.by_group["agent_y"], (2, 12, 7));
    assert_eq!(counts.by_group.len(), 2);
    assert_eq!(
        counts.listed,
        [
            ("agent_x/a.jsonl".to_string(), 10),
            ("agent_x/b.jsonl".to_string(), 3),
            ("agent_y/c.jsonl".to_string(), 5),
            ("agent_y/d.jsonl".to_string(), 7),
        ]
    );
    assert_eq!(*bucket.objects.borrow(), before, "nothing written");
    assert_eq!(*bucket.writes.borrow(), writes, "no PUT");
    fs::remove_dir_all(&dir).expect("clean up");
    fs::remove_dir_all(&work).expect("clean up");
}

/// A plan's group names directories, never a file: a transcript's agent;
/// a session file's root and project (or session) directory, fewer when it
/// is higher up; `history.jsonl` by its fixed name; any other file at the
/// top as `(top)`.
#[test]
fn a_plans_group_is_a_directory_never_a_file() {
    assert_eq!(group("transcripts", "agent_x/a.jsonl"), "agent_x");
    assert_eq!(group("transcripts", "agent_x/sub/a.jsonl"), "agent_x");
    assert_eq!(group("transcripts", "loose.jsonl"), "(top)");
    assert_eq!(
        group("sessions", "projects/-home-x/s.jsonl"),
        "projects/-home-x"
    );
    assert_eq!(
        group("sessions", "projects/-home-x/sub/t.jsonl"),
        "projects/-home-x"
    );
    assert_eq!(group("sessions", "file-history/u1/h@v1"), "file-history/u1");
    assert_eq!(group("sessions", "projects/x.jsonl"), "projects");
    assert_eq!(group("sessions", "history.jsonl"), "history.jsonl");
    assert_eq!(group("sessions", "other.jsonl"), "(top)");
}

/// The lead's ruling: what a plan prints names no file, and its groups'
/// counts add up to the `--list` file's lines (every file name is there
/// alone). A symlink put would skip is counted by why, not named.
#[test]
fn a_plans_lines_name_no_file_and_add_up_to_its_list() {
    let dir = scratch("plan-lines");
    let work = scratch("plan-lines-work");
    let bucket = Memory::default();
    for (agent, name, text) in [
        ("agent_x", "secret-a.jsonl", &b"aaaa\n"[..]),
        ("agent_x", "secret-b.jsonl", b"bb\n"),
        ("agent_y", "secret-c.jsonl", b"c\n"),
    ] {
        fs::create_dir_all(dir.join(agent)).expect("agent");
        fs::write(dir.join(agent).join(name), text).expect("file");
    }
    std::os::unix::fs::symlink(
        dir.join("agent_y/secret-c.jsonl"),
        dir.join("agent_y/secret-link"),
    )
    .expect("link");
    let mut said = Vec::new();
    let selected = sources("transcripts", &dir, &mut said).expect("sources");
    assert_eq!(said.len(), 1, "{said:?}");
    let refusing = Refusing(&bucket);
    let counts = planner(&refusing, &work).plan(&selected).expect("the plan");
    let lines = plan_lines("transcripts", "work", "locktest/t/", &said, &counts);
    for line in &lines {
        assert!(!line.contains("secret"), "a file named: {line}");
    }
    assert!(
        lines.iter().any(|line| line
            == "store plan transcripts: 1 entry(ies) skipped as put skips them (a symlink)"),
        "{lines:?}"
    );
    let at = work.join("list.txt");
    let mut file = create_list(&at).expect("new");
    write_list(&mut file, &counts.listed).expect("written");
    let listed = fs::read_to_string(&at).expect("the list").lines().count() as u64;
    let grouped: u64 = lines
        .iter()
        .filter_map(|line| line.strip_suffix(" to send"))
        .filter_map(|line| line.rsplit(": ").next())
        .filter_map(|counted| counted.split(' ').next()?.parse::<u64>().ok())
        .sum();
    assert_eq!((grouped, listed), (3, 3), "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("store plan transcripts: 3 file(s): ")),
        "{lines:?}"
    );
    fs::remove_dir_all(&dir).expect("clean up");
    fs::remove_dir_all(&work).expect("clean up");
}

/// A plan and the put after it, nothing changed between them, agree on the
/// files: the same counts, and their lines begin alike ("N file(s): U
/// unchanged, G grown, W whole"), on a first run and on one after changes.
#[test]
fn a_plan_and_the_put_after_it_agree_on_the_files() {
    let dir = scratch("plan-put");
    let work = scratch("plan-put-work");
    let bucket = Memory::default();
    fs::create_dir_all(dir.join("agent_x")).expect("agent");
    fs::write(dir.join("agent_x/a.jsonl"), b"aaaa\n").expect("a");
    fs::write(dir.join("agent_x/b.jsonl"), b"bbbb\n").expect("b");
    for round in 0..2 {
        if round == 1 {
            fs::write(dir.join("agent_x/a.jsonl"), b"aaaa\nmore\n").expect("a");
            set_mtime(&dir.join("agent_x/a.jsonl"), 2_000_000_000);
            fs::write(dir.join("agent_x/c.jsonl"), b"cc\n").expect("c");
        }
        let refusing = Refusing(&bucket);
        let planned = planner(&refusing, &work)
            .plan(&listed(&dir))
            .expect("the plan");
        let put = store(&bucket, &work)
            .put(&listed(&dir), 1_000 + round)
            .expect("the put");
        assert_eq!(
            (
                planned.files,
                planned.unchanged,
                planned.appended,
                planned.whole
            ),
            (put.files, put.unchanged, put.appended, put.whole),
            "round {round}"
        );
        let first = |line: String| line.split(';').next().map(str::to_string);
        assert_eq!(
            first(plan_summary(&planned)),
            first(put_summary(&put)),
            "round {round}"
        );
    }
    fs::remove_dir_all(&dir).expect("clean up");
    fs::remove_dir_all(&work).expect("clean up");
}

/// `--list`: a new file, mode 0600, a line per path with its size; an entry
/// already at the name (a file, a symlink) is refused and left as it is.
#[test]
fn the_plans_list_is_a_new_private_file_never_written_over() {
    use std::io::Read as _;
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("plan-list");
    let at = dir.join("list.txt");
    let mut file = create_list(&at).expect("new");
    write_list(
        &mut file,
        &[("a/b.jsonl".to_string(), 3), ("tab\tname".to_string(), 0)],
    )
    .expect("written");
    let mut text = String::new();
    fs::File::open(&at)
        .expect("read")
        .read_to_string(&mut text)
        .expect("read");
    assert_eq!(text, "3\t\"a/b.jsonl\"\n0\t\"tab\\tname\"\n");
    assert_eq!(
        fs::metadata(&at).expect("mode").permissions().mode() & 0o777,
        0o600
    );
    let error = create_list(&at).unwrap_err();
    assert!(error.contains("never written over"), "{error}");
    assert_eq!(fs::read_to_string(&at).expect("kept"), text);
    let link = dir.join("link.txt");
    std::os::unix::fs::symlink(dir.join("target.txt"), &link).expect("link");
    assert!(create_list(&link).is_err());
    assert!(!dir.join("target.txt").exists(), "created through the link");
    fs::remove_dir_all(&dir).expect("clean up");
}

/// Objective 116: after a manifest of another key, `put` names none of its
/// segments again (another key's HMACs) and puts every file whole under
/// this run's key, with the next generation; `plan` says the same.
#[test]
fn after_another_keys_manifest_every_file_is_put_whole_and_no_old_segment_named() {
    let bucket = Memory::default();
    let work = scratch("other-key-work");
    let dir = scratch("other-key-files");
    fs::write(dir.join("a.jsonl"), b"line 1\nline 2\n").expect("a");
    fs::write(dir.join("b.jsonl"), b"memory v1\n").expect("b");
    let sources = listed(&dir);
    let old = store(&bucket, &work);
    old.put(&sources, 1_000).expect("the old key's put");
    let (_, last) = old.newest().expect("newest").expect("one");
    let old_names: Vec<String> = last
        .files
        .iter()
        .flat_map(|entry| entry.segments.iter().map(|segment| segment.name.clone()))
        .collect();
    let mut new = store(&bucket, &work);
    new.name_key = [9; 32];
    let reused = new.plan_after(&sources, Some(&last), true).expect("plan");
    assert_eq!(
        (reused.unchanged, reused.whole),
        (2, 0),
        "the same key reuses"
    );
    let planned = new.plan_after(&sources, Some(&last), false).expect("plan");
    assert_eq!(
        (planned.unchanged, planned.appended, planned.whole),
        (0, 0, 2)
    );
    assert_eq!(planned.send, planned.bytes, "every byte is to be sent");
    let counts = new
        .put_after(&sources, 2_000, Some(&last), false)
        .expect("the new key's put");
    assert_eq!(
        (
            counts.files,
            counts.unchanged,
            counts.appended,
            counts.whole
        ),
        (2, 0, 0, 2)
    );
    assert_eq!(counts.segments_found, 0);
    let (_, made) = new.newest().expect("newest").expect("one");
    assert_eq!(made.generation, last.generation + 1);
    for entry in &made.files {
        for segment in &entry.segments {
            assert!(!old_names.contains(&segment.name), "{}", segment.name);
        }
    }
    fs::remove_dir_all(&work).expect("clean up");
    fs::remove_dir_all(&dir).expect("clean up");
}

/// A bucket whose objects all say `seal` in their header.
struct Sealed<'a>(&'a Memory, &'static str);

impl Reader for Sealed<'_> {
    fn head(&self, key: &str) -> Result<Option<Head>, String> {
        self.0.head(key)
    }
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.0.get(key)
    }
    fn list(&self, prefix: &str) -> Result<Vec<Listed>, String> {
        self.0.list(prefix)
    }
    fn sealed_with(&self, _: &str) -> Result<String, String> {
        crate::backup::key_name(Some(self.1))
    }
}

/// The newest manifest's key: none without a manifest or when it is the
/// sealing key's; a bucket that keeps no header reads as k1; another key
/// whose file is not on this host refuses the run, naming the key, never a
/// value.
#[test]
fn the_newest_manifests_key_is_its_headers_and_its_file_must_be_here() {
    use super::older_key;
    let bucket = Memory::default();
    let work = scratch("older-key-work");
    let dir = scratch("older-key-files");
    let plain = store(&bucket, &work);
    assert_eq!(older_key(&plain, "k2"), Ok(None), "no manifest yet");
    fs::write(dir.join("a.jsonl"), b"line\n").expect("a");
    plain.put(&listed(&dir), 1_000).expect("put");
    assert_eq!(
        older_key(&plain, "k1"),
        Ok(None),
        "no header: k1's, the sealing key"
    );
    let sealed = Sealed(&bucket, "k9");
    let reading = Store {
        bucket: &sealed,
        keys: keys(),
        name_key: [7; 32],
        seal: &seal,
        open: &open,
        scratch: &work,
        nonce: &|| random_hex(4),
    };
    assert_eq!(older_key(&reading, "k9"), Ok(None));
    let refused = older_key(&reading, "k2").expect_err("k9's file is not here");
    assert!(
        refused.starts_with("the last manifest is sealed with k9; its key file ")
            && refused.ends_with(" is not on this host"),
        "{refused}"
    );
    fs::remove_dir_all(&work).expect("clean up");
    fs::remove_dir_all(&dir).expect("clean up");
}
