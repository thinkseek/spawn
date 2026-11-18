//! The storage line's "scratch nobody owns" and "leaked test schemas"
//! (objective 094), read in the background and kept.
//!
//! The Core lines file's `storage` names what to read, re-read at every
//! request like the ranges:
//!
//! ```text
//! "storage": {
//!   "scratch_roots": [<dir>], "scratch_exclude": [<path>],
//!   "scratch_bound": {"entries": <n>, "ms": <n>},
//!   "test_database_url_files": [{"name", "url_file"}]
//! }
//! ```
//!
//! `scratch_bound` sets each root's walk's bound (objective 155): both whole
//! numbers from 1, otherwise the default ([`Bound::DEFAULT`]).
//!
//! The scratch walk ([`super::scratch`]) and the test servers' schemas
//! ([`super::schemas`]) are read at most every [`READ_EVERY_MS`], on a
//! thread of their own: a request (and the loop's sample) serves the kept
//! reading and never waits for a refresh. Only the first read after start
//! is waited for, at most [`FIRST_WAIT`]; past that the figures are null
//! (not collected) until it ends. A walk past its bound keeps the previous
//! scratch figure and says so in the log, never a partial count; of the
//! previous claimed folders it keeps only those still a real directory
//! (objective 220, [`still_there`]: one removed since, or now a symlink or
//! a file, is left out). A test server that does not answer is left out and
//! says why.
//!
//! Read only: nothing here removes, moves, renames or touches a file or a
//! schema. No gate, integrator or deploy step reads any of it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::schemas::{self, UrlFile};
use super::scratch::{self, Bound, Claimed, Unowned};

/// How long a reading is kept before it is read again.
pub(super) const READ_EVERY_MS: i64 = 600_000;

/// How long the first read after start is waited for.
pub(super) const FIRST_WAIT: Duration = Duration::from_secs(10);

/// How many entries nobody owns the answer lists, largest first.
const LISTED: usize = 20;

/// The file's `storage`: the scratch roots, the excludes, each root's
/// walk's bound and the URL files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Settings {
    pub(super) roots: Vec<PathBuf>,
    pub(super) excludes: Vec<PathBuf>,
    pub(super) bound: Bound,
    pub(super) url_files: Vec<UrlFile>,
}

/// `scratch_bound` `{"entries": <n>, "ms": <n>}`: both whole numbers from 1,
/// otherwise [`Bound::DEFAULT`].
fn bound(section: &Value) -> Bound {
    let whole = |field: &str| section[field].as_u64().filter(|value| *value >= 1);
    match (whole("entries"), whole("ms")) {
        (Some(entries), Some(ms)) => Bound {
            entries,
            time: Duration::from_millis(ms),
        },
        _ => Bound::DEFAULT,
    }
}

/// The `storage` of the Core lines file's `text` (`None`: no file): no
/// roots, excludes or URL files where it names none.
pub(super) fn settings(text: Option<&str>) -> Settings {
    let Some(value) = text.and_then(|text| serde_json::from_str::<Value>(text).ok()) else {
        return Settings::default();
    };
    let section = &value["storage"];
    let paths = |field: &str| -> Vec<PathBuf> {
        section[field]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .collect()
    };
    let url_files = section["test_database_url_files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some(UrlFile {
                name: entry["name"].as_str()?.to_string(),
                path: PathBuf::from(entry["url_file"].as_str()?),
            })
        })
        .collect();
    Settings {
        roots: paths("scratch_roots"),
        excludes: paths("scratch_exclude"),
        bound: bound(&section["scratch_bound"]),
        url_files,
    }
}

/// What one reading reads: the roots and excludes, each root's walk's bound
/// (a change in the file starts a new reading), the paths never counted,
/// the test servers by name (the env's first, then the URL files taken), and
/// where the first sights are kept. Its URLs are never printed.
#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct Plan {
    pub(super) roots: Vec<PathBuf>,
    pub(super) excludes: Vec<PathBuf>,
    pub(super) bound: Bound,
    pub(super) never: Vec<PathBuf>,
    pub(super) servers: Vec<(String, String)>,
    pub(super) seen: Option<PathBuf>,
}

impl std::fmt::Debug for Plan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Plan")
            .field("roots", &self.roots)
            .field("excludes", &self.excludes)
            .field("bound", &self.bound)
            .field(
                "servers",
                &self
                    .servers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// The plan for `settings` now, and the URL files refused, each with why:
/// the env's test servers (`SPAWN_TEST_DATABASE_URL` "test",
/// `SPAWN_FLEET_TEST_DATABASE_URL` "fleet_test"), then each URL file
/// [`schemas::url_from_file`] takes, compared with `SPAWN_DATABASE_URL`.
pub(super) fn plan(settings: &Settings) -> (Plan, Vec<(String, String)>) {
    let live = crate::env_value("SPAWN_DATABASE_URL");
    let uid = spawn_host::process::current_uid();
    let mut servers: Vec<(String, String)> = crate::alerts::test_database_urls()
        .into_iter()
        .map(|(role, url)| (role.to_string(), url.0))
        .collect();
    let mut refused = Vec::new();
    for file in &settings.url_files {
        match schemas::url_from_file(&file.path, live.as_deref(), uid) {
            Ok(url) => servers.push((file.name.clone(), url)),
            Err(why) => refused.push((file.name.clone(), why)),
        }
    }
    let plan = Plan {
        roots: settings.roots.clone(),
        excludes: settings.excludes.clone(),
        bound: settings.bound,
        never: scratch::never_counted(&crate::storage::projects_path()),
        servers,
        seen: crate::env_value("SPAWN_WORKSPACE_ROOT")
            .map(|root| PathBuf::from(root).join(schemas::SEEN_FILE)),
    };
    (plan, refused)
}

/// One test server's schemas, as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Server {
    pub(super) name: String,
    pub(super) schemas: u64,
    pub(super) leaked: u64,
}

/// One reading: the entries nobody owns (`None` with no roots, or before a
/// walk within its bound), and the test servers that answered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Reading {
    pub(super) unowned: Option<Vec<Unowned>>,
    /// Every claimed folder the walk met (objective 140), by agent then
    /// oldest first; none with no roots or before a walk within its bound.
    pub(super) claimed: Vec<Claimed>,
    pub(super) servers: Vec<Server>,
    /// The scratch roots refused (a symlink at or above them, or an exclude
    /// that holds them), each with why: named in the answer, and the figure
    /// is not collected while any is.
    pub(super) refused: Vec<(PathBuf, String)>,
}

impl Reading {
    /// `storage.unowned_scratch_bytes`: the entries' bytes, null with no
    /// roots, no walk within its bound yet, or a root refused (a count
    /// without it would read as the whole).
    pub(super) fn unowned_bytes(&self) -> Value {
        if !self.refused.is_empty() {
            return Value::Null;
        }
        self.unowned.as_ref().map_or(Value::Null, |entries| {
            json!(entries.iter().map(|entry| entry.bytes).sum::<u64>())
        })
    }

    /// `storage.leaked_test_schemas`: the sum over the servers that
    /// answered; null when none did.
    pub(super) fn leaked(&self) -> Value {
        if self.servers.is_empty() {
            Value::Null
        } else {
            json!(self.servers.iter().map(|server| server.leaked).sum::<u64>())
        }
    }
}

/// The first sights when there is no workspace root to keep them under.
static SEEN: Mutex<BTreeMap<String, i64>> = Mutex::new(BTreeMap::new());

/// Read `plan` at `now_ms`: the scratch walk (each root within `bound`; past
/// it the `previous` reading's entries are kept and the log says so) and
/// each test server's schemas (one that does not answer is left out, and
/// the log says why), the first sights brought up to date.
pub(super) fn read(plan: &Plan, previous: Option<&Reading>, now_ms: i64, bound: Bound) -> Reading {
    let (unowned, claimed) = if plan.roots.is_empty() {
        (None, Vec::new())
    } else {
        match scratch::walk(
            &plan.roots,
            &plan.excludes,
            &plan.never,
            now_ms,
            &crate::integrate::still_runs,
            bound,
        ) {
            Ok(walk) => (Some(walk.unowned), walk.claimed),
            Err(why) => {
                eprintln!("core-lines: scratch nobody owns: {why}; the previous figure is kept");
                previous.map_or((None, Vec::new()), |previous| {
                    (previous.unowned.clone(), still_there(&previous.claimed))
                })
            }
        }
    };
    let mut seen = match &plan.seen {
        Some(path) => schemas::read_seen(path),
        None => SEEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    };
    let mut changed = false;
    let mut servers = Vec::new();
    for (name, url) in &plan.servers {
        let listed = match spawn_postgres::test_schemas(url) {
            Ok(listed) => listed,
            Err(error) => {
                eprintln!("core-lines: the test server {name} is left out: {error}");
                continue;
            }
        };
        let rows = listed.as_array().cloned().unwrap_or_default();
        let names: Vec<&str> = rows.iter().filter_map(|row| row["name"].as_str()).collect();
        changed |= schemas::update_seen(&mut seen, name, &names, now_ms);
        servers.push(Server {
            name: name.clone(),
            schemas: u64::try_from(rows.len()).unwrap_or(u64::MAX),
            leaked: schemas::leaked(&rows, name, &seen, now_ms, &schemas::pid_runs),
        });
    }
    if changed {
        match &plan.seen {
            Some(path) => {
                if let Err(why) = schemas::write_seen(path, &seen) {
                    eprintln!("core-lines: the test schemas' first sights: {why}");
                }
            }
            None => {
                *SEEN
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = seen;
            }
        }
    }
    Reading {
        unowned,
        claimed,
        servers,
        refused: scratch::refused_roots(&plan.roots, &plan.excludes),
    }
}

/// The `(mission, objective)` pairs the reading's claimed folders name, each
/// once, in order: what [`scratch_json`]'s `landed` is read for.
/// Of a kept reading's claimed folders, those still a real directory
/// (lstat, never followed): one removed since, or replaced by a symlink or a
/// file, is no claim to show (objective 220).
fn still_there(claimed: &[Claimed]) -> Vec<Claimed> {
    claimed
        .iter()
        .filter(|folder| {
            std::fs::symlink_metadata(&folder.path)
                .is_ok_and(|metadata| metadata.file_type().is_dir())
        })
        .cloned()
        .collect()
}

pub(super) fn claimed_pairs(reading: Option<&Reading>) -> Vec<(i64, i64)> {
    let mut pairs: Vec<(i64, i64)> = Vec::new();
    for claimed in reading
        .map(|reading| reading.claimed.as_slice())
        .unwrap_or_default()
    {
        if let Some(pair) = claimed.claim.pair()
            && !pairs.contains(&pair)
        {
            pairs.push(pair);
        }
    }
    pairs
}

/// The answer's `scratch`: the roots, the 20 largest entries nobody owns,
/// the roots refused, each with why, and every claimed folder (objective
/// 140) {path, agent, objective, created_ms, lease_until_ms, bytes,
/// build_bytes, landed}, `landed` whether `landed` (the store's landed
/// pairs) holds its objective, null when the store could not be read.
pub(super) fn scratch_json(
    plan: &Plan,
    reading: Option<&Reading>,
    landed: Option<&[(i64, i64)]>,
) -> Value {
    let claimed: Vec<Value> = reading
        .map(|reading| reading.claimed.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|folder| {
            let landed = match (landed, folder.claim.pair()) {
                (Some(landed), Some(pair)) => json!(landed.contains(&pair)),
                _ => Value::Null,
            };
            json!({
                "path": folder.path.display().to_string(),
                "agent": folder.claim.agent,
                "objective": folder.claim.objective,
                "created_ms": folder.claim.created_ms,
                "lease_until_ms": folder.claim.lease_until_ms,
                "bytes": folder.bytes,
                "build_bytes": folder.build_bytes,
                "landed": landed,
            })
        })
        .collect();
    let unowned: Vec<Value> = reading
        .and_then(|reading| reading.unowned.as_ref())
        .into_iter()
        .flatten()
        .take(LISTED)
        .map(|entry| {
            json!({
                "path": entry.path.display().to_string(),
                "bytes": entry.bytes,
                "reason": entry.reason.word(),
            })
        })
        .collect();
    let refused: Vec<Value> = reading
        .map(|reading| reading.refused.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|(root, why)| json!({"root": root.display().to_string(), "why": why}))
        .collect();
    json!({
        "roots": plan.roots.iter().map(|root| root.display().to_string()).collect::<Vec<_>>(),
        "unowned": unowned,
        "refused": refused,
        "claimed": claimed,
    })
}

/// The answer's `test_schemas`: each server that answered, in order.
pub(super) fn test_schemas_json(reading: Option<&Reading>) -> Value {
    json!(
        reading
            .map(|reading| reading.servers.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|server| json!({
                "server": server.name,
                "schemas": server.schemas,
                "leaked": server.leaked,
            }))
            .collect::<Vec<_>>()
    )
}

/// A reading kept, the plan it read and when.
struct Kept {
    at_ms: i64,
    plan: Plan,
    reading: Arc<Reading>,
}

#[derive(Default)]
struct State {
    kept: Option<Kept>,
    refreshing: bool,
    /// While nothing is kept: when the wait for the refresh under way ends.
    /// Set once, as that refresh starts, so every request shares the one
    /// deadline (cc-astra A28): past it they return at once.
    first_deadline: Option<Instant>,
}

/// The kept reading and the one refresh at a time.
#[derive(Default)]
pub(super) struct Cache {
    state: Mutex<State>,
    done: Condvar,
}

/// Clears `refreshing` however the refresh ends (a panic too), and wakes
/// whoever waits.
struct Refreshing(Arc<Cache>);

impl Drop for Refreshing {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .refreshing = false;
        self.0.done.notify_all();
    }
}

impl Cache {
    /// The reading for `plan` at `now_ms`: the kept one when it read this
    /// plan less than [`READ_EVERY_MS`] ago. Otherwise a refresh starts on a
    /// thread of its own (one at a time) through `read` (the plan, the
    /// previous reading, the moment), and the kept reading is served
    /// without waiting; with none kept yet, the refresh is waited for at
    /// most `first_wait` from that refresh's start, one deadline for every
    /// request, and `None` past it until the refresh ends. A refresh that
    /// ends with nothing kept (its read panicked) leaves the next request to
    /// start another, which sets a deadline of its own: each refresh is
    /// waited for at most once, never each request.
    pub(super) fn get(
        self: &Arc<Self>,
        plan: &Plan,
        now_ms: i64,
        first_wait: Duration,
        read: impl FnOnce(&Plan, Option<&Reading>, i64) -> Reading + Send + 'static,
    ) -> Option<Arc<Reading>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(kept) = state.kept.as_ref().filter(|kept| {
            kept.plan == *plan && (0..READ_EVERY_MS).contains(&now_ms.saturating_sub(kept.at_ms))
        }) {
            return Some(Arc::clone(&kept.reading));
        }
        if !state.refreshing {
            state.refreshing = true;
            state.first_deadline = state.kept.is_none().then(|| Instant::now() + first_wait);
            let cache = Arc::clone(self);
            let previous = state.kept.as_ref().map(|kept| Arc::clone(&kept.reading));
            let plan = plan.clone();
            std::thread::spawn(move || {
                let guard = Refreshing(Arc::clone(&cache));
                let reading = Arc::new(read(&plan, previous.as_deref(), now_ms));
                cache
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .kept = Some(Kept {
                    at_ms: now_ms,
                    plan,
                    reading,
                });
                drop(guard);
            });
        }
        if let Some(kept) = &state.kept {
            return Some(Arc::clone(&kept.reading));
        }
        let left = state.first_deadline.map_or(Duration::ZERO, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        });
        let (state, _) = self
            .done
            .wait_timeout_while(state, left, |state| {
                state.kept.is_none() && state.refreshing
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.kept.as_ref().map(|kept| Arc::clone(&kept.reading))
    }
}

/// The process's cache.
static CACHE: LazyLock<Arc<Cache>> = LazyLock::new(|| Arc::new(Cache::default()));

/// What the answer gains: the reading (kept, or `None` while the first is
/// out), the plan it is for, and the URL files refused, each with why.
pub(super) struct Now {
    pub(super) plan: Plan,
    pub(super) reading: Option<Arc<Reading>>,
    pub(super) refused: Vec<(String, String)>,
}

/// The reading for the Core lines file's `text` at `now_ms`, kept or
/// refreshed in the background ([`Cache::get`]).
pub(super) fn now(text: Option<&str>, now_ms: i64) -> Now {
    let (plan, refused) = plan(&settings(text));
    let reading = if plan.roots.is_empty() && plan.servers.is_empty() {
        Some(Arc::new(Reading::default()))
    } else {
        CACHE.get(&plan, now_ms, FIRST_WAIT, |plan, previous, now_ms| {
            read(plan, previous, now_ms, plan.bound)
        })
    };
    Now {
        plan,
        reading,
        refused,
    }
}

#[cfg(test)]
mod tests {
    use super::{Cache, Plan, READ_EVERY_MS, Reading, Server, read, settings};
    use crate::serve::core_lines::scratch::{Bound, Reason, Unowned};
    use serde_json::{Value, json};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    fn plan(root: &str) -> Plan {
        Plan {
            roots: vec![PathBuf::from(root)],
            ..Plan::default()
        }
    }

    fn reading(bytes: u64) -> Reading {
        Reading {
            unowned: Some(vec![Unowned {
                path: PathBuf::from("/s/x"),
                bytes,
                reason: Reason::Idle,
            }]),
            claimed: Vec::new(),
            servers: vec![Server {
                name: "test".into(),
                schemas: 3,
                leaked: 1,
            }],
            refused: Vec::new(),
        }
    }

    #[test]
    fn the_files_storage_names_the_roots_excludes_and_url_files() {
        let read = settings(Some(
            r#"{"ranges": {}, "storage": {"scratch_roots": ["/var/tmp/spawn", ""],
                "scratch_exclude": ["/var/tmp/spawn/sync"],
                "test_database_url_files": [{"name": "ccd", "url_file": "/x/ccd.url"},
                                            {"name": "no file"}]}}"#,
        ));
        assert_eq!(read.roots, [PathBuf::from("/var/tmp/spawn")]);
        assert_eq!(read.excludes, [PathBuf::from("/var/tmp/spawn/sync")]);
        assert_eq!(read.url_files.len(), 1);
        assert_eq!(read.url_files[0].name, "ccd");
        assert_eq!(read.bound, Bound::DEFAULT, "no scratch_bound");
        // Objective 155: the file's bound, both whole numbers from 1.
        let bound = |text: &str| {
            settings(Some(&format!(
                r#"{{"storage": {{"scratch_bound": {text}}}}}"#
            )))
            .bound
        };
        assert_eq!(
            bound(r#"{"entries": 900, "ms": 30000}"#),
            Bound {
                entries: 900,
                time: Duration::from_secs(30),
            }
        );
        for bad in [
            r#"{"entries": 0, "ms": 30000}"#,
            r#"{"entries": 900}"#,
            r#"{"entries": 9.5, "ms": 30000}"#,
            r#"{"entries": "900", "ms": 30000}"#,
            r#"{"entries": 900, "ms": -1}"#,
            "null",
        ] {
            assert_eq!(bound(bad), Bound::DEFAULT, "{bad}");
        }
        assert_eq!(settings(None), super::Settings::default());
        assert_eq!(settings(Some("{")), super::Settings::default());
    }

    #[test]
    fn the_figures_are_the_bytes_and_the_sum_over_the_servers_that_answered() {
        assert_eq!(reading(7).unowned_bytes(), json!(7));
        assert_eq!(reading(7).leaked(), json!(1));
        let none = Reading::default();
        assert_eq!(none.unowned_bytes(), Value::Null, "no roots");
        assert_eq!(none.leaked(), Value::Null, "no server answered");
    }

    /// The reads are kept between requests; a request during a refresh gets
    /// the kept figure without waiting; the first read is waited for.
    #[test]
    fn a_reading_is_kept_and_a_refresh_never_holds_a_request() {
        let cache = Arc::new(Cache::default());
        let reads = Arc::new(AtomicUsize::new(0));
        let counted = |bytes: u64| {
            let reads = Arc::clone(&reads);
            move |_: &Plan, _: Option<&Reading>, _: i64| {
                reads.fetch_add(1, Ordering::SeqCst);
                reading(bytes)
            }
        };
        let first = cache
            .get(&plan("/s"), 1000, Duration::from_secs(10), counted(1))
            .expect("the first read is waited for");
        assert_eq!(first.unowned_bytes(), json!(1));
        let again = cache
            .get(
                &plan("/s"),
                1000 + READ_EVERY_MS - 1,
                Duration::from_secs(10),
                counted(2),
            )
            .expect("kept");
        assert_eq!(again.unowned_bytes(), json!(1), "kept, not read again");
        assert_eq!(reads.load(Ordering::SeqCst), 1);

        // Due again: the refresh blocks until released; the request is
        // served the kept figure at once.
        let (release, wait) = mpsc::channel::<()>();
        let (started, began) = mpsc::channel::<()>();
        let started_at = Instant::now();
        let stale = cache
            .get(
                &plan("/s"),
                1000 + READ_EVERY_MS,
                Duration::from_secs(10),
                move |_: &Plan, previous: Option<&Reading>, _: i64| {
                    assert!(previous.is_some(), "the previous reading is passed on");
                    let _ = started.send(());
                    let _ = wait.recv();
                    reading(3)
                },
            )
            .expect("the kept reading");
        assert!(started_at.elapsed() < Duration::from_secs(2), "no wait");
        assert_eq!(stale.unowned_bytes(), json!(1));
        began.recv().expect("the refresh started");
        // Another request while it runs: kept, no second refresh.
        let during = cache
            .get(
                &plan("/s"),
                1000 + READ_EVERY_MS + 5,
                Duration::from_secs(10),
                counted(9),
            )
            .expect("kept");
        assert_eq!(during.unowned_bytes(), json!(1));
        release.send(()).expect("release");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let now = cache
                .get(
                    &plan("/s"),
                    1000 + READ_EVERY_MS + 10,
                    Duration::from_secs(10),
                    counted(9),
                )
                .expect("kept");
            if now.unowned_bytes() == json!(3) {
                break;
            }
            assert!(Instant::now() < deadline, "the refresh is kept once done");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(reads.load(Ordering::SeqCst), 1, "counted(9) never ran");

        // A first read that runs over the wait: not collected, no hang.
        let slow = Arc::new(Cache::default());
        let (unblock, blocked) = mpsc::channel::<()>();
        let started_at = Instant::now();
        let none = slow.get(
            &plan("/s"),
            0,
            Duration::from_millis(100),
            move |_: &Plan, _: Option<&Reading>, _: i64| {
                let _ = blocked.recv();
                reading(4)
            },
        );
        assert!(none.is_none());
        assert!(started_at.elapsed() < Duration::from_secs(2));
        drop(unblock);
    }

    /// cc-astra A28: the first read has one deadline. A request after it,
    /// while that read is still blocked, returns not collected at once
    /// instead of waiting a whole `first_wait` again.
    #[test]
    fn a_request_past_the_first_reads_deadline_returns_at_once() {
        let slow = Arc::new(Cache::default());
        let (unblock, blocked) = mpsc::channel::<()>();
        let reads = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&reads);
        let wait = Duration::from_millis(400);
        let first = slow.get(
            &plan("/s"),
            0,
            wait,
            move |_: &Plan, _: Option<&Reading>, _: i64| {
                counted.fetch_add(1, Ordering::SeqCst);
                let _ = blocked.recv();
                reading(4)
            },
        );
        assert!(first.is_none(), "the first read ran over its wait");
        let started_at = Instant::now();
        let second = slow.get(
            &plan("/s"),
            5,
            wait,
            |_: &Plan, _: Option<&Reading>, _: i64| reading(5),
        );
        assert!(second.is_none(), "still not collected");
        assert!(
            started_at.elapsed() < wait / 2,
            "no second wait: {:?}",
            started_at.elapsed()
        );
        unblock.send(()).expect("unblock");
        let deadline = Instant::now() + Duration::from_secs(5);
        while slow
            .get(
                &plan("/s"),
                10,
                wait,
                |_: &Plan, _: Option<&Reading>, _: i64| reading(6),
            )
            .is_none()
        {
            assert!(Instant::now() < deadline, "the read is kept once done");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(reads.load(Ordering::SeqCst), 1, "one refresh at a time");
    }

    /// A walk past its bound keeps the previous figure, never a partial
    /// count; with no previous one, not collected.
    #[test]
    fn a_walk_over_its_bound_keeps_the_previous_figure() {
        let root = crate::storage::tests::scratch("unowned_bound");
        for n in 0..10 {
            let entry = root.join(format!("e{n}"));
            std::fs::create_dir_all(&entry).expect("mkdir");
            std::fs::write(entry.join("f"), b"x").expect("write");
        }
        let plan = Plan {
            roots: vec![root.clone()],
            ..Plan::default()
        };
        let tight = Bound {
            entries: 3,
            time: Duration::from_secs(5),
        };
        let kept = read(&plan, Some(&reading(42)), 0, tight);
        assert_eq!(kept.unowned_bytes(), json!(42), "the previous figure");
        assert_eq!(read(&plan, None, 0, tight).unowned_bytes(), Value::Null);
        // Within the bound: the walk's own count (everything is idle at a
        // moment far ahead).
        let far = i64::MAX / 2;
        assert_eq!(
            read(&plan, Some(&reading(42)), far, Bound::DEFAULT).unowned_bytes(),
            json!(10)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Objective 220: a walk over its bound keeps the previous figure, and
    /// of the previous claimed folders only those still a real directory:
    /// one removed since and one now a symlink are left out.
    #[test]
    fn a_kept_reading_leaves_out_a_claimed_folder_removed_since() {
        let root = crate::storage::tests::scratch("unowned_kept_claims");
        for n in 0..10 {
            let entry = root.join(format!("e{n}"));
            std::fs::create_dir_all(&entry).expect("mkdir");
            std::fs::write(entry.join("f"), b"x").expect("write");
        }
        let here = root.join("e0");
        let removed = root.join("removed");
        let link = root.join("link");
        std::os::unix::fs::symlink(&here, &link).expect("symlink");
        let mut previous = reading(42);
        previous.claimed = [&here, &removed, &link]
            .iter()
            .map(|path| claimed(&path.display().to_string(), "rev-j", "4/220"))
            .collect();
        let plan = Plan {
            roots: vec![root.clone()],
            ..Plan::default()
        };
        let tight = Bound {
            entries: 3,
            time: Duration::from_secs(5),
        };
        let kept = read(&plan, Some(&previous), 0, tight);
        assert_eq!(kept.unowned_bytes(), json!(42), "the previous figure");
        let paths: Vec<&PathBuf> = kept.claimed.iter().map(|folder| &folder.path).collect();
        assert_eq!(paths, [&here], "here only");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// rev-g N1 on 329cbb0a: a refused scratch root is named in the answer
    /// with why, and the figure is not collected while it is, though the
    /// other root counts.
    #[test]
    fn a_refused_root_is_named_and_the_figure_is_not_collected() {
        let dir = crate::storage::tests::scratch("unowned_refused");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let dir = std::fs::canonicalize(&dir).expect("resolve");
        let real = dir.join("real");
        std::fs::create_dir_all(real.join("e")).expect("mkdir");
        std::fs::write(real.join("e/f"), b"x").expect("write");
        let alias = dir.join("alias");
        std::os::unix::fs::symlink(&real, &alias).expect("symlink");
        let far = i64::MAX / 2;
        let alone = Plan {
            roots: vec![real.clone()],
            ..Plan::default()
        };
        let counted = read(&alone, None, far, Bound::DEFAULT);
        assert_eq!(counted.unowned_bytes(), json!(1), "the real root counts");
        assert_eq!(
            super::scratch_json(&alone, Some(&counted), None)["refused"],
            json!([])
        );
        let both = Plan {
            roots: vec![real.clone(), alias.clone()],
            ..Plan::default()
        };
        let got = read(&both, None, far, Bound::DEFAULT);
        assert_eq!(got.unowned_bytes(), Value::Null, "not collected");
        let answer = super::scratch_json(&both, Some(&got), None);
        assert_eq!(
            answer["refused"].as_array().map(Vec::len),
            Some(1),
            "{answer}"
        );
        assert_eq!(
            answer["refused"][0]["root"],
            json!(alias.display().to_string())
        );
        assert!(
            answer["refused"][0]["why"]
                .as_str()
                .is_some_and(|why| why.contains("is a symlink")),
            "{answer}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn claimed(path: &str, agent: &str, objective: &str) -> super::Claimed {
        super::Claimed {
            path: PathBuf::from(path),
            claim: crate::scratch_owner::Claim {
                agent: agent.to_string(),
                objective: objective.to_string(),
                created_ms: 1,
                lease_until_ms: 2,
                build: vec!["t/target".to_string()],
            },
            bytes: Some(30),
            build_bytes: 20,
        }
    }

    /// Objective 140: the claimed folders' objectives are asked once each,
    /// and every folder's `landed` is whether the store's landed pairs hold
    /// its objective, null when the store was not read.
    #[test]
    fn the_claimed_folders_objectives_are_read_once_and_landed_set() {
        let mut got = reading(0);
        got.claimed = vec![
            claimed("/s/a", "rev-b", "4/140"),
            claimed("/s/b", "rev-e", "4/140"),
            claimed("/s/c", "rev-e", "4/141"),
        ];
        assert_eq!(super::claimed_pairs(Some(&got)), [(4, 140), (4, 141)]);
        assert_eq!(super::claimed_pairs(Some(&reading(0))), []);
        assert_eq!(super::claimed_pairs(None), []);
        let plan = plan("/s");
        let answer = super::scratch_json(&plan, Some(&got), Some(&[(4, 141), (5, 140)]));
        assert_eq!(
            answer["claimed"][0],
            json!({
                "path": "/s/a",
                "agent": "rev-b",
                "objective": "4/140",
                "created_ms": 1,
                "lease_until_ms": 2,
                "bytes": 30,
                "build_bytes": 20,
                "landed": false,
            })
        );
        let landed: Vec<Value> = (0..3)
            .map(|at| answer["claimed"][at]["landed"].clone())
            .collect();
        assert_eq!(landed, [json!(false), json!(false), json!(true)]);
        let unread = super::scratch_json(&plan, Some(&got), None);
        assert_eq!(
            unread["claimed"][2]["landed"],
            Value::Null,
            "the store was not read"
        );
        assert_eq!(super::scratch_json(&plan, None, None)["claimed"], json!([]));
    }
}
