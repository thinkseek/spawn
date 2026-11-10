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
//! the store answers that pass with the set-aside it already made (the
//! message below names the task), so it is recorded and pushed then, and
//! never made twice.

use std::path::Path;
use std::sync::Arc;

use spawn_core::event::{ArtifactRef, CheckpointReason, EventKind, NewEvent, Source};
use spawn_core::id::{AgentId, BranchName, CommitId, SessionId};
use spawn_core::port::{EventLog, SetAside, WorkspaceStore};
use spawn_host::GitWorkspace;
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
             started from. None of it is in your workspace now; do not redo it."
        ),
        Done::Nothing => {
            format!("Task #{task} changed nothing in your workspace, so nothing was set aside.")
        }
        Done::NoBranch => String::new(),
    }
}

/// Set the work of `cancelled`'s task aside on its agent's branch under
/// `root` (once: a set-aside already recorded is only read back), record it
/// in the agent's session and push it before this returns.
pub(crate) fn set_aside(
    store: &Arc<PostgresStore>,
    root: &Path,
    cancelled: &Cancelled,
) -> Result<Done, String> {
    let agent = cancelled.agent_id.as_str();
    let source = match store.agent_source(agent) {
        Ok(Some(source)) => source,
        Ok(None) => return Ok(Done::NoBranch),
        Err(error) => return Err(format!("mission source of {agent}: {error}")),
    };
    let mirror =
        sources::ensure_mirror(root, source.as_deref()).map_err(|error| error.to_string())?;
    if let Some(commit) = &cancelled.set_aside {
        let path = mirror.to_string_lossy();
        let kept = sources::git(&["--git-dir", &path, "rev-parse", &format!("{commit}^1")])
            .map_err(|error| format!("the work under set-aside {commit}: {error}"))?;
        return Ok(Done::SetAside {
            commit: commit.clone(),
            kept,
        });
    }
    let factory: sources::Factory = {
        let root = root.to_path_buf();
        Arc::new(move |repo: &Path| GitWorkspace::new(repo, &root))
    };
    let workspaces = sources::MissionWorkspaces::one_shot(
        GitWorkspace::new(&mirror, root),
        root,
        Arc::clone(store),
        factory,
    );
    let branch = BranchName::new(agent).map_err(|error| error.to_string())?;
    let message = format!(
        "spawn: set aside the work of cancelled task #{} (this tree is the checkpoint it \
         started from; the work stays in the parent)",
        cancelled.id
    );
    let done = match workspaces
        .set_aside(&branch, cancelled.start.as_deref(), &message)
        .map_err(|error| format!("set aside the work of task #{}: {error}", cancelled.id))?
    {
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
        assert!(note(7, &Done::Nothing).contains("nothing was set aside"));
        assert!(note(7, &Done::NoBranch).is_empty());
    }
}
