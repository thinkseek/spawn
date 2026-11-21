//! Work stealing (objective 239, docs/fleet-architecture.md §3.2 "Work
//! stealing"): in each pass, before deliveries are planned, each pool's idle
//! residents, in name order, each take one task, the pool's oldest stealable
//! task (lowest id, from any other member's queue), through the store's
//! `spawn_task_steal`, which checks every rule again under the append lock.
//! A refusal (a race lost) is one line in the loop's log and nothing else.
//! Local rule work: no model call, no judge, no digest item; the candidates
//! come from the pass's own [`Work`]. `SPAWN_WORK_STEALING=0` switches it
//! off (the owner's `spawn task steal` still works).

use std::collections::{BTreeMap, BTreeSet};

use spawn_core::mission::TaskState;
use spawn_postgres::PostgresStore;

use crate::deliver::{Resident, TaskRow, Work};

/// The switch: `0` turns the automatic moves off; unset, they are on.
pub(crate) const ENV: &str = "SPAWN_WORK_STEALING";

/// Who the dispatcher's moves name, and why.
const BY: &str = "spawn-dispatch";
const REASON: &str = "oldest queued task of a busy pool member";

/// Whether the automatic moves are on, from the switch's value.
pub(crate) fn enabled(value: Option<&str>) -> bool {
    value.map(str::trim) != Some("0")
}

/// Whether `task` keeps its worker's session from taking another task: the
/// states delivery waits on ([`Work::blocked`]), and a revision.
fn holds_the_worker(task: &TaskRow) -> bool {
    matches!(
        task.state,
        TaskState::Delivered | TaskState::Reported | TaskState::Revise | TaskState::Landing
    ) || (task.state == TaskState::Accepted && task.gated)
}

/// Whether `session`'s resident has a cancel notice not yet told.
fn untold(work: &Work, session: &str) -> bool {
    work.cancelled
        .iter()
        .any(|cancelled| !cancelled.told && cancelled.session_id == session)
}

/// Whether the resident is idle: no open task of its own (assigned or any
/// state [`holds_the_worker`]) and no cancel notice not yet told.
fn idle(work: &Work, resident: &Resident) -> bool {
    !untold(work, &resident.session_id)
        && !work.tasks.iter().any(|task| {
            task.session_id == resident.session_id
                && (task.state == TaskState::Assigned || holds_the_worker(task))
        })
}

/// Whether `task` waits in its worker's queue, never the one delivery
/// gives its worker now: the worker is held, or has an older task of its
/// own assigned.
fn queued(work: &Work, task: &TaskRow) -> bool {
    let session = task.session_id.as_str();
    untold(work, session)
        || work.tasks.iter().any(|other| {
            other.id != task.id
                && other.session_id == session
                && (holds_the_worker(other)
                    || (other.state == TaskState::Assigned && other.id < task.id))
        })
}

/// Whether `task` may move at all: a resident task, assigned, never
/// revised, not pinned, of a mission open, not paused, not halted, waiting
/// in its worker's queue. The store's mission rule, read from this pass's
/// own work, so a halted or closed mission's task is never picked: it is
/// skipped and the pool's next stealable task goes instead.
fn stealable(work: &Work, task: &TaskRow) -> bool {
    task.kind == "resident"
        && task.state == TaskState::Assigned
        && task.revisions == 0
        && !task.pinned
        && matches!(task.mission_state.as_str(), "open" | "active")
        && !task.mission_halted
        && !work.paused(task.mission_id)
        && queued(work, task)
}

/// This pass's tries, `(taker's name, the tasks it may try in order)`: for
/// each pool, its idle approved residents in name order, each with the
/// pool's stealable queued tasks not its own, lowest id first. The store
/// checks every rule again (a delivery command, a move that just happened
/// among them); a refusal lets the taker try its next one in the same pass,
/// and a task another taker already moved is skipped ([`attempt`]).
pub(crate) fn choices(work: &Work) -> Vec<(String, Vec<i64>)> {
    let pool_of: BTreeMap<&str, &str> = work
        .residents
        .iter()
        .filter_map(|resident| Some((resident.agent_id.as_str(), resident.pool.as_deref()?)))
        .collect();
    let mut members: BTreeMap<&str, Vec<&Resident>> = BTreeMap::new();
    for resident in &work.residents {
        if let Some(pool) = resident.pool.as_deref()
            && resident.state == "approved"
        {
            members.entry(pool).or_default().push(resident);
        }
    }
    let mut tries = Vec::new();
    for (pool, residents) in members {
        let mut candidates: Vec<&TaskRow> = work
            .tasks
            .iter()
            .filter(|task| {
                pool_of.get(task.agent_id.as_str()) == Some(&pool) && stealable(work, task)
            })
            .collect();
        candidates.sort_by_key(|task| task.id);
        let mut sorted = residents;
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for resident in sorted.into_iter().filter(|resident| idle(work, resident)) {
            tries.push((
                resident.name.clone(),
                candidates
                    .iter()
                    .filter(|task| task.session_id != resident.session_id)
                    .map(|task| task.id)
                    .collect(),
            ));
        }
    }
    tries
}

/// The moves of one pass's tries ([`choices`]), each tried in order through
/// `steal`: the first of a taker's tasks it wins is its one move for the
/// pass; a refusal is a line in `lines` and its next task is tried, and a
/// task another taker already moved is skipped. The store's answer names
/// from and to whose session.
fn attempt(
    tries: &[(String, Vec<i64>)],
    lines: &mut Vec<String>,
    mut steal: impl FnMut(i64, &str) -> Result<String, String>,
) -> Vec<(i64, String)> {
    let mut moved: BTreeSet<i64> = BTreeSet::new();
    let mut moves = Vec::new();
    for (to, tasks) in tries {
        for task in tasks {
            if moved.contains(task) {
                continue;
            }
            match steal(*task, to) {
                Ok(where_) => {
                    lines.push(format!("dispatch: task #{task} stolen {where_}"));
                    moved.insert(*task);
                    moves.push((*task, to.clone()));
                    break;
                }
                Err(error) => lines.push(format!(
                    "dispatch: task #{task} not stolen by {to}: {error}"
                )),
            }
        }
    }
    moves
}

/// Make this pass's tries ([`choices`]) through the store, each said in
/// `lines`, a refusal too. Returns whether any move was made, so the pass
/// reads its work again before it plans deliveries; a pass whose tries were
/// all refused changed nothing and needs no reload.
pub(crate) fn run(store: &PostgresStore, work: &Work, lines: &mut Vec<String>) -> bool {
    let moves = attempt(&choices(work), lines, |task, to| {
        let answer = store
            .task_steal(task, to, BY, Some(REASON))
            .map_err(|error| error.to_string())?;
        Ok(format!(
            "from {} to {}",
            answer["from_session"].as_str().unwrap_or("?"),
            answer["to_session"].as_str().unwrap_or("?")
        ))
    });
    !moves.is_empty()
}

#[cfg(test)]
mod tests {
    use super::{attempt, choices, enabled};
    use crate::deliver::{Cancelled, Resident, TaskRow, Work};
    use spawn_core::mission::TaskState;

    fn resident(name: &str, pool: Option<&str>, state: &str) -> Resident {
        Resident {
            name: name.to_string(),
            agent_id: format!("agent_resident_{name}"),
            session_id: format!("session_resident_{name}"),
            state: state.to_string(),
            pool: pool.map(str::to_string),
        }
    }

    fn task(id: i64, worker: &str, state: TaskState) -> TaskRow {
        TaskRow {
            id,
            mission_id: 1,
            kind: "resident".to_string(),
            agent_id: format!("agent_resident_{worker}"),
            session_id: format!("session_resident_{worker}"),
            brief: format!("task {id}"),
            state,
            revisions: 0,
            review_note: None,
            mission_text: String::new(),
            charter: None,
            resident: Some(worker.to_string()),
            gated: false,
            mission_state: "open".to_string(),
            mission_halted: false,
            model: spawn_core::event::ModelProfile::default(),
            reissue_of: None,
            reissued_attempts: None,
            reassign_of: None,
            reassign_from_session: None,
            reassigned_as: None,
            reassigned_to_session: None,
            restart: None,
            pinned: false,
        }
    }

    /// The moves a pass makes when every try wins.
    fn moves(work: &Work) -> Vec<(i64, String)> {
        attempt(&choices(work), &mut Vec::new(), |task, _| {
            Ok(format!("task {task}"))
        })
    }

    /// ben runs task 1 and queues 2, 3 and 4; ann, cid and dee are idle,
    /// all four in pool `logs`.
    fn pool() -> Work {
        Work {
            residents: vec![
                resident("ann", Some("logs"), "approved"),
                resident("ben", Some("logs"), "approved"),
                resident("cid", Some("logs"), "approved"),
                resident("dee", Some("logs"), "approved"),
            ],
            tasks: vec![
                task(1, "ben", TaskState::Delivered),
                task(2, "ben", TaskState::Assigned),
                task(3, "ben", TaskState::Assigned),
                task(4, "ben", TaskState::Assigned),
            ],
            ..Work::default()
        }
    }

    /// Each idle resident, in name order, takes one task, the oldest not
    /// taken yet; the busy worker keeps the rest.
    #[test]
    fn idle_residents_in_name_order_take_the_oldest_queued_tasks_one_each() {
        let mut work = pool();
        work.residents.reverse();
        assert_eq!(
            moves(&work),
            [
                (2, "ann".to_string()),
                (3, "cid".to_string()),
                (4, "dee".to_string())
            ]
        );
        // One idle resident: one task, the oldest.
        work.residents
            .retain(|resident| ["ben", "dee"].contains(&resident.name.as_str()));
        assert_eq!(moves(&work), [(2, "dee".to_string())]);
    }

    /// A running, pinned, revised, paused-mission or ephemeral task never
    /// moves; nor an idle worker's next; nor to a taker that is not idle,
    /// not approved, or of another pool or none.
    #[test]
    fn only_a_queued_task_moves_and_only_to_an_idle_member_of_its_pool() {
        let mut work = pool();
        work.residents = vec![
            resident("ben", Some("logs"), "approved"),
            resident("cid", Some("logs"), "approved"),
            resident("hal", Some("logs"), "proposed"),
            resident("ivy", Some("logs"), "retired"),
            resident("sol", Some("other"), "approved"),
            resident("una", None, "approved"),
        ];
        work.tasks[1].pinned = true;
        work.tasks[2].revisions = 1;
        work.tasks[3].kind = "ephemeral".to_string();
        // cid holds a task of its own, assigned: not idle.
        work.tasks.push(task(9, "cid", TaskState::Assigned));
        assert_eq!(moves(&work), []);
        // cid's task 9 is its next (cid is idle otherwise): never taken.
        work.residents
            .push(resident("dee", Some("logs"), "approved"));
        assert_eq!(moves(&work), []);
        // A paused mission's task waits for its mission.
        let mut paused = pool();
        paused
            .residents
            .retain(|resident| ["ann", "ben"].contains(&resident.name.as_str()));
        for task in &mut paused.tasks {
            task.mission_id = 5;
        }
        paused.pauses = vec![crate::deliver::PausedMission {
            id: 5,
            pause_event: Some(1),
            ..Default::default()
        }];
        assert_eq!(moves(&paused), []);
        // A worker not yet told of a cancel is held: its tasks are queued,
        // and it takes none.
        let mut told = pool();
        told.tasks = vec![task(2, "ben", TaskState::Assigned)];
        told.residents
            .retain(|resident| ["ann", "ben"].contains(&resident.name.as_str()));
        assert_eq!(moves(&told), [], "ben is idle: task 2 is its next");
        told.cancelled = vec![Cancelled {
            id: 1,
            mission_id: 1,
            agent_id: "agent_resident_ben".to_string(),
            session_id: "session_resident_ben".to_string(),
            start: None,
            set_aside: None,
            told: false,
            closed_by: None,
            reason: None,
            yielded: false,
            yield_reason: None,
            reassigned_as: None,
            reassigned_to_session: None,
        }];
        assert_eq!(moves(&told), [(2, "ann".to_string())]);
    }

    /// One halt does not stop its pool's stealing: a halted or closed
    /// mission's queued task stays its worker's, and the pool's next
    /// stealable task goes instead.
    #[test]
    fn a_halted_or_closed_missions_task_is_skipped_and_the_next_one_moves() {
        for (state, halted) in [("open", true), ("closed", false)] {
            let mut work = pool();
            work.residents
                .retain(|resident| ["ann", "ben"].contains(&resident.name.as_str()));
            work.tasks[1].mission_state = state.to_string();
            work.tasks[1].mission_halted = halted;
            assert_eq!(
                moves(&work),
                [(3, "ann".to_string())],
                "task 2 ({state}, halted {halted}) stays ben's; ann takes 3"
            );
            assert!(
                !choices(&work).iter().any(|(_, tasks)| tasks.contains(&2)),
                "task 2 is in no one's tries"
            );
        }
        // All of a pool's queued tasks halted: nothing moves, nothing tried.
        let mut all = pool();
        for task in &mut all.tasks[1..] {
            task.mission_halted = true;
        }
        assert_eq!(choices(&all), []);
    }

    /// A refused first candidate (a lost race, a delivery command that
    /// appeared) lets the taker try its next one in the same pass, one move
    /// per taker.
    #[test]
    fn a_refused_first_candidate_lets_the_taker_try_the_next() {
        let work = pool();
        let tries = choices(&work);
        let mut lines = Vec::new();
        let mut asked = Vec::new();
        let moved = attempt(&tries, &mut lines, |task, to| {
            asked.push((task, to.to_string()));
            if task == 2 {
                Err("task 2 is not queued (a delivery command exists)".to_string())
            } else {
                Ok(format!("task {task}"))
            }
        });
        assert_eq!(
            moved,
            [(3, "ann".to_string()), (4, "cid".to_string())],
            "ann's next after 2, then cid's oldest untaken: one move each"
        );
        assert_eq!(asked[..2], [(2, "ann".to_string()), (3, "ann".to_string())]);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("task #2 not stolen by ann")),
            "the refusal is said"
        );
        // Every try refused: no move, so the pass does not reload its work.
        let refused = attempt(&tries, &mut Vec::new(), |task, _| Err(format!("no {task}")));
        assert_eq!(refused, []);
    }

    #[test]
    fn work_stealing_is_off_only_with_zero() {
        assert!(enabled(None));
        assert!(enabled(Some("1")));
        assert!(!enabled(Some("0")));
        assert!(!enabled(Some(" 0 ")));
    }
}
