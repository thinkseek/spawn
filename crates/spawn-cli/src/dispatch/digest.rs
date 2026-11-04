//! The supervisor digest: pending escalations coalesced into one message,
//! typed into the supervisor's pane while it is idle. What the supervisor
//! was already told is read back from the markers in delivered digests.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use spawn_core::command::CommandAction;
use spawn_core::observation::SemanticState;

use super::facts::{Escalation, Facts, Order, Why};
use crate::supervise::{SUPERVISOR_AGENT, SUPERVISOR_SESSION};

/// Escalations per digest; the rest wait for the next one.
const DIGEST_ITEMS: usize = 8;

/// Escalations the supervisor already received, from the markers in the
/// digests that reached its pane.
pub(super) fn told(supervisor: Option<&Facts>) -> BTreeSet<String> {
    supervisor
        .map(|facts| {
            facts
                .dispatched
                .iter()
                .filter(|sent| sent.succeeded == Some(true))
                .filter_map(|sent| sent.text.as_deref())
                .flat_map(|text| {
                    text.lines()
                        .filter_map(|line| {
                            let start = line.find("[spawn:esc ")?;
                            let end = line[start..].find(']')? + start + 1;
                            Some(line[start..end].to_string())
                        })
                        .collect::<Vec<_>>()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether the supervisor can take a digest now: live, idle at its prompt,
/// nothing in flight.
fn supervisor_idle(supervisor: &Facts) -> bool {
    supervisor.live_and_free()
        && supervisor.observation.as_ref().is_some_and(|observation| {
            matches!(
                observation.state,
                SemanticState::Waiting | SemanticState::Completed
            )
        })
}

fn fnv(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
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
            "- {} — {}: {} {}",
            item.session_id,
            heading(item.why),
            item.line,
            item.marker()
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
    let markers: String = items.iter().map(Escalation::marker).collect();
    Some(Order {
        agent_id: SUPERVISOR_AGENT.to_string(),
        session_id: SUPERVISOR_SESSION.to_string(),
        // A new supervisor judgement allows one retry of an undelivered digest.
        key: format!(
            "dispatch-digest-{}-{:016x}",
            supervisor.observation_event_id.unwrap_or(0),
            fnv(&markers)
        ),
        action: CommandAction::Input {
            text: text(&items, spawn),
        },
        task: None,
    })
}
