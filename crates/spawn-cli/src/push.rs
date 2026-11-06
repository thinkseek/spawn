//! Agents' branches published to their mission's source for the integrator
//! to land (docs/storage.md, "Published for landing").
//!
//! A checkpoint marks its branch dirty; a background thread pushes the
//! branch's head as it is then, `refs/heads/<agent>` to the source's
//! `refs/heads/spawn/<agent>`, at most once per [`INTERVAL`] per branch, so a
//! burst of checkpoints costs one push and the Bridge's loop never waits on
//! the network. A push carries the head, so pushes that never happened in
//! between lose nothing. It is never forced: a rejection means
//! `spawn/<agent>` is not an ancestor of the head (a rewritten branch), which
//! is reported, not overwritten. A push
//! that failed (an unreachable source) is retried with a growing pause.
//!
//! **One pusher per source, for the life of the process** ([`shared`]).
//! Every store a process builds (the Bridge's, and the one-shot stores the
//! dispatcher and the integrator build for a merge of main or a set-aside)
//! queues its pushes on the same pusher, so a source has one queue and one
//! background thread per process however many merges it sees: a pusher per
//! merge left one parked thread behind per merge (issue #12). A change that
//! must be published before its caller goes on is pushed at once on the
//! caller's thread ([`Pusher::push_now`]), never beside a push of the same
//! branch on the background thread; one that fails stays queued and the
//! background thread retries it, with the same growing pause (issue #8).
//! Nothing is dropped from the queue but a push that succeeded or that the
//! source refused.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// Least time between two pushes of one branch.
pub(crate) const INTERVAL: Duration = Duration::from_secs(5);

/// Pause after a first failed push; it doubles per failure up to
/// [`MAX_RETRY`].
const RETRY: Duration = Duration::from_secs(5);

/// Longest pause between retries.
const MAX_RETRY: Duration = Duration::from_mins(5);

/// Longest wait for a push of the same branch already under way before
/// [`Pusher::push_now`] or [`Pusher::flush`] goes ahead anyway.
const IN_FLIGHT_WAIT: Duration = Duration::from_secs(30);

/// What one push did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Pushed {
    /// The source has the head (or already had it).
    Done,
    /// The source refused: `spawn/<agent>` is not an ancestor of the head.
    Rejected(String),
    /// The push could not run or reach the source.
    Failed(String),
}

/// Ref an agent's branch is pushed to on the source.
pub(crate) fn pushed_ref(branch: &str) -> String {
    format!("refs/heads/spawn/{branch}")
}

/// Push one branch's head from a mirror to its source, without force.
pub(crate) fn push_branch(mirror: &Path, source: &str, branch: &str) -> Pushed {
    let refspec = format!("refs/heads/{branch}:{}", pushed_ref(branch));
    // Not a source git may be given: no push can ever succeed, so it is not
    // retried.
    if let Err(error) = crate::sources::MissionSource::parse(source) {
        return Pushed::Rejected(error);
    }
    let output = crate::sources::controller_git()
        .arg("--git-dir")
        .arg(mirror)
        .args(["push", "--porcelain", "--", source, &refspec])
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) => return Pushed::Failed(format!("run git: {error}")),
    };
    if output.status.success() {
        return Pushed::Done;
    }
    // `--porcelain` flags a refused ref with `!`, whatever the language.
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(line) = stdout.lines().find(|line| line.starts_with('!')) {
        return Pushed::Rejected(line.replace('\t', " ").trim().to_string());
    }
    Pushed::Failed(String::from_utf8_lossy(&output.stderr).trim().to_string())
}


type PushFn = dyn Fn(&Path, &str, &str) -> Pushed + Send + Sync;

/// A branch of one mirror.
type Key = (PathBuf, String);

/// A branch waiting for its push.
struct Entry {
    source: String,
    due: Instant,
    failures: u32,
}

#[derive(Default)]
struct State {
    dirty: HashMap<Key, Entry>,
    /// When each branch was last pushed.
    last: HashMap<Key, Instant>,
    /// Branches whose rejection was reported, until a push succeeds.
    warned: HashSet<Key>,
    /// Branches being pushed now, on the background thread or a caller's:
    /// two pushes of one branch never race for its ref.
    pushing: HashSet<Key>,
    /// Background threads started: one per pusher, ever.
    threads: usize,
    started: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    push: Box<PushFn>,
    interval: Duration,
    retry: Duration,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record what a push did: a failure comes back after a pause.
    fn settle(&self, key: Key, entry: &Entry, pushed: Pushed) {
        let mut state = self.lock();
        let (mirror, branch) = (&key.0, &key.1);
        match pushed {
            Pushed::Done => {
                state.warned.remove(&key);
            }
            Pushed::Rejected(why) => {
                if state.warned.insert(key.clone()) {
                    eprintln!(
                        "sources: {} refused {branch} from {}; another writer moved it, \
                         not overwritten: {why}",
                        entry.source,
                        mirror.display()
                    );
                }
            }
            Pushed::Failed(why) => {
                let failures = entry.failures + 1;
                let pause = self
                    .retry
                    .saturating_mul(1 << (failures - 1).min(10))
                    .min(MAX_RETRY.max(self.retry));
                if failures == 1 {
                    eprintln!(
                        "sources: could not push {branch} to {}, retrying: {why}",
                        entry.source
                    );
                }
                let due = Instant::now() + pause;
                state
                    .dirty
                    .entry(key)
                    .and_modify(|waiting| {
                        waiting.failures = waiting.failures.max(failures);
                        waiting.due = waiting.due.max(due);
                    })
                    .or_insert(Entry {
                        source: entry.source.clone(),
                        due,
                        failures,
                    });
            }
        }
    }

    /// The background thread: push what is due, sleep until the next.
    ///
    /// It never returns, by design: its pusher is the process's for its
    /// source ([`shared`]) and lives as long as the process, so there is
    /// one such thread per source, not one per store.
    fn run(&self) {
        loop {
            let batch: Vec<(Key, Entry)> = {
                let mut state = self.lock();
                loop {
                    let now = Instant::now();
                    // A branch a caller is pushing now waits for that push.
                    let due: Vec<Key> = state
                        .dirty
                        .iter()
                        .filter(|(key, entry)| entry.due <= now && !state.pushing.contains(*key))
                        .map(|(key, _)| key.clone())
                        .collect();
                    if !due.is_empty() {
                        let batch: Vec<(Key, Entry)> = due
                            .into_iter()
                            .filter_map(|key| state.dirty.remove_entry(&key))
                            .collect();
                        for (key, _) in &batch {
                            state.last.insert(key.clone(), now);
                            state.pushing.insert(key.clone());
                        }
                        break batch;
                    }
                    // A branch a caller is pushing is looked at again when
                    // that push ends: it wakes this thread.
                    let wait = state
                        .dirty
                        .iter()
                        .filter(|(key, _)| !state.pushing.contains(*key))
                        .map(|(_, entry)| entry.due.saturating_duration_since(now))
                        .min()
                        .unwrap_or(Duration::from_mins(1));
                    state = self
                        .wake
                        .wait_timeout(state, wait)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0;
                }
            };
            for (key, entry) in batch {
                let pushed = (self.push)(&key.0, &entry.source, &key.1);
                self.settle(key.clone(), &entry, pushed);
                self.lock().pushing.remove(&key);
                self.wake.notify_all();
            }
        }
    }
}

/// Coalescing background pusher of agents' branches.
pub(crate) struct Pusher {
    shared: Arc<Shared>,
}

/// The process's pusher for `source`, made on first use and kept until the
/// process ends. Every store of the process pushes `source`'s branches
/// through it (see the module documentation).
pub(crate) fn shared(source: &str) -> Arc<Pusher> {
    let mut pushers = pushers()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(
        pushers
            .entry(source.to_string())
            .or_insert_with(|| Arc::new(Pusher::new())),
    )
}

/// Push what waits on every pusher of the process ([`Pusher::flush`]): a
/// process that ends cleanly leaves nothing behind. How many pushes failed.
pub(crate) fn flush_shared() -> usize {
    let all: Vec<Arc<Pusher>> = pushers()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .cloned()
        .collect();
    all.iter().map(|pusher| pusher.flush()).sum()
}

fn pushers() -> &'static Mutex<HashMap<String, Arc<Pusher>>> {
    static PUSHERS: OnceLock<Mutex<HashMap<String, Arc<Pusher>>>> = OnceLock::new();
    PUSHERS.get_or_init(Mutex::default)
}

impl Pusher {
    /// Pusher that runs `git push`.
    pub(crate) fn new() -> Self {
        Self::with(Box::new(push_branch), INTERVAL, RETRY)
    }

    /// Pusher over another push function and timing, for tests.
    pub(crate) fn with(push: Box<PushFn>, interval: Duration, retry: Duration) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                wake: Condvar::new(),
                push,
                interval,
                retry,
            }),
        }
    }

    /// The branch moved: push its head soon. A branch pushed less than the
    /// interval ago waits for the rest of it; one already waiting keeps its
    /// time (and its retry pause).
    pub(crate) fn mark(&self, mirror: &Path, source: &str, branch: &str) {
        let key = (mirror.to_path_buf(), branch.to_string());
        let mut state = self.shared.lock();
        let now = Instant::now();
        let due = state
            .last
            .get(&key)
            .map_or(now, |at| (*at + self.shared.interval).max(now));
        state.dirty.entry(key).or_insert(Entry {
            source: source.to_string(),
            due,
            failures: 0,
        });
        self.start_thread(&mut state);
        drop(state);
        self.shared.wake.notify_all();
    }

    /// The background thread, started once, by the first push queued.
    fn start_thread(&self, state: &mut State) {
        if state.started {
            return;
        }
        state.started = true;
        let shared = Arc::clone(&self.shared);
        let spawned = std::thread::Builder::new()
            .name("spawn-push".to_string())
            .spawn(move || shared.run());
        match spawned {
            Ok(_) => state.threads += 1,
            Err(error) => {
                state.started = false;
                eprintln!("sources: no pusher thread: {error}");
            }
        }
    }

    /// Background threads this pusher started: at most one, however many
    /// pushes it was given.
    #[cfg(test)]
    pub(crate) fn threads(&self) -> usize {
        self.shared.lock().threads
    }

    /// Whether `branch` of `mirror` waits for a push (a new one, or a
    /// failed one's retry).
    #[cfg(test)]
    pub(crate) fn waiting(&self, mirror: &Path, branch: &str) -> bool {
        self.shared
            .lock()
            .dirty
            .contains_key(&(mirror.to_path_buf(), branch.to_string()))
    }

    /// Push `branch`'s head now, on this thread, after a push of it already
    /// under way, and say what it did: a change its caller must have
    /// published (a merge of main) is not left to the interval. What waited
    /// for the branch is taken by this push. A push that failed is not
    /// dropped: the branch waits again, with its pause, and the background
    /// thread retries it until it is done or refused.
    pub(crate) fn push_now(&self, mirror: &Path, source: &str, branch: &str) -> Pushed {
        let key = (mirror.to_path_buf(), branch.to_string());
        let failures = {
            let mut state = self.shared.lock();
            let deadline = Instant::now() + IN_FLIGHT_WAIT;
            while state.pushing.contains(&key) && Instant::now() < deadline {
                state = self
                    .shared
                    .wake
                    .wait_timeout(state, Duration::from_millis(100))
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
            let failures = state.dirty.remove(&key).map_or(0, |entry| entry.failures);
            state.pushing.insert(key.clone());
            state.last.insert(key.clone(), Instant::now());
            failures
        };
        let entry = Entry {
            source: source.to_string(),
            due: Instant::now(),
            failures,
        };
        let pushed = (self.shared.push)(mirror, source, branch);
        self.shared.settle(key.clone(), &entry, pushed.clone());
        {
            let mut state = self.shared.lock();
            state.pushing.remove(&key);
            if matches!(pushed, Pushed::Failed(_)) {
                self.start_thread(&mut state);
            }
        }
        self.shared.wake.notify_all();
        pushed
    }

    /// Push every waiting branch now, on this thread, after the pushes in
    /// flight: a process that exits (`spawn tick`) leaves nothing behind. A
    /// push that fails here waits again, like any failed push, for a
    /// process that goes on; how many failed.
    pub(crate) fn flush(&self) -> usize {
        let waiting: Vec<(Key, String)> = {
            let mut state = self.shared.lock();
            let deadline = Instant::now() + IN_FLIGHT_WAIT;
            while !state.pushing.is_empty() && Instant::now() < deadline {
                state = self
                    .shared
                    .wake
                    .wait_timeout(state, Duration::from_millis(100))
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
            state
                .dirty
                .iter()
                .map(|(key, entry)| (key.clone(), entry.source.clone()))
                .collect()
        };
        let mut failed = 0;
        for ((mirror, branch), source) in waiting {
            match self.push_now(&mirror, &source, &branch) {
                Pushed::Done => {}
                Pushed::Rejected(why) | Pushed::Failed(why) => {
                    failed += 1;
                    eprintln!("sources: push of {branch} to {source}: {why}");
                }
            }
        }
        failed
    }
}

#[cfg(test)]
mod tests {
    use super::{Pushed, Pusher};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// A pusher whose pushes are counted and answered by `answer(n)`.
    fn counting(
        answer: impl Fn(usize) -> Pushed + Send + Sync + 'static,
        interval: Duration,
    ) -> (Pusher, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let pusher = Pusher::with(
            Box::new(move |_: &Path, _: &str, _: &str| answer(seen.fetch_add(1, Ordering::SeqCst))),
            interval,
            Duration::from_millis(20),
        );
        (pusher, count)
    }

    fn wait_for(count: &AtomicUsize, at_least: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while count.load(Ordering::SeqCst) < at_least && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_burst_of_checkpoints_costs_at_most_two_pushes() {
        let (pusher, count) = counting(|_| Pushed::Done, Duration::from_millis(200));
        let mirror = Path::new("/m");
        for _ in 0..20 {
            pusher.mark(mirror, "/s", "agent_a");
        }
        wait_for(&count, 1);
        for _ in 0..20 {
            pusher.mark(mirror, "/s", "agent_a");
        }
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "the first at once, the rest after the interval"
        );
        pusher.mark(mirror, "/s", "agent_b");
        wait_for(&count, 3);
        assert_eq!(
            count.load(Ordering::SeqCst),
            3,
            "another branch is not held back"
        );
    }

    #[test]
    fn a_failed_push_is_retried_and_a_rejected_one_is_not() {
        let (pusher, count) = counting(
            |n| {
                if n < 2 {
                    Pushed::Failed("unreachable".to_string())
                } else {
                    Pushed::Done
                }
            },
            Duration::from_millis(1),
        );
        pusher.mark(Path::new("/m"), "/s", "agent_a");
        wait_for(&count, 3);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(count.load(Ordering::SeqCst), 3, "two failures, then done");

        let (pusher, count) = counting(
            |_| Pushed::Rejected("! non-fast-forward".to_string()),
            Duration::from_millis(1),
        );
        pusher.mark(Path::new("/m"), "/s", "agent_a");
        wait_for(&count, 1);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "a rejection is not retried"
        );
    }

    #[test]
    fn flush_pushes_what_waits_at_once() {
        let branches: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = Arc::clone(&branches);
        let pusher = Pusher::with(
            Box::new(move |_: &Path, _: &str, branch: &str| {
                seen.lock().expect("lock").push(branch.to_string());
                Pushed::Done
            }),
            Duration::from_mins(1),
            Duration::from_mins(1),
        );
        pusher.mark(Path::new("/m"), "/s", "agent_a");
        let deadline = Instant::now() + Duration::from_secs(10);
        while branches.lock().expect("lock").is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        // Inside the interval: only a flush pushes it now.
        pusher.mark(Path::new("/m"), "/s", "agent_a");
        pusher.flush();
        assert_eq!(*branches.lock().expect("lock"), vec!["agent_a", "agent_a"]);
    }

    /// Issue #8: a push made at once for a change that must be published
    /// (a merge of main) and failing once is not dropped: the branch waits
    /// again and the pusher's thread pushes it.
    #[test]
    fn a_push_now_that_fails_once_is_retried_not_dropped() {
        let (pusher, count) = counting(
            |n| {
                if n == 0 {
                    Pushed::Failed("unreachable".to_string())
                } else {
                    Pushed::Done
                }
            },
            Duration::from_millis(1),
        );
        let mirror = Path::new("/m");
        assert_eq!(
            pusher.push_now(mirror, "/s", "agent_a"),
            Pushed::Failed("unreachable".to_string())
        );
        assert!(pusher.waiting(mirror, "agent_a"), "it waits for its retry");
        wait_for(&count, 2);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(count.load(Ordering::SeqCst), 2, "failed once, then done");
        assert!(!pusher.waiting(mirror, "agent_a"));

        // A refusal is not retried: the branch was rewritten, and a person
        // must look.
        let (pusher, count) = counting(
            |_| Pushed::Rejected("! non-fast-forward".to_string()),
            Duration::from_millis(1),
        );
        assert!(matches!(
            pusher.push_now(mirror, "/s", "agent_a"),
            Pushed::Rejected(_)
        ));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(!pusher.waiting(mirror, "agent_a"));
    }

    /// Issue #8: a flush whose push fails leaves the branch waiting, and the
    /// pusher's thread pushes it; it used to be dropped, and a merge commit
    /// then never reached the source.
    #[test]
    fn a_flush_whose_push_fails_leaves_the_branch_waiting() {
        let (pusher, count) = counting(
            |n| {
                if n == 1 {
                    Pushed::Failed("ref lock".to_string())
                } else {
                    Pushed::Done
                }
            },
            Duration::from_mins(1),
        );
        let mirror = Path::new("/m");
        assert_eq!(pusher.push_now(mirror, "/s", "agent_a"), Pushed::Done);
        // Inside the interval: only the flush pushes it, and that fails.
        pusher.mark(mirror, "/s", "agent_a");
        assert_eq!(pusher.flush(), 1);
        assert!(pusher.waiting(mirror, "agent_a"), "still owed its push");
        wait_for(&count, 3);
        assert_eq!(count.load(Ordering::SeqCst), 3, "the retry pushed it");
        assert!(!pusher.waiting(mirror, "agent_a"));
    }

    /// A push made at once never runs beside the thread's push of the same
    /// branch: two pushes of one ref race for its lock (issue #8's lost
    /// merge push).
    #[test]
    fn a_push_now_waits_for_the_threads_push_of_the_same_branch() {
        let running = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let (seen_running, seen_most) = (Arc::clone(&running), Arc::clone(&most));
        let pusher = Pusher::with(
            Box::new(move |_: &Path, _: &str, _: &str| {
                let now = seen_running.fetch_add(1, Ordering::SeqCst) + 1;
                seen_most.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
                seen_running.fetch_sub(1, Ordering::SeqCst);
                Pushed::Done
            }),
            Duration::from_millis(1),
            Duration::from_millis(1),
        );
        let mirror = Path::new("/m");
        for _ in 0..5 {
            pusher.mark(mirror, "/s", "agent_a");
            std::thread::sleep(Duration::from_millis(10));
            assert_eq!(pusher.push_now(mirror, "/s", "agent_a"), Pushed::Done);
        }
        assert_eq!(most.load(Ordering::SeqCst), 1, "never two at once");
    }

    /// Issue #12: a source has one pusher per process, whatever asks for it
    /// and however often, and that pusher one thread.
    #[test]
    fn a_source_has_one_pusher_and_one_thread_per_process() {
        let source = format!("/spawn-push-shared-{}", std::process::id());
        let first = super::shared(&source);
        let others: Vec<Arc<Pusher>> = (0..8)
            .map(|_| {
                let source = source.clone();
                std::thread::spawn(move || super::shared(&source))
                    .join()
                    .expect("joined")
            })
            .collect();
        assert!(others.iter().all(|other| Arc::ptr_eq(&first, other)));
        assert!(!Arc::ptr_eq(&first, &super::shared("/another-source")));
        assert_eq!(first.threads(), 0, "no thread before the first push");
    }
}
