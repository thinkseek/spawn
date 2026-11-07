//! A Stop that failed is pursued, whoever sent it: the dispatcher's own
//! (a model switch, a closed task's worker, a retired resident), the
//! integrator's budget abort, the supervisor's or a person's.
//!
//! What a Stop did is its command's recorded outcome, never the intent to
//! send it (docs/design.md §14). A Stop fails when the execution could not
//! be ended (the terminal will not die, the final checkpoint keeps failing):
//! the worker still runs. The Bridge keeps trying that end on every tick,
//! but only in its memory, which a restart forgets. So the dispatcher reads
//! every Stop and Discard sent during the session's live execution from the
//! command queue ([`Facts::stops`]), and while the latest one failed:
//!
//! - it sends the same action again under a key of its own
//!   (`stop-<execution>-retry-<n>`), once the wait after that failure is
//!   over (a minute, doubling: [`crate::deliver::retry_wait_ms`]), up to
//!   [`DELIVERY_RETRIES`] times. A failed Stop changed nothing on the
//!   worker, so a retry does not wait for a fresh judgement of it: the
//!   decision to stop was already taken;
//! - once the retries are spent, it escalates [`Why::StopFailed`] to the
//!   supervisor, naming the task the stop was for, and sends no more.
//!
//! Meanwhile no task is typed into the worker and the worker ladder leaves
//! it alone (no nudge into a worker someone wants stopped).

use spawn_core::command::CommandAction;

use super::worker::Decision;
use super::{Escalation, Facts, Order, StopSent, Why};
use crate::deliver::{DELIVERY_RETRIES, one_line, retry_wait_ms};

/// What was the stop for, from the first one sent during the execution.
fn purpose(first: &StopSent) -> String {
    let task = |rest: &str| rest.split('-').next().and_then(|id| id.parse::<i64>().ok());
    let named = |prefix: &str| first.key.strip_prefix(prefix).and_then(task);
    if let Some(id) = named("switch-model-") {
        format!("task #{id} waits for it to stop, to resume it on the task's model profile")
    } else if let Some(id) = named("retire-task-") {
        format!("task #{id} is closed, so its worker is stopped")
    } else if let Some(id) = named("integrate-abort-") {
        format!("task #{id} was aborted over its mission's budget")
    } else if first.key.starts_with("retire-") {
        "the resident was retired".to_string()
    } else {
        format!("`{}` asked to stop it", first.principal)
    }
}

/// The pursuit of a live worker's failed stop: its retry once due, or the
/// escalation once the retries are spent. Only for a worker whose stop is
/// under way ([`Facts::stopping`]).
pub(super) fn pursue(facts: &Facts, now_ms: i64) -> Decision {
    let (Some(first), Some(last)) = (facts.stops.first(), facts.stops.last()) else {
        return Decision::Nothing;
    };
    // In flight, or done (the end is the next fact).
    if last.succeeded != Some(false) || facts.open_command {
        return Decision::Nothing;
    }
    let execution = facts.execution_id.as_deref().unwrap_or("none");
    let failures = facts
        .stops
        .iter()
        .filter(|stop| stop.succeeded == Some(false))
        .count();
    if failures > DELIVERY_RETRIES {
        return Decision::Escalate(Escalation::about(
            facts,
            format!("stop-{execution}"),
            Why::StopFailed,
            format!(
                "{}, but its execution {execution} could not be stopped ({failures} attempts; \
                 the last: {}); no more are sent",
                purpose(first),
                one_line(last.error.as_deref().unwrap_or("no error recorded"), 200)
            ),
        ));
    }
    let finished = last.finished_at_ms.unwrap_or(last.created_at_ms);
    if now_ms < finished.saturating_add(retry_wait_ms(failures)) {
        return Decision::Nothing;
    }
    let action = if last.action == "discard" {
        CommandAction::Discard
    } else {
        CommandAction::Stop
    };
    Decision::Act(Order::to(
        facts,
        format!("stop-{execution}-retry-{failures}"),
        action,
    ))
}

/// When the dispatcher's next step is due for a failed Stop: the wait after
/// that failure ends, [`retry_wait_ms`]. `None` while the stop is not under
/// way, its latest attempt is in flight or succeeded, or its retries are
/// spent (it waits for no clock then: the escalation goes with the next
/// step).
pub(super) fn next_due(facts: &Facts) -> Option<i64> {
    let (Some(_first), Some(last)) = (facts.stops.first(), facts.stops.last()) else {
        return None;
    };
    if last.succeeded != Some(false) || facts.open_command {
        return None;
    }
    let failures = facts
        .stops
        .iter()
        .filter(|stop| stop.succeeded == Some(false))
        .count();
    if failures > DELIVERY_RETRIES {
        return None;
    }
    let finished = last.finished_at_ms.unwrap_or(last.created_at_ms);
    Some(finished.saturating_add(retry_wait_ms(failures)))
}

#[cfg(test)]
mod tests {
    use super::next_due;
    use crate::deliver::{DELIVERY_RETRIES, retry_wait_ms};
    use crate::dispatch::{Facts, StopSent};
    use spawn_core::event::ModelProfile;

    fn stopping(stops: Vec<StopSent>) -> Facts {
        Facts {
            session_id: "session_resident_core".to_string(),
            agent_id: "agent_resident_core".to_string(),
            live: true,
            execution_id: Some("exec_1".to_string()),
            end_cause: None,
            observation: None,
            observation_event_id: None,
            observation_at_ms: None,
            evidence_seq: None,
            turn_ended: false,
            checkpoint: None,
            episode: None,
            open_command: false,
            dispatched: Vec::new(),
            told: std::collections::BTreeSet::new(),
            last_delivery: None,
            model: ModelProfile::Default,
            resumed_young: 0,
            failed_resume: None,
            stops,
            charter: None,
            jobs: Vec::new(),
        }
    }

    fn stop(id: i64, finished: i64, succeeded: Option<bool>) -> StopSent {
        StopSent {
            id,
            key: format!("switch-model-7-0-{id}"),
            principal: "spawn-dispatch".to_string(),
            action: "stop".to_string(),
            created_at_ms: finished - 500,
            finished_at_ms: Some(finished),
            succeeded,
            error: succeeded
                .filter(|ok| !ok)
                .map(|_| "kill-session refused".to_string()),
        }
    }

    /// A failed Stop's retry is due one wait after the failure finished
    /// (`retry_wait_ms`, doubling), and the waits stop when the retries are
    /// spent: then the step escalates instead of waiting.
    #[test]
    fn a_failed_stop_is_due_one_wait_after_it_failed() {
        let failed = stopping(vec![stop(1, 100_000, Some(false))]);
        assert_eq!(next_due(&failed), Some(100_000 + retry_wait_ms(1)));
        let twice = stopping(vec![stop(1, 100_000, Some(false)), stop(2, 200_000, Some(false))]);
        assert_eq!(next_due(&twice), Some(200_000 + retry_wait_ms(2)));
        let spent = stopping(
            (0..=DELIVERY_RETRIES)
                .map(|n| stop(i64::try_from(n).expect("small"), 100_000 + n as i64 * 1000, Some(false)))
                .collect(),
        );
        assert_eq!(next_due(&spent), None, "the retries are spent: escalate");
    }

    /// Nothing waits while the latest Stop is in flight or succeeded, or
    /// while a command of the session is in flight (the retry would be
    /// queued behind it anyway).
    #[test]
    fn a_stop_that_is_not_failing_waits_for_no_time() {
        assert_eq!(next_due(&stopping(Vec::new())), None);
        assert_eq!(
            next_due(&stopping(vec![stop(1, 100_000, None)])),
            None,
            "in flight"
        );
        assert_eq!(
            next_due(&stopping(vec![stop(1, 100_000, Some(true))])),
            None,
            "done"
        );
        let mut busy = stopping(vec![stop(1, 100_000, Some(false))]);
        busy.open_command = true;
        assert_eq!(next_due(&busy), None, "a command is in flight");
    }
}
