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
