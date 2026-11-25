//! The two read-only routes of file provenance (PV2, docs/provenance.md),
//! read through the reader login like every other read, from the database
//! alone (`crate::provenance` has the answers; a line's blame needs the
//! repository and is `spawn prov who`'s only):
//!
//! - `/api/prov/who?path=<path>[&source=<source>]`: who made the path as
//!   the last recorded first-parent commit that touched it left it: that
//!   `land` or `import` Change, its entry, the revision the content came
//!   from (`origin`) and the path's whole history, oldest first. `source`
//!   narrows it to the domain `git:<source>`. 400 without a path, or with
//!   a `line` (a line needs the repository).
//! - `/api/prov/change/<id>`: what `spawn prov show <id>` prints: every
//!   revision of a Change (`c` and 26 base32 characters), every Change of a
//!   task (digits) or the Change recorded for a commit (40 hex characters,
//!   read through the commit map both ways), each with its entries and
//!   edges; `{changes: []}` when none is recorded. 400 for anything else.

use serde_json::Value;

use super::{BAD_REQUEST, open_store, parameter, percent_decoded};
use crate::provenance;

/// `/api/prov/who`.
pub(super) fn who(query: &str) -> Result<Value, String> {
    let decoded = |name: &str| {
        parameter(query, name)
            .map(|text| {
                percent_decoded(text).ok_or_else(|| format!("{BAD_REQUEST}bad {name} {text}"))
            })
            .transpose()
    };
    if parameter(query, "line").is_some() {
        return Err(format!(
            "{BAD_REQUEST}a line's provenance needs the repository: spawn prov who <path>:<line>"
        ));
    }
    let path = decoded("path")?
        .filter(|path| !path.is_empty())
        .ok_or_else(|| format!("{BAD_REQUEST}?path= is required"))?;
    let domain = decoded("source")?.map(|source| provenance::index::domain_of(&source));
    let store = open_store()?;
    provenance::who_recorded(&store, domain.as_deref(), &path)
}

/// `/api/prov/change/<id>`.
pub(super) fn change(id: &str) -> Result<Value, String> {
    let target = provenance::target(id).map_err(|error| format!("{BAD_REQUEST}{error}"))?;
    let store = open_store()?;
    provenance::show(&store, &target)
}

#[cfg(test)]
mod tests {
    use super::{BAD_REQUEST, change, who};

    #[test]
    fn a_bad_request_is_refused_before_the_store_is_opened() {
        for query in ["", "path=", "path=a.rs&line=3", "line=1"] {
            let error = who(query).expect_err(query);
            assert!(error.starts_with(BAD_REQUEST), "{query}: {error}");
        }
        for id in ["x", "abc", "c123", "12345678901234567890", "g".repeat(40).as_str()] {
            let error = change(id).expect_err(id);
            assert!(error.starts_with(BAD_REQUEST), "{id}: {error}");
        }
    }
}
