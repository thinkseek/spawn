//! No history from before a rewrite written back into a ref (objective
//! 177). After the owner rewrites a source's history and loads the commit
//! map (objective 173), the readers read a recorded commit through the map;
//! an id the map cannot translate (a dropped commit's, or an old object no
//! ref reaches) reads as itself. Three writers put recorded ids into refs:
//! the set-aside of a cancelled task's work (to its start), the fork of an
//! earlier task's work, and the re-push of a lost checkpoint. Each asks
//! [`why_old`] first and writes nothing of an id it refuses; [`refused`]
//! records the refusal once, as an open alert a person judges.

use serde_json::json;
use spawn_postgres::PostgresStore;

use crate::sources;

/// Why `commit` must not be written into a ref of the repository at
/// `git_dir`, if it must not: with a commit map loaded, the map names it as
/// a commit the rewrite changed or dropped, it is no commit of the
/// repository, or its history outside every ref of the repository holds a
/// commit the rewrite changed or dropped. A commit the rewrite kept (listed
/// as itself) is history after the rewrite too. With no map loaded nothing
/// is refused.
pub(crate) fn why_old(
    store: &PostgresStore,
    git_dir: &str,
    commit: &str,
) -> Result<Option<String>, String> {
    let error = |error: spawn_core::port::StoreError| error.to_string();
    if store.commit_map_size().map_err(error)? == 0 {
        return Ok(None);
    }
    match store.commit_map_entry(commit).map_err(error)? {
        Some(None) => {
            return Ok(Some(format!(
                "the commit {commit} was dropped by the history rewrite"
            )));
        }
        Some(Some(new)) if !new.eq_ignore_ascii_case(commit) => {
            return Ok(Some(format!(
                "the commit {commit} is from before the history rewrite (now {new})"
            )));
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
        return Ok(Some(format!(
            "the commit {commit} is an unknown commit (in neither the commit map nor the \
             repository)"
        )));
    }
    let outside = sources::git(&["--git-dir", git_dir, "rev-list", commit, "--not", "--all"])
        .map_err(|error| format!("the history of {commit} outside every ref: {error}"))?;
    let olds = store.commit_map_olds().map_err(error)?;
    let old: Vec<&str> = outside
        .lines()
        .map(str::trim)
        .filter(|line| olds.contains(&line.to_ascii_lowercase()))
        .collect();
    Ok(old.first().map(|first| {
        format!(
            "the commit {commit} would bring {} commit(s) from before the history rewrite (the \
             first: {first})",
            old.len()
        )
    }))
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
    use super::why_old;
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
        let store = spawn_postgres::PostgresStore::connect_in_schema(schema.url(), &schema)
            .expect("connect");
        let base = std::env::temp_dir().join(format!("spawn-rewrite-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo.git").to_string_lossy().to_string();
        git(&["init", "-q", "--bare", &repo]).expect("init");
        let empty = git(&["--git-dir", &repo, "hash-object", "-t", "tree", "-w", "/dev/null"])
            .expect("the empty tree");
        let m0 = commit_tree(&repo, &empty, None, "m0");
        let m1 = commit_tree(&repo, &empty, Some(&m0), "m1");
        let e = commit_tree(&repo, &empty, Some(&m1), "e");
        let m1n = commit_tree(&repo, &empty, Some(&m0), "m1 (rewritten)");
        let on_old = commit_tree(&repo, &empty, Some(&e), "made on the old history");
        let new = commit_tree(&repo, &empty, Some(&m1n), "made on the new history");
        git(&["--git-dir", &repo, "update-ref", "refs/heads/main", &m1n]).expect("main");

        assert_eq!(why_old(&store, &repo, &m1), Ok(None), "no map: nothing refused");

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
            Ok(Some(format!("the commit {e} was dropped by the history rewrite")))
        );
        assert_eq!(
            why_old(&store, &repo, &on_old),
            Ok(Some(format!(
                "the commit {on_old} would bring 2 commit(s) from before the history rewrite \
                 (the first: {e})"
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
}
