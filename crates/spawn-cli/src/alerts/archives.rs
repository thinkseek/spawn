//! `archive_failed` (objective 193): the transcript archive's failures are
//! an alert.
//!
//! The Bridge records a transcript's archive failure and its end as events
//! on the session's stream, on a change only (objective 179):
//! `archive.failed {native, reason}` and `archive.resumed {native, copied}`.
//! The step reads them after the last one it read ([`Monitor::read_archives`])
//! and folds them, in order, into occurrences: per transcript (agent and
//! native session), an occurrence runs from its first `archive.failed`
//! after the last `archive.resumed`. A repeat (a restarted Bridge says it
//! again) or a new reason changes only the reason; the occurrence keeps its
//! first failure's id and time, its key, and nothing more fires.
//!
//! When it is read: at the first check, even on a log that does not move
//! (its first read begins at the first event, so a restarted loop rebuilds
//! every occurrence before it applies a new one, and an alert the store
//! holds fires nothing again), when the log moved, and again after a read
//! that failed (`archives_due`, set by the read's own answer).

use serde_json::{Value, json};

use super::{Event, Monitor, scrub, time_text};

/// The kind.
pub(super) const KIND: &str = "archive_failed";

/// A transcript's current occurrence: its alert's key and its latest reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Occurrence {
    pub(super) key: String,
    pub(super) reason: String,
}

impl Monitor {
    /// Read the archive events after the last one read, when due (the first
    /// check, a log that moved, a read that failed before), and record what
    /// each decides before the next is applied.
    pub(super) fn read_archives(
        &mut self,
        store: &spawn_postgres::PostgresStore,
        now: i64,
        moved: bool,
        lines: &mut Vec<String>,
    ) {
        if !moved && !self.archives_due {
            return;
        }
        let read = store
            .alert_archives(self.archives_after)
            .map_err(|error| error.to_string());
        self.archives_read(read, lines, |monitor, events, lines| {
            monitor.flush(store, now, &events, lines);
        });
    }

    /// Apply one read of the archive events: each in order, its alert
    /// transitions handed to `record` before the next is applied, so a
    /// resume's clear stands before a later failure of its transcript opens
    /// a new occurrence. A read that failed is said and due again.
    pub(super) fn archives_read(
        &mut self,
        read: Result<Value, String>,
        lines: &mut Vec<String>,
        mut record: impl FnMut(&mut Self, Vec<Event>, &mut Vec<String>),
    ) {
        let rows = match read {
            Ok(rows) => rows,
            Err(error) => {
                lines.push(format!(
                    "alerts: read the transcript archive's events: {error}"
                ));
                self.archives_due = true;
                return;
            }
        };
        self.archives_due = false;
        for row in rows.as_array().into_iter().flatten() {
            let mut out = Vec::new();
            self.archive_event(row, &mut out);
            if let Some(id) = row["event_id"].as_i64() {
                self.archives_after = id;
            }
            if !out.is_empty() {
                record(self, out, lines);
            }
        }
    }

    /// One archive event: a failure opens its transcript's occurrence (or
    /// changes its reason), a resume ends it and clears each open alert of
    /// the transcript fired before it.
    fn archive_event(&mut self, row: &Value, out: &mut Vec<Event>) {
        let (Some(id), Some(agent), Some(native)) = (
            row["event_id"].as_i64(),
            row["agent_id"].as_str(),
            row["native"].as_str(),
        ) else {
            return;
        };
        let transcript = (agent.to_string(), native.to_string());
        let session = row["session_id"].as_str().unwrap_or_default();
        match row["name"].as_str().unwrap_or_default() {
            "archive.failed" => {
                let reason = scrub(row["reason"].as_str().unwrap_or_default());
                if let Some(occurrence) = self.archive_occurrences.get_mut(&transcript) {
                    occurrence.reason = reason;
                    return;
                }
                let key = format!("{KIND}:{agent}:{native}:{id}");
                let since = row["at_ms"].as_i64().unwrap_or_default();
                out.push(Event::Fire {
                    key: key.clone(),
                    kind: KIND,
                    subject: agent.to_string(),
                    evidence: json!({
                        "agent_id": agent, "session_id": session, "native": native,
                        "reason": reason, "since_ms": since, "event_id": id,
                    }),
                    text: format!(
                        "the transcript archive of {agent} (session {session}, native session \
                         {native}) copies nothing: {reason}; unarchived since {} UTC; at risk \
                         from Claude Code's cleanup",
                        time_text(since)
                    ),
                    notice: false,
                });
                self.archive_occurrences
                    .insert(transcript, Occurrence { key, reason });
            }
            "archive.resumed" => {
                self.archive_occurrences.remove(&transcript);
                for key in self.open_archive_alerts(agent, native, id) {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({
                            "agent_id": agent, "session_id": session, "native": native,
                            "event_id": id,
                        }),
                    });
                }
            }
            _ => {}
        }
    }

    /// The open `archive_failed` alerts of a transcript fired on an event
    /// before `before`.
    fn open_archive_alerts(&self, agent: &str, native: &str, before: i64) -> Vec<String> {
        self.known
            .iter()
            .filter(|(_, row)| {
                row.open()
                    && row.kind == KIND
                    && row.evidence["agent_id"].as_str() == Some(agent)
                    && row.evidence["native"].as_str() == Some(native)
                    && row.evidence["event_id"]
                        .as_i64()
                        .is_some_and(|fired| fired < before)
            })
            .map(|(key, _)| key.clone())
            .collect()
    }
}
