//! Leaked test schemas (objective 094): the schemas of the test databases
//! that are the leftovers of a test that is gone.
//!
//! A schema (not `public`, not the system's) is leaked only when all three
//! hold:
//! - no live session holds it: no session's `application_name` is its name
//!   (a test's store names its session after its schema,
//!   `PostgresStore::connect_in_schema_with_options`);
//! - its owner's pid is gone: the `<pid>` of `spawn_test_<pid>_...` is no
//!   process on this host (a pid that runs counts as alive: in a gate's PID
//!   namespace that can only under-count); a name with no pid has no owner;
//! - it has been idle 24 h: made at least 24 h ago by its comment
//!   `{"made_at_ms": <ms>}`, or, with no comment or one that does not read
//!   so, first seen at least 24 h ago by this reader, which keeps its first
//!   sights in `<root>/test-schemas-seen.json` (`{"<server>/<schema>": <first
//!   seen ms>}`): a schema is added when first seen, a first sight never
//!   moves, and a schema no longer there is dropped (a server that did not
//!   answer keeps its entries).
//!
//! The test servers: the env's `SPAWN_TEST_DATABASE_URL` ("test") and
//! `SPAWN_FLEET_TEST_DATABASE_URL` ("fleet_test"), then the Core lines
//! file's `storage.test_database_url_files` [{name, url_file}]. Agents can
//! write that file, so a URL file is used only when this user owns it as a
//! regular file at mode 0600, its URL names a local socket or a loopback
//! host, and it does not name the live database (`SPAWN_DATABASE_URL`),
//! compared by the database each names, never by text ([`url_from_file`]).
//! No URL is ever printed.
//!
//! Read only: nothing here drops, alters or comments on a schema.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// How long a schema must have been idle to be a leftover.
const IDLE_MS: i64 = 86_400_000;

/// The seen file's name under the workspace root.
/// (The workspaces' size leaves it out, `crate::storage::workspaces`.)
pub(super) const SEEN_FILE: &str = crate::storage::TEST_SCHEMAS_SEEN;

/// One `storage.test_database_url_files` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UrlFile {
    pub(super) name: String,
    pub(super) path: PathBuf,
}

/// The URL in the file at `path`, or why it is refused: not a regular file
/// this user (`uid`) owns at mode 0600, not a postgres URL, a host that is
/// not this one's (a socket or loopback), or the live database (`live`,
/// compared by the database each names using `defaults`). The reason never
/// holds the URL.
pub(super) fn url_from_file_with(
    path: &Path,
    live: Option<&str>,
    uid: u32,
    defaults: &spawn_postgres::LibpqDefaults,
) -> Result<String, String> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("cannot be read: {}", error.kind()))?;
    if !metadata.file_type().is_file() {
        return Err("not a regular file".to_string());
    }
    if metadata.uid() != uid {
        return Err("not owned by this user".to_string());
    }
    let mode = metadata.mode() & 0o7777;
    if mode != 0o600 {
        return Err(format!("mode {mode:04o}, not 0600"));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot be read: {}", error.kind()))?;
    let url = text.trim().to_string();
    let named = spawn_postgres::database_identity_with_defaults(&url, defaults)?;
    if !named.local {
        return Err("names a host that is not this one's (a local socket or loopback)".to_string());
    }
    if let Some(live) =
        live.and_then(|live| spawn_postgres::database_identity_with_defaults(live, defaults).ok())
        && live == named
    {
        return Err("names the live database".to_string());
    }
    Ok(url)
}

#[cfg(test)]
pub(super) fn url_from_file(path: &Path, live: Option<&str>, uid: u32) -> Result<String, String> {
    url_from_file_with(path, live, uid, &spawn_postgres::LibpqDefaults::default())
}

/// The `<pid>` of a test schema's name, `spawn_test_<pid>_...`; `None` for
/// a name with no pid.
pub(super) fn pid_of(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("spawn_test_")?;
    let digits = rest.split('_').next()?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// When a schema was made by its comment, `{"made_at_ms": <ms>}`; `None`
/// with no comment or one that does not read so.
pub(super) fn made_at(comment: &Value) -> Option<i64> {
    let text = comment.as_str()?;
    let value: Value = serde_json::from_str(text).ok()?;
    value["made_at_ms"].as_i64()
}

/// Whether process `pid` is on this host.
pub(super) fn pid_runs(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// The seen map's key of `schema` on `server`.
fn seen_key(server: &str, schema: &str) -> String {
    format!("{server}/{schema}")
}

/// Bring `seen` up to `server`'s schemas `names` at `now_ms`: a schema first
/// seen now is added, a first sight never moves, and a schema of this server
/// no longer there is dropped. Returns whether anything changed.
pub(super) fn update_seen(
    seen: &mut BTreeMap<String, i64>,
    server: &str,
    names: &[&str],
    now_ms: i64,
) -> bool {
    let prefix = format!("{server}/");
    let present: std::collections::BTreeSet<String> =
        names.iter().map(|name| seen_key(server, name)).collect();
    let before = seen.len();
    seen.retain(|key, _| !key.starts_with(&prefix) || present.contains(key));
    let mut changed = seen.len() != before;
    for key in present {
        if let std::collections::btree_map::Entry::Vacant(slot) = seen.entry(key) {
            slot.insert(now_ms);
            changed = true;
        }
    }
    changed
}

/// How many of `server`'s `schemas` (`[{name, comment, held}]`, as
/// `spawn_postgres::test_schemas_with_passfile` gives them) are leaked at `now_ms`: not
/// held, their pid gone (`runs`), idle 24 h by their comment or else by
/// their first sight in `seen`.
pub(super) fn leaked(
    schemas: &[Value],
    server: &str,
    seen: &BTreeMap<String, i64>,
    now_ms: i64,
    runs: &dyn Fn(u32) -> bool,
) -> u64 {
    let count = schemas
        .iter()
        .filter(|schema| {
            let Some(name) = schema["name"].as_str() else {
                return false;
            };
            if schema["held"].as_bool() != Some(false) {
                return false;
            }
            if pid_of(name).is_some_and(runs) {
                return false;
            }
            let made =
                made_at(&schema["comment"]).or_else(|| seen.get(&seen_key(server, name)).copied());
            made.is_some_and(|made| now_ms.saturating_sub(made) >= IDLE_MS)
        })
        .count();
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// The seen file's map: empty when there is none or it does not read.
pub(super) fn read_seen(path: &Path) -> BTreeMap<String, i64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<BTreeMap<String, Value>>(&text).ok())
        .map(|map| {
            map.into_iter()
                .filter_map(|(key, value)| value.as_i64().map(|at| (key, at)))
                .collect()
        })
        .unwrap_or_default()
}

/// Write the seen map to `path`, through a file beside it renamed over it.
pub(super) fn write_seen(path: &Path, seen: &BTreeMap<String, i64>) -> Result<(), String> {
    let text = serde_json::to_string(seen).map_err(|error| error.to_string())?;
    let partial = path.with_extension("json.partial");
    std::fs::write(&partial, text).map_err(|error| format!("{}: {error}", partial.display()))?;
    std::fs::rename(&partial, path).map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use super::{leaked, made_at, pid_of, update_seen, url_from_file};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    const HOUR: i64 = 3_600_000;

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn url_file(dir: &Path, name: &str, url: &str, mode: u32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("{url}\n")).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        path
    }

    /// The URL file rule: not this user's, mode not 0600, a remote host,
    /// the live URL, the live database under another spelling (a socket
    /// against 127.0.0.1, an explicit 5432, another user); a socket and a
    /// loopback URL accepted.
    #[test]
    fn a_url_file_is_used_only_when_private_local_and_not_the_live_database() {
        let scratch = Scratch(crate::storage::tests::scratch("url_files"));
        let dir = &scratch.0;
        let me = spawn_host::process::current_uid();
        let live = "postgres://spawn@%2Fvar%2Frun%2Fpostgresql/spawn_live";
        let check = |path: &Path| url_from_file(path, Some(live), me);

        let socket = url_file(
            dir,
            "socket",
            "postgres://bb@%2Fvar%2Frun%2Fpostgresql/spawn_test",
            0o600,
        );
        assert_eq!(
            check(&socket).as_deref(),
            Ok("postgres://bb@%2Fvar%2Frun%2Fpostgresql/spawn_test")
        );
        let loopback = url_file(
            dir,
            "loopback",
            "postgres://bb:pw@127.0.0.1:5433/spawn_test",
            0o600,
        );
        assert!(check(&loopback).is_ok());
        let localhost = url_file(dir, "localhost", "postgresql://bb@localhost/other", 0o600);
        assert!(check(&localhost).is_ok());
        let key_value = url_file(dir, "kv", "host=/tmp dbname=spawn_test", 0o600);
        assert!(check(&key_value).is_ok());

        for (name, url, mode, want) in [
            (
                "loose",
                "postgres://bb@127.0.0.1/spawn_test",
                0o644,
                "mode 0644",
            ),
            (
                "group",
                "postgres://bb@127.0.0.1/spawn_test",
                0o640,
                "mode 0640",
            ),
            (
                "remote",
                "postgres://bb@192.0.2.1:5432/spawn_test",
                0o600,
                "not this one's",
            ),
            (
                "named",
                "postgres://bb@db.example.com/spawn_test",
                0o600,
                "not this one's",
            ),
            ("live", live, 0o600, "live database"),
            // The live database under other spellings: TCP loopback against
            // its socket, an explicit default port, another user, ::1.
            (
                "live_tcp",
                "postgres://spawn@127.0.0.1/spawn_live",
                0o600,
                "live database",
            ),
            (
                "live_port",
                "postgres://spawn@localhost:5432/spawn_live",
                0o600,
                "live database",
            ),
            (
                "live_user",
                "postgres://someone_else:pw@[::1]:5432/spawn_live",
                0o600,
                "live database",
            ),
            ("garbage", "not a url at all ===", 0o600, "postgres URL"),
        ] {
            let why = check(&url_file(dir, name, url, mode)).expect_err(name);
            assert!(why.contains(want), "{name}: {why}");
            assert!(
                !why.contains("://") && !why.contains("spawn_") && !why.contains("192.0.2"),
                "the reason never holds the URL: {why}"
            );
        }
        // Another port is another server's database.
        let other_port = url_file(
            dir,
            "other_port",
            "postgres://spawn@127.0.0.1:5433/spawn_live",
            0o600,
        );
        assert!(check(&other_port).is_ok());
        // Not this user's: checked against another uid.
        let why = url_from_file(&socket, Some(live), me.wrapping_add(1)).expect_err("not mine");
        assert!(why.contains("not owned by this user"), "{why}");
        // A symlink to a good file is not a regular file.
        std::os::unix::fs::symlink(&socket, dir.join("link")).expect("link");
        assert!(check(&dir.join("link")).is_err_and(|why| why.contains("not a regular file")));
        assert!(check(&dir.join("absent")).is_err_and(|why| why.contains("cannot be read")));
    }

    #[test]
    fn a_schema_names_its_pid_and_its_comment_its_making() {
        assert_eq!(pid_of("spawn_test_4127_1a2b_0_case"), Some(4127));
        assert_eq!(pid_of("spawn_test_x_0"), None);
        assert_eq!(pid_of("bb_other"), None);
        assert_eq!(pid_of("spawn_test__0"), None);
        assert_eq!(
            made_at(&json!("{\"made_at_ms\": 1731400000000}")),
            Some(1_731_400_000_000)
        );
        assert_eq!(made_at(&json!("made by hand")), None);
        assert_eq!(made_at(&json!(null)), None);
        assert_eq!(made_at(&json!("{\"made\": 1}")), None);
    }

    /// The leaked rule over a server's schemas: held, fresh, old with its
    /// pid gone, old with its pid alive, no comment first seen 25 h ago and
    /// never seen, a bad comment, no pid.
    #[test]
    fn a_schema_is_leaked_only_when_unheld_ownerless_and_idle_a_day() {
        let now = 100 * 24 * HOUR;
        let day_ago = now - 25 * HOUR;
        let made = |at: i64| json!(format!("{{\"made_at_ms\": {at}}}"));
        let schemas = vec![
            json!({"name": "spawn_test_9_a_held", "comment": made(day_ago), "held": true}),
            json!({"name": "spawn_test_9_a_fresh", "comment": made(now), "held": false}),
            json!({"name": "spawn_test_9_a_old", "comment": made(day_ago), "held": false}),
            json!({"name": "spawn_test_7_b_old", "comment": made(day_ago), "held": false}),
            json!({"name": "spawn_test_9_a_seen", "comment": null, "held": false}),
            json!({"name": "spawn_test_9_a_unseen", "comment": null, "held": false}),
            json!({"name": "spawn_test_9_a_junk", "comment": "made by hand", "held": false}),
            json!({"name": "bb_other", "comment": null, "held": false}),
        ];
        let names: Vec<&str> = schemas
            .iter()
            .filter_map(|schema| schema["name"].as_str())
            .collect();
        let mut seen = BTreeMap::from([
            ("test/spawn_test_9_a_seen".to_string(), day_ago),
            ("test/bb_other".to_string(), day_ago),
            ("test/gone_since".to_string(), day_ago),
            ("spare/kept".to_string(), day_ago),
        ]);
        assert!(update_seen(&mut seen, "test", &names, now));
        assert_eq!(
            seen["test/spawn_test_9_a_seen"], day_ago,
            "a first sight never moves"
        );
        assert_eq!(seen["test/spawn_test_9_a_unseen"], now, "first seen now");
        assert!(
            !seen.contains_key("test/gone_since"),
            "a schema no longer there is dropped"
        );
        assert_eq!(seen["spare/kept"], day_ago, "another server's entries stay");
        assert!(
            !update_seen(&mut seen, "test", &names, now + 1),
            "nothing new"
        );
        // Process 7 runs; 9 does not.
        let runs = |pid: u32| pid == 7;
        assert_eq!(
            leaked(&schemas, "test", &seen, now, &runs),
            3,
            "old, seen and bb_other"
        );
        // A day on, the never-seen and the bad-comment ones are leftovers too.
        assert_eq!(leaked(&schemas, "test", &seen, now + 25 * HOUR, &runs), 6);
    }

    /// The schemas this test makes in the test database, dropped by name
    /// when it ends, passed or not.
    struct Made {
        url: String,
        names: Vec<String>,
    }

    impl Drop for Made {
        fn drop(&mut self) {
            if let Ok(mut client) = postgres::Client::connect(&self.url, postgres::NoTls) {
                for name in &self.names {
                    let _ = client.batch_execute(&format!(
                        "set lock_timeout = '5s'; drop schema if exists {name} cascade"
                    ));
                }
            }
        }
    }

    /// The statements that make the test's eight schemas (`names`, in the
    /// order of [`the_test_database_read_counts_only_the_leftovers`]): the
    /// held, the old ones and the one whose pid runs made 25 h ago, the
    /// fresh one now, a bad comment, the rest with none.
    fn making(names: &[String], now: i64, day_ago: i64) -> String {
        use std::fmt::Write as _;
        let mut sql = String::new();
        for name in names {
            let _ = write!(sql, "create schema {name}; ");
        }
        for (name, at) in [
            (&names[0], day_ago),
            (&names[1], now),
            (&names[2], day_ago),
            (&names[3], day_ago),
        ] {
            let _ = write!(
                sql,
                "comment on schema {name} is '{{\"made_at_ms\": {at}}}'; "
            );
        }
        let _ = write!(sql, "comment on schema {} is 'made by hand'; ", names[6]);
        sql
    }

    /// Against the test database: the one read gives each schema's comment
    /// and whether a live session holds it, and the leaked count is held,
    /// fresh, old with its pid gone, old with its pid alive, no comment
    /// first seen 25 h ago and never seen, a bad comment, no pid.
    #[test]
    fn the_test_database_read_counts_only_the_leftovers() {
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let mut exited = std::process::Command::new("true")
            .spawn()
            .expect("a process");
        let dead = exited.id();
        exited.wait().expect("it exits");
        assert!(!super::pid_runs(dead), "process {dead} is gone");
        let alive = std::process::id();
        let token = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos())
        );
        let gone = |case: &str| format!("spawn_test_{dead}_{token}_{case}");
        let names = vec![
            gone("held"),
            gone("fresh"),
            gone("old"),
            format!("spawn_test_{alive}_{token}_alive"),
            gone("seen"),
            gone("unseen"),
            gone("junk"),
            format!("bb_{token}_other"),
        ];
        let made = Made {
            url: url.clone(),
            names: names.clone(),
        };
        let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connect");
        let now: i64 = client
            .query_one(
                "select floor(extract(epoch from clock_timestamp()) * 1000)::bigint",
                &[],
            )
            .expect("the clock")
            .get(0);
        let day_ago = now - 25 * HOUR;
        let sql = making(&names, now, day_ago);
        client.batch_execute(&sql).expect("make the schemas");
        // A live session holding the first, as a running test's store.
        let mut holder = postgres::Client::connect(&url, postgres::NoTls).expect("connect");
        holder
            .batch_execute(&format!("set application_name to '{}'", names[0]))
            .expect("name the session");

        let all = spawn_postgres::test_schemas_with_passfile(&url, None).expect("the read");
        let mine: Vec<serde_json::Value> = all
            .as_array()
            .expect("an array")
            .iter()
            .filter(|schema| {
                schema["name"]
                    .as_str()
                    .is_some_and(|name| name.contains(&token))
            })
            .cloned()
            .collect();
        assert_eq!(mine.len(), 8, "{mine:?}");
        let held: Vec<&str> = mine
            .iter()
            .filter(|schema| schema["held"] == true)
            .filter_map(|schema| schema["name"].as_str())
            .collect();
        assert_eq!(held, [names[0].as_str()]);
        assert!(all.as_array().expect("an array").iter().all(|schema| {
            let name = schema["name"].as_str().unwrap_or("pg_");
            name != "public" && name != "information_schema" && !name.starts_with("pg_")
        }));
        let mut seen = BTreeMap::from([
            (format!("test/{}", names[4]), day_ago),
            (format!("test/{}", names[7]), day_ago),
        ]);
        let listed: Vec<&str> = mine
            .iter()
            .filter_map(|schema| schema["name"].as_str())
            .collect();
        update_seen(&mut seen, "test", &listed, now);
        assert_eq!(
            leaked(&mine, "test", &seen, now, &super::pid_runs),
            3,
            "the old one whose pid is gone, the one first seen 25 h ago and the one with no pid"
        );
        drop(holder);
        drop(client);
        drop(made);
    }
}
