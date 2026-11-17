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
//! The counts live in the step's memory, and a restarted loop adopts an
//! alert the record already holds open (one open per figure, across
//! restarts). A reading that fails is a line in the loop's log and decides
//! nothing.
//!
//! A restart does not restart the raise hold (objective 192): every deploy
//! restarts the loop, mostly within the hold, so a count kept in memory alone
//! never completed. The loop's first reading carries the hourly
//! `host.core_lines` samples of the hold plus two sample periods
//! ([`crate::serve::core_lines::reading_with_samples`]), and at it each
//! figure out of range, not stale and not counted yet takes its out-since
//! from them ([`out_since`]): the oldest sample of the newest unbroken run
//! out of its current range that reaches the reading. A sample in range, one
//! that does not carry the figure as a number, or a gap of more than two
//! periods (from the reading to the newest sample, or between two) ends the
//! run; without one the count starts at that reading, as before. This is
//! done once per loop, whatever it finds; a samples error is a line in the
//! loop's log, and the holds start then. It is an estimate from samples, not
//! proof of time out of range: the seeded out-since is never before a
//! sample that saw the figure out, but a dip back in range between two
//! hourly samples is not seen. The alert fires once the seeded time is a
//! hold old: never later than before, with the same key, text and evidence
//! (`out_since_ms` the seeded time).
//!
//! A judged stretch keeps its key (objective 192 (5)): a figure whose newest
//! alert the owner judged while open (its cleared evidence `{judged}`, from
//! the alert rows the loop starts with) keeps that alert's out-since, so it
//! is not raised again, the seeding window having slid past where the
//! stretch began. A loop that keeps running does the same (objective 209):
//! an `alert.judged` record (a verdict other than `right`) of an open alert
//! it knows keeps that alert's out-since, unless this loop already read the
//! figure in range after it (the alert was open in its clear hold, its
//! stretch ended). Every fresh reading that finds the figure out takes the
//! judged out-since, so its key is known and no reading out raises it again,
//! however long the figure stays out. Only positive evidence ends it: a
//! fresh reading with the figure in range (`in`; no range, no value and a
//! stale figure are none, before or after a reading out), or a sample in
//! range after that out-since among a restarted loop's first reading's
//! samples (none for a figure with no range now). A samples error keeps it.
//! Ended, the next stretch out is new, from that reading or seeded from the
//! samples at the first reading as above.
//!
//! A reading is the whole answer (the 7-day Scorecard with its transcript
//! walk, the cost window, the storage report...), and its one-minute cache
//! has always expired at the step's 300 s, so it never runs inside a pass
//! (cc-astra A18): it runs on a thread of its own, on the dashboard's
//! read-only connection and its statement timeout, as 090's sampler does
//! ([`crate::core_lines_sample`]). Each check only looks whether it has come
//! back, and the step looks again every [`POLL_MS`] while it runs; a reading
//! back is said with two times (cc-astra A24): how long its worker took to
//! compute it, and how long after its start the step saw it, which adds
//! the wait for the step's next look (`alerts: core lines read:
//! compute_ms=<n> observed_ms=<n>`), and decides at that check. One not back after [`READ_LIMIT_MS`] is given
//! up and decides nothing, and no other starts until its thread has ended,
//! so a slow reading never piles up threads; the pass never waits on it nor
//! on the performance figures' lock it may hold.
//!
//! A figure of a kept group past its stale line (`stale`, crate
//! serve/core_lines/kept.rs) neither raises nor counts toward clearing: its
//! value is not current. The group itself is seen instead
//! (sync/design-core-lines-kept-figures.md): `core_lines_stale`, one per
//! group of the answer's `groups`, subject `core lines <group>`, key
//! `core_lines_stale:<group>:<stale since ms>`, evidence `{group, as_of_ms,
//! age_ms, every_ms, stale_ms, last_error, reading_ms}`, raised at the first
//! reading that finds the group stale (no hold: stale is already its stale
//! line of failures), once per occurrence. It clears at the first reading
//! that finds the group not stale (`{group, state: "fresh", as_of_ms,
//! reading_ms}`), or no longer in the answer (`state: "gone"`). A restarted
//! loop adopts an open one from the record, by its subject, as the range
//! alerts are.

use serde_json::{Value, json};

use super::{Event, Monitor};
use crate::core_lines_sample::Reading as Pending;
use crate::serve::core_lines::Parts;
use std::sync::mpsc::TryRecvError;

/// How long a reading may take before it is given up: as long as the
/// sampler's.
pub(super) const READ_LIMIT_MS: i64 = crate::core_lines_sample::READ_LIMIT_MS;

/// A reading's answer with how long each of its parts took.
type Timed = (Value, Parts);

/// How soon the step looks again while a reading runs.
pub(super) const POLL_MS: i64 = 5_000;

/// The step's Core lines readings: the one in flight (when it started, by
/// the step's clock and the host's), and one given up whose thread may
/// still run.
#[derive(Default)]
pub(super) struct Reads {
    reading: Option<(Pending<Timed>, i64, std::time::Instant)>,
    abandoned: Option<Pending<Timed>>,
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

/// A kept group past its stale line.
pub(super) const STALE_KIND: &str = "core_lines_stale";

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
    /// Its kept group is past its stale line.
    stale: bool,
}

/// Every figure of an answer of `GET /api/core-lines` the step watches
/// (objective 176): the lines' north stars and guardrails, the figures
/// demoted to an alert (`alert_figures`), and the diagnostics that carry a
/// line (leaked test schemas, until objective 182), each under its line.
/// Any other diagnostic raises nothing.
fn figures(answer: &Value) -> Vec<Reading> {
    let reading = |line: &str, figure: &Value| {
        Some(Reading {
            line: line.to_string(),
            key: figure["key"].as_str()?.to_string(),
            name: figure["name"].as_str().unwrap_or_default().to_string(),
            unit: figure["unit"].as_str().unwrap_or_default().to_string(),
            value: figure["value"].clone(),
            range: figure["range"].clone(),
            state: figure["state"].as_str().unwrap_or_default().to_string(),
            stale: figure["stale"] == true,
        })
    };
    let mut figures: Vec<Reading> = answer["lines"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|line| {
            let name = line["line"].as_str().unwrap_or_default();
            std::iter::once(&line["north_star"])
                .chain(line["guardrails"].as_array().into_iter().flatten())
                .filter_map(|figure| reading(name, figure))
                .collect::<Vec<_>>()
        })
        .collect();
    for list in ["alert_figures", "diagnostics"] {
        for figure in answer[list].as_array().into_iter().flatten() {
            if let Some(line) = figure["line"].as_str() {
                figures.extend(reading(line, figure));
            }
        }
    }
    figures
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

/// The line that says a reading came back (cc-astra A24): `computed`, the
/// worker's own time around the calculation (none when it ended without
/// one), `observed`, from its start until the step saw it, and how long
/// each part of it took (all none for a reading that failed).
fn read_line(
    computed: Option<std::time::Duration>,
    observed: std::time::Duration,
    parts: &Parts,
) -> String {
    let compute_ms =
        computed.map_or_else(|| "none".to_string(), |took| took.as_millis().to_string());
    format!(
        "alerts: core lines read: compute_ms={compute_ms} observed_ms={} {}",
        observed.as_millis(),
        parts.text()
    )
}

/// The figure's bounds in `range` (`{min?, max?}`), `None` without one.
fn bounds(range: &Value) -> Option<(Option<f64>, Option<f64>)> {
    let bounds = (
        range.get("min").and_then(Value::as_f64),
        range.get("max").and_then(Value::as_f64),
    );
    (bounds != (None, None)).then_some(bounds)
}

/// Whether `value` lies within `bounds`, bounds included.
fn within((min, max): (Option<f64>, Option<f64>), value: f64) -> bool {
    min.is_none_or(|min| value >= min) && max.is_none_or(|max| value <= max)
}

/// The samples' `(at_ms, the figure's number)`, oldest first, up to `now`
/// (`[]` when `samples` is no list).
fn sampled(samples: &Value, key: &str, now: i64) -> Vec<(i64, Option<f64>)> {
    let mut list: Vec<(i64, Option<f64>)> = samples
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|sample| {
            let at = sample["at_ms"].as_i64()?;
            (at <= now).then(|| (at, sample["values"][key].as_f64()))
        })
        .collect();
    list.sort_by_key(|(at, _)| *at);
    list
}

/// When figure `key` has been out of `range` (its current range) by the
/// `host.core_lines` `samples` (`[{at_ms, values}]`) at `now`: the time of
/// the oldest sample of the newest unbroken run of samples out of it, the
/// run reaching `now`. A sample in range, a sample not carrying the figure
/// as a number, or a gap of more than two `period_ms` (from `now` to the
/// newest sample, or between two samples) ends the run. `None` without such
/// a run, or without a range.
pub(super) fn out_since(
    samples: &Value,
    key: &str,
    range: &Value,
    now: i64,
    period_ms: i64,
) -> Option<i64> {
    let bounds = bounds(range)?;
    let gap = period_ms.saturating_mul(2);
    let mut reached = now;
    let mut since = None;
    for (at, value) in sampled(samples, key, now).into_iter().rev() {
        if reached.saturating_sub(at) > gap {
            break;
        }
        match value {
            Some(value) if !within(bounds, value) => {
                since = Some(at);
                reached = at;
            }
            _ => break,
        }
    }
    since
}

/// Whether a sample after `after` has figure `key` back within `range` (its
/// current range): positive evidence a judged stretch ended. Never for a
/// figure with no range.
fn back_after(samples: &Value, key: &str, range: &Value, after: i64) -> bool {
    let Some(bounds) = bounds(range) else {
        return false;
    };
    sampled(samples, key, i64::MAX)
        .into_iter()
        .any(|(at, value)| at > after && value.is_some_and(|value| within(bounds, value)))
}

impl Monitor {
    /// The figures are due at `now`: start a reading on a thread of its
    /// own ([`crate::serve::core_lines::reading`]); the loop's first one
    /// carries the samples since the hold plus two sample periods ago
    /// ([`crate::serve::core_lines::reading_with_samples`], objective 192).
    pub(super) fn start_guardrails(&mut self, now: i64) {
        if self.guardrail_seeded {
            self.start_guardrails_with(now, || Pending::spawn(crate::serve::core_lines::reading));
        } else {
            let (period, _) = crate::core_lines_sample::every_ms_from_env();
            let since = now
                .saturating_sub(self.settings.guardrail_raise_ms)
                .saturating_sub(period.saturating_mul(2));
            self.start_guardrails_with(now, || {
                Pending::spawn(move || crate::serve::core_lines::reading_with_samples(since))
            });
        }
    }

    /// The judged stretches in the alert `rows` the loop starts with (the
    /// store's, oldest first): each figure whose newest
    /// `guardrail_out_of_range` alert was cleared by a judgement (its
    /// cleared evidence `{judged}`), with that alert's out-since.
    pub(super) fn seed_judged(&mut self, rows: &Value) {
        let mut newest: std::collections::BTreeMap<String, (i64, Option<i64>)> =
            std::collections::BTreeMap::new();
        for row in rows.as_array().into_iter().flatten() {
            if row["kind"] != KIND {
                continue;
            }
            let figure = figure_of(row["subject"].as_str().unwrap_or(""), &row["evidence"]);
            let fired = row["fired_at_ms"].as_i64().unwrap_or(0);
            let judged = row["cleared_evidence"]["judged"]
                .is_string()
                .then(|| row["evidence"]["out_since_ms"].as_i64())
                .flatten();
            if newest.get(&figure).is_none_or(|(at, _)| fired >= *at) {
                newest.insert(figure, (fired, judged));
            }
        }
        for (figure, (_, judged)) in newest {
            if let Some(since) = judged {
                self.guardrail_judged.insert(figure, since);
            }
        }
    }

    /// An open `guardrail_out_of_range` alert (`subject`, `evidence`) the
    /// owner judged while this loop runs (objective 209 (1)): its figure
    /// keeps the alert's out-since as a judged stretch, as a restarted loop
    /// seeds it from the alert rows ([`Monitor::seed_judged`]). Unless this
    /// loop read the figure in range after that out-since: the stretch had
    /// ended (the alert was still open in its clear hold), and nothing is
    /// kept.
    pub(super) fn judged_stretch(&mut self, subject: &str, evidence: &Value) {
        let figure = figure_of(subject, evidence);
        let Some(since) = evidence["out_since_ms"].as_i64() else {
            return;
        };
        if self
            .guardrail_in
            .get(&figure)
            .is_some_and(|back| *back > since)
        {
            return;
        }
        self.guardrail_judged.insert(figure, since);
    }

    /// Start a reading through `start`, unless one is in flight or one
    /// given up still runs (then this turn has none).
    pub(super) fn start_guardrails_with(
        &mut self,
        now: i64,
        start: impl FnOnce() -> Pending<Timed>,
    ) {
        let reads = &mut self.guardrail_reads;
        if reads.abandoned.as_ref().is_some_and(|old| !old.running()) {
            reads.abandoned = None;
        }
        if reads.reading.is_some() || reads.abandoned.is_some() {
            return;
        }
        // Its clock starts before its thread does, so the time the step sees
        // holds the whole computation.
        let begun = std::time::Instant::now();
        reads.reading = Some((start(), now, begun));
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
        let parts = reading
            .as_ref()
            .map(|(_, parts)| *parts)
            .unwrap_or_default();
        lines.push(read_line(pending.computed(), begun.elapsed(), &parts));
        self.guardrail_events(now, reading.map(|(answer, _)| answer), out, lines);
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
        let seeding = self.seeding(&answer, &figures, lines);
        for figure in &figures {
            if figure.stale {
                // Not current: it neither raises nor clears; the stale
                // group's own alert says so.
                continue;
            }
            let subject = format!("guardrail {}", figure.key);
            let open = self.open_keys(KIND, &subject);
            if figure.state == "out" {
                self.guardrail_back.remove(&figure.key);
                let since = self.out_since_of(figure, now, seeding.as_ref());
                // The stretch's alert, once judged by the owner (objective
                // 092), is not open and is not raised again: the next stretch
                // out of range is a new occurrence.
                let key = format!("{KIND}:{}:{since}", figure.key);
                if open.is_empty()
                    && !self.known.contains_key(&key)
                    && now.saturating_sub(since) >= self.settings.guardrail_raise_ms
                {
                    let line = figure.line.clone();
                    out.push(Event::Fire {
                        key,
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
                if figure.state == "in" {
                    // Positive evidence the judged stretch ended; no range,
                    // no value end nothing (objective 209 (2)).
                    self.guardrail_judged.remove(&figure.key);
                    self.guardrail_in.insert(figure.key.clone(), now);
                }
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
        self.stale_events(now, &answer, out);
    }

    /// Since when `figure`, out at `now`, has been out: a judged stretch's
    /// out-since (taken by every fresh reading out, kept until a reading in
    /// range ends it: objective 209 (2)), else its count, else, at the first
    /// reading, the samples' ([`out_since`]), else now.
    fn out_since_of(&mut self, figure: &Reading, now: i64, seeding: Option<&(Value, i64)>) -> i64 {
        if let Some(since) = self.guardrail_judged.get(&figure.key) {
            let since = *since;
            self.guardrail_out.insert(figure.key.clone(), since);
            return since;
        }
        if let Some(since) = self.guardrail_out.get(&figure.key) {
            return *since;
        }
        let since = seeding
            .and_then(|(samples, period)| {
                out_since(samples, &figure.key, &figure.range, now, *period)
            })
            .unwrap_or(now);
        self.guardrail_out.insert(figure.key.clone(), since);
        since
    }

    /// At the loop's first answered reading (once per loop): its samples
    /// and the sample period, `None` at any other reading or when the
    /// samples could not be read (said, and each hold starts now). A judged
    /// stretch with a sample in range after its out-since is ended here.
    fn seeding(
        &mut self,
        answer: &Value,
        figures: &[Reading],
        lines: &mut Vec<String>,
    ) -> Option<(Value, i64)> {
        if self.guardrail_seeded {
            return None;
        }
        self.guardrail_seeded = true;
        if let Some(error) = answer["recent_samples_error"].as_str() {
            lines.push(format!(
                "alerts: the guardrails' samples: {}; each hold starts at this reading",
                super::scrub(error)
            ));
            return None;
        }
        let samples = answer["recent_samples"].clone();
        let (period, _) = crate::core_lines_sample::every_ms_from_env();
        self.guardrail_judged.retain(|key, since| {
            !figures
                .iter()
                .find(|figure| &figure.key == key)
                .is_some_and(|figure| back_after(&samples, key, &figure.range, *since))
        });
        Some((samples, period))
    }

    /// `core_lines_stale`, from one reading's `groups`.
    fn stale_events(&self, now: i64, answer: &Value, out: &mut Vec<Event>) {
        let groups: Vec<&Value> = answer["groups"].as_array().into_iter().flatten().collect();
        for group in &groups {
            let Some(name) = group["group"].as_str() else {
                continue;
            };
            let subject = format!("core lines {name}");
            let open = self.open_keys(STALE_KIND, &subject);
            if group["stale"] == true {
                let since = group["stale_since_ms"].as_i64().unwrap_or(now);
                let key = format!("{STALE_KIND}:{name}:{since}");
                if !open.is_empty() || self.known.contains_key(&key) {
                    continue;
                }
                let as_of = group["as_of_ms"].as_i64();
                let error = group["last_error"].as_str().map(super::scrub);
                out.push(Event::Fire {
                    key,
                    kind: STALE_KIND,
                    text: format!(
                        "core lines: the {name} figures are stale: {}; the refresh {}",
                        as_of.map_or_else(
                            || "none has been read since the start".to_string(),
                            |at| format!(
                                "last read {} ({} min ago)",
                                crate::scorecard::time::format_rfc3339(at),
                                now.saturating_sub(at) / 60_000
                            )
                        ),
                        error.as_deref().map_or_else(
                            || "said no error".to_string(),
                            |error| format!("failed: {error}")
                        )
                    ),
                    subject,
                    evidence: json!({
                        "group": name, "as_of_ms": as_of, "age_ms": group["age_ms"],
                        "every_ms": group["every_ms"], "stale_ms": group["stale_ms"],
                        "last_error": error, "reading_ms": now,
                    }),
                    notice: false,
                });
            } else {
                for key in open {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({
                            "group": name, "state": "fresh",
                            "as_of_ms": group["as_of_ms"], "reading_ms": now,
                        }),
                    });
                }
            }
        }
        let present: std::collections::BTreeSet<String> = groups
            .iter()
            .filter_map(|group| group["group"].as_str())
            .map(|name| format!("core lines {name}"))
            .collect();
        for (key, row) in &self.known {
            if row.open() && row.kind == STALE_KIND && !present.contains(&row.subject) {
                out.push(Event::Clear {
                    key: key.clone(),
                    evidence: json!({
                        "group": row.subject.strip_prefix("core lines ").unwrap_or(&row.subject),
                        "state": "gone", "reading_ms": now,
                    }),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Event, Monitor, Row, Settings};
    use super::{
        KIND, POLL_MS, Parts, Pending, READ_LIMIT_MS, STALE_KIND, out_since, range_words,
        read_line, words,
    };
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

    /// Objective 176: an answer of the owner's set with free disk an alert
    /// figure under storage, leaked test schemas a diagnostic with its line
    /// and scratch nobody owns one without, all three out of range.
    fn adopted_answer() -> Value {
        json!({
            "lines": [{"line": "storage",
                "north_star": {"key": "storage.bytes_per_landing", "name": "Bytes a landing leaves behind",
                    "unit": "bytes", "value": null, "range": null, "state": "not_measured_yet"},
                "guardrails": []}],
            "alert_figures": [{"key": "storage.free_bytes", "name": "Free disk", "unit": "bytes",
                "value": 2 * GIB, "range": {"min": 42_949_672_960_i64}, "state": "out", "line": "storage"}],
            "diagnostics": [
                {"key": "storage.unowned_scratch_bytes", "name": "Scratch nobody owns", "unit": "bytes",
                 "value": 9 * GIB, "range": {"max": GIB}, "state": "out"},
                {"key": "storage.leaked_test_schemas", "name": "Leaked test schemas", "unit": "count",
                 "value": 4, "range": {"max": 0}, "state": "out", "line": "storage"},
            ],
        })
    }

    #[test]
    fn an_alert_figure_or_a_lined_diagnostic_raises_and_any_other_diagnostic_never() {
        let mut monitor = monitor();
        assert!(read(&mut monitor, 0, Ok(adopted_answer())).0.is_empty());
        let (fired, _) = read(&mut monitor, RAISE, Ok(adopted_answer()));
        let mut fired = keys(&fired);
        fired.sort();
        assert_eq!(
            fired,
            [
                format!("fire {KIND}:storage.free_bytes:0"),
                format!("fire {KIND}:storage.leaked_test_schemas:0"),
            ]
        );
        let (later, _) = read(&mut monitor, 10 * RAISE, Ok(adopted_answer()));
        assert!(
            !keys(&later)
                .iter()
                .any(|key| key.contains("unowned_scratch_bytes")),
            "a diagnostic without a line never raises"
        );
    }

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

    /// Objective 092: an open alert the owner judges while the loop runs is
    /// cleared in the loop's view by the next pass (the `alert.judged`
    /// record it reads), so the loop clears it no more, and the figure's
    /// next stretch out of range fires a new alert.
    #[test]
    fn an_alert_judged_while_the_loop_runs_is_cleared_in_its_view_and_a_new_stretch_fires() {
        let mut monitor = monitor();
        let low = || Ok(answer("out", &json!(2 * GIB)));
        let high = || Ok(answer("in", &json!(50 * GIB)));
        read(&mut monitor, 0, low());
        let (fired, _) = read(&mut monitor, RAISE, low());
        let first = format!("{KIND}:storage.free_bytes:0");
        assert_eq!(keys(&fired), [format!("fire {first}")]);
        commit(&mut monitor, &fired);
        // The owner judges it false: the next pass reads the record.
        let mut out = Vec::new();
        monitor.feed(
            50,
            RAISE + 1,
            &json!({"type": "alert.judged", "key": first, "verdict": "false"}),
            &mut out,
        );
        assert!(out.is_empty(), "a judgement fires and clears nothing");
        assert!(!monitor.known[&first].open(), "cleared in the loop's view");
        // Still out in the same stretch: the judged alert is not raised again.
        assert!(read(&mut monitor, 2 * RAISE, low()).0.is_empty());
        // Back in for longer than the clear hold: no clear is tried.
        let back = 2 * RAISE + 1;
        assert!(read(&mut monitor, back, high()).0.is_empty());
        assert!(
            read(&mut monitor, back + CLEAR, high()).0.is_empty(),
            "no clear of the judged alert"
        );
        // Out again, held: a new alert fires.
        let again = back + CLEAR + 1;
        read(&mut monitor, again, low());
        let (fired, _) = read(&mut monitor, again + RAISE, low());
        assert_eq!(
            keys(&fired),
            [format!("fire {KIND}:storage.free_bytes:{again}")]
        );
        // A judgement of a key the loop does not know, or of one not open,
        // changes nothing.
        commit(&mut monitor, &fired);
        let mut out = Vec::new();
        monitor.feed(
            60,
            again + RAISE,
            &json!({"type": "alert.judged", "key": "elsewhere:1", "verdict": "stale"}),
            &mut out,
        );
        assert!(out.is_empty());
        assert!(monitor.known[&format!("{KIND}:storage.free_bytes:{again}")].open());
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
                Ok((answer("out", &json!(1)), Parts::default()))
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
            Pending::done(Ok((answer("out", &json!(1)), Parts::default())))
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
            Pending::done(Ok((answer("out", &json!(1)), Parts::default())))
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
                Pending::done(Ok((answer("out", &json!(1)), Parts::default())))
            });
            let (out, lines) = poll(&mut monitor, at);
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with("alerts: core lines read: compute_ms=")),
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

    /// cc-astra A24: a reading says the worker's own time apart from the
    /// time the step saw it, which adds the wait for its next look. Lower
    /// bounds only (the sleeps), never an upper limit on a time.
    #[test]
    fn a_reading_says_its_compute_time_apart_from_when_it_was_seen() {
        let mut monitor = monitor();
        monitor.start_guardrails_with(0, || {
            Pending::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                Ok((answer("in", &json!(1)), Parts::default()))
            })
        });
        let mut done = false;
        for _ in 0..500 {
            if !monitor
                .guardrail_reads
                .reading
                .as_ref()
                .is_some_and(|(pending, _, _)| pending.running())
            {
                done = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(done, "the reading's thread ended");
        // The answer waits for the step's next look.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (mut out, mut lines) = (Vec::new(), Vec::new());
        monitor.poll_guardrails(POLL_MS, &mut out, &mut lines);
        let line = lines
            .iter()
            .find(|line| line.starts_with("alerts: core lines read: "))
            .unwrap_or_else(|| panic!("no read line: {lines:?}"));
        let number = |name: &str| -> u128 {
            line.split_whitespace()
                .find_map(|word| word.strip_prefix(&format!("{name}=")))
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| panic!("no {name} in {line}"))
        };
        let (compute, observed) = (number("compute_ms"), number("observed_ms"));
        assert!(compute >= 50, "the worker slept 50 ms: {line}");
        assert!(
            observed >= compute + 300,
            "seen at least 300 ms after it was computed: {line}"
        );
        assert_eq!(
            read_line(None, std::time::Duration::from_millis(7), &Parts::default()),
            "alerts: core lines read: compute_ms=none observed_ms=7 \
             open_ms=none trends_ms=none facts_ms=none archive_ms=none cost_ms=none storage_ms=none unowned_ms=none health_ms=none \
             lock_wait_ms=none performance=none"
        );
        let ms = |n| Some(std::time::Duration::from_millis(n));
        assert_eq!(
            read_line(
                ms(900),
                std::time::Duration::from_secs(4),
                &Parts {
                    open: ms(20),
                    trends: ms(300),
                    facts: ms(3),
                    archive: None,
                    cost: None,
                    storage: ms(500),
                    unowned: ms(7),
                    health: ms(40),
                    lock_wait: None,
                    performance: Some("kept"),
                    performance_age_ms: Some(30_000),
                    storage_age_ms: None,
                    aged: true,
                }
            ),
            "alerts: core lines read: compute_ms=900 observed_ms=4000 \
             open_ms=20 trends_ms=300 facts_ms=3 archive_ms=none cost_ms=none storage_ms=500 unowned_ms=7 health_ms=40 \
             lock_wait_ms=none performance=kept performance_age_ms=30000 storage_age_ms=none",
            "a part kept by performance_now is none, not 0"
        );
    }

    /// `answer` with its free disk figure stale or not, and the `storage`
    /// group as `group` (stale since 5000 when stale).
    fn aged_answer(state: &str, figure_stale: bool, group: &Value) -> Value {
        let mut view = answer(state, &json!(2 * GIB));
        view["lines"][1]["guardrails"][0]["stale"] = json!(figure_stale);
        view["groups"] = json!([group]);
        view
    }

    fn group(stale: bool, error: Option<&str>) -> Value {
        json!({"group": "storage", "as_of_ms": 1000, "age_ms": 4000, "every_ms": 600_000,
               "stale_ms": 1_800_000, "stale": stale, "stale_since_ms": 5000,
               "last_error": error, "refresh_ms": 12})
    }

    #[test]
    fn a_stale_figure_neither_raises_nor_clears_its_range_alert() {
        let mut monitor = monitor();
        let fresh = group(false, None);
        // Out and stale the whole hold: no range alert.
        for at in [1000, 1000 + RAISE, 1000 + 2 * RAISE] {
            let (events, _) = read(&mut monitor, at, Ok(aged_answer("out", true, &fresh)));
            assert!(
                events.iter().all(
                    |event| !matches!(event, super::Event::Fire { kind, .. } if *kind == KIND)
                ),
                "{:?}",
                keys(&events)
            );
        }
        // Raised while current; then stale and "in" for the clear's hold:
        // it stays open.
        let low = || Ok(aged_answer("out", false, &fresh));
        read(&mut monitor, 10 * RAISE, low());
        let (fired, _) = read(&mut monitor, 11 * RAISE, low());
        assert_eq!(keys(&fired).len(), 1, "{:?}", keys(&fired));
        commit(&mut monitor, &fired);
        for at in [
            11 * RAISE + 1,
            11 * RAISE + CLEAR + 1,
            11 * RAISE + 2 * CLEAR,
        ] {
            let (events, _) = read(&mut monitor, at, Ok(aged_answer("in", true, &fresh)));
            assert!(events.is_empty(), "{:?}", keys(&events));
        }
    }

    #[test]
    fn a_stale_group_raises_once_and_a_good_refresh_clears_it() {
        let mut monitor = monitor();
        let (fired, _) = read(
            &mut monitor,
            6000,
            Ok(aged_answer("in", true, &group(true, Some("walk failed")))),
        );
        assert_eq!(keys(&fired), [format!("fire {STALE_KIND}:storage:5000")]);
        let super::Event::Fire {
            subject,
            evidence,
            text,
            ..
        } = &fired[0]
        else {
            panic!("a fire");
        };
        assert_eq!(subject, "core lines storage");
        assert_eq!(
            evidence,
            &json!({"group": "storage", "as_of_ms": 1000, "age_ms": 4000, "every_ms": 600_000,
                    "stale_ms": 1_800_000, "last_error": "walk failed", "reading_ms": 6000})
        );
        assert!(
            text.contains("stale") && text.contains("walk failed"),
            "{text}"
        );
        commit(&mut monitor, &fired);
        // Still stale: no second.
        let (again, _) = read(
            &mut monitor,
            7000,
            Ok(aged_answer("in", true, &group(true, Some("walk failed")))),
        );
        assert!(again.is_empty(), "{:?}", keys(&again));
        // A good refresh: cleared.
        let (cleared, _) = read(
            &mut monitor,
            8000,
            Ok(aged_answer("in", false, &group(false, None))),
        );
        assert_eq!(keys(&cleared), [format!("clear {STALE_KIND}:storage:5000")]);
        commit(&mut monitor, &cleared);
        assert!(!monitor.known[&format!("{STALE_KIND}:storage:5000")].open());
    }

    #[test]
    fn a_restarted_loop_adopts_an_open_stale_alert_and_clears_it_at_a_good_refresh() {
        let open = |monitor: &mut Monitor| {
            monitor.known.insert(
                format!("{STALE_KIND}:storage:5000"),
                Row {
                    kind: STALE_KIND.to_string(),
                    subject: "core lines storage".to_string(),
                    state: "open".to_string(),
                    evidence: json!({"group": "storage", "reading_ms": 6000}),
                    fired_event_id: 1,
                    fired_at_ms: 6000,
                },
            );
        };
        // Restarted, still stale (a new stale since, its first try): the
        // open one stands, no second.
        let mut restarted = monitor();
        open(&mut restarted);
        let mut stale = group(true, Some("down"));
        stale["as_of_ms"] = Value::Null;
        stale["stale_since_ms"] = json!(9000);
        let (events, _) = read(&mut restarted, 9500, Ok(aged_answer("in", true, &stale)));
        assert!(events.is_empty(), "{:?}", keys(&events));
        // Its first good refresh clears it.
        let (cleared, _) = read(
            &mut restarted,
            9600,
            Ok(aged_answer("in", false, &group(false, None))),
        );
        assert_eq!(keys(&cleared), [format!("clear {STALE_KIND}:storage:5000")]);
        // A group no longer in the answer clears it as gone.
        let mut gone_one = monitor();
        open(&mut gone_one);
        let mut none = answer("in", &json!(50 * GIB));
        none["groups"] = json!([]);
        let (gone, _) = read(&mut gone_one, 9700, Ok(none));
        assert_eq!(keys(&gone), [format!("clear {STALE_KIND}:storage:5000")]);
        let super::Event::Clear { evidence, .. } = &gone[0] else {
            panic!("a clear");
        };
        assert_eq!(evidence["state"], "gone");
    }

    const H: i64 = 3_600_000;
    /// The sample period the tests run with (the default, 3600 s).
    const PERIOD: i64 = 3_600_000;
    const FREE: &str = "storage.free_bytes";
    /// Free disk out of its range (min 40 GiB) and in it.
    const OUT: i64 = 1_000;
    const IN: i64 = 50_000_000_000;

    /// Samples of free disk at each `(at_ms, value)` (`None`: a sample
    /// without the figure).
    fn samples(points: &[(i64, Option<i64>)]) -> Value {
        Value::Array(
            points
                .iter()
                .map(|(at, value)| match value {
                    Some(value) => json!({"at_ms": at, "values": {FREE: value}}),
                    None => json!({"at_ms": at, "values": {"storage.disk_fall": 1}}),
                })
                .collect(),
        )
    }

    /// [`answer`] with free disk out, as the loop's first reading gives it:
    /// with `samples` as its `recent_samples`.
    fn first(samples: Value) -> Value {
        let mut answer = answer("out", &json!(OUT));
        answer["recent_samples"] = samples;
        answer
    }

    fn fires(events: &[Event]) -> Vec<(String, Value)> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Fire { key, evidence, .. } => Some((key.clone(), evidence.clone())),
                Event::Clear { .. } => None,
            })
            .collect()
    }

    /// One reading at `at`, recorded: the keys it fired.
    fn step(monitor: &mut Monitor, at: i64, answer: Value) -> Vec<String> {
        let (events, _) = read(monitor, at, Ok(answer));
        commit(monitor, &events);
        fires(&events).into_iter().map(|(key, _)| key).collect()
    }

    fn key_at(since: i64) -> String {
        format!("{KIND}:{FREE}:{since}")
    }

    /// Objective 192: a figure's out-since from the samples is the oldest
    /// sample of the newest unbroken run out of its current range reaching
    /// now; a sample in range, one without the figure, or a gap of more
    /// than two periods ends the run.
    #[test]
    fn a_figure_out_takes_its_out_since_from_the_newest_unbroken_run_of_samples() {
        let now = 100 * H;
        let range = json!({"min": 42_949_672_960_i64});
        let seed =
            |points: &[(i64, Option<i64>)]| out_since(&samples(points), FREE, &range, now, PERIOD);
        // A run.
        assert_eq!(
            seed(&[
                (now - 3 * H, Some(OUT)),
                (now - 2 * H, Some(OUT)),
                (now - H, Some(OUT))
            ]),
            Some(now - 3 * H)
        );
        // The newest of two runs.
        assert_eq!(
            seed(&[
                (now - 4 * H, Some(OUT)),
                (now - 3 * H, Some(IN)),
                (now - 2 * H, Some(OUT)),
                (now - H, Some(OUT)),
            ]),
            Some(now - 2 * H)
        );
        // The newest sample in range.
        assert_eq!(seed(&[(now - 2 * H, Some(OUT)), (now - H, Some(IN))]), None);
        // A sample without the figure.
        assert_eq!(
            seed(&[
                (now - 3 * H, Some(OUT)),
                (now - 2 * H, None),
                (now - H, Some(OUT))
            ]),
            Some(now - H)
        );
        // A gap from the start: the newest sample more than two periods ago.
        assert_eq!(seed(&[(now - 2 * H - 1, Some(OUT))]), None);
        assert_eq!(seed(&[(now - 2 * H, Some(OUT))]), Some(now - 2 * H));
        // A gap between samples.
        assert_eq!(
            seed(&[
                (now - 5 * H, Some(OUT)),
                (now - 2 * H, Some(OUT)),
                (now - H, Some(OUT))
            ]),
            Some(now - 2 * H)
        );
        // No samples, and samples that are not a list.
        assert_eq!(seed(&[]), None);
        assert_eq!(out_since(&Value::Null, FREE, &range, now, PERIOD), None);
        assert_eq!(out_since(&json!({"a": 1}), FREE, &range, now, PERIOD), None);
        // Judged against the current range, not an old one: 30 is out of
        // {min: 40} now, whatever range it was read against then.
        let points = samples(&[(now - H, Some(30))]);
        assert_eq!(
            out_since(&points, FREE, &json!({"min": 40}), now, PERIOD),
            Some(now - H)
        );
        assert_eq!(
            out_since(&points, FREE, &json!({"min": 20}), now, PERIOD),
            None
        );
        // No range: no run.
        assert_eq!(out_since(&points, FREE, &Value::Null, now, PERIOD), None);
    }

    /// Objective 192: a restarted loop raises a figure out since an hour in
    /// the samples at that sample's time plus the hold, not a hold after the
    /// restart; it seeds once; a run broken by a sample in range seeds
    /// nothing; a samples error is said and the hold starts then.
    #[test]
    fn a_restarted_loop_raises_at_the_hold_its_samples_began_not_a_hold_after_the_restart() {
        let start = 100 * H;
        let mut seeded = monitor();
        let out = samples(&[(start - 2 * H, Some(IN)), (start - H, Some(OUT))]);
        assert_eq!(step(&mut seeded, start, first(out)), Vec::<String>::new());
        assert_eq!(
            step(&mut seeded, start + H - 1, answer("out", &json!(OUT))),
            Vec::<String>::new()
        );
        let (events, _) = read(&mut seeded, start + H, Ok(answer("out", &json!(OUT))));
        let fired = fires(&events);
        assert_eq!(fired.len(), 1, "{events:?}");
        assert_eq!(fired[0].0, key_at(start - H));
        assert_eq!(fired[0].1["out_since_ms"], start - H);
        assert_eq!(fired[0].1["reading_ms"], start + H);

        // Seeded once: a later reading's samples change nothing.
        let mut once = monitor();
        assert_eq!(
            step(&mut once, start, first(json!([]))),
            Vec::<String>::new()
        );
        let later = first(samples(&[(start + H - 1, Some(OUT))]));
        let mut later = later;
        later["recent_samples"] = samples(&[(start - 5 * H, Some(OUT)), (start, Some(OUT))]);
        assert_eq!(step(&mut once, start + H, later), Vec::<String>::new());
        assert_eq!(
            step(&mut once, start + 2 * H, answer("out", &json!(OUT))),
            [key_at(start)]
        );

        // A run broken by a sample in range seeds nothing: the hold begins
        // at the restart.
        let mut broken = monitor();
        let points = samples(&[(start - 2 * H, Some(OUT)), (start - H, Some(IN))]);
        assert_eq!(
            step(&mut broken, start, first(points)),
            Vec::<String>::new()
        );
        assert_eq!(
            step(&mut broken, start + 2 * H - 1, answer("out", &json!(OUT))),
            Vec::<String>::new()
        );
        assert_eq!(
            step(&mut broken, start + 2 * H, answer("out", &json!(OUT))),
            [key_at(start)]
        );

        // A samples error is said, and the hold starts at this reading.
        let mut failed = monitor();
        let mut answer_failed = answer("out", &json!(OUT));
        answer_failed["recent_samples_error"] = json!("connection reset");
        let (events, lines) = read(&mut failed, start, Ok(answer_failed));
        assert!(fires(&events).is_empty());
        assert!(
            lines.iter().any(
                |line| line.contains("the guardrails' samples: connection reset")
                    && line.contains("each hold starts at this reading")
            ),
            "{lines:?}"
        );
        assert_eq!(
            step(&mut failed, start + 2 * H, answer("out", &json!(OUT))),
            [key_at(start)]
        );
    }

    /// The alert rows a restarted loop starts with: `key`'s alert, fired on
    /// free disk out since `since`, cleared with `cleared` (`{judged}` for a
    /// judgement, else its condition's evidence).
    fn rows(key: &str, since: i64, cleared: &Value) -> Value {
        json!([{
            "key": key, "kind": KIND, "subject": format!("guardrail {FREE}"),
            "state": "cleared",
            "evidence": {"line": "storage", "key": FREE, "out_since_ms": since},
            "fired_at_ms": since + RAISE, "cleared_evidence": cleared,
        }])
    }

    /// A Monitor rebuilt from the persisted `rows`, as `start` reads them.
    fn restarted(rows: &Value) -> Monitor {
        let mut monitor = monitor();
        for row in rows.as_array().into_iter().flatten() {
            if let Some((key, row)) = Row::from_value(row) {
                monitor.known.insert(key, row);
            }
        }
        monitor.seed_judged(rows);
        monitor
    }

    /// Objective 192 (5): a stretch the owner judged false stays judged after
    /// a restart over a slid window, however long it stays out; a samples
    /// error keeps it; a sample in range after it began makes a new stretch,
    /// one before it does not; a newest alert cleared by its condition is
    /// seeded as before.
    #[test]
    fn a_judged_stretch_still_out_after_a_restart_is_not_raised_again() {
        let start = 100 * H;
        // Seed, fire, judge false.
        let mut first_loop = monitor();
        step(
            &mut first_loop,
            start,
            first(samples(&[(start - H, Some(OUT))])),
        );
        let fired = step(&mut first_loop, start + H, answer("out", &json!(OUT)));
        assert_eq!(fired, [key_at(start - H)]);
        let judged = rows(&fired[0], start - H, &json!({"judged": "false"}));

        // Restarted hours later: the window has slid past where it began.
        let restart = start + 5 * H;
        let still_out = samples(&[
            (restart - 3 * H, Some(OUT)),
            (restart - 2 * H, Some(OUT)),
            (restart - H, Some(OUT)),
        ]);
        let mut again = restarted(&judged);
        assert_eq!(
            step(&mut again, restart, first(still_out.clone())),
            Vec::<String>::new()
        );
        for later in [restart + H, restart + 10 * H, restart + 100 * H] {
            assert_eq!(
                step(&mut again, later, answer("out", &json!(OUT))),
                Vec::<String>::new(),
                "{later}"
            );
        }

        // A samples error: nothing shows the stretch ended.
        let mut failed = restarted(&judged);
        let mut answer_failed = answer("out", &json!(OUT));
        answer_failed["recent_samples_error"] = json!("timeout");
        assert!(read(&mut failed, restart, Ok(answer_failed)).0.is_empty());
        assert_eq!(
            step(&mut failed, restart + 10 * H, answer("out", &json!(OUT))),
            Vec::<String>::new()
        );

        // A sample in range after the stretch began: a new stretch, seeded.
        let back = samples(&[
            (restart - 3 * H, Some(IN)),
            (restart - 2 * H, Some(OUT)),
            (restart - H, Some(OUT)),
        ]);
        let mut new = restarted(&judged);
        assert_eq!(
            step(&mut new, restart, first(back)),
            [key_at(restart - 2 * H)]
        );

        // One in range before it began is no evidence.
        let before = samples(&[
            (start - 2 * H, Some(IN)),
            (restart - 2 * H, Some(OUT)),
            (restart - H, Some(OUT)),
        ]);
        let mut kept = restarted(&judged);
        assert_eq!(
            step(&mut kept, restart, first(before)),
            Vec::<String>::new()
        );
        assert_eq!(
            step(&mut kept, restart + 10 * H, answer("out", &json!(OUT))),
            Vec::<String>::new()
        );

        // A newest alert cleared by its condition: seeded as before.
        let by_condition = rows(
            &fired[0],
            start - H,
            &json!({"key": FREE, "state": "in", "reading_ms": start + 2 * H}),
        );
        let mut seeded = restarted(&by_condition);
        assert_eq!(
            step(&mut seeded, restart, first(still_out)),
            [key_at(restart - 3 * H)]
        );
    }

    /// [`answer`] with free disk `state`, its value `value`, stale.
    fn stale(state: &str, value: &Value) -> Value {
        let mut answer = answer(state, value);
        answer["lines"][1]["guardrails"][0]["stale"] = json!(true);
        answer
    }

    /// Objective 192 (6), cc-astra A87: a first reading where the figure is
    /// stale, then not collected, loses nothing: the judged out-since is
    /// taken by the first fresh reading out (no second alert); only a fresh
    /// reading in range ends it, after which a new stretch out is raised
    /// after its own hold.
    #[test]
    fn a_judged_stretch_survives_a_first_reading_with_no_fresh_figure() {
        let since = 50 * H;
        let judged = rows(&key_at(since), since, &json!({"judged": "false"}));
        let restart = 100 * H;
        let mut monitor = restarted(&judged);
        let mut first_stale = stale("out", &json!(OUT));
        first_stale["recent_samples"] = samples(&[(restart - H, Some(OUT))]);
        assert_eq!(
            step(&mut monitor, restart, first_stale),
            Vec::<String>::new()
        );
        assert_eq!(
            step(
                &mut monitor,
                restart + 300_000,
                answer("not_collected", &Value::Null)
            ),
            Vec::<String>::new()
        );
        assert_eq!(
            step(&mut monitor, restart + 600_000, stale("in", &json!(IN))),
            Vec::<String>::new()
        );
        // The first fresh reading out takes the judged out-since.
        for later in [restart + H, restart + 10 * H] {
            assert_eq!(
                step(&mut monitor, later, answer("out", &json!(OUT))),
                Vec::<String>::new(),
                "{later}"
            );
        }
        // A fresh reading in range ends it; the next stretch is new.
        let back = restart + 11 * H;
        step(&mut monitor, back, answer("in", &json!(IN)));
        let out = back + 300_000;
        assert_eq!(
            step(&mut monitor, out, answer("out", &json!(OUT))),
            Vec::<String>::new()
        );
        assert_eq!(
            step(&mut monitor, out + RAISE, answer("out", &json!(OUT))),
            [key_at(out)]
        );
    }

    /// Objective 192 (6), R4: once taken, a judged out-since lives as any
    /// other count: a fresh reading with no range ends it, and the next
    /// reading out starts a new stretch, raised after its own hold.
    #[test]
    fn a_taken_judged_stretch_ends_like_any_other() {
        let since = 50 * H;
        let judged = rows(&key_at(since), since, &json!({"judged": "false"}));
        let restart = 100 * H;
        let mut monitor = restarted(&judged);
        assert_eq!(
            step(
                &mut monitor,
                restart,
                first(samples(&[(restart - H, Some(OUT))]))
            ),
            Vec::<String>::new()
        );
        assert_eq!(
            step(
                &mut monitor,
                restart + 300_000,
                answer("no_range", &json!(OUT))
            ),
            Vec::<String>::new()
        );
        let out = restart + 600_000;
        assert_eq!(
            step(&mut monitor, out, answer("out", &json!(OUT))),
            Vec::<String>::new()
        );
        assert_eq!(
            step(&mut monitor, out + RAISE - 1, answer("out", &json!(OUT))),
            Vec::<String>::new()
        );
        assert_eq!(
            step(&mut monitor, out + RAISE, answer("out", &json!(OUT))),
            [key_at(out)]
        );
    }
}
