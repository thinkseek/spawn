//! Collection of unreferenced chunk-store objects (`spawn chunks gc`,
//! docs/storage.md, "The chunk store").
//!
//! Live is what any reachable commit can still restore: every commit
//! reachable from any ref of any of the given repositories (not only branch
//! heads, since an older commit of a branch still restores its files) names
//! a manifest in its `Spawn-Large` trailer, and the manifest and every chunk
//! it lists are live. An object goes only when it is not live and older than
//! the grace period, its age read again just before it is removed: chunks
//! are written before the commit that names them, and a `put` (or a
//! [`ChunkStore::touch`]) makes an object young again.
//!
//! Everything is decided before anything is deleted, and a read error
//! deletes nothing: a repository that cannot be read, or a manifest a
//! reachable commit names that is missing or does not verify, stops the
//! collection with an error that names it. One exception: a manifest that
//! is missing (not corrupt) and that only refs outside `refs/heads/` name
//! (a fetched `refs/spawn-integrate/*`, a mirror's `refs/remotes/*`) is
//! another host's, whose chunk store is not this one; it is skipped and
//! reported ([`Collected::skipped`]), or no collection would ever run on a
//! host that shares branches.
//!
//! Each removal reads the object's age again under the store's exclusive
//! lock ([`ChunkStore::remove_if_older`]), which a `put` or `touch` holds
//! shared, so an object refreshed after the listing stays.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use spawn_core::port::{ChunkError, ChunkStore};

use crate::chunks::{Manifest, trailer_of};

/// What one collection did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Collected {
    /// Objects still in the store.
    pub kept: usize,
    /// Objects removed.
    pub removed: usize,
    /// Bytes the removed objects took.
    pub freed: u64,
    /// Manifests missing here that only refs outside `refs/heads/` name:
    /// another host's, skipped.
    pub skipped: Vec<String>,
}

/// The live set of a collection ([`live_objects`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Live {
    /// Every manifest a reachable commit names and every chunk it lists.
    pub objects: HashSet<String>,
    /// Manifests missing here that only refs outside `refs/heads/` name.
    pub skipped: Vec<String>,
}

/// The repositories whose commits keep objects alive: `default` and every
/// bare repository directly under `<root>/.sources/` (the mission mirrors).
pub fn repositories(default: &Path, root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut repositories = vec![default.to_path_buf()];
    let sources = root.join(".sources");
    let entries = match std::fs::read_dir(&sources) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(repositories),
        Err(error) => return Err(format!("cannot read {}: {error}", sources.display())),
    };
    let mut mirrors = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read {}: {error}", sources.display()))?;
        // The entry's own type: a mirror its owner cannot enter is still a
        // directory, and must be read, not skipped.
        let directory = entry
            .file_type()
            .map_err(|error| format!("cannot read {}: {error}", entry.path().display()))?
            .is_dir();
        if directory {
            mirrors.push(entry.path());
        }
    }
    mirrors.sort();
    repositories.extend(mirrors);
    Ok(repositories)
}

/// Whether `repo` has at least one ref. A collection over a default
/// repository without any (a wrong `--repo`) would find nothing live, so
/// the command refuses it.
pub fn has_refs(repo: &Path) -> Result<bool, String> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(["for-each-ref", "--count=1", "--format=%(refname)"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            format!(
                "cannot read repository {}: run git: {error}",
                repo.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "cannot read repository {}: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(!output.stdout.trim_ascii().is_empty())
}

/// The manifests named by the `Spawn-Large` trailers of every commit
/// reachable from any ref of `repo`.
pub fn manifests_named(repo: &Path) -> Result<BTreeSet<String>, String> {
    trailers(repo, "--all")
}

/// The manifests named by commits reachable from `refs/heads/*` of `repo`.
fn manifests_on_branches(repo: &Path) -> Result<BTreeSet<String>, String> {
    trailers(repo, "--branches")
}

/// The `Spawn-Large` trailers of the commits `git log <revisions>` walks.
fn trailers(repo: &Path, revisions: &str) -> Result<BTreeSet<String>, String> {
    let unreadable = |why: &str| format!("cannot read repository {}: {why}", repo.display());
    if !repo.is_dir() {
        return Err(unreadable("not a directory"));
    }
    // A repository without any ref has no commit to walk, and `log --all`
    // would say so as success; the check above and git's own refusal of an
    // unreadable directory keep every other failure an error.
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(["log", revisions, "-z", "--format=%B"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| unreadable(&format!("run git: {error}")))?;
    if !output.status.success() {
        return Err(unreadable(String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter_map(trailer_of)
        .collect())
}

/// Every object a reachable commit of `repos` can still restore: the
/// manifests and the chunks they list. A manifest that cannot be read, does
/// not verify or does not parse is an error naming it, and so is a missing
/// one a branch (`refs/heads/*`) reaches; a missing one only other refs
/// reach is skipped ([`Live::skipped`]).
pub fn live_objects(store: &dyn ChunkStore, repos: &[PathBuf]) -> Result<Live, String> {
    let mut manifests = BTreeSet::new();
    let mut on_branches = BTreeSet::new();
    for repo in repos {
        manifests.extend(manifests_named(repo)?);
        on_branches.extend(manifests_on_branches(repo)?);
    }
    let mut live = Live::default();
    for name in manifests {
        let bytes = match store.get(&name) {
            Ok(bytes) => bytes,
            Err(ChunkError::Missing(_)) if !on_branches.contains(&name) => {
                live.skipped.push(name);
                continue;
            }
            Err(error) => {
                return Err(format!(
                    "manifest {name} named by a reachable commit: {error}"
                ));
            }
        };
        let manifest = Manifest::from_bytes(&bytes)
            .map_err(|error| format!("manifest {name} named by a reachable commit: {error}"))?;
        for file in manifest.files {
            live.objects.extend(file.chunks);
        }
        live.objects.insert(name);
    }
    Ok(live)
}

/// Remove the objects of `store` that no reachable commit of `repos` names
/// and that are older than `grace`.
pub fn collect(
    store: &dyn ChunkStore,
    repos: &[PathBuf],
    grace: Duration,
) -> Result<Collected, String> {
    // No commit is between its first object and its ref while this is held
    // (they hold `writing` that long): the refs read next name every object
    // a commit wrote, however long it took.
    let _collecting = store
        .collecting()
        .map_err(|error| format!("cannot lock the chunk store: {error}"))?;
    // Decide everything first: any read error returns before a removal.
    let live = live_objects(store, repos)?;
    let objects = store
        .list()
        .map_err(|error| format!("cannot list the chunk store: {error}"))?;
    let doomed: Vec<&str> = objects
        .iter()
        .filter(|object| !live.objects.contains(&object.name) && object.age > grace)
        .map(|object| object.name.as_str())
        .collect();
    let mut collected = Collected {
        skipped: live.skipped,
        ..Collected::default()
    };
    for name in doomed {
        // The age is read again by the store just before the removal.
        if let Some(bytes) = store
            .remove_if_older(name, grace)
            .map_err(|error| format!("remove {name}: {error}"))?
        {
            collected.removed += 1;
            collected.freed += bytes;
        }
    }
    collected.kept = objects.len() - collected.removed;
    Ok(collected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunks::{LargeFile, LocalChunkStore, hash_of, with_trailer};
    use std::time::SystemTime;

    fn scratch(tag: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("spawn-gc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create scratch directory");
        directory
    }

    fn git(repo: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(repo)
            .args(arguments)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@localhost")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@localhost")
            .output()
            .expect("git is installed");
        assert!(output.status.success(), "git {arguments:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn bare(directory: &Path) -> PathBuf {
        let repo = directory.join("repo.git");
        let status = Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&repo)
            .status()
            .expect("git is installed");
        assert!(status.success());
        repo
    }

    /// A commit on `parent` (or a root commit) whose message names `manifest`.
    fn commit(repo: &Path, parent: Option<&str>, manifest: Option<&str>) -> String {
        let tree = git(repo, &["mktree"]);
        let message = with_trailer("checkpoint", manifest);
        let mut arguments = vec!["commit-tree", tree.as_str(), "-m", message.as_str()];
        if let Some(parent) = parent {
            arguments.extend(["-p", parent]);
        }
        git(repo, &arguments)
    }

    /// A manifest of one file over `chunks`, stored; its name.
    fn manifest(store: &LocalChunkStore, chunks: &[&[u8]]) -> (String, Vec<String>) {
        let names: Vec<String> = chunks
            .iter()
            .map(|chunk| store.put(chunk).expect("put chunk"))
            .collect();
        let bytes = Manifest {
            files: vec![LargeFile {
                path: "big.bin".into(),
                mode: 0o100_644,
                size: chunks.iter().map(|chunk| chunk.len() as u64).sum(),
                chunks: names.clone(),
            }],
        }
        .to_bytes();
        (store.put(&bytes).expect("put manifest"), names)
    }

    fn age(store: &LocalChunkStore, name: &str, by: Duration) {
        let path = store.directory().join(&name[..2]).join(name);
        std::fs::File::open(path)
            .expect("open object")
            .set_modified(SystemTime::now() - by)
            .expect("age object");
    }

    const HOUR: Duration = Duration::from_hours(1);

    #[test]
    fn the_live_set_is_every_manifest_a_reachable_commit_names_and_its_chunks() {
        let directory = scratch("live");
        let repo = bare(&directory);
        let store = LocalChunkStore::new(directory.join("store"));
        let (old, old_chunks) = manifest(&store, &[b"old chunk one", b"shared chunk"]);
        let (new, new_chunks) = manifest(&store, &[b"shared chunk", b"new chunk"]);
        let (dropped, dropped_chunks) = manifest(&store, &[b"dropped chunk"]);
        let first = commit(&repo, None, Some(&old));
        let head = commit(&repo, Some(&first), Some(&new));
        let plain = commit(&repo, Some(&head), None);
        git(&repo, &["update-ref", "refs/heads/agent", &plain]);
        // A ref outside refs/heads keeps its commits alive too.
        let other = commit(&repo, None, Some(&dropped));
        git(&repo, &["update-ref", "refs/spawn-integrate/agent", &other]);

        let live = live_objects(&store, std::slice::from_ref(&repo))
            .expect("live set")
            .objects;
        for name in [&old, &new, &dropped]
            .into_iter()
            .chain(&old_chunks)
            .chain(&new_chunks)
            .chain(&dropped_chunks)
        {
            assert!(live.contains(name), "{name} is live");
        }

        // Dropped: its objects are no longer live, and go once old.
        git(&repo, &["update-ref", "-d", "refs/spawn-integrate/agent"]);
        let live = live_objects(&store, std::slice::from_ref(&repo))
            .expect("live set")
            .objects;
        assert!(!live.contains(&dropped) && !live.contains(&dropped_chunks[0]));
        for object in store.list().expect("list") {
            age(&store, &object.name, 2 * HOUR);
        }
        let collected = collect(&store, std::slice::from_ref(&repo), HOUR).expect("collect");
        assert_eq!(collected.removed, 2, "the dropped manifest and its chunk");
        assert!(collected.freed > 0);
        assert!(!store.contains(&dropped).expect("contains"));
        assert!(
            store.contains(&old_chunks[0]).expect("contains"),
            "an older commit's chunk"
        );
        let again = collect(&store, std::slice::from_ref(&repo), HOUR).expect("collect again");
        assert_eq!(
            (again.removed, again.kept),
            (0, collected.kept),
            "nothing new, nothing removed"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn only_objects_older_than_the_grace_period_go() {
        let directory = scratch("grace");
        let repo = bare(&directory);
        let store = LocalChunkStore::new(directory.join("store"));
        let young = store.put(b"young").expect("put");
        let old = store.put(b"old").expect("put");
        age(&store, &young, Duration::from_mins(29));
        age(&store, &old, Duration::from_mins(31));
        let collected =
            collect(&store, std::slice::from_ref(&repo), Duration::from_mins(30)).expect("collect");
        assert_eq!((collected.kept, collected.removed), (1, 1));
        assert!(store.contains(&young).expect("contains"));
        assert!(!store.contains(&old).expect("contains"));
        // The store reads the age again before it removes.
        let refreshed = store.put(b"refreshed").expect("put");
        assert_eq!(
            store.remove_if_older(&refreshed, HOUR).expect("remove"),
            None
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_put_or_touch_of_a_held_object_makes_it_young_again() {
        let directory = scratch("refresh");
        let repo = bare(&directory);
        let store = LocalChunkStore::new(directory.join("store"));
        let name = store.put(b"reused bytes").expect("put");
        age(&store, &name, 2 * HOUR);
        assert_eq!(store.put(b"reused bytes").expect("put again"), name);
        let listed = store.list().expect("list");
        assert!(listed[0].age < Duration::from_mins(1), "{listed:?}");
        let collected = collect(&store, std::slice::from_ref(&repo), HOUR).expect("collect");
        assert_eq!(collected.removed, 0, "a put refreshed it");

        age(&store, &name, 2 * HOUR);
        assert!(store.touch(&name).expect("touch"));
        assert!(store.list().expect("list")[0].age < Duration::from_mins(1));
        assert!(!store.touch(&hash_of(b"never stored")).expect("touch"));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_missing_or_corrupt_reachable_manifest_stops_the_collection() {
        let directory = scratch("refuse");
        let repo = bare(&directory);
        let store = LocalChunkStore::new(directory.join("store"));
        let garbage = store.put(b"unreferenced").expect("put");
        age(&store, &garbage, 2 * HOUR);

        let absent = hash_of(b"a manifest never stored");
        let head = commit(&repo, None, Some(&absent));
        git(&repo, &["update-ref", "refs/heads/agent", &head]);
        let error = collect(&store, std::slice::from_ref(&repo), HOUR).expect_err("missing");
        assert!(error.contains(&absent), "{error}");
        assert!(
            store.contains(&garbage).expect("contains"),
            "nothing deleted"
        );

        let (name, _) = manifest(&store, &[b"chunk"]);
        let head = commit(&repo, None, Some(&name));
        git(&repo, &["update-ref", "refs/heads/agent", &head]);
        let path = store.directory().join(&name[..2]).join(&name);
        let mut bytes = std::fs::read(&path).expect("read manifest");
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xff;
        std::fs::write(&path, bytes).expect("corrupt manifest");
        let error = collect(&store, std::slice::from_ref(&repo), HOUR).expect_err("corrupt");
        assert!(error.contains(&name), "{error}");
        assert!(
            store.contains(&garbage).expect("contains"),
            "nothing deleted"
        );
        assert!(
            store.contains(&name).expect("contains"),
            "never unreferenced"
        );

        let error = collect(&store, &[directory.join("absent.git")], HOUR).expect_err("no repo");
        assert!(error.contains("absent.git"), "{error}");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn another_hosts_manifest_that_only_fetched_refs_name_is_skipped_not_fatal() {
        let directory = scratch("other-host");
        let repo = bare(&directory);
        let store = LocalChunkStore::new(directory.join("store"));
        let garbage = store.put(b"unreferenced").expect("put");
        age(&store, &garbage, 2 * HOUR);
        let (here, here_chunks) = manifest(&store, &[b"this host's chunk"]);
        let head = commit(&repo, None, Some(&here));
        git(&repo, &["update-ref", "refs/heads/agent", &head]);
        // Another host's checkpoint, fetched for landing and mirrored: its
        // manifest lives in that host's store.
        let elsewhere = hash_of(b"a manifest in another host's store");
        let fetched = commit(&repo, Some(&head), Some(&elsewhere));
        git(
            &repo,
            &["update-ref", "refs/spawn-integrate/agent", &fetched],
        );
        git(
            &repo,
            &["update-ref", "refs/remotes/source/spawn/agent", &fetched],
        );

        let collected = collect(&store, std::slice::from_ref(&repo), HOUR).expect("collect");
        assert_eq!(collected.skipped, vec![elsewhere.clone()]);
        assert_eq!(collected.removed, 1, "the unreferenced object went");
        assert!(!store.contains(&garbage).expect("contains"));
        assert!(store.contains(&here).expect("contains"));
        assert!(store.contains(&here_chunks[0]).expect("contains"));

        // A branch of this host naming it: this host must have it.
        git(&repo, &["update-ref", "refs/heads/adopted", &fetched]);
        let error = collect(&store, std::slice::from_ref(&repo), HOUR).expect_err("missing");
        assert!(error.contains(&elsewhere), "{error}");
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A writer that refreshes an object after the collection read its age
    /// (it holds the store's lock shared, as a `put` or `touch` does) keeps
    /// the object: the removal waits for it and reads the age again.
    #[test]
    fn an_object_refreshed_while_its_removal_waits_stays() {
        let directory = scratch("race");
        let store = LocalChunkStore::new(directory.join("store"));
        let name = store.put(b"reused by a checkpoint in flight").expect("put");
        age(&store, &name, 2 * HOUR);
        let writer = std::fs::File::open(store.directory()).expect("the store's directory");
        writer.lock_shared().expect("hold it as a writer");
        let remover = {
            let store = store.clone();
            let name = name.clone();
            std::thread::spawn(move || store.remove_if_older(&name, HOUR).expect("remove"))
        };
        std::thread::sleep(Duration::from_millis(300));
        // The writer's refresh, between the collection's listing and the
        // removal's unlink.
        std::fs::File::open(store.directory().join(&name[..2]).join(&name))
            .expect("still there while the writer holds the lock")
            .set_modified(SystemTime::now())
            .expect("refresh");
        writer.unlock().expect("release");
        assert_eq!(remover.join().expect("the removal"), None, "young again");
        assert!(store.contains(&name).expect("contains"));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_mirrors_under_the_root_are_collected_with_the_default_repository() {
        let directory = scratch("mirrors");
        let root = directory.join("ws");
        std::fs::create_dir_all(root.join(".sources/one.git")).expect("mirror");
        std::fs::create_dir_all(root.join(".sources/two.git")).expect("mirror");
        std::fs::write(root.join(".sources/note"), "not a repository").expect("file");
        let found = repositories(&directory.join("default.git"), &root).expect("repositories");
        assert_eq!(
            found,
            vec![
                directory.join("default.git"),
                root.join(".sources/one.git"),
                root.join(".sources/two.git"),
            ]
        );
        let none = repositories(&directory.join("default.git"), &directory.join("nothing"))
            .expect("no mirrors");
        assert_eq!(none, vec![directory.join("default.git")]);
        let _ = std::fs::remove_dir_all(&directory);
    }
}
