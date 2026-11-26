//! Target path grammar and identifier validation for `spawn-node` v1.

use std::fmt;

use super::Error;

/// Every target starts with this.
pub const PATH_PREFIX: &str = "/node/v1/";
/// The longest path part after [`PATH_PREFIX`].
pub const MAX_PATH_TAIL: usize = 220;
/// The longest query after `?`.
pub const MAX_QUERY: usize = 200;

/// Checks a request target (path and query, exactly as sent) against
/// rule 3: `^/node/v1/[A-Za-z0-9._:/-]{1,220}(\?[A-Za-z0-9._=&-]{1,200})?$`,
/// no `%`, no `//`, no `.` or `..` segment.
pub fn check_target(target: &str) -> Result<(), Error> {
    let tail = target.strip_prefix(PATH_PREFIX).ok_or(Error::Target)?;
    let (path, query) = match tail.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (tail, None),
    };
    let path_ok = (1..=MAX_PATH_TAIL).contains(&path.len())
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'/' | b'-'));
    let query_ok = query.is_none_or(|query| {
        (1..=MAX_QUERY).contains(&query.len())
            && query
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'=' | b'&' | b'-'))
    });
    // The prefix ends in `/`, so a tail starting with `/` makes a `//`.
    let segments_ok = !path.starts_with('/')
        && !path.contains("//")
        && path
            .split('/')
            .all(|segment| segment != "." && segment != "..");
    if path_ok && query_ok && segments_ok {
        Ok(())
    } else {
        Err(Error::Target)
    }
}

/// A raw target with the gateway's `--public-prefix` removed; `None` when
/// the target does not start with it.
pub fn strip_public_prefix<'a>(target: &'a str, prefix: &str) -> Option<&'a str> {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return Some(target);
    }
    target
        .strip_prefix(prefix)
        .filter(|rest| rest.starts_with('/'))
}

/// A job id as a path segment: 1 to 128 of `A-Z a-z 0-9 . _ -`, not `.` or
/// `..`.
pub fn is_job_id(text: &str) -> bool {
    (1..=128).contains(&text.len())
        && text != "."
        && text != ".."
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The gateway-relative URL of an artifact (`/node/v1/artifacts/<digest>`).
pub fn artifact_path(digest: &impl fmt::Display) -> String {
    format!("{PATH_PREFIX}artifacts/{digest}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_grammar_refuses_encoding_dot_segments_and_oversized_parts() {
        for good in [
            "/node/v1/heartbeat",
            "/node/v1/artifacts/sha256:00",
            "/node/v1/jobs/gate-0123_ab.c/result?a=1&b=2",
            "/node/v1/a..b",
        ] {
            check_target(good).unwrap();
        }
        let long = format!("/node/v1/{}", "a".repeat(221));
        let long_query = format!("/node/v1/a?{}", "b".repeat(201));
        for bad in [
            "/node/v1/",
            "/node/v2/heartbeat",
            "/node/v1/artifacts/sha256%3Aab",
            "/node/v1//heartbeat",
            "/node/v1/a//b",
            "/node/v1/./spec",
            "/node/v1/a/../spec",
            "/node/v1/a/..",
            "/node/v1/heart beat",
            "/node/v1/a?",
            "/node/v1/a?b?c",
            "/node/v1/a?b/c",
            "/node/v1/a#b",
            "node/v1/heartbeat",
            long.as_str(),
            long_query.as_str(),
        ] {
            assert_eq!(check_target(bad), Err(Error::Target), "{bad}");
        }
        assert_eq!(
            check_target(&format!("/node/v1/{}", "a".repeat(220))),
            Ok(())
        );
    }

    #[test]
    fn the_public_prefix_is_stripped_only_whole() {
        assert_eq!(
            strip_public_prefix("/x/node/v1/spec", "/x"),
            Some("/node/v1/spec")
        );
        assert_eq!(
            strip_public_prefix("/x/node/v1/spec", "/x/"),
            Some("/node/v1/spec")
        );
        assert_eq!(strip_public_prefix("/xy/node/v1/spec", "/x"), None);
        assert_eq!(
            strip_public_prefix("/node/v1/spec", ""),
            Some("/node/v1/spec")
        );
    }
}
