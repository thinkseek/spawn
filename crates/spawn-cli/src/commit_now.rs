//! A recorded commit id as it is now (objective 178): the Rust readers that
//! compare an id recorded before a history rewrite with git read it through
//! the commit map (objective 173). The map's new commit for one it names; a
//! commit the rewrite dropped says so; an id in neither the map nor the
//! repository is an unknown commit, never "behind", "does not descend" or a
//! mismatch.

use std::path::Path;
use std::process::Stdio;

use spawn_core::port::StoreError;
use spawn_postgres::PostgresStore;

/// One recorded commit id, read through the commit map and the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommitNow {
    /// The commit as it is now: the map's new id, or the id itself when the
    /// map does not name it and the repository holds it.
    Commit(String),
    /// The map names it with no new commit: the rewrite dropped it.
    Dropped,
    /// In neither the map nor the repository.
    Unknown,
}

impl CommitNow {
    /// `commit` read through the map's `entry` for it
    /// ([`PostgresStore::commit_map_entry`]); `held` (whether the repository
    /// holds `commit` as a commit) is asked only when the map does not name
    /// it.
    pub(crate) fn of(
        commit: &str,
        entry: Option<Option<String>>,
        held: impl FnOnce() -> bool,
    ) -> Self {
        match entry {
            Some(Some(new)) => Self::Commit(new),
            Some(None) => Self::Dropped,
            None if held() => Self::Commit(commit.to_string()),
            None => Self::Unknown,
        }
    }

    /// The id, or why there is none: "<what> <commit> was dropped by the
    /// history rewrite (…)" / "<what> <commit> is an unknown commit (…)".
    pub(crate) fn id_or_why(self, what: &str, commit: &str) -> Result<String, String> {
        match self {
            Self::Commit(id) => Ok(id),
            Self::Dropped => Err(format!(
                "{what} {commit} was dropped by the history rewrite (the commit map names no new \
                 commit for it)"
            )),
            Self::Unknown => Err(format!(
                "{what} {commit} is an unknown commit (in neither the commit map nor the source)"
            )),
        }
    }
}

/// `commit` as it is now: through `store`'s commit map, else the id itself
/// when the repository at `repository` holds it as a commit (`git cat-file
/// -e <id>^{commit}`, read only). An id that begins with '-' is never given
/// to git: it is no commit.
pub(crate) fn commit_now(
    store: &PostgresStore,
    repository: &Path,
    commit: &str,
) -> Result<CommitNow, StoreError> {
    let entry = store.commit_map_entry(commit)?;
    Ok(CommitNow::of(commit, entry, || held(repository, commit)))
}

/// Whether the repository at `repository` holds `commit` as a commit.
pub(crate) fn held(repository: &Path, commit: &str) -> bool {
    if commit.is_empty() || commit.starts_with('-') {
        return false;
    }
    crate::sources::controller_git()
        .arg("-C")
        .arg(repository)
        .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::CommitNow;
    use std::cell::Cell;

    #[test]
    fn a_mapped_id_is_its_new_one_without_asking_the_repository() {
        let asked = Cell::new(false);
        let now = CommitNow::of("old", Some(Some("new".to_string())), || {
            asked.set(true);
            true
        });
        assert_eq!(now, CommitNow::Commit("new".to_string()));
        assert!(!asked.get(), "the repository was asked");
    }

    #[test]
    fn a_dropped_id_is_dropped_even_when_the_repository_still_holds_it() {
        assert_eq!(CommitNow::of("old", Some(None), || true), CommitNow::Dropped);
    }

    #[test]
    fn an_unmapped_id_is_itself_when_held_else_unknown() {
        assert_eq!(
            CommitNow::of("abc", None, || true),
            CommitNow::Commit("abc".to_string())
        );
        assert_eq!(CommitNow::of("abc", None, || false), CommitNow::Unknown);
    }

    #[test]
    fn the_dropped_and_unknown_sentences_word_for_word() {
        assert_eq!(
            CommitNow::Commit("n".to_string()).id_or_why("--from", "o"),
            Ok("n".to_string())
        );
        assert_eq!(
            CommitNow::Dropped.id_or_why("--from", "e1"),
            Err("--from e1 was dropped by the history rewrite (the commit map names no new \
                 commit for it)"
                .to_string())
        );
        assert_eq!(
            CommitNow::Unknown.id_or_why("the controller's commit", "u1"),
            Err("the controller's commit u1 is an unknown commit (in neither the commit map \
                 nor the source)"
                .to_string())
        );
    }

    #[test]
    fn an_id_like_an_option_is_never_held() {
        assert!(!super::held(std::path::Path::new("."), "--all"));
        assert!(!super::held(std::path::Path::new("."), ""));
    }
}
