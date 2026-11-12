//! `guardrail_out_of_range` (objective 089): a Core lines figure held out of
//! its healthy range raises one alert, and it clears once held back in.
//!
//! Every `SPAWN_ALERT_GUARDRAIL_SECS` (default 300) the step reads the Core
//! lines figures, the same `GET /api/core-lines` gives, by the same code
//! ([`crate::serve::core_lines::reading`]), the ranges file read at each
//! reading. A figure `out` at every reading for `SPAWN_GUARDRAIL_RAISE_SECS`
//! (default 7200) opens one alert: subject `guardrail <key>`, key
//! `guardrail_out_of_range:<key>:<out since ms>`, evidence `{line, key, name,
//! value, range, out_since_ms, reading_ms}`. A reading not `out` (in range,
//! no range, no value) restarts the count. An open alert clears once its
//! figure has not been `out` at any reading for `SPAWN_GUARDRAIL_CLEAR_SECS`
//! (default 3600), that reading its evidence `{key, state, value, range,
//! reading_ms}`; a reading `out` again restarts that count. An open alert
//! whose figure is no longer in the answer clears at the next reading,
//! `{key, state: "gone", reading_ms}`. The holds keep a figure near its limit
//! from raising and clearing every hour: Needs you being right is the
//! observability north star.
//!
//! The counts live in the step's memory: a restarted loop counts again from
//! its first reading, and adopts an alert the record already holds open
//! (one open per figure, across restarts). A reading that fails is a line in
//! the loop's log and decides nothing.
//!
//! A reading is the whole answer (the 7-day Scorecard with its transcript
//! walk, the cost window, the storage report...), and its one-minute cache
//! has always expired at the step's 300 s, so it never runs inside a pass
//! (cc-astra A18): it runs on a thread of its own, on the dashboard's
//! read-only connection and its statement timeout, as 090's sampler does
//! ([`crate::core_lines_sample`]). Each check only looks whether it has come
//! back, and the step looks again every [`POLL_MS`] while it runs; a reading
//! back is said with how long it took (`alerts: core lines read in <s> s`)
//! and decides at that check. One not back after [`READ_LIMIT_MS`] is given
//! up and decides nothing, and no other starts until its thread has ended,
//! so a slow reading never piles up threads; the pass never waits on it nor
//! on the performance figures' lock it may hold.

use serde_json::{Value, json};

use super::{Event, Monitor};
use crate::core_lines_sample::Reading as Pending;
use std::sync::mpsc::TryRecvError;

/// How long a reading may take before it is given up: as long as the
/// sampler's.
pub(super) const READ_LIMIT_MS: i64 = crate::core_lines_sample::READ_LIMIT_MS;

/// How soon the step looks again while a reading runs.
pub(super) const POLL_MS: i64 = 5_000;

/// The step's Core lines readings: the one in flight (when it started, by
/// the step's clock and the host's), and one given up whose thread may
/// still run.
#[derive(Default)]
pub(super) struct Reads {
    reading: Option<(Pending<Value>, i64, std::time::Instant)>,
    abandoned: Option<Pending<Value>>,
    /// When the step looks again, while a reading runs.
    poll_ms: Option<i64>,
}

impl Reads {
    /// Whether a reading is in flight.
    pub(super) fn in_flight(&self) -> bool {
        self.reading.is_some()
    }

    /// When the step should look again: while a reading runs, shortly.
    pub(super) fn poll_ms(&self) -> Option<i64> {
        self.reading.as_ref().and(self.poll_ms)
    }
}

/// The alert's kind.
pub(super) const KIND: &str = "guardrail_out_of_range";

/// One figure of a reading, as the step reads it.
#[derive(Debug, Clone, PartialEq)]
struct Reading {
    line: String,
    key: String,
    name: String,
    unit: String,
    value: Value,
    range: Value,
    state: String,
}

/// Every figure of an answer of `GET /api/core-lines`, north stars and
/// guardrails alike.
fn figures(answer: &Value) -> Vec<Reading> {
    answer["lines"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|line| {
            let name = line["line"].as_str().unwrap_or_default().to_string();
            std::iter::once(&line["north_star"])
                .chain(line["guardrails"].as_array().into_iter().flatten())
                .filter_map(move |figure| {
                    Some(Reading {
                        line: name.clone(),
                        key: figure["key"].as_str()?.to_string(),
                        name: figure["name"].as_str().unwrap_or_default().to_string(),
                        unit: figure["unit"].as_str().unwrap_or_default().to_string(),
                        value: figure["value"].clone(),
                        range: figure["range"].clone(),
                        state: figure["state"].as_str().unwrap_or_default().to_string(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// A value in its unit's words, as the card says it (web/src/coreLines.ts),
/// roughly: sizes, spans, percents and dollars.
fn words(unit: &str, value: &Value) -> String {
    let Some(number) = value.as_f64() else {
        return "no value".to_string();
    };
    match unit {
        "bytes" | "bytes_per_day" => {
            let size = if (0.0..1.8e19).contains(&number) {
                // A non-negative size within u64 converts as whole bytes.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let bytes = number as u64;
                super::size_text(bytes)
            } else {
                format!("{number} bytes")
            };
            if unit == "bytes_per_day" {
                format!("{size} a day")
            } else {
                size
            }
        }
        "days" => format!("{number:.0} days"),
        "ratio" => format!("{number:.1}×"),
        "ms" => {
            let minutes = (number / 60_000.0).round();
            if minutes < 60.0 {
                format!("{minutes:.0} min")
            } else {
                format!(
                    "{:.0} h {:.0} min",
                    (minutes / 60.0).floor(),
                    minutes % 60.0
                )
            }
        }
        "share" => format!("{:.1}%", number * 100.0),
        "usd" => format!("${number:.2}"),
        _ => format!("{number}"),
    }
}

/// A range in its unit's words: "<min> or more", "<max> or less", "between
/// <min> and <max>".
fn range_words(unit: &str, range: &Value) -> String {
    match (range.get("min"), range.get("max")) {
        (Some(min), Some(max)) => format!("between {} and {}", words(unit, min), words(unit, max)),
        (Some(min), None) => format!("{} or more", words(unit, min)),
        (None, Some(max)) => format!("{} or less", words(unit, max)),
        (None, None) => "no range set".to_string(),
    }
}

/// The figure an open `guardrail_out_of_range` alert is about: its
/// evidence's `key`, else its subject's.
fn figure_of(subject: &str, evidence: &Value) -> String {
    evidence["key"].as_str().map_or_else(
        || {
            subject
                .strip_prefix("guardrail ")
                .unwrap_or(subject)
                .to_string()
        },
        str::to_string,
    )
}

impl Monitor {
    /// The figures are due at `now`: start a reading on a thread of its
    /// own ([`crate::serve::core_lines::reading`]).
    pub(super) fn start_guardrails(&mut self, now: i64) {
        self.start_guardrails_with(now, || Pending::spawn(crate::serve::core_lines::reading));
    }

    /// Start a reading through `start`, unless one is in flight or one
    /// given up still runs (then this turn has none).
    pub(super) fn start_guardrails_with(
        &mut self,
        now: i64,
        start: impl FnOnce() -> Pending<Value>,
    ) {
        let reads = &mut self.guardrail_reads;
        if reads.abandoned.as_ref().is_some_and(|old| !old.running()) {
            reads.abandoned = None;
        }
        if reads.reading.is_some() || reads.abandoned.is_some() {
            return;
        }
        reads.reading = Some((start(), now, std::time::Instant::now()));
        reads.poll_ms = Some(now.saturating_add(POLL_MS));
    }

    /// Look at the reading in flight, never waiting: one back decides on
    /// the alerts at `now`, said with how long it took; one over the limit
    /// is given up.
    pub(super) fn poll_guardrails(
        &mut self,
        now: i64,
        out: &mut Vec<Event>,
        lines: &mut Vec<String>,
    ) {
        let Some((pending, started, begun)) = self.guardrail_reads.reading.take() else {
            return;
        };
        let reading = match pending.try_answer() {
            Err(TryRecvError::Empty) => {
                if now.saturating_sub(started) < READ_LIMIT_MS {
                    self.guardrail_reads.reading = Some((pending, started, begun));
                    self.guardrail_reads.poll_ms = Some(now.saturating_add(POLL_MS));
                } else {
                    self.guardrail_reads.abandoned = Some(pending);
                    self.guardrail_reads.poll_ms = None;
                    lines.push(format!(
                        "alerts: the core lines' reading took over {} s: given up, it decides nothing",
                        READ_LIMIT_MS / 1000
                    ));
                }
                return;
            }
            Err(TryRecvError::Disconnected) => {
                Err("the reading ended without an answer".to_string())
            }
            Ok(reading) => reading,
        };
        self.guardrail_reads.poll_ms = None;
        lines.push(format!(
            "alerts: core lines read in {:.1} s",
            begun.elapsed().as_secs_f64()
        ));
        self.guardrail_events(now, reading, out, lines);
    }

    /// `guardrail_out_of_range`, from one reading (the answer of `GET
    /// /api/core-lines`, or why it could not be read).
    pub(super) fn guardrail_events(
        &mut self,
        now: i64,
        reading: Result<Value, String>,
        out: &mut Vec<Event>,
        lines: &mut Vec<String>,
    ) {
        let answer = match reading {
            Ok(answer) => answer,
            Err(error) => {
                lines.push(format!(
                    "alerts: the core lines' figures: {}",
                    super::scrub(&error)
                ));
                return;
            }
        };
        let figures = figures(&answer);
        for figure in &figures {
            let subject = format!("guardrail {}", figure.key);
            let open = self.open_keys(KIND, &subject);
            if figure.state == "out" {
                self.guardrail_back.remove(&figure.key);
                let since = *self.guardrail_out.entry(figure.key.clone()).or_insert(now);
                if open.is_empty() && now.saturating_sub(since) >= self.settings.guardrail_raise_ms
                {
                    let line = figure.line.clone();
                    out.push(Event::Fire {
                        key: format!("{KIND}:{}:{since}", figure.key),
                        kind: KIND,
                        text: format!(
                            "guardrail out of range: {line}'s {} is {}, out of its range ({}) \
                             since {}",
                            figure.name,
                            words(&figure.unit, &figure.value),
                            range_words(&figure.unit, &figure.range),
                            crate::scorecard::time::format_rfc3339(since)
                        ),
                        subject,
                        evidence: json!({
                            "line": line, "key": figure.key, "name": figure.name,
                            "value": figure.value, "range": figure.range,
                            "out_since_ms": since, "reading_ms": now,
                        }),
                        notice: false,
                    });
                }
            } else {
                self.guardrail_out.remove(&figure.key);
                if open.is_empty() {
                    self.guardrail_back.remove(&figure.key);
                    continue;
                }
                let since = *self.guardrail_back.entry(figure.key.clone()).or_insert(now);
                if now.saturating_sub(since) >= self.settings.guardrail_clear_ms {
                    self.guardrail_back.remove(&figure.key);
                    for key in open {
                        out.push(Event::Clear {
                            key,
                            evidence: json!({
                                "key": figure.key, "state": figure.state,
                                "value": figure.value, "range": figure.range,
                                "reading_ms": now,
                            }),
                        });
                    }
                }
            }
        }
        // An open alert whose figure the answer no longer has.
        let present: std::collections::BTreeSet<&str> =
            figures.iter().map(|figure| figure.key.as_str()).collect();
        let gone: Vec<(String, String)> = self
            .known
            .iter()
            .filter(|(_, row)| row.open() && row.kind == KIND)
            .map(|(key, row)| (key.clone(), figure_of(&row.subject, &row.evidence)))
            .filter(|(_, figure)| !present.contains(figure.as_str()))
            .collect();
        for (key, figure) in gone {
            self.guardrail_out.remove(&figure);
            self.guardrail_back.remove(&figure);
            out.push(Event::Clear {
                key,
                evidence: json!({"key": figure, "state": "gone", "reading_ms": now}),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Event, Monitor, Row, Settings};
    use super::{KIND, POLL_MS, Pending, READ_LIMIT_MS, range_words, words};
    use serde_json::{Value, json};

    const RAISE: i64 = 7_200_000;
    const CLEAR: i64 = 3_600_000;

    fn monitor() -> Monitor {
        let (settings, _) = Settings::from_env();
        Monitor::with(Settings {
            guardrail_raise_ms: RAISE,
            guardrail_clear_ms: CLEAR,
            ..settings
        })
    }

    /// An answer with free disk at `value` in state `state` (its range min
    /// 40 GiB), and the database ratio in range.
    fn answer(state: &str, value: &Value) -> Value {
        json!({"lines": [
            {"line": "performance", "north_star": {"key": "performance.report_to_live",
                "name": "Time from report to live", "unit": "ms", "value": null,
                "range": null, "state": "not_collected"}, "guardrails": []},
            {"line": "storage",
             "north_star": {"key": "storage.days_left", "name": "Days until the disk is full",
                "unit": "days", "value": 30, "range": null, "state": "no_range"},
             "guardrails": [
                {"key": "storage.free_bytes", "name": "Free disk", "unit": "bytes",
                 "value": value, "range": {"min": 42_949_672_960_i64}, "state": state},
                {"key": "storage.database_ratio", "name": "Database against its log",
                 "unit": "ratio", "value": 2.5, "range": {"max": 10}, "state": "in"},
             ]},
        ]})
    }

    /// One reading at `at`: the events, and the lines.
    fn read(
        monitor: &mut Monitor,
        at: i64,
        reading: Result<Value, String>,
    ) -> (Vec<Event>, Vec<String>) {
        let (mut out, mut lines) = (Vec::new(), Vec::new());
        monitor.guardrail_events(at, reading, &mut out, &mut lines);
        (out, lines)
    }

    /// The store's side: a fire opens its key, a clear closes it.
    fn commit(monitor: &mut Monitor, events: &[Event]) {
        for event in events {
            match event {
                Event::Fire {
                    key,
                    kind,
                    subject,
                    evidence,
                    ..
                } => {
                    monitor.known.entry(key.clone()).or_insert(Row {
                        kind: (*kind).to_string(),
                        subject: subject.clone(),
                        state: "open".to_string(),
                        evidence: evidence.clone(),
                        fired_event_id: 1,
                        fired_at_ms: 0,
                    });
                }
                Event::Clear { key, evidence } => {
                    if monitor.clears(key, evidence, i64::MAX)
                        && let Some(row) = monitor.known.get_mut(key)
                    {
                        row.state = "cleared".to_string();
                    }
                }
            }
        }
    }

    fn keys(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                Event::Fire { key, .. } => format!("fire {key}"),
                Event::Clear { key, .. } => format!("clear {key}"),
            })
            .collect()
    }

    const GIB: i64 = 1_073_741_824;

    #[test]
    fn a_figure_held_out_raises_once_after_the_hold_and_a_reading_in_restarts_it() {
        let mut monitor = monitor();
        let low = || Ok(answer("out", &json!(2 * GIB)));
        assert!(
            read(&mut monitor, 1000, low()).0.is_empty(),
            "out, the hold just began"
        );
        assert!(
            read(&mut monitor, 1000 + RAISE - 1, low()).0.is_empty(),
            "not held long enough"
        );
        // A reading in range restarts the count.
        assert!(
            read(
                &mut monitor,
                1000 + RAISE - 1,
                Ok(answer("in", &json!(50 * GIB)))
            )
            .0
            .is_empty()
        );
        assert!(
            read(&mut monitor, 1000 + RAISE, low()).0.is_empty(),
            "the hold starts again"
        );
        let start = 1000 + RAISE;
        let (fired, _) = read(&mut monitor, start + RAISE, low());
        assert_eq!(
            keys(&fired),
            [format!("fire {KIND}:storage.free_bytes:{start}")]
        );
        let Event::Fire {
            kind,
            subject,
            evidence,
            text,
            ..
        } = &fired[0]
        else {
            panic!("a fire");
        };
        assert_eq!(*kind, KIND);
        assert_eq!(subject, "guardrail storage.free_bytes");
        assert_eq!(
            evidence,
            &json!({"line": "storage", "key": "storage.free_bytes", "name": "Free disk",
                    "value": 2 * GIB, "range": {"min": 42_949_672_960_i64},
                    "out_since_ms": start, "reading_ms": start + RAISE})
        );
        assert!(
            text.contains("storage") && text.contains("Free disk"),
            "{text}"
        );
        assert!(
            text.contains("2.0 GB") && text.contains("40.0 GB or more"),
            "{text}"
        );
        commit(&mut monitor, &fired);
        // One open per figure: still out, nothing more.
        assert!(read(&mut monitor, start + 3 * RAISE, low()).0.is_empty());
    }

    #[test]
    fn an_open_alert_clears_after_the_clear_hold_and_a_reading_out_restarts_it() {
        let mut monitor = monitor();
        let low = || Ok(answer("out", &json!(2 * GIB)));
        let fine = || Ok(answer("in", &json!(50 * GIB)));
        read(&mut monitor, 0, low());
        let (fired, _) = read(&mut monitor, RAISE, low());
        commit(&mut monitor, &fired);
        let back = RAISE + 1000;
        assert!(
            read(&mut monitor, back, fine()).0.is_empty(),
            "back in, the hold just began"
        );
        assert!(read(&mut monitor, back + CLEAR - 1, fine()).0.is_empty());
        // Out again: the clear hold restarts.
        assert!(read(&mut monitor, back + CLEAR - 1, low()).0.is_empty());
        let again = back + CLEAR;
        assert!(read(&mut monitor, again, fine()).0.is_empty());
        let (cleared, _) = read(&mut monitor, again + CLEAR, fine());
        assert_eq!(
            keys(&cleared),
            [format!("clear {KIND}:storage.free_bytes:0")]
        );
        let Event::Clear { evidence, .. } = &cleared[0] else {
            panic!("a clear")
        };
        assert_eq!(
            evidence,
            &json!({"key": "storage.free_bytes", "state": "in", "value": 50 * GIB,
                    "range": {"min": 42_949_672_960_i64}, "reading_ms": again + CLEAR})
        );
        commit(&mut monitor, &cleared);
        assert!(read(&mut monitor, again + 2 * CLEAR, fine()).0.is_empty());
    }

    #[test]
    fn no_range_or_no_value_raises_nothing_and_clears_an_open_one_after_the_hold() {
        let mut monitor = monitor();
        for at in [0, RAISE, 3 * RAISE] {
            assert!(
                read(&mut monitor, at, Ok(answer("no_range", &json!(1))))
                    .0
                    .is_empty()
            );
            assert!(
                read(&mut monitor, at, Ok(answer("not_collected", &Value::Null)))
                    .0
                    .is_empty()
            );
        }
        // A figure that loses its value counts as not out.
        read(&mut monitor, 10 * RAISE, Ok(answer("out", &json!(1))));
        let (fired, _) = read(&mut monitor, 11 * RAISE, Ok(answer("out", &json!(1))));
        commit(&mut monitor, &fired);
        let at = 11 * RAISE + 1;
        read(&mut monitor, at, Ok(answer("not_collected", &Value::Null)));
        let (cleared, _) = read(&mut monitor, at + CLEAR, Ok(answer("no_range", &json!(1))));
        assert_eq!(cleared.len(), 1, "{cleared:?}");
    }

    #[test]
    fn a_failed_reading_decides_nothing_and_is_a_line() {
        let mut monitor = monitor();
        read(&mut monitor, 0, Ok(answer("out", &json!(1))));
        let (events, lines) = read(&mut monitor, RAISE, Err("connection refused".to_string()));
        assert!(events.is_empty());
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("connection refused"), "{lines:?}");
        // The count went on: the next reading out raises.
        let (fired, _) = read(&mut monitor, RAISE + 1, Ok(answer("out", &json!(1))));
        assert_eq!(fired.len(), 1);
    }

    #[test]
    fn an_alert_open_in_the_record_is_adopted_after_a_restart() {
        let mut restarted = monitor();
        let key = format!("{KIND}:storage.free_bytes:5");
        restarted.known.insert(
            key.clone(),
            Row {
                kind: KIND.to_string(),
                subject: "guardrail storage.free_bytes".to_string(),
                state: "open".to_string(),
                evidence: json!({"key": "storage.free_bytes", "out_since_ms": 5, "reading_ms": 10}),
                fired_event_id: 7,
                fired_at_ms: 10,
            },
        );
        // Still out, well past the hold: no second alert.
        for at in [100, 100 + RAISE, 100 + 3 * RAISE] {
            assert!(
                read(&mut restarted, at, Ok(answer("out", &json!(1))))
                    .0
                    .is_empty()
            );
        }
        // Back in: it clears after the hold, counted from this run's reading.
        let back = 100 + 4 * RAISE;
        assert!(
            read(&mut restarted, back, Ok(answer("in", &json!(50 * GIB))))
                .0
                .is_empty()
        );
        let (cleared, _) = read(
            &mut restarted,
            back + CLEAR,
            Ok(answer("in", &json!(50 * GIB))),
        );
        assert_eq!(keys(&cleared), [format!("clear {key}")]);
    }

    #[test]
    fn an_open_alert_whose_figure_is_gone_clears_at_the_next_reading() {
        let mut monitor = monitor();
        let key = format!("{KIND}:storage.free_disk:5");
        monitor.known.insert(
            key.clone(),
            Row {
                kind: KIND.to_string(),
                subject: "guardrail storage.free_disk".to_string(),
                state: "open".to_string(),
                evidence: json!({"key": "storage.free_disk", "out_since_ms": 5, "reading_ms": 10}),
                fired_event_id: 7,
                fired_at_ms: 10,
            },
        );
        let (cleared, _) = read(&mut monitor, 20, Ok(answer("in", &json!(50 * GIB))));
        assert_eq!(keys(&cleared), [format!("clear {key}")]);
        let Event::Clear { evidence, .. } = &cleared[0] else {
            panic!("a clear")
        };
        assert_eq!(
            evidence,
            &json!({"key": "storage.free_disk", "state": "gone", "reading_ms": 20})
        );
        // A failed reading clears nothing.
        let mut other = monitor;
        let (none, _) = read(&mut other, 30, Err("no".to_string()));
        assert!(none.is_empty());
    }

    #[test]
    fn values_and_ranges_read_in_their_units_words() {
        assert_eq!(words("bytes", &json!(2 * GIB)), "2.0 GB");
        assert_eq!(words("bytes_per_day", &json!(1_048_576)), "1.0 MB a day");
        assert_eq!(words("ms", &json!(5_520_000)), "1 h 32 min");
        assert_eq!(words("ms", &json!(840_000)), "14 min");
        assert_eq!(words("share", &json!(0.059)), "5.9%");
        assert_eq!(words("usd", &json!(8.08)), "$8.08");
        assert_eq!(words("ratio", &json!(12.34)), "12.3×");
        assert_eq!(words("count", &json!(1214)), "1214");
        assert_eq!(words("days", &json!(30)), "30 days");
        assert_eq!(range_words("share", &json!({"max": 0.05})), "5.0% or less");
        assert_eq!(
            range_words("count", &json!({"min": 1, "max": 2})),
            "between 1 and 2"
        );
        assert_eq!(range_words("count", &json!({})), "no range set");
    }

    /// cc-astra A18: the reading runs beside the checks. A check never waits
    /// for it (nor for the lock it may hold); the step looks again every
    /// POLL_MS while it runs; one over the limit is given up, and no other
    /// starts beside its thread; a reading back decides, said with its time.
    #[test]
    fn a_slow_core_lines_reading_never_holds_a_check() {
        let mut monitor = monitor();
        let (release, hold) = std::sync::mpsc::channel::<()>();
        let mut started = 0;
        let poll = |monitor: &mut Monitor, at: i64| {
            let begun = std::time::Instant::now();
            let (mut out, mut lines) = (Vec::new(), Vec::new());
            monitor.poll_guardrails(at, &mut out, &mut lines);
            assert!(
                begun.elapsed() < std::time::Duration::from_secs(2),
                "the check waited"
            );
            (out, lines)
        };
        monitor.start_guardrails_with(0, || {
            started += 1;
            Pending::spawn(move || {
                let _ = hold.recv();
                Ok(answer("out", &json!(1)))
            })
        });
        assert!(monitor.guardrail_reads.in_flight());
        assert_eq!(monitor.guardrail_reads.poll_ms(), Some(POLL_MS));
        assert!(monitor.due_ms() <= POLL_MS, "the step looks again shortly");
        let (out, lines) = poll(&mut monitor, 1000);
        assert!(
            out.is_empty() && lines.is_empty(),
            "still reading: {lines:?}"
        );
        assert_eq!(monitor.guardrail_reads.poll_ms(), Some(1000 + POLL_MS));
        // Due again while it runs: no second reading.
        monitor.start_guardrails_with(2000, || {
            started += 1;
            Pending::done(Ok(answer("out", &json!(1))))
        });
        assert_eq!(started, 1);
        // Over the limit: given up, it decides nothing.
        let (out, lines) = poll(&mut monitor, READ_LIMIT_MS);
        assert!(out.is_empty());
        assert!(
            lines.len() == 1 && lines[0].contains("took over 300 s: given up"),
            "{lines:?}"
        );
        assert!(!monitor.guardrail_reads.in_flight());
        assert_eq!(monitor.guardrail_reads.poll_ms(), None);
        // Due again, but the given-up thread still runs: none starts.
        monitor.start_guardrails_with(READ_LIMIT_MS + 1, || {
            started += 1;
            Pending::done(Ok(answer("out", &json!(1))))
        });
        assert_eq!(started, 1);
        assert!(!monitor.guardrail_reads.in_flight());
        release.send(()).expect("release the reading");
        let mut ended = false;
        for _ in 0..200 {
            if !monitor
                .guardrail_reads
                .abandoned
                .as_ref()
                .is_some_and(Pending::running)
            {
                ended = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ended, "the given-up reading's thread ended");
        // Once it has ended, readings start again, and one back decides:
        // out at two readings RAISE apart fires once.
        let t0 = 2 * READ_LIMIT_MS;
        for at in [t0, t0 + RAISE] {
            monitor.start_guardrails_with(at, || {
                started += 1;
                Pending::done(Ok(answer("out", &json!(1))))
            });
            let (out, lines) = poll(&mut monitor, at);
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with("alerts: core lines read in ")),
                "{lines:?}"
            );
            if at == t0 {
                assert!(out.is_empty(), "{:?}", keys(&out));
            } else {
                assert_eq!(keys(&out), [format!("fire {KIND}:storage.free_bytes:{t0}")]);
            }
        }
        assert_eq!(started, 3);
        // A reading that fails is a line and decides nothing.
        monitor.start_guardrails_with(t0 + 2 * RAISE, || {
            Pending::done(Err("no store".to_string()))
        });
        let (out, lines) = poll(&mut monitor, t0 + 2 * RAISE);
        assert!(out.is_empty());
        assert!(
            lines
                .iter()
                .any(|line| line.contains("the core lines' figures: no store")),
            "{lines:?}"
        );
    }
}
