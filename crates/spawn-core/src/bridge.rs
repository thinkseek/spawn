//! Agent Bridge: turns commands into executions and persists what happens.
//!
//! The Bridge owns four jobs: start and resume engines, record events, commit
//! workspace state, and map native identifiers to Spawn sessions. It does not
//! run the agent loop and does not judge state.
//!
//! The Bridge is the only writer of a Bridge-sourced `execution.ended`, and
//! it writes that event only after the final checkpoint, the terminal kill
//! and the unmount. A refold of the log therefore never repeats cleanup.
//!
//! **An end's cause and its signal.** The Bridge tells three ends apart:
//! `exited` (the runner wrote the engine's exit status), `stopped` (a Stop
//! or a Discard, a workspace the store kept off its branch) and `lost` (no
//! runner of the Bridge's own runs the execution any more: its terminal is
//! gone, or is there and hosts no runner, which the end's `evidence` says).
//! An engine the OS killed has no exit status, so the runner writes the
//! signal that killed it beside the status (<[`signal_file`]> of
//! [`exit_file`], `9` for a SIGKILL) and the end records it: without it a
//! SIGKILL read as a clean exit with no code, and a crash was
//! indistinguishable from an exit. What the Bridge records is the *how*; the
//! *why* — the class a recovery acts on (`crashed`, `disk_full`,
//! `model_unavailable`, `host_restart`, …) — is read from these facts, the
//! transcript's API error lines and the host's own observations by the
//! recovery (`spawn-cli`'s `recover`, docs/fleet-architecture.md §3.1).
//! A turn the model failed ends no execution: Claude Code writes its API
//! error line to the transcript and waits at its prompt, so `turn.failed`
//! keeps the engine's own `apiErrorStatus` and `error` tag for that rule.
//!
//! An input to a busy session waits for its turn (docs/design.md §5). An
//! `Input` from a person or the supervisor (any principal but the loops in
//! [`LOOP_PRINCIPALS`]) that finds the pane not ready, or a capture that
//! keeps failing on a live terminal, records `command.step` `waiting` once
//! (key `<command>:waiting`) and stays unfinished. It is typed on a Ready
//! reading, and a Ready reading is spent by the typing: the next input needs
//! one taken after the previous `typed` (or that typing's turn to have
//! ended). The session's waiting inputs go oldest first: none is typed
//! before an earlier one still waiting. At most once holds as for any
//! input: `typed` succeeds, `typing` without `typed` fails with
//! [`MAY_HAVE_TYPED`]. The loops' own inputs keep their contract: refused at
//! once while the pane is not ready (they retry on their own), and refused
//! behind a waiting input too. A waiting input holds nothing else: the
//! queue hands out what comes behind it (a stop, a discard, a resume, a
//! later input to be recorded waiting). When the execution ends, its
//! waiting inputs fail in the same tick or the next: as stopped if a stop or
//! a discard ended it, otherwise with the end's cause; a resume does not
//! revive them. An input to a session with no execution still fails at
//! once.
//!
//! The wait has no timeout of its own. Its lease approach: a waiting input
//! keeps no lease between ticks. The Bridge holds the claim for the rest of
//! the tick (so a command behind it can be claimed), then releases it
//! ([`CommandQueue::release`]) to be claimable again after
//! [`BridgeConfig::input_wait_retry`]; the waiting itself is read from the
//! log, so another or a restarted Bridge carries on where this one stopped.
//! The `waiting` record is not activity: the stall layer and the
//! dispatcher's staleness ignore it, and the Observer covers it like any
//! command step.
//!
//! What a pass reads of a running engine, and when (docs/design.md §5, §6).
//! A pass costs nothing per idle agent. The Bridge watches each live
//! execution's transcript and its runner's exit status ([`FileWatch`]) and
//! reads one only when it changed; where no watch can be set for an
//! execution (an error, a filesystem without watches, the platform's
//! limit) its files are read on every pass, as before, and the fallback is
//! said once. The offset rule of [`Bridge::follow_output`] stays: a line
//! written while a read is in progress is read at the next change or pass,
//! never lost.
//!
//! A pane is captured on a pass only when it can matter: a command is in
//! flight on the session (claimed and unfinished), or its transcript has
//! held still for [`BridgeConfig::screen_quiet`] since it last grew and its
//! screen was not read since — with readings repeated on the following
//! passes until two in a row agree, the confirmation a fact needs — or once
//! per [`BridgeConfig::screen_refresh`] as a backstop for a screen that
//! changes with nothing in the transcript (a dialog, a crash to a shell).
//! A turn that ends in the transcript is read on the following passes until
//! a reading accepts input or `screen_refresh` has passed since the turn
//! ended: the engine paints its prompt a moment after the line that ends
//! the turn, and that reading — the worker idle at its prompt — is the one
//! a delivery waits for, so it is never left to the backstop. A busy screen
//! is still recorded again every `screen_refresh` while a turn keeps it
//! busy. An idle session with a quiet transcript and nothing
//! in flight costs no capture between backstops, and no tmux call of its
//! own: whether the fleet still runs is one call per pass
//! ([`TerminalHost::runners`]), so a terminal killed from outside ends its
//! execution as lost on the next pass, as before.
//!
//! Causality (§14): a screen state is recorded from a capture, and a turn's
//! end from the transcript's line. The absence of a capture, or of a
//! transcript change, is never read as a state; a quiet transcript only
//! says when to look at the screen again. So a screen change that decides a
//! delivery (the worker at its prompt, its turn stopped) is recorded no
//! later than a reading of the pane would show it: the prompt a turn's end
//! is followed by is read on the pass it appears, not at the next backstop.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use serde_json::json;

use crate::command::{Claimed, CommandAction, NamedKey, Outcome};
use crate::event::{
    ArtifactRef, CheckpointReason, CommandStep, ENGINE, EndCause, EventKind, EventRecord, Fork,
    GlobalPosition, ModelProfile, NewEvent, Source,
};
use crate::id::{
    AgentId, BranchName, CommandId, CommitId, ExecutionId, SessionId, TerminalName, WorkspaceId,
};
use crate::ingest::{claude_line, claude_version};
use crate::port::WORKSPACE_STALE;
use crate::port::{
    BaseMerge, Branch, CommandQueue, Commit, EventLog, FileWatch, Launch, READ_LIMIT, Runner,
    StoreError, TerminalError, TerminalHost, TranscriptArchive, WorkspaceError, WorkspaceStore,
};
use crate::screen::{ScreenState, TAIL_LINES, TRUST_KEYS, is_trust_dialog, read_claude_screen};
use crate::time::{Clock, SystemClock};
use crate::transcripts;

/// Final checkpoints an ending execution tries on consecutive ticks before
/// its end counts as stalled: the terminal is killed, the workspace stays
/// mounted, `execution.end_stalled` asks for attention, and the checkpoint is
/// retried every [`END_RETRY_MS`]. A workspace is unmounted only after its
/// final checkpoint commits, or on a `discard` command.
pub const END_ATTEMPTS: u32 = 20;

/// How often a stalled end retries its final checkpoint, by the Bridge's
/// clock.
pub const END_RETRY_MS: i64 = 30_000;

/// Backstop captures one pass takes at most (see [`BridgeConfig::screen_refresh`]):
/// the screens that fell due are read over the next passes, so a fleet does
/// not spend one pass capturing every pane and holding the next command.
pub const BACKSTOPS_PER_PASS: usize = 2;

/// Why a pass wants a live execution's pane read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wanted {
    /// A command is in flight, a reading awaits its twin, or the transcript
    /// has just held still: read it now.
    Now,
    /// Nothing changed for [`BridgeConfig::screen_refresh`]: the backstop.
    Backstop,
    /// Nothing to read.
    Nothing,
}

/// The session and settings of one launch.
///
/// Grouped so that [`Bridge::launch`] keeps a readable parameter list.
struct Run<'a> {
    native: Option<&'a str>,
    /// `native` names an earlier session to resume, not a new one.
    resume: bool,
    /// Claude Code's inline settings ([`claude_settings`]).
    settings: &'a str,
}

/// What one start asks for. Grouped so that [`Bridge::start`] keeps a
/// readable parameter list.
struct Begin<'a> {
    prompt: &'a str,
    parent: Option<&'a SessionId>,
    /// A session to resume.
    native: Option<&'a str>,
    /// Where a new branch starts (`start {from}`).
    from: Option<&'a str>,
    /// Models the engine runs on; unset keeps the session's last profile.
    model: Option<ModelProfile>,
}

/// The loopback relay engines reach their models through
/// (`scripts/model-relay.py`), and the models each profile names. The relay
/// routes `deepseek-*` models to DeepSeek with its own key and everything
/// else to Claude Code's gateway with the engine's own credentials, so
/// Spawn holds neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRelay {
    /// Base URL of the relay, `http://127.0.0.1:<port>`.
    pub url: String,
    /// The cheap profile's main model, also in its sonnet and opus slots.
    pub cheap_model: String,
    /// The small, fast model (the haiku slot) of every profile.
    pub small_model: String,
}

impl ModelRelay {
    /// Whether the relay accepts connections now. Checked at every start,
    /// so a relay that comes back is used again without a restart.
    pub fn reachable(&self) -> bool {
        let address = self
            .url
            .split_once("://")
            .map_or(self.url.as_str(), |(_, rest)| rest);
        let address = address.split('/').next().unwrap_or_default();
        let Ok(mut addresses) = std::net::ToSocketAddrs::to_socket_addrs(address) else {
            return false;
        };
        addresses.any(|address| {
            std::net::TcpStream::connect_timeout(&address, Duration::from_millis(500)).is_ok()
        })
    }
}

/// Bridge settings.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// Absolute path of the `spawn` binary, which runs the engine (`spawn run`).
    pub spawn_program: String,
    /// Claude Code executable.
    pub claude_program: String,
    /// Extra environment for the engine process, for example the database
    /// URL that the hook and the runner need.
    pub environment: Vec<(String, String)>,
    /// Environment that replaces entries of `environment` for one session,
    /// for example a different database login for the supervisor.
    pub session_environment: Vec<(String, Vec<(String, String)>)>,
    /// Worker name recorded in the lease.
    pub worker: String,
    /// Lease duration of one command.
    pub lease_ttl: Duration,
    /// Commands handled per tick.
    pub max_commands_per_tick: usize,
    /// Attempts to find a ready input box before typing.
    pub ready_attempts: usize,
    /// Delay between ready attempts, and between input attempts.
    pub ready_interval: Duration,
    /// Attempts of an input whose terminal call failed (a capture, or the
    /// staging of its text) while the terminal is still there. A pane that
    /// reads as not ready refuses an input of Spawn's own loops at once
    /// ([`PANE_NOT_READY`]); any other input waits ([`waits`]).
    pub input_attempts: usize,
    /// How soon an input that waits for its turn is claimed again: its lease
    /// is released for this long ([`CommandQueue::release`]), so a busy pane
    /// is read about this often until it takes the input.
    pub input_wait_retry: Duration,
    /// Where Claude Code writes session transcripts (its `projects`
    /// directory), as the terminal host sees it.
    pub claude_projects: String,
    /// A busy screen is recorded again this often while it stays busy, so
    /// the Observer keeps a current fact for a long turn. It is also the
    /// backstop of a screen that changes with nothing in the transcript (a
    /// dialog, a crash to a shell): every live pane is captured at least
    /// this often.
    pub screen_refresh: Duration,
    /// How long a live execution's transcript must hold still before its
    /// pane is read for what the transcript cannot say: the prompt is back.
    /// A turn's end is read from the transcript as soon as the file changes;
    /// this is how long after the last line the screen is compared with what
    /// was recorded. Shorter is readier sooner and costs more captures.
    pub screen_quiet: Duration,
    /// The model relay, when Claude Code engines go through one.
    pub model_relay: Option<ModelRelay>,
    /// The watch on the files the engines write (transcripts and exit
    /// statuses), when the host has one. `None` reads every live execution's
    /// files on every pass, as Spawn did before watches: correct, and the
    /// cost grows with the fleet.
    pub watch: Option<Arc<dyn FileWatch>>,
    /// Environment decided again at every engine launch, after
    /// `environment` and before `session_environment`: the fleet's test
    /// database, given only once it is known not to be the gates' server or
    /// the live one.
    pub launch_environment: Option<Arc<dyn LaunchEnvironment>>,
}

/// Environment an engine gets only when it is safe at its launch (see
/// [`BridgeConfig::launch_environment`]). Asked at every launch, so what
/// was withheld at one is given at a later one once it checks out.
pub trait LaunchEnvironment: Send + Sync + std::fmt::Debug {
    /// The variables to add now; none when they are withheld (the
    /// implementation says why where the owner reads it).
    fn environment(&self) -> Vec<(String, String)>;
}

impl BridgeConfig {
    /// Defaults for one worker.
    pub fn new(worker: &str) -> Self {
        Self {
            spawn_program: "spawn".to_string(),
            claude_program: "claude".to_string(),
            environment: Vec::new(),
            session_environment: Vec::new(),
            worker: worker.to_string(),
            lease_ttl: Duration::from_mins(2),
            max_commands_per_tick: 16,
            ready_attempts: 40,
            ready_interval: Duration::from_millis(250),
            input_attempts: 4,
            input_wait_retry: Duration::from_secs(1),
            claude_projects: "/root/.claude/projects".to_string(),
            screen_refresh: Duration::from_secs(30),
            screen_quiet: Duration::from_millis(500),
            model_relay: None,
            watch: None,
            launch_environment: None,
        }
    }
}

/// An end the Bridge found: its cause, the exit code the runner wrote and
/// the signal that killed the engine (one of the two codes is set when the
/// end has one). It is what `execution.ended` records, and the recovery
/// reads it to classify the end.
type Ending = (EndCause, Option<i32>, Option<i32>);

/// Running execution of one session.
#[derive(Debug, Clone)]
struct Active {
    execution_id: ExecutionId,
    workspace: WorkspaceId,
    /// A tool or turn finished after the last commit.
    // ponytail: an `Unchanged` commit writes no event, so after a refold an
    // idle session repeats one no-op commit per tick; record attempts if
    // Workspace calls get expensive.
    checkpoint: Option<CheckpointReason>,
    /// The runner reported the engine exit; cleanup is still due.
    reported_end: Option<Ending>,
    /// The end committed its final checkpoint and killed the terminal
    /// (`execution.unmounting`): what is left is the unmount and the record.
    unmounting: bool,
    /// What the Bridge has read of the engine's files and screen.
    follow: Follow,
}

impl Active {
    /// An execution as its `execution.started` record has it.
    fn started(
        execution_id: ExecutionId,
        workspace: WorkspaceId,
        transcript_from: Option<u64>,
    ) -> Self {
        Self {
            execution_id,
            workspace,
            checkpoint: None,
            reported_end: None,
            unmounting: false,
            // The execution's lines are the ones after where the transcript
            // ended when it started, after a restart too.
            follow: Follow {
                offset: transcript_from.unwrap_or(0),
                ..Follow::default()
            },
        }
    }

    /// The terminal that hosts the execution: always `spawn-<execution>`,
    /// so it is derived, never stored.
    fn terminal(&self) -> TerminalName {
        TerminalName::for_execution(&self.execution_id)
    }
}

/// The Bridge's own view of a running engine, rebuilt after a restart: read
/// positions start at zero (event keys make a second read a no-op), and the
/// last recorded screen is folded from the log.
#[derive(Debug, Clone, Default)]
struct Follow {
    /// The watch on its files.
    watch: Watching,
    /// When its transcript last grew and its screen has not been read since,
    /// by the Bridge's clock. The pane is due once the transcript has held
    /// still for [`BridgeConfig::screen_quiet`].
    unread_since: Option<i64>,
    /// Its last capture could not be recorded (one reading is not a fact, or
    /// it read [`ScreenState::Unknown`]): capture again next pass.
    again: bool,
    /// When a turn of this execution last ended in its transcript, by the
    /// Bridge's clock, while the screen has not yet shown the turn over (a
    /// reading that accepts input). The engine paints the prompt a moment
    /// after the line that ends the turn, and that reading decides a
    /// delivery: the pane is read on the following passes until it is here,
    /// or [`BridgeConfig::screen_refresh`] has passed since the turn ended.
    settling: Option<i64>,
    /// When its pane was last captured.
    last_capture: Option<i64>,
    /// When to read its files next although nothing was watched: the grace
    /// after which a transcript that never appeared is reported.
    due_at: Option<i64>,
    /// Bytes of the transcript read so far.
    offset: u64,
    /// The last screen state recorded, and when.
    screen: Option<(ScreenState, i64)>,
    /// A state read once and not recorded yet: a screen is recorded when
    /// two readings in a row agree, so a frame mid-redraw is not a fact.
    seen: Option<ScreenState>,
    /// When this Bridge first followed the execution, by its own clock.
    first_seen: Option<i64>,
    /// The engine's file is missing after the start grace, or unreadable;
    /// warned once.
    warned: bool,
    /// The runner's exit file is unreadable; warned once.
    exit_warned: bool,
    /// Final checkpoints of this execution's end that failed. The
    /// workspace stays mounted until one succeeds (or a `discard`); from
    /// [`END_ATTEMPTS`] on the end is stalled.
    end_failures: u32,
    /// When the end's final checkpoint was last tried, by the Bridge's
    /// clock: a stalled end retries every [`END_RETRY_MS`].
    end_tried_at: Option<i64>,
    /// When the end's kill last failed (the runner outlived it), by the
    /// Bridge's clock: a kill waits up to its bound for the runner's
    /// processes (TerminalHost::terminate), so it is tried again only after
    /// [`END_RETRY_MS`], discard or not (rev-f on 204d1a1f).
    kill_failed_at: Option<i64>,
    /// The end's kill, running off the Bridge's loop while it waits for the
    /// engine's processes to exit ([`TerminalHost::terminate_waits`]):
    /// each end attempt reads whether it is done; until then the end is
    /// pending, which is no failure.
    killing: Option<Killing>,
    /// The workspace answered [`WORKSPACE_STALE`] with this message: it
    /// cannot checkpoint to its branch again, and its state is kept off it,
    /// so the execution is ended (and its workspace unmounted) instead of
    /// retried.
    stale: Option<String>,
    /// The engine's version, once read from this execution's transcript
    /// and recorded.
    version: Option<String>,
    /// What the Bridge saw when it found no runner of its own running the
    /// execution (the terminal gone, or there and hosting no runner): the
    /// end record's `evidence`.
    evidence: Option<String>,
}

/// A kill of an execution's terminal running on a thread of its own
/// ([`Bridge::kill`]): its result and how long it waited, once it is done.
#[derive(Clone)]
struct Killing {
    done: Arc<(std::sync::Mutex<Option<Killed>>, std::sync::Condvar)>,
}

/// A kill's result and its wait.
type Killed = (Result<(), TerminalError>, std::time::Duration);

impl std::fmt::Debug for Killing {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Killing")
    }
}

/// How long an end waits in line for a kill run off the loop before it
/// leaves it running and goes on: a terminal already gone (its processes
/// exited) is done well within it, so its end finishes in the same tick.
const KILL_IN_LINE: std::time::Duration = std::time::Duration::from_millis(50);

/// The watch on an execution's files ([`FileWatch`]).
#[derive(Debug, Clone)]
struct Watching {
    /// Its files changed since the last read. A session read on every pass
    /// keeps it set, so a read that left a line half-written or a checkpoint
    /// uncommitted tries again next pass.
    dirty: bool,
    /// No watch could be set on its files: they are read on every pass, as
    /// before watches, and [`Watching::said`] says so once.
    poll: bool,
    /// The fallback to reading on every pass was said.
    said: bool,
}

impl Default for Watching {
    fn default() -> Self {
        Self {
            // Nothing has been read yet: the first pass reads.
            dirty: true,
            poll: false,
            said: false,
        }
    }
}

/// What the Bridge knows about one session.
#[derive(Debug, Clone)]
struct SessionView {
    agent_id: AgentId,
    /// The native session the latest execution ran, which a resume
    /// continues.
    native: Option<String>,
    /// The log holds events read from that session's transcript: it
    /// existed, and a resume must not start without it.
    transcript_read: bool,
    /// The profile the session's last execution started with: what a start
    /// or resume that names none runs on.
    model: ModelProfile,
    active: Option<Active>,
    /// What the log last says the agent's branch points at: the commit of
    /// its last `artifact.committed`, or the fork its branch was created at.
    /// The agent's branch is shared by its sessions: recovery takes the
    /// latest of these over all of them, by log position.
    recorded_head: Option<(CommitId, GlobalPosition)>,
    /// The session's latest execution, where it started in the log, and
    /// whether it has ended.
    latest: Option<(ExecutionId, GlobalPosition, bool)>,
    /// What the log says each command's handling already did
    /// ([`CommandStep`]): a retried command and a restarted Bridge go on from
    /// there instead of doing an effect twice.
    steps: HashMap<CommandId, Steps>,
    /// The latest prompt the engine's transcript recorded: its execution and
    /// where it is in the log.
    last_message: Option<(ExecutionId, GlobalPosition)>,
    /// What the log says about the session's inputs ([`Inputs`]).
    inputs: Inputs,
}

/// What the log says about a session's inputs: which wait for their turn,
/// and whether the input typed last has been answered, so the next goes on
/// a ready screen of its own. All of it is folded from the log, so a
/// restarted Bridge carries on with it.
#[derive(Debug, Clone, Default)]
struct Inputs {
    /// Unfinished inputs with a `waiting` step, each with the execution it
    /// waited on.
    waiting: BTreeMap<CommandId, Option<ExecutionId>>,
    /// How each ended execution of the session ended.
    ended: HashMap<ExecutionId, EndCause>,
    /// The latest `typed` step (a prompt's or an input's).
    typed: Option<(ExecutionId, GlobalPosition)>,
    /// The latest screen the Bridge recorded as ready.
    ready: Option<(ExecutionId, GlobalPosition)>,
    /// The latest end of a turn in the engine's transcript.
    turn_end: Option<(ExecutionId, GlobalPosition)>,
}

/// `at` is later than `than` in the same execution.
fn after(
    at: Option<&(ExecutionId, GlobalPosition)>,
    execution: &ExecutionId,
    than: GlobalPosition,
) -> bool {
    at.is_some_and(|(seen, position)| seen == execution && *position > than)
}

/// The recorded steps of one command ([`CommandStep`]).
#[derive(Debug, Clone, Default)]
struct Steps {
    /// The start mounted this workspace.
    mounted: Option<WorkspaceId>,
    /// That mount went again before an execution used it.
    released: bool,
    /// The command's execution started.
    started: bool,
    /// Where the Bridge recorded that it was about to type.
    typing_at: Option<GlobalPosition>,
    /// The typing succeeded.
    typed: bool,
    /// How far the command's key went (a key command's steps).
    press: Press,
}

/// How far a key command's key went, by its steps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Press {
    /// Nothing recorded: the key is due.
    #[default]
    Due,
    /// `pressing` recorded and no `pressed`: it may have reached the pane.
    Pressing,
    /// `pressed`: it was sent.
    Pressed,
}

/// What a retried command's typing comes to ([`SessionView::typing`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Typing {
    /// Nothing was typed yet.
    Due,
    /// It was typed: the record says so, or the engine's transcript has a
    /// prompt after the record that typing began.
    Done,
    /// Typing began and nothing says whether it reached the engine; it is
    /// not typed again.
    Unknown,
}

impl SessionView {
    /// Fold the command steps and the latest prompt.
    fn fold_steps(&mut self, record: &EventRecord) {
        let event = &record.event;
        match &event.kind {
            EventKind::CommandStepped {
                command_id,
                step,
                workspace,
            } if event.source == Source::Bridge => {
                let steps = self.steps.entry(*command_id).or_default();
                match step {
                    CommandStep::Mounted => steps.mounted.clone_from(workspace),
                    CommandStep::MountReleased => steps.released = true,
                    CommandStep::Typing => steps.typing_at = Some(record.position),
                    CommandStep::Typed => {
                        steps.typed = true;
                        if let Some(execution) = event.execution_id.clone() {
                            self.inputs.typed = Some((execution, record.position));
                        }
                    }
                    CommandStep::Waiting => {
                        self.inputs
                            .waiting
                            .insert(*command_id, event.execution_id.clone());
                    }
                    // A key's steps say nothing of the session's inputs:
                    // no input was typed, and none waits for an answer.
                    CommandStep::Pressing => {
                        if steps.press == Press::Due {
                            steps.press = Press::Pressing;
                        }
                    }
                    CommandStep::Pressed => steps.press = Press::Pressed,
                }
            }
            EventKind::CommandFinished { command_id, .. } => {
                self.inputs.waiting.remove(command_id);
            }
            EventKind::ScreenObserved {
                state: ScreenState::Ready,
                ..
            } if event.source == Source::Bridge => {
                if let Some(execution) = event.execution_id.clone() {
                    self.inputs.ready = Some((execution, record.position));
                }
            }
            EventKind::TurnStopped { .. } | EventKind::TurnFailed { .. } => {
                if let Some(execution) = event.execution_id.clone() {
                    self.inputs.turn_end = Some((execution, record.position));
                }
            }
            EventKind::ExecutionEnded { cause, .. } if event.source == Source::Bridge => {
                if let Some(execution) = event.execution_id.clone() {
                    self.inputs.ended.insert(execution, *cause);
                }
            }
            EventKind::ExecutionStarted { command_id, .. } => {
                self.steps.entry(*command_id).or_default().started = true;
            }
            EventKind::MessageReceived => {
                if let Some(execution) = event.execution_id.clone() {
                    self.last_message = Some((execution, record.position));
                }
            }
            _ => {}
        }
    }

    /// Whether the typing of `command` into `execution` is due, done or
    /// unknown, from the log.
    fn typing(&self, command: CommandId, execution: &ExecutionId) -> Typing {
        let Some(steps) = self.steps.get(&command) else {
            return Typing::Due;
        };
        match steps.typing_at {
            _ if steps.typed => Typing::Done,
            None => Typing::Due,
            Some(at) => {
                let arrived = self
                    .last_message
                    .as_ref()
                    .is_some_and(|(seen, position)| seen == execution && *position > at);
                if arrived {
                    Typing::Done
                } else {
                    Typing::Unknown
                }
            }
        }
    }

    /// Whether the input typed last into `execution` has been answered, so
    /// another may be typed: nothing was typed into it yet, or since the
    /// last `typed` the Bridge recorded a ready screen, or the engine
    /// received a prompt and ended its turn. A ready screen read before a
    /// typing is consumed by it (the rule the Observer's snapshot applies),
    /// so two inputs never land in one prompt.
    fn answered(&self, execution: &ExecutionId) -> bool {
        let Some((typed_in, typed_at)) = &self.inputs.typed else {
            return true;
        };
        if typed_in != execution {
            return true;
        }
        if after(self.inputs.ready.as_ref(), execution, *typed_at) {
            return true;
        }
        self.last_message
            .as_ref()
            .filter(|(seen, position)| seen == execution && *position > *typed_at)
            .is_some_and(|(_, message)| after(self.inputs.turn_end.as_ref(), execution, *message))
    }

    /// Fold what recovery needs to compare the agent's branch with the log:
    /// the commit last recorded for it and the latest execution.
    fn fold_branch(&mut self, record: &EventRecord) {
        let event = &record.event;
        match &event.kind {
            EventKind::ExecutionStarted { fork, .. } => {
                if let Some(execution) = event.execution_id.clone() {
                    self.latest = Some((execution, record.position, false));
                }
                if let Some(fork) = fork {
                    self.recorded_head = Some((fork.commit.clone(), record.position));
                }
            }
            EventKind::ExecutionEnded { .. } if event.source == Source::Bridge => {
                if let Some((execution, _, ended)) = self.latest.as_mut()
                    && event.execution_id.as_ref() == Some(execution)
                {
                    *ended = true;
                }
            }
            EventKind::ArtifactCommitted { artifact, .. } => {
                self.recorded_head = Some((artifact.commit.clone(), record.position));
            }
            _ => {}
        }
    }
}

/// Result of one tick.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickReport {
    /// Commands handled.
    pub commands: usize,
    /// Non-fatal problems, for example a failed commit.
    pub warnings: Vec<String>,
    /// What the Bridge measured and says for the log, not a problem: how
    /// long each end's kill waited for the engine's processes to exit.
    pub notes: Vec<String>,
}

/// Agent Bridge.
pub struct Bridge {
    log: Arc<dyn EventLog>,
    queue: Arc<dyn CommandQueue>,
    workspaces: Arc<dyn WorkspaceStore>,
    terminals: Arc<dyn TerminalHost>,
    config: BridgeConfig,
    cursor: GlobalPosition,
    sessions: HashMap<SessionId, SessionView>,
    clock: Arc<dyn Clock>,
    /// Per session, the `artifact.committed` of a commit the branch already
    /// took whose append failed: appended again first at the next tick
    /// ([`Bridge::record_commit`]).
    unrecorded_commits: std::sync::Mutex<HashMap<SessionId, Vec<NewEvent>>>,
    /// Per session, the store's notices this Bridge already logged as a
    /// warning. Only the log: whether the agent was told is the store's
    /// record ([`WorkspaceStore::notices_told`]).
    warned: std::sync::Mutex<HashMap<SessionId, HashSet<String>>>,
    /// Where the engines' transcripts are copied ([`crate::transcripts`]).
    transcripts: Option<Arc<dyn TranscriptArchive>>,
    /// The latest claim of each input this Bridge left waiting, so that an
    /// execution's end fails them in the same tick. Only a shortcut: a
    /// restarted Bridge, which has none, fails them when it claims them
    /// next (the log says which wait and how the execution ended).
    held: HashMap<CommandId, Claimed>,
    /// The watch keys whose files changed since the last pass, drained at
    /// the start of a pass: what an execution whose files are quiet is left
    /// alone for.
    touched: HashSet<String>,
    /// Backstop captures taken this pass ([`BACKSTOPS_PER_PASS`]).
    backstops: std::sync::atomic::AtomicUsize,
    /// Whether each live terminal hosts its runner, asked once this pass
    /// ([`TerminalHost::runners`]) when the fleet's files are watched: a
    /// killed terminal is then seen on the next pass without a tmux call per
    /// agent. `None` asks each terminal when the execution is advanced.
    runners: Option<HashMap<TerminalName, Runner>>,
    /// The threads of the kills run off the loop ([`Bridge::kill`]), until
    /// they are done: a Bridge that goes away (the loop's clean exit, a
    /// Bridge built again after a store outage) waits for them on drop,
    /// up to [`KILL_DRAIN`]. A kill abandoned mid-wait is lost for good: the
    /// host reads the session's processes once, before it kills the
    /// session, so a later kill of the same terminal finds it gone and
    /// waits for nothing (rev-e on 471e29d8).
    kill_threads: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// Notes of a report the tick does not see ([`Bridge::abandon`] ends
    /// on a report of its own): given to the tick's report at the end of
    /// [`Bridge::work`].
    stray_notes: Vec<String>,
}

/// How long a Bridge that goes away waits for its kills still under way
/// ([`Bridge::kill_threads`]): longer than a host's kill takes at its bound
/// (tmux: 5 s for the session's processes, SIGKILL, 2 s more).
const KILL_DRAIN: std::time::Duration = std::time::Duration::from_secs(10);

impl Drop for Bridge {
    fn drop(&mut self) {
        let threads = std::mem::take(
            &mut *self
                .kill_threads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let deadline = std::time::Instant::now() + KILL_DRAIN;
        let mut left = 0;
        for thread in threads {
            while !thread.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                left += 1;
            }
        }
        if left > 0 {
            eprintln!(
                "bridge: {left} kill(s) still waiting for their processes after {} s; left running",
                KILL_DRAIN.as_secs()
            );
        }
    }
}

/// What [`Bridge::settle`] did with an open execution no runner runs.
enum Settled {
    /// It ended now: what it was, for the command's report.
    Ended(String),
    /// Its end could not finish this tick (its final checkpoint failed): it
    /// stays due, and the command fails with why.
    Ending(String),
    /// Its end waits for its kill, off the loop ([`Bridge::kill`]): the
    /// command waits too, and is claimed again after
    /// [`BridgeConfig::input_wait_retry`].
    Pending,
}

/// What handling a claimed command came to.
enum Handled {
    /// It finished, with this outcome.
    Finished(Outcome),
    /// An input that waits for its turn: left unfinished, claimed again
    /// after [`BridgeConfig::input_wait_retry`].
    Waiting,
}

impl Bridge {
    /// Bridge over one store and one worker name.
    pub fn new(
        log: Arc<dyn EventLog>,
        queue: Arc<dyn CommandQueue>,
        workspaces: Arc<dyn WorkspaceStore>,
        terminals: Arc<dyn TerminalHost>,
        config: BridgeConfig,
    ) -> Self {
        Self {
            log,
            queue,
            workspaces,
            terminals,
            config,
            cursor: GlobalPosition(0),
            sessions: HashMap::new(),
            clock: Arc::new(SystemClock),
            unrecorded_commits: std::sync::Mutex::default(),
            warned: std::sync::Mutex::default(),
            transcripts: None,
            held: HashMap::new(),
            touched: HashSet::new(),
            backstops: std::sync::atomic::AtomicUsize::new(0),
            runners: None,
            kill_threads: std::sync::Mutex::new(Vec::new()),
            stray_notes: Vec::new(),
        }
    }

    /// Copy every engine's transcript into `archive` at turn ends and
    /// execution ends, and restore a missing one from it before a resume.
    #[must_use]
    pub fn with_transcript_archive(mut self, archive: Arc<dyn TranscriptArchive>) -> Self {
        self.transcripts = Some(archive);
        self
    }

    /// Use `clock` for screen refreshes instead of the system clock.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Sessions the Bridge tracks.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Whether an execution's end waits for its kill, run off the loop
    /// ([`Bridge::kill`]): the end, and the stop that asked for it, finish
    /// at a later tick. A one-shot caller (`spawn tick`) ticks again until
    /// none does, so it finishes what it claimed, as the loop does over its
    /// next passes.
    pub fn ends_pending(&self) -> bool {
        // A stop or discard left waiting for its end: the end may be done
        // already (a tick's own pass reads the kill), while the command is
        // claimed again only after `input_wait_retry`.
        let stop_waits = self.held.values().any(|claimed| {
            matches!(
                claimed.action(),
                Ok(CommandAction::Stop | CommandAction::Discard)
            )
        });
        stop_waits
            || self.sessions.values().any(|view| {
                view.active
                    .as_ref()
                    .is_some_and(|active| active.follow.killing.is_some())
            })
    }

    /// Rebuild state from the whole log.
    pub fn recover(&mut self) -> Result<(), StoreError> {
        self.cursor = GlobalPosition(0);
        self.sessions.clear();
        self.catch_up()?;
        self.rederive_unrecorded();
        self.release_orphan_mounts()?;
        Ok(())
    }

    /// A start that mounted its workspace and died before its execution was
    /// recorded (or before the mount's release was) left a mount no
    /// execution owns. After a restart, each recorded mount with no
    /// execution started and no release goes, unless an execution runs in
    /// that workspace, and its release is recorded. An unmount that fails
    /// is left for the next restart.
    fn release_orphan_mounts(&mut self) -> Result<(), StoreError> {
        let used: HashSet<WorkspaceId> = self
            .sessions
            .values()
            .filter_map(|view| view.active.as_ref())
            .map(|active| active.workspace.clone())
            .collect();
        let mut orphans = Vec::new();
        for (session, view) in &self.sessions {
            for (command, steps) in &view.steps {
                // Compared as the store names them now (an execution's
                // workspace is read that way): one directory, one name.
                if let Some(workspace) = steps.mounted.as_ref()
                    && !steps.released
                    && !steps.started
                    && !used.contains(&self.workspaces.canonical(workspace))
                {
                    orphans.push((
                        view.agent_id.clone(),
                        session.clone(),
                        *command,
                        workspace.clone(),
                    ));
                }
            }
        }
        for (agent_id, session, command, workspace) in orphans {
            // A mount the log's open execution uses is no orphan, whatever
            // this view missed: unmounting it would take the directory from
            // under its engine. The execution is followed again instead.
            if let Some(active) = self
                .log
                .open_execution(&session)?
                .and_then(|record| self.track_from_log(&session, &record))
                && (active.execution_id == ExecutionId::for_command(command)
                    || active.workspace == self.workspaces.canonical(&workspace))
            {
                eprintln!(
                    "spawn: {session}: the log holds execution {} open in {}, which this \
                     Bridge's view had not read; it is followed again, its mount kept",
                    active.execution_id, active.workspace
                );
                continue;
            }
            if self.workspaces.unmount(&workspace).is_err() {
                continue;
            }
            self.append(
                &agent_id,
                &session,
                None,
                Source::Bridge,
                Some(format!("{command}:mount_released")),
                EventKind::CommandStepped {
                    command_id: command,
                    step: CommandStep::MountReleased,
                    workspace: Some(workspace),
                },
            )?;
        }
        self.catch_up()
    }

    /// After a restart, the `artifact.committed` records that were waiting
    /// in memory are gone. Re-derive them: an agent whose branch head is not
    /// the commit the log last recorded for it (its last checkpoint or
    /// merge, or the fork it was created at) took a commit whose record
    /// never reached the log. It waits, as a failed append would, under the
    /// agent's latest session and execution, and the next tick appends it
    /// before anything else of that session.
    ///
    /// The record says what the branch holds, not how it got there: a merge
    /// of main re-derived this way is recorded as a checkpoint, without the
    /// files the merge changed.
    fn rederive_unrecorded(&self) {
        let mut latest: HashMap<&AgentId, (&SessionId, &SessionView, GlobalPosition)> =
            HashMap::new();
        let mut recorded: HashMap<&AgentId, (&CommitId, GlobalPosition)> = HashMap::new();
        for (session, view) in &self.sessions {
            if let Some((commit, at)) = view.recorded_head.as_ref()
                && recorded
                    .get(&view.agent_id)
                    .is_none_or(|(_, known)| at > known)
            {
                recorded.insert(&view.agent_id, (commit, *at));
            }
            let Some((_, started, _)) = view.latest.as_ref() else {
                continue;
            };
            let newer = latest
                .get(&view.agent_id)
                .is_none_or(|(_, _, known)| *started > *known);
            if newer {
                latest.insert(&view.agent_id, (session, view, *started));
            }
        }
        for (agent_id, (session, view, _)) in latest {
            let (Some((recorded, _)), Some((execution, _, ended))) =
                (recorded.get(agent_id), view.latest.as_ref())
            else {
                continue;
            };
            let branch = BranchName::for_agent(agent_id);
            let Ok(Some(head)) = self.workspaces.branch_head(&branch) else {
                continue;
            };
            if &head == *recorded {
                continue;
            }
            let reason = if *ended {
                CheckpointReason::ExecutionEnded
            } else {
                CheckpointReason::TurnStopped
            };
            self.unrecorded_commits_lock()
                .entry(session.clone())
                .or_default()
                .push(NewEvent {
                    agent_id: agent_id.clone(),
                    session_id: session.clone(),
                    execution_id: Some(execution.clone()),
                    source: Source::Bridge,
                    source_event_id: Some(format!("commit:{head}")),
                    kind: EventKind::ArtifactCommitted {
                        reason,
                        artifact: ArtifactRef {
                            project: self.workspaces.project().to_string(),
                            branch,
                            commit: head,
                        },
                        files: Vec::new(),
                    },
                });
        }
    }

    /// Append the waiting commit records of every session, each on its own:
    /// one that the log keeps refusing is warned and stays waiting, and the
    /// other sessions go on.
    fn flush_unrecorded(&self, report: &mut TickReport) {
        let waiting: Vec<SessionId> = self.unrecorded_commits_lock().keys().cloned().collect();
        for session in waiting {
            if let Err(error) = self.record_unrecorded_commit(&session) {
                report.warnings.push(format!(
                    "{session}: a commit record waits, its append failed: {error}"
                ));
            }
        }
    }

    /// Handle queued commands and advance running executions: the whole
    /// tick, in the order [`Self::work`] then [`Self::poll`] run it.
    pub fn tick(&mut self) -> Result<TickReport, StoreError> {
        let (mut report, _) = self.work()?;
        self.act(&mut report)?;
        self.catch_up()?;
        Ok(report)
    }

    /// The Bridge's own reading of the log and of the command queue: catch
    /// up with the events written since the last one, and claim the commands
    /// waiting in the queue, one per session at a time, until none is
    /// claimable or [`BridgeConfig::max_commands_per_tick`] is reached.
    ///
    /// This is the part of a tick that reads the database, and it reads it
    /// for records: its input is the log's latest events and the queue's
    /// rows, so it is run when the log's watermark has moved and not on a
    /// clock (docs/design.md §14). `true` says the claim stopped at that
    /// limit, so a command may still be waiting for a claim now: the caller
    /// runs this again on its next pass. The wait a lease dates
    /// (`lease_until_ms`) is the caller's to time.
    pub fn work(&mut self) -> Result<(TickReport, bool), StoreError> {
        self.catch_up()?;
        let mut report = TickReport::default();
        // Inputs left waiting keep their lease until the claims of this tick
        // are done, so the commands behind them are claimed now; then they
        // are released, to be claimed again after `input_wait_retry`.
        let mut waited: Vec<Claimed> = Vec::new();
        while report.commands < self.config.max_commands_per_tick {
            let Some(claimed) = self
                .queue
                .claim(&self.config.worker, self.config.lease_ttl)?
            else {
                break;
            };
            match self.handle(&claimed, &mut report)? {
                Handled::Finished(outcome) => {
                    self.held.remove(&claimed.command_id);
                    self.queue.finish(&claimed, &outcome)?;
                }
                Handled::Waiting => {
                    self.held.insert(claimed.command_id, claimed.clone());
                    waited.push(claimed);
                }
            }
            report.commands += 1;
            self.catch_up()?;
        }
        for claimed in &waited {
            // One its execution's end failed meanwhile is not released.
            if self.held.contains_key(&claimed.command_id) {
                self.queue.release(claimed, self.config.input_wait_retry)?;
            }
        }
        report.notes.append(&mut self.stray_notes);
        let left = report.commands >= self.config.max_commands_per_tick;
        Ok((report, left))
    }

    /// What the Bridge reads on its own clock, never from the log: each
    /// running execution's transcript (what the engine wrote) and its screen
    /// (what the terminal shows), the checkpoints they ask for, the exit
    /// statuses they leave, and the executions whose engines are gone. What
    /// it reads it turns into records; the next [`Self::work`] folds them.
    ///
    /// This holds no read of the log: it runs on every pass, whatever the
    /// watermark says, because an engine's turn ending and its screen
    /// settling at the prompt are facts no append announces
    /// (objective 025 gives that schedule its own bounds).
    pub fn poll(&mut self) -> Result<TickReport, StoreError> {
        let mut report = TickReport::default();
        self.backstops.store(0, std::sync::atomic::Ordering::SeqCst);
        // What changed in the engines' files since the last pass: a
        // transcript or an exit status is read when it is here, not on every
        // pass ([`Self::follow_output`]).
        self.touched = self
            .config
            .watch
            .as_ref()
            .map(|watch| watch.drain().into_iter().collect())
            .unwrap_or_default();
        // One call for the fleet's liveness, its end detection included: a
        // terminal killed from outside ends its execution as lost on the
        // next pass, as before, without a tmux call per agent.
        self.runners = self.fleet_runners();
        self.act(&mut report)?;
        Ok(report)
    }

    fn catch_up(&mut self) -> Result<(), StoreError> {
        const BATCH: usize = 200;
        loop {
            let batch = self.log.read_global(self.cursor, BATCH)?;
            if batch.is_empty() {
                return Ok(());
            }
            for record in &batch {
                self.fold(record);
            }
            if let Some(last) = batch.last() {
                self.cursor = last.position;
            }
            if batch.len() < BATCH {
                return Ok(());
            }
        }
    }

    fn fold(&mut self, record: &EventRecord) {
        let event = &record.event;
        // The control stream is not an agent's session.
        if event.kind.is_control() {
            return;
        }
        let view = self
            .sessions
            .entry(event.session_id.clone())
            .or_insert_with(|| SessionView {
                agent_id: event.agent_id.clone(),
                native: None,
                transcript_read: false,
                model: ModelProfile::Default,
                active: None,
                recorded_head: None,
                latest: None,
                steps: HashMap::new(),
                last_message: None,
                inputs: Inputs::default(),
            });
        view.fold_branch(record);
        view.fold_steps(record);
        view.transcript_read |= transcripts::read_from_transcript(event);
        let current = view
            .active
            .as_ref()
            .is_some_and(|active| event.execution_id.as_ref() == Some(&active.execution_id));
        match &event.kind {
            EventKind::ExecutionStarted {
                workspace,
                model,
                transcript_from,
                ..
            } => {
                view.model = *model;
                if let Some(execution_id) = event.execution_id.clone() {
                    // As the store names it now: a record from before the
                    // root was resolved names the same directory.
                    let workspace = self.workspaces.canonical(workspace);
                    view.active = Some(Active::started(execution_id, workspace, *transcript_from));
                }
            }
            // After a restart, the last recorded screen. The Bridge's own
            // readings keep its own clock, which the refresh is timed by.
            EventKind::ScreenObserved { state, .. } if current => {
                if let Some(active) = view.active.as_mut()
                    && active.follow.screen.map(|(known, _)| known) != Some(*state)
                {
                    active.follow.screen = Some((*state, record.recorded_at.0));
                }
            }
            EventKind::ExecutionEnded { .. } if current && event.source == Source::Bridge => {
                if let Some(active) = view.active.take()
                    && let Some(watch) = &self.config.watch
                {
                    watch.unwatch(&watch_key(&active.execution_id));
                }
            }
            // An exit report of an old runner (engine-sourced; runners now
            // write SPAWN_EXIT_FILE): only old logs hold one.
            EventKind::ExecutionEnded {
                cause,
                exit_code,
                signal,
                ..
            } if current => {
                if let Some(active) = view.active.as_mut() {
                    active.reported_end = Some((*cause, *exit_code, *signal));
                }
            }
            // A recovered end goes straight to the unmount.
            EventKind::ExecutionUnmounting if current => {
                if let Some(active) = view.active.as_mut() {
                    active.unmounting = true;
                }
            }
            // A stalled end stays due after a restart, slow from the start.
            EventKind::ExecutionEndStalled {
                cause, exit_code, ..
            } if current => {
                if let Some(active) = view.active.as_mut() {
                    active.reported_end = Some((*cause, *exit_code, None));
                    active.follow.end_failures = active.follow.end_failures.max(END_ATTEMPTS);
                }
            }
            EventKind::NativeSessionStarted {
                native_session_id, ..
            } => {
                if view.native.as_ref() != Some(native_session_id) {
                    view.transcript_read = false;
                }
                view.native = Some(native_session_id.clone());
            }
            // A merge of main is no checkpoint of the agent's work: one
            // still due stays due. Nor is a commit of an earlier execution.
            EventKind::ArtifactCommitted { reason, .. }
                if current && *reason != CheckpointReason::BaseMerged =>
            {
                if let Some(active) = view.active.as_mut() {
                    active.checkpoint = None;
                }
            }
            // Checkpoints are the current execution's own: an event of an
            // earlier one that reaches the log late (or one with no
            // execution) asks for none.
            other if current => {
                if let Some(active) = view.active.as_mut()
                    && let Some(reason) = other.checkpoint_reason()
                {
                    active.checkpoint = Some(reason);
                }
            }
            _ => {}
        }
    }

    fn act(&mut self, report: &mut TickReport) -> Result<(), StoreError> {
        // Sessions with no running execution have no `advance` to do it.
        self.flush_unrecorded(report);
        let running: Vec<(SessionId, AgentId, Option<String>, Active)> = self
            .sessions
            .iter()
            .filter_map(|(session, view)| {
                let active = view.active.clone()?;
                Some((
                    session.clone(),
                    view.agent_id.clone(),
                    view.native.clone(),
                    active,
                ))
            })
            .collect();
        let mut ended_now = Vec::new();
        for (session, agent_id, native, mut active) in running {
            let ended =
                self.advance(&agent_id, &session, native.as_deref(), &mut active, report)?;
            if ended {
                // The Bridge's view of what runs changes only by folding the
                // log (the end just recorded, read back below): it never
                // drops an execution the log still holds open, which the
                // log's guard would refuse every start for while the Bridge
                // saw nothing to end.
                ended_now.push((session.clone(), active.execution_id.clone()));
                continue;
            }
            if let Some(held) = self
                .sessions
                .get_mut(&session)
                .and_then(|view| view.active.as_mut())
            {
                // Cleared by `advance` only once committed: a failed
                // checkpoint stays due and is tried again next tick.
                held.checkpoint = active.checkpoint;
                held.reported_end = active.reported_end;
                held.follow = active.follow;
            }
        }
        if !ended_now.is_empty() {
            // What waited for a turn of an execution that ended is never
            // typed: failed now, with how the execution ended.
            self.catch_up()?;
            for (session, execution) in ended_now {
                let cause = self
                    .sessions
                    .get(&session)
                    .and_then(|view| view.inputs.ended.get(&execution).copied());
                self.fail_waiting(&session, cause)?;
            }
        }
        Ok(())
    }

    /// Record what the engine did since the last tick, commit a pending
    /// checkpoint, or end an execution whose process is gone. Returns
    /// whether the execution ended.
    ///
    /// An execution with nothing to say is left alone: its files are read
    /// when the watch says they changed, and its pane captured when a
    /// command is in flight on it, when its transcript has held still and
    /// the screen was not read since (or a reading awaits its twin), or
    /// every [`BridgeConfig::screen_refresh`] as a backstop. Its terminal's
    /// liveness was already asked once for the whole fleet
    /// ([`Self::fleet_runners`]), so an idle fleet costs no call per agent
    /// per pass; a terminal found gone is looked at at once.
    fn advance(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        native: Option<&str>,
        active: &mut Active,
        report: &mut TickReport,
    ) -> Result<bool, StoreError> {
        // Nothing of this session goes into the log before its waiting
        // commit records; one that keeps failing holds up only this session.
        if let Err(error) = self.record_unrecorded_commit(session) {
            report.warnings.push(format!(
                "{session}: a commit record waits, its append failed: {error}"
            ));
            return Ok(false);
        }
        self.register_watch(session, native, active, report);
        self.follow_output(agent_id, session, native, active, report)?;
        let end = if self.looked_at(session, active) {
            match active
                .reported_end
                .or_else(|| self.exit_status(session, active, report))
            {
                Some(end) => Some(end),
                // Running means a runner of the Bridge's own in the terminal,
                // not a terminal of that name: one that outlived its runner
                // (a dead pane, a shell restored after a restart) runs no
                // engine of Spawn's, and would otherwise hold the execution
                // open forever.
                None => match self.not_running(session, active, report) {
                    Ok(None) => {
                        self.follow_screen(agent_id, session, active)?;
                        None
                    }
                    Ok(Some(end)) => Some(end),
                    Err(error) => {
                        report
                            .warnings
                            .push(format!("runner {}: {error}", active.terminal()));
                        None
                    }
                },
            }
        } else {
            None
        };
        if let Some(ending) = end {
            // What the engine wrote last comes before its end.
            self.follow_output(agent_id, session, native, active, report)?;
            return self.end(agent_id, session, active, ending, false, report);
        }
        if let Some(reason) = active.checkpoint {
            // A commit that failed was reported; the checkpoint stays due.
            if self.checkpoint(agent_id, session, active, reason, report)? {
                active.checkpoint = None;
                if reason == CheckpointReason::TurnStopped {
                    self.archive_transcript(agent_id, session, active, report);
                }
            }
        }
        if active.follow.stale.is_some() {
            return self.end(
                agent_id,
                session,
                active,
                (EndCause::Stopped, None, None),
                false,
                report,
            );
        }
        Ok(false)
    }

    /// The exit status the runner wrote when the engine exited, if it has.
    /// An unreadable file (not a regular file, a host error) is warned once
    /// and the engine counts as running: the pane's liveness still ends it.
    ///
    /// The runner writes the signal that killed the engine beside the
    /// status ([`signal_file`]): an engine the OS killed has no exit code,
    /// and without its signal a SIGKILL would read as a clean exit with
    /// none. The end's record carries it (`execution.ended`'s `signal`).
    fn exit_status(
        &self,
        session: &SessionId,
        active: &mut Active,
        report: &mut TickReport,
    ) -> Option<Ending> {
        let bytes = match self
            .terminals
            .read_from(&active.workspace, &exit_file(active), 0)
        {
            Ok(bytes) => bytes?,
            Err(error) => {
                if !active.follow.exit_warned {
                    active.follow.exit_warned = true;
                    report
                        .warnings
                        .push(format!("{session}: exit file: {error}"));
                }
                return None;
            }
        };
        let text = String::from_utf8_lossy(&bytes);
        let signal = self
            .terminals
            .read_from(&active.workspace, &signal_file(active), 0)
            .ok()
            .flatten()
            .and_then(|bytes| String::from_utf8_lossy(&bytes).trim().parse().ok());
        Some((EndCause::Exited, text.trim().parse().ok(), signal))
    }

    /// How the execution ended when no runner of the Bridge's own runs it
    /// any more ([`TerminalHost::runner`]), `None` while one does. The
    /// engine may have written its exit status and closed the pane between
    /// the read before this check and it, so the exit file is read again
    /// first: an exit is an exit. Otherwise it is lost, and what the host
    /// saw is kept for its end record (`follow.evidence`). An error says
    /// nothing about the terminal: the execution is left as it is.
    fn not_running(
        &self,
        session: &SessionId,
        active: &mut Active,
        report: &mut TickReport,
    ) -> Result<Option<Ending>, TerminalError> {
        // The fleet's view of every runner when it is watched
        // ([`Self::fleet_runners`]), else the one terminal asked about.
        let asked = match self.runners.as_ref() {
            Some(runners) => runners.get(&active.terminal()).cloned(),
            None => Some(
                self.terminals
                    .runner(&active.terminal(), &active.workspace)?,
            ),
        };
        let seen = match asked {
            // The fleet's answer has no terminal of that name.
            None | Some(Runner::Gone) => format!("its terminal {} is gone", active.terminal()),
            Some(Runner::Running) => return Ok(None),
            Some(Runner::Untracked(seen)) => seen,
        };
        if let Some(end) = self.exit_status(session, active, report) {
            return Ok(Some(end));
        }
        active.follow.evidence = Some(format!("{seen}; its runner wrote no exit status"));
        Ok(Some((EndCause::Lost, None, None)))
    }

    /// Append the events in what the engine wrote to its transcript since
    /// the last read. Only whole lines are read; a line being written waits
    /// for the next tick, and so does one whose checkpoint failed.
    ///
    /// The read happens when the watch says the file changed, and on every
    /// pass when no watch could be set for the execution
    /// ([`Follow::poll`]). A read that leaves a line half-written, or a
    /// checkpoint uncommitted, keeps the execution due, so the next pass
    /// reads again: a line is never lost to the moment it was written.
    fn follow_output(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        native: Option<&str>,
        active: &mut Active,
        report: &mut TickReport,
    ) -> Result<(), StoreError> {
        let watched = self.config.watch.is_some() && !active.follow.watch.poll;
        if watched && !active.follow.watch.dirty && !self.due_to_read(active) {
            return Ok(());
        }
        let Some(native) = native else {
            return Ok(());
        };
        let path = transcript_path(&self.config.claude_projects, &active.workspace, native);
        let bytes = match self
            .terminals
            .read_from(&active.workspace, &path, active.follow.offset)
        {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                let now = self.clock.now().0;
                let since = *active.follow.first_seen.get_or_insert(now);
                let grace = now.saturating_sub(since) > 60_000;
                if grace && !active.follow.warned {
                    active.follow.warned = true;
                    report
                        .warnings
                        .push(format!("{session}: no engine output at {path}"));
                }
                // The file's appearance is a change the watch reports: with
                // one set, it is not asked again every pass, only once at
                // the grace that reports a transcript that never came.
                active.follow.watch.dirty = false;
                active.follow.due_at =
                    (!active.follow.warned).then(|| since.saturating_add(60_001));
                return Ok(());
            }
            Err(error) => {
                if !active.follow.warned {
                    active.follow.warned = true;
                    report
                        .warnings
                        .push(format!("{session}: read {path}: {error}"));
                }
                // An error says nothing about the file: it is read again on
                // the next pass.
                return Ok(());
            }
        };
        if !bytes.is_empty() {
            // The transcript grew: its screen is looked at once it holds
            // still, not while it is being written.
            active.follow.unread_since = Some(self.clock.now().0);
            active.follow.due_at = None;
        }
        let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') else {
            // A full read without a newline is one line longer than the
            // limit: skip what was read rather than stall on it forever (the
            // rest of it parses as nothing). Anything shorter is a line
            // still being written: it is read next pass.
            if bytes.len() >= READ_LIMIT {
                active.follow.offset += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                active.follow.watch.dirty = false;
                report.warnings.push(format!(
                    "{session}: skipped a line over {READ_LIMIT} bytes in {path}"
                ));
            }
            return Ok(());
        };
        let mut read = 0_usize;
        for line in bytes[..end].split(|byte| *byte == b'\n') {
            let start = read;
            read += line.len() + 1;
            let line = String::from_utf8_lossy(line);
            let events = claude_line(&line);
            // A turn's checkpoint is part of the turn's end: the workspace
            // is committed (and its `artifact.committed` recorded) before
            // the `turn.stopped` or `turn.failed` that ends it, so whoever
            // reads the turn's end from the log also finds the checkpoint
            // of what it wrote. A commit that fails holds the line back: it
            // is read (and committed) again next tick.
            let ends_turn = events
                .iter()
                .any(|event| event.kind.checkpoint_reason() == Some(CheckpointReason::TurnStopped));
            if ends_turn {
                // The screen has not shown this turn over yet, or this pass
                // would not be reading it: see [`Follow::settling`].
                active.follow.settling = Some(self.clock.now().0);
            }
            if ends_turn
                && !self.checkpoint(
                    agent_id,
                    session,
                    active,
                    CheckpointReason::TurnStopped,
                    report,
                )?
            {
                active.follow.offset += u64::try_from(start).unwrap_or(u64::MAX);
                // The commit stays due: the line is read again next pass.
                return Ok(());
            }
            if active.follow.version.is_none()
                && let Some(version) = claude_version(&line)
            {
                self.record_version(agent_id, session, native, active, &path, version)?;
            }
            for event in events {
                self.log.append(NewEvent {
                    agent_id: agent_id.clone(),
                    session_id: session.clone(),
                    execution_id: Some(active.execution_id.clone()),
                    source: Source::Claude,
                    source_event_id: event.source_event_id,
                    kind: event.kind,
                })?;
            }
        }
        active.follow.offset += u64::try_from(end + 1).unwrap_or(u64::MAX);
        active.follow.watch.dirty = false;
        Ok(())
    }

    /// Whether the transcript is read on this pass although nothing was
    /// watched: the grace that reports a transcript that never appeared.
    fn due_to_read(&self, active: &Active) -> bool {
        active
            .follow
            .due_at
            .is_some_and(|at| self.clock.now().0 >= at)
    }

    /// Watch the execution's files: its transcript (once the session's
    /// native id is known) and the runner's exit status. A file that cannot
    /// be watched is read on every pass instead, said once.
    fn register_watch(
        &self,
        session: &SessionId,
        native: Option<&str>,
        active: &mut Active,
        report: &mut TickReport,
    ) {
        let Some(watch) = &self.config.watch else {
            return;
        };
        let key = watch_key(&active.execution_id);
        if self.touched.contains(&key) {
            active.follow.watch.dirty = true;
        } else if !active.follow.watch.dirty {
            // Nothing new and the watch is set: no system call at all. A
            // dirty execution calls again, so a watch the kernel dropped
            // (its directory went) is carried again or falls back.
            return;
        }
        let mut paths = vec![exit_file(active)];
        if let Some(native) = native {
            paths.push(transcript_path(
                &self.config.claude_projects,
                &active.workspace,
                native,
            ));
        }
        for path in paths {
            if let Err(error) = watch.watch(&key, &path) {
                active.follow.watch.poll = true;
                active.follow.watch.dirty = true;
                if !active.follow.watch.said {
                    active.follow.watch.said = true;
                    report.warnings.push(format!(
                        "{session}: no watch on {path} ({error}); its files are read on every pass"
                    ));
                }
                return;
            }
        }
    }

    /// Whether this pass looks at the execution: reads the runner's exit
    /// status, asks whether its runner still runs, and captures its pane.
    /// An execution with nothing in flight, no pending end or checkpoint and
    /// a quiet transcript is left alone between backstops.
    fn looked_at(&self, session: &SessionId, active: &Active) -> bool {
        self.config.watch.is_none()
            || active.reported_end.is_some()
            || active.unmounting
            || active.checkpoint.is_some()
            || active.follow.stale.is_some()
            || active.follow.end_failures > 0
            || active.follow.watch.poll
            || active.follow.watch.dirty
            || self.due_to_read(active)
            || self.in_flight(session)
            || !matches!(self.screen_wanted(session, active), Wanted::Nothing)
            // A terminal killed from outside, or one hosting no runner of
            // ours, is asked about on every pass: its end is due now, and
            // the fleet's answer already holds it.
            || self
                .runners
                .as_ref()
                .is_some_and(|runners| !matches!(runners.get(&active.terminal()), Some(Runner::Running)))
    }

    /// The liveness of every live execution's terminal in one call, when the
    /// fleet's files are watched ([`Self::advance`]); `None` when they are
    /// not, so each execution asks its own terminal as it is advanced.
    fn fleet_runners(&self) -> Option<HashMap<TerminalName, Runner>> {
        self.config.watch.as_ref()?;
        let terminals: Vec<(TerminalName, WorkspaceId)> = self
            .sessions
            .values()
            .filter_map(|view| view.active.as_ref())
            .map(|active| (active.terminal(), active.workspace.clone()))
            .collect();
        if terminals.is_empty() {
            return Some(HashMap::new());
        }
        match self.terminals.runners(&terminals) {
            Ok(states) => Some(states.into_iter().collect()),
            // A failing terminal server says nothing about any execution: it
            // is asked per terminal as it is advanced, as before.
            Err(_) => None,
        }
    }

    /// Whether a command of this session is claimed and unfinished: the
    /// Bridge holds it waiting for its turn.
    fn in_flight(&self, session: &SessionId) -> bool {
        self.held
            .values()
            .any(|claimed| &claimed.session_id == session)
    }

    /// Why this pass wants the pane read, if it does.
    fn screen_wanted(&self, session: &SessionId, active: &Active) -> Wanted {
        let follow = &active.follow;
        // No watch: read on every pass, as before watches. A command in
        // flight reads it too: its next move waits on what the pane says.
        if self.config.watch.is_none() || follow.watch.poll || self.in_flight(session) {
            return Wanted::Now;
        }
        if follow.again {
            return Wanted::Now;
        }
        let now = self.clock.now().0;
        // A turn ended in the transcript and the screen has not shown it
        // over yet: the engine paints the prompt a moment later, and a
        // reading that turns to the prompt decides a delivery. Read on
        // until it does, within the bound the backstop would take — a
        // screen that never settles costs no more than before watches.
        if let Some(ended) = follow.settling {
            let bound = i64::try_from(self.config.screen_refresh.as_millis()).unwrap_or(i64::MAX);
            if now.saturating_sub(ended) < bound {
                return Wanted::Now;
            }
        }
        if let Some(grown) = follow.unread_since {
            // The transcript grew: wait until it has held still long enough
            // for the screen to say what the transcript cannot (the prompt
            // is back), then read it until two readings agree.
            let quiet = i64::try_from(self.config.screen_quiet.as_millis()).unwrap_or(i64::MAX);
            return if now.saturating_sub(grown) >= quiet {
                Wanted::Now
            } else {
                Wanted::Nothing
            };
        }
        // The first look at an execution is not a backstop: nothing is
        // known about its screen yet.
        let Some(last) = follow.last_capture else {
            return Wanted::Now;
        };
        // The backstop: a screen that changes with nothing in the
        // transcript (a dialog, a crash to a shell).
        let refresh = i64::try_from(self.config.screen_refresh.as_millis()).unwrap_or(i64::MAX);
        if now.saturating_sub(last) >= refresh {
            Wanted::Backstop
        } else {
            Wanted::Nothing
        }
    }

    /// Whether the pane is captured on this pass: a change or a command in
    /// flight is read at once, a backstop only within the passes' budget, so
    /// a fleet whose screens fall due together does not spend one pass
    /// capturing them all.
    fn capture_due(&self, session: &SessionId, active: &Active) -> bool {
        match self.screen_wanted(session, active) {
            Wanted::Now => true,
            Wanted::Nothing => false,
            Wanted::Backstop => {
                let taken = self
                    .backstops
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                taken < BACKSTOPS_PER_PASS
            }
        }
    }

    /// The engine's version, read from the execution's transcript: the
    /// native session is recorded again with it, once per execution.
    fn record_version(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        native: &str,
        active: &mut Active,
        path: &str,
        version: String,
    ) -> Result<(), StoreError> {
        active.follow.version = Some(version.clone());
        self.append(
            agent_id,
            session,
            Some(&active.execution_id),
            Source::Claude,
            Some(format!("{}:engine_version", active.execution_id)),
            EventKind::NativeSessionStarted {
                engine: ENGINE.to_string(),
                native_session_id: native.to_string(),
                transcript_path: Some(path.to_string()),
                engine_version: Some(version),
            },
        )
    }

    /// Read the pane and record its state when it changes, or again after
    /// [`BridgeConfig::screen_refresh`] while a turn keeps it busy.
    fn follow_screen(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &mut Active,
    ) -> Result<(), StoreError> {
        if !self.capture_due(session, active) {
            return Ok(());
        }
        let Ok(pane) =
            self.terminals
                .capture_tail(&active.terminal(), &active.workspace, TAIL_LINES)
        else {
            // A pane that cannot be read says nothing: the reading is due
            // again on the next pass, never read as a state (§14).
            active.follow.again = self.config.watch.is_some() && !active.follow.watch.poll;
            return Ok(());
        };
        let now = self.clock.now().0;
        active.follow.last_capture = Some(now);
        active.follow.unread_since = None;
        let reading = read_claude_screen(&pane);
        // A reading that accepts input is the screen showing the turn over:
        // what a turn's end in the transcript was waiting for.
        if reading.state.accepts_input() {
            active.follow.settling = None;
        }
        let confirmed = active.follow.seen == Some(reading.state);
        active.follow.seen = Some(reading.state);
        let due = match active.follow.screen {
            None => true,
            Some((state, _)) if state != reading.state => true,
            Some((state, at)) => {
                state == ScreenState::Busy
                    && now.saturating_sub(at)
                        >= i64::try_from(self.config.screen_refresh.as_millis()).unwrap_or(i64::MAX)
            }
        };
        // Unknown is never a fact, and one reading is not one either: what
        // the log cannot say now is read again next pass.
        let record = due && confirmed && reading.state != ScreenState::Unknown;
        active.follow.again = due && !record;
        if !record {
            return Ok(());
        }
        active.follow.screen = Some((reading.state, now));
        self.append(
            agent_id,
            session,
            Some(&active.execution_id),
            Source::Bridge,
            None,
            EventKind::ScreenObserved {
                state: reading.state,
                evidence: reading.evidence,
            },
        )?;
        Ok(())
    }

    /// Kill, final checkpoint, unmount, then `execution.ended`. Returns
    /// false when the terminal's runner outlived its kill or the final
    /// checkpoint failed; the execution then stays active, its workspace
    /// mounted, and the next tick tries the same end again. It is false too
    /// while the kill still waits off the loop for the engine's processes
    /// ([`Self::kill`]; `active.follow.killing` is then set): the end is
    /// pending, not failed, and the next attempt goes on from it.
    ///
    /// The kill comes first, and returns only once the runner and every
    /// process it started have exited ([`TerminalHost::terminate`]): an
    /// engine that is stopped writes until it is gone, and a checkpoint
    /// taken before that missed its last writes, which the unmount then
    /// deleted (028's stop, a line written after the checkpoint lost). So
    /// nothing the engine wrote can land after the final checkpoint. Every
    /// end takes this way: a stop, an exit (the engine gone, what it left
    /// running not), a lost runner and an abandoned start.
    ///
    /// A failed final checkpoint never leads to an unmount: its workspace
    /// holds the only copy of what the execution did not commit; nor does a
    /// runner that outlived its kill, which may still write there. After
    /// [`END_ATTEMPTS`] failures of either the end is stalled:
    /// `execution.end_stalled` is recorded and warned once, and the end is
    /// retried every [`END_RETRY_MS`] until it gets through. Only `discard`
    /// (a person's or the supervisor's decision) unmounts a workspace whose
    /// final checkpoint failed, and only once its runner is gone.
    fn end(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &mut Active,
        ending: Ending,
        discard: bool,
        report: &mut TickReport,
    ) -> Result<bool, StoreError> {
        let (cause, exit_code, signal) = ending;
        active.reported_end = Some(ending);
        // The checkpoint and the terminal are behind an end that recorded
        // its unmount: what is left is the unmount and the record.
        if active.unmounting {
            if self.kill_waits(active) {
                return Ok(false);
            }
            // A restart may find the engine's terminal still there.
            let Some(killed) = self.kill(session, active, report) else {
                return Ok(false);
            };
            if let Err(error) = killed {
                active.follow.kill_failed_at = Some(self.clock.now().0);
                report
                    .warnings
                    .push(format!("terminate {}: {error}", active.terminal()));
                return Ok(false);
            }
            active.follow.kill_failed_at = None;
            return self.finish_end(agent_id, session, active, ending, report);
        }
        // A checkpoint whose record waits is recorded before the end.
        if let Err(error) = self.record_unrecorded_commit(session) {
            report.warnings.push(format!(
                "{session}: a commit record waits, its append failed: {error}; the end is \
                 tried again"
            ));
            return Ok(false);
        }
        let now = self.clock.now().0;
        let stalled = active.follow.end_failures >= END_ATTEMPTS;
        if stalled
            && !discard
            && active
                .follow
                .end_tried_at
                .is_some_and(|at| now.saturating_sub(at) < END_RETRY_MS)
        {
            return Ok(false);
        }
        // A runner that outlived its last kill: the next waits, so a runner
        // that will not die is killed (and waited for, off the loop) once
        // per END_RETRY_MS, not every tick.
        if self.kill_waits(active) {
            return Ok(false);
        }
        active.follow.end_tried_at = Some(now);
        // The runner first, gone with everything it started: what the engine
        // writes as it dies is in the checkpoint below. A kill still waiting
        // for its processes leaves the end pending (no failure): the next
        // attempt reads it.
        let Some(killed) = self.kill(session, active, report) else {
            return Ok(false);
        };
        if let Err(error) = killed {
            active.follow.kill_failed_at = Some(now);
            let error = format!("terminate {}: {error}", active.terminal());
            report.warnings.push(error.clone());
            return self.end_failed(agent_id, session, active, (false, error), report);
        }
        active.follow.kill_failed_at = None;
        let committed = self.checkpoint(
            agent_id,
            session,
            active,
            CheckpointReason::ExecutionEnded,
            report,
        )?;
        if !committed && discard {
            report.warnings.push(format!(
                "{session}: final checkpoint failed; discarded: unmounting {} with what it \
                 did not commit",
                active.workspace
            ));
        } else if !committed {
            let error = report.warnings.last().cloned().unwrap_or_default();
            return self.end_failed(agent_id, session, active, (true, error), report);
        }
        // The transcript is whole now, and a container's copy goes next.
        self.archive_transcript(agent_id, session, active, report);
        self.append(
            agent_id,
            session,
            Some(&active.execution_id),
            Source::Bridge,
            Some(format!("{}:unmounting", active.execution_id)),
            EventKind::ExecutionUnmounting,
        )?;
        active.unmounting = true;
        self.finish_end(
            agent_id,
            session,
            active,
            (cause, exit_code, signal),
            report,
        )
    }

    /// The last of an end, from its recorded `execution.unmounting` (the
    /// terminal is gone): the workspace unmounted (an absent one is no
    /// failure) and `execution.ended` recorded.
    fn finish_end(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &Active,
        ending: Ending,
        report: &mut TickReport,
    ) -> Result<bool, StoreError> {
        let (cause, exit_code, signal) = ending;
        if let Err(error) = self.workspaces.unmount(&active.workspace) {
            report
                .warnings
                .push(format!("unmount {}: {error}", active.workspace));
        }
        self.append(
            agent_id,
            session,
            Some(&active.execution_id),
            Source::Bridge,
            Some(format!("{}:ended", active.execution_id)),
            EventKind::ExecutionEnded {
                cause,
                exit_code,
                signal,
                evidence: active.follow.evidence.clone(),
            },
        )?;
        Ok(true)
    }

    /// Kill `active`'s terminal ([`TerminalHost::terminate`]): its result,
    /// or `None` while it still waits for the engine's processes to exit.
    ///
    /// A host whose kill waits (tmux: until every process of the session is
    /// gone, up to its bound) has it run on a thread of its own, so the
    /// Bridge's loop goes on with every other session meanwhile (P7: the
    /// loop is serial, and a stop held it up to 7 s). The end waits for it
    /// in line for [`KILL_IN_LINE`] only, and otherwise reads it again at
    /// its next attempt (`active.follow.killing`), each tick. Each kill's
    /// wait goes in the tick's notes: how long ends take, measured live.
    fn kill(
        &self,
        session: &SessionId,
        active: &mut Active,
        report: &mut TickReport,
    ) -> Option<Result<(), TerminalError>> {
        let terminal = active.terminal();
        let (result, waited) = if self.terminals.terminate_waits() {
            // A kill already under way is read without waiting: it had its
            // time in line when it started.
            let pending = active.follow.killing.take();
            let in_line = if pending.is_some() {
                std::time::Duration::ZERO
            } else {
                KILL_IN_LINE
            };
            let killing = pending.unwrap_or_else(|| {
                let killing = Killing {
                    done: Arc::new((std::sync::Mutex::new(None), std::sync::Condvar::new())),
                };
                let done = Arc::clone(&killing.done);
                let terminals = Arc::clone(&self.terminals);
                let (name, workspace) = (terminal.clone(), active.workspace.clone());
                let thread = std::thread::spawn(move || {
                    let started = std::time::Instant::now();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        terminals.terminate(&name, &workspace)
                    }))
                    .unwrap_or_else(|_| Err(TerminalError("the kill panicked".to_string())));
                    let (slot, ready) = &*done;
                    *slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some((result, started.elapsed()));
                    ready.notify_all();
                });
                let mut threads = self
                    .kill_threads
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                threads.retain(|thread| !thread.is_finished());
                threads.push(thread);
                killing
            });
            let (slot, ready) = &*killing.done;
            let guard = slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (mut guard, _) = ready
                .wait_timeout_while(guard, in_line, |slot| slot.is_none())
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(killed) = guard.take() else {
                drop(guard);
                active.follow.killing = Some(killing);
                return None;
            };
            killed
        } else {
            let started = std::time::Instant::now();
            let result = self.terminals.terminate(&terminal, &active.workspace);
            (result, started.elapsed())
        };
        report.notes.push(format!(
            "{session}: the kill of {terminal} waited {} ms for its processes{}",
            waited.as_millis(),
            if result.is_ok() { "" } else { ", and failed" }
        ));
        Some(result)
    }

    /// Whether the end's last kill failed within [`END_RETRY_MS`]: the next
    /// kill waits for it.
    fn kill_waits(&self, active: &Active) -> bool {
        let now = self.clock.now().0;
        active
            .follow
            .kill_failed_at
            .is_some_and(|at| now.saturating_sub(at) < END_RETRY_MS)
    }

    /// One end attempt failed, at its kill (`checkpoint` false: the runner
    /// outlived it) or at its final checkpoint: counted, said, and at
    /// [`END_ATTEMPTS`] the end is stalled. The workspace stays mounted.
    fn end_failed(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &mut Active,
        failure: (bool, String),
        report: &mut TickReport,
    ) -> Result<bool, StoreError> {
        let (checkpoint, error) = failure;
        active.follow.end_failures = active.follow.end_failures.saturating_add(1);
        let failures = active.follow.end_failures;
        let what = if checkpoint {
            "final checkpoint failed"
        } else {
            "the runner outlived its kill"
        };
        if failures < END_ATTEMPTS {
            report.warnings.push(format!(
                "{session}: {what} ({failures} of {END_ATTEMPTS}); the workspace stays mounted \
                 and the end is tried again"
            ));
        } else if failures == END_ATTEMPTS {
            self.stall(agent_id, session, active, (checkpoint, error), report)?;
        }
        Ok(false)
    }

    /// The end failed [`END_ATTEMPTS`] times (its final checkpoint, or its
    /// kill, which the runner outlived): record `execution.end_stalled` and
    /// warn, once. The workspace stays, with all of the engine's work.
    fn stall(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &Active,
        failure: (bool, String),
        report: &mut TickReport,
    ) -> Result<(), StoreError> {
        let (checkpoint, error) = failure;
        report.warnings.push(if checkpoint {
            format!(
                "{session}: FINAL CHECKPOINT FAILED {END_ATTEMPTS} TIMES; the terminal is down, \
                 {} STAYS MOUNTED with everything it holds and the checkpoint is retried every \
                 {} s. Fix the cause ({error}); a discard command gives the uncommitted work up",
                active.workspace,
                END_RETRY_MS / 1000
            )
        } else {
            format!(
                "{session}: THE RUNNER OUTLIVED ITS KILL {END_ATTEMPTS} TIMES; {} STAYS MOUNTED \
                 and is not checkpointed while something of the engine's may still write there; \
                 the kill is retried every {} s. Find what keeps running ({error})",
                active.workspace,
                END_RETRY_MS / 1000
            )
        });
        let (cause, exit_code, _) = active.reported_end.unwrap_or((EndCause::Lost, None, None));
        self.append(
            agent_id,
            session,
            Some(&active.execution_id),
            Source::Bridge,
            Some(format!("{}:end_stalled", active.execution_id)),
            EventKind::ExecutionEndStalled {
                cause,
                exit_code,
                failures: END_ATTEMPTS,
                error,
            },
        )
    }

    /// Commit the workspace and record a new commit. Returns whether the
    /// workspace's state is now on its branch, or kept off it by a store
    /// that answered [`WORKSPACE_STALE`] (`follow.stale` then holds why and the
    /// execution ends); false: the commit failed and was reported.
    fn checkpoint(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &mut Active,
        reason: CheckpointReason,
        report: &mut TickReport,
    ) -> Result<bool, StoreError> {
        let message = format!("spawn checkpoint {}", active.execution_id);
        match self.workspaces.commit(&active.workspace, &message) {
            Ok(Commit::Unchanged) => {}
            Ok(Commit::Created(commit)) => {
                let artifact = ArtifactRef {
                    project: self.workspaces.project().to_string(),
                    branch: BranchName::for_agent(agent_id),
                    commit,
                };
                self.record_commit(
                    agent_id,
                    session,
                    Some(&active.execution_id),
                    Source::Bridge,
                    Some(format!("commit:{}", artifact.commit)),
                    EventKind::ArtifactCommitted {
                        reason,
                        artifact,
                        files: Vec::new(),
                    },
                )?;
            }
            // The store kept the workspace's state off its branch: an
            // unmount loses nothing, and the execution ends.
            Err(WorkspaceError::Refused { code, message }) if code == WORKSPACE_STALE => {
                if active.follow.stale.is_none() {
                    report.warnings.push(format!(
                        "{session}: commit refused {code}: {message} The execution ends."
                    ));
                }
                active.follow.stale = Some(message);
            }
            Err(WorkspaceError::Refused { code, message }) => {
                report
                    .warnings
                    .push(format!("commit refused {code}: {message}"));
                return Ok(false);
            }
            Err(WorkspaceError::Unavailable(text)) => {
                report.warnings.push(format!("commit unavailable: {text}"));
                return Ok(false);
            }
        }
        // Logged once, and carried by the next prompt the Bridge types.
        let notices = self.workspaces.notices(&active.workspace);
        if !notices.is_empty()
            && let Ok(mut warned) = self.warned.lock()
        {
            let seen = warned.entry(session.clone()).or_default();
            for notice in notices {
                if !seen.contains(&notice) {
                    report.warnings.push(format!("{session}: {notice}"));
                    seen.insert(notice);
                }
            }
        }
        Ok(true)
    }

    fn append(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        execution: Option<&ExecutionId>,
        source: Source,
        key: Option<String>,
        kind: EventKind,
    ) -> Result<(), StoreError> {
        self.log.append(NewEvent {
            agent_id: agent_id.clone(),
            session_id: session.clone(),
            execution_id: execution.cloned(),
            source,
            source_event_id: key,
            kind,
        })?;
        Ok(())
    }

    /// Append the `artifact.committed` of a commit the branch already took.
    /// The branch moved first, so an append that fails must not lose the
    /// record: it waits in `unrecorded_commits` and the session's next tick
    /// appends it before anything else ([`Self::record_unrecorded_commit`]).
    /// Its key (`commit:<id>`) makes a second append of one that did land
    /// a no-op.
    fn record_commit(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        execution: Option<&ExecutionId>,
        source: Source,
        key: Option<String>,
        kind: EventKind,
    ) -> Result<(), StoreError> {
        let event = NewEvent {
            agent_id: agent_id.clone(),
            session_id: session.clone(),
            execution_id: execution.cloned(),
            source,
            source_event_id: key,
            kind,
        };
        self.unrecorded_commits_lock()
            .entry(session.clone())
            .or_default()
            .push(event.clone());
        self.record_unrecorded_commit(session)
    }

    /// Append, in order, the commit records of `session` whose append
    /// failed; each leaves the list once the log took it.
    fn record_unrecorded_commit(&self, session: &SessionId) -> Result<(), StoreError> {
        loop {
            let Some(event) = self
                .unrecorded_commits_lock()
                .get(session)
                .and_then(|waiting| waiting.first().cloned())
            else {
                return Ok(());
            };
            self.log.append(event)?;
            let mut unrecorded = self.unrecorded_commits_lock();
            if let Some(waiting) = unrecorded.get_mut(session) {
                waiting.remove(0);
                if waiting.is_empty() {
                    unrecorded.remove(session);
                }
            }
        }
    }

    fn unrecorded_commits_lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<SessionId, Vec<NewEvent>>> {
        self.unrecorded_commits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn handle(
        &mut self,
        claimed: &Claimed,
        report: &mut TickReport,
    ) -> Result<Handled, StoreError> {
        let action = match claimed.action() {
            Ok(action) => action,
            // The parse error names what is wrong (a key outside the set
            // names the key).
            Err(error) => {
                return Ok(Handled::Finished(Outcome::Failed(format!(
                    "stored action is not valid: {error}"
                ))));
            }
        };
        // What a command appends comes after the session's waiting commit
        // records. While the log refuses them the command is not handled
        // (the error ends the tick; the lease brings the command back).
        self.record_unrecorded_commit(&claimed.session_id)?;
        // An end whose kill still waits off the loop (Self::kill): every
        // command but a stop or a discard waits for it, claimed again after
        // `input_wait_retry`, and meets the session as the end leaves it:
        // an input is never typed into a terminal being killed (it fails
        // once the end is done, as every waiting input does), and a start or
        // a resume goes on once nothing runs.
        if !matches!(action, CommandAction::Stop | CommandAction::Discard)
            && self
                .running(&claimed.session_id)
                .is_some_and(|active| active.follow.killing.is_some())
        {
            return Ok(Handled::Waiting);
        }
        // An execution the log holds open that nothing runs any more is
        // ended first, so the command meets the session as it is.
        let settled = match &action {
            CommandAction::Start { .. }
            | CommandAction::Resume { .. }
            | CommandAction::Input { .. }
            | CommandAction::Key { .. } => self.settle(claimed, report)?,
            CommandAction::Stop | CommandAction::Discard => None,
        };
        match (&action, settled) {
            (_, None) => {}
            (_, Some(Settled::Pending)) => return Ok(Handled::Waiting),
            (_, Some(Settled::Ending(why))) => {
                return Ok(Handled::Finished(Outcome::Failed(why)));
            }
            (CommandAction::Input { .. }, Some(Settled::Ended(what))) => {
                return Ok(Handled::Finished(Outcome::Failed(format!(
                    "{what}; an input needs a running engine: resume the session"
                ))));
            }
            (CommandAction::Key { .. }, Some(Settled::Ended(what))) => {
                return Ok(Handled::Finished(Outcome::Failed(format!(
                    "{what}; a key needs a running engine: resume the session"
                ))));
            }
            // A start or resume goes on, on a session that runs nothing.
            (_, Some(Settled::Ended(what))) => report.warnings.push(what),
        }
        let outcome = match action {
            CommandAction::Start {
                engine: _,
                prompt,
                parent_session_id,
                from,
                model,
                // The control layer's record of the charter it gave: nothing
                // the Bridge does depends on it.
                charter: _,
            } => self.start(
                claimed,
                &Begin {
                    prompt: &prompt,
                    parent: parent_session_id.as_ref(),
                    native: None,
                    from: from.as_deref(),
                    model,
                },
                report,
            ),
            CommandAction::Input { text, .. } => return self.input(claimed, &text),
            CommandAction::Key { key } => self.press(claimed, key),
            CommandAction::Stop => return self.stop(claimed, false, report),
            CommandAction::Discard => return self.stop(claimed, true, report),
            CommandAction::Resume { prompt, model } => {
                self.resume(claimed, prompt.as_deref(), model, report)
            }
        }?;
        Ok(Handled::Finished(outcome))
    }

    fn running(&self, session: &SessionId) -> Option<Active> {
        self.sessions.get(session)?.active.clone()
    }

    /// The session's open execution: the Bridge's view's, or, when that
    /// runs none, the log's ([`EventLog::open_execution`]), taken back into
    /// the view as its `execution.started` has it, with a warning. The view
    /// follows the log, but one that missed a record would otherwise answer
    /// "runs no execution" while the log refuses every start until an end
    /// that nothing would write. Taking the record back adopts no engine:
    /// what runs it is checked next ([`Self::settle`], the stop's end).
    fn open(
        &mut self,
        claimed: &Claimed,
        report: &mut TickReport,
    ) -> Result<Option<Active>, StoreError> {
        if let Some(active) = self.running(&claimed.session_id) {
            return Ok(Some(active));
        }
        let Some(active) = self
            .log
            .open_execution(&claimed.session_id)?
            .and_then(|record| self.track_from_log(&claimed.session_id, &record))
        else {
            return Ok(None);
        };
        report.warnings.push(format!(
            "{}: the log holds execution {} open and this Bridge's view had none; it is \
             followed again",
            claimed.session_id, active.execution_id
        ));
        Ok(Some(active))
    }

    /// The open execution the log answered with (`record`, an
    /// `execution.started`), put into the session's view as the fold would
    /// have: the view follows the log again.
    fn track_from_log(&mut self, session: &SessionId, record: &EventRecord) -> Option<Active> {
        let EventKind::ExecutionStarted {
            workspace,
            transcript_from,
            ..
        } = &record.event.kind
        else {
            return None;
        };
        let active = Active::started(
            record.event.execution_id.clone()?,
            self.workspaces.canonical(workspace),
            *transcript_from,
        );
        if let Some(view) = self.sessions.get_mut(session) {
            view.active = Some(active.clone());
        }
        Some(active)
    }

    /// Before a start, a resume or an input acts on its session: the
    /// session's open execution (the log's, which the Bridge's view
    /// follows) is checked against what runs. One that no runner of the
    /// Bridge's own runs any more (its terminal gone, or there and hosting
    /// no runner: a dead pane, a shell restored after a restart) is ended
    /// now, through the end every execution takes (final checkpoint,
    /// terminal killed, unmount, `execution.ended`), as `exited` when its
    /// runner left an exit status and as `lost` with what the host saw
    /// otherwise. Without this the command would fail on an execution that
    /// will never answer: a start or resume refused because the session
    /// "already runs" one, an input refused at a pane that hosts no engine,
    /// until a later tick ended it, or forever while the terminal stays.
    ///
    /// Nothing to settle (`None`): no open execution, a running one, an end
    /// already under way (the normal path finishes it), the command's own
    /// execution (a retried start goes on from it), or a host that cannot
    /// tell. The engine in an untracked terminal is never adopted: it is
    /// ended, and the terminal killed.
    fn settle(
        &mut self,
        claimed: &Claimed,
        tick: &mut TickReport,
    ) -> Result<Option<Settled>, StoreError> {
        let Some(mut active) = self.open(claimed, tick)? else {
            return Ok(None);
        };
        if active.execution_id == ExecutionId::for_command(claimed.command_id)
            || active.reported_end.is_some()
            || active.unmounting
        {
            return Ok(None);
        }
        let mut report = TickReport::default();
        let Ok(Some((cause, exit_code, signal))) =
            self.not_running(&claimed.session_id, &mut active, &mut report)
        else {
            return Ok(None);
        };
        let seen = active
            .follow
            .evidence
            .clone()
            .unwrap_or_else(|| format!("its runner exited ({exit_code:?})"));
        if let Some(evidence) = active.follow.evidence.take() {
            active.follow.evidence = Some(format!("{evidence}; found by {}", claimed.command_id));
        }
        let ended = self.end(
            &claimed.agent_id,
            &claimed.session_id,
            &mut active,
            (cause, exit_code, signal),
            false,
            &mut report,
        )?;
        self.catch_up()?;
        tick.notes.append(&mut report.notes);
        if ended {
            // The command being handled is finished by the tick, with its
            // own outcome; every other input waiting on the execution fails.
            self.held.remove(&claimed.command_id);
            self.fail_waiting(&claimed.session_id, Some(cause))?;
            tick.warnings.append(&mut report.warnings);
            return Ok(Some(Settled::Ended(format!(
                "{}'s execution {} was not running ({seen}); it ended as {}",
                claimed.session_id,
                active.execution_id,
                cause_name(cause)
            ))));
        }
        // Its end stays due: each tick tries it again, as a stop's.
        let pending = active.follow.killing.is_some();
        if let Some(held) = self
            .sessions
            .get_mut(&claimed.session_id)
            .and_then(|view| view.active.as_mut())
        {
            held.reported_end = active.reported_end;
            held.follow.end_failures = active.follow.end_failures;
            held.follow.end_tried_at = active.follow.end_tried_at;
            held.follow.kill_failed_at = active.follow.kill_failed_at;
            held.follow.killing.clone_from(&active.follow.killing);
            held.follow.evidence = active.follow.evidence;
        }
        if pending {
            // Its kill still waits for the engine's processes: no failure.
            tick.warnings.append(&mut report.warnings);
            return Ok(Some(Settled::Pending));
        }
        report.warnings.insert(
            0,
            format!(
                "{}'s execution {} is not running ({seen}) and its end is not done yet",
                claimed.session_id, active.execution_id
            ),
        );
        Ok(Some(Settled::Ending(report.warnings.join("; "))))
    }

    fn start(
        &mut self,
        claimed: &Claimed,
        begin: &Begin<'_>,
        report: &mut TickReport,
    ) -> Result<Outcome, StoreError> {
        let Begin {
            prompt,
            parent,
            native,
            from,
            model,
        } = *begin;
        // Another Bridge may have started the session since this tick read
        // the log; the log refuses a second start either way.
        self.catch_up()?;
        if let Some(outcome) = self.started_before(claimed, prompt) {
            return outcome;
        }
        let model = model.unwrap_or_else(|| {
            self.sessions
                .get(&claimed.session_id)
                .map_or(ModelProfile::Default, |view| view.model)
        });
        let settings = match self.engine_settings(claimed, model, report) {
            Ok(settings) => settings,
            Err(why) => return Ok(Outcome::Failed(why)),
        };
        let branch = BranchName::for_agent(&claimed.agent_id);
        let fork = match self.ensure_branch(claimed, &branch, from, report) {
            Ok(fork) => fork,
            Err(why) => return Ok(Outcome::Failed(why)),
        };
        self.merge_main_at_start(claimed, &branch, report)?;
        let workspace = match self.workspaces.mount(&branch) {
            Ok(workspace) => workspace,
            Err(error) => return Ok(Outcome::Failed(format!("mount {branch}: {error}"))),
        };
        // Recorded, so a restart that finds no execution using it unmounts it
        // ([`Self::release_orphan_mounts`]).
        if let Err(error) = self.step(claimed, None, CommandStep::Mounted, Some(&workspace)) {
            self.give_up_mount(claimed, &workspace, error.to_string());
            return Err(error);
        }
        // A resumed session's transcript holds the earlier executions' lines:
        // this execution's are the ones after its length now.
        let transcript_from = match native
            .map(|native| self.resumed_transcript(claimed, &workspace, native, report))
            .transpose()
        {
            Ok(length) => length.flatten(),
            Err(why) => return Ok(self.give_up_mount(claimed, &workspace, why)),
        };
        if let Some(parent_session) = parent {
            let linked = self.append(
                &claimed.agent_id,
                &claimed.session_id,
                None,
                Source::Bridge,
                Some(format!("{}:linked:{parent_session}", claimed.session_id)),
                EventKind::SessionLinked {
                    parent_session_id: parent_session.clone(),
                },
            );
            if let Err(error) = linked {
                self.give_up_mount(claimed, &workspace, error.to_string());
                return Err(error);
            }
        }
        let active = Active {
            execution_id: ExecutionId::for_command(claimed.command_id),
            workspace,
            checkpoint: None,
            reported_end: None,
            unmounting: false,
            follow: Follow::default(),
        };
        // The event goes first: a crash after it leaves a recorded execution
        // whose missing terminal the next tick ends as lost.
        let recorded = self.append(
            &claimed.agent_id,
            &claimed.session_id,
            Some(&active.execution_id),
            Source::Bridge,
            Some(format!("{}:started", active.execution_id)),
            EventKind::ExecutionStarted {
                engine: ENGINE.to_string(),
                workspace: active.workspace.clone(),
                command_id: claimed.command_id,
                fork,
                model,
                transcript_from,
            },
        );
        match recorded {
            Ok(()) => {}
            Err(StoreError::Rejected(why)) => {
                return self.refused_start(claimed, &active.workspace, why);
            }
            // Whether it reached the log is unknown: the mount goes either
            // way. A recorded execution without its terminal or workspace
            // ends as lost on the next tick.
            Err(error) => {
                self.give_up_mount(claimed, &active.workspace, error.to_string());
                return Err(error);
            }
        }
        self.launch_and_prompt(claimed, &active, native, &settings, prompt)
    }

    /// A retried Start or Resume whose execution the log already records
    /// (the tick that handled it died, or its finish failed): go on from
    /// what the log says it did. An engine whose terminal is gone ends as
    /// lost on the next tick; a prompt is typed only when the log shows it
    /// was not ([`SessionView::typing`]), and an engine whose prompt may or
    /// may not have arrived is ended rather than typed into again or left
    /// waiting at an empty prompt.
    fn go_on_starting(
        &mut self,
        claimed: &Claimed,
        active: &Active,
        prompt: &str,
    ) -> Result<Outcome, StoreError> {
        // Nothing is typed into a terminal that does not host the runner.
        match self.terminals.runner(&active.terminal(), &active.workspace) {
            Ok(Runner::Running) => {}
            Ok(Runner::Gone) => {
                return Ok(Outcome::Failed(format!(
                    "{}: the engine's terminal is gone; the execution ends as lost",
                    claimed.session_id
                )));
            }
            Ok(Runner::Untracked(seen)) => {
                return Ok(Outcome::Failed(format!(
                    "{}: {seen}; the execution ends as lost",
                    claimed.session_id
                )));
            }
            Err(error) => {
                return Ok(Outcome::Failed(format!(
                    "runner {}: {error}",
                    active.terminal()
                )));
            }
        }
        if prompt.is_empty() {
            return Ok(Outcome::Succeeded);
        }
        match self.typing_of(claimed, &active.execution_id) {
            Typing::Done => Ok(Outcome::Succeeded),
            Typing::Unknown => self.abandon(
                claimed,
                active,
                format!("the prompt {MAY_HAVE_TYPED}"),
                false,
            ),
            Typing::Due => self.prompt_engine(claimed, active, prompt),
        }
    }

    /// What a Start or Resume does when the session already runs an
    /// execution or this command's ran: it goes on with its own execution
    /// ([`Self::go_on_starting`]) and refuses otherwise. `None` when it
    /// starts afresh.
    fn started_before(
        &mut self,
        claimed: &Claimed,
        prompt: &str,
    ) -> Option<Result<Outcome, StoreError>> {
        if let Some(active) = self.running(&claimed.session_id) {
            if active.execution_id == ExecutionId::for_command(claimed.command_id) {
                return Some(self.go_on_starting(claimed, &active, prompt));
            }
            return Some(Ok(Outcome::Failed(format!(
                "{} already runs an execution",
                claimed.session_id
            ))));
        }
        self.recorded_steps(claimed).started.then(|| {
            Ok(Outcome::Failed(format!(
                "{}: this command's execution already ran and ended",
                claimed.session_id
            )))
        })
    }

    /// The command's recorded steps.
    fn recorded_steps(&self, claimed: &Claimed) -> Steps {
        self.sessions
            .get(&claimed.session_id)
            .and_then(|view| view.steps.get(&claimed.command_id))
            .cloned()
            .unwrap_or_default()
    }

    /// Whether the command's typing into `execution` is due, done or unknown.
    fn typing_of(&self, claimed: &Claimed, execution: &ExecutionId) -> Typing {
        self.sessions
            .get(&claimed.session_id)
            .map_or(Typing::Due, |view| {
                view.typing(claimed.command_id, execution)
            })
    }

    /// Record a step of the command being handled, once (keyed by command
    /// and step).
    fn step(
        &self,
        claimed: &Claimed,
        execution: Option<&ExecutionId>,
        step: CommandStep,
        workspace: Option<&WorkspaceId>,
    ) -> Result<(), StoreError> {
        let name = match step {
            CommandStep::Mounted => "mounted",
            CommandStep::MountReleased => "mount_released",
            CommandStep::Typing => "typing",
            CommandStep::Typed => "typed",
            CommandStep::Waiting => "waiting",
            CommandStep::Pressing => "pressing",
            CommandStep::Pressed => "pressed",
        };
        self.append(
            &claimed.agent_id,
            &claimed.session_id,
            execution,
            Source::Bridge,
            Some(format!("{}:{name}", claimed.command_id)),
            EventKind::CommandStepped {
                command_id: claimed.command_id,
                step,
                workspace: workspace.cloned(),
            },
        )
    }

    /// A key command: its key pressed in the running engine's pane, at
    /// once and in any pane state, at most once. Unlike an input it never
    /// waits: Esc interrupts a busy turn, and the arrows, Enter, Y and N
    /// answer a dialog, the very states in which text waits. So it may
    /// overtake an input sent before it that still waits. Handling is
    /// serial, and an input's typing (stage, paste, Enter) is one call
    /// ([`Self::type_once`]), so a key lands before or after it, never
    /// between its paste and its Enter. The intent is recorded before the
    /// key goes (`pressing`) and the send after (`pressed`): a retried key
    /// whose `pressing` has no `pressed` is not sent again.
    fn press(&mut self, claimed: &Claimed, key: NamedKey) -> Result<Outcome, StoreError> {
        let Some(active) = self.running(&claimed.session_id) else {
            return Ok(Outcome::Failed(format!(
                "{} runs no execution: a key needs a running engine; resume the session",
                claimed.session_id
            )));
        };
        match self.recorded_steps(claimed).press {
            Press::Due => {}
            Press::Pressed => return Ok(Outcome::Succeeded),
            Press::Pressing => {
                return Ok(Outcome::Failed(format!(
                    "the key {} may have reached the pane before the Bridge stopped; it is \
                     not sent again",
                    key.name()
                )));
            }
        }
        self.step(
            claimed,
            Some(&active.execution_id),
            CommandStep::Pressing,
            None,
        )?;
        match self
            .terminals
            .send_keys(&active.terminal(), &active.workspace, &[key.tmux()])
        {
            Ok(()) => {
                self.step(
                    claimed,
                    Some(&active.execution_id),
                    CommandStep::Pressed,
                    None,
                )?;
                Ok(Outcome::Succeeded)
            }
            Err(error) => {
                let gone = if self.terminal_gone(&active) {
                    "; the terminal is gone"
                } else {
                    ""
                };
                Ok(Outcome::Failed(format!(
                    "press {}: {error}{gone}",
                    key.name()
                )))
            }
        }
    }

    /// Text into the engine's input box, at most once: the text is staged
    /// first (no key goes), the intent recorded before the keys go and the
    /// completion after, so a retried command or a restarted Bridge can tell
    /// whether it was typed. A staging refused is [`Untyped::Refused`] with
    /// nothing recorded: nothing was typed, and that is what the log says.
    /// What the workspace store has to tell and no prompt carried yet goes
    /// before it, and is marked told in the store once the typing is recorded.
    fn type_once(
        &self,
        claimed: &Claimed,
        active: &Active,
        text: &str,
    ) -> Result<Result<(), Untyped>, StoreError> {
        // What the workspace store has to tell the agent goes first. It is
        // the store's record, not this process's memory, so a restart
        // between the checkpoint and this prompt loses none of it.
        let notices = self.workspaces.notices(&active.workspace);
        let text = if notices.is_empty() {
            text.to_string()
        } else {
            format!("{}\n\n{text}", notices.join("\n\n"))
        };
        let typed = {
            // A merge of main into the workspace waits while this types,
            // and once it gets the workspace it finds the `typed` step
            // recorded: no merge writes the workspace under the turn this
            // starts ([`WorkspaceStore::merge_base_when`]).
            let _held = self.workspaces.hold(&active.workspace);
            if let Err(error) =
                self.terminals
                    .stage_text(&active.terminal(), &active.workspace, &text)
            {
                return Ok(Err(Untyped::Refused(error)));
            }
            self.step(
                claimed,
                Some(&active.execution_id),
                CommandStep::Typing,
                None,
            )?;
            let typed = self
                .terminals
                .submit_staged(&active.terminal(), &active.workspace);
            if typed.is_ok() {
                self.step(
                    claimed,
                    Some(&active.execution_id),
                    CommandStep::Typed,
                    None,
                )?;
            }
            typed
        };
        // After the hold: telling the store takes the branch's lock too.
        if typed.is_ok() && !notices.is_empty() {
            self.workspaces.notices_told(&active.workspace, &notices);
        }
        Ok(typed.map_err(Untyped::Unknown))
    }

    /// Whether `has-session` says the terminal is gone. A host call that
    /// failed on a terminal that is still there, or that cannot be told
    /// (the check failed too), is a transient failure, not a lost terminal.
    fn terminal_gone(&self, active: &Active) -> bool {
        matches!(
            self.terminals
                .is_alive(&active.terminal(), &active.workspace),
            Ok(false)
        )
    }

    /// The engine of a recorded execution started in its terminal, then its
    /// prompt typed (an engine that never got its prompt is abandoned).
    fn launch_and_prompt(
        &mut self,
        claimed: &Claimed,
        active: &Active,
        native: Option<&str>,
        settings: &str,
        prompt: &str,
    ) -> Result<Outcome, StoreError> {
        let fresh = self.name_session(claimed, native, active)?;
        let launch = self.launch(
            claimed,
            active,
            &Run {
                native: fresh.as_deref().or(native),
                resume: native.is_some(),
                settings,
            },
        );
        if let Err(error) = self.terminals.start(&launch) {
            return self.abandon(claimed, active, format!("start terminal: {error}"), true);
        }
        if prompt.is_empty() {
            return Ok(Outcome::Succeeded);
        }
        self.prompt_engine(claimed, active, prompt)
    }

    /// Type a started engine's prompt, answering the engine's trust dialog
    /// first when it shows ([`Self::type_prompt`]). An engine that never got
    /// it must not look alive (whoever started it would wait on it forever):
    /// it is abandoned.
    fn prompt_engine(
        &mut self,
        claimed: &Claimed,
        active: &Active,
        prompt: &str,
    ) -> Result<Outcome, StoreError> {
        let outcome = self.type_prompt(claimed, active, prompt)?;
        if let Outcome::Failed(why) = outcome {
            let discard = self.never_typed(claimed, active)?;
            return self.abandon(claimed, active, why, discard);
        }
        self.catch_up()?;
        Ok(outcome)
    }

    /// Before a resume of `native`: its transcript is where the engine
    /// reads it, put back from the archive when it went missing, or why the
    /// resume cannot go on ([`transcripts::restore`]).
    fn ready_transcript(
        &self,
        claimed: &Claimed,
        workspace: &WorkspaceId,
        native: &str,
        report: &mut TickReport,
    ) -> Result<(), String> {
        let path = transcript_path(&self.config.claude_projects, workspace, native);
        let read_before = self
            .sessions
            .get(&claimed.session_id)
            .is_some_and(|view| view.transcript_read);
        let restored = transcripts::restore(
            self.terminals.as_ref(),
            self.transcripts.as_deref(),
            workspace,
            &path,
            &claimed.agent_id,
            native,
            read_before,
        )?;
        if let Some(bytes) = restored {
            report.warnings.push(format!(
                "{}: {path} was missing; restored {bytes} bytes from the transcript archive",
                claimed.session_id
            ));
        }
        Ok(())
    }

    /// Copy what the engine appended to its transcript into the archive,
    /// if there is one. A failure is warned; the next copy tries again.
    fn archive_transcript(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &Active,
        report: &mut TickReport,
    ) {
        let native = self
            .sessions
            .get(session)
            .and_then(|view| view.native.as_deref());
        let (Some(archive), Some(native)) = (self.transcripts.as_deref(), native) else {
            return;
        };
        let path = transcript_path(&self.config.claude_projects, &active.workspace, native);
        if let Err(error) = transcripts::archive(
            self.terminals.as_ref(),
            archive,
            &active.workspace,
            &path,
            agent_id,
            native,
        ) {
            report
                .warnings
                .push(format!("{session}: archive the transcript: {error}"));
        }
    }

    /// The log refused `execution.started`: the session runs an execution
    /// another Bridge started. The mount goes, unless it is that
    /// execution's own workspace.
    fn refused_start(
        &mut self,
        claimed: &Claimed,
        workspace: &WorkspaceId,
        why: String,
    ) -> Result<Outcome, StoreError> {
        self.catch_up()?;
        let theirs = self
            .running(&claimed.session_id)
            .is_some_and(|active| active.workspace == *workspace);
        if theirs {
            return Ok(Outcome::Failed(why));
        }
        Ok(self.give_up_mount(claimed, workspace, why))
    }

    /// Main reaches an agent's branch only here, when an execution starts or
    /// resumes, and only when it merges cleanly: the merge is recorded as a
    /// `base_merged` commit. A conflict leaves the branch as it is and is
    /// recorded (`merge.conflicted`, `marked: false`, keyed by the branch
    /// head and the files, so a retried start records it once) for the
    /// supervisor and the integrator; the execution starts on the branch as
    /// it is. A store that cannot merge is warned of, and changes nothing.
    fn merge_main_at_start(
        &self,
        claimed: &Claimed,
        branch: &BranchName,
        report: &mut TickReport,
    ) -> Result<(), StoreError> {
        let session = &claimed.session_id;
        match self.workspaces.merge_main_clean(branch) {
            Ok(BaseMerge::Current) => Ok(()),
            Ok(BaseMerge::Merged { commit, files }) => {
                let artifact = ArtifactRef {
                    project: self.workspaces.project().to_string(),
                    branch: branch.clone(),
                    commit,
                };
                self.record_commit(
                    &claimed.agent_id,
                    session,
                    None,
                    Source::Bridge,
                    Some(format!("commit:{}", artifact.commit)),
                    EventKind::ArtifactCommitted {
                        reason: CheckpointReason::BaseMerged,
                        artifact,
                        files,
                    },
                )
            }
            Ok(BaseMerge::Conflicted { files, .. }) => {
                report.warnings.push(format!(
                    "{session}: main conflicts with {branch} in {}; the execution starts on \
                     the branch as it is",
                    files.join(", ")
                ));
                let head = self
                    .workspaces
                    .branch_head(branch)
                    .ok()
                    .flatten()
                    .map_or_else(|| "-".to_string(), |head| head.to_string());
                self.append(
                    &claimed.agent_id,
                    session,
                    None,
                    Source::Bridge,
                    Some(format!("conflict:{head}:{:016x}", files_digest(&files))),
                    EventKind::MergeConflicted {
                        files,
                        marked: false,
                    },
                )
            }
            Err(error) => {
                report.warnings.push(format!(
                    "{session}: merge main into {branch}: {error}; the execution starts on the \
                     branch as it is"
                ));
                Ok(())
            }
        }
    }

    /// Before a resume of `native`: its transcript ready
    /// ([`Self::ready_transcript`]), and its length, which the execution's
    /// own lines come after.
    fn resumed_transcript(
        &self,
        claimed: &Claimed,
        workspace: &WorkspaceId,
        native: &str,
        report: &mut TickReport,
    ) -> Result<Option<u64>, String> {
        self.ready_transcript(claimed, workspace, native, report)?;
        self.transcript_length(workspace, native)
    }

    /// How long the transcript of `native` is, as the engine will see it;
    /// `None` when it does not exist (yet).
    fn transcript_length(
        &self,
        workspace: &WorkspaceId,
        native: &str,
    ) -> Result<Option<u64>, String> {
        let path = transcript_path(&self.config.claude_projects, workspace, native);
        let mut length = 0u64;
        loop {
            match self.terminals.read_from(workspace, &path, length) {
                Ok(None) => return Ok((length > 0).then_some(length)),
                Ok(Some(bytes)) if bytes.is_empty() => return Ok(Some(length)),
                Ok(Some(bytes)) => {
                    length = length.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
                }
                Err(error) => return Err(format!("read {path}: {error}")),
            }
        }
    }

    /// A start that fails after its mount, before anything ran in it: the
    /// mount goes, and the start fails with `why`. The release is recorded
    /// when the log takes it; when it does not, a restart unmounts again
    /// ([`Self::release_orphan_mounts`]).
    fn give_up_mount(&self, claimed: &Claimed, workspace: &WorkspaceId, why: String) -> Outcome {
        if let Err(error) = self.workspaces.unmount(workspace) {
            return Outcome::Failed(format!("{why}; unmount {workspace}: {error}"));
        }
        match self.step(claimed, None, CommandStep::MountReleased, Some(workspace)) {
            Ok(()) => Outcome::Failed(why),
            Err(error) => Outcome::Failed(format!("{why}; record the unmount: {error}")),
        }
    }

    /// Claude Code's inline settings for an engine on `model`, or why it
    /// cannot start. The default profile falls back on Claude Code's own
    /// settings (the gateway, its own small model) when the relay is down;
    /// the cheap profile has no fallback, and fails where its sender sees
    /// it.
    fn engine_settings(
        &self,
        claimed: &Claimed,
        model: ModelProfile,
        report: &mut TickReport,
    ) -> Result<String, String> {
        let relay = self.config.model_relay.as_ref();
        let reachable = relay.filter(|relay| relay.reachable());
        match (model, relay, reachable) {
            (_, _, Some(relay)) => Ok(claude_settings(Some(relay), model)),
            (ModelProfile::Default, Some(relay), None) => {
                report.warnings.push(format!(
                    "{}: model relay {} unreachable; the default profile runs on Claude Code's \
                     own settings",
                    claimed.session_id, relay.url
                ));
                Ok(claude_settings(None, model))
            }
            (ModelProfile::Default, None, None) => Ok(claude_settings(None, model)),
            (ModelProfile::Cheap, Some(relay), None) => Err(format!(
                "the cheap profile needs the model relay, and {} is unreachable",
                relay.url
            )),
            (ModelProfile::Cheap, None, None) => Err(
                "the cheap profile needs the model relay, and none is configured \
                 (SPAWN_MODEL_RELAY)"
                    .to_string(),
            ),
        }
    }

    /// End a start that failed as lost; the failure names `why` first.
    /// `discard` only when the log shows its prompt was never typed
    /// (nothing ran in it, so an unmount loses nothing). Otherwise the
    /// prompt may have reached the engine: a final checkpoint that fails
    /// keeps the workspace, and the end stays due, as a Stop's does.
    fn abandon(
        &mut self,
        claimed: &Claimed,
        active: &Active,
        why: String,
        discard: bool,
    ) -> Result<Outcome, StoreError> {
        let mut report = TickReport::default();
        let mut ending = active.clone();
        ending.follow.evidence = Some(format!("its start failed: {why}"));
        let ended = self.end(
            &claimed.agent_id,
            &claimed.session_id,
            &mut ending,
            (EndCause::Lost, None, None),
            discard,
            &mut report,
        )?;
        if !ended {
            self.catch_up()?;
            if let Some(held) = self
                .sessions
                .get_mut(&claimed.session_id)
                .and_then(|view| view.active.as_mut())
                .filter(|held| held.execution_id == ending.execution_id)
            {
                held.reported_end = ending.reported_end;
                held.follow.end_failures = ending.follow.end_failures;
                held.follow.end_tried_at = ending.follow.end_tried_at;
                held.follow.kill_failed_at = ending.follow.kill_failed_at;
                held.follow.killing.clone_from(&ending.follow.killing);
                held.follow.evidence = ending.follow.evidence;
            }
        }
        self.stray_notes.append(&mut report.notes);
        report.warnings.insert(0, why);
        Ok(Outcome::Failed(report.warnings.join("; ")))
    }

    /// Whether the log shows this command's typing never began, after
    /// folding what this tick appended: only then may its start be
    /// discarded.
    fn never_typed(&mut self, claimed: &Claimed, active: &Active) -> Result<bool, StoreError> {
        self.catch_up()?;
        Ok(matches!(
            self.typing_of(claimed, &active.execution_id),
            Typing::Due
        ))
    }

    /// The agent's branch, created at `from` (else at the base) unless it
    /// exists. Returns where a new branch began; an existing branch ignores
    /// `from`, with a warning.
    fn ensure_branch(
        &self,
        claimed: &Claimed,
        branch: &BranchName,
        from: Option<&str>,
        report: &mut TickReport,
    ) -> Result<Option<Fork>, String> {
        let fork = match self.workspaces.ensure_branch(branch, from) {
            Ok(Branch::Created(fork)) => Some(fork),
            Ok(Branch::Existing) => None,
            Err(error) => return Err(format!("ensure branch {branch}: {error}")),
        };
        if let Some(from) = from
            && fork.as_ref().is_none_or(|fork| fork.from != from)
        {
            report.warnings.push(format!(
                "{}: branch {branch} exists; from {from} ignored",
                claimed.session_id
            ));
        }
        Ok(fork)
    }

    /// A new Claude session gets its id from the Bridge, recorded before the
    /// engine starts, so the transcript it will write is known. Returns the
    /// id, or `None` when the engine names its own session or one is resumed.
    fn name_session(
        &mut self,
        claimed: &Claimed,
        native: Option<&str>,
        active: &Active,
    ) -> Result<Option<String>, StoreError> {
        if native.is_some() {
            return Ok(None);
        }
        let id = claude_session_id(&active.execution_id);
        self.append(
            &claimed.agent_id,
            &claimed.session_id,
            Some(&active.execution_id),
            Source::Bridge,
            Some(format!("{}:native", active.execution_id)),
            EventKind::NativeSessionStarted {
                engine: ENGINE.to_string(),
                native_session_id: id.clone(),
                transcript_path: Some(transcript_path(
                    &self.config.claude_projects,
                    &active.workspace,
                    &id,
                )),
                engine_version: None,
            },
        )?;
        self.catch_up()?;
        Ok(Some(id))
    }

    fn launch(&self, claimed: &Claimed, active: &Active, run: &Run<'_>) -> Launch {
        let Run {
            native,
            resume,
            settings,
        } = *run;
        let mut arguments = vec![
            "run".to_string(),
            "--".to_string(),
            self.config.claude_program.clone(),
        ];
        arguments.extend(claude_arguments(native, resume, settings));
        let mut environment = self.config.environment.clone();
        if let Some(launch) = &self.config.launch_environment {
            for (name, value) in launch.environment() {
                environment.retain(|(known, _)| *known != name);
                environment.push((name, value));
            }
        }
        for (session, overrides) in &self.config.session_environment {
            if session != claimed.session_id.as_str() {
                continue;
            }
            for (name, value) in overrides {
                environment.retain(|(known, _)| known != name);
                environment.push((name.clone(), value.clone()));
            }
        }
        environment.push(("SPAWN_AGENT_ID".to_string(), claimed.agent_id.to_string()));
        environment.push((
            "SPAWN_SESSION_ID".to_string(),
            claimed.session_id.to_string(),
        ));
        environment.push((
            "SPAWN_EXECUTION_ID".to_string(),
            active.execution_id.to_string(),
        ));
        // The runner writes the exit status where the Bridge reads it: the
        // pane needs no login of its own.
        environment.push(("SPAWN_EXIT_FILE".to_string(), exit_file(active)));
        Launch {
            terminal: active.terminal(),
            workspace: active.workspace.clone(),
            cwd: active.workspace.path().to_string(),
            program: self.config.spawn_program.clone(),
            arguments,
            environment,
        }
    }

    /// Wait for the engine's input box, then type `prompt` into it. The
    /// Bridge exported the workspace itself, for a fresh session and a
    /// resume alike, so it answers the engine's trust dialog in both: trust
    /// lives in the engine's own config (`~/.claude.json`), which can forget
    /// a workspace its session ran in (a host move, a reset or rewritten
    /// config, a workspace exported at a new path) while the transcript
    /// survives. Nothing in the log says whether the engine still trusts it.
    fn type_prompt(
        &mut self,
        claimed: &Claimed,
        active: &Active,
        prompt: &str,
    ) -> Result<Outcome, StoreError> {
        let mut last = ScreenState::Unknown;
        // Whether this wait answered the trust dialog already.
        let mut answered = false;
        // The trust dialog comes before the engine draws any prompt: once a
        // prompt was seen, nothing on the pane is that dialog any more.
        let mut prompt_seen = false;
        // A dialog frame is answered only when the next read shows it again.
        let mut dialog: Option<String> = None;
        // The last terminal call that failed on a terminal still there.
        let mut transient: Option<String> = None;
        // The trust dialog comes before the engine starts up, so answering it
        // restarts the budget instead of spending it. A failed capture, or a
        // staging refused before any key went, spends one attempt: only a
        // terminal that is gone ends the wait early.
        let mut attempt = 0;
        while attempt < self.config.ready_attempts {
            if attempt > 0 {
                sleep(self.config.ready_interval);
            }
            attempt += 1;
            let pane =
                match self
                    .terminals
                    .capture_tail(&active.terminal(), &active.workspace, TAIL_LINES)
                {
                    Ok(pane) => pane,
                    Err(error) => {
                        if self.terminal_gone(active) {
                            return Ok(Outcome::Failed(format!("terminal disappeared: {error}")));
                        }
                        transient = Some(format!("capture: {error}"));
                        continue;
                    }
                };
            let reading = read_claude_screen(&pane);
            prompt_seen |= reading.state != ScreenState::Unknown;
            // The Bridge exported this directory itself, so it trusts it,
            // whether the session is fresh or resumed. Answer once (a second
            // answer would land in the input box), and only the dialog
            // itself: on two equal reads, before any prompt.
            if !answered && !prompt_seen && is_trust_dialog(&pane) {
                if dialog.as_deref() != Some(pane.as_str()) {
                    dialog = Some(pane);
                    continue;
                }
                answered = true;
                if let Err(error) =
                    self.terminals
                        .send_keys(&active.terminal(), &active.workspace, &TRUST_KEYS)
                {
                    return Ok(Outcome::Failed(format!("answer trust dialog: {error}")));
                }
                attempt = 1;
                continue;
            }
            dialog = None;
            last = reading.state;
            if !reading.state.accepts_input() {
                continue;
            }
            self.append(
                &claimed.agent_id,
                &claimed.session_id,
                Some(&active.execution_id),
                Source::Bridge,
                Some(format!("{}:screen:ready", active.execution_id)),
                EventKind::ScreenObserved {
                    state: reading.state,
                    evidence: reading.evidence,
                },
            )?;
            match self.type_once(claimed, active, prompt)? {
                Ok(()) => return Ok(Outcome::Succeeded),
                Err(Untyped::Refused(error)) => {
                    transient = Some(format!("submit prompt refused before typing: {error}"));
                }
                Err(Untyped::Unknown(error)) => {
                    return Ok(Outcome::Failed(format!(
                        "submit prompt: {error}; the prompt {MAY_HAVE_TYPED}"
                    )));
                }
            }
        }
        let why = transient.map_or_else(String::new, |why| format!("; last failure {why}"));
        Ok(Outcome::Failed(format!(
            "pane never became ready, last state {last:?}{why}"
        )))
    }

    /// An input typed into the running engine, at most once ([`Self::type_once`]),
    /// on a ready pane, after every earlier input of the session that waits,
    /// and only once the input typed last was answered
    /// ([`SessionView::answered`]). When it cannot be typed yet, an input of
    /// Spawn's own loops is refused at once ([`PANE_NOT_READY`]: they retry
    /// on their own schedule) and any other waits for its turn
    /// ([`Self::not_yet`]).
    fn input(&mut self, claimed: &Claimed, text: &str) -> Result<Handled, StoreError> {
        let finished = |outcome| Ok(Handled::Finished(outcome));
        let waited = self
            .sessions
            .get(&claimed.session_id)
            .and_then(|view| view.inputs.waiting.get(&claimed.command_id).cloned());
        let Some(active) = self.running(&claimed.session_id) else {
            // One that waited outlived its execution: it is never typed.
            if let Some(waited_on) = waited {
                return finished(Outcome::Failed(
                    self.ended_untyped(&claimed.session_id, waited_on.as_ref()),
                ));
            }
            return finished(Outcome::Failed(format!(
                "{} runs no execution",
                claimed.session_id
            )));
        };
        // Nor in a later execution: a resume does not revive it.
        if let Some(Some(waited_on)) = &waited
            && *waited_on != active.execution_id
        {
            return finished(Outcome::Failed(
                self.ended_untyped(&claimed.session_id, Some(waited_on)),
            ));
        }
        // A retried input is never typed twice.
        match self.typing_of(claimed, &active.execution_id) {
            Typing::Due => {}
            Typing::Done => return finished(Outcome::Succeeded),
            Typing::Unknown => {
                return finished(Outcome::Failed(format!("the text {MAY_HAVE_TYPED}")));
            }
        }
        let shown = active
            .follow
            .screen
            .map_or(ScreenState::Unknown, |(state, _)| state);
        // Behind an earlier input that waits, or a typing not answered yet.
        let held = self.sessions.get(&claimed.session_id).and_then(|view| {
            if let Some(earlier) = view.inputs.waiting.range(..claimed.command_id).next() {
                return Some(format!("{}, sent before it, waits for its turn", earlier.0));
            }
            (!view.answered(&active.execution_id))
                .then(|| "the input typed last has not been answered yet".to_string())
        });
        if let Some(why) = held {
            return self.not_yet(claimed, &active, shown, Some(&why));
        }
        self.type_when_ready(claimed, &active, text, waited.is_some(), shown)
    }

    /// Read the pane and type `text` if it is ready, else leave the input to
    /// [`Self::not_yet`]. `waited`: the input was recorded waiting before.
    fn type_when_ready(
        &mut self,
        claimed: &Claimed,
        active: &Active,
        text: &str,
        waited: bool,
        shown: ScreenState,
    ) -> Result<Handled, StoreError> {
        let finished = |outcome| Ok(Handled::Finished(outcome));
        // A failed capture or a staging refused, on a terminal still there,
        // spends one attempt; out of attempts, the input is not ready, which
        // typed nothing. Only a terminal that is gone fails one that cannot
        // wait; one that waits is failed by the execution's end, with why.
        let mut attempt = 0;
        loop {
            if attempt > 0 {
                sleep(self.config.ready_interval);
            }
            attempt += 1;
            let last = attempt >= self.config.input_attempts;
            let pane =
                match self
                    .terminals
                    .capture_tail(&active.terminal(), &active.workspace, TAIL_LINES)
                {
                    Ok(pane) => pane,
                    Err(error) => {
                        if self.terminal_gone(active) {
                            if waits(&claimed.principal) {
                                return self.not_yet(claimed, active, shown, None);
                            }
                            return finished(Outcome::Failed(format!(
                                "terminal disappeared: {error}"
                            )));
                        }
                        if last {
                            return self.not_yet(
                                claimed,
                                active,
                                ScreenState::Unknown,
                                Some(&format!("pane unread ({error})")),
                            );
                        }
                        continue;
                    }
                };
            let reading = read_claude_screen(&pane);
            // A waiting input's busy readings are not recorded again each
            // time it is tried: they are no new fact.
            if reading.state.accepts_input() || !waited {
                self.append(
                    &claimed.agent_id,
                    &claimed.session_id,
                    Some(&active.execution_id),
                    Source::Bridge,
                    None,
                    EventKind::ScreenObserved {
                        state: reading.state,
                        evidence: reading.evidence,
                    },
                )?;
            }
            if !reading.state.accepts_input() {
                return self.not_yet(claimed, active, reading.state, None);
            }
            match self.type_once(claimed, active, text)? {
                Ok(()) => return finished(Outcome::Succeeded),
                Err(Untyped::Refused(error)) => {
                    if self.terminal_gone(active) {
                        if waits(&claimed.principal) {
                            return self.not_yet(claimed, active, shown, None);
                        }
                        return finished(Outcome::Failed(format!("terminal disappeared: {error}")));
                    }
                    if last {
                        return self.not_yet(
                            claimed,
                            active,
                            reading.state,
                            Some(&format!("submit text refused before typing ({error})")),
                        );
                    }
                }
                Err(Untyped::Unknown(error)) => {
                    return finished(Outcome::Failed(format!(
                        "submit text: {error}; the text {MAY_HAVE_TYPED}"
                    )));
                }
            }
        }
    }

    /// An input that cannot be typed yet (the pane shows `state`, or `why`):
    /// one of Spawn's own loops is refused at once, `pane state <state> does
    /// not accept input`, having typed nothing; any other is recorded as
    /// waiting (`command.step` `waiting`, once) and left unfinished.
    fn not_yet(
        &self,
        claimed: &Claimed,
        active: &Active,
        state: ScreenState,
        why: Option<&str>,
    ) -> Result<Handled, StoreError> {
        if !waits(&claimed.principal) {
            let why = why.map_or_else(String::new, |why| format!(": {why}"));
            return Ok(Handled::Finished(Outcome::Failed(format!(
                "pane state {state:?} {PANE_NOT_READY}{why}"
            ))));
        }
        self.step(
            claimed,
            Some(&active.execution_id),
            CommandStep::Waiting,
            None,
        )?;
        Ok(Handled::Waiting)
    }

    /// Why an input that waited on `execution` was never typed: the
    /// session was stopped, or its execution ended otherwise, first.
    fn ended_untyped(&self, session: &SessionId, execution: Option<&ExecutionId>) -> String {
        let cause = execution.and_then(|execution| {
            self.sessions
                .get(session)
                .and_then(|view| view.inputs.ended.get(execution).copied())
        });
        untyped_because(cause)
    }

    /// Fail every input of `session` this Bridge left waiting, now that its
    /// execution has ended (`cause`): none of them was typed. One claimed by
    /// someone else since is failed when it is claimed next. A command of
    /// another kind left waiting for the end's kill (a stop, a start or a
    /// resume: [`Settled::Pending`]) is not failed: claimed again, it meets
    /// the session as the end left it (a stop then succeeds).
    fn fail_waiting(
        &mut self,
        session: &SessionId,
        cause: Option<EndCause>,
    ) -> Result<(), StoreError> {
        let mine: Vec<CommandId> = self
            .held
            .iter()
            .filter(|(_, claimed)| &claimed.session_id == session)
            .filter(|(_, claimed)| matches!(claimed.action(), Ok(CommandAction::Input { .. })))
            .map(|(id, _)| *id)
            .collect();
        for id in mine {
            let Some(claimed) = self.held.remove(&id) else {
                continue;
            };
            match self
                .queue
                .finish(&claimed, &Outcome::Failed(untyped_because(cause)))
            {
                Ok(()) | Err(StoreError::LeaseLost { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// `Stop`, or with `discard` the `Discard` that unmounts even when the
    /// final checkpoint fails.
    fn stop(
        &mut self,
        claimed: &Claimed,
        discard: bool,
        tick: &mut TickReport,
    ) -> Result<Handled, StoreError> {
        // No execution open in the log: no end to wait for. One the log
        // holds open is ended here even when this Bridge's view had lost it.
        // So a stop that waited for its kill succeeds here once its end is
        // done.
        let Some(mut active) = self.open(claimed, tick)? else {
            return Ok(Handled::Finished(Outcome::Succeeded));
        };
        let mut report = TickReport::default();
        // A stop of an end already under way keeps its cause.
        let (cause, exit_code, signal) =
            active
                .reported_end
                .unwrap_or((EndCause::Stopped, None, None));
        // A stop that finds no runner of its own there (the terminal gone,
        // or hosting no runner) still writes the end the log waits for, and
        // says what it found.
        if active.reported_end.is_none() && !active.unmounting && active.follow.evidence.is_none() {
            let seen = match self.terminals.runner(&active.terminal(), &active.workspace) {
                Ok(Runner::Gone) => Some(format!("its terminal {} is gone", active.terminal())),
                Ok(Runner::Untracked(seen)) => Some(seen),
                Ok(Runner::Running) | Err(_) => None,
            };
            active.follow.evidence = seen.map(|seen| {
                format!(
                    "{seen}: no runner was running when {} came",
                    claimed.command_id
                )
            });
        }
        let ended = self.end(
            &claimed.agent_id,
            &claimed.session_id,
            &mut active,
            (cause, exit_code, signal),
            discard,
            &mut report,
        )?;
        self.catch_up()?;
        tick.notes.append(&mut report.notes);
        if ended {
            // What waited for a turn of this execution is never typed.
            self.fail_waiting(&claimed.session_id, Some(cause))?;
            // A discard that lost work says so in the tick's report too.
            tick.warnings.append(&mut report.warnings);
            return Ok(Handled::Finished(Outcome::Succeeded));
        }
        // The stop stays due: each tick tries the same end again.
        if let Some(held) = self
            .sessions
            .get_mut(&claimed.session_id)
            .and_then(|view| view.active.as_mut())
        {
            held.reported_end = active.reported_end;
            held.follow.end_failures = active.follow.end_failures;
            held.follow.end_tried_at = active.follow.end_tried_at;
            held.follow.kill_failed_at = active.follow.kill_failed_at;
            held.follow.killing.clone_from(&active.follow.killing);
            held.follow.evidence = active.follow.evidence;
        }
        if active.follow.killing.is_some() {
            // Its kill still waits for the engine's processes, off the loop:
            // the stop waits, claimed again after `input_wait_retry`, and
            // succeeds once the end that the ticks go on with is done.
            tick.warnings.append(&mut report.warnings);
            return Ok(Handled::Waiting);
        }
        if report.warnings.is_empty() {
            report.warnings.push(format!(
                "{}: the end is stalled and its final checkpoint retried every {} s; a \
                 discard command gives the uncommitted work up",
                claimed.session_id,
                END_RETRY_MS / 1000
            ));
        }
        Ok(Handled::Finished(Outcome::Failed(
            report.warnings.join("; "),
        )))
    }

    fn resume(
        &mut self,
        claimed: &Claimed,
        prompt: Option<&str>,
        model: Option<ModelProfile>,
        report: &mut TickReport,
    ) -> Result<Outcome, StoreError> {
        // A retried resume whose execution started goes on in `start`.
        let ours = ExecutionId::for_command(claimed.command_id);
        if self
            .running(&claimed.session_id)
            .is_some_and(|active| active.execution_id != ours)
        {
            return Ok(Outcome::Failed(format!(
                "{} still runs an execution",
                claimed.session_id
            )));
        }
        let Some(native) = self
            .sessions
            .get(&claimed.session_id)
            .and_then(|view| view.native.clone())
        else {
            return Ok(Outcome::Failed("no native session to resume".to_string()));
        };
        self.start(
            claimed,
            &Begin {
                prompt: prompt.unwrap_or(""),
                parent: None,
                native: Some(native.as_str()),
                from: None,
                model,
            },
            report,
        )
    }
}

/// How an input fails when the pane does not show a prompt that takes
/// text (`pane state <state> does not accept input`), or when the terminal,
/// still there, kept failing the capture or the staging of the text:
/// refused before anything was typed, so nothing reached the engine. The pane may be a
/// moment from ready: a turn's end reaches the transcript before the screen
/// settles at the prompt.
pub const PANE_NOT_READY: &str = "does not accept input";

/// Principals of Spawn's own loops. Their inputs are refused at once when
/// the pane cannot take them ([`PANE_NOT_READY`]) and retried on their own
/// schedule (the dispatcher's delivery retries, which its stall evidence
/// reads); every other principal's input (a person's, the supervisor's)
/// waits for its turn ([`CommandStep::Waiting`]).
pub const LOOP_PRINCIPALS: [&str; 3] = ["spawn-dispatch", "spawn-integrate", "spawn-liveness"];

/// Whether an input from `principal` waits for its turn instead of being
/// refused while the pane cannot take it.
pub fn waits(principal: &str) -> bool {
    !LOOP_PRINCIPALS.contains(&principal)
}

/// The wire name of an end's cause.
fn cause_name(cause: EndCause) -> &'static str {
    match cause {
        EndCause::Exited => "exited",
        EndCause::Stopped => "stopped",
        EndCause::Lost => "lost",
    }
}

/// The failure of an input that waited and was never typed, the session's
/// execution having ended first (`cause`, when the log says).
fn untyped_because(cause: Option<EndCause>) -> String {
    match cause {
        Some(EndCause::Stopped) => "the session was stopped before the input was typed".to_string(),
        Some(EndCause::Exited) => {
            "the session's execution ended (exited) before the input was typed".to_string()
        }
        Some(EndCause::Lost) => {
            "the session's execution ended (lost) before the input was typed".to_string()
        }
        None => "the session's execution ended before the input was typed".to_string(),
    }
}

/// Why text sent into a terminal did not arrive.
enum Untyped {
    /// Refused before any key went (the text could not be staged): nothing
    /// reached the engine and nothing says it might have.
    Refused(TerminalError),
    /// Failed once the keys began to go (`command.typing` is recorded):
    /// some or all of it may have arrived.
    Unknown(TerminalError),
}

/// How a command that may or may not have typed its text fails (the log
/// shows typing began and nothing shows it arrived): it is not typed again,
/// and whoever sent it must not send it again blindly either.
pub const MAY_HAVE_TYPED: &str =
    "may have been typed and the log cannot tell; it is not typed again";

/// A digest of a list of paths that stays the same across builds and hosts
/// (FNV-1a over the paths, each ended by a NUL): part of a record's key.
fn files_digest(files: &[String]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for file in files {
        for byte in file.bytes().chain(std::iter::once(0)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

/// Claude Code permission rules that keep an engine out of the host's
/// credentials and the tools that hold them:
///
/// - Files: the loop's environment (`~/.local/share/spawn-rsi/*.env*`,
///   `env.sh`) and its backups, the fleet's test cluster's files
///   (`fleet-pg`), `~/.config/spawn` (database, R2, backup key, relay and
///   API keys), `~/.config/cloudflare`, `~/.hermes`, all of `~/.claude`
///   (Claude Code's settings and login, and the transcripts, which hold
///   whatever a session ever printed) and `~/.claude.json`, `~/.ssh`,
///   `~/.gnupg`, the ccd sessions' directories and their test cluster
///   (`/data/tmp/ccd`, `/data/tmp/ccd-pg`).
/// - Programs that read them for whoever runs them: the owner's helpers
///   in `/var/tmp/spawn` (`*.sh`; the fleet's `TMPDIR` is that directory,
///   so not the directory itself), and in every checkout of Spawn
///   `deploy/hold.sh`, `deploy/deploy.sh`, `deploy/test-deploy.sh`,
///   `deploy/cutover-controller.sh` and `deploy/backup` (`lib.sh`, which
///   `pg-backup.sh` and `pg-restore-drill.sh` source).
/// - Killing by name or pattern (`pkill`, `killall`, `kill $(pgrep …)`,
///   `pgrep … | xargs kill`): it reaches other agents' and the gates'
///   processes. So does `kill -1`, which signals every process the user
///   may signal, where `-1` is a whole argument (the patterns never match
///   `-15` or `-19`). `kill <pid>` of the engine's own job stays allowed.
/// - Another process's environment (`/proc/<pid>/environ`, `ps e`): the
///   long-lived spawn processes are also not dumpable
///   (`spawn_host::undumpable`), which is what keeps their environment
///   from the same user.
///
/// Claude Code's own files under `~/.claude` are its internal writes, not
/// tool calls: a session, its transcript and a resume work with all of it
/// denied (checked, below).
///
/// Checked with Claude Code 2.1.319 in throwaway interactive sessions
/// started as the Bridge starts engines (bypass mode, these keys inline),
/// on a scratch canary file:
///
/// - Deny rules apply in bypass mode ("bypassPermissions auto-approves
///   every tool call (except explicit deny rules)").
/// - A `Read` rule also stops the Read, Grep and Glob tools and the Bash
///   commands Claude Code reads as reads of the path: cat, head, tail,
///   grep, sed, awk, cp, base64, od and a `<` redirect. It does not stop
///   `source`, `.`, `less`, `xargs cat`, `bash -c` or an interpreter
///   (python3 `open()`).
/// - A `Bash(*<path>*)` rule denies every command whose text names the
///   path, those included.
///
/// So each path has both. What remains open is a command that spells the
/// path without naming it (a variable, a glob, a `cd` then a relative path
/// to an interpreter), a program that reads a file the command does not
/// name, and the environment the engine itself gets. Rules, not a
/// boundary: the boundary would be Claude Code's sandbox, which on Linux
/// needs bubblewrap and socat. The Bash rules also deny ordinary commands
/// that name these files, e.g. `git diff deploy/deploy.sh`; the Read tool
/// still reads the checkout's scripts.
pub const CREDENTIAL_DENY: &[&str] = &[
    "Read(~/.local/share/spawn-rsi/*.env*)",
    "Read(~/.local/share/spawn-rsi/env.sh)",
    "Read(~/.local/share/spawn-rsi/backups/**)",
    "Read(~/.local/share/spawn-rsi/fleet-pg/**)",
    "Read(~/.config/spawn/**)",
    "Read(~/.config/cloudflare/**)",
    "Read(~/.hermes/**)",
    "Read(~/.claude/**)",
    "Read(~/.claude.json*)",
    "Read(~/.ssh/**)",
    "Read(~/.gnupg/**)",
    "Read(//data/tmp/ccd/**)",
    "Read(//data/tmp/ccd-pg/**)",
    "Read(//var/tmp/spawn/**/*.sh)",
    "Edit(~/.local/share/spawn-rsi/*.env*)",
    "Edit(~/.local/share/spawn-rsi/env.sh)",
    "Edit(~/.local/share/spawn-rsi/backups/**)",
    "Edit(~/.local/share/spawn-rsi/fleet-pg/**)",
    "Edit(~/.config/spawn/**)",
    "Edit(~/.config/cloudflare/**)",
    "Edit(~/.hermes/**)",
    "Edit(~/.claude/**)",
    "Edit(~/.claude.json*)",
    "Edit(~/.ssh/**)",
    "Edit(~/.gnupg/**)",
    "Edit(//data/tmp/ccd/**)",
    "Edit(//data/tmp/ccd-pg/**)",
    "Edit(//var/tmp/spawn/**/*.sh)",
    // The workspaces live under spawn-rsi too, so these name files, not
    // the directory.
    "Bash(*spawn.env*)",
    "Bash(*spawn-rsi/env.sh*)",
    "Bash(*spawn-rsi/backups*)",
    "Bash(*fleet-pg*)",
    "Bash(*.config/spawn*)",
    "Bash(*test-database.url*)",
    "Bash(*model-relay.env*)",
    "Bash(*typesafe.env*)",
    "Bash(*backup.key*)",
    "Bash(*r2.env*)",
    "Bash(*.config/cloudflare*)",
    "Bash(*.hermes*)",
    "Bash(*.claude/*)",
    "Bash(*.claude.json*)",
    "Bash(*/.ssh*)",
    "Bash(*.gnupg*)",
    "Bash(*/data/tmp/ccd*)",
    "Bash(*/var/tmp/spawn/*.sh*)",
    "Bash(*livepsql*)",
    "Bash(*pgenv*)",
    "Bash(*/hold.sh*)",
    "Bash(* hold.sh*)",
    "Bash(*/deploy.sh*)",
    "Bash(* deploy.sh*)",
    "Bash(*test-deploy.sh*)",
    "Bash(*cutover-controller*)",
    "Bash(*pg-backup*)",
    "Bash(*pg-restore-drill*)",
    "Bash(*backup/lib.sh*)",
    "Bash(*/environ*)",
    "Bash(*ps *eww*)",
    "Bash(*ps *axe*)",
    "Bash(*ps *auxe*)",
    // Killing by name or pattern reaches other agents' and gates'
    // processes (2026-11-08 05:19: a resident's `pkill -f 'cargo test'`
    // killed a ccd's run). `kill <pid>` stays: an engine stops its own
    // background jobs by their id.
    "Bash(pkill:*)",
    "Bash(killall:*)",
    "Bash(*pkill *)",
    "Bash(*killall *)",
    "Bash(*kill *$(pgrep*)",
    "Bash(*kill *`pgrep*)",
    "Bash(*kill *$(pidof*)",
    "Bash(*pgrep *|*kill*)",
    "Bash(*pidof *|*kill*)",
    // A signal to every process the user may signal: `-1` as a whole
    // argument, never as the first digits of `-15` or `-19`. Not
    // `Bash(kill -1:*)`: a prefix match, it also catches `kill -15`.
    "Bash(*kill -1)",
    "Bash(*kill -1 *)",
    "Bash(*kill -- -1)",
    "Bash(*kill -- -1 *)",
    "Bash(*kill * -1)",
    "Bash(*kill * -1 *)",
    // A pipeline that ends in `xargs kill` (ps | grep | awk | xargs kill):
    // the id it kills is the pipeline's to choose.
    "Bash(*xargs kill*)",
    "Bash(*xargs * kill*)",
];

/// Settings passed to Claude Code for an engine on `profile`, through
/// `relay` when there is one.
///
/// `--settings` takes inline JSON, which Claude Code merges over the user's
/// settings files: a key set here wins, and `env` merges name by name
/// (checked with Claude Code 2.1.318: an inline `env` naming only the base
/// URL and the haiku slot kept the user's main model and auth token). So
/// the gateway's token reaches the engine from the user's own settings, as
/// it would without Spawn, and Spawn never holds it.
///
/// The permission keys are repeated so the engine never depends on the
/// user's file for them: without them it stops on a startup dialog and
/// never reads the prompt the Bridge types into the pane. No hooks: the
/// Bridge reads what the engine does from its transcript and its screen.
///
/// With a relay, the engine calls it instead of the gateway, and the haiku
/// slot (Claude Code's small model for background work) is the relay's
/// small model; the cheap profile also puts its main model in the main,
/// sonnet and opus slots. Without a relay nothing about models is set: the
/// cheap profile needs one, and the Bridge refuses to start it without.
///
/// Every engine is denied the host's credential files and the programs
/// that read them ([`CREDENTIAL_DENY`]): a fleet worker read the loop's
/// environment file once (2026-11-08).
pub fn claude_settings(relay: Option<&ModelRelay>, profile: ModelProfile) -> String {
    let mut settings = json!({
        "permissions": {
            "defaultMode": "bypassPermissions",
            "deny": CREDENTIAL_DENY,
        },
        "skipDangerousModePermissionPrompt": true,
    });
    if let Some(relay) = relay {
        let mut env = serde_json::Map::new();
        env.insert("ANTHROPIC_BASE_URL".to_string(), json!(relay.url));
        if profile == ModelProfile::Cheap {
            for slot in [
                "ANTHROPIC_MODEL",
                "ANTHROPIC_DEFAULT_OPUS_MODEL",
                "ANTHROPIC_DEFAULT_SONNET_MODEL",
            ] {
                env.insert(slot.to_string(), json!(relay.cheap_model));
            }
        }
        env.insert(
            "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(),
            json!(relay.small_model),
        );
        settings["env"] = serde_json::Value::Object(env);
    }
    settings.to_string()
}

/// Claude arguments for the session `native`: a new one with that id, or
/// the earlier one resumed, with `settings` from [`claude_settings`]. The
/// prompt is typed into the pane, never passed here.
pub fn claude_arguments(native: Option<&str>, resume: bool, settings: &str) -> Vec<String> {
    // The engine runs in a container with no credentials of its own, so the
    // permission prompts have nowhere to go. The sandbox is the boundary.
    let mut arguments = vec![
        "--ax-screen-reader".to_string(),
        "--allow-dangerously-skip-permissions".to_string(),
        "--settings".to_string(),
        settings.to_string(),
    ];
    if let Some(native) = native {
        arguments.push(if resume { "--resume" } else { "--session-id" }.to_string());
        arguments.push(native.to_string());
    }
    arguments
}

/// The id of the Claude session a new execution starts: a UUID derived from
/// the execution, so the Bridge knows the transcript before the engine runs.
pub fn claude_session_id(execution: &ExecutionId) -> String {
    let hash = |seed: u64| {
        execution.as_str().bytes().fold(seed, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        })
    };
    let (high, low) = (hash(0xcbf2_9ce4_8422_2325), hash(0x8422_2325_cbf2_9ce4));
    // Version 4 and the RFC 4122 variant, so the engine accepts it.
    let high = (high & !0xf000) | 0x4000;
    let low = (low & !(0b11 << 62)) | (0b10 << 62);
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        high >> 32,
        (high >> 16) & 0xffff,
        high & 0xffff,
        low >> 48,
        low & 0xffff_ffff_ffff
    )
}

/// Where Claude Code writes the transcript of `native` for an engine started
/// in `workspace`: its projects directory, one directory per working
/// directory with every character but ASCII letters and digits replaced by
/// `-`, one file per session.
pub fn transcript_path(projects: &str, workspace: &WorkspaceId, native: &str) -> String {
    let directory: String = workspace
        .path()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!(
        "{}/{directory}/{native}.jsonl",
        projects.trim_end_matches('/')
    )
}

/// The watch key of an execution's files.
fn watch_key(execution: &ExecutionId) -> String {
    format!("execution:{execution}")
}

/// Where the runner writes the engine's exit status. `.spawn/` is never
/// committed.
fn exit_file(active: &Active) -> String {
    format!(
        "{}/.spawn/exit-{}",
        active.workspace.path(),
        active.execution_id
    )
}

/// Where the runner notes the signal that killed the engine, beside the
/// exit status ([`exit_file`]): the status of a program the OS killed is
/// empty, and a kill without its signal cannot be told from a clean exit.
fn signal_file(active: &Active) -> String {
    format!("{}.signal", exit_file(active))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{Bridge, BridgeConfig};
    use crate::command::{CommandAction, Submission};
    use crate::event::{ENGINE, EventKind, GlobalPosition};
    use crate::id::{
        AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId, TerminalName,
        WorkspaceId,
    };
    use crate::memory::{MemoryStore, MemoryWatch};
    use crate::port::{CommandQueue, EventLog, Launch, TerminalError, TerminalHost};
    use crate::screen::ScreenState;
    use crate::time::{ManualClock, Timestamp};
    use std::time::Duration;

    /// A pane at the input box, as the screen reader draws it.
    const READY: &str = "❯ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";

    /// A pane of a turn that runs.
    const BUSY: &str = "✽ Effecting… (esc to interrupt)\n❯ \n  model line";

    /// A pane the engine blocked on a question of its own: the choices sit
    /// at the prompt position.
    const DIALOG: &str = "claude: Which branch should I use?\n\
                          ❯ 1. main\n  2. a new one\n";

    /// The memory terminal, counting the panes it is asked for.
    struct Counted {
        inner: Arc<MemoryStore>,
        captures: Mutex<usize>,
    }

    impl Counted {
        fn captures(&self) -> usize {
            *self.captures.lock().expect("lock")
        }
    }

    impl TerminalHost for Counted {
        fn start(&self, launch: &Launch) -> Result<(), TerminalError> {
            self.inner.start(launch)
        }
        fn stage_text(
            &self,
            terminal: &TerminalName,
            workspace: &WorkspaceId,
            text: &str,
        ) -> Result<(), TerminalError> {
            self.inner.stage_text(terminal, workspace, text)
        }
        fn submit_staged(
            &self,
            terminal: &TerminalName,
            workspace: &WorkspaceId,
        ) -> Result<(), TerminalError> {
            self.inner.submit_staged(terminal, workspace)
        }
        fn send_keys(
            &self,
            terminal: &TerminalName,
            workspace: &WorkspaceId,
            keys: &[&str],
        ) -> Result<(), TerminalError> {
            self.inner.send_keys(terminal, workspace, keys)
        }
        fn is_alive(
            &self,
            terminal: &TerminalName,
            workspace: &WorkspaceId,
        ) -> Result<bool, TerminalError> {
            self.inner.is_alive(terminal, workspace)
        }
        fn runner(
            &self,
            terminal: &TerminalName,
            workspace: &WorkspaceId,
        ) -> Result<crate::port::Runner, TerminalError> {
            self.inner.runner(terminal, workspace)
        }
        fn capture_tail(
            &self,
            terminal: &TerminalName,
            workspace: &WorkspaceId,
            lines: usize,
        ) -> Result<String, TerminalError> {
            *self.captures.lock().expect("lock") += 1;
            self.inner.capture_tail(terminal, workspace, lines)
        }
        fn read_from(
            &self,
            workspace: &WorkspaceId,
            path: &str,
            offset: u64,
        ) -> Result<Option<Vec<u8>>, TerminalError> {
            self.inner.read_from(workspace, path, offset)
        }
        fn write_new(
            &self,
            workspace: &WorkspaceId,
            path: &str,
            bytes: &[u8],
        ) -> Result<bool, TerminalError> {
            self.inner.write_new(workspace, path, bytes)
        }
        fn terminate(
            &self,
            terminal: &TerminalName,
            workspace: &WorkspaceId,
        ) -> Result<(), TerminalError> {
            self.inner.terminate(terminal, workspace)
        }
    }

    /// A Bridge over one memory store, its clock, and a watch the test
    /// drives. One start command has run in it: a session whose engine waits
    /// at its prompt, its transcript known and watched.
    struct World {
        store: Arc<MemoryStore>,
        terminal: Arc<Counted>,
        watch: Arc<MemoryWatch>,
        clock: Arc<ManualClock>,
        bridge: Bridge,
        /// The session's transcript.
        path: String,
        /// Screens the log held once the start had settled: a test reads
        /// what was recorded after that.
        record: usize,
        /// What the start's own tick reported.
        first: Vec<String>,
    }

    impl World {
        fn new() -> Self {
            Self::with_watch(Arc::new(MemoryWatch::new()))
        }

        /// A Bridge whose watch refuses every path: its files are read on
        /// every pass, so it never settles.
        fn polled() -> Self {
            Self::build(Arc::new(MemoryWatch::refusing()), false)
        }

        fn with_watch(watch: Arc<MemoryWatch>) -> Self {
            Self::build(watch, true)
        }

        fn build(watch: Arc<MemoryWatch>, settle: bool) -> Self {
            let clock: Arc<ManualClock> = Arc::new(ManualClock::new(Timestamp(1_000)));
            let store = Arc::new(MemoryStore::new("shop", clock.clone()));
            let terminal = Arc::new(Counted {
                inner: store.clone(),
                captures: Mutex::new(0),
            });
            let mut config = BridgeConfig::new("tester");
            config.claude_projects = "/projects".to_string();
            config.screen_quiet = Duration::from_millis(500);
            config.screen_refresh = Duration::from_secs(30);
            config.watch = Some(watch.clone());
            let mut bridge = Bridge::new(
                store.clone(),
                store.clone(),
                store.clone(),
                terminal.clone(),
                config,
            )
            .with_clock(clock.clone());
            let execution = ExecutionId::for_command(CommandId(1));
            store.set_pane(&TerminalName::for_execution(&execution), READY);
            store
                .submit(
                    &Principal::new("tester").expect("principal"),
                    &Submission {
                        agent_id: AgentId::new("agent_one").expect("agent"),
                        session_id: SessionId::new("session_one").expect("session"),
                        idempotency_key: IdempotencyKey::new("start").expect("key"),
                        action: CommandAction::Start {
                            engine: ENGINE.to_string(),
                            prompt: String::new(),
                            parent_session_id: None,
                            from: None,
                            model: None,
                            charter: None,
                        },
                    },
                )
                .expect("submit");
            let first = bridge.tick().expect("the start's tick").warnings;
            let path = store
                .read_global(GlobalPosition(0), 100)
                .expect("read")
                .into_iter()
                .find_map(|record| match record.event.kind {
                    EventKind::NativeSessionStarted {
                        transcript_path, ..
                    } => transcript_path,
                    _ => None,
                })
                .expect("the start records the transcript's path");
            let mut world = Self {
                store,
                terminal,
                watch,
                clock,
                bridge,
                path,
                record: 0,
                first,
            };
            // Two readings in a row: the screen at the prompt is recorded.
            if settle {
                world.settle();
            }
            world.record = world.all_screens().len();
            world
        }

        /// One pass of the loop: the Bridge's reading of the log and the
        /// queue, then its reading of the engines' files and screens.
        fn tick(&mut self) -> Vec<String> {
            let (report, _) = self.bridge.work().expect("work");
            let mut warnings = report.warnings;
            warnings.extend(self.bridge.poll().expect("poll").warnings);
            warnings
        }

        fn pass(&mut self) -> usize {
            let before = self.terminal.captures();
            self.tick();
            self.terminal.captures() - before
        }

        /// Passes until nothing is captured any more.
        fn settle(&mut self) {
            for _ in 0..6 {
                if self.pass() == 0 {
                    return;
                }
            }
            panic!("the Bridge kept capturing an idle session");
        }

        fn advance(&self, millis: u64) {
            self.clock.advance(Duration::from_millis(millis));
        }

        /// Append whole lines of the engine's transcript, as the engine
        /// would, and let the watch say so.
        fn engine_writes(&mut self, lines: &str) {
            self.store.append_file(&self.path, lines);
            self.watch.changed(&self.path);
        }

        /// The engine's lines the log holds.
        fn engine_events(&self) -> Vec<&'static str> {
            self.store
                .read_global(GlobalPosition(0), 500)
                .expect("read")
                .into_iter()
                .filter(|record| record.event.source == crate::event::Source::Claude)
                .map(|record| record.event.kind.name())
                .collect()
        }

        /// The screen states the Bridge recorded since the start settled.
        fn screens(&self) -> Vec<crate::screen::ScreenState> {
            let mut screens = self.all_screens();
            screens.drain(..self.record.min(screens.len()));
            screens
        }

        fn all_screens(&self) -> Vec<crate::screen::ScreenState> {
            self.store
                .read_global(GlobalPosition(0), 500)
                .expect("read")
                .into_iter()
                .filter_map(|record| match record.event.kind {
                    EventKind::ScreenObserved { state, .. } => Some(state),
                    _ => None,
                })
                .collect()
        }

        fn set_pane(&self, pane: &str) {
            self.store.set_pane(
                &TerminalName::for_execution(&ExecutionId::for_command(CommandId(1))),
                pane,
            );
        }
    }

    /// The engine's line that ends a turn: a checkpoint, and the moment the
    /// screen is about to turn back to its prompt.
    const TURN_LINE: &str = "{\"type\":\"system\",\"subtype\":\"turn_duration\",\"uuid\":\"t1\"}\n";

    /// Something the engine wrote during the turn (no checkpoint of a turn's
    /// end, so nothing is committed).
    const TOOL_LINE: &str = "{\"type\":\"assistant\",\"uuid\":\"a1\",\"timestamp\":\"2099-01-01T00:00:00.000Z\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Bash\"}]}}\n";

    /// A session whose command is in flight (an input waiting for its turn)
    /// is captured on every pass: its screen is what the delivery waits for.
    #[test]
    fn a_session_with_a_command_in_flight_is_captured_every_pass() {
        let mut world = World::new();
        assert_eq!(world.pass(), 0, "an idle session costs no capture");
        world.set_pane(BUSY);
        world
            .store
            .submit(
                &Principal::new("person").expect("principal"),
                &Submission {
                    agent_id: AgentId::new("agent_one").expect("agent"),
                    session_id: SessionId::new("session_one").expect("session"),
                    idempotency_key: IdempotencyKey::new("input").expect("key"),
                    action: CommandAction::Input {
                        text: "go on".to_string(),
                        tags: Vec::new(),
                    },
                },
            )
            .expect("submit");
        // The input finds the pane busy and waits for its turn.
        world.tick();
        let held = world.pass();
        assert_eq!(held, 1, "the pass after the claim captures once");
        assert_eq!(world.pass(), 1, "a waiting command keeps the pane read");
        world.advance(500);
        assert_eq!(world.pass(), 1);
    }

    /// A turn ends in the transcript while the screen still shows the turn:
    /// the pane keeps being read until the engine paints its prompt, so the
    /// reading a delivery waits for is never left to the backstop.
    #[test]
    fn a_turn_that_ended_is_read_until_its_prompt_is_back() {
        let mut world = World::new();
        world.set_pane(BUSY);
        world.engine_writes(TURN_LINE);
        assert_eq!(world.pass(), 1, "the turn's end: a reading at once");
        assert_eq!(world.pass(), 1, "and its twin, which agrees");
        assert_eq!(world.screens(), vec![ScreenState::Busy]);
        // The transcript is quiet and the engine paints the prompt a moment
        // later: that reading is due on the next pass, not at the backstop.
        world.set_pane(READY);
        assert_eq!(world.pass(), 1, "the prompt: a reading");
        assert_eq!(world.pass(), 1, "and its twin");
        assert_eq!(
            world.screens(),
            vec![ScreenState::Busy, ScreenState::Ready],
            "the worker at its prompt is a fact"
        );
        assert_eq!(world.pass(), 0, "and then the session is left alone");
    }

    /// A screen that never shows its turn over costs the passes of one
    /// `screen_refresh`, no more: the backstop takes it back.
    #[test]
    fn a_screen_that_never_settles_is_read_for_one_refresh_at_most() {
        let mut world = World::new();
        world.set_pane(BUSY);
        world.engine_writes(TURN_LINE);
        world.pass();
        world.pass();
        let mut reads = 0;
        for _ in 0..70 {
            world.advance(500);
            reads += world.pass();
        }
        assert!(
            (58..=62).contains(&reads),
            "one capture a pass for the 30 s after the turn ended: {reads}"
        );
        assert_eq!(world.pass(), 0, "then not until the backstop");
    }

    /// A transcript that grew does not take the pane with it: the screen is
    /// read once the transcript has held still, then again until two
    /// readings agree, and then not again until the backstop.
    #[test]
    fn a_grown_transcript_waits_for_quiet_then_two_readings() {
        let mut world = World::new();
        world.set_pane(BUSY);
        world.engine_writes(TOOL_LINE);
        world.advance(100);
        assert_eq!(world.pass(), 0, "the transcript only just grew");
        assert!(
            world.engine_events().contains(&"tool.started"),
            "the transcript is read as soon as it changed: {:?}",
            world.engine_events()
        );
        world.advance(500);
        assert_eq!(world.pass(), 1, "quiet for the bound: one reading");
        assert!(
            world.screens().is_empty(),
            "one reading is not a fact: {:?}",
            world.screens()
        );
        world.advance(500);
        assert_eq!(world.pass(), 1, "the second reading, which agrees");
        assert_eq!(world.screens(), vec![ScreenState::Busy]);
        world.advance(500);
        assert_eq!(world.pass(), 0, "and then nothing more");
    }

    /// The backstop: a screen that changes with nothing in the transcript (a
    /// dialog, a crash to a shell) is read once per `screen_refresh`.
    #[test]
    fn the_backstop_reads_a_screen_the_transcript_does_not_move() {
        let mut world = World::new();
        world.set_pane(DIALOG);
        assert_eq!(world.pass(), 0, "not yet due");
        world.advance(29_000);
        assert_eq!(world.pass(), 0, "still short of the backstop");
        world.advance(1_000);
        assert_eq!(world.pass(), 1, "the backstop reads it");
        assert_eq!(world.pass(), 1, "the reading waits for its twin");
        assert_eq!(world.screens(), vec![ScreenState::Permission]);
        assert_eq!(world.pass(), 0, "and then nothing until the next");
    }

    /// A busy screen is recorded again every `screen_refresh` while the turn
    /// keeps it busy, so the Observer keeps a current fact for a long turn.
    #[test]
    fn a_busy_screen_is_recorded_again_every_refresh() {
        let mut world = World::new();
        world.set_pane(BUSY);
        world.advance(30_000);
        world.pass();
        world.pass();
        assert_eq!(world.screens(), vec![ScreenState::Busy]);
        world.advance(30_000);
        world.pass();
        world.pass();
        assert_eq!(
            world.screens(),
            vec![ScreenState::Busy, ScreenState::Busy],
            "a long turn keeps a current fact"
        );
    }

    /// A change is read once, and a line written while a read is in progress
    /// is read at the next pass: never lost to the moment it was written.
    #[test]
    fn a_transcript_change_is_read_once_and_a_half_written_line_next_pass() {
        let mut world = World::new();
        world.engine_writes(TOOL_LINE);
        world.tick();
        assert_eq!(world.engine_events(), vec!["tool.started"]);
        world.tick();
        assert_eq!(
            world.engine_events(),
            vec!["tool.started"],
            "nothing read twice"
        );
        // Half a line: the read that took it left it half-written, so the
        // next pass reads again even though no change is reported for the
        // rest (the write and the read raced).
        world
            .store
            .append_file(&world.path, "{\"type\":\"system\",\"subtype\":\"turn_dur");
        world.watch.changed(&world.path);
        world.tick();
        assert_eq!(world.engine_events(), vec!["tool.started"]);
        world
            .store
            .append_file(&world.path, "ation\",\"uuid\":\"t2\"}\n");
        world.tick();
        assert_eq!(
            world.engine_events(),
            vec!["tool.started", "turn.stopped"],
            "the rest of the line is read at the next pass"
        );
    }

    /// A terminal killed from outside ends its execution on the next pass,
    /// as before: the fleet's liveness is one call per pass, not a capture
    /// and not a call per agent.
    #[test]
    fn a_killed_terminal_ends_its_execution_on_the_next_pass() {
        let mut world = World::new();
        assert_eq!(world.pass(), 0, "idle: nothing to do");
        world.store.untrack(
            &TerminalName::for_execution(&ExecutionId::for_command(CommandId(1))),
            "a shell prompt after the host move",
        );
        world.tick();
        let ended = world
            .store
            .read_global(GlobalPosition(0), 500)
            .expect("read")
            .into_iter()
            .find_map(|record| match record.event.kind {
                EventKind::ExecutionEnded { cause, .. } => Some(cause),
                _ => None,
            });
        assert_eq!(ended, Some(crate::event::EndCause::Lost), "ended as lost");
    }

    /// A watch that can be set on one of an execution's files and not on
    /// another falls back for the whole execution: both are read on every
    /// pass, and the file that could not be watched is named once.
    #[test]
    fn a_watch_refused_for_one_file_falls_back_for_the_whole_execution() {
        let watch = Arc::new(MemoryWatch::new());
        // The first mount's exit file: the start command's execution.
        let exit = "/workspaces/1/.spawn/exit-exec_1";
        watch.refuse(exit);
        let mut world = World::build(watch.clone(), false);
        assert!(
            world
                .first
                .iter()
                .any(|warning| warning.contains(exit) && warning.contains("read on every pass")),
            "the file that cannot be watched is named: {:?}",
            world.first
        );
        world.set_pane(BUSY);
        assert_eq!(world.pass(), 1, "its files are read on every pass");
        assert_eq!(world.pass(), 1);
    }

    /// A session whose files cannot be watched is read on every pass, as
    /// before, and the fallback is said once.
    #[test]
    fn a_watch_that_cannot_be_set_falls_back_to_reading_every_pass_once() {
        let mut world = World::polled();
        assert!(
            world
                .first
                .iter()
                .any(|warning| warning.contains("read on every pass")),
            "the fallback is said: {:?}",
            world.first
        );
        world.set_pane(BUSY);
        assert_eq!(world.pass(), 1, "read on every pass");
        assert_eq!(world.pass(), 1);
        let warnings = world.tick();
        assert!(
            !warnings
                .iter()
                .any(|warning| warning.contains("read on every pass")),
            "said once: {warnings:?}"
        );
    }
}
