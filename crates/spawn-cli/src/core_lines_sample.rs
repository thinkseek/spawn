//! The Core lines figures' history (objective 090): the loop records a
//! `host.core_lines` sample, `{"type": "host.core_lines", "values": {<key>:
//! <value>}}`, holding every figure of `GET /api/core-lines` that has a
//! value as the answer would give it then, at its start and then every
//! `SPAWN_CORE_LINES_SAMPLE_SECS` (default 3600). The answer's 7-day trends
//! are read from those samples (crate serve/core_lines.rs).
//!
//! Reading the figures is the answer's own work (the storage report, the
//! scorecard, the backups, the nightly's facts), so it never runs inside a
//! pass: it runs on a thread of its own, on the dashboard's read-only
//! connection and its statement timeout, and the pass only looks whether it
//! has come back. One that has not after [`READ_LIMIT_MS`] is given up. A
//! sample that cannot be read or written is a line in the loop's log, never
//! a failed pass, and is tried again a minute later, as the storage sample
//! is ([`crate::storage::Sampler`]). No gate, integrator or deploy step
//! reads the samples: monitoring only.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread::JoinHandle;

use serde_json::{Map, Value, json};
use spawn_postgres::PostgresStore;

/// The interval between samples when `SPAWN_CORE_LINES_SAMPLE_SECS` names
/// none.
pub(crate) const SAMPLE_EVERY_SECS: u64 = 3600;

/// How soon a sample that could not be read or written is tried again.
const RETRY_MS: i64 = 60_000;

/// How long a reading may take before it is given up.
pub(crate) const READ_LIMIT_MS: i64 = 300_000;

/// The figures' values, as a reading gives them.
type Values = Result<Map<String, Value>, String>;

/// One reading of the figures, running or done: its answer comes on `answer`;
/// `worker` is the thread computing it (none when it was read in place).
pub(crate) struct Reading {
    answer: Receiver<Values>,
    worker: Option<JoinHandle<()>>,
}

impl Reading {
    /// A reading on a thread of its own.
    pub(crate) fn spawn(read: impl FnOnce() -> Values + Send + 'static) -> Self {
        let (send, answer) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("core-lines-sample".to_string())
            .spawn(move || {
                // The loop may have given the reading up: nobody listens.
                let _ = send.send(read());
            })
            .ok();
        if worker.is_none() {
            // No thread: an answer that says so, read at once.
            let (send, answer) = std::sync::mpsc::channel();
            let _ = send.send(Err("could not start the reading's thread".to_string()));
            return Self {
                answer,
                worker: None,
            };
        }
        Self { answer, worker }
    }

    /// A reading already done: `values`.
    pub(crate) fn done(values: Values) -> Self {
        let (send, answer) = std::sync::mpsc::channel();
        let _ = send.send(values);
        Self {
            answer,
            worker: None,
        }
    }

    /// Whether its thread is still running.
    fn running(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
    }
}

/// The loop's `host.core_lines` samples: one at its start, then one every
/// `every_ms`.
pub(crate) struct Sampler {
    every_ms: i64,
    /// When the next sample is due.
    due_ms: i64,
    /// The reading in flight, and when it started.
    reading: Option<(Reading, i64)>,
    /// A reading given up whose thread may still run: no other starts
    /// before it ends, so a slow reading never piles up threads.
    abandoned: Option<Reading>,
}

impl Sampler {
    /// The sampler `SPAWN_CORE_LINES_SAMPLE_SECS` sets, with a warning for
    /// a setting that is not a number of seconds above 0.
    pub(crate) fn from_env() -> (Self, Option<String>) {
        let name = "SPAWN_CORE_LINES_SAMPLE_SECS";
        let (secs, warning) = match crate::config::env_value(name) {
            None => (SAMPLE_EVERY_SECS, None),
            Some(text) => match text.trim().parse::<u64>() {
                Ok(secs) if secs > 0 => (secs, None),
                _ => (
                    SAMPLE_EVERY_SECS,
                    Some(format!(
                        "{name}: {text:?} is not a number of seconds above 0; using {SAMPLE_EVERY_SECS}"
                    )),
                ),
            },
        };
        (
            Self::every(i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX)),
            warning,
        )
    }

    /// A sampler taking one sample every `every_ms`, the first at once.
    pub(crate) fn every(every_ms: i64) -> Self {
        Self {
            every_ms: every_ms.max(1),
            due_ms: i64::MIN,
            reading: None,
            abandoned: None,
        }
    }

    /// Whether a sample is due at `now_ms` (a reading in flight aside).
    pub(crate) fn due(&self, now_ms: i64) -> bool {
        self.due_ms <= now_ms
    }

    /// A failed sample at `now_ms`: tried again a minute later.
    fn failed(&mut self, now_ms: i64, why: &str) -> Result<Option<String>, String> {
        self.due_ms = now_ms.saturating_add(RETRY_MS.min(self.every_ms));
        Err(format!(
            "core lines sample: {why}; trying again in {} s",
            RETRY_MS.min(self.every_ms) / 1000
        ))
    }

    /// When due, start a reading through `start`; when the reading in
    /// flight has come back, write its sample through `write`. Returns the
    /// line for the loop's log; an error is a line too, never a failed pass.
    pub(crate) fn check_with(
        &mut self,
        now_ms: i64,
        start: impl FnOnce() -> Reading,
        write: impl FnOnce(&Value) -> Result<i64, String>,
    ) -> Result<Option<String>, String> {
        if self.abandoned.as_ref().is_some_and(|old| !old.running()) {
            self.abandoned = None;
        }
        if self.reading.is_none() {
            if !self.due(now_ms) || self.abandoned.is_some() {
                return Ok(None);
            }
            self.reading = Some((start(), now_ms));
        }
        let Some((reading, started)) = self.reading.take() else {
            return Ok(None);
        };
        let values = match reading.answer.try_recv() {
            Err(TryRecvError::Empty) => {
                if now_ms.saturating_sub(started) < READ_LIMIT_MS {
                    self.reading = Some((reading, started));
                    return Ok(None);
                }
                self.abandoned = Some(reading);
                return self.failed(
                    now_ms,
                    &format!("the reading took over {} s", READ_LIMIT_MS / 1000),
                );
            }
            Err(TryRecvError::Disconnected) => {
                return self.failed(now_ms, "the reading ended without an answer");
            }
            Ok(Err(error)) => return self.failed(now_ms, &error),
            Ok(Ok(values)) => values,
        };
        let count = values.len();
        let sample = json!({"type": "host.core_lines", "values": values});
        if let Err(error) = write(&sample) {
            return self.failed(now_ms, &error);
        }
        self.due_ms = now_ms.saturating_add(self.every_ms);
        Ok(Some(format!(
            "core lines: sampled {count} figures with a value"
        )))
    }

    /// [`Sampler::check_with`] through the store: the reading on a thread
    /// of its own ([`crate::serve::core_lines::sample_values`]), the sample
    /// through `spawn_host_record`.
    pub(crate) fn check(
        &mut self,
        store: &PostgresStore,
        now_ms: i64,
    ) -> Result<Option<String>, String> {
        self.check_with(
            now_ms,
            || Reading::spawn(crate::serve::core_lines::sample_values),
            |sample| store.host_record(sample).map_err(|error| error.to_string()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{READ_LIMIT_MS, Reading, Sampler};
    use serde_json::{Map, Value, json};

    const H: i64 = 3_600_000;

    fn values() -> Map<String, Value> {
        let mut values = Map::new();
        values.insert("storage.free_bytes".to_string(), json!(120));
        values
    }

    /// One check at `now`: the reading done at once, the write through
    /// `write`; whether a sample was written.
    fn at(
        sampler: &mut Sampler,
        now: i64,
        written: &mut Vec<(i64, Value)>,
        fails: bool,
    ) -> Result<bool, String> {
        sampler
            .check_with(
                now,
                || Reading::done(Ok(values())),
                |sample| {
                    if fails {
                        return Err("down".to_string());
                    }
                    written.push((now, sample.clone()));
                    Ok(1)
                },
            )
            .map(|line| line.is_some())
    }

    #[test]
    fn the_loop_samples_at_its_start_then_hourly_and_retries_a_failed_write() {
        let mut sampler = Sampler::every(H);
        let mut written = Vec::new();
        let start = 1_000_000;
        assert_eq!(at(&mut sampler, start, &mut written, false), Ok(true));
        assert_eq!(at(&mut sampler, start + 1, &mut written, false), Ok(false));
        assert_eq!(at(&mut sampler, start + H - 1, &mut written, false), Ok(false));
        // The hour's sample cannot be written: a line, tried a minute later.
        let failed = at(&mut sampler, start + H, &mut written, true);
        assert!(failed.as_ref().is_err_and(|line| line.contains("down")), "{failed:?}");
        assert_eq!(at(&mut sampler, start + H + 59_999, &mut written, false), Ok(false));
        assert_eq!(at(&mut sampler, start + H + 60_000, &mut written, false), Ok(true));
        // The next is an hour after the one written.
        assert_eq!(at(&mut sampler, start + 2 * H, &mut written, false), Ok(false));
        assert_eq!(at(&mut sampler, start + 2 * H + 60_000, &mut written, false), Ok(true));
        assert_eq!(
            written.iter().map(|(now, _)| *now).collect::<Vec<_>>(),
            [start, start + H + 60_000, start + 2 * H + 60_000]
        );
        assert_eq!(
            written[0].1,
            json!({"type": "host.core_lines", "values": {"storage.free_bytes": 120}})
        );
    }

    #[test]
    fn a_reading_that_failed_is_tried_again_in_a_minute() {
        let mut sampler = Sampler::every(H);
        let result = sampler.check_with(0, || Reading::done(Err("no store".to_string())), |_| Ok(1));
        assert!(result.is_err_and(|line| line.contains("no store")));
        assert!(!sampler.due(59_999));
        assert!(sampler.due(60_000));
    }

    /// A reading runs beside the passes: a pass never waits for it, and
    /// one that takes over the limit is given up and tried again, never
    /// beside the one still running.
    #[test]
    fn a_slow_reading_never_holds_a_pass() {
        let (release, hold) = std::sync::mpsc::channel::<()>();
        let mut sampler = Sampler::every(H);
        let mut started = 0;
        let mut written = 0;
        let mut check = |sampler: &mut Sampler, now: i64, hold: Option<std::sync::mpsc::Receiver<()>>| {
            let begun = std::time::Instant::now();
            let result = sampler.check_with(
                now,
                || {
                    started += 1;
                    Reading::spawn(move || {
                        if let Some(hold) = hold {
                            let _ = hold.recv();
                        }
                        Ok(super::tests::values())
                    })
                },
                |_| {
                    written += 1;
                    Ok(1)
                },
            );
            assert!(begun.elapsed() < std::time::Duration::from_secs(2), "the pass waited");
            result
        };
        assert_eq!(check(&mut sampler, 0, Some(hold)), Ok(None));
        assert_eq!(check(&mut sampler, 1000, None), Ok(None), "still reading");
        let given_up = check(&mut sampler, READ_LIMIT_MS, None);
        assert!(given_up.is_err_and(|line| line.contains("took over")));
        // Due again a minute later, but the old reading still runs.
        assert_eq!(check(&mut sampler, READ_LIMIT_MS + 60_000, None), Ok(None));
        release.send(()).expect("release the reading");
        // Once it has ended, a new reading starts.
        let mut now = READ_LIMIT_MS + 61_000;
        let mut line = Ok(None);
        for _ in 0..200 {
            line = check(&mut sampler, now, None);
            if line != Ok(None) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            now += 10;
        }
        assert!(line.is_ok_and(|line| line.is_some()));
        drop(check);
        assert_eq!(started, 2);
        assert_eq!(written, 1);
    }
}
