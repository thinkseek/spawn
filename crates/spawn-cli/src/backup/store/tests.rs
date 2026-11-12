//! The store layer without a network or gpg: an in-memory bucket that
//! counts every write per key, and a seal that only marks the bytes.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{
    Bucket, Entry, Head, Keys, Plan, Put, Source, Store, hmac_sha256, may_write, plan, ranges,
    sources,
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
    fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>, String> {
        Ok(self
            .objects
            .borrow()
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, (bytes, _))| (key.clone(), bytes.len() as u64))
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
        "spawn-store-{name}-{}-{}",
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

#[test]
fn it_writes_only_under_locktest_until_switched_on() {
    assert!(may_write("locktest/store-test-1/", None).is_ok());
    assert!(may_write("", None).unwrap_err().contains("switched off"));
    assert!(may_write("sessions/", None).is_err());
    assert!(may_write("sessions/", Some("0")).is_err());
    assert!(may_write("", Some("1")).is_ok());
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
    let (present, missing, unnamed) = store.check(&newest).expect("check");
    assert_eq!(missing, 0);
    // Named: a's and b's (c is b's bytes, the same object); not named:
    // a's two older segments and b's first version.
    assert_eq!((present, unnamed), (2, 3));
    // One gone (as a test, by hand: the layer itself never deletes): missing.
    let named = store.keys.segment(&newest.files[0].segments[0].name);
    bucket.objects.borrow_mut().remove(&named);
    assert_eq!(store.check(&newest).expect("check").1, 1);

    // What is stored is sealed, and its name is the HMAC of its bytes.
    let (bytes, sha256) = bucket
        .objects
        .borrow()
        .iter()
        .find(|(key, _)| key.contains("/segments/"))
        .map(|(_, value)| value.clone())
        .expect("a segment");
    assert!(bytes.starts_with(b"sealed:"));
    assert_eq!(sha256, hex(&Sha256::digest(&bytes[7..])));
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
