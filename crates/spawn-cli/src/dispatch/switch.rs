//! Model-switch Stops that fail: retried, then given up and escalated
//! (GitHub issue #3).
//!
//! A task for a live worker on the other model profile stops the worker
//! first ([`crate::deliver`], under `switch-model-<task>-<revision>`); a later
//! pass resumes it on the task's profile with the task. A Stop that fails
//! leaves the worker live on the wrong profile and the task waiting. It is
//! tried again like a failed delivery: under
//! `switch-model-<task>-<revision>-retry-<n>`, with a wait of a minute that
//! doubles up to 30 minutes, [`RETRIES`] times. Then no Stop goes for that
//! revision, the task stays waiting (never marked delivered: a Stop carries
//! no task), and the supervisor's digest carries a `DeliveryFailed`
//! escalation that names the task.
//!
//! A retry goes on the worker's last judgement of idle at its prompt, not a
//! current one ([`Facts::last_judged_idle`]): a failed Stop changed nothing
//! on the worker, but its own records are newer than that judgement, which
//! an Observer without a model judge does not write again while nothing
//! changes.

use spawn_core::command::CommandAction;
use spawn_core::mission::TaskState;

use crate::deliver::{TaskRow, Work};
use crate::dispatch::{Escalation, Facts, Order, Sent, Why};

/// Retries of a failed switch Stop, as for a failed delivery: at most
/// `RETRIES + 1` Stops per task revision.
const RETRIES: usize = 3;

/// Wait before the first retry; it doubles for each later one.
const RETRY_MS: i64 = 60_000;

/// Longest wait between two retries.
const RETRY_MAX_MS: i64 = 30 * 60_000;

/// Idempotency key of the Stop that switches a task's worker to the task's
/// model profile, for its current revision.
pub(crate) fn key(task: &TaskRow) -> String {
    format!("switch-model-{}-{}", task.id, task.revisions)
}

/// Where a task's switch Stops stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// None sent, or the latest one is running or succeeded: nothing for
    /// this module to do.
    Quiet,
    /// The latest failed; retry `number` goes at `due_ms`.
    Retry { number: usize, due_ms: i64 },
    /// The latest failed and the retries are spent.
    GivenUp,
}

/// The switch Stops sent for `task`'s revision into `facts`' session, oldest
/// first.
fn sent<'a>(task: &TaskRow, facts: &'a Facts) -> Vec<&'a Sent> {
    let key = key(task);
    let retry = format!("{key}-retry-");
    facts
        .dispatched
        .iter()
        .filter(|sent| sent.key == key || sent.key.starts_with(&retry))
        .collect()
}

fn state(task: &TaskRow, facts: &Facts) -> State {
    let sent = sent(task, facts);
    let Some(last) = sent.last() else {
        return State::Quiet;
    };
    if last.succeeded != Some(false) {
        return State::Quiet;
    }
    if sent.len() > RETRIES {
        return State::GivenUp;
    }
    let doublings = u32::try_from(sent.len() - 1).unwrap_or(u32::MAX).min(20);
    let wait = RETRY_MS
        .saturating_mul(1_i64 << doublings)
        .min(RETRY_MAX_MS);
    State::Retry {
        number: sent.len(),
        due_ms: last
            .finished_at_ms
            .unwrap_or(last.created_at_ms)
            .saturating_add(wait),
    }
}

/// Waiting tasks (assigned, or sent back) with their worker's facts.
fn waiting<'a>(
    work: &'a Work,
    rows: &'a [Facts],
) -> impl Iterator<Item = (&'a TaskRow, &'a Facts)> {
    work.tasks
        .iter()
        .filter(|task| matches!(task.state, TaskState::Assigned | TaskState::Revise))
        .filter_map(|task| {
            let facts = rows
                .iter()
                .find(|facts| facts.session_id == task.session_id)?;
            Some((task, facts))
        })
}

/// Fit the delivery orders to the switch Stops that failed: a Stop planned
/// again under a key already used goes (it would only find the old command),
/// and a retry that is due goes in, to a worker still live on the other
/// profile, last judged idle at its prompt, with nothing in flight and
/// nothing else planned for its session.
pub(crate) fn fit(orders: &mut Vec<Order>, work: &Work, rows: &[Facts], now_ms: i64) {
    let blocked = work.blocked();
    for (task, facts) in waiting(work, rows) {
        let state = state(task, facts);
        if state == State::Quiet {
            continue;
        }
        let first = key(task);
        orders.retain(|order| order.key != first);
        let State::Retry { number, due_ms } = state else {
            continue;
        };
        let session = task.session_id.as_str();
        let free =
            !blocked.contains(session) && !orders.iter().any(|order| order.session_id == session);
        if now_ms >= due_ms
            && free
            && facts.live
            && facts.model != task.model
            && facts.last_judged_idle()
        {
            orders.push(Order {
                agent_id: task.agent_id.clone(),
                session_id: task.session_id.clone(),
                key: format!("{first}-retry-{number}"),
                action: CommandAction::Stop,
                task: None,
            });
        }
    }
}

/// Escalations for switches given up: the worker stays live on the other
/// profile and the task waits for it, so someone must act.
pub(crate) fn news(work: &Work, rows: &[Facts]) -> Vec<Escalation> {
    waiting(work, rows)
        .filter(|(task, facts)| state(task, facts) == State::GivenUp)
        .map(|(task, _)| Escalation {
            session_id: task.session_id.clone(),
            id: key(task),
            why: Why::DeliveryFailed,
            line: format!(
                "task #{} is still waiting: its worker could not be stopped to switch its model \
                 profile ({} Stops failed); stop the worker, or cancel the task and assign it again",
                task.id,
                RETRIES + 1
            ),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{RETRIES, fit, key, news};
    use crate::deliver::{TaskRow, Work, delivered, orders};
    use crate::dispatch::{Facts, Order, Sent, Why};
    use serde_json::json;
    use spawn_core::command::CommandAction;
    use spawn_core::event::{ModelProfile, SessionSeq};
    use spawn_core::mission::TaskState;
    use spawn_core::observation::{Ambiguity, Confidence, Observation, SemanticState};

    /// A cheap task for the resident `core`.
    fn cheap_task() -> TaskRow {
        let mut task: TaskRow = serde_json::from_value(json!({
            "id": 4, "mission_id": 1, "kind": "resident",
            "agent_id": "agent_resident_core", "session_id": "session_resident_core",
            "brief": "work", "state": "assigned", "resident": "core",
        }))
        .expect("a task row");
        task.model = ModelProfile::Cheap;
        task
    }

    fn work() -> Work {
        Work {
            tasks: vec![cheap_task()],
            ..Work::default()
        }
    }

    /// The worker live on the default profile, last judged idle at its
    /// prompt, a judgement a failed Stop's own records have made stale.
    fn worker() -> Facts {
        let mut facts: Facts = serde_json::from_value(json!({
            "session_id": "session_resident_core", "agent_id": "agent_resident_core",
            "live": true, "execution_id": "exec_3", "evidence_seq": 9,
        }))
        .expect("facts");
        let mut observation = Observation::basic(
            SemanticState::Waiting,
            None,
            SessionSeq(5),
            Confidence::new(0.9).expect("valid"),
            "rules",
        );
        observation.ambiguity = Some(Ambiguity::TurnStoppedIdle);
        facts.observation = Some(observation);
        facts
    }

    fn stop(id: i64, key: &str, finished: i64, failed: bool) -> Sent {
        Sent {
            id: Some(id),
            key: key.to_string(),
            text: None,
            created_at_ms: finished - 1,
            finished_at_ms: Some(finished),
            succeeded: Some(!failed),
            error: failed.then(|| "kill-session refused".to_string()),
        }
    }

    /// What the dispatcher plans for the task, as `decide_with` does.
    fn plan(work: &Work, rows: &[Facts], now_ms: i64) -> Vec<Order> {
        let mut planned = orders(work, rows, now_ms);
        fit(&mut planned, work, rows, now_ms);
        planned
    }

    #[test]
    fn a_failed_switch_stop_is_retried_under_a_new_key_only_once_due() {
        let work = work();
        let mut facts = worker();
        facts
            .dispatched
            .push(stop(10, "switch-model-4-0", 1_000, true));
        let rows = vec![facts.clone()];
        assert!(!facts.idle_at_prompt(), "the judgement is stale");
        assert!(plan(&work, &rows, 1_000 + 59_999).is_empty(), "not due yet");
        assert!(news(&work, &rows).is_empty(), "a retry is coming");
        assert!(delivered(&work, &rows).is_empty(), "still waiting");
        let retry = plan(&work, &rows, 1_000 + 60_000);
        assert_eq!(retry.len(), 1, "{retry:?}");
        assert_eq!(retry[0].key, "switch-model-4-0-retry-1");
        assert_eq!(retry[0].action, CommandAction::Stop);
        assert_eq!(retry[0].task, None, "a Stop delivers nothing");

        // Not to a worker judged busy since, nor while a command runs.
        let mut busy = facts.clone();
        if let Some(observation) = busy.observation.as_mut() {
            observation.state = SemanticState::Working;
            observation.ambiguity = None;
        }
        assert!(plan(&work, &[busy], 1_000 + 60_000).is_empty());
        let running = Facts {
            open_command: true,
            ..facts.clone()
        };
        assert!(plan(&work, &[running], 1_000 + 60_000).is_empty());

        // The wait doubles after the second failure.
        facts
            .dispatched
            .push(stop(11, "switch-model-4-0-retry-1", 70_000, true));
        let rows = vec![facts];
        assert!(plan(&work, &rows, 70_000 + 119_999).is_empty());
        assert_eq!(
            plan(&work, &rows, 70_000 + 120_000)[0].key,
            "switch-model-4-0-retry-2"
        );
    }

    #[test]
    fn no_stop_after_the_last_retry_and_the_supervisor_hears_of_it() {
        let work = work();
        let mut facts = worker();
        facts
            .dispatched
            .push(stop(10, "switch-model-4-0", 1_000, true));
        for retry in 1..=RETRIES {
            facts.dispatched.push(stop(
                10 + i64::try_from(retry).expect("small"),
                &format!("switch-model-4-0-retry-{retry}"),
                2_000,
                true,
            ));
        }
        const { assert!(RETRIES < 5, "at most 5 Stops in all") };
        let rows = vec![facts];
        assert!(
            plan(&work, &rows, i64::MAX / 2).is_empty(),
            "no Stop after the last retry"
        );
        assert!(delivered(&work, &rows).is_empty(), "never delivered");
        let items = news(&work, &rows);
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].why, Why::DeliveryFailed);
        assert_eq!(items[0].tag(), "session_resident_core switch-model-4-0");
        assert!(
            items[0].line.starts_with("task #4 ") && items[0].line.contains("could not be stopped"),
            "{}",
            items[0].line
        );
        assert_eq!(key(&work.tasks[0]), "switch-model-4-0");
    }

    #[test]
    fn a_switch_stop_that_succeeds_still_leads_to_the_resume_that_delivers() {
        let work = work();
        // First the Stop, under the first key, once the worker is idle.
        let mut idle = worker();
        idle.evidence_seq = Some(5);
        let first = plan(&work, &[idle], 0);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].key, "switch-model-4-0");
        assert_eq!(first[0].action, CommandAction::Stop);

        // A retry that succeeded: the worker is down, and the resume on the
        // task's profile delivers the task.
        let mut stopped = worker();
        stopped.live = false;
        stopped
            .dispatched
            .push(stop(10, "switch-model-4-0", 1_000, true));
        stopped
            .dispatched
            .push(stop(11, "switch-model-4-0-retry-1", 70_000, false));
        let rows = vec![stopped];
        let resume = plan(&work, &rows, 80_000);
        assert_eq!(resume.len(), 1, "{resume:?}");
        assert_eq!(resume[0].key, "deliver-4-0");
        assert_eq!(resume[0].task, Some(4));
        assert!(matches!(
            resume[0].action,
            CommandAction::Resume {
                model: Some(ModelProfile::Cheap),
                ..
            }
        ));
        assert!(news(&work, &rows).is_empty());
    }
}
