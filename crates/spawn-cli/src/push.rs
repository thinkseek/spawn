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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Least time between two pushes of one branch.
pub(crate) const INTERVAL: Duration = Duration::from_secs(5);

/// Pause after a first failed push; it doubles per failure up to
/// [`MAX_RETRY`].
const RETRY: Duration = Duration::from_secs(5);

/// Longest pause between retries.
const MAX_RETRY: Duration = Duration::from_mins(5);

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
    /// Pushes running on the background thread.
    in_flight: usize,
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
    fn run(&self) {
        loop {
            let batch: Vec<(Key, Entry)> = {
                let mut state = self.lock();
                loop {
                    let now = Instant::now();
                    let due: Vec<Key> = state
                        .dirty
                        .iter()
                        .filter(|(_, entry)| entry.due <= now)
                        .map(|(key, _)| key.clone())
                        .collect();
                    if !due.is_empty() {
                        let batch: Vec<(Key, Entry)> = due
                            .into_iter()
                            .filter_map(|key| state.dirty.remove_entry(&key))
                            .collect();
                        for (key, _) in &batch {
                            state.last.insert(key.clone(), now);
                        }
                        state.in_flight += batch.len();
                        break batch;
                    }
                    let wait = state
                        .dirty
                        .values()
                        .map(|entry| entry.due.saturating_duration_since(now))
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
                self.settle(key, &entry, pushed);
                self.lock().in_flight -= 1;
                self.wake.notify_all();
            }
        }
    }
}

/// Coalescing background pusher of agents' branches.
pub(crate) struct Pusher {
    shared: Arc<Shared>,
}

impl Pusher {
    /// Pusher that runs `git push`.
    pub(crate) fn new() -> Self {
        Self::with(Box::new(push_branch), INTERVAL, RETRY)
    }

    /// Pusher over another push function and timing, for tests.
    fn with(push: Box<PushFn>, interval: Duration, retry: Duration) -> Self {
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
        if !state.started {
            state.started = true;
            let shared = Arc::clone(&self.shared);
            let spawned = std::thread::Builder::new()
                .name("spawn-push".to_string())
                .spawn(move || shared.run());
            if let Err(error) = spawned {
                state.started = false;
                eprintln!("sources: no pusher thread: {error}");
            }
        }
        drop(state);
        self.shared.wake.notify_all();
    }

    /// Push every waiting branch now, on this thread, after the pushes in
    /// flight: a process that exits (`spawn tick`) leaves nothing behind.
    pub(crate) fn flush(&self) {
        let batch: Vec<(Key, Entry)> = {
            let mut state = self.shared.lock();
            let deadline = Instant::now() + Duration::from_secs(30);
            while state.in_flight > 0 && Instant::now() < deadline {
                state = self
                    .shared
                    .wake
                    .wait_timeout(state, Duration::from_millis(100))
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
            state.dirty.drain().collect()
        };
        for (key, entry) in batch {
            match (self.shared.push)(&key.0, &entry.source, &key.1) {
                Pushed::Done => {}
                Pushed::Rejected(why) | Pushed::Failed(why) => {
                    eprintln!("sources: push of {} to {}: {why}", key.1, entry.source);
                }
            }
        }
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
}
