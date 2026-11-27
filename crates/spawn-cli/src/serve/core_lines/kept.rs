//! Kept groups (sync/design-core-lines-kept-figures.md): a group of Core
//! lines figures whose reading is slow (a walk of the disk, the 7-day
//! scorecard) is refreshed beside the readings, never inside one, and every
//! reading says how old what it served is.
//!
//! A reading that finds its group older than the group's cadence starts one
//! refresh on a thread of its own (one at a time) and serves the kept group
//! at once. Only a process's first reading waits, at most the group's first
//! wait from that refresh's start, one deadline for every reading (as
//! [`super::unowned`]'s cache, cc-astra A28); past it the reading has
//! nothing to serve until the refresh ends. A refresh that fails, or
//! panics, keeps the group it had: its age grows, and the error is kept to
//! be said. Each refresh says one line in its process's log: `core lines:
//! <group> refreshed in <n> ms (<its parts>)` or `core lines: <group>
//! refresh failed after <n> ms: <error>`.
//!
//! Every group in use in the process is listed ([`statuses`]): its
//! [`Status`] goes into the answer (`groups`), the reading's parts line (each
//! group's age) and the alert step (`core_lines_stale`). A group older than
//! its stale line is stale: shown as stale, never as current.

use std::collections::BTreeMap;
use std::sync::Mutex;
#[cfg(test)]
use std::sync::{Arc, Condvar};
#[cfg(test)]
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// A group's state as a reading sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Status {
    /// The group's name (`performance`, `storage`).
    pub(crate) group: &'static str,
    /// When the kept group was read (the reading's now that started its
    /// refresh); `None` before a refresh has kept one.
    pub(crate) as_of_ms: Option<i64>,
    /// When this process first tried to read the group.
    pub(crate) first_tried_ms: Option<i64>,
    /// How old the group may be before a reading refreshes it.
    pub(crate) every_ms: i64,
    /// How old the group may be before it is stale.
    pub(crate) stale_ms: i64,
    /// The last refresh's error, while no refresh has succeeded since.
    pub(crate) last_error: Option<String>,
    /// How long the last refresh that ended took.
    pub(crate) refresh_ms: Option<u64>,
}

impl Status {
    /// How old the kept group is at `now_ms` (`None` without one).
    pub(crate) fn age_ms(&self, now_ms: i64) -> Option<i64> {
        self.as_of_ms
            .map(|as_of| now_ms.saturating_sub(as_of).max(0))
    }

    /// From when the group is stale: its stale line after the kept group's
    /// read, or after the first try while none was kept; `None` before any.
    pub(crate) fn stale_since_ms(&self) -> Option<i64> {
        self.as_of_ms
            .or(self.first_tried_ms)
            .map(|from| from.saturating_add(self.stale_ms))
    }

    /// Whether the group is stale at `now_ms`.
    pub(crate) fn stale(&self, now_ms: i64) -> bool {
        self.stale_since_ms().is_some_and(|since| now_ms >= since)
    }

    /// The answer's entry for the group at `now_ms`.
    pub(crate) fn json(&self, now_ms: i64) -> Value {
        json!({
            "group": self.group,
            "as_of_ms": self.as_of_ms,
            "age_ms": self.age_ms(now_ms),
            "every_ms": self.every_ms,
            "stale_ms": self.stale_ms,
            "stale": self.stale(now_ms),
            "stale_since_ms": self.stale_since_ms(),
            "last_error": self.last_error,
            "refresh_ms": self.refresh_ms,
        })
    }
}

/// What a reading is served: the kept group, if any, and when it was read.
#[cfg(test)]
pub(crate) struct Served<T> {
    pub(crate) value: Option<Arc<T>>,
    pub(crate) as_of_ms: Option<i64>,
}

#[cfg(test)]
struct State<T> {
    kept: Option<(i64, Arc<T>)>,
    refreshing: bool,
    /// While nothing is kept: when the wait for the refresh under way ends,
    /// set once as that refresh starts, so every reading shares it.
    first_deadline: Option<Instant>,
    first_tried_ms: Option<i64>,
    last_error: Option<String>,
    refresh_ms: Option<u64>,
}

/// One kept group.
#[cfg(test)]
pub(crate) struct Group<T> {
    name: &'static str,
    every_ms: i64,
    stale_ms: i64,
    first_wait: Duration,
    state: Mutex<State<T>>,
    done: Condvar,
}

/// A group's status, whatever it keeps: what [`statuses`] lists.
trait Report: Sync {
    fn status(&self) -> Status;
}

#[cfg(test)]
impl<T: Send + Sync + 'static> Report for Group<T> {
    fn status(&self) -> Status {
        Group::status(self)
    }
}

/// Every group a reading has used in this process, by name.
static GROUPS: Mutex<BTreeMap<&'static str, &'static dyn Report>> = Mutex::new(BTreeMap::new());

/// The status of every group in use in this process, by name.
pub(crate) fn statuses() -> Vec<Status> {
    GROUPS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .map(|group| group.status())
        .collect()
}

/// Clears `refreshing` however the refresh ends (a panic too, said as the
/// error), and wakes whoever waits.
#[cfg(test)]
struct Refreshing<T: Send + Sync + 'static> {
    group: &'static Group<T>,
    begun: Instant,
    ended: bool,
}

#[cfg(test)]
impl<T: Send + Sync + 'static> Drop for Refreshing<T> {
    fn drop(&mut self) {
        let mut state = self.group.lock();
        state.refreshing = false;
        if !self.ended {
            let took = self.begun.elapsed();
            state.refresh_ms = Some(millis(took));
            state.last_error = Some("the refresh panicked".to_string());
            eprintln!(
                "core lines: {} refresh failed after {} ms: the refresh panicked",
                self.group.name,
                took.as_millis()
            );
        }
        drop(state);
        self.group.done.notify_all();
    }
}

#[cfg(test)]
fn millis(took: Duration) -> u64 {
    u64::try_from(took.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
impl<T: Send + Sync + 'static> Group<T> {
    /// A group named `group`, refreshed when older than `every_ms`, stale
    /// past `stale_ms`, its first refresh waited for at most `first_wait`.
    pub(crate) const fn new(
        group: &'static str,
        every_ms: i64,
        stale_ms: i64,
        first_wait: Duration,
    ) -> Self {
        Group {
            name: group,
            every_ms,
            stale_ms,
            first_wait,
            state: Mutex::new(State {
                kept: None,
                refreshing: false,
                first_deadline: None,
                first_tried_ms: None,
                last_error: None,
                refresh_ms: None,
            }),
            done: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State<T>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The group's status now.
    pub(crate) fn status(&self) -> Status {
        let state = self.lock();
        Status {
            group: self.name,
            as_of_ms: state.kept.as_ref().map(|(at, _)| *at),
            first_tried_ms: state.first_tried_ms,
            every_ms: self.every_ms,
            stale_ms: self.stale_ms,
            last_error: state.last_error.clone(),
            refresh_ms: state.refresh_ms,
        }
    }

    /// The group for a reading at `now_ms`: the kept one, and a refresh
    /// started through `refresh` (on a thread of its own, one at a time)
    /// when it is older than the cadence or there is none. `refresh` reads
    /// the group at its moment and gives it with the line its log says
    /// after "refreshed in <n> ms", or why it could not. With nothing kept,
    /// the refresh is waited for at most the first wait from its start.
    pub(crate) fn get(
        &'static self,
        now_ms: i64,
        refresh: impl FnOnce(i64) -> Result<(T, String), String> + Send + 'static,
    ) -> Served<T> {
        GROUPS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(self.name)
            .or_insert(self);
        let mut state = self.lock();
        let fresh = state
            .kept
            .as_ref()
            .is_some_and(|(at, _)| (0..self.every_ms).contains(&now_ms.saturating_sub(*at)));
        if !fresh && !state.refreshing {
            state.refreshing = true;
            state.first_tried_ms.get_or_insert(now_ms);
            state.first_deadline = state
                .kept
                .is_none()
                .then(|| Instant::now() + self.first_wait);
            let group: &'static Group<T> = self;
            std::thread::spawn(move || {
                let mut guard = Refreshing {
                    group,
                    begun: Instant::now(),
                    ended: false,
                };
                let read = refresh(now_ms);
                let took = guard.begun.elapsed();
                let mut state = group.lock();
                state.refresh_ms = Some(millis(took));
                match read {
                    Ok((value, said)) => {
                        state.kept = Some((now_ms, Arc::new(value)));
                        state.last_error = None;
                        eprintln!(
                            "core lines: {} refreshed in {} ms ({said})",
                            group.name,
                            took.as_millis()
                        );
                    }
                    Err(error) => {
                        eprintln!(
                            "core lines: {} refresh failed after {} ms: {error}",
                            group.name,
                            took.as_millis()
                        );
                        state.last_error = Some(error);
                    }
                }
                guard.ended = true;
                drop(state);
                drop(guard);
            });
        }
        if state.kept.is_none() && state.refreshing {
            let left = state.first_deadline.map_or(Duration::ZERO, |deadline| {
                deadline.saturating_duration_since(Instant::now())
            });
            state = self
                .done
                .wait_timeout_while(state, left, |state| {
                    state.kept.is_none() && state.refreshing
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        Served {
            value: state.kept.as_ref().map(|(_, value)| Arc::clone(value)),
            as_of_ms: state.kept.as_ref().map(|(at, _)| *at),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Group, Status, statuses};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const EVERY: i64 = 60_000;
    const STALE: i64 = 900_000;

    fn leaked(name: &'static str, first_wait: Duration) -> &'static Group<u32> {
        Box::leak(Box::new(Group::new(name, EVERY, STALE, first_wait)))
    }

    /// A test refresh, the sender that lets it end, and the receiver that
    /// says it began.
    type Gated = (
        Box<dyn FnOnce(i64) -> Result<(u32, String), String> + Send>,
        mpsc::Sender<()>,
        mpsc::Receiver<()>,
    );

    /// A refresh that gives `value` once `gate` lets it, and says when it
    /// began.
    fn gated(value: Result<u32, String>) -> Gated {
        let (open, gate) = mpsc::channel::<()>();
        let (began, began_rx) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        (
            Box::new(move |_| {
                let _ = began.send(());
                let _ = gate.lock().expect("gate").recv();
                value.map(|value| (value, "parts".to_string()))
            }),
            open,
            began_rx,
        )
    }

    /// Refreshes that must never start: each one counts here.
    static NEVER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// A refresh that must not start (a reading within the cadence, or
    /// beside one already running): it counts in [`NEVER`], which every
    /// test that uses it checks is still 0.
    fn never() -> impl FnOnce(i64) -> Result<(u32, String), String> + Send + 'static {
        |_| {
            NEVER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err("a refresh that must not start".to_string())
        }
    }

    fn none_started() {
        assert_eq!(
            NEVER.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a refresh started that must not"
        );
    }

    /// Wait until the group's refresh has ended.
    fn settled(group: &'static Group<u32>) {
        let begun = Instant::now();
        while group.lock().refreshing {
            assert!(
                begun.elapsed() < Duration::from_secs(10),
                "the refresh ended"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_reading_while_a_refresh_runs_is_served_the_kept_group_at_once() {
        let group = leaked("test-kept-at-once", Duration::from_secs(5));
        let (first, open, _) = gated(Ok(1));
        open.send(()).expect("open");
        assert_eq!(group.get(1_000, first).value.as_deref(), Some(&1));
        settled(group);
        // Due again: the refresh blocks, and the reading does not wait on it.
        let (second, open, began) = gated(Ok(2));
        let asked = Instant::now();
        let served = group.get(1_000 + EVERY, second);
        assert!(asked.elapsed() < Duration::from_millis(500), "no wait");
        assert_eq!(served.value.as_deref(), Some(&1));
        assert_eq!(served.as_of_ms, Some(1_000));
        began.recv().expect("the refresh began");
        // Another reading meanwhile starts no second refresh.
        let served = group.get(1_000 + EVERY + 1, never());
        assert_eq!(served.value.as_deref(), Some(&1));
        open.send(()).expect("open");
        settled(group);
        let served = group.get(1_000 + EVERY + 2, never());
        assert_eq!(served.value.as_deref(), Some(&2));
        assert_eq!(served.as_of_ms, Some(1_000 + EVERY));
        let status = group.status();
        assert_eq!(status.age_ms(1_000 + EVERY + 2), Some(2));
        assert!(status.refresh_ms.is_some());
        settled(group);
        none_started();
    }

    #[test]
    fn the_first_reading_waits_at_most_the_first_wait_and_then_has_nothing() {
        let group = leaked("test-kept-first-wait", Duration::from_millis(200));
        let (slow, open, _) = gated(Ok(7));
        let asked = Instant::now();
        let served = group.get(5_000, slow);
        let waited = asked.elapsed();
        assert!(served.value.is_none(), "nothing kept yet");
        assert!(
            waited >= Duration::from_millis(150) && waited < Duration::from_secs(3),
            "waited {waited:?}"
        );
        // A second reading shares the deadline: it is past, so no wait.
        let asked = Instant::now();
        assert!(group.get(5_001, never()).value.is_none());
        assert!(asked.elapsed() < Duration::from_millis(100));
        open.send(()).expect("open");
        settled(group);
        assert_eq!(group.get(5_002, never()).value.as_deref(), Some(&7));
        settled(group);
        none_started();
    }

    #[test]
    fn a_failed_refresh_keeps_the_group_and_its_age_grows_to_stale() {
        let group = leaked("test-kept-failed", Duration::from_secs(5));
        let (first, open, _) = gated(Ok(3));
        open.send(()).expect("open");
        group.get(10_000, first);
        settled(group);
        let (failing, open, _) = gated(Err("the store is down".to_string()));
        open.send(()).expect("open");
        let served = group.get(10_000 + EVERY, failing);
        assert_eq!(served.value.as_deref(), Some(&3));
        settled(group);
        let status = group.status();
        assert_eq!(status.as_of_ms, Some(10_000));
        assert_eq!(status.last_error.as_deref(), Some("the store is down"));
        assert!(!status.stale(10_000 + STALE - 1));
        assert!(status.stale(10_000 + STALE));
        assert_eq!(status.stale_since_ms(), Some(10_000 + STALE));
        assert_eq!(status.json(10_000 + STALE)["stale"], true);
        // A good refresh clears the error and the age.
        let (good, open, _) = gated(Ok(4));
        open.send(()).expect("open");
        group.get(10_000 + STALE, good);
        settled(group);
        let status = group.status();
        assert_eq!(status.last_error, None);
        assert!(!status.stale(10_000 + STALE));
    }

    #[test]
    fn a_refresh_that_panics_keeps_the_group_and_says_so() {
        let group = leaked("test-kept-panic", Duration::from_secs(5));
        let (first, open, _) = gated(Ok(5));
        open.send(()).expect("open");
        group.get(1, first);
        settled(group);
        group.get(1 + EVERY, |_| panic!("boom"));
        settled(group);
        let status = group.status();
        assert_eq!(status.as_of_ms, Some(1));
        assert_eq!(status.last_error.as_deref(), Some("the refresh panicked"));
    }

    #[test]
    fn a_group_with_nothing_kept_is_stale_from_its_first_try() {
        let status = Status {
            group: "g",
            as_of_ms: None,
            first_tried_ms: Some(100),
            every_ms: EVERY,
            stale_ms: STALE,
            last_error: Some("down".into()),
            refresh_ms: None,
        };
        assert_eq!(status.age_ms(200), None);
        assert!(!status.stale(100 + STALE - 1));
        assert!(status.stale(100 + STALE));
    }

    #[test]
    fn every_group_in_use_is_listed() {
        let group = leaked("test-kept-listed", Duration::from_secs(5));
        let (first, open, _) = gated(Ok(9));
        open.send(()).expect("open");
        group.get(1, first);
        settled(group);
        let listed: Vec<Arc<str>> = statuses()
            .into_iter()
            .map(|status| Arc::from(status.group))
            .collect();
        assert!(listed.iter().any(|name| &**name == "test-kept-listed"));
    }
}
