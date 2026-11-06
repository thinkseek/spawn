//! The source's main merged into an agent's mounted, idle workspace, with
//! conflict markers when the two conflict (docs/storage.md, "Main into a
//! workspace").
//!
//! The Bridge merges main into a branch only when an execution starts or
//! resumes, and only when it merges cleanly ([`spawn_core::bridge`]). A
//! long-lived worker never restarts, so its branch would never get main, and
//! every piece of its work after main moved would be judged `conflict` with
//! nothing in the workspace to resolve. Two moments merge main into it
//! instead, both while the agent is idle at its prompt:
//!
//! - the integrator's `conflict` verdict ([`crate::integrate`]), before the
//!   verdict is recorded, so the note sent back names the files that now
//!   carry markers;
//! - the delivery of a task typed into a live worker
//!   ([`crate::dispatch::step`]), so the task starts from main and its
//!   brief says what the merge did.
//!
//! Both go through [`WorkspaceStore::merge_base_when`] of the mission's
//! store ([`sources::MissionWorkspaces`], which refreshes main from the
//! source): it takes the branch's lock, so it cannot race a checkpoint, and
//! it writes markers only where a content conflict shows them, leaving
//! `main` pending under `.merge/<agent>` as the second parent of the
//! checkpoint that resolves them. A clean merge is a `base_merged`
//! checkpoint, recorded, then pushed to the source before [`merge_main`]
//! returns (a push that fails stays queued on the process's pusher for the
//! source, which retries it); a conflict is recorded as `merge.conflicted`.
//!
//! **Only while the agent is idle.** Both moments are chosen while the
//! worker waits, but on facts read earlier: a human's input may have
//! started a turn since. So the store asks again, under the branch's lock,
//! after the fetch of main and right before it writes anything, on facts
//! read then ([`may_merge`], the rule a revision is typed in by). The Bridge
//! types under the same lock ([`WorkspaceStore::hold`]), so a turn typed in
//! meanwhile is seen. A worker that is not idle keeps its workspace as it
//! is: the merge is [`Merge::Busy`] and waits. The integrator's verdict then
//! says main is merged when the revision is typed in, and the dispatcher
//! merges it then; a delivery whose merge is busy is not sent this pass, and
//! is tried again on the next.
//!
//! A conflict markers cannot show (a file deleted on one side and changed on
//! the other, a rename, a binary file) changes nothing in the workspace but
//! a copy of main's version of each such file under [`MAIN_COPIES`], so the
//! worker, which cannot run git, has what it needs to resolve it. `.spawn/`
//! is never committed, and a mount exports the workspace afresh: the store
//! records the copies beside the workspace and every mount puts them back
//! while the conflict is open, so the note that names them still holds
//! after a resume.

use std::path::Path;
use std::sync::Arc;

use spawn_core::event::{ArtifactRef, CheckpointReason, EventKind, NewEvent, Source};
use spawn_core::id::{AgentId, BranchName, CommitId, SessionId, WorkspaceId};
use spawn_core::port::{BaseMerge, EventLog, MERGE_WAITS, WorkspaceError, WorkspaceStore};
use spawn_host::GitWorkspace;
use spawn_postgres::PostgresStore;

use crate::dispatch::Facts;
use crate::push::Pushed;
use crate::sources;

/// Where main's version of a file that conflicts without markers is copied,
/// relative to the workspace (`.spawn/` is never committed; every mount puts
/// the copies back while the conflict is open).
pub(crate) use spawn_host::MAIN_COPIES;

/// What merging main into an agent's workspace did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Merge {
    /// The branch already contains main (or a pending merge's markers are
    /// all resolved: the next checkpoint records it).
    Current,
    /// Main merged cleanly: the branch is at the merge `commit`; `files`
    /// changed in the workspace.
    Merged { commit: String, files: Vec<String> },
    /// Main conflicts with the branch, and `files` carry conflict markers in
    /// the workspace (written now, or by an earlier merge not resolved yet).
    Marked { files: Vec<String> },
    /// Main conflicts with the branch in `files` in a way markers cannot
    /// show; nothing in the workspace changed. `copied`: those of them main
    /// has, copied to [`MAIN_COPIES`]; the others main does not have.
    Unmarked {
        files: Vec<String>,
        copied: Vec<String>,
    },
    /// The agent has no workspace mounted under this root (or no mission
    /// with a source): nothing was merged.
    NotMounted,
    /// The agent was not idle (why): nothing was merged, and the merge
    /// waits for a moment it is ([`may_merge`]).
    Busy(String),
    /// The merge did not happen; why.
    Failed(String),
}

/// The workspace of `agent` mounted under `root`, if there is one.
fn mounted(root: &Path, agent: &str) -> Option<WorkspaceId> {
    let directory = root.join(agent);
    if !directory.is_dir() {
        return None;
    }
    WorkspaceId::new(format!("{agent}:{}", directory.display())).ok()
}

/// Merge main into the workspace of `agent` mounted under `root`, through
/// `workspaces` (whose `main` is the one in `mirror`). The git half of
/// [`merge_main`], without the database.
#[cfg(test)]
pub(crate) fn into_workspace(
    workspaces: &dyn WorkspaceStore,
    mirror: &Path,
    root: &Path,
    agent: &str,
) -> Merge {
    into_workspace_when(workspaces, mirror, root, agent, &|| Ok(()))
}

/// [`into_workspace`], only while `ready` says the agent is idle, asked by
/// the store right before it would write ([`WorkspaceStore::merge_base_when`]).
pub(crate) fn into_workspace_when(
    workspaces: &dyn WorkspaceStore,
    mirror: &Path,
    root: &Path,
    agent: &str,
    ready: &dyn Fn() -> Result<(), String>,
) -> Merge {
    let Some(workspace) = mounted(root, agent) else {
        return Merge::NotMounted;
    };
    match workspaces.merge_base_when(&workspace, ready) {
        Ok(BaseMerge::Current) => Merge::Current,
        Ok(BaseMerge::Merged { commit, files }) => Merge::Merged {
            commit: commit.as_str().to_string(),
            files,
        },
        Ok(BaseMerge::Conflicted {
            files,
            marked: true,
        }) => Merge::Marked { files },
        Ok(BaseMerge::Conflicted {
            files,
            marked: false,
        }) => {
            // The store copied main's versions and recorded which.
            let copied = GitWorkspace::new(mirror, root)
                .main_copies(agent)
                .map(|(_, copied)| copied)
                .unwrap_or_default();
            Merge::Unmarked { files, copied }
        }
        Err(WorkspaceError::Refused { code, message }) if code == MERGE_WAITS => {
            Merge::Busy(message)
        }
        Err(error) => Merge::Failed(error.to_string()),
    }
}

/// Whether main may be merged into the workspace of the session `facts`
/// describe now, or why not: no command in flight in the session (none
/// being typed, no start, resume or stop under way), and a worker that runs
/// sits idle at its prompt, its turn stopped, judged on its latest fact
/// ([`Facts::turn_stopped_idle`], what a revision waits for before it is
/// typed in). A session with no execution running has no turn to disturb.
/// A session the facts do not list is not known to be idle.
pub(crate) fn may_merge(facts: Option<&Facts>) -> Result<(), String> {
    let Some(facts) = facts else {
        return Err("its session has no facts yet".to_string());
    };
    if facts.open_command {
        return Err("a command is in flight in its session".to_string());
    }
    if facts.stopping() {
        return Err("its worker's stop is under way".to_string());
    }
    if facts.live && !facts.turn_stopped_idle() {
        return Err("its worker is not idle at its prompt (a turn may be running)".to_string());
    }
    Ok(())
}

/// [`may_merge`] for `session`, on the dispatcher's facts read now, not the
/// ones a pass started with.
fn ready_to_merge(store: &PostgresStore, session: &str) -> Result<(), String> {
    let rows: Vec<Facts> = serde_json::from_value(
        store
            .dispatch_facts()
            .map_err(|error| format!("its session's facts could not be read: {error}"))?,
    )
    .map_err(|error| format!("its session's facts could not be read: {error}"))?;
    may_merge(rows.iter().find(|facts| facts.session_id == session))
}

/// Merge main into the mounted workspace of the mission agent `agent`
/// (session `session`) under `root`, through its mission's store, only while
/// the agent is idle ([`may_merge`], asked under the branch's lock), and
/// record what happened in the agent's session: a clean merge as a
/// `base_merged` `artifact.committed`, a conflict as `merge.conflicted`. A
/// merge commit is pushed before this returns; a push that failed is not
/// dropped but queued on the process's pusher for the source, which retries
/// it, and the integrator pushes a branch whose recorded checkpoint the
/// source lacks again ([`sources::push_recorded`]).
pub(crate) fn merge_main(
    store: &Arc<PostgresStore>,
    root: &Path,
    agent: &str,
    session: &str,
) -> Merge {
    let source = match store.agent_source(agent) {
        Ok(Some(Some(source))) => source,
        Ok(_) => return Merge::NotMounted,
        Err(error) => return Merge::Failed(format!("mission source of {agent}: {error}")),
    };
    if mounted(root, agent).is_none() {
        return Merge::NotMounted;
    }
    let mirror = match sources::ensure_mirror(root, Some(&source)) {
        Ok(mirror) => mirror,
        Err(error) => return Merge::Failed(error.to_string()),
    };
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
    let project = workspaces.project().to_string();
    // A merge whose record failed (here, or the Bridge's at a start) is
    // recorded from the branch now; one already recorded is a no-op.
    let earlier = head_merge(&mirror, agent);
    if let Some(earlier) = &earlier
        && let Err(error) = record(store, &project, agent, session, "-", earlier)
    {
        eprintln!("merge main into {agent}: record the earlier merge: {error}");
    }
    let ready = || ready_to_merge(store, session);
    let merge = into_workspace_when(&workspaces, &mirror, root, agent, &ready);
    let head = GitWorkspace::new(&mirror, root)
        .commit_id_of(&format!("refs/heads/{agent}"))
        .unwrap_or_else(|_| "-".to_string());
    // Recorded before the push: the published head has its record.
    if let Err(error) = record(store, &project, agent, session, &head, &merge) {
        eprintln!("merge main into {agent}: record (retried at its next merge): {error}");
    }
    if matches!(merge, Merge::Merged { .. }) || earlier.is_some() {
        publish_merge(&mirror, &source, agent, &head);
    }
    merge
}

/// Push the merge commit `head` of `agent`'s branch now, and say so when it
/// did not reach the source: a failed push stays queued and is retried
/// ([`sources::publish`]).
fn publish_merge(mirror: &Path, source: &str, agent: &str, head: &str) {
    match sources::publish(mirror, source, agent) {
        None | Some(Pushed::Done) => {}
        Some(Pushed::Failed(why)) => eprintln!(
            "merge main into {agent}: the push of {head} failed; it stays queued and is \
             retried: {why}"
        ),
        Some(Pushed::Rejected(why)) => {
            eprintln!("merge main into {agent}: the source refused {head}: {why}");
        }
    }
}

/// The head of `agent`'s branch in `mirror` as a clean merge of main, when it
/// is one (two parents, made by [`WorkspaceStore::merge_base`] or the
/// Bridge's start merge: "spawn: merge main …"): the commit and the files it
/// changed against the branch's own parent.
pub(crate) fn head_merge(mirror: &Path, agent: &str) -> Option<Merge> {
    let path = mirror.to_string_lossy();
    let shown = sources::git(&[
        "--git-dir",
        &path,
        "log",
        "-1",
        "--format=%H%n%P%n%s",
        &format!("refs/heads/{agent}"),
        "--",
    ])
    .ok()?;
    let mut lines = shown.lines();
    let (commit, parents, subject) = (lines.next()?, lines.next()?, lines.next()?);
    let parents: Vec<&str> = parents.split_whitespace().collect();
    if parents.len() != 2 || !subject.starts_with("spawn: merge main ") {
        return None;
    }
    let files = sources::git(&[
        "--git-dir",
        &path,
        "diff",
        "--name-only",
        "--no-renames",
        parents[0],
        commit,
    ])
    .ok()?;
    Some(Merge::Merged {
        commit: commit.to_string(),
        files: files.lines().map(str::to_string).collect(),
    })
}

/// The event that records `merge` in the agent's session, if it changed or
/// found anything. The keys make a repeat of the same merge or the same
/// conflict (the next delivery finding it pending) one record.
fn record(
    store: &PostgresStore,
    project: &str,
    agent: &str,
    session: &str,
    head: &str,
    merge: &Merge,
) -> Result<(), String> {
    let digest = |files: &[String]| sources::fnv(&files.join("\0"));
    let (key, kind) = match merge {
        Merge::Merged { commit, files } => (
            format!("commit:{commit}"),
            EventKind::ArtifactCommitted {
                reason: CheckpointReason::BaseMerged,
                artifact: ArtifactRef {
                    project: project.to_string(),
                    branch: BranchName::new(agent).map_err(|error| error.to_string())?,
                    commit: CommitId::new(commit.clone()).map_err(|error| error.to_string())?,
                },
                files: files.clone(),
            },
        ),
        Merge::Marked { files } => (
            format!("conflict-marked:{head}:{:016x}", digest(files)),
            EventKind::MergeConflicted {
                files: files.clone(),
                marked: true,
            },
        ),
        Merge::Unmarked { files, .. } => (
            format!("conflict:{head}:{:016x}", digest(files)),
            EventKind::MergeConflicted {
                files: files.clone(),
                marked: false,
            },
        ),
        Merge::Current | Merge::NotMounted | Merge::Busy(_) | Merge::Failed(_) => return Ok(()),
    };
    store
        .append(NewEvent {
            agent_id: AgentId::new(agent).map_err(|error| error.to_string())?,
            session_id: SessionId::new(session).map_err(|error| error.to_string())?,
            execution_id: None,
            source: Source::Bridge,
            source_event_id: Some(key),
            kind,
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// How to resolve a conflict markers cannot show, for a worker that cannot
/// run git.
fn unmarked_instructions(files: &[String], copied: &[String]) -> String {
    let missing: Vec<&str> = files
        .iter()
        .filter(|file| !copied.contains(file))
        .map(String::as_str)
        .collect();
    let mut steps = Vec::new();
    if !copied.is_empty() {
        steps.push(format!(
            "make each of {} exactly main's version, which is copied to `{MAIN_COPIES}/<the same \
             path>` (copy it over yours)",
            copied.join(", ")
        ));
    }
    if !missing.is_empty() {
        steps.push(format!(
            "delete {} from your workspace: main does not have {}",
            missing.join(", "),
            if missing.len() == 1 { "it" } else { "them" }
        ));
    }
    format!(
        "These files conflict in a way conflict markers cannot show (a file deleted on one side \
         and changed on the other, a rename, or a binary file): {}. Nothing in your workspace was \
         changed. To resolve it: {}. Your own change to such a file is then lost: redo it on top \
         of main's version for a text file, or put it in a new file.",
        files.join(", "),
        steps.join("; then ")
    )
}

/// How to resolve markers.
const RESOLVE_MARKERS: &str = "Resolve them by editing the marked regions (<<<<<<< / ======= / \
     >>>>>>>), keeping both sides' intent and removing every marker; do not rewrite the files \
     from memory. The checkpoint after the last marker is gone records the merge, with main as \
     its second parent.";

/// The note a `conflict` verdict sends back, after `merge` merged `main`
/// into the workspace.
pub(crate) fn conflict_note(main: &str, merge: &Merge) -> String {
    let opening = format!("Your work conflicts with the source's current main ({main}).");
    match merge {
        Merge::Marked { files } => format!(
            "{opening} Main is now merged into your workspace, and these files carry conflict \
             markers: {}. {RESOLVE_MARKERS} Then report again.",
            files.join(", ")
        ),
        Merge::Unmarked { files, copied } => format!(
            "{opening} {} Then report again.",
            unmarked_instructions(files, copied)
        ),
        Merge::Merged { files, .. } => format!(
            "{opening} Main has now been merged into your workspace without a conflict (it \
             changed {}). Check that your work still holds with those files, then report again.",
            shown(files)
        ),
        Merge::Current => format!(
            "{opening} Your workspace already holds main (any markers there are resolved). \
             Report again: your next checkpoint records the merge."
        ),
        Merge::NotMounted => format!(
            "{opening} Your workspace is not mounted on the integrator's host, so main could not \
             be merged into it. Once it is mounted again (your worker resumed), main is merged \
             into it if it merges cleanly; if not, your next report is sent back again with main \
             merged in and the conflicting regions marked, for you to resolve."
        ),
        Merge::Busy(why) => format!(
            "{opening} Main could not be merged into your workspace when your work was judged \
             ({why}): nothing in it was changed. Main is merged into it right before this note is \
             typed in, and what that merge did is said after this note. If your worker is \
             resumed instead, main is merged into it if it merges cleanly; if not, your next \
             report is sent back again with main merged in and the conflicting regions marked, \
             for you to resolve."
        ),
        Merge::Failed(error) => format!(
            "{opening} Merging main into your workspace failed ({error}). Report again: the next \
             verdict tries the merge again."
        ),
    }
}

/// What a task's brief says about `merge`, done right before the task was
/// typed in, and about the clean merges recorded since the worker was last
/// typed a delivery (`untold`: the files they changed,
/// [`PostgresStore::untold_merges`]). A delivery refused before typing
/// (a pane not ready) is retried, and the retry's merge finds main already
/// in: what the first one merged is told from its record. Nothing when no
/// merge is untold.
pub(crate) fn delivery_note(merge: &Merge, untold: &[String]) -> Option<String> {
    let opening = "Before this task, the source's main was merged into your workspace";
    let clean = |files: &[String]| {
        let mut files: Vec<String> = untold.iter().chain(files).cloned().collect();
        files.sort();
        files.dedup();
        format!(
            "{opening}, without a conflict; it changed {}. Re-read those files before editing.",
            shown(&files)
        )
    };
    match merge {
        Merge::Merged { files, .. } => Some(clean(files)),
        Merge::Marked { files } => Some(format!(
            "{opening}, and these files carry conflict markers: {}. {RESOLVE_MARKERS} Resolve \
             them first.",
            files.join(", ")
        )),
        Merge::Unmarked { files, copied } => Some(format!(
            "{opening}, but it conflicts with your branch. {} Do that first.",
            unmarked_instructions(files, copied)
        )),
        Merge::Current | Merge::NotMounted | Merge::Busy(_) | Merge::Failed(_) => {
            (!untold.is_empty()).then(|| clean(&[]))
        }
    }
}

fn shown(files: &[String]) -> String {
    if files.is_empty() {
        "no file".to_string()
    } else {
        files.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::{Merge, conflict_note};

    /// Audit #22: a conflict judged while the workspace is not mounted is
    /// not left to a person: once mounted, the next report's `conflict`
    /// verdict merges main with markers, and the worker resolves them.
    #[test]
    fn a_conflict_while_not_mounted_comes_back_marked_for_the_worker() {
        let note = conflict_note("abc123", &Merge::NotMounted);
        assert!(!note.contains("a person"), "{note}");
        assert!(note.contains("marked, for you to resolve"), "{note}");
    }
}
