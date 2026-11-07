//! A worker waiting on its own background job is working, not idle
//! (docs/fleet-architecture.md §2.2, "Waiting on its own job").
//!
//! Claude Code wakes an agent when a job it started in the background (a
//! Bash call with `run_in_background`, or one moved there; a Monitor; an
//! Agent launched in the background, or resumed by a message) reports. The Bridge records the jobs from the transcript (`job.started`,
//! `job.ended`; an execution's end closes the ones still open), and the
//! rules settle a turn that stopped while one is open as WORKING
//! (`WaitReason::WaitingForBackgroundJob`), with no model call: no
//! attention, no idle item and no nudge for it ([`super::worker`]), and its
//! task's `activity` step is not stalled ([`super::stall`]).
//!
//! What may still need someone is a job that runs too long (a hung test, a
//! watch that never ends): a job open for its bound (`SPAWN_STALL_JOB_SECS`,
//! 7200 s; a Monitor expires by itself within an hour) or longer is
//! escalated once, about its worker, tagged `<session>
//! job-<execution>-<job>-<start record>` (an Agent resumed under its id is
//! a new run, with a start record and a bound of its own). Told once, it
//! stays told while the job is open,
//! and it is the only item about it: the task's `activity` step stays quiet
//! while the worker waits on the job. Its age is on the database's clock
//! (the facts' `age_ms`). It is an item, never an action: stopping the job
//! or the worker is the supervisor's decision.

use super::stall::took;
use super::{Escalation, Facts, Job, Why, is_worker};

/// What the worker of `facts` is doing while `job` runs.
fn worker_line(facts: &Facts) -> String {
    if facts.waits_on_own_job() {
        return "its worker waits on it at its prompt (judged WORKING by the rules); Claude \
                Code wakes it when the job reports"
            .to_string();
    }
    facts.current_observation().map_or_else(
        || "its worker has no judgement covering its latest fact".to_string(),
        |observation| {
            format!(
                "its worker is judged {} by {}",
                observation.state.key(),
                observation.judge
            )
        },
    )
}

/// One job, for a line: `bash b8w03og7k`.
fn named(job: &Job) -> String {
    format!("{} {}", job.kind, job.id)
}

/// The escalations for the background jobs in `rows` open for `bound_ms`
/// or longer: one per job, about its worker, as of the record that started
/// it.
pub(crate) fn items(rows: &[Facts], bound_ms: i64) -> Vec<Escalation> {
    let mut items = Vec::new();
    for facts in rows.iter().filter(|facts| is_worker(facts) && facts.live) {
        let execution = facts.execution_id.as_deref().unwrap_or("none");
        for job in facts.jobs.iter().filter(|job| job.age_ms >= bound_ms) {
            let others: Vec<String> = facts
                .jobs
                .iter()
                .filter(|other| other.id != job.id)
                .map(|other| format!("{} ({})", named(other), took(other.age_ms)))
                .collect();
            let others = if others.is_empty() {
                String::new()
            } else {
                format!("; also open: {}", others.join(", "))
            };
            items.push(Escalation {
                session_id: facts.session_id.clone(),
                id: format!("job-{execution}-{}-{}", job.id, job.event_id),
                why: Why::BackgroundJob,
                line: format!(
                    "a background job has run for {} (bound {}): {}, started by record {} in \
                     execution {execution}; {}{others}",
                    took(job.age_ms),
                    took(bound_ms),
                    named(job),
                    job.event_id,
                    worker_line(facts),
                ),
                since_ms: Some(job.started_at_ms),
            });
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use spawn_core::command::CommandAction;
    use spawn_core::event::{ModelProfile, SessionSeq};
    use spawn_core::observation::{Ambiguity, Confidence, Observation, SemanticState, WaitReason};

    use super::items;
    use crate::deliver::Work;
    use crate::dispatch::stall::{Bounds, Stalls};
    use crate::dispatch::{Escalation, Facts, Job, Plan, Sent, Why, decide_with, stall};

    const NOW: i64 = 50_000_000;
    const HOUR: i64 = 3_600_000;
    const SESSION: &str = "session_worker_gate";

    /// The rules' judgement of a stopped turn with a job open.
    fn on_its_job() -> Observation {
        Observation::basic(
            SemanticState::Working,
            Some(WaitReason::WaitingForBackgroundJob),
            SessionSeq(20),
            Confidence::new(1.0).expect("valid"),
            "rules",
        )
    }

    /// Jev's judgement of a stopped turn with no job open: idle and fine.
    fn idle() -> Observation {
        let mut observation = Observation::basic(
            SemanticState::Waiting,
            None,
            SessionSeq(20),
            Confidence::new(0.9).expect("valid"),
            "jev",
        );
        observation.ambiguity = Some(Ambiguity::TurnStoppedIdle);
        observation.needs_human = Some(0.05);
        observation.stuck = Some(0.05);
        observation
    }

    fn job(id: &str, kind: &str, age_ms: i64) -> Job {
        Job {
            id: id.to_string(),
            kind: kind.to_string(),
            event_id: 700,
            started_at_ms: NOW - age_ms,
            age_ms,
        }
    }

    /// A live ephemeral worker at its prompt, judged on its latest fact.
    fn worker(observation: Observation, jobs: Vec<Job>) -> Facts {
        Facts {
            session_id: SESSION.to_string(),
            agent_id: "agent_worker_gate".to_string(),
            live: true,
            execution_id: Some("exec_5".to_string()),
            end_cause: None,
            observation: Some(observation),
            observation_event_id: Some(900),
            observation_at_ms: Some(NOW - 1_000),
            evidence_seq: Some(20),
            turn_ended: true,
            checkpoint: None,
            episode: Some(7),
            open_command: false,
            dispatched: Vec::new(),
            told: BTreeSet::new(),
            last_delivery: None,
            model: ModelProfile::Default,
            resumed_young: 0,
            stops: Vec::new(),
            charter: None,
            jobs,
        }
    }

    fn supervisor(told: &[String]) -> Facts {
        Facts {
            session_id: "session_supervisor".to_string(),
            agent_id: "agent_supervisor".to_string(),
            told: told.iter().cloned().collect(),
            ..worker(idle(), Vec::new())
        }
    }

    /// Task 7, delivered to the worker 3 h ago, its latest fact 20 min old.
    fn delivered() -> (Work, Stalls) {
        let work = Work {
            tasks: serde_json::from_value(serde_json::json!([{
                "id": 7, "mission_id": 1, "kind": "ephemeral",
                "agent_id": "agent_worker_gate", "session_id": SESSION,
                "brief": "run the gate", "state": "delivered", "revisions": 0,
                "mission_text": "m",
            }]))
            .expect("task rows"),
            ..Work::default()
        };
        let stalls: Stalls = serde_json::from_value(serde_json::json!({
            "now_ms": NOW,
            "tasks": [{"id": 7, "mission_id": 1, "kind": "ephemeral", "session_id": SESSION,
                       "state": "delivered", "revisions": 0, "gated": false, "accept": "review",
                       "halted": false,
                       "record": {"id": 140, "type": "task.delivered", "at_ms": NOW - 3 * HOUR}}],
            "sessions": [{"session_id": SESSION, "seq": 20, "event_name": "turn.stopped",
                          "at_ms": NOW - 1_200_000}],
        }))
        .expect("stall facts");
        (work, stalls)
    }

    /// The whole pass, as `read_plan` makes it: stalls and job items, then
    /// the ladder and the digest.
    fn pass(rows: &[Facts], bounds: &Bounds) -> Plan {
        let (work, stalls) = delivered();
        let mut host = stall::items(rows, &work, &stalls, bounds);
        host.extend(items(rows, bounds.job));
        decide_with(rows, &work, &host, true, NOW, "spawn")
    }

    fn digest(plan: &Plan) -> Option<(&String, &Vec<String>)> {
        plan.orders
            .iter()
            .find(|order| order.session_id == "session_supervisor")
            .map(|order| match &order.action {
                CommandAction::Input { text, tags } => (text, tags),
                other => panic!("a digest is typed input, not {other:?}"),
            })
    }

    #[test]
    fn a_worker_on_its_own_job_is_neither_escalated_nudged_nor_stalled() {
        let rows = [
            worker(on_its_job(), vec![job("b8w03og7k", "bash", 20 * 60_000)]),
            supervisor(&[]),
        ];
        let plan = pass(&rows, &Bounds::default());
        assert!(plan.pending.is_empty(), "{:?}", plan.pending);
        assert!(
            plan.orders.is_empty(),
            "no nudge, no digest: {:?}",
            plan.orders
        );
        assert!(rows[0].turn_stopped_idle(), "at its prompt: input is safe");
        assert!(!rows[0].idle_at_prompt(), "but not idle: no new work");
        assert!(rows[0].waits_on_own_job());
    }

    #[test]
    fn a_job_past_its_bound_is_escalated_once_with_its_kind_and_id() {
        let long = job("bys3gpa46", "monitor", 2 * HOUR + 5 * 60_000);
        let rows = [
            worker(
                on_its_job(),
                vec![long.clone(), job("b0uk1xi12", "bash", 60_000)],
            ),
            supervisor(&[]),
        ];
        let plan = pass(&rows, &Bounds::default());
        let pending: Vec<&Escalation> = plan.pending.iter().collect();
        assert_eq!(
            pending.len(),
            1,
            "one item, the job's, no stall: {pending:?}"
        );
        let item = pending[0];
        assert_eq!(item.why, Why::BackgroundJob);
        assert_eq!(item.tag(), format!("{SESSION} job-exec_5-bys3gpa46-700"));
        assert_eq!(item.since_ms, Some(long.started_at_ms));
        for words in [
            "a background job has run for 2h05m (bound 2h00m)",
            "monitor bys3gpa46",
            "record 700",
            "execution exec_5",
            "waits on it at its prompt",
            "also open: bash b0uk1xi12 (1m00s)",
        ] {
            assert!(item.line.contains(words), "{words}: {}", item.line);
        }
        let (text, tags) = digest(&plan).expect("the supervisor is told");
        assert_eq!(tags, &[item.tag()]);
        assert!(
            text.contains(&format!(
                "- {SESSION} — background job past its bound: a background job has run for"
            )),
            "{text}"
        );
        // Told: never sent again while the job runs, kept as told.
        let told = [rows[0].clone(), supervisor(tags)];
        let plan = pass(&told, &Bounds::default());
        assert!(plan.pending.is_empty(), "{:?}", plan.pending);
        assert!(digest(&plan).is_none());
        assert_eq!(plan.told.len(), 1);
        assert_eq!(
            plan.told[0].tag(),
            format!("{SESSION} job-exec_5-bys3gpa46-700")
        );
    }

    #[test]
    fn a_background_agent_resumed_under_its_id_has_a_bound_of_its_own() {
        // The first run passed its bound and was told.
        let first = job("a2adacaabc9f9b048", "agent", 2 * HOUR + 60_000);
        let rows = [worker(on_its_job(), vec![first]), supervisor(&[])];
        let plan = pass(&rows, &Bounds::default());
        let (_, tags) = digest(&plan).expect("told");
        assert_eq!(
            tags,
            &[format!("{SESSION} job-exec_5-a2adacaabc9f9b048-700")]
        );
        assert!(plan.pending[0].line.contains("agent a2adacaabc9f9b048"));
        // It ended, a message resumed it (a new start record), and this run
        // passed the bound too: a new item, not one already told.
        let mut again = job("a2adacaabc9f9b048", "agent", 2 * HOUR + 60_000);
        again.event_id = 960;
        let rows = [worker(on_its_job(), vec![again]), supervisor(tags)];
        let plan = pass(&rows, &Bounds::default());
        let (_, tags) = digest(&plan).expect("told again, about the new run");
        assert_eq!(
            tags,
            &[format!("{SESSION} job-exec_5-a2adacaabc9f9b048-960")]
        );
    }

    #[test]
    fn the_bound_is_a_setting_and_its_edge_counts() {
        let (bounds, warnings) =
            Bounds::read(|name| (name == "SPAWN_STALL_JOB_SECS").then(|| "600".to_string()));
        assert!(warnings.is_empty());
        assert_eq!(bounds.job, 600_000);
        assert_eq!(Bounds::default().job, 2 * HOUR, "two hours by default");
        let at = |age_ms: i64| [worker(on_its_job(), vec![job("b1", "bash", age_ms)])];
        assert!(items(&at(599_999), bounds.job).is_empty());
        assert_eq!(items(&at(600_000), bounds.job).len(), 1);
        // A worker that is not live has no open job; neither has the
        // supervisor's own session an item.
        let mut gone = at(3 * HOUR);
        gone[0].live = false;
        assert!(items(&gone, bounds.job).is_empty());
        let mut own = supervisor(&[]);
        own.jobs = vec![job("b2", "bash", 3 * HOUR)];
        assert!(items(&[own], bounds.job).is_empty());
    }

    #[test]
    fn with_no_job_open_a_stopped_worker_is_handled_as_before() {
        // Jev's idle and fine: the one nudge; quiet for 20 min, its task's
        // activity has stalled.
        let rows = [worker(idle(), Vec::new()), supervisor(&[])];
        let plan = pass(&rows, &Bounds::default());
        assert!(
            plan.orders
                .iter()
                .any(|order| order.key == "dispatch-nudge-session_worker_gate-7"),
            "{:?}",
            plan.orders
        );
        let whys: Vec<Why> = plan.pending.iter().map(|item| item.why).collect();
        assert_eq!(whys, [Why::Stalled], "{:?}", plan.pending);
        assert!(plan.pending[0].id.starts_with("stall-7-activity-"));
        // Nudged already: idle again escalates, and its activity stalled.
        let mut nudged = worker(idle(), Vec::new());
        nudged.dispatched.push(Sent {
            id: Some(1),
            key: "dispatch-nudge-session_worker_gate-7".to_string(),
            text: Some("continue".to_string()),
            created_at_ms: NOW - 1_300_000,
            finished_at_ms: Some(NOW - 1_299_000),
            succeeded: Some(true),
            error: None,
        });
        let rows = [nudged, supervisor(&[])];
        let plan = pass(&rows, &Bounds::default());
        let whys: Vec<Why> = plan.pending.iter().map(|item| item.why).collect();
        assert_eq!(whys, [Why::NudgeFailed, Why::Stalled], "{:?}", plan.pending);
        // Jev unreachable on the same facts: attention, as before.
        let mut unsure = idle();
        unsure.judge = "none".to_string();
        unsure.reason = Some(WaitReason::JudgeUnavailable);
        unsure.attention = true;
        unsure.attention_reason = Some("judge_unavailable".to_string());
        let rows = [worker(unsure, Vec::new()), supervisor(&[])];
        let plan = pass(&rows, &Bounds::default());
        assert!(
            plan.pending.iter().any(|item| item.why == Why::Attention),
            "{:?}",
            plan.pending
        );
    }

    #[test]
    fn a_job_open_while_the_worker_is_judged_otherwise_is_not_its_wait() {
        // The judgement is older than the latest fact (a new turn began):
        // nothing is decided on it, and the job alone holds no stall back
        // unless the worker's turn has ended.
        let mut busy = worker(on_its_job(), vec![job("b8w03og7k", "bash", 20 * 60_000)]);
        busy.evidence_seq = Some(21);
        assert!(!busy.waits_on_own_job());
        busy.turn_ended = false;
        let rows = [busy, supervisor(&[])];
        let plan = pass(&rows, &Bounds::default());
        let whys: Vec<Why> = plan.pending.iter().map(|item| item.why).collect();
        assert_eq!(whys, [Why::Stalled], "{:?}", plan.pending);
    }
}
