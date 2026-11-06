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
    use super::{Merge, conflict_note, delivery_note, into_workspace, into_workspace_when};
    use crate::push::{Pushed, Pusher, push_branch};
    use crate::sources::{ensure_mirror, fetch_pushed_into, git};
    use spawn_core::id::{BranchName, WorkspaceId};
    use spawn_core::port::{Commit, WorkspaceStore};
    use spawn_host::GitWorkspace;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// Audit #22: a conflict judged while the workspace is not mounted is
    /// not left to a person: once mounted, the next report's `conflict`
    /// verdict merges main with markers, and the worker resolves them.
    #[test]
    fn a_conflict_while_not_mounted_comes_back_marked_for_the_worker() {
        let note = conflict_note("abc123", &Merge::NotMounted);
        assert!(!note.contains("a person"), "{note}");
        assert!(note.contains("marked, for you to resolve"), "{note}");
    }

    fn commit_in(directory: &Path, message: &str) {
        let directory = directory.to_string_lossy().to_string();
        git(&["-C", &directory, "add", "-A"]).expect("add");
        git(&[
            "-C",
            &directory,
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--no-gpg-sign",
            "-qm",
            message,
        ])
        .expect("commit");
    }

    /// A bare source with README.md and notes.txt on main, this host's
    /// mirror of it under the workspace root, and the store over the
    /// mirror: `(base, source, root, mirror, store)`.
    fn world(tag: &str) -> (PathBuf, String, PathBuf, PathBuf, GitWorkspace) {
        let base = std::env::temp_dir().join(format!("spawn-merge-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed");
        git(&["init", "-q", "--initial-branch=main", &seed.to_string_lossy()]).expect("init");
        std::fs::write(seed.join("README.md"), "one\ntwo\nthree\n").expect("write");
        std::fs::write(seed.join("notes.txt"), "notes\n").expect("write");
        commit_in(&seed, "base");
        let source = base.join("source.git").to_string_lossy().to_string();
        git(&["clone", "-q", "--bare", &seed.to_string_lossy(), &source]).expect("source");
        let root = base.join("host");
        let mirror = ensure_mirror(&root, Some(&source)).expect("mirror");
        let store = GitWorkspace::new(&mirror, &root);
        (base, source, root, mirror, store)
    }

    /// Main moves on the source by `change`, and the mirror follows; main's
    /// new commit.
    fn main_moves(base: &Path, source: &str, mirror: &Path, change: impl Fn(&Path)) -> String {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let clone = base.join(format!("clone-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        let clone_path = clone.to_string_lossy().to_string();
        git(&["clone", "-q", source, &clone_path]).expect("clone");
        change(&clone);
        commit_in(&clone, "main moves");
        git(&["-C", &clone_path, "push", "-q", "origin", "main"]).expect("push main");
        crate::sources::refresh(mirror, source).expect("refresh");
        git(&["--git-dir", &mirror.to_string_lossy(), "rev-parse", "refs/heads/main"])
            .expect("main")
    }

    /// `agent`'s branch with one checkpoint by `work`, mounted.
    fn agent_with_work(store: &GitWorkspace, agent: &str, work: impl Fn(&Path)) -> WorkspaceId {
        let branch = BranchName::new(agent).expect("valid branch");
        store.ensure_branch(&branch, None).expect("branch");
        let workspace = store.mount(&branch).expect("mount");
        work(Path::new(workspace.path()));
        assert!(matches!(
            store.commit(&workspace, "work").expect("commit"),
            Commit::Created(_)
        ));
        workspace
    }

    fn head(mirror: &Path, agent: &str) -> String {
        git(&[
            "--git-dir",
            &mirror.to_string_lossy(),
            "rev-parse",
            &format!("refs/heads/{agent}"),
        ])
        .expect("head")
    }

    /// Facts of a live worker idle at its prompt: its turn stopped, judged
    /// on its latest fact, nothing in flight.
    fn idle_worker() -> crate::dispatch::Facts {
        use spawn_core::event::SessionSeq;
        use spawn_core::observation::{Ambiguity, Confidence, Observation, SemanticState};
        let mut facts: crate::dispatch::Facts = serde_json::from_value(serde_json::json!({
            "session_id": "session_worker_a",
            "agent_id": "agent_worker_a",
            "live": true,
            "execution_id": "exec_5",
            "evidence_seq": 9,
            "turn_ended": true,
        }))
        .expect("facts");
        let mut observation = Observation::basic(
            SemanticState::Waiting,
            None,
            SessionSeq(9),
            Confidence::new(0.9).expect("valid"),
            "jev",
        );
        observation.ambiguity = Some(Ambiguity::TurnStoppedIdle);
        facts.observation = Some(observation);
        facts
    }

    /// Issue #7: main is merged into a workspace only while its agent is
    /// idle, by the rule a revision is typed in: nothing in flight in the
    /// session, no stop under way, and a live worker stopped at its prompt
    /// on its latest fact. A session with no execution has no turn.
    #[test]
    fn main_is_merged_only_into_an_idle_worker() {
        use super::may_merge;
        let idle = idle_worker();
        assert_eq!(may_merge(Some(&idle)), Ok(()));

        let mut typing = idle_worker();
        typing.open_command = true;
        assert!(may_merge(Some(&typing)).is_err(), "an input in flight");

        let mut working = idle_worker();
        working.evidence_seq = Some(12);
        assert!(
            may_merge(Some(&working)).is_err(),
            "a fact newer than the idle judgement: a turn may be running"
        );

        let mut running = idle_worker();
        if let Some(observation) = running.observation.as_mut() {
            observation.ambiguity = None;
            observation.state = spawn_core::observation::SemanticState::Working;
        }
        assert!(may_merge(Some(&running)).is_err(), "judged working");

        let mut stopping = idle_worker();
        stopping.stops = vec![crate::dispatch::StopSent {
            id: 3,
            key: "retire-x".to_string(),
            principal: "spawn-dispatch".to_string(),
            action: "stop".to_string(),
            created_at_ms: 1,
            finished_at_ms: Some(2),
            succeeded: Some(false),
            error: Some("pane gone".to_string()),
        }];
        assert!(may_merge(Some(&stopping)).is_err(), "a stop under way");

        let mut ended = idle_worker();
        ended.live = false;
        ended.observation = None;
        assert_eq!(may_merge(Some(&ended)), Ok(()), "no execution runs");
        ended.open_command = true;
        assert!(may_merge(Some(&ended)).is_err(), "a resume in flight");

        assert!(may_merge(None).is_err(), "no facts: not known idle");
    }

    /// Issue #7: a merge whose agent is found busy right before it would
    /// write changes nothing (no merge commit, no file of main's), and the
    /// conflict note says main comes with the revision; once the agent is
    /// idle, the same call merges.
    #[test]
    fn a_merge_found_busy_changes_nothing_and_says_when_main_comes() {
        let (base, source, root, mirror, store) = world("busy");
        let agent = "agent_busy";
        agent_with_work(&store, agent, |directory| {
            std::fs::write(directory.join("work.txt"), "mine\n").expect("write");
        });
        let before = head(&mirror, agent);
        let main = main_moves(&base, &source, &mirror, |clone| {
            std::fs::write(clone.join("added.txt"), "from main\n").expect("write");
        });
        let merge = into_workspace_when(&store, &mirror, &root, agent, &|| {
            Err("its worker is not idle at its prompt (a turn may be running)".to_string())
        });
        let Merge::Busy(why) = &merge else {
            panic!("the merge waits: {merge:?}");
        };
        assert!(why.contains("not idle"), "{why}");
        assert_eq!(head(&mirror, agent), before, "no merge commit");
        assert!(!root.join(agent).join("added.txt").exists());
        let note = conflict_note(&main, &merge);
        assert!(
            note.contains("nothing in it was changed")
                && note.contains("right before this note is typed in")
                && note.contains("not idle"),
            "{note}"
        );
        assert_eq!(delivery_note(&merge, &[]), None);

        // Idle now: the merge happens.
        assert!(matches!(
            into_workspace_when(&store, &mirror, &root, agent, &|| Ok(())),
            Merge::Merged { .. }
        ));
        assert!(root.join(agent).join("added.txt").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Issue #8: the push of a merge commit fails once (a ref lock lost to
    /// another writer, a source briefly away); the commit still reaches the
    /// source, where the integrator fetches it: the failed push stayed
    /// queued and was retried, where the one-shot flush used to drop it and
    /// leave the task `stale` forever.
    #[test]
    fn a_merge_whose_push_fails_once_reaches_the_source_on_the_retry() {
        let (base, source, root, mirror, store) = world("push-fails");
        let agent = "agent_push";
        agent_with_work(&store, agent, |directory| {
            std::fs::write(directory.join("work.txt"), "mine\n").expect("write");
        });
        assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
        main_moves(&base, &source, &mirror, |clone| {
            std::fs::write(clone.join("added.txt"), "from main\n").expect("write");
        });
        let Merge::Merged { commit, .. } = into_workspace(&store, &mirror, &root, agent) else {
            panic!("a clean merge");
        };
        let failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let once = std::sync::Arc::clone(&failed);
        let pusher = Pusher::with(
            Box::new(move |mirror: &Path, source: &str, branch: &str| {
                if once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    push_branch(mirror, source, branch)
                } else {
                    Pushed::Failed("cannot lock ref 'refs/heads/spawn/agent_push'".to_string())
                }
            }),
            Duration::from_millis(1),
            Duration::from_millis(20),
        );
        assert!(matches!(
            crate::sources::publish_with(&pusher, &mirror, &source, agent),
            Some(Pushed::Failed(_))
        ));
        let landing = format!("refs/spawn-integrate/{agent}");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut pushed = None;
        while Instant::now() < deadline {
            pushed = fetch_pushed_into(&mirror, &source, agent, &landing).expect("fetch");
            if pushed.as_deref() == Some(commit.as_str()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(pushed, Some(commit), "the retry published the merge");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Issue #8: a recorded checkpoint the source lacks (its push lost with
    /// the process that queued it) is pushed again by the integrator's stale
    /// check; a head past the record is not.
    #[test]
    fn a_recorded_checkpoint_the_source_lacks_is_pushed_again() {
        let (base, source, _root, mirror, store) = world("push-again");
        let agent = "agent_again";
        agent_with_work(&store, agent, |directory| {
            std::fs::write(directory.join("work.txt"), "mine\n").expect("write");
        });
        let recorded = head(&mirror, agent);
        let landing = format!("refs/spawn-integrate/{agent}");
        assert_eq!(fetch_pushed_into(&mirror, &source, agent, &landing).expect("fetch"), None);
        assert_eq!(
            crate::sources::push_recorded(&mirror, &source, agent, "0123abcd"),
            None,
            "a head that is not the record is left to its own push"
        );
        assert_eq!(
            crate::sources::push_recorded(&mirror, &source, agent, &recorded),
            Some(Pushed::Done)
        );
        assert_eq!(
            fetch_pushed_into(&mirror, &source, agent, &landing).expect("fetch"),
            Some(recorded)
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Issue #11, at the integrator's verdict: the note for a conflict
    /// markers cannot show promises main's copies, and they are there again
    /// after the workspace was unmounted and a resume mounted it afresh
    /// (the Bridge's order: a clean-only merge, then the mount).
    #[test]
    fn the_unmarked_note_holds_after_a_resume_remounts_the_workspace() {
        let (base, source, root, mirror, store) = world("unmarked-resume");
        let agent = "agent_unmarked";
        let workspace = agent_with_work(&store, agent, |directory| {
            std::fs::remove_file(directory.join("notes.txt")).expect("delete");
        });
        let main = main_moves(&base, &source, &mirror, |clone| {
            std::fs::write(clone.join("notes.txt"), "notes, revised\n").expect("write");
        });
        let merge = into_workspace(&store, &mirror, &root, agent);
        assert_eq!(
            merge,
            Merge::Unmarked {
                files: vec!["notes.txt".to_string()],
                copied: vec!["notes.txt".to_string()],
            }
        );
        let note = conflict_note(&main, &merge);
        assert!(note.contains(".spawn/main"), "{note}");
        store.unmount(&workspace).expect("the engine is lost");
        let branch = BranchName::new(agent).expect("valid branch");
        store.merge_main_clean(&branch).expect("the resume's merge");
        store.mount(&branch).expect("the resume's mount");
        assert_eq!(
            std::fs::read_to_string(root.join(agent).join(super::MAIN_COPIES).join("notes.txt"))
                .expect("the copy the note names"),
            "notes, revised\n"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A schema of its own in the test database, dropped afterwards.
    struct Db {
        store: std::sync::Arc<spawn_postgres::PostgresStore>,
        url: String,
        schema: String,
    }

    impl Db {
        fn new(case: &str) -> Self {
            let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
                "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
            );
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.subsec_nanos());
            let schema = format!("spawn_test_{}_{nanos:x}_{case}", std::process::id());
            let store =
                spawn_postgres::PostgresStore::connect_in_schema(&url, &schema).expect("connect");
            Self {
                store: std::sync::Arc::new(store),
                url,
                schema,
            }
        }

        /// A task on a mission whose source is `source`: its agent and
        /// session, with one event, so the session has facts.
        fn task_on(&self, source: &str, label: &str) -> (String, String) {
            use spawn_core::event::{EventKind, NewEvent, Source};
            use spawn_core::id::{AgentId, SessionId};
            use spawn_core::port::EventLog;
            let mission = self
                .store
                .mission_add(label, &serde_json::json!([source]), "human")
                .expect("mission");
            let task = self
                .store
                .task_assign(mission, "ephemeral", label, "brief", "default")
                .expect("task");
            let mut client =
                postgres::Client::connect(&self.url, postgres::NoTls).expect("connection");
            client
                .batch_execute(&format!("set search_path to {}", self.schema))
                .expect("schema");
            let row = client
                .query_one(
                    "select agent_id, session_id from mission_tasks where id = $1",
                    &[&task],
                )
                .expect("the task");
            let (agent, session): (String, String) = (row.get(0), row.get(1));
            self.store
                .append(NewEvent {
                    agent_id: AgentId::new(agent.clone()).expect("agent"),
                    session_id: SessionId::new(session.clone()).expect("session"),
                    execution_id: None,
                    source: Source::Observer,
                    source_event_id: None,
                    kind: EventKind::MessageReceived,
                })
                .expect("an event");
            (agent, session)
        }
    }

    impl Drop for Db {
        fn drop(&mut self) {
            if let Ok(mut client) = postgres::Client::connect(&self.url, postgres::NoTls) {
                let _ =
                    client.batch_execute(&format!("drop schema if exists {} cascade", self.schema));
            }
        }
    }

    /// Issues #7, #8 and #12 through `merge_main` itself: with an input in
    /// flight in the worker's session, main is not merged (the workspace and
    /// the branch stay as they were); with none, every merge is recorded and
    /// published to the source before `merge_main` returns, and all of them
    /// run on one pusher, with one thread, for the source.
    #[test]
    fn merge_main_waits_for_a_busy_worker_and_publishes_every_merge_on_one_pusher() {
        use spawn_core::command::{CommandAction, Submission};
        use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId};
        use spawn_core::port::CommandQueue;
        let db = Db::new("merge_main");
        let (base, source, root, mirror, store) = world("merge-main");
        let (busy, busy_session) = db.task_on(&source, "busy");
        let (idle, idle_session) = db.task_on(&source, "idle");
        for agent in [&busy, &idle] {
            agent_with_work(&store, agent, |directory| {
                std::fs::write(directory.join("work.txt"), "mine\n").expect("write");
            });
        }
        // A person types into the busy worker: the input is in flight.
        db.store
            .submit(
                &Principal::new("human").expect("principal"),
                &Submission {
                    agent_id: AgentId::new(busy.clone()).expect("agent"),
                    session_id: SessionId::new(busy_session.clone()).expect("session"),
                    idempotency_key: IdempotencyKey::new("human-1").expect("key"),
                    action: CommandAction::Input {
                        text: "run the tests".to_string(),
                        tags: Vec::new(),
                    },
                },
            )
            .expect("submit");
        let landing = |agent: &str| {
            fetch_pushed_into(&mirror, &source, agent, &format!("refs/spawn-integrate/{agent}"))
                .expect("fetch")
        };
        let busy_head = head(&mirror, &busy);
        for round in 0..3 {
            main_moves(&base, &source, &mirror, |clone| {
                std::fs::write(clone.join(format!("main-{round}.txt")), "from main\n")
                    .expect("write");
            });
            let merge = super::merge_main(&db.store, &root, &busy, &busy_session);
            assert!(matches!(merge, Merge::Busy(_)), "{merge:?}");
            assert_eq!(head(&mirror, &busy), busy_head, "no merge while it is busy");
            assert!(!root.join(&busy).join(format!("main-{round}.txt")).exists());

            let merge = super::merge_main(&db.store, &root, &idle, &idle_session);
            let Merge::Merged { commit, .. } = &merge else {
                panic!("an idle worker gets main: {merge:?}");
            };
            assert_eq!(
                landing(&idle).as_deref(),
                Some(commit.as_str()),
                "published before merge_main returned"
            );
            assert_eq!(
                db.store.latest_checkpoint(&idle).expect("checkpoint").as_deref(),
                Some(commit.as_str()),
                "recorded"
            );
        }
        assert!(
            crate::push::shared(&source).threads() <= 1,
            "at most one pusher thread for the source, not one per merge"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
