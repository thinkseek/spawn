//! The commit map of a history rewrite (objective 173): the owner loads git
//! filter-repo's commit-map once (`spawn commit-map load`, through the
//! owner-only `spawn_commit_map_load`), and every reader that compares a
//! recorded commit id with git reads it through `spawn_commit_now`. The map
//! is never changed: a statement trigger refuses every update, delete and
//! truncate of `spawn_commit_map`.

use std::collections::HashSet;

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;

impl PostgresStore {
    /// Load the commit map `pairs` (owner only): each `(old, new)`, `new`
    /// `None` for a commit the rewrite dropped. On an empty map it inserts
    /// them; given exactly the pairs loaded already it changes nothing.
    /// Returns how many pairs the map holds, and whether this load put them
    /// there (`false`: they were loaded already). A different map, a
    /// malformed or repeated old and an empty list are refused.
    pub fn commit_map_load(&self, pairs: &[(String, Option<String>)]) -> Result<(i64, bool), StoreError> {
        let before = self.commit_map_size()?;
        let pairs = Value::Array(
            pairs
                .iter()
                .map(|(old, new)| serde_json::json!([old, new]))
                .collect(),
        );
        let loaded: i32 = self
            .query_one("select spawn_commit_map_load($1)", &[&pairs])?
            .get(0);
        Ok((i64::from(loaded), before == 0))
    }

    /// `commit` as it is now (`spawn_commit_now`): the map's new commit for
    /// one it names (any case), `None` for one the rewrite dropped, and
    /// `commit` itself otherwise.
    pub fn commit_now(&self, commit: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .query_one("select spawn_commit_now($1::text)", &[&commit])?
            .get(0))
    }

    /// How many pairs the commit map holds (0: none loaded).
    pub fn commit_map_size(&self) -> Result<i64, StoreError> {
        Ok(self
            .query_one("select count(*) from spawn_commit_map", &[])?
            .get(0))
    }

    /// The map's entry for `old` (any case): `None` when the map does not
    /// name it, `Some(None)` for a commit the rewrite dropped, and
    /// `Some(Some(new))` otherwise.
    pub fn commit_map_entry(&self, old: &str) -> Result<Option<Option<String>>, StoreError> {
        Ok(self
            .query_opt(
                "select new from spawn_commit_map where old = lower($1::text)",
                &[&old],
            )?
            .map(|row| row.get(0)))
    }

    /// The old commits the rewrite changed or dropped: never one it kept
    /// (`new` the same commit). None of them may come onto main again.
    pub fn commit_map_olds(&self) -> Result<HashSet<String>, StoreError> {
        Ok(self
            .query(
                "select old from spawn_commit_map where new is distinct from old",
                &[],
            )?
            .iter()
            .map(|row| row.get(0))
            .collect())
    }
}
