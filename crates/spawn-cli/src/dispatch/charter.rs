//! A changed charter reaches the running supervisor
//! (docs/fleet-architecture.md §3, the supervisor).
//!
//! The supervisor's charter is the prompt of a new native session. A resume
//! (after an engine exit, a crash or a reboot), a restarted `spawn loop` and
//! a compaction all keep the native session, and with it the charter it
//! holds: a deployed change to `supervise::charter` would wait for the next
//! handoff. So the charter a native session holds is a record, and the
//! dispatcher compares it with the one this build writes
//! ([`Charter::deployed`]):
//!
//! - the start that began the session recorded the hash of the charter it
//!   gave (`CommandAction::Start`'s `charter`); a start that recorded none (a
//!   person's, or one from before the record) gave no known charter;
//! - a charter input the dispatcher typed into that session since (an input
//!   tagged `charter <hash>` that succeeded, or failed after typing may have
//!   begun) replaced it: the newest one is what the session holds.
//!
//! When that differs from the deployed charter, the dispatcher types the
//! deployed one in at the supervisor's next idle prompt, through the same
//! path as a digest, which waits for the pass after: one input, keyed
//! `dispatch-charter-<hash>-over-<command>` where `<command>` is the record it
//! replaces (the start, or the charter input before it), so a restarted loop
//! submits the same key and a charter that comes back after a revert is
//! typed again over the one that replaced it. Once typed, the record says
//! the session holds it, and nothing is sent again for it, whatever
//! restarts; a new native session started with it holds it from its start.
//!
//! An attempt refused before anything was typed (a pane not ready, an
//! execution gone) told the supervisor nothing: it is sent again under
//! `<key>-retry-<n>` at an idle prompt judged on a fact after it (its
//! `command.finished` is one), [`ATTEMPTS`] times in all, then escalated as
//! never typed. One that may have typed is not typed again.
//!
//! Every decision rests on those records and on the supervisor's current
//! judgement, never on the time or the pane's text (docs/design.md §14).

use serde::Deserialize;
use spawn_core::command::CommandAction;

use super::{DIGEST_OPENING, Escalation, Facts, Order, Plan, Why, digest, worker};
use crate::supervise::{Charter, SUPERVISOR_AGENT, SUPERVISOR_SESSION};

/// What a charter input files with its command: `charter <hash>`.
const TAG: &str = "charter ";

/// Idempotency-key prefix of every charter input.
const KEY_PREFIX: &str = "dispatch-charter-";

/// Attempts at typing one charter that typed nothing, before it is
/// escalated instead.
const ATTEMPTS: usize = 3;

/// The records of the charter the supervisor's current native session
/// holds, as `dispatch_facts` returns them (its `charter`).
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub(crate) struct Records {
    /// The start that began the current native session: the command of the
    /// execution that first named it.
    #[serde(default)]
    pub(crate) start: Option<i64>,
    /// The hash of the charter that start gave; `None` when it recorded
    /// none.
    #[serde(default)]
    pub(crate) given: Option<String>,
    /// The dispatcher's charter inputs into the session since that start,
    /// oldest first.
    #[serde(default)]
    pub(crate) inputs: Vec<Input>,
}

/// One charter input the dispatcher sent, with its outcome.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct Input {
    pub(crate) id: i64,
    pub(crate) key: String,
    /// The hash its `charter <hash>` tag names.
    pub(crate) hash: String,
    #[serde(default)]
    pub(crate) finished_at_ms: Option<i64>,
    /// `None` while in flight.
    #[serde(default)]
    pub(crate) succeeded: Option<bool>,
    #[serde(default)]
    pub(crate) error: Option<String>,
}

/// The charter a native session holds, from its records.
struct Held<'a> {
    /// The record it came from: the charter input that typed it, or the
    /// start (0 when the log names none).
    basis: i64,
    /// Its hash; `None`: a start that recorded none.
    hash: Option<&'a str>,
    /// Charter inputs after `basis`, every one refused before typing.
    unreached: Vec<&'a Input>,
}

impl Records {
    /// What the session holds: the newest charter input that reached it
    /// (or may have), else what its start gave.
    fn held(&self) -> Held<'_> {
        let mut held = Held {
            basis: self.start.unwrap_or(0),
            hash: self.given.as_deref(),
            unreached: Vec::new(),
        };
        for input in &self.inputs {
            if worker::reached_with(input.succeeded, input.error.as_deref()) {
                held = Held {
                    basis: input.id,
                    hash: Some(&input.hash),
                    unreached: Vec::new(),
                };
            } else {
                held.unreached.push(input);
            }
        }
        held
    }
}

/// The key a charter input's attempts share: its own, without
/// `-retry-<n>`.
fn series(key: &str) -> &str {
    key.rsplit_once("-retry-")
        .filter(|(_, n)| !n.is_empty() && n.bytes().all(|byte| byte.is_ascii_digit()))
        .map_or(key, |(series, _)| series)
}

/// The text typed in: a line that says the charter changed and replaces the
/// earlier one, then the charter. It opens like a digest, so nothing reads
/// it as a person's answer.
pub(crate) fn text(deployed: &Charter) -> String {
    format!(
        "{DIGEST_OPENING} your charter changed (charter {}). The text below replaces, in full, \
the charter you were given earlier in this session; follow it from now on.\n\n{}",
        deployed.hash, deployed.text
    )
}

/// The input that gives the idle supervisor the `deployed` charter, when its
/// native session holds another and no attempt at it is spent.
fn order(supervisor: &Facts, deployed: &Charter) -> Option<Order> {
    let records = supervisor.charter.as_ref()?;
    // Live, nothing in flight, at its prompt on a judgement that covers its
    // latest fact: an attempt that failed finished before that fact.
    if !digest::supervisor_idle(supervisor) {
        return None;
    }
    let held = records.held();
    if held.hash == Some(deployed.hash.as_str()) {
        return None;
    }
    let key = format!("{KEY_PREFIX}{}-over-{}", deployed.hash, held.basis);
    let attempts = held
        .unreached
        .iter()
        .filter(|input| series(&input.key) == key)
        .count();
    if attempts >= ATTEMPTS {
        return None;
    }
    let key = if attempts == 0 {
        key
    } else {
        format!("{key}-retry-{attempts}")
    };
    Some(Order {
        agent_id: SUPERVISOR_AGENT.to_string(),
        session_id: SUPERVISOR_SESSION.to_string(),
        key,
        action: CommandAction::Input {
            text: text(deployed),
            tags: vec![format!("{TAG}{}", deployed.hash)],
        },
        task: None,
    })
}

/// Give the supervisor the `deployed` charter if its native session holds
/// another ([`order`]): the input takes this pass's turn at its prompt, and
/// a digest due now waits, its items still pending, for the next.
pub(super) fn give(plan: &mut Plan, rows: &[Facts], deployed: &Charter) {
    let Some(order) = rows
        .iter()
        .find(|facts| facts.session_id == SUPERVISOR_SESSION)
        .and_then(|supervisor| order(supervisor, deployed))
    else {
        return;
    };
    plan.orders
        .retain(|other| other.session_id != SUPERVISOR_SESSION);
    plan.orders.push(order);
}

/// The escalation for a charter never typed in: the latest charter input's
/// attempts since what the session holds were all refused before typing,
/// [`ATTEMPTS`] of them. From the records alone, so the attention view,
/// which does not know the deployed charter, shows the same item.
pub(super) fn escalation(supervisor: &Facts) -> Option<Escalation> {
    let records = supervisor.charter.as_ref()?;
    let held = records.held();
    let last = held.unreached.last()?;
    let key = series(&last.key);
    let attempts = held
        .unreached
        .iter()
        .filter(|input| series(&input.key) == key)
        .count();
    if attempts < ATTEMPTS {
        return None;
    }
    let holds = held.hash.map_or_else(
        || "the charter its start gave, which Spawn did not record".to_string(),
        |hash| format!("charter {hash}"),
    );
    Some(Escalation {
        session_id: supervisor.session_id.clone(),
        id: key.trim_start_matches("dispatch-").to_string(),
        why: Why::Charter,
        line: format!(
            "the deployed charter {} was never typed into this pane: {attempts} attempts were \
             refused before typing (the last: {}). It is not sent again; this native session \
             still holds {holds}. A handoff gives the new charter, or the owner types it in.",
            last.hash,
            crate::deliver::one_line(last.error.as_deref().unwrap_or("no error"), 200),
        ),
        since_ms: last.finished_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::{ATTEMPTS, Input, Records, escalation, give, order, series, text};
    use crate::dispatch::{Escalation, Facts, Plan, Why, decide_with};
    use crate::deliver::Work;
    use crate::supervise::Charter;
    use spawn_core::bridge::{MAY_HAVE_TYPED, PANE_NOT_READY};
    use spawn_core::command::CommandAction;
    use spawn_core::event::{ModelProfile, SessionSeq};
    use spawn_core::observation::{Ambiguity, Confidence, Observation, SemanticState};

    const NOW: i64 = 1_793_957_840_000;

    fn deployed() -> Charter {
        Charter::of("You are the Spawn supervisor (new).".to_string())
    }

    fn earlier() -> Charter {
        Charter::of("You are the Spawn supervisor (old).".to_string())
    }

    /// The supervisor live and idle at its prompt on a current judgement,
    /// its native session begun by start command 40 with `given`.
    fn supervisor(given: Option<&Charter>, inputs: Vec<Input>) -> Facts {
        Facts {
            session_id: "session_supervisor".to_string(),
            agent_id: "agent_supervisor".to_string(),
            live: true,
            execution_id: Some("exec_9".to_string()),
            end_cause: None,
            observation: Some(Observation {
                ambiguity: Some(Ambiguity::TurnStoppedIdle),
                ..Observation::basic(
                    SemanticState::Waiting,
                    None,
                    SessionSeq(9),
                    Confidence::new(0.9).expect("confidence"),
                    "jev",
                )
            }),
            observation_event_id: Some(50),
            observation_at_ms: Some(NOW - 5_000),
            evidence_seq: Some(9),
            episode: Some(1),
            open_command: false,
            dispatched: Vec::new(),
            told: std::collections::BTreeSet::new(),
            last_delivery: None,
            model: ModelProfile::Default,
            resumed_young: 0,
            stops: Vec::new(),
            turn_ended: true,
            checkpoint: None,
            charter: Some(Records {
                start: Some(40),
                given: given.map(|charter| charter.hash.clone()),
                inputs,
            }),
        }
    }

    fn input(id: i64, key: &str, hash: &str, error: Option<&str>) -> Input {
        Input {
            id,
            key: key.to_string(),
            hash: hash.to_string(),
            finished_at_ms: Some(NOW - 10_000),
            succeeded: Some(error.is_none()),
            error: error.map(str::to_string),
        }
    }

    fn key(charter: &Charter, over: i64) -> String {
        format!("dispatch-charter-{}-over-{over}", charter.hash)
    }

    #[test]
    fn a_session_started_with_the_deployed_charter_needs_nothing() {
        let facts = supervisor(Some(&deployed()), Vec::new());
        assert_eq!(order(&facts, &deployed()), None);
        assert_eq!(escalation(&facts), None);
    }

    /// A loop restarted after a deploy, or a resume: the native session runs
    /// on the charter its start gave. The new one is typed in once, tagged
    /// with its hash, opening with the line that says it replaces the old.
    #[test]
    fn a_running_session_on_an_older_charter_is_given_the_new_one_once() {
        let new = deployed();
        let facts = supervisor(Some(&earlier()), Vec::new());
        let order = order(&facts, &new).expect("the new charter is due");
        assert_eq!(order.session_id, "session_supervisor");
        assert_eq!(order.agent_id, "agent_supervisor");
        assert_eq!(order.key, key(&new, 40), "keyed by hash and the start it replaces");
        let CommandAction::Input { text: typed, tags } = &order.action else {
            panic!("the charter is typed input: {:?}", order.action);
        };
        assert_eq!(tags, &vec![format!("charter {}", new.hash)]);
        assert_eq!(typed, &text(&new));
        let opening = typed.lines().next().expect("an opening line");
        assert!(opening.starts_with("Spawn dispatcher:"), "{opening}");
        assert!(opening.contains("your charter changed"), "{opening}");
        assert!(opening.contains("replaces, in full,"), "{opening}");
        assert!(typed.ends_with(&new.text), "then the whole charter");

        // Typed: the session holds it, whatever restarts read these records.
        let told = supervisor(
            Some(&earlier()),
            vec![input(41, &order.key, &new.hash, None)],
        );
        assert_eq!(super::order(&told, &new), None);
        assert_eq!(escalation(&told), None);
        // Still in flight: nothing more.
        let mut flying = input(41, &order.key, &new.hash, None);
        flying.succeeded = None;
        flying.finished_at_ms = None;
        let mut in_flight = supervisor(Some(&earlier()), vec![flying]);
        in_flight.open_command = true;
        assert_eq!(super::order(&in_flight, &new), None);
    }

    /// The supervisor started before starts recorded their charter: it holds
    /// no known charter, so it is given the deployed one.
    #[test]
    fn a_start_that_recorded_no_charter_is_given_the_deployed_one() {
        let facts = supervisor(None, Vec::new());
        assert_eq!(
            order(&facts, &deployed()).map(|order| order.key),
            Some(key(&deployed(), 40))
        );
    }

    #[test]
    fn a_busy_or_unjudged_supervisor_waits_for_its_idle_prompt() {
        let new = deployed();
        let mut busy = supervisor(Some(&earlier()), Vec::new());
        busy.open_command = true;
        assert_eq!(order(&busy, &new), None);
        let mut stale = supervisor(Some(&earlier()), Vec::new());
        stale.evidence_seq = Some(12);
        assert_eq!(order(&stale, &new), None, "a judgement older than its latest fact");
        let mut down = supervisor(Some(&earlier()), Vec::new());
        down.live = false;
        assert_eq!(order(&down, &new), None);
        let mut working = supervisor(Some(&earlier()), Vec::new());
        working.observation = working.observation.map(|observation| Observation {
            ambiguity: None,
            state: SemanticState::Working,
            ..observation
        });
        assert_eq!(order(&working, &new), None);
    }

    /// A refused attempt told nothing: sent again under a key of its own on
    /// the next idle judgement, [`ATTEMPTS`] in all, then escalated once as
    /// never typed, and not sent again.
    #[test]
    fn a_charter_refused_before_typing_is_retried_then_escalated() {
        let new = deployed();
        let first = key(&new, 40);
        let refused = |id: i64, key: &str| input(id, key, &new.hash, Some(PANE_NOT_READY));
        let one = supervisor(Some(&earlier()), vec![refused(41, &first)]);
        assert_eq!(
            order(&one, &new).map(|order| order.key),
            Some(format!("{first}-retry-1"))
        );
        assert_eq!(escalation(&one), None);
        let two = supervisor(
            Some(&earlier()),
            vec![refused(41, &first), refused(42, &format!("{first}-retry-1"))],
        );
        assert_eq!(
            order(&two, &new).map(|order| order.key),
            Some(format!("{first}-retry-2"))
        );
        let spent = supervisor(
            Some(&earlier()),
            vec![
                refused(41, &first),
                refused(42, &format!("{first}-retry-1")),
                refused(43, &format!("{first}-retry-2")),
            ],
        );
        assert_eq!(ATTEMPTS, 3);
        assert_eq!(order(&spent, &new), None, "spent: not sent again");
        let item = escalation(&spent).expect("escalated");
        assert_eq!(item.why, Why::Charter);
        assert_eq!(
            item.tag(),
            format!("session_supervisor charter-{}-over-40", new.hash)
        );
        assert!(item.line.contains(&new.hash), "{}", item.line);
        assert!(item.line.contains("3 attempts"), "{}", item.line);
        assert!(item.line.contains(PANE_NOT_READY), "{}", item.line);
        assert!(
            item.line.contains(&format!("still holds charter {}", earlier().hash)),
            "{}",
            item.line
        );
    }

    /// One that failed after typing may have begun is not typed again: the
    /// session may hold it, as for a delivery.
    #[test]
    fn a_charter_that_may_have_been_typed_is_not_typed_again() {
        let new = deployed();
        let facts = supervisor(
            Some(&earlier()),
            vec![input(41, &key(&new, 40), &new.hash, Some(MAY_HAVE_TYPED))],
        );
        assert_eq!(order(&facts, &new), None);
        assert_eq!(escalation(&facts), None);
    }

    /// The records say what the session holds, not the deploy's history: a
    /// charter reverted and deployed again is typed again over the one that
    /// replaced it, under a key of its own.
    #[test]
    fn a_charter_deployed_again_after_a_revert_is_typed_again() {
        let new = deployed();
        let old = earlier();
        let facts = supervisor(
            Some(&old),
            vec![
                input(41, &key(&new, 40), &new.hash, None),
                input(55, &key(&old, 41), &old.hash, None),
            ],
        );
        assert_eq!(
            order(&facts, &new).map(|order| order.key),
            Some(key(&new, 55))
        );
        assert_eq!(order(&facts, &old), None, "it holds the reverted one");
    }

    /// A deploy that changed the charter again since a spent one: the new
    /// charter is tried afresh, and the old one's item lasts only until the
    /// new attempt is the latest record.
    #[test]
    fn a_newer_charter_is_tried_after_a_spent_one() {
        let new = deployed();
        let newer = Charter::of("You are the Spawn supervisor (newer).".to_string());
        let first = key(&new, 40);
        let refused = |id: i64, key: &str, hash: &str| input(id, key, hash, Some(PANE_NOT_READY));
        let mut inputs = vec![
            refused(41, &first, &new.hash),
            refused(42, &format!("{first}-retry-1"), &new.hash),
            refused(43, &format!("{first}-retry-2"), &new.hash),
        ];
        let spent = supervisor(Some(&earlier()), inputs.clone());
        assert_eq!(
            order(&spent, &newer).map(|order| order.key),
            Some(key(&newer, 40))
        );
        inputs.push(refused(44, &key(&newer, 40), &newer.hash));
        let tried = supervisor(Some(&earlier()), inputs);
        assert_eq!(escalation(&tried), None, "the latest attempt is not spent");
    }

    /// The input takes the pass's turn at the idle supervisor's prompt; the
    /// digest due now waits for the next, its items still pending.
    #[test]
    fn the_charter_takes_the_digests_turn() {
        let new = deployed();
        let facts = supervisor(Some(&earlier()), Vec::new());
        let host = vec![Escalation {
            session_id: "host".to_string(),
            id: "disk-7".to_string(),
            why: Why::Disk,
            line: "free disk is low".to_string(),
            since_ms: None,
        }];
        let mut plan = decide_with(
            std::slice::from_ref(&facts),
            &Work::default(),
            &host,
            true,
            NOW,
            "spawn",
        );
        assert_eq!(plan.orders.len(), 1, "a digest is due: {:?}", plan.orders);
        give(&mut plan, std::slice::from_ref(&facts), &new);
        let [order] = plan.orders.as_slice() else {
            panic!("one order: {:?}", plan.orders);
        };
        assert_eq!(order.key, key(&new, 40));
        assert_eq!(plan.pending.len(), 1, "the digest's item waits");

        // Nothing due: the plan is left as it was.
        let held = supervisor(Some(&new), Vec::new());
        let mut plan = decide_with(
            std::slice::from_ref(&held),
            &Work::default(),
            &host,
            true,
            NOW,
            "spawn",
        );
        let before = plan.orders.clone();
        give(&mut plan, std::slice::from_ref(&held), &new);
        assert_eq!(plan.orders, before);
        let mut empty = Plan::default();
        give(&mut empty, &[], &new);
        assert!(empty.orders.is_empty(), "no supervisor, nothing to give");
    }

    /// The spent charter's item reaches the supervisor and the attention
    /// queue like any condition: pending until a digest told it.
    #[test]
    fn a_spent_charter_is_an_item_of_the_plan() {
        let new = deployed();
        let first = key(&new, 40);
        let refused = |id: i64, key: &str| input(id, key, &new.hash, Some(PANE_NOT_READY));
        let facts = supervisor(
            Some(&earlier()),
            vec![
                refused(41, &first),
                refused(42, &format!("{first}-retry-1")),
                refused(43, &format!("{first}-retry-2")),
            ],
        );
        let plan = decide_with(
            std::slice::from_ref(&facts),
            &Work::default(),
            &[],
            true,
            NOW,
            "spawn",
        );
        assert_eq!(plan.pending.len(), 1, "{:?}", plan.pending);
        assert_eq!(plan.pending[0].why, Why::Charter);
        assert!(!Why::Charter.is_news(), "a condition, while it holds");
    }

    #[test]
    fn retries_share_the_key_of_their_first_attempt() {
        assert_eq!(series("dispatch-charter-ab-over-4"), "dispatch-charter-ab-over-4");
        assert_eq!(
            series("dispatch-charter-ab-over-4-retry-2"),
            "dispatch-charter-ab-over-4"
        );
        assert_eq!(series("x-retry-"), "x-retry-");
        assert_eq!(series("x-retry-a"), "x-retry-a");
    }

    /// The records as `dispatch_facts` returns them.
    #[test]
    fn records_read_from_the_facts_row() {
        let row = serde_json::json!({
            "session_id": "session_supervisor",
            "agent_id": "agent_supervisor",
            "charter": {
                "start": 40,
                "given": "00aa",
                "inputs": [{
                    "id": 41, "key": "dispatch-charter-bb-over-40", "hash": "bb",
                    "finished_at_ms": 5, "succeeded": true, "error": null
                }]
            }
        });
        let facts: Facts = serde_json::from_value(row).expect("a facts row");
        let records = facts.charter.expect("records");
        assert_eq!(records.start, Some(40));
        assert_eq!(records.given.as_deref(), Some("00aa"));
        assert_eq!(records.held().hash, Some("bb"));
        assert_eq!(records.held().basis, 41);
        let worker: Facts = serde_json::from_value(serde_json::json!({
            "session_id": "session_worker_a", "agent_id": "agent_worker_a", "charter": null
        }))
        .expect("a worker's row");
        assert_eq!(worker.charter, None);
    }
}
