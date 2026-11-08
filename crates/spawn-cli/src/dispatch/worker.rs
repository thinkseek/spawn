//! The decision ladder for one worker (docs/fleet-architecture.md §3):
//! rules over facts, then the one guarded nudge, then escalation.

use spawn_core::bridge::MAY_HAVE_TYPED;
use spawn_core::command::CommandAction;
use spawn_core::observation::{Ambiguity, Observation, SemanticState, WaitReason};

use super::{Escalation, Facts, Order, Sent, Why};

/// Nudges per worker per hour, across episodes.
pub(super) const NUDGES_PER_HOUR: usize = 3;

/// Least confidence of a WAITING judgement that may nudge.
const NUDGE_CONFIDENCE: f64 = 0.7;

/// Most `needs_human` that may nudge.
const NUDGE_NEEDS_HUMAN: f64 = 0.2;

/// Most `stuck` that may nudge.
const NUDGE_STUCK: f64 = 0.5;

/// Idempotency-key prefix of every nudge, used to count them per hour.
const NUDGE_KEY_PREFIX: &str = "dispatch-nudge-";

/// Retries of a nudge that typed nothing, in one episode.
const NUDGE_RETRIES: usize = 2;

const HOUR_MS: i64 = 3_600_000;

/// What the ladder decides for one worker.
pub(super) enum Decision {
    Nothing,
    Act(Order),
    Escalate(Escalation),
}

fn percent(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_string(), |value| format!("{:.0}%", value * 100.0))
}

/// One line on what the Observer judged, for the digest.
fn describe(observation: &Observation) -> String {
    let mut parts = vec![format!(
        "{} by {} ({:.0}%)",
        observation.state.key(),
        observation.judge,
        observation.confidence.value() * 100.0
    )];
    if let Some(ambiguity) = observation.ambiguity {
        parts.push(format!("rules could not decide: {ambiguity:?}"));
    }
    if observation.needs_human.is_some() || observation.stuck.is_some() {
        parts.push(format!(
            "needs_human {} stuck {}",
            percent(observation.needs_human),
            percent(observation.stuck)
        ));
    }
    if observation.reason == Some(WaitReason::JudgeUnavailable) {
        parts.push("Jev unreachable, rules' guess".to_string());
    }
    parts.join("; ")
}

/// Whether Jev's judgement allows the one reversible nudge.
fn nudgeable(observation: &Observation) -> bool {
    observation.judge == "jev"
        && observation.state == SemanticState::Waiting
        && observation.ambiguity == Some(Ambiguity::TurnStoppedIdle)
        && !observation.attention
        && observation.confidence.value() >= NUDGE_CONFIDENCE
        && observation
            .needs_human
            .is_some_and(|value| value <= NUDGE_NEEDS_HUMAN)
        && observation.stuck.is_none_or(|value| value < NUDGE_STUCK)
}

/// The one-resume rule (docs/fleet-architecture.md §3): a lost execution is
/// resumed once; a failed resume escalates, never resumed again by rule.
/// Failed is read from what the log records about the session, since its
/// last execution that stayed up, not from the key of its latest execution
/// (a resumed execution has a new id, so its own key would resume it again
/// forever):
///
/// - a resume whose execution ended lost before its engine became ready
///   ([`Facts::failed_resume`], whatever its command's outcome: the ready
///   wait failed, the start was abandoned), escalated with the evidence,
///   once: the item is named by that execution, so a later execution that
///   fails the same way (a person's resume) is not told again;
/// - a resume whose engine was lost again within a minute
///   ([`Facts::resumed_young`]);
/// - a resume whose command failed before it started an execution (the
///   current execution's key, `dispatch-resume-<execution>`, failed).
fn resume_lost(facts: &Facts) -> Decision {
    let execution = facts.execution_id.as_deref().unwrap_or("none");
    if let Some(failed) = &facts.failed_resume {
        let mut escalation = Escalation::about(
            facts,
            format!("resume-{}", failed.execution_id),
            Why::ResumeFailed,
            format!(
                "engine was lost and its automatic resume (command {}, execution {}) ended lost \
                 before its engine became ready: {}; not resumed again",
                failed
                    .command_id
                    .map_or_else(|| "-".to_string(), |id| id.to_string()),
                failed.execution_id,
                crate::deliver::one_line(failed.why(), 300)
            ),
        );
        // It rests on that execution's end, not on a judgement.
        escalation.since_ms = failed.ended_at_ms.or(escalation.since_ms);
        return Decision::Escalate(escalation);
    }
    if facts.resumed_young > 0 {
        return Decision::Escalate(Escalation::about(
            facts,
            format!("resume-{execution}"),
            Why::ResumeFailed,
            "engine was lost again within a minute of its automatic resume".to_string(),
        ));
    }
    let key = format!("dispatch-resume-{execution}");
    match facts.sent(&key) {
        None => Decision::Act(Order::to(
            facts,
            key,
            CommandAction::Resume {
                prompt: Some(
                    "Spawn restarted your engine after it was lost. Check where you were \
                     and continue your task."
                        .to_string(),
                ),
                // The session's own profile: a lost cheap worker stays cheap.
                model: None,
            },
        )),
        Some(resume) if resume.succeeded == Some(false) => Decision::Escalate(Escalation::about(
            facts,
            format!("resume-{execution}"),
            Why::ResumeFailed,
            "engine was lost and the automatic resume failed".to_string(),
        )),
        Some(_) => Decision::Nothing,
    }
}

/// Whether a nudge reached the worker, or may have: it succeeded, is still
/// in flight, or failed after typing may have begun. One that failed before
/// typing (a pane not ready, a terminal gone) told the worker nothing.
fn reached(sent: &Sent) -> bool {
    reached_with(sent.succeeded, sent.error.as_deref())
}

/// [`reached`], from a command's outcome: `succeeded` (`None` while in
/// flight) and its `error`.
pub(crate) fn reached_with(succeeded: Option<bool>, error: Option<&str>) -> bool {
    succeeded != Some(false) || error.is_some_and(|error| error.contains(MAY_HAVE_TYPED))
}

/// Nudges that reached this worker within the last hour, across episodes.
fn nudges_this_hour(facts: &Facts, now_ms: i64) -> usize {
    facts
        .dispatched
        .iter()
        .filter(|sent| {
            sent.key.starts_with(NUDGE_KEY_PREFIX)
                && reached(sent)
                && now_ms.saturating_sub(sent.created_at_ms) < HOUR_MS
        })
        .count()
}

/// How long until this worker's nudge limit frees up with no new record:
/// the moment a nudge that reached it leaves the hour
/// ([`nudges_this_hour`]). `None` when nothing keeps it from a nudge now.
pub(super) fn due_in(facts: &Facts, now_ms: i64) -> Option<i64> {
    if !facts.live || facts.open_command {
        return None;
    }
    if !nudgeable(facts.current_observation()?) {
        return None;
    }
    facts
        .dispatched
        .iter()
        .filter(|sent| sent.key.starts_with(NUDGE_KEY_PREFIX) && reached(sent))
        .map(|sent| {
            sent.created_at_ms
                .saturating_add(HOUR_MS)
                .saturating_sub(now_ms)
        })
        .filter(|left| *left > 0)
        .min()
}

/// The guarded nudge, for a worker Jev judged idle and fine: `continue`
/// once per episode within the hourly limit; a nudge that did not help, or
/// the limit, escalates. Only a nudge that reached the worker ([`reached`])
/// can have failed to help: one that typed nothing is sent again under a
/// key of its own (`<key>-retry-<n>`) on a judgement made after it, up to
/// [`NUDGE_RETRIES`] times, and then escalated as never typed.
fn nudge(facts: &Facts, observation: &Observation, now_ms: i64) -> Decision {
    let episode = facts.episode.unwrap_or(0);
    let judged_at = facts.observation_at_ms.unwrap_or(0);
    let key = format!("{NUDGE_KEY_PREFIX}{}-{episode}", facts.session_id);
    let retry = format!("{key}-retry-");
    let attempts: Vec<&Sent> = facts
        .dispatched
        .iter()
        .filter(|sent| sent.key == key || sent.key.starts_with(&retry))
        .collect();
    let judged_after = |sent: &Sent| sent.finished_at_ms.is_some_and(|done| judged_at > done);
    match attempts.last().copied() {
        None if nudges_this_hour(facts, now_ms) < NUDGES_PER_HOUR => Decision::Act(Order::to(
            facts,
            key,
            CommandAction::Input {
                text: "continue".to_string(),
                tags: Vec::new(),
            },
        )),
        None => Decision::Escalate(Escalation::about(
            facts,
            format!("nudges-{episode}"),
            Why::NudgeFailed,
            format!(
                "stopped again; nudge limit of {NUDGES_PER_HOUR}/hour reached ({})",
                describe(observation)
            ),
        )),
        // Judged idle again after the nudge landed: it did not help.
        Some(sent) if reached(sent) && judged_after(sent) => Decision::Escalate(Escalation::about(
            facts,
            format!("nudge-{episode}"),
            Why::NudgeFailed,
            format!("stopped again after `continue` ({})", describe(observation)),
        )),
        // Typed nothing, and still judged idle after it: again, or, once
        // the retries are spent, someone has to look.
        Some(sent) if !reached(sent) && judged_after(sent) => {
            if attempts.len() > NUDGE_RETRIES {
                return Decision::Escalate(Escalation::about(
                    facts,
                    format!("nudge-{episode}"),
                    Why::NudgeFailed,
                    format!(
                        "`continue` could not be typed in {} attempts (the last: {}); it is \
                         still idle ({})",
                        attempts.len(),
                        crate::deliver::one_line(sent.error.as_deref().unwrap_or("no error"), 200),
                        describe(observation)
                    ),
                ));
            }
            Decision::Act(Order::to(
                facts,
                format!("{retry}{}", attempts.len()),
                CommandAction::Input {
                    text: "continue".to_string(),
                    tags: Vec::new(),
                },
            ))
        }
        Some(_) => Decision::Nothing,
    }
}

/// The policy for one worker: one order, one escalation, or nothing.
pub(super) fn decide_worker(facts: &Facts, now_ms: i64) -> Decision {
    // Something is in flight; its outcome is the next fact.
    if facts.open_command {
        return Decision::Nothing;
    }
    // 1. Rules over facts. The recovery (crate::recover) revives every end
    // it recorded, whatever the cause and whoever the agent; the one-resume
    // rule below is what is left for an end it has not recorded (a host
    // whose loop is not running the recovery), so only ephemeral workers
    // are resumed here.
    if !facts.live
        && facts.recovered.is_none()
        && facts.end_cause.as_deref() == Some("lost")
        && facts.agent_id.starts_with("agent_worker_")
    {
        return resume_lost(facts);
    }
    // Act only on a current judgement; a stale one waits for the next.
    let Some(observation) = facts.current_observation() else {
        return Decision::Nothing;
    };
    // 2. The one reversible action a Jev judgement may trigger.
    if facts.live && nudgeable(observation) {
        return nudge(facts, observation, now_ms);
    }
    // 3. Everything else that matters escalates.
    if observation.attention {
        let id = facts.observation_event_id.map_or_else(
            || format!("seq-{}", observation.based_on_seq.0),
            |id| id.to_string(),
        );
        return Decision::Escalate(Escalation::about(
            facts,
            id,
            Why::Attention,
            describe(observation),
        ));
    }
    let episode = facts.episode.unwrap_or(0);
    // Stopped at its prompt and not nudged: someone has to give the next
    // step or review the result. Once per episode.
    if facts.live && observation.ambiguity == Some(Ambiguity::TurnStoppedIdle) {
        let (why, line) = if observation.state == SemanticState::Completed {
            (Why::Completed, "finished its task")
        } else {
            (Why::Idle, "stopped at its prompt")
        };
        return Decision::Escalate(Escalation::about(
            facts,
            format!("idle-{episode}"),
            why,
            format!("{line} ({})", describe(observation)),
        ));
    }
    // An execution someone stopped ended by their decision; telling them
    // again is noise. Read from how it ended, never from a Stop having been
    // sent: one that failed stopped nothing, and the engine may have exited
    // by itself afterwards.
    if !facts.live
        && observation.state == SemanticState::Completed
        && facts.end_cause.as_deref() != Some("stopped")
    {
        return Decision::Escalate(Escalation::about(
            facts,
            format!("done-{episode}"),
            Why::Completed,
            format!("execution ended, task finished ({})", describe(observation)),
        ));
    }
    Decision::Nothing
}
