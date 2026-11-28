//! No history from before a rewrite written back into a ref (objective
//! 177). After the owner rewrites a source's history and loads the commit
//! map (objective 173), the readers read a recorded commit through the map;
//! an id the map cannot translate (a dropped commit's, or an old object no
//! ref reaches) reads as itself. Three writers put recorded ids into refs:
//! the set-aside of a cancelled task's work (to its start), the fork of an
//! earlier task's work, and the re-push of a lost checkpoint. Each asks
//! [`why_old`] first and writes nothing of an id it refuses; [`refused`]
//! records the refusal once, as an open alert a person judges.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::json;
use spawn_postgres::PostgresStore;

use crate::sources;

/// Why `commit` must not be written into a ref of the repository at
/// `git_dir`, if it must not: with a commit map loaded, the map names it as
/// a commit the rewrite changed or dropped, it is no commit of the
/// repository, or its history (whatever ref holds it, objective 194) holds
/// a commit the rewrite changed or dropped. A commit the rewrite kept
/// (listed as itself) is history after the rewrite too. With no map loaded
/// nothing is refused.
pub(crate) fn why_old(
    store: &PostgresStore,
    git_dir: &str,
    commit: &str,
) -> Result<Option<String>, String> {
    why_old_walked(store, git_dir, commit).map(|(reason, _)| reason)
}

/// How many proofs of each kind a repository keeps (objective 194).
const PROOFS_KEPT: usize = 64;

/// What one repository (git dir) is known to hold, for this process's life
/// (commits never change): the old commits of the map (changed or dropped;
/// the map is insert-once), the last commits proven clean (a clean commit's
/// whole history is clean, so they bound every walk) and the last proven
/// old, with their reason.
#[derive(Default)]
struct Proofs {
    olds: Option<Arc<HashSet<String>>>,
    clean: VecDeque<String>,
    old: VecDeque<(String, String)>,
}

fn proofs() -> &'static Mutex<HashMap<String, Proofs>> {
    static PROOFS: OnceLock<Mutex<HashMap<String, Proofs>>> = OnceLock::new();
    PROOFS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_proofs<T>(git_dir: &str, act: impl FnOnce(&mut Proofs) -> T) -> T {
    let mut all = proofs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    act(all.entry(git_dir.to_string()).or_default())
}

/// [`why_old`], and how many commits its history walk read (0: answered
/// from a proof, or before any walk).
fn why_old_walked(
    store: &PostgresStore,
    git_dir: &str,
    commit: &str,
) -> Result<(Option<String>, usize), String> {
    let error = |error: spawn_core::port::StoreError| error.to_string();
    if store.commit_map_size().map_err(error)? == 0 {
        return Ok((None, 0));
    }
    match store.commit_map_entry(commit).map_err(error)? {
        Some(None) => {
            return Ok((
                Some(format!(
                    "the commit {commit} was dropped by the history rewrite"
                )),
                0,
            ));
        }
        Some(Some(new)) if !new.eq_ignore_ascii_case(commit) => {
            return Ok((
                Some(format!(
                    "the commit {commit} is from before the history rewrite (now {new})"
                )),
                0,
            ));
        }
        _ => {}
    }
    let known = sources::git(&[
        "--git-dir",
        git_dir,
        "cat-file",
        "-e",
        &format!("{commit}^{{commit}}"),
    ])
    .is_ok();
    if !known {
        return Ok((
            Some(format!(
                "the commit {commit} is an unknown commit (in neither the commit map nor the \
                 repository)"
            )),
            0,
        ));
    }
    walk_history(store, git_dir, commit)
}

/// The third check of [`why_old`] on a commit of the repository: its whole
/// history, walked against the commits proven clean, or its proof.
fn walk_history(
    store: &PostgresStore,
    git_dir: &str,
    commit: &str,
) -> Result<(Option<String>, usize), String> {
    let id = commit.to_ascii_lowercase();
    let (olds, clean, proven) = with_proofs(git_dir, |proofs| {
        let proven = if proofs.clean.contains(&id) {
            Some(None)
        } else {
            proofs
                .old
                .iter()
                .find(|(old, _)| *old == id)
                .map(|(_, reason)| Some(reason.clone()))
        };
        (proofs.olds.clone(), proofs.clean.clone(), proven)
    });
    if let Some(proven) = proven {
        return Ok((proven, 0));
    }
    let olds = if let Some(olds) = olds {
        olds
    } else {
        let olds = Arc::new(store.commit_map_olds().map_err(|error| error.to_string())?);
        with_proofs(git_dir, |proofs| proofs.olds = Some(olds.clone()));
        olds
    };
    let walk = |clean: &VecDeque<String>| {
        let mut arguments = vec!["--git-dir", git_dir, "rev-list", commit];
        if !clean.is_empty() {
            arguments.push("--not");
            arguments.extend(clean.iter().map(String::as_str));
        }
        sources::git(&arguments)
    };
    // A clean commit the repository lost since (a gc) fails the walk: it is
    // made again from nothing, and the clean proofs start over.
    let history = match walk(&clean) {
        Ok(history) => history,
        Err(_) if !clean.is_empty() => {
            with_proofs(git_dir, |proofs| proofs.clean.clear());
            walk(&VecDeque::new()).map_err(|error| format!("the history of {commit}: {error}"))?
        }
        Err(error) => return Err(format!("the history of {commit}: {error}")),
    };
    let walked: Vec<&str> = history
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let old: Vec<&str> = walked
        .iter()
        .copied()
        .filter(|line| olds.contains(&line.to_ascii_lowercase()))
        .collect();
    let reason = old.first().map(|first| {
        format!(
            "the commit {commit} is built on the history before the rewrite: {} commit(s) of its \
             history were changed or dropped by it (the first: {first})",
            old.len()
        )
    });
    with_proofs(git_dir, |proofs| match &reason {
        None => {
            proofs.clean.push_back(id);
            while proofs.clean.len() > PROOFS_KEPT {
                proofs.clean.pop_front();
            }
        }
        Some(reason) => {
            proofs.old.push_back((id, reason.clone()));
            while proofs.old.len() > PROOFS_KEPT {
                proofs.old.pop_front();
            }
        }
    });
    Ok((reason, walked.len()))
}

/// Record that `writer` refused `commit` for `task` (`reason`, from
/// [`why_old`]): once, as an open alert keyed
/// `rewrite_refused:<writer>:<task>:<commit>`, which a person sees in Needs
/// you and judges. A refusal recorded already is not recorded again.
pub(crate) fn refused(
    store: &PostgresStore,
    writer: &str,
    task: i64,
    commit: &str,
    reason: &str,
) -> Result<(), String> {
    let key = format!("rewrite_refused:{writer}:{task}:{commit}");
    match store.alert_record(&json!({
        "type": "alert.fired", "key": key, "kind": "rewrite_refused",
        "subject": format!("task #{task}"),
        "evidence": {"writer": writer, "task": task, "commit": commit, "reason": reason},
        "text": format!("task #{task}: {writer} refused: {reason}; it went on from main"),
        "notice": false,
    })) {
        Ok(_) => Ok(()),
        Err(error) if error.to_string().contains("was already fired") => Ok(()),
        Err(error) => Err(format!("record the refusal {key}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use super::{why_old, why_old_walked};
    use crate::sources::test_git as git;

    fn commit_tree(repo: &str, tree: &str, parent: Option<&str>, message: &str) -> String {
        let mut arguments = vec![
            "--git-dir",
            repo,
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit-tree",
            tree,
            "-m",
            message,
        ];
        if let Some(parent) = parent {
            arguments.extend(["-p", parent]);
        }
        git(&arguments).expect("commit-tree")
    }

    /// No map: nothing refused. With one: a changed commit, a dropped one, a
    /// commit made on the old history (no ref reaches it) and one the
    /// repository lacks are refused, each saying why; a new commit and one
    /// the rewrite kept are not.
    #[test]
    fn a_commit_is_refused_when_it_or_its_unreferenced_history_is_from_before_the_rewrite() {
        let schema = spawn_postgres::testing::TestSchema::new("rewrite_guard");
        let store = spawn_postgres::PostgresStore::connect_in_schema_with_options(
            schema.url(),
            &schema,
            &spawn_postgres::testing::test_connect_options(),
        )
        .expect("connect");
        let base = std::env::temp_dir().join(format!("spawn-rewrite-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo.git").to_string_lossy().to_string();
        git(&["init", "-q", "--bare", &repo]).expect("init");
        let empty = git(&[
            "--git-dir",
            &repo,
            "hash-object",
            "-t",
            "tree",
            "-w",
            "/dev/null",
        ])
        .expect("the empty tree");
        let m0 = commit_tree(&repo, &empty, None, "m0");
        let m1 = commit_tree(&repo, &empty, Some(&m0), "m1");
        let e = commit_tree(&repo, &empty, Some(&m1), "e");
        let m1n = commit_tree(&repo, &empty, Some(&m0), "m1 (rewritten)");
        let on_old = commit_tree(&repo, &empty, Some(&e), "made on the old history");
        let new = commit_tree(&repo, &empty, Some(&m1n), "made on the new history");
        git(&["--git-dir", &repo, "update-ref", "refs/heads/main", &m1n]).expect("main");

        assert_eq!(
            why_old(&store, &repo, &m1),
            Ok(None),
            "no map: nothing refused"
        );

        store
            .commit_map_load(&[
                (m0.clone(), Some(m0.clone())),
                (m1.clone(), Some(m1n.clone())),
                (e.clone(), None),
            ])
            .expect("the map");
        assert_eq!(
            why_old(&store, &repo, &m1),
            Ok(Some(format!(
                "the commit {m1} is from before the history rewrite (now {m1n})"
            )))
        );
        assert_eq!(
            why_old(&store, &repo, &e),
            Ok(Some(format!(
                "the commit {e} was dropped by the history rewrite"
            )))
        );
        assert_eq!(
            why_old(&store, &repo, &on_old),
            Ok(Some(format!(
                "the commit {on_old} is built on the history before the rewrite: 2 commit(s) of \
                 its history were changed or dropped by it (the first: {e})"
            )))
        );
        let absent = "1".repeat(40);
        assert_eq!(
            why_old(&store, &repo, &absent),
            Ok(Some(format!(
                "the commit {absent} is an unknown commit (in neither the commit map nor the \
                 repository)"
            )))
        );
        assert_eq!(why_old(&store, &repo, &new), Ok(None), "new history");
        assert_eq!(why_old(&store, &repo, &m0), Ok(None), "kept by the rewrite");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The mirror's refs reach the old history and the resident's
    /// checkpoint built on it: the checkpoint is refused all the same, and a
    /// commit on the new history is not. The walk is incremental: a first
    /// walk reads the whole history, a child of a clean commit itself alone,
    /// a commit proven once nothing, and an old-history commit only what the
    /// clean proofs do not reach, then nothing.
    #[test]
    fn a_commit_built_on_the_old_history_is_refused_whatever_ref_holds_it_walked_once() {
        let schema = spawn_postgres::testing::TestSchema::new("rewrite_guard_walk");
        let store = spawn_postgres::PostgresStore::connect_in_schema_with_options(
            schema.url(),
            &schema,
            &spawn_postgres::testing::test_connect_options(),
        )
        .expect("connect");
        let base = std::env::temp_dir().join(format!("spawn-rewrite-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mirror = base.join("mirror.git").to_string_lossy().to_string();
        let own = base.join("own.git").to_string_lossy().to_string();
        let empty_tree = |repo: &str| {
            git(&["init", "-q", "--bare", repo]).expect("init");
            git(&[
                "--git-dir",
                repo,
                "hash-object",
                "-t",
                "tree",
                "-w",
                "/dev/null",
            ])
            .expect("the empty tree")
        };
        let empty = empty_tree(&mirror);
        let m0 = commit_tree(&mirror, &empty, None, "m0");
        let m1 = commit_tree(&mirror, &empty, Some(&m0), "m1");
        let w = commit_tree(&mirror, &empty, Some(&m1), "w");
        let m1n = commit_tree(&mirror, &empty, Some(&m0), "m1 (rewritten)");
        let wn = commit_tree(&mirror, &empty, Some(&m1n), "w (rewritten)");
        let c = commit_tree(&mirror, &empty, Some(&w), "the resident's checkpoint");
        let n = commit_tree(&mirror, &empty, Some(&wn), "on the new history");
        for (name, at) in [
            ("refs/heads/main", &m1n),
            ("refs/heads/old-main", &m1),
            ("refs/spawn-integrate/resident", &w),
            ("refs/heads/resident", &c),
        ] {
            git(&["--git-dir", &mirror, "update-ref", name, at]).expect("a ref");
        }

        let empty = empty_tree(&own);
        let r0 = commit_tree(&own, &empty, None, "r0");
        let r1 = commit_tree(&own, &empty, Some(&r0), "r1");
        let r2 = commit_tree(&own, &empty, Some(&r1), "r2");
        let r3 = commit_tree(&own, &empty, Some(&r2), "r3");
        let r4 = commit_tree(&own, &empty, Some(&r3), "r4");
        let o1 = commit_tree(&own, &empty, Some(&r0), "o1 (old)");
        let o2 = commit_tree(&own, &empty, Some(&o1), "o2");
        let x = commit_tree(&own, &empty, Some(&o2), "x, on the old history");

        store
            .commit_map_load(&[
                (m0.clone(), Some(m0.clone())),
                (m1.clone(), Some(m1n.clone())),
                (w.clone(), Some(wn.clone())),
                (o1.clone(), None),
            ])
            .expect("the map");
        assert_eq!(
            why_old(&store, &mirror, &c),
            Ok(Some(format!(
                "the commit {c} is built on the history before the rewrite: 2 commit(s) of its \
                 history were changed or dropped by it (the first: {w})"
            ))),
            "the checkpoint on the old history, whatever ref holds it"
        );
        assert_eq!(why_old(&store, &mirror, &n), Ok(None), "the new history");

        let walked = |commit: &str| why_old_walked(&store, &own, commit).expect("why_old");
        assert_eq!(walked(&r3), (None, 4), "a first walk: the whole history");
        assert_eq!(walked(&r4), (None, 1), "a child of a clean commit: itself");
        assert_eq!(walked(&r4), (None, 0), "proven once: no walk");
        assert_eq!(walked(&r2), (None, 0), "in a clean commit's history");
        let reason = format!(
            "the commit {x} is built on the history before the rewrite: 1 commit(s) of its \
             history were changed or dropped by it (the first: {o1})"
        );
        assert_eq!(
            walked(&x),
            (Some(reason.clone()), 3),
            "only what the clean proofs do not reach"
        );
        assert_eq!(walked(&x), (Some(reason), 0), "proven old: no walk");
        let _ = std::fs::remove_dir_all(&base);
    }
}
