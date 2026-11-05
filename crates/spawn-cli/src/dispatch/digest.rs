//! The supervisor digest: pending escalations coalesced into one message,
//! typed into the supervisor's pane while it is idle. What the supervisor
//! was already told is read back from the tags delivered digests filed on
//! their commands, never from their text: that quotes workers' reports.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use spawn_core::command::CommandAction;
use spawn_core::observation::SemanticState;

use super::{Escalation, Facts, Order, Why};
use crate::supervise::{SUPERVISOR_AGENT, SUPERVISOR_SESSION};

/// Escalations per digest; the rest wait for the next one.
const DIGEST_ITEMS: usize = 8;

/// Escalations the supervisor already received: the tags of the digests
/// that reached its pane.
pub(super) fn told(supervisor: Option<&Facts>) -> BTreeSet<String> {
    supervisor
        .map(|facts| facts.told.clone())
        .unwrap_or_default()
}

/// Whether the supervisor can take a digest now: live, idle on its latest
/// fact, nothing in flight.
fn supervisor_idle(supervisor: &Facts) -> bool {
    supervisor.live_and_free()
        && supervisor.current_observation().is_some_and(|observation| {
            matches!(
                observation.state,
                SemanticState::Waiting | SemanticState::Completed
            )
        })
}

/// What the supervisor is asked to do about an item.
fn heading(why: Why) -> &'static str {
    match why {
        Why::Attention => "attention",
        Why::NudgeFailed => "nudge did not help",
        Why::ResumeFailed => "resume failed",
        Why::Completed => "completed, review it",
        Why::Idle => "idle, give it the next step or review it",
        Why::Mission => "new mission, plan it and assign tasks",
        Why::Report => "report, review it (`task accept` or `task reject`)",
        Why::Approved => "resident approved",
        Why::Human => "human message",
        Why::DeliveryFailed => "delivery failed",
        Why::Disk => "disk space on this host",
    }
}

/// The digest typed into the supervisor's pane.
fn text(items: &[Escalation], spawn: &str) -> String {
    let mut text = format!(
        "Spawn dispatcher: {} update{} for you. Check a worker with `{spawn} tail --session \
<session>` before acting; missions and tasks with `{spawn} mission list`.\n",
        items.len(),
        if items.len() == 1 { "" } else { "s" },
    );
    for item in items {
        let _ = writeln!(
            text,
            "- {} — {}: {}",
            item.session_id,
            heading(item.why),
            item.line
        );
    }
    text
}

/// The digest order for the pending escalations, if the supervisor can
/// take one now.
pub(super) fn order(supervisor: &Facts, pending: &[Escalation], spawn: &str) -> Option<Order> {
    if pending.is_empty() || !supervisor_idle(supervisor) {
        return None;
    }
    let items: Vec<Escalation> = pending.iter().take(DIGEST_ITEMS).cloned().collect();
    let tags: Vec<String> = items.iter().map(Escalation::tag).collect();
    Some(Order {
        agent_id: SUPERVISOR_AGENT.to_string(),
        session_id: SUPERVISOR_SESSION.to_string(),
        // A new supervisor judgement allows one retry of an undelivered digest.
        key: format!(
            "dispatch-digest-{}-{:016x}",
            supervisor.observation_event_id.unwrap_or(0),
            crate::sources::fnv(&tags.join("\n"))
        ),
        action: CommandAction::Input {
            text: text(&items, spawn),
            tags,
        },
        task: None,
    })
}
