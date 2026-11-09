//! The model judge's calls, watched: one `host.judge` control record per
//! crossing between answering and failing, so the loop's alert step
//! (`judge_down`, crate alerts.rs) rests on a recorded fact.
//!
//! A judge that is down leaves few traces in the log: the Observer records
//! a failure as a `JUDGE_UNAVAILABLE` judgement once and retries without
//! writing each retry, so counting judgements saw a whole afternoon's
//! outage (2026-11-09, the relay's proxy forwarding nothing) as rare. The
//! watcher sits between the Observer and the judge and sees every call:
//! the first failure after an answer records `down` (with the call's error,
//! its credentials scrubbed), the first answer after failures records `up`
//! (with `failed_calls` since the `down`). Nothing is recorded per call.
//! A restarted watcher reads the latest record of its judge back, so it
//! records neither crossing twice. A record that cannot be written is said
//! once and tried again at the next call; the judgement goes on regardless.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use spawn_core::observe::{JudgeError, JudgeInput, Judgement, StateJudge};
use spawn_postgres::PostgresStore;

/// The longest error text a `down` record keeps.
const ERROR_CHARS: usize = 300;

/// Where the watched judge stands, as recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Standing {
    /// Answering (or nothing recorded yet).
    Up,
    /// Failing since its `down` record, with the calls failed since.
    Down { failed: u64 },
}

/// A [`StateJudge`] that records its judge's crossings (`host.judge`).
pub(crate) struct WatchedJudge {
    inner: Arc<dyn StateJudge>,
    store: Arc<PostgresStore>,
    standing: Mutex<Standing>,
    /// The last failure to record said, so a store that refuses is said
    /// once per message, not at every call.
    said: Mutex<Option<String>>,
}

impl WatchedJudge {
    /// Watch `inner`, starting from its latest `host.judge` record.
    pub(crate) fn new(inner: Arc<dyn StateJudge>, store: Arc<PostgresStore>) -> Self {
        let standing = match store.host_judge() {
            Ok(records) => standing_of(&records, inner.name()),
            Err(error) => {
                eprintln!(
                    "judge watch: cannot read the judge's last record ({error}); \
                     a failure records `down` again"
                );
                Standing::Up
            }
        };
        Self {
            inner,
            store,
            standing: Mutex::new(standing),
            said: Mutex::new(None),
        }
    }

    fn record(&self, record: &Value) -> bool {
        match self.store.host_record(record) {
            Ok(_) => {
                *self
                    .said
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                true
            }
            Err(error) => {
                let message = error.to_string();
                let mut said = self
                    .said
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if said.as_deref() != Some(message.as_str()) {
                    eprintln!("judge watch: cannot record {}: {message}", record["state"]);
                    *said = Some(message);
                }
                false
            }
        }
    }
}

/// The standing of `judge` by its latest record ([`PostgresStore::host_judge`]).
fn standing_of(records: &Value, judge: &str) -> Standing {
    let latest = records
        .as_array()
        .into_iter()
        .flatten()
        .find(|record| record["judge"] == judge);
    match latest.and_then(|record| record["state"].as_str()) {
        Some("down") => Standing::Down { failed: 0 },
        _ => Standing::Up,
    }
}

impl StateJudge for WatchedJudge {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn judge(&self, input: &JudgeInput) -> Result<Judgement, JudgeError> {
        let result = self.inner.judge(input);
        let mut standing = self
            .standing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match (&result, *standing) {
            (Err(error), Standing::Up) => {
                let error: String = crate::alerts::scrub(&error.to_string())
                    .chars()
                    .take(ERROR_CHARS)
                    .collect();
                let record = json!({
                    "type": "host.judge", "judge": self.inner.name(), "state": "down",
                    "error": error,
                });
                if self.record(&record) {
                    *standing = Standing::Down { failed: 1 };
                }
            }
            (Err(_), Standing::Down { failed }) => {
                *standing = Standing::Down {
                    failed: failed.saturating_add(1),
                };
            }
            (Ok(_), Standing::Down { failed }) => {
                let record = json!({
                    "type": "host.judge", "judge": self.inner.name(), "state": "up",
                    "failed_calls": failed,
                });
                if self.record(&record) {
                    *standing = Standing::Up;
                }
            }
            (Ok(_), Standing::Up) => {}
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::{Standing, WatchedJudge, standing_of};
    use serde_json::{Value, json};
    use spawn_core::observation::{Confidence, SemanticState};
    use spawn_core::observe::{JudgeError, JudgeInput, Judgement, StateJudge};
    use spawn_postgres::PostgresStore;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A judge that fails while `down` is set, its error naming a URL with
    /// a password, as a relay's might.
    struct Switch {
        down: AtomicBool,
    }

    impl StateJudge for Switch {
        fn name(&self) -> &'static str {
            "jev"
        }

        fn judge(&self, _: &JudgeInput) -> Result<Judgement, JudgeError> {
            if self.down.load(Ordering::SeqCst) {
                Err(JudgeError::Unavailable(
                    "502 from http://relay:pa55w0rd@host.orb.internal:8118/v1".to_string(),
                ))
            } else {
                Ok(Judgement::of(
                    SemanticState::Working,
                    Confidence::new(0.9).expect("valid confidence"),
                ))
            }
        }
    }

    /// Run `case` on a migrated schema of its own in the test database
    /// (`SPAWN_TEST_DATABASE_URL`, never the live one), dropped afterwards.
    fn in_schema(name: &str, case: impl FnOnce(Arc<PostgresStore>)) {
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let schema = format!("spawn_test_{}_{nanos:x}_{name}", std::process::id());
        let store = Arc::new(
            PostgresStore::connect_in_schema(&url, &schema).expect("a schema of its own, migrated"),
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| case(store)));
        let _ = postgres::Client::connect(&url, postgres::NoTls)
            .and_then(|mut client| client.batch_execute(&format!("drop schema {schema} cascade")));
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }

    /// The `host.judge` records, oldest first.
    fn records(store: &PostgresStore) -> Vec<Value> {
        store
            .backup_records("host.judge", 0, 100)
            .expect("the control stream")
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|entry| entry["record"].clone())
            .collect()
    }

    /// Five failed calls record one `down` (its error scrubbed), two
    /// answers one `up` counting the five; a restarted watcher of a judge
    /// that is down records no second `down`, and records the `up`.
    #[test]
    fn a_judge_records_each_crossing_once_and_a_restart_reads_it_back() {
        in_schema("judge_watch", |store| {
            let switch = Arc::new(Switch {
                down: AtomicBool::new(false),
            });
            let watched = WatchedJudge::new(switch.clone(), store.clone());
            let input = JudgeInput::default();
            assert!(watched.judge(&input).is_ok());
            assert!(records(&store).is_empty(), "an answer records nothing");
            switch.down.store(true, Ordering::SeqCst);
            for _ in 0..5 {
                assert!(
                    watched.judge(&input).is_err(),
                    "the failure is the caller's"
                );
            }
            let down = records(&store);
            assert_eq!(down.len(), 1, "one down for five failures: {down:?}");
            assert_eq!(down[0]["state"], "down");
            assert_eq!(down[0]["judge"], "jev");
            let error = down[0]["error"].as_str().unwrap_or_default();
            assert!(
                error.contains("502") && !error.contains("pa55w0rd"),
                "{error}"
            );

            // A restart while it is down: no second down.
            let restarted = WatchedJudge::new(switch.clone(), store.clone());
            assert!(restarted.judge(&input).is_err());
            assert_eq!(records(&store).len(), 1, "the restart read the down back");
            switch.down.store(false, Ordering::SeqCst);
            assert!(restarted.judge(&input).is_ok());
            assert!(restarted.judge(&input).is_ok());
            let all = records(&store);
            assert_eq!(all.len(), 2, "{all:?}");
            assert_eq!(all[1]["state"], "up");
            assert_eq!(
                all[1]["failed_calls"], 1,
                "the restart counts its own failures"
            );
            assert_eq!(
                standing_of(&store.host_judge().expect("latest"), "jev"),
                Standing::Up
            );
        });
    }

    /// The store checks the record's shape: a judge, a state down or up, a
    /// down's error and an up's failed_calls.
    #[test]
    fn the_store_refuses_a_judge_record_without_its_fields() {
        in_schema("judge_shape", |store| {
            for bad in [
                json!({"type": "host.judge", "state": "down", "error": "x"}),
                json!({"type": "host.judge", "judge": "jev", "state": "sideways"}),
                json!({"type": "host.judge", "judge": "jev", "state": "down"}),
                json!({"type": "host.judge", "judge": "jev", "state": "up"}),
            ] {
                assert!(store.host_record(&bad).is_err(), "{bad}");
            }
            assert!(
                store
                    .host_record(&json!({"type": "host.judge", "judge": "jev",
                                         "state": "down", "error": "timeout"}))
                    .is_ok()
            );
        });
    }
}
