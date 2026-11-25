//! A cancelled task's work set aside before its resident gets anything else
//! (docs/storage.md, "Work set aside").
//!
//! A resident keeps its branch from task to task, and the Bridge commits and
//! publishes every checkpoint. Without this, the work of a task cancelled
//! while it ran would stay in the workspace and on the branch, the next task
//! would start on top of it, and in a mission with a gate the next task's
//! candidate (the branch) would carry it into a landing under another
//! objective's name.
//!
//! So once such a resident no longer runs (the dispatcher stopped it, which
//! took a final checkpoint, [`crate::deliver`]), and before its notice is
//! sent, the dispatcher puts a new commit on top of the branch whose tree is
//! the checkpoint the task started from ([`WorkspaceStore::set_aside`]), under
//! the branch's lock. Nothing is rewound: pushes are never forced
//! ([`crate::push`]), and the integrator requires the published branch to
//! descend from the agent's latest checkpoint (`integrate::freshness`). The
//! work stays in the branch's history, in the set-aside commit's parent, and
//! the notice names both commits. The set-aside is recorded as the agent's
//! checkpoint (`artifact.committed`, reason `set_aside`, key
//! `set-aside:<task>`, so it is done once) and published like any.
//!
//! The branch moves before the record, as for every commit. When the append
//! fails the dispatcher holds the notice and tries again on a later pass:
//! the store answers that pass with the set-aside it already made while it
//! is still the branch head (the message below names the task), so it is
//! recorded and pushed then. Should a checkpoint land on top before that
//! pass (a late write of the stopped worker), the pass sets that aside too,
//! with a commit of its own, and records that one: the first stays in the
//! branch's history without a record.
//!
//! A file the checkpoints leave out (over the size cap, a special file) is
//! in no commit: the set-aside leaves it in the workspace, and the notice
//! says so ([`note`]).
//!
//! A resident stopped once for the task and revived since (GitHub #76) gets
//! its notice typed in while its engine runs, its turn stopped at its
//! prompt. Its work is then set aside only while it is still idle, asked
//! again under the branch's lock right before anything is written
//! ([`set_aside_when`]), the way main is merged before a typed delivery
//! ([`crate::main_merge::ready_to_merge`]): a resident that went busy keeps
//! its workspace as it is, and the notice waits for a later pass.

use std::path::Path;
use std::sync::Arc;

use spawn_core::event::{ArtifactRef, CheckpointReason, EventKind, NewEvent, Source};
use spawn_core::id::{AgentId, BranchName, CommitId, SessionId};
use spawn_core::port::{EventLog, SetAside, WorkspaceError, WorkspaceStore};
use spawn_host::{SET_ASIDE_FILES, SET_ASIDE_WAITS};
use spawn_postgres::PostgresStore;

use crate::deliver::Cancelled;
use crate::sources;

/// What setting a cancelled task's work aside did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Done {
    /// `commit` (recorded) has the start's tree; `kept` holds the work.
    SetAside { commit: String, kept: String },
    /// The branch already had the start's tree: the task changed nothing.
    Nothing,
    /// The agent has no branch on this host (no mission owns it).
    NoBranch,
}

/// The sentence the notice gets about the task's work.
pub(crate) fn note(task: i64, done: &Done) -> String {
    match done {
        Done::SetAside { commit, kept } => format!(
            "Its work was set aside, not lost: commit {kept} on your branch holds it, and \
             commit {commit} on top of it returned your workspace to the checkpoint task #{task} \
             started from. None of it is in your workspace now, except files a checkpoint \
             leaves out (over the size cap, or not regular files): those are on no branch and \
             stay where they are, and one that was in the way of a file of that checkpoint is \
             under `{SET_ASIDE_FILES}/{kept}/`. Do not redo the work."
        ),
        Done::Nothing => {
            format!("Task #{task} changed nothing a checkpoint holds, so nothing was set aside.")
        }
        Done::NoBranch => String::new(),
    }
}

/// Why task `task`'s cancel notice is held this pass: `why`, the reason its
/// resident is not idle ([`set_aside_when`]).
pub(crate) fn waits(task: i64, why: &str) -> String {
    format!("task #{task}'s cancel notice waits while {why}; tried again next pass")
}

/// Set the work of `cancelled`'s task aside on its agent's branch under
/// `root` (once: a set-aside already recorded is only read back), record it
/// in the agent's session and push it before this returns.
#[cfg(test)]
pub(crate) fn set_aside(
    store: &Arc<PostgresStore>,
    root: &Path,
    cancelled: &Cancelled,
) -> Result<Done, String> {
    set_aside_when(store, root, cancelled, &|| Ok(()))
}

/// Set the work of `cancelled`'s task aside as `set_aside` does, only while
/// `ready` says the resident is idle: asked under the branch's lock right
/// before the set-aside would write
/// ([`spawn_host::LocalWorkspaceStore::set_aside_when`]), and, where nothing is to be written
/// (no branch here, or the set-aside already recorded), before this
/// returns. When `ready` refuses, nothing changes and the error says the
/// notice waits ([`waits`]).
pub(crate) fn set_aside_when(
    store: &Arc<PostgresStore>,
    root: &Path,
    cancelled: &Cancelled,
    ready: &dyn Fn() -> Result<(), String>,
) -> Result<Done, String> {
    let agent = cancelled.agent_id.as_str();
    let source = match store.agent_source(agent) {
        Ok(Some(source)) => source,
        Ok(None) => {
            ready().map_err(|why| waits(cancelled.id, &why))?;
            return Ok(Done::NoBranch);
        }
        Err(error) => return Err(format!("mission source of {agent}: {error}")),
    };
    let mirror =
        sources::ensure_mirror(root, source.as_deref()).map_err(|error| error.to_string())?;
    if let Some(commit) = &cancelled.set_aside {
        ready().map_err(|why| waits(cancelled.id, &why))?;
        let path = mirror.to_string_lossy();
        let kept = sources::git(&["--git-dir", &path, "rev-parse", &format!("{commit}^1")])
            .map_err(|error| format!("the work under set-aside {commit}: {error}"))?;
        return Ok(Done::SetAside {
            commit: commit.clone(),
            kept,
        });
    }
    let workspaces = crate::config::WorkspaceSettings::from_env(root)?.one_shot(
        &mirror,
        root,
        Arc::clone(store),
    );
    let branch = BranchName::new(agent).map_err(|error| error.to_string())?;
    let start = start_after_rewrite(store, &mirror, cancelled)?;
    let message = format!(
        "spawn: set aside the work of cancelled task #{} (this tree is the checkpoint it \
         started from; the work stays in the parent)",
        cancelled.id
    );
    let done = match workspaces
        .set_aside_when(&branch, start.as_deref(), &message, ready)
        .map_err(|error| match error {
            WorkspaceError::Refused { code, message } if code == SET_ASIDE_WAITS => {
                waits(cancelled.id, &message)
            }
            error => format!("set aside the work of task #{}: {error}", cancelled.id),
        })? {
        SetAside::Unchanged => Done::Nothing,
        // Made now, or by a pass whose record failed.
        SetAside::Created {
            commit,
            kept,
            files,
        } => {
            // Recorded before the push: the published head has its record.
            record(
                store,
                workspaces.project(),
                cancelled,
                &branch,
                &commit,
                files,
            )?;
            Done::SetAside {
                commit: commit.as_str().to_string(),
                kept: kept.as_str().to_string(),
            }
        }
    };
    // Pushed now, through the process's pusher for the source; a push that
    // fails stays queued there and is retried.
    if matches!(done, Done::SetAside { .. })
        && let Some(source) = source.as_deref()
        && let Some(crate::push::Pushed::Failed(why) | crate::push::Pushed::Rejected(why)) =
            sources::publish(&mirror, source, agent)
    {
        eprintln!("set aside the work of task #{}: push: {why}", cancelled.id);
    }
    Ok(done)
}

/// The start `cancelled`'s work is set aside to: its recorded start, unless
/// that is from before a history rewrite ([`crate::rewrite_guard::why_old`],
/// objective 177): then the refusal is recorded once (`set-aside`) and the
/// work is set aside to the branch's base with main instead, so no old
/// content is written into the branch.
fn start_after_rewrite(
    store: &PostgresStore,
    mirror: &Path,
    cancelled: &Cancelled,
) -> Result<Option<String>, String> {
    let Some(start) = cancelled.start.as_deref() else {
        return Ok(None);
    };
    let path = mirror.to_string_lossy();
    let Some(reason) = crate::rewrite_guard::why_old(store, &path, start)? else {
        return Ok(Some(start.to_string()));
    };
    crate::rewrite_guard::refused(store, "set-aside", cancelled.id, start, &reason)?;
    let base = sources::git(&[
        "--git-dir",
        &path,
        "merge-base",
        "refs/heads/main",
        &format!("refs/heads/{}", cancelled.agent_id),
    ])
    .map_err(|error| format!("the base with main of {}: {error}", cancelled.agent_id))?;
    eprintln!(
        "set aside the work of task #{}: {reason}; set aside to the branch's base with main \
         {base}",
        cancelled.id
    );
    Ok(Some(base))
}

/// The set-aside as the agent's checkpoint, once per task.
fn record(
    store: &PostgresStore,
    project: &str,
    cancelled: &Cancelled,
    branch: &BranchName,
    commit: &CommitId,
    files: Vec<String>,
) -> Result<(), String> {
    store
        .append(NewEvent {
            agent_id: AgentId::new(cancelled.agent_id.as_str()).map_err(|e| e.to_string())?,
            session_id: SessionId::new(cancelled.session_id.as_str()).map_err(|e| e.to_string())?,
            execution_id: None,
            source: Source::Bridge,
            source_event_id: Some(format!("set-aside:{}", cancelled.id)),
            kind: EventKind::ArtifactCommitted {
                reason: CheckpointReason::SetAside,
                artifact: ArtifactRef {
                    project: project.to_string(),
                    branch: branch.clone(),
                    commit: commit.clone(),
                },
                files,
            },
        })
        .map(|_| ())
        .map_err(|error| format!("record the set-aside of task #{}: {error}", cancelled.id))
}

#[cfg(test)]
mod tests {
    use super::{Done, note};

    #[test]
    fn the_note_says_where_the_work_is() {
        let text = note(
            7,
            &Done::SetAside {
                commit: "c0ffee".to_string(),
                kept: "beef".to_string(),
            },
        );
        assert!(text.contains("beef"), "{text}");
        assert!(text.contains("c0ffee"), "{text}");
        assert!(text.contains("task #7"), "{text}");
        assert!(text.contains(".spawn/set-aside/beef/"), "{text}");
        assert!(note(7, &Done::Nothing).contains("nothing was set aside"));
        assert!(note(7, &Done::NoBranch).is_empty());
    }
}
