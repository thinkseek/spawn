//! Dispatcher: the control layer's deterministic policy for workers.
//!
//! The Observer says what state a worker is in; this decides what to do
//! about it, on the decision ladder of docs/fleet-architecture.md §3:
//!
//! 1. Rules over facts decide fixed cases: a lost execution is resumed once,
//!    and a session whose resume failed is escalated, never resumed again by
//!    rule (the one-resume rule, `worker::resume_lost`).
//! 2. One guarded nudge: when Jev judged that a worker stopped at an idle
//!    prompt without needing anyone (WAITING, confident, `needs_human` and
//!    `stuck` low), it gets `continue` typed in, once per episode and at most
//!    [`NUDGES_PER_HOUR`](worker::NUDGES_PER_HOUR) times per agent-hour.
//!    Jev's judgement is an input, never the chooser of the action, and the
//!    action is reversible.
//! 3. Everything else escalates: attention, a nudge that did not help (or
//!    could not be typed), a resume that failed, and completions (so work
//!    gets reviewed).
//!
//! A Stop that failed, whoever sent it, is sent again and then escalated
//! ([`stops`]): until then the worker gets no task, no nudge.
//!
//! A changed charter reaches the running supervisor ([`charter`]): when the
//! charter its native session holds (the hash its start recorded, or the
//! one a later charter input typed) is not the one this build writes, the
//! new one is typed in at its next idle prompt, once, in place of that
//! pass's digest.
//!
//! A worker waiting on a background job of its own is working, not idle
//! ([`jobs`]): the rules settle its stopped turn as WORKING, so it is not
//! nudged nor escalated, and its task is not stalled; a job open past its
//! bound (`SPAWN_STALL_JOB_SECS`) is escalated once.
//!
//! No task waits silently ([`stall`]): every open task is at one step
//! (`delivery`, `activity`, `judgement`, `landing`, `stop`, or its mission
//! `halted`), measured from the record that started it on the database's
//! clock, and a step that has not moved within its bound
//! (`SPAWN_STALL_*_SECS`) is escalated once per stall, tagged
//! `<session> stall-<task>-<step>-<n>` with `n` naming that record. That
//! layer acts on nothing; the recoveries Spawn has run on their own
//! schedules, and the item says what was tried and what is still due.
//!
//! Escalations are coalesced into one digest typed into the supervisor's
//! pane while it is idle. Without a supervisor they stay in the attention
//! queue.
//!
//! The dispatcher keeps no state. What it already did is read back from the
//! command queue: idempotency keys and their commands' outcomes mark
//! nudges, resumes and stops, and the tags a
//! delivered digest files on its command (one per escalation, never in its
//! text) mark what the supervisor was told.
//! It commands only `agent_worker_*` sessions, plus digests to the supervisor.
//!
//! Layout: this file holds the shared vocabulary (facts, orders,
//! escalations) and the step; [`worker`] is the per-worker ladder and
//! [`digest`] the supervisor digest. Task delivery (the mission layer's
//! rules) lives in [`crate::deliver`]; this module combines both into one
//! [`Plan`] ([`decide_with`]), reads it from the store ([`read_plan`], which
//! the UI's attention queue calls too: `GET /api/attention`) and submits it
//! ([`step`]).

mod charter;
mod digest;
mod jobs;
#[cfg(test)]
mod resume_replay;
mod stall;
mod stops;
mod worker;

pub(crate) use digest::{OPENING as DIGEST_OPENING, heading};
#[cfg(test)]
pub(crate) use jobs::items as job_items;
pub(crate) use stall::Bounds;
pub(crate) use worker::reached_with;

use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;
use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::ModelProfile;
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId};
use spawn_core::observation::{Ambiguity, Observation, SemanticState};
use spawn_core::port::CommandQueue;
use spawn_postgres::PostgresStore;

use crate::deliver::{self, Work};
use crate::supervise::{Charter, SUPERVISOR_SESSION};

/// One command the dispatcher sent.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct Sent {
    /// The command's id.
    #[serde(default)]
    pub(crate) id: Option<i64>,
    pub(crate) key: String,
    #[serde(default)]
    pub(crate) text: Option<String>,
    pub(crate) created_at_ms: i64,
    #[serde(default)]
    pub(crate) finished_at_ms: Option<i64>,
    #[serde(default)]
    pub(crate) succeeded: Option<bool>,
    /// Why it failed, when it did.
    #[serde(default)]
    pub(crate) error: Option<String>,
}

/// A Stop or Discard sent into a session during its current execution, by
/// anyone: one that failed is pursued and escalated ([`stops`]).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct StopSent {
    pub(crate) id: i64,
    pub(crate) key: String,
    pub(crate) principal: String,
    /// `stop` or `discard`.
    pub(crate) action: String,
    pub(crate) created_at_ms: i64,
    #[serde(default)]
    pub(crate) finished_at_ms: Option<i64>,
    #[serde(default)]
    pub(crate) succeeded: Option<bool>,
    #[serde(default)]
    pub(crate) error: Option<String>,
}

/// A background job the session's live execution started and that has not
/// ended (`job.started` without its `job.ended`), as `dispatch_facts`
/// reads it. The execution's end closes it, so only a live one has any.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct Job {
    /// The engine's task id.
    pub(crate) id: String,
    /// `bash`, `monitor` or `agent`.
    pub(crate) kind: String,
    /// The `job.started` record's id.
    pub(crate) event_id: i64,
    /// When it started (the record's time).
    pub(crate) started_at_ms: i64,
    /// How long it has run when the facts were read, on the database's
    /// clock.
    pub(crate) age_ms: i64,
}

/// A resume that started an execution which ended lost before its engine
/// became ready ([`Facts::failed_resume`]), with what the log says of it.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct FailedResume {
    /// The execution the resume started.
    pub(crate) execution_id: String,
    /// The resume command.
    #[serde(default)]
    pub(crate) command_id: Option<i64>,
    #[serde(default)]
    pub(crate) ended_at_ms: Option<i64>,
    /// The `evidence` of the execution's end: for a start abandoned at its
    /// ready wait, `its start failed: pane never became ready, last state
    /// <state>…`.
    #[serde(default)]
    pub(crate) evidence: Option<String>,
    /// The resume command's error.
    #[serde(default)]
    pub(crate) error: Option<String>,
}

impl FailedResume {
    /// What the log says of how it failed: the end's evidence, else the
    /// command's error.
    pub(crate) fn why(&self) -> &str {
        self.evidence
            .as_deref()
            .or(self.error.as_deref())
            .unwrap_or("no evidence recorded")
    }
}

/// Facts about one session, as `dispatch_facts` returns them.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Facts {
    pub(crate) session_id: String,
    pub(crate) agent_id: String,
    #[serde(default)]
    pub(crate) live: bool,
    #[serde(default)]
    pub(crate) execution_id: Option<String>,
    #[serde(default)]
    pub(crate) end_cause: Option<String>,
    #[serde(default)]
    pub(crate) observation: Option<Observation>,
    #[serde(default)]
    pub(crate) observation_event_id: Option<i64>,
    #[serde(default)]
    pub(crate) observation_at_ms: Option<i64>,
    /// Session position of the latest fact about the engine: not an
    /// observation, nor a storage record (`artifact.committed`,
    /// `merge.conflicted`), which says nothing about the engine's state and
    /// may be written between turns by the integrator or the dispatcher
    /// ([`crate::main_merge`]); a judgement older than one still holds.
    #[serde(default)]
    pub(crate) evidence_seq: Option<u64>,
    /// The live execution's latest turn event in the log is a turn's end
    /// (`turn.stopped` or `turn.failed`), not a newer `message.received`.
    /// The Bridge records a turn's checkpoint before its end.
    #[serde(default)]
    pub(crate) turn_ended: bool,
    /// The commit of the agent's latest `artifact.committed`, read with the
    /// rest of these facts: once `turn_ended`, the checkpoint of the turn
    /// that ended (or a later one).
    #[serde(default)]
    pub(crate) checkpoint: Option<String>,
    #[serde(default)]
    pub(crate) episode: Option<i64>,
    #[serde(default)]
    pub(crate) open_command: bool,
    #[serde(default)]
    pub(crate) dispatched: Vec<Sent>,
    /// The tags of every input the dispatcher delivered into the session,
    /// however long ago: for the supervisor, the escalations its digests
    /// carried ([`Escalation::tag`]).
    #[serde(default)]
    pub(crate) told: BTreeSet<String>,
    /// Key of the latest delivery (`deliver-<task>-<revision>`, or one of
    /// its retries) the dispatcher sent into the session that reached the
    /// worker or may have, however long ago. One refused before anything was
    /// typed is not the worker's: it told the worker nothing.
    #[serde(default)]
    pub(crate) last_delivery: Option<String>,
    /// The model profile of the session's latest execution.
    #[serde(default)]
    pub(crate) model: ModelProfile,
    /// Executions the dispatcher's resumes started that were lost (or
    /// exited non-zero) within a minute, since the session's last execution
    /// that stayed up.
    #[serde(default)]
    pub(crate) resumed_young: u32,
    /// The latest execution one of the dispatcher's resumes started that
    /// ended lost before its engine became ready (whatever its command's
    /// outcome), since the session's last execution that stayed up: a
    /// failed resume, read from the log rather than from the key of the
    /// session's latest execution, which no resume was sent under.
    #[serde(default)]
    pub(crate) failed_resume: Option<FailedResume>,
    /// Every Stop and Discard sent into the session since its live
    /// execution started (none when it is not live), oldest first.
    #[serde(default)]
    pub(crate) stops: Vec<StopSent>,
    /// For the supervisor: the records of the charter its current native
    /// session holds ([`charter::Records`]); `None` for every other session.
    #[serde(default)]
    pub(crate) charter: Option<charter::Records>,
    /// The live execution's open background jobs, oldest first.
    #[serde(default)]
    pub(crate) jobs: Vec<Job>,
}

/// Whether a judgement says the turn stopped at the prompt: the rules left
/// it to the judge (`TurnStoppedIdle`), or settled it as waiting on a
/// background job of its own. Either way the pane waits at its prompt.
fn stopped_at_prompt(observation: &Observation) -> bool {
    observation.ambiguity == Some(Ambiguity::TurnStoppedIdle) || observation.waits_on_own_job()
}

impl Facts {
    /// The command the dispatcher sent under `key`, if any.
    pub(crate) fn sent(&self, key: &str) -> Option<&Sent> {
        self.dispatched.iter().find(|sent| sent.key == key)
    }

    /// The latest judgement, unless it is older than the latest fact.
    ///
    /// A judgement older than the latest fact describes a moment that has
    /// passed (for example the ready screen just before the prompt was
    /// typed). The Observer judges again, and once the session's facts
    /// settle its latest observation covers its latest fact, whether or not
    /// the verdict changed (the coverage rule in `spawn_core::observe`): an
    /// idle worker whose last fact changed nothing still gets a current
    /// judgement, a few seconds later.
    pub(crate) fn current_observation(&self) -> Option<&Observation> {
        self.observation.as_ref().filter(|observation| {
            self.evidence_seq
                .is_none_or(|latest| observation.based_on_seq.0 >= latest)
        })
    }

    /// Live, and someone has asked for its execution to end: no task is
    /// typed into it, and its stop is pursued ([`stops`]).
    pub(crate) fn stopping(&self) -> bool {
        self.live && !self.stops.is_empty()
    }

    /// Live, with no command in flight.
    pub(crate) fn live_and_free(&self) -> bool {
        self.live && !self.open_command
    }

    /// Whether the worker sits at its prompt, judged on its latest fact,
    /// with nothing in flight: the only moment typed input is safe. The
    /// turn stopped and the pane waits at its prompt, whatever Jev made of
    /// it: a fact the rules settle. A report written by then is the
    /// worker's answer even when it asks a question (judged BLOCKED). A
    /// worker waiting on its own background job sits at its prompt too
    /// (judged WORKING): input is safe, though it is not idle.
    pub(crate) fn turn_stopped_idle(&self) -> bool {
        self.live_and_free() && self.current_observation().is_some_and(stopped_at_prompt)
    }

    /// Whether the rules judged the worker, on its latest fact, at its
    /// prompt waiting on a background job of its own: working, not idle.
    pub(crate) fn waits_on_own_job(&self) -> bool {
        self.live
            && self
                .current_observation()
                .is_some_and(Observation::waits_on_own_job)
    }

    /// Whether a report written in the worker's workspace may be collected
    /// now: the turn that wrote it has ended in the log (the transcript's
    /// end of the turn, not only a ready screen), and so its checkpoint has
    /// been taken ([`Self::checkpoint`]); the worker is live with no command
    /// open; and its latest judgement is [`Ambiguity::TurnStoppedIdle`].
    ///
    /// A judgement the Observer writes again only when its verdict changes
    /// (one with no model answer: no model judge, or the judge unavailable)
    /// need not be current: a turn's end read after the screen showed the
    /// prompt leaves the idle judgement older than the latest fact, and
    /// nobody judges again. The log's own end of the turn is newer evidence
    /// than the judgement, and is what the collection rests on; a turn that
    /// started since (`message.received`) or a verdict that changed holds it
    /// back. A model's answer (it carries a fingerprint) is written for every
    /// new situation, so a stale one waits for the model's judgement of the
    /// turn, as before.
    pub(crate) fn report_due(&self) -> bool {
        let idle = stopped_at_prompt;
        self.live_and_free()
            && self.turn_ended
            && match self.current_observation() {
                Some(current) => idle(current),
                None => self
                    .observation
                    .as_ref()
                    .is_some_and(|latest| latest.fingerprint.is_none() && idle(latest)),
            }
    }

    pub(crate) fn idle_at_prompt(&self) -> bool {
        self.live_and_free()
            && self.current_observation().is_some_and(|observation| {
                observation.ambiguity == Some(Ambiguity::TurnStoppedIdle)
                    && matches!(
                        observation.state,
                        SemanticState::Waiting | SemanticState::Completed
                    )
            })
    }
}

/// Why a worker is escalated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Why {
    /// The observation asks for attention.
    Attention,
    /// A nudge was sent this episode and the worker stopped again.
    NudgeFailed,
    /// The execution was lost and the resume failed.
    ResumeFailed,
    /// The worker finished; someone should review the result.
    Completed,
    /// The worker stopped at its prompt and nothing allowed a nudge: it
    /// waits for its next instruction.
    Idle,
    /// A new mission is in the inbox.
    Mission,
    /// A worker reported on its task.
    Report,
    /// A human approved a resident.
    Approved,
    /// A human wrote to a worker directly.
    Human,
    /// The command that delivered a task failed.
    DeliveryFailed,
    /// A Stop of a live execution kept failing.
    StopFailed,
    /// Free disk on this host fell under the warning threshold, and is not
    /// back yet (crate::disk).
    Disk,
    /// Free disk on this host is back above the warning threshold
    /// (crate::disk): news, see [`Why::is_news`].
    DiskRecovered,
    /// A mission's gate has had no room in TMPDIR for hours, and its run of
    /// `no_room` goes on ([`crate::disk::gate_room_escalations`]).
    GateRoom,
    /// A mission's gate failed twice in a row because the host did (the
    /// test database), not the work ([`digest::host_fault_escalations`]).
    GateHostFault,
    /// An open task's step has not moved within its bound ([`stall`]).
    Stalled,
    /// The deployed charter could not be typed into the supervisor's pane:
    /// every attempt was refused before typing, and the retries are spent
    /// ([`charter`]).
    Charter,
    /// A worker's background job has run longer than its bound ([`jobs`]).
    BackgroundJob,
    /// An owner's hold on a mission source reached its bound and the
    /// integrator ended it by record ([`digest::hold_expiries`]): landings
    /// there resumed, and an owner verdict judged against its main may be
    /// stale.
    HoldExpired,
    /// An objective ran out of attempts and its task yielded the slot to
    /// the next objective ([`digest::objective_yields`]).
    ObjectiveYielded,
}

impl Why {
    /// Whether an item of this kind is news rather than a condition: told
    /// once, it leaves nothing to resolve. A disk recovery, an approval, a
    /// human's message, a pair of host faults at the gate (the integrator
    /// judges again on its own) and an expired hold (its landings resumed
    /// on their own) are news. Every other item is a
    /// condition that holds while its facts do (a worker's state, a stall,
    /// a path still low on disk, a gate still without room): told, it stays
    /// in [`Plan::told`] until its facts change.
    pub(crate) fn is_news(self) -> bool {
        matches!(
            self,
            Self::DiskRecovered
                | Self::Approved
                | Self::Human
                | Self::GateHostFault
                | Self::HoldExpired
        )
    }
}

/// One escalation, identified so a digest can mark it delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Escalation {
    pub(crate) session_id: String,
    pub(crate) id: String,
    pub(crate) why: Why,
    pub(crate) line: String,
    /// When the record it rests on was written (the judgement of a
    /// worker's state, the record a stall is measured from, a disk or gate
    /// record), on the database's clock; `None` when the rows carry no
    /// time. Only for showing its age (`GET /api/attention`): never in its
    /// tag or a digest's text.
    pub(crate) since_ms: Option<i64>,
}

impl Escalation {
    /// An escalation about the session `facts` describe, as of its latest
    /// judgement.
    pub(crate) fn about(facts: &Facts, id: String, why: Why, line: String) -> Self {
        Self {
            session_id: facts.session_id.clone(),
            id,
            why,
            line,
            since_ms: facts.observation_at_ms,
        }
    }

    /// The tag a digest files for this escalation on its command (never in
    /// its text): a delivered digest carrying it means the supervisor was
    /// told.
    pub(crate) fn tag(&self) -> String {
        format!("{} {}", self.session_id, self.id)
    }
}

/// One command to submit.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Order {
    pub(crate) agent_id: String,
    pub(crate) session_id: String,
    pub(crate) key: String,
    pub(crate) action: CommandAction,
    /// The task this command delivers, if any: it is marked delivered once
    /// the command has succeeded ([`Plan::delivered`]).
    pub(crate) task: Option<i64>,
}

impl Order {
    /// A command to the session `facts` describe, delivering no task.
    pub(crate) fn to(facts: &Facts, key: String, action: CommandAction) -> Self {
        Self {
            agent_id: facts.agent_id.clone(),
            session_id: facts.session_id.clone(),
            key,
            action,
            task: None,
        }
    }
}

/// What one dispatcher step does.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Plan {
    /// Tasks whose delivery command succeeded, as (task, command): they
    /// are marked delivered.
    pub(crate) delivered: Vec<(i64, i64)>,
    pub(crate) orders: Vec<Order>,
    /// Escalations not delivered yet, whether or not a digest goes out now.
    pub(crate) pending: Vec<Escalation>,
    /// Escalations already delivered to the supervisor (their tag is in its
    /// told set) whose condition still holds: left out of `pending`, so
    /// never sent again. News ([`Why::is_news`]) is not among them: told,
    /// it is resolved. The attention view shows these as unresolved.
    pub(crate) told: Vec<Escalation>,
}

/// Principal the dispatcher submits as.
pub(crate) const DISPATCHER: &str = "spawn-dispatch";

/// Whether the dispatcher's worker ladder applies to this agent.
fn is_worker(facts: &Facts) -> bool {
    facts.agent_id.starts_with("agent_worker_") || facts.agent_id.starts_with("agent_resident_")
}

/// The whole policy as a pure function of the facts, without missions.
#[cfg(test)]
pub(crate) fn decide(rows: &[Facts], supervised: bool, now_ms: i64, spawn: &str) -> Plan {
    decide_with(rows, &Work::default(), &[], supervised, now_ms, spawn)
}

/// The whole policy as a pure function of the facts, the mission rows and
/// the host's news (disk records): delivery orders first, then the worker
/// ladder (a worker whose stop is under way gets only its pursuit,
/// [`stops`]), then at most one digest.
pub(crate) fn decide_with(
    rows: &[Facts],
    work: &Work,
    host: &[Escalation],
    supervised: bool,
    now_ms: i64,
    spawn: &str,
) -> Plan {
    let supervisor = rows
        .iter()
        .find(|facts| facts.session_id == SUPERVISOR_SESSION);
    let told = digest::told(supervisor);
    let mut plan = Plan {
        delivered: deliver::delivered(work, rows),
        orders: deliver::orders(work, rows, now_ms),
        ..Plan::default()
    };
    let in_flight = work.in_flight();
    for facts in rows.iter().filter(|facts| is_worker(facts)) {
        let decision = if facts.stopping() {
            // Whoever asked for the stop, and whatever the worker's task.
            stops::pursue(facts, now_ms)
        } else if work.managed(&facts.session_id) && !in_flight.contains(facts.session_id.as_str())
        {
            // A managed worker between tasks is not news: its report
            // already was, and the supervisor decides what comes next.
            continue;
        } else {
            worker::decide_worker(facts, now_ms)
        };
        match decision {
            worker::Decision::Nothing => {}
            worker::Decision::Act(order) => plan.orders.push(order),
            worker::Decision::Escalate(escalation) => plan.pending.push(escalation),
        }
    }
    plan.pending.extend(deliver::news_items(work, rows));
    plan.pending
        .extend(supervisor.and_then(charter::escalation));
    plan.pending.extend_from_slice(host);
    (plan.told, plan.pending) = std::mem::take(&mut plan.pending)
        .into_iter()
        .partition(|item| told.contains(&item.tag()));
    // Every item here holds now (each source gives only what its facts
    // still say); told news was resolved by the telling.
    plan.told.retain(|item| !item.why.is_news());
    if let Some(supervisor) = supervisor.filter(|_| supervised) {
        plan.orders
            .extend(digest::order(supervisor, &plan.pending, spawn));
    }
    plan
}

/// Facts and mission rows, read once.
pub(crate) fn load(store: &PostgresStore) -> Result<(Vec<Facts>, Work), String> {
    let rows: Vec<Facts> =
        serde_json::from_value(store.dispatch_facts().map_err(|error| error.to_string())?)
            .map_err(|error| format!("dispatch facts: {error}"))?;
    Ok((rows, Work::load(store)?))
}

/// What stalls are measured from, read after the facts and rows: a record
/// newer than them only makes a step look younger.
fn load_stalls(store: &PostgresStore) -> Result<stall::Stalls, String> {
    serde_json::from_value(store.stall_facts().map_err(|error| error.to_string())?)
        .map_err(|error| format!("stall facts: {error}"))
}

fn now_ms() -> i64 {
    spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0
}

/// Submit one order as the dispatcher. An order that delivers a task does
/// not move it: the task is marked delivered once the command succeeded.
fn submit(store: &PostgresStore, principal: &Principal, order: Order) -> Result<(), String> {
    let submission = Submission {
        agent_id: AgentId::new(order.agent_id).map_err(|error| error.to_string())?,
        session_id: SessionId::new(order.session_id).map_err(|error| error.to_string())?,
        idempotency_key: IdempotencyKey::new(order.key).map_err(|error| error.to_string())?,
        action: order.action,
    };
    store
        .submit(principal, &submission)
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// One pass's plan, read from the store before anything is submitted.
pub(crate) struct Reading {
    /// The facts the plan was made from.
    pub(crate) rows: Vec<Facts>,
    /// The mission rows the plan was made from.
    pub(crate) work: Work,
    pub(crate) plan: Plan,
    /// Log lines: what could not be read (a stall read that failed).
    pub(crate) lines: Vec<String>,
}

/// The plan for this moment, reading only: the host's news (the disk items
/// `disk` gives for the moment's clock, the gate-room and gate host-fault
/// records, the owner's holds that expired, the stalls past their `bounds`,
/// [`stall`], and the background
/// jobs past theirs, [`jobs`]) added to the facts and mission rows
/// `loaded` ([`load`]), then [`decide_with`], then the input that gives the
/// supervisor the `deployed` charter if it holds another ([`charter::give`];
/// `None`: no input, as for a reader that submits nothing). [`step`]
/// submits what it returns; `GET /api/attention` (crate::serve) shows it.
/// One function for both, so the queue a person reads is the dispatcher's
/// own.
pub(crate) fn read_plan(
    store: &PostgresStore,
    loaded: (Vec<Facts>, Work),
    disk: impl FnOnce(i64) -> Vec<Escalation>,
    bounds: &Bounds,
    supervised: bool,
    spawn: &str,
    deployed: Option<&Charter>,
) -> Result<Reading, String> {
    let (rows, work) = loaded;
    let mut lines = Vec::new();
    let now = now_ms();
    let mut host = disk(now);
    host.extend(crate::disk::gate_room_escalations(
        store
            .gate_room_escalations()
            .map_err(|error| format!("gate room records: {error}"))?
            .as_array()
            .map_or(&[][..], Vec::as_slice),
    ));
    host.extend(digest::host_fault_escalations(
        store
            .gate_host_fault_escalations()
            .map_err(|error| format!("gate host fault records: {error}"))?
            .as_array()
            .map_or(&[][..], Vec::as_slice),
    ));
    host.extend(digest::objective_yields(
        store
            .objective_yields()
            .map_err(|error| format!("objective yield records: {error}"))?
            .as_array()
            .map_or(&[][..], Vec::as_slice),
    ));
    host.extend(digest::hold_expiries(
        store
            .hold_expiries()
            .map_err(|error| format!("source hold records: {error}"))?
            .as_array()
            .map_or(&[][..], Vec::as_slice),
    ));
    // A stall read that failed costs this pass its stall items, never its
    // deliveries: the next pass measures the same stalls again.
    match load_stalls(store) {
        Ok(stalls) => host.extend(stall::items(&rows, &work, &stalls, bounds)),
        Err(error) => lines.push(format!("dispatch: stalls: {error}")),
    }
    host.extend(jobs::items(&rows, bounds.job));
    let mut plan = decide_with(&rows, &work, &host, supervised, now, spawn);
    if let Some(deployed) = deployed.filter(|_| supervised) {
        charter::give(&mut plan, &rows, deployed);
    }
    Ok(Reading {
        rows,
        work,
        plan,
        lines,
    })
}

/// Run one dispatcher step against the store: collect reports from idle
/// workers under `root` (a draft is not taken: its worker is told instead,
/// [`deliver::collect_reports`]), then submit the plan ([`read_plan`]), with the
/// stalls past their `bounds` among its escalations ([`stall`]), without
/// its new executions while the disk is under its floor ([`crate::disk`]).
/// Returns log lines.
pub(crate) fn step(
    store: &std::sync::Arc<PostgresStore>,
    supervised: bool,
    spawn: &str,
    root: Option<&Path>,
    disk: &crate::disk::Monitor,
    bounds: &Bounds,
) -> Result<Vec<String>, String> {
    let (mut rows, mut work) = load(store)?;
    let mut lines = root.map_or_else(Vec::new, |root| {
        deliver::collect_reports(store, &work, &rows, root, now_ms())
    });
    if !lines.is_empty() {
        // Collecting a report changed a task's state.
        (rows, work) = load(store)?;
    }
    let Reading {
        work,
        mut plan,
        lines: read,
        ..
    } = read_plan(
        store,
        (rows, work),
        |now| disk.escalations(now),
        bounds,
        supervised,
        spawn,
        Some(&Charter::deployed(spawn)),
    )?;
    lines.extend(read);
    if disk.below_floor() {
        let held = crate::disk::hold_new_executions(&mut plan.orders);
        if held > 0 {
            lines.push(format!(
                "dispatch: held {held} new execution(s): free disk is under SPAWN_DISK_FLOOR_GB"
            ));
        }
    }
    for (task, command) in plan.delivered {
        store
            .task_delivered(task, command)
            .map_err(|error| error.to_string())?;
        lines.push(format!(
            "dispatch: task #{task} delivered (command {command})"
        ));
    }
    let principal = Principal::new(DISPATCHER).map_err(|error| error.to_string())?;
    for mut order in plan.orders {
        // A cancelled task's work is set aside before its resident is told
        // ([`crate::set_aside`]); a set-aside that fails holds the notice,
        // and so the resident's next task, until a later pass.
        if let Some(task) = deliver::notice_task(&order.key) {
            match set_aside_before(store, root, &work, task, &mut order) {
                Ok(line) => lines.extend(line),
                Err(error) => {
                    lines.push(format!("deliver: {error}"));
                    continue;
                }
            }
        }
        // A task typed into a live worker starts from the source's main:
        // merged into its idle workspace first, and the brief says what the
        // merge did ([`crate::main_merge`]). A worker found busy at the
        // merge gets nothing this pass: the delivery is tried again on the
        // next, by the same rules.
        if let Some(root) = root {
            match merge_main_before(store, root, &mut order) {
                Ok(line) => lines.extend(line),
                Err(held) => {
                    lines.push(held);
                    continue;
                }
            }
        }
        let line = format!("dispatch: {} -> {}", order.key, order.session_id);
        // Before the command exists, so the worker's report for it comes
        // after the mark (the mark decides which report is new).
        if let Some(root) = root
            && let Err(error) = deliver::mark_delivery(root, &order)
        {
            lines.push(format!("deliver: {error}"));
        }
        submit(store, &principal, order)?;
        lines.push(line);
    }
    Ok(lines)
}

/// Set aside the work of the cancelled task `task` before `order`, its
/// notice, is sent, and add to the notice where the work went. Without a
/// workspace root there is no branch here to set aside: the notice goes as
/// it is. Returns a log line when the set-aside did something.
fn set_aside_before(
    store: &std::sync::Arc<PostgresStore>,
    root: Option<&Path>,
    work: &Work,
    task: i64,
    order: &mut Order,
) -> Result<Option<String>, String> {
    let Some(cancelled) = work.cancelled.iter().find(|cancelled| cancelled.id == task) else {
        return Err(format!(
            "task #{task}'s cancel notice: the task is not a cancelled delivery"
        ));
    };
    let Some(root) = root else {
        return Ok(None);
    };
    let done = crate::set_aside::set_aside(store, root, cancelled)?;
    let note = crate::set_aside::note(task, &done);
    if let CommandAction::Resume {
        prompt: Some(text), ..
    } = &mut order.action
        && !note.is_empty()
    {
        text.push_str("\n\n");
        text.push_str(&note);
    }
    Ok(match done {
        crate::set_aside::Done::SetAside { commit, kept } if cancelled.set_aside.is_none() => {
            Some(format!(
                "deliver: task #{task} cancelled: its work set aside in {kept}, the branch at {commit}"
            ))
        }
        _ => None,
    })
}

/// Merge main into the workspace of the live worker `order` types a task
/// into, and add what the merge did to the typed text. A start or resume
/// mounts its workspace later; the Bridge merges main then, when it merges
/// cleanly. Returns a log line when the merge did something, and an error
/// (the log line) when the order must not be sent this pass: the worker was
/// not idle when the merge looked again ([`crate::main_merge::Merge::Busy`]).
pub(crate) fn merge_main_before(
    store: &std::sync::Arc<PostgresStore>,
    root: &Path,
    order: &mut Order,
) -> Result<Option<String>, String> {
    let CommandAction::Input { text, .. } = &mut order.action else {
        return Ok(None);
    };
    let Some(task) = order.task else {
        return Ok(None);
    };
    let merge = crate::main_merge::merge_main(store, root, &order.agent_id, &order.session_id);
    if let crate::main_merge::Merge::Busy(why) = &merge {
        return Err(format!(
            "deliver: task #{task} waits: main is not merged into its workspace while {why}; \
             tried again next pass"
        ));
    }
    // What earlier merges changed and no typed delivery told yet, from
    // their records: this attempt's merge may find main already in.
    let untold = store
        .untold_merges(&order.session_id)
        .map_err(|error| eprintln!("deliver: untold merges of {}: {error}", order.session_id))
        .unwrap_or_default();
    if let Some(note) = crate::main_merge::delivery_note(&merge, &untold) {
        text.push_str("\n\n");
        text.push_str(&note);
    }
    Ok(match merge {
        crate::main_merge::Merge::Current | crate::main_merge::Merge::NotMounted => None,
        other => Some(format!(
            "deliver: task #{task}: main into its workspace: {other:?}"
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::{Escalation, Facts, FailedResume, Sent, Why, decide, decide_with};
    use crate::deliver::Work;
    use spawn_core::command::CommandAction;
    use spawn_core::event::{ModelProfile, SessionSeq};
    use spawn_core::observation::{Ambiguity, Confidence, Observation, SemanticState};

    const NOW: i64 = 10_000_000;

    fn judged(state: SemanticState, confidence: f64) -> Observation {
        let mut observation = Observation::basic(
            state,
            None,
            SessionSeq(9),
            Confidence::new(confidence).expect("valid"),
            "jev",
        );
        observation.ambiguity = Some(Ambiguity::TurnStoppedIdle);
        observation.needs_human = Some(0.05);
        observation.stuck = Some(0.1);
        observation
    }

    fn worker(observation: Observation) -> Facts {
        Facts {
            session_id: "session_worker_a".to_string(),
            agent_id: "agent_worker_a".to_string(),
            live: true,
            execution_id: Some("exec_5".to_string()),
            end_cause: None,
            observation: Some(observation),
            observation_event_id: Some(100),
            observation_at_ms: Some(NOW - 1_000),
            evidence_seq: Some(9),
            episode: Some(7),
            stops: Vec::new(),
            open_command: false,
            dispatched: Vec::new(),
            told: std::collections::BTreeSet::new(),
            last_delivery: None,
            model: ModelProfile::Default,
            resumed_young: 0,
            failed_resume: None,
            turn_ended: true,
            checkpoint: None,
            charter: None,
            jobs: Vec::new(),
        }
    }

    /// The whole pass for the live sequence of task 25 (a resident's
    /// delivery refused by a pane still busy at its turn's end, then the
    /// idle worker judged BLOCKED): the plan holds the retry that delivers
    /// task 25, and the worker is not escalated as blocked or idle meanwhile,
    /// since it is between tasks the mission layer manages.
    #[test]
    fn a_refused_delivery_is_retried_by_the_whole_pass_to_a_blocked_idle_worker() {
        let mut resident = worker(judged(SemanticState::Blocked, 0.9));
        resident.session_id = "session_resident_core".to_string();
        resident.agent_id = "agent_resident_core".to_string();
        let sent = |id: i64, key: &str, finished: i64, error: Option<&str>| Sent {
            id: Some(id),
            key: key.to_string(),
            text: None,
            created_at_ms: finished - 1,
            finished_at_ms: Some(finished),
            succeeded: Some(error.is_none()),
            error: error.map(str::to_string),
        };
        resident.dispatched = vec![
            sent(30, "deliver-24-0", NOW - 600_000, None),
            sent(
                31,
                "deliver-25-0",
                NOW - 10_000,
                Some("pane state Busy does not accept input"),
            ),
        ];
        resident.last_delivery = Some("deliver-24-0".to_string());
        let row = |id: i64, state: &str| {
            serde_json::json!({
                "id": id, "mission_id": 1, "kind": "resident",
                "agent_id": "agent_resident_core", "session_id": "session_resident_core",
                "brief": format!("brief {id}"), "state": state, "revisions": 0,
                "mission_text": "m", "resident": "core", "charter": "c",
            })
        };
        let work = Work {
            tasks: serde_json::from_value(serde_json::json!([
                row(24, "accepted"),
                row(25, "assigned")
            ]))
            .expect("task rows"),
            ..Work::default()
        };
        let plan = decide_with(&[resident], &work, &[], false, NOW, "spawn");
        assert!(plan.delivered.is_empty());
        assert_eq!(plan.orders.len(), 1, "{:?}", plan.orders);
        assert_eq!(plan.orders[0].key, "deliver-25-0-retry-1");
        assert_eq!(plan.orders[0].task, Some(25));
        assert!(matches!(plan.orders[0].action, CommandAction::Input { .. }));
        assert!(plan.pending.is_empty(), "{:?}", plan.pending);
    }

    /// The digest `order` reached the supervisor: the tags it filed are
    /// told, as the dispatcher's facts read them back.
    fn delivered(supervisor: &mut Facts, order: &super::Order) {
        let CommandAction::Input { tags, .. } = &order.action else {
            panic!("a digest is typed input");
        };
        supervisor.told.extend(tags.iter().cloned());
    }

    #[test]
    fn a_blocked_worker_at_its_prompt_has_its_report_collected_but_gets_no_new_work() {
        let blocked = worker(judged(SemanticState::Blocked, 0.9));
        assert!(blocked.turn_stopped_idle(), "its report is collected");
        assert!(!blocked.idle_at_prompt(), "but nothing new is delivered");
        let mut running = judged(SemanticState::Working, 0.9);
        running.ambiguity = None;
        assert!(
            !worker(running).turn_stopped_idle(),
            "a running turn is not idle"
        );
    }

    fn idle_supervisor() -> Facts {
        let mut observation = judged(SemanticState::Waiting, 0.9);
        observation.ambiguity = Some(Ambiguity::TurnStoppedIdle);
        Facts {
            session_id: "session_supervisor".to_string(),
            agent_id: "agent_supervisor".to_string(),
            live: true,
            execution_id: Some("exec_2".to_string()),
            end_cause: None,
            observation: Some(observation),
            observation_event_id: Some(50),
            observation_at_ms: Some(NOW - 5_000),
            evidence_seq: Some(9),
            episode: Some(1),
            stops: Vec::new(),
            open_command: false,
            dispatched: Vec::new(),
            told: std::collections::BTreeSet::new(),
            last_delivery: None,
            model: ModelProfile::Default,
            resumed_young: 0,
            failed_resume: None,
            turn_ended: true,
            checkpoint: None,
            charter: None,
            jobs: Vec::new(),
        }
    }

    fn nudge_sent(finished_at_ms: i64) -> Sent {
        Sent {
            id: None,
            key: "dispatch-nudge-session_worker_a-7".to_string(),
            text: Some("continue".to_string()),
            created_at_ms: finished_at_ms - 500,
            finished_at_ms: Some(finished_at_ms),
            succeeded: Some(true),
            error: None,
        }
    }

    #[test]
    fn a_confident_idle_worker_gets_one_continue() {
        let plan = decide(
            &[worker(judged(SemanticState::Waiting, 0.9))],
            false,
            NOW,
            "spawn",
        );
        assert_eq!(plan.orders.len(), 1);
        assert_eq!(plan.orders[0].key, "dispatch-nudge-session_worker_a-7");
        assert_eq!(
            plan.orders[0].action,
            CommandAction::Input {
                text: "continue".to_string(),
                tags: Vec::new(),
            }
        );
        assert!(plan.pending.is_empty());
    }

    #[test]
    fn a_judgement_older_than_the_latest_fact_is_not_acted_on() {
        // Live case: the ready screen before the prompt was judged idle, and
        // the prompt had already arrived when the dispatcher looked.
        let mut stale = worker(judged(SemanticState::Waiting, 0.9));
        stale.evidence_seq = Some(10);
        let plan = decide(&[stale], false, NOW, "spawn");
        assert_eq!(plan, super::Plan::default());
    }

    #[test]
    fn an_unsure_or_human_needing_worker_is_not_nudged() {
        let unsure = worker(judged(SemanticState::Waiting, 0.6));
        assert!(decide(&[unsure], false, NOW, "spawn").orders.is_empty());
        let mut asking = judged(SemanticState::Waiting, 0.9);
        asking.needs_human = Some(0.4);
        assert!(
            decide(&[worker(asking)], false, NOW, "spawn")
                .orders
                .is_empty()
        );
        let mut rules = judged(SemanticState::Waiting, 1.0);
        rules.judge = "rules".to_string();
        assert!(
            decide(&[worker(rules)], false, NOW, "spawn")
                .orders
                .is_empty()
        );
    }

    #[test]
    fn a_nudge_that_did_not_help_escalates_instead_of_repeating() {
        let mut facts = worker(judged(SemanticState::Waiting, 0.9));
        facts.dispatched.push(nudge_sent(NOW - 5_000));
        let plan = decide(&[facts], false, NOW, "spawn");
        assert!(plan.orders.is_empty(), "no second nudge in one episode");
        assert_eq!(plan.pending.len(), 1);
        assert_eq!(plan.pending[0].why, Why::NudgeFailed);
    }

    #[test]
    fn the_judgement_from_before_the_nudge_landed_waits() {
        let mut facts = worker(judged(SemanticState::Waiting, 0.9));
        facts.dispatched.push(nudge_sent(NOW));
        let plan = decide(&[facts], false, NOW, "spawn");
        assert!(plan.orders.is_empty());
        assert!(plan.pending.is_empty());
    }

    #[test]
    fn a_blocked_worker_reaches_an_idle_supervisor_once() {
        let mut blocked = judged(SemanticState::Blocked, 0.8);
        blocked.needs_human = Some(0.9);
        blocked.attention = true;
        let rows = [worker(blocked), idle_supervisor()];
        let plan = decide(&rows, true, NOW, "spawn");
        assert_eq!(plan.orders.len(), 1);
        let digest = &plan.orders[0];
        assert_eq!(digest.session_id, "session_supervisor");
        let CommandAction::Input { text, tags } = &digest.action else {
            panic!("a digest is typed input");
        };
        assert_eq!(tags, &["session_worker_a 100"]);
        assert!(text.contains("needs_human 90%"));

        // Once delivered, the same escalation is not sent again; while it
        // holds, the plan keeps it as told (the attention view shows it).
        let mut supervisor = idle_supervisor();
        delivered(&mut supervisor, digest);
        let rows = [rows[0].clone(), supervisor];
        let plan = decide(&rows, true, NOW, "spawn");
        assert!(plan.orders.is_empty());
        assert!(plan.pending.is_empty());
        let told: Vec<String> = plan.told.iter().map(Escalation::tag).collect();
        assert_eq!(told, ["session_worker_a 100"]);
        assert_eq!(
            plan.told[0].since_ms,
            Some(NOW - 1_000),
            "as of its judgement"
        );
    }

    #[test]
    fn a_disk_record_reaches_an_idle_supervisor_once() {
        let host = [Escalation {
            session_id: "host".to_string(),
            id: "disk-7".to_string(),
            why: Why::Disk,
            line: "tmpdir /tmp has 5.0 GiB free".to_string(),
            since_ms: None,
        }];
        let rows = [idle_supervisor()];
        let plan = decide_with(&rows, &Work::default(), &host, true, NOW, "spawn");
        let CommandAction::Input { text, .. } = &plan.orders[0].action else {
            panic!("a digest is typed input");
        };
        assert!(text.contains("- host — disk space on this host: tmpdir /tmp has 5.0 GiB free\n"));
        let mut supervisor = idle_supervisor();
        delivered(&mut supervisor, &plan.orders[0]);
        let plan = decide_with(&[supervisor], &Work::default(), &host, true, NOW, "spawn");
        assert!(
            plan.orders.is_empty() && plan.pending.is_empty(),
            "told once"
        );
        assert_eq!(
            plan.told, host,
            "told, and the path is still low: unresolved"
        );
    }

    /// The live page of 2026-11-06 20:30: two disk recoveries told 8 h
    /// earlier were listed as told and unresolved. A recovery is news: told
    /// once, then resolved, while a low path stays unresolved until its
    /// recovery replaces it (crate::disk::escalations).
    #[test]
    fn a_told_recovery_is_resolved_and_a_told_low_path_is_not() {
        let record = |id: i64, path: &str, state: &str, free: u64| {
            serde_json::json!({"type": "host.disk", "label": "tmpdir", "path": path,
                               "state": state, "free_bytes": free << 30, "warn_bytes": 20_u64 << 30,
                               "floor_bytes": 2_u64 << 30, "event_id": id,
                               "at_ms": NOW - 8 * 3_600_000})
        };
        let host = crate::disk::escalations(
            &[
                record(7, "/data/tmp", "recovered", 43),
                record(8, "/ws", "low", 5),
            ],
            NOW,
        );
        let rows = [idle_supervisor()];
        let plan = decide_with(&rows, &Work::default(), &host, true, NOW, "spawn");
        let pending: Vec<String> = plan.pending.iter().map(Escalation::tag).collect();
        assert_eq!(
            pending,
            ["host disk-7", "host disk-8"],
            "both news until told"
        );
        let CommandAction::Input { text, .. } = &plan.orders[0].action else {
            panic!("a digest is typed input");
        };
        assert!(
            text.contains(
                "- host — disk space back on this host: tmpdir /data/tmp has 43.0 GiB free again\n"
            ),
            "{text}"
        );
        let mut supervisor = idle_supervisor();
        delivered(&mut supervisor, &plan.orders[0]);
        let plan = decide_with(&[supervisor], &Work::default(), &host, true, NOW, "spawn");
        assert!(
            plan.orders.is_empty() && plan.pending.is_empty(),
            "told once"
        );
        let told: Vec<String> = plan.told.iter().map(Escalation::tag).collect();
        assert_eq!(told, ["host disk-8"], "the recovery is not unresolved");
    }

    /// News of every kind is resolved by being told: a told approval, human
    /// message or pair of host faults is never kept as unresolved, even if
    /// its source still gives it.
    #[test]
    fn told_news_is_never_unresolved() {
        let item = |why: Why, id: &str| Escalation {
            session_id: "host".to_string(),
            id: id.to_string(),
            why,
            line: String::new(),
            since_ms: None,
        };
        let host = [
            item(Why::Approved, "approved-core"),
            item(Why::Human, "human-4"),
            item(Why::GateHostFault, "gate-host-fault-9"),
            item(Why::GateRoom, "gate-room-12"),
        ];
        let mut supervisor = idle_supervisor();
        supervisor.told = host.iter().map(Escalation::tag).collect();
        let plan = decide_with(&[supervisor], &Work::default(), &host, true, NOW, "spawn");
        assert!(plan.pending.is_empty());
        let told: Vec<String> = plan.told.iter().map(Escalation::tag).collect();
        assert_eq!(told, ["host gate-room-12"], "only the condition holds");
    }

    /// The live sequence of 2026-11-06: 100 human notes to one worker could
    /// not be delivered, each its own line, and they filled 16 digests in a
    /// row while a report waited behind them. One session's items of one
    /// kind are one line, every tag filed.
    #[test]
    fn repeated_items_about_one_session_are_one_digest_line() {
        let human = |n: usize| Escalation {
            session_id: "session_worker_b".to_string(),
            id: format!("human-{n}"),
            why: Why::Human,
            line: format!("note {n} could not be delivered"),
            since_ms: None,
        };
        let mut host: Vec<Escalation> = (1..=20).map(human).collect();
        host.push(Escalation {
            session_id: "session_worker_c".to_string(),
            id: "report-9-0".to_string(),
            why: Why::Report,
            line: "task #9 reported".to_string(),
            since_ms: None,
        });
        // Items about the host are each about something else.
        for place in ["workspace", "tmpdir"] {
            host.push(Escalation {
                session_id: "host".to_string(),
                id: format!("disk-{place}"),
                why: Why::Disk,
                line: format!("{place} is low"),
                since_ms: None,
            });
        }
        let plan = decide_with(
            &[idle_supervisor()],
            &Work::default(),
            &host,
            true,
            NOW,
            "spawn",
        );
        let [digest] = plan.orders.as_slice() else {
            panic!("one digest: {:?}", plan.orders);
        };
        let CommandAction::Input { text, tags } = &digest.action else {
            panic!("a typed digest");
        };
        assert!(text.starts_with("Spawn dispatcher: 4 updates"), "{text}");
        assert!(
            text.contains("workspace is low") && text.contains("tmpdir is low"),
            "{text}"
        );
        assert!(
            text.contains("human message (20 of them, the latest): note 20 could not"),
            "{text}"
        );
        assert!(
            text.contains("task #9 reported"),
            "the report is not starved"
        );
        assert_eq!(tags.len(), 23, "every item it stands for is told");
    }

    /// The live sequence of 2026-11-06: the supervisor ended its turn on a
    /// question for the human, Jev judged it BLOCKED at its prompt, and no
    /// digest reached it for four hours while a worker sat idle.
    #[test]
    fn a_supervisor_that_asked_the_human_a_question_still_gets_digests() {
        let mut blocked = judged(SemanticState::Blocked, 0.8);
        blocked.attention = true;
        let mut asking = idle_supervisor();
        let mut question = judged(SemanticState::Blocked, 0.96);
        question.attention = true;
        question.needs_human = Some(0.87);
        asking.observation = Some(question);
        let plan = decide(
            &[worker(blocked.clone()), asking.clone()],
            true,
            NOW,
            "spawn",
        );
        assert_eq!(plan.orders.len(), 1, "the digest goes to the supervisor");
        assert_eq!(plan.orders[0].session_id, "session_supervisor");

        let mut working = asking;
        working.observation = Some(judged(SemanticState::Blocked, 0.96));
        working.observation.as_mut().expect("set").ambiguity = None;
        let plan = decide(&[worker(blocked), working], true, NOW, "spawn");
        assert!(
            plan.orders.is_empty(),
            "BLOCKED away from its prompt (a dialog, a running turn) is not idle"
        );
    }

    /// Objective 20: the second host fault in a row reaches an idle
    /// supervisor once, as an item of `host` tagged
    /// `host gate-host-fault-<event id>`, naming the task, the mission, the
    /// count, the evidence and when the task is judged again.
    #[test]
    fn two_host_faults_in_a_row_reach_an_idle_supervisor_once() {
        let crash = "2026-11-06 01:46:58.017 UTC [1] LOG:  server process (PID 4242) was \
                     terminated by signal 11: Segmentation fault";
        let records = [
            serde_json::json!({"type": "task.gated", "verdict": "host_fault", "id": 9,
                "event_id": 41, "mission": 3, "host_faults": 2, "escalate": true,
                "evidence": "server_log", "crash": [crash, "2026-11-06 01:46:58.018 UTC [1] \
                LOG:  terminating any other active server processes"],
                "failures": [{"test": "merges::tests::a", "message": "db error: ERROR: x",
                              "class": "database"}],
                "backoff_ms": 600_000, "due_ms": 1_793_930_000_000_i64}),
            serde_json::json!({"type": "task.gated", "verdict": "host_fault", "id": 10,
                "event_id": 52, "mission": 4, "host_faults": 2, "escalate": true,
                "evidence": "failures",
                "failures": [{"test": "api::tests::claims_in_order",
                              "message": "error connecting to server: Connection refused",
                              "class": "connection"},
                             {"test": "control::tests::the_fold", "message": "SqlState(E57P02)",
                              "class": "connection"}],
                "backoff_ms": 600_000, "due_ms": 1_793_930_000_500_i64}),
        ];
        let host = super::digest::host_fault_escalations(&records);
        let tags: Vec<String> = host.iter().map(Escalation::tag).collect();
        assert_eq!(tags, ["host gate-host-fault-41", "host gate-host-fault-52"]);
        assert!(host.iter().all(|item| item.why == Why::GateHostFault));
        assert!(
            host[0].line.starts_with(
                "task #9 of mission 3: 2 gate failures in a row were the \
                                      host's, not the work's: the test database's log shows a \
                                      crash during the gate: "
            ) && host[0].line.contains(crash)
                && host[0]
                    .line
                    .contains("terminating any other active server processes")
                && host[0].line.contains("judged again at 2026-11-06T")
                && host[0].line.contains("nothing halts"),
            "{}",
            host[0].line
        );
        assert!(
            host[1].line.contains(
                "every check-tests.sh failure (2) was a connection error to the test database, \
                 first api::tests::claims_in_order: error connecting to server: Connection \
                 refused"
            ),
            "{}",
            host[1].line
        );
        assert!(host.iter().all(|item| !item.line.contains('\n')));
        assert!(
            super::digest::host_fault_escalations(&[serde_json::json!({"id": 9})]).is_empty(),
            "a record without its event is no item"
        );

        let rows = [idle_supervisor()];
        let plan = decide_with(&rows, &Work::default(), &host[..1], true, NOW, "spawn");
        let CommandAction::Input { text, tags } = &plan.orders[0].action else {
            panic!("a digest is typed input");
        };
        assert_eq!(tags, &["host gate-host-fault-41"]);
        let line = text
            .lines()
            .find(|line| line.contains("task #9"))
            .expect("the item's line");
        assert!(
            line.starts_with("- host — host faults at the gate (the host's, not the work's): ")
                && line.contains("was terminated by signal 11"),
            "{line}"
        );
        let mut supervisor = idle_supervisor();
        delivered(&mut supervisor, &plan.orders[0]);
        let plan = decide_with(
            &[supervisor],
            &Work::default(),
            &host[..1],
            true,
            NOW,
            "spawn",
        );
        assert!(
            plan.orders.is_empty() && plan.pending.is_empty(),
            "told once"
        );
    }

    /// An objective that ran out of attempts and yielded its slot reaches
    /// an idle supervisor once, as an item of `missions` tagged
    /// `missions objective-yielded-<event id>`, its line naming the mission
    /// and the record's reason; two yields keep a line each.
    #[test]
    fn a_yielded_objective_reaches_an_idle_supervisor_once() {
        let reason = "objective 19 had 3 failed attempts since its last turn (conflict, \
                      conflict, rejected); task #127 yields its slot to the next objective, its \
                      work set aside";
        let records = [
            serde_json::json!({"event_id": 901, "at_ms": 1_794_063_946_000_i64, "id": 127,
                "mission": 4, "objective": 19, "failures": 3, "limit": 3, "reason": reason}),
            serde_json::json!({"event_id": 902, "at_ms": 1_794_063_947_000_i64, "id": 106,
                "mission": 4, "objective": 21, "failures": 3, "limit": 3}),
        ];
        let items = super::digest::objective_yields(&records);
        let tags: Vec<String> = items.iter().map(Escalation::tag).collect();
        assert_eq!(
            tags,
            [
                "missions objective-yielded-901",
                "missions objective-yielded-902"
            ]
        );
        assert!(items.iter().all(|item| item.why == Why::ObjectiveYielded));
        assert_eq!(items[0].since_ms, Some(1_794_063_946_000));
        assert!(
            items[0]
                .line
                .starts_with("mission 4: objective 19 had 3 failed attempts")
                && items[0].line.contains("task #127 yields its slot")
                && items[0].line.contains("nothing halts"),
            "{}",
            items[0].line
        );
        assert!(
            items[1].line.contains("task #106 yielded its slot"),
            "{}",
            items[1].line
        );
        assert!(
            super::digest::objective_yields(&[serde_json::json!({"id": 5})]).is_empty(),
            "a record without its event is no item"
        );

        let rows = [idle_supervisor()];
        let plan = decide_with(&rows, &Work::default(), &items, true, NOW, "spawn");
        let CommandAction::Input { text, tags } = &plan.orders[0].action else {
            panic!("a digest is typed input");
        };
        assert_eq!(
            tags,
            &[
                "missions objective-yielded-901",
                "missions objective-yielded-902"
            ]
        );
        assert!(
            text.lines().any(|line| line.starts_with(
                "- missions — an objective ran out of attempts and yielded its slot: mission 4"
            ) && line.contains("task #127")),
            "{text}"
        );
        assert!(
            text.lines().any(|line| line.contains("task #106")),
            "each yield keeps its own line: {text}"
        );
    }

    /// Objective 34: an owner's hold that reached its bound and was ended by
    /// the integrator's `source.hold_expired` reaches an idle supervisor
    /// once, as an item of `missions` tagged
    /// `missions source-hold-expired-<event id>`, under its own heading, its
    /// line naming the source, the hold (event id, reason, when taken, its
    /// bound), when it expired, each waiting task with its mission, and that
    /// landings resumed. Two expiries keep a line each.
    #[test]
    fn an_expired_hold_reaches_an_idle_supervisor_once_with_its_waiting_tasks() {
        let records = [
            serde_json::json!({"event_id": 88, "at_ms": 1_793_930_021_000_i64,
                "source": "/srv/missions/spawn.git", "hold": 71,
                "expires_ms": 1_793_930_020_000_i64, "reason": "owner gate ffc9fe7\nsecond line",
                "held_at_ms": 1_793_919_220_000_i64, "bound_s": 10_800,
                "main": "a3543c2e0f1b2c3d4e5f60718293a4b5c6d7e8f9",
                "waiting": [{"task": 85, "mission": 4}, {"task": 91, "mission": 6}]}),
            serde_json::json!({"event_id": 95, "at_ms": 1_793_930_030_000_i64,
                "source": "/srv/missions/supply-pa.git", "hold": 90,
                "expires_ms": 1_793_930_029_000_i64, "held_at_ms": 1_793_930_009_000_i64,
                "bound_s": 20, "main": "b3543c2e0f1b2c3d4e5f60718293a4b5c6d7e8f9",
                "waiting": []}),
        ];
        let items = super::digest::hold_expiries(&records);
        let tags: Vec<String> = items.iter().map(Escalation::tag).collect();
        assert_eq!(
            tags,
            [
                "missions source-hold-expired-88",
                "missions source-hold-expired-95"
            ]
        );
        assert!(items.iter().all(|item| item.why == Why::HoldExpired));
        assert_eq!(items[0].since_ms, Some(1_793_930_021_000));
        let line = &items[0].line;
        for part in [
            "the owner's hold 71 (\"owner gate ffc9fe7 second line\") on source \
             /srv/missions/spawn.git",
            "taken 2026-11-0",
            "a bound of 10800 s",
            "expired at 2026-11-0",
            "ended by record 88",
            "task #85 of mission 4, task #91 of mission 6 waited behind it",
            "landings on /srv/missions/spawn.git resumed",
            "against its main a3543c2e0f1b may be stale now: tell the owner",
        ] {
            assert!(line.contains(part), "{part}: {line}");
        }
        assert!(
            items[1].line.contains("no task waited behind it"),
            "{}",
            items[1].line
        );
        assert!(items.iter().all(|item| !item.line.contains('\n')));
        assert!(
            super::digest::hold_expiries(&[serde_json::json!({"source": "/srv/x.git"})]).is_empty(),
            "a record without its event is no item"
        );

        let rows = [idle_supervisor()];
        let plan = decide_with(&rows, &Work::default(), &items, true, NOW, "spawn");
        let CommandAction::Input { text, tags } = &plan.orders[0].action else {
            panic!("a digest is typed input");
        };
        assert_eq!(
            tags,
            &[
                "missions source-hold-expired-88",
                "missions source-hold-expired-95"
            ]
        );
        let line = text
            .lines()
            .find(|line| line.contains("task #85"))
            .expect("the first expiry's line");
        assert!(
            line.starts_with("- missions — a source hold expired: the owner's hold 71")
                && line.contains("/srv/missions/spawn.git"),
            "{line}"
        );
        assert!(
            text.lines()
                .any(|line| line.contains("/srv/missions/supply-pa.git")),
            "each expiry keeps its own line: {text}"
        );
        let mut supervisor = idle_supervisor();
        delivered(&mut supervisor, &plan.orders[0]);
        let plan = decide_with(&[supervisor], &Work::default(), &items, true, NOW, "spawn");
        assert!(
            plan.orders.is_empty() && plan.pending.is_empty() && plan.told.is_empty(),
            "told once, and resolved by the telling"
        );
    }

    #[test]
    fn a_busy_or_absent_supervisor_leaves_escalations_pending() {
        let mut blocked = judged(SemanticState::Blocked, 0.8);
        blocked.attention = true;
        let mut busy = idle_supervisor();
        busy.open_command = true;
        let plan = decide(&[worker(blocked.clone()), busy], true, NOW, "spawn");
        assert!(plan.orders.is_empty());
        assert_eq!(plan.pending.len(), 1, "it waits, coalescing");
        let plan = decide(&[worker(blocked)], true, NOW, "spawn");
        assert!(
            plan.orders.is_empty(),
            "no supervisor: the attention queue holds it"
        );
        assert_eq!(plan.pending.len(), 1);
    }

    #[test]
    fn a_lost_worker_is_resumed_once_then_escalated() {
        let mut lost = worker(judged(SemanticState::Unknown, 0.5));
        lost.live = false;
        lost.end_cause = Some("lost".to_string());
        let plan = decide(&[lost.clone()], false, NOW, "spawn");
        assert_eq!(plan.orders[0].key, "dispatch-resume-exec_5");
        lost.dispatched.push(Sent {
            id: None,
            key: "dispatch-resume-exec_5".to_string(),
            text: None,
            created_at_ms: NOW - 10,
            finished_at_ms: Some(NOW - 5),
            succeeded: Some(false),
            error: None,
        });
        let plan = decide(&[lost], false, NOW, "spawn");
        assert!(plan.orders.is_empty());
        assert_eq!(plan.pending[0].why, Why::ResumeFailed);
    }

    #[test]
    fn a_worker_lost_again_right_after_its_resume_is_escalated_not_resumed_again() {
        let mut lost = worker(judged(SemanticState::Unknown, 0.5));
        lost.live = false;
        lost.end_cause = Some("lost".to_string());
        // The resumed execution has its own id, so its resume key is new;
        // what it did with the last resume decides.
        lost.execution_id = Some("exec_6".to_string());
        lost.resumed_young = 1;
        let plan = decide(&[lost.clone()], false, NOW, "spawn");
        assert!(plan.orders.is_empty(), "no second resume");
        assert_eq!(plan.pending[0].why, Why::ResumeFailed);
        lost.resumed_young = 0;
        let plan = decide(&[lost], false, NOW, "spawn");
        assert_eq!(
            plan.orders[0].key, "dispatch-resume-exec_6",
            "an execution that stayed up is resumed again"
        );
    }

    /// What the Bridge leaves of a resume it abandoned at its ready wait,
    /// as the 129 sequence of 2026-11-07 did: command `command` started
    /// `exec_<command>`, whose pane never became ready, so the execution
    /// ended lost and the command failed.
    fn abandoned_at_ready(command: i64, finished_at_ms: i64) -> FailedResume {
        FailedResume {
            execution_id: format!("exec_{command}"),
            command_id: Some(command),
            ended_at_ms: Some(finished_at_ms),
            evidence: Some(
                "its start failed: pane never became ready, last state Unknown".to_string(),
            ),
            error: Some("pane never became ready, last state Unknown".to_string()),
        }
    }

    /// Task 132 (1): the Bridge abandons every resume at its ready wait (a
    /// pane that never becomes ready, as after the host move of
    /// 2026-11-07), and no resume is ever sent under the key of the
    /// execution it started. Over many passes, the dispatcher resumes once
    /// and escalates once, with the evidence; a second resume that ends the
    /// same way in a row (a person's, after the escalation) neither brings a
    /// dispatch nor a second escalation: the dispatcher sends nothing more.
    #[test]
    fn a_resume_lost_before_ready_twice_in_a_row_is_escalated_once_and_never_dispatched_again() {
        let mut lost = worker(judged(SemanticState::Unknown, 0.5));
        lost.live = false;
        lost.end_cause = Some("lost".to_string());
        let mut supervisor = idle_supervisor();
        let mut resumes = Vec::new();
        let mut digested = Vec::new();
        for pass in 0..12_i64 {
            let now = NOW + pass * 20_000;
            if pass == 6 {
                // Someone resumes it once more, outside the rule; the pane
                // never becomes ready again.
                lost.execution_id = Some("exec_940".to_string());
            }
            let plan = decide(&[lost.clone(), supervisor.clone()], true, now, "spawn");
            for order in &plan.orders {
                if order.session_id == supervisor.session_id {
                    let CommandAction::Input { tags, .. } = &order.action else {
                        panic!("a digest is typed input");
                    };
                    digested.extend(tags.iter().cloned());
                    delivered(&mut supervisor, order);
                    continue;
                }
                assert!(
                    matches!(order.action, CommandAction::Resume { .. }),
                    "{order:?}"
                );
                resumes.push(order.key.clone());
                let command = 920 + i64::try_from(resumes.len()).expect("few");
                let failed = abandoned_at_ready(command, now + 12_000);
                lost.dispatched.push(Sent {
                    id: Some(command),
                    key: order.key.clone(),
                    text: None,
                    created_at_ms: now,
                    finished_at_ms: failed.ended_at_ms,
                    succeeded: Some(false),
                    error: failed.error.clone(),
                });
                lost.execution_id = Some(failed.execution_id.clone());
                lost.failed_resume = Some(failed);
            }
        }
        assert_eq!(resumes, ["dispatch-resume-exec_5"], "resumed once, only");
        assert_eq!(
            digested,
            ["session_worker_a resume-exec_921"],
            "escalated once, about the resume that failed"
        );
        let plan = decide(&[lost, supervisor], true, NOW + 3_600_000, "spawn");
        assert!(plan.orders.is_empty(), "{:?}", plan.orders);
        assert!(plan.pending.is_empty(), "{:?}", plan.pending);
        let item = &plan.told[0];
        assert_eq!(item.why, Why::ResumeFailed);
        assert!(
            item.line
                .contains("its start failed: pane never became ready, last state Unknown")
                && item.line.contains("command 921")
                && item.line.contains("not resumed again"),
            "{}",
            item.line
        );
        assert_eq!(
            item.since_ms,
            Some(NOW + 12_000),
            "as of the failed execution's end"
        );
    }

    /// Task 132 (2): a resume that succeeded (its engine became ready and
    /// took its prompt) and whose engine then died within the minute still
    /// follows the young-death rule; one that stayed up is resumed again,
    /// as before.
    #[test]
    fn a_resume_that_succeeded_and_died_young_still_follows_the_young_death_rule() {
        let mut lost = worker(judged(SemanticState::Unknown, 0.5));
        lost.live = false;
        lost.end_cause = Some("lost".to_string());
        lost.execution_id = Some("exec_6".to_string());
        lost.dispatched.push(Sent {
            id: Some(6),
            key: "dispatch-resume-exec_5".to_string(),
            text: None,
            created_at_ms: NOW - 50_000,
            finished_at_ms: Some(NOW - 45_000),
            succeeded: Some(true),
            error: None,
        });
        lost.resumed_young = 1;
        let plan = decide(&[lost.clone()], false, NOW, "spawn");
        assert!(plan.orders.is_empty(), "no second resume");
        assert_eq!(plan.pending.len(), 1);
        assert_eq!(plan.pending[0].why, Why::ResumeFailed);
        assert_eq!(plan.pending[0].tag(), "session_worker_a resume-exec_6");
        assert_eq!(
            plan.pending[0].line,
            "engine was lost again within a minute of its automatic resume"
        );
        lost.resumed_young = 0;
        let plan = decide(&[lost], false, NOW, "spawn");
        assert_eq!(
            plan.orders[0].key, "dispatch-resume-exec_6",
            "a resumed execution that stayed up is resumed again"
        );
        assert!(plan.pending.is_empty());
    }

    #[test]
    fn a_lost_resident_is_left_to_the_liveness_rule() {
        let mut lost = worker(judged(SemanticState::Unknown, 0.5));
        lost.agent_id = "agent_resident_core".to_string();
        lost.session_id = "session_resident_core".to_string();
        lost.live = false;
        lost.end_cause = Some("lost".to_string());
        assert!(decide(&[lost], false, NOW, "spawn").orders.is_empty());
    }

    #[test]
    fn nudges_are_rate_limited_per_agent_hour() {
        let mut facts = worker(judged(SemanticState::Waiting, 0.9));
        for episode in 1..=3 {
            facts.dispatched.push(Sent {
                id: None,
                key: format!("dispatch-nudge-session_worker_a-{episode}"),
                text: Some("continue".to_string()),
                created_at_ms: NOW - 60_000,
                finished_at_ms: Some(NOW - 59_000),
                succeeded: Some(true),
                error: None,
            });
        }
        let plan = decide(&[facts], false, NOW, "spawn");
        assert!(plan.orders.is_empty());
        assert_eq!(plan.pending[0].why, Why::NudgeFailed);
    }

    #[test]
    fn the_supervisor_and_rsi_are_never_dispatched() {
        let mut rsi = worker(judged(SemanticState::Waiting, 0.9));
        rsi.agent_id = "agent_rsi".to_string();
        rsi.session_id = "session_rsi".to_string();
        let plan = decide(&[rsi, idle_supervisor()], true, NOW, "spawn");
        assert_eq!(plan, super::Plan::default());
    }

    #[test]
    fn an_idle_worker_too_unsure_to_nudge_is_reported() {
        // Live case: WAITING 0.64 / COMPLETED 0.32, needs_human 0.28.
        let mut idle = judged(SemanticState::Waiting, 0.67);
        idle.needs_human = Some(0.28);
        let plan = decide(&[worker(idle), idle_supervisor()], true, NOW, "spawn");
        assert_eq!(plan.orders.len(), 1);
        let CommandAction::Input { text, tags } = &plan.orders[0].action else {
            panic!("digest");
        };
        assert!(text.contains("idle, give it the next step"));
        assert_eq!(tags, &["session_worker_a idle-7"]);
    }

    #[test]
    fn a_worker_someone_stopped_is_not_reported_back() {
        let mut stopped = worker(Observation::basic(
            SemanticState::Completed,
            None,
            SessionSeq(9),
            Confidence::new(1.0).expect("valid"),
            "rules",
        ));
        stopped.live = false;
        stopped.end_cause = Some("stopped".to_string());
        let plan = decide(&[stopped, idle_supervisor()], true, NOW, "spawn");
        assert_eq!(plan, super::Plan::default());
    }

    #[test]
    fn a_completed_worker_is_reported_for_review() {
        let mut done = judged(SemanticState::Completed, 0.9);
        done.ambiguity = Some(Ambiguity::TurnStoppedIdle);
        let plan = decide(&[worker(done), idle_supervisor()], true, NOW, "spawn");
        assert_eq!(plan.orders.len(), 1);
        let CommandAction::Input { text, .. } = &plan.orders[0].action else {
            panic!("digest");
        };
        assert!(text.contains("completed, review it"));
    }

    #[test]
    fn a_supervisor_judged_before_its_latest_fact_gets_no_digest() {
        let mut blocked = judged(SemanticState::Blocked, 0.8);
        blocked.attention = true;
        let mut busy = idle_supervisor();
        busy.evidence_seq = Some(10);
        let plan = decide(&[worker(blocked), busy], true, NOW, "spawn");
        assert!(plan.orders.is_empty());
        assert_eq!(plan.pending.len(), 1);
    }

    /// Audit #1: a nudge that typed nothing (the pane was not ready) did
    /// not reach the worker, so a later idle judgement is no sign that
    /// `continue` did not help: it is sent again, and only once the retries
    /// are spent is the supervisor told, that it could not be typed.
    #[test]
    fn a_nudge_that_typed_nothing_is_sent_again_not_reported_as_unhelpful() {
        let refused = |key: &str, finished: i64| Sent {
            key: key.to_string(),
            succeeded: Some(false),
            error: Some("pane state Busy does not accept input".to_string()),
            ..nudge_sent(finished)
        };
        let mut facts = worker(judged(SemanticState::Waiting, 0.9));
        facts
            .dispatched
            .push(refused("dispatch-nudge-session_worker_a-7", NOW - 5_000));
        let plan = decide(&[facts.clone()], false, NOW, "spawn");
        assert!(plan.pending.is_empty(), "{:?}", plan.pending);
        assert_eq!(plan.orders.len(), 1, "{:?}", plan.orders);
        assert_eq!(
            plan.orders[0].key,
            "dispatch-nudge-session_worker_a-7-retry-1"
        );
        // Not before a judgement made after the refusal.
        let mut early = facts.clone();
        early.observation_at_ms = Some(NOW - 6_000);
        assert_eq!(
            decide(&[early], false, NOW, "spawn"),
            super::Plan::default()
        );
        // Failed nudges do not use up the hour's limit.
        let mut other = facts.clone();
        for episode in 1..=3 {
            other.dispatched.insert(
                0,
                refused(
                    &format!("dispatch-nudge-session_worker_a-{episode}"),
                    NOW - 60_000,
                ),
            );
        }
        assert_eq!(
            decide(&[other], false, NOW, "spawn").orders[0].key,
            "dispatch-nudge-session_worker_a-7-retry-1"
        );
        // The retry reached it and the worker stopped again: that is a nudge
        // that did not help.
        let mut landed = facts.clone();
        landed.dispatched.push(nudge_sent(NOW - 4_000));
        landed.dispatched[1].key = "dispatch-nudge-session_worker_a-7-retry-1".to_string();
        let plan = decide(&[landed], false, NOW, "spawn");
        assert!(plan.orders.is_empty());
        assert!(
            plan.pending[0]
                .line
                .contains("stopped again after `continue`")
        );
        // Refused every time: someone is told it was never typed.
        for retry in 1..=2 {
            facts.dispatched.push(refused(
                &format!("dispatch-nudge-session_worker_a-7-retry-{retry}"),
                NOW - 4_000,
            ));
        }
        let plan = decide(&[facts], false, NOW, "spawn");
        assert!(plan.orders.is_empty(), "{:?}", plan.orders);
        assert_eq!(plan.pending.len(), 1);
        assert_eq!(plan.pending[0].why, Why::NudgeFailed);
        assert!(
            plan.pending[0]
                .line
                .contains("could not be typed in 3 attempts"),
            "{}",
            plan.pending[0].line
        );
    }

    /// Audit #23: whether someone stopped the execution is how it ended,
    /// not whether a Stop was sent. A Stop that failed, then the engine
    /// exiting by itself, is a finished execution to review.
    #[test]
    fn a_worker_that_exited_by_itself_after_a_failed_stop_is_reported() {
        let mut exited = worker(Observation::basic(
            SemanticState::Completed,
            None,
            SessionSeq(9),
            Confidence::new(1.0).expect("valid"),
            "rules",
        ));
        exited.live = false;
        exited.end_cause = Some("exited".to_string());
        let plan = decide(&[exited, idle_supervisor()], true, NOW, "spawn");
        assert_eq!(plan.pending.len(), 1, "{:?}", plan.pending);
        assert_eq!(plan.pending[0].why, Why::Completed);
        assert_eq!(plan.pending[0].tag(), "session_worker_a done-7");
    }

    /// A Stop of the live execution, sent by `principal` under `key`, that
    /// failed at `finished`.
    fn failed_stop(id: i64, principal: &str, key: &str, finished: i64) -> super::StopSent {
        super::StopSent {
            id,
            key: key.to_string(),
            principal: principal.to_string(),
            action: "stop".to_string(),
            created_at_ms: finished - 100,
            finished_at_ms: Some(finished),
            succeeded: Some(false),
            error: Some("terminate spawn-exec_5: kill-session refused".to_string()),
        }
    }

    /// Audit #3: a Stop that failed leaves the worker running; whoever sent
    /// it (the integrator's budget abort, a person, the dispatcher retiring
    /// a closed task's worker), the dispatcher sends it again under a key of
    /// its own once the wait is over, without waiting for a fresh judgement,
    /// and escalates once the retries are spent. Nothing rests on memory:
    /// the plan is the same after any restart, from the command queue.
    #[test]
    fn a_failed_stop_is_pursued_then_escalated_whoever_sent_it() {
        for (principal, key, says) in [
            (
                "spawn-integrate",
                "integrate-abort-9",
                "task #9 was aborted over its mission's budget",
            ),
            ("human", "key_stop", "`human` asked to stop it"),
            (
                "spawn-dispatch",
                "retire-task-9",
                "task #9 is closed, so its worker is stopped",
            ),
        ] {
            // Its judgement went stale with the Stop's own records.
            let mut facts = worker(judged(SemanticState::Waiting, 0.9));
            facts.evidence_seq = Some(12);
            facts
                .stops
                .push(failed_stop(40, principal, key, NOW - 30_000));
            let rows = [facts.clone(), idle_supervisor()];
            let plan = decide(&rows, true, NOW, "spawn");
            assert_eq!(plan, super::Plan::default(), "a minute's wait first");
            let plan = decide(&rows, true, NOW + 30_000, "spawn");
            assert_eq!(plan.orders.len(), 1, "{key}: {:?}", plan.orders);
            assert_eq!(plan.orders[0].key, "stop-exec_5-retry-1");
            assert_eq!(plan.orders[0].session_id, "session_worker_a");
            assert_eq!(plan.orders[0].action, CommandAction::Stop);
            // Each retry fails too: the wait doubles, then it is escalated.
            let mut at = NOW + 31_000;
            for retry in 1..=crate::deliver::DELIVERY_RETRIES {
                facts.stops.push(failed_stop(
                    41,
                    "spawn-dispatch",
                    &format!("stop-exec_5-retry-{retry}"),
                    at,
                ));
                at += 3_600_000;
            }
            let rows = [facts.clone(), idle_supervisor()];
            let plan = decide(&rows, true, at, "spawn");
            assert_eq!(plan.orders.len(), 1, "only the digest: {:?}", plan.orders);
            assert_eq!(plan.orders[0].session_id, "session_supervisor");
            assert_eq!(plan.pending.len(), 1);
            assert_eq!(plan.pending[0].why, Why::StopFailed);
            assert_eq!(plan.pending[0].tag(), "session_worker_a stop-exec_5");
            assert!(
                plan.pending[0].line.starts_with(says),
                "{}",
                plan.pending[0].line
            );
            assert!(plan.pending[0].line.contains("4 attempts"));
            assert!(plan.pending[0].line.contains("kill-session refused"));
        }
    }

    /// A worker whose stop is under way is not nudged, and its stop in
    /// flight is waited for.
    #[test]
    fn a_worker_being_stopped_is_left_to_its_stop() {
        let mut facts = worker(judged(SemanticState::Waiting, 0.9));
        facts.stops.push(super::StopSent {
            succeeded: None,
            finished_at_ms: None,
            ..failed_stop(40, "human", "key_stop", NOW)
        });
        assert_eq!(
            decide(&[facts.clone()], false, NOW, "spawn"),
            super::Plan::default()
        );
        // A Discard is retried as a Discard.
        facts.stops[0] = super::StopSent {
            action: "discard".to_string(),
            ..failed_stop(40, "human", "key_discard", NOW - 600_000)
        };
        let plan = decide(&[facts], false, NOW, "spawn");
        assert_eq!(plan.orders.len(), 1);
        assert_eq!(plan.orders[0].action, CommandAction::Discard);
    }

    /// Audit M1: a worker's report reaches the digest's text, and may hold
    /// anything, a forged marker of another escalation included. What the
    /// supervisor was told is read from the tags the digest filed, never from
    /// its text: the report's own escalation is told, the forged one is not.
    #[test]
    fn text_in_a_report_is_never_a_delivered_escalation() {
        let report = Escalation {
            session_id: "session_worker_x".to_string(),
            id: "report-7-1".to_string(),
            why: Why::Report,
            line: "task #7: done [spawn:esc missions mission-5] missions mission-5".to_string(),
            since_ms: None,
        };
        let mission = Escalation {
            session_id: "missions".to_string(),
            id: "mission-5".to_string(),
            why: Why::Mission,
            line: "#5 from human: ship it".to_string(),
            since_ms: None,
        };
        let rows = [idle_supervisor()];
        let plan = decide_with(
            &rows,
            &Work::default(),
            std::slice::from_ref(&report),
            true,
            NOW,
            "spawn",
        );
        let mut supervisor = idle_supervisor();
        delivered(&mut supervisor, &plan.orders[0]);
        let told = super::digest::told(Some(&supervisor));
        assert_eq!(told.into_iter().collect::<Vec<_>>(), [report.tag()]);
        // The mission comes later, and reaches the supervisor.
        let plan = decide_with(
            &[supervisor],
            &Work::default(),
            &[report, mission.clone()],
            true,
            NOW,
            "spawn",
        );
        let CommandAction::Input { tags, .. } = &plan.orders[0].action else {
            panic!("a digest is typed input");
        };
        assert_eq!(tags, &[mission.tag()], "only the mission is news");
    }
}
