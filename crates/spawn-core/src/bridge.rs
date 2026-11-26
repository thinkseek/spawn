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
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::Duration;

use serde_json::json;

use crate::command::{Claimed, CommandAction, NamedKey, Outcome, Submission};
use crate::driver::{
    AgentDriver, EngineHome, EngineLaunch, LineCursor, SessionBinding, driver_or_default,
    plain_session_id,
};
use crate::event::{
    ARCHIVE_REASON_MAX, ArtifactRef, CheckpointReason, CommandStep, EndCause, EventKind,
    EventRecord, Fork, GlobalPosition, LaunchSummary, ModelProfile, NewEvent, Source,
};
use crate::id::{
    AgentId, BranchName, CommandId, CommitId, ExecutionId, IdempotencyKey, Principal, SessionId,
    TerminalName, WorkspaceId,
};
use crate::port::WORKSPACE_STALE;
use crate::port::{
    AgentReading, AgentStatus, BaseMerge, Branch, CommandQueue, Commit, EngineView, EventLog,
    FileWatch, Launch, MOUNT_CANCELLED, READ_LIMIT, Runner, ServerSeen, StoreError, TerminalError,
    TerminalHost, TranscriptArchive, UNMOUNT_IN_USE, ViewedLaunch, WatchError, WorkspaceError,
    WorkspaceStore,
};
use crate::screen::{ScreenState, TAIL_LINES};
use crate::time::{Clock, SystemClock};
use crate::transcripts::{self, ArchiveProgress};

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
    /// The model an engine told its model at launch runs, by its own label
    /// (`start {model_label}`); unset, the host's (`SPAWN_AGY_MODEL`).
    model_label: Option<&'a str>,
    /// The engine's driver: the start's engine, or the session's for a
    /// resume.
    driver: &'static dyn AgentDriver,
}

/// The loopback relay engines reach their models through
/// (`scripts/model-relay.py`), and the models each profile names. The relay
/// routes `deepseek-*` models to DeepSeek with its own key and everything
/// else to Claude Code's gateway with the engine's own credentials, so
/// Spawn holds neither.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelRelay {
    /// Base URL of the relay, `http://127.0.0.1:<port>`.
    pub url: String,
    /// The cheap profile's main model, also in its sonnet, opus and
    /// subagent slots.
    pub cheap_model: String,
    /// The small, fast model (the haiku slot) of every profile.
    pub small_model: String,
    /// The default profile's main model (`SPAWN_DEFAULT_MODEL`), and the
    /// sonnet, opus and subagent slots' unless they name their own. Unset
    /// keeps Claude Code's own main model (issue #79).
    pub default_model: Option<String>,
    /// The default profile's sonnet slot (`SPAWN_SONNET_MODEL`).
    pub sonnet_model: Option<String>,
    /// The default profile's opus slot (`SPAWN_OPUS_MODEL`).
    pub opus_model: Option<String>,
    /// The default profile's subagent model (`SPAWN_SUBAGENT_MODEL`).
    pub subagent_model: Option<String>,
}

impl ModelRelay {
    /// The main model `profile` pins through this relay; `None` keeps
    /// Claude Code's own.
    pub fn main_model(&self, profile: ModelProfile) -> Option<&str> {
        match profile {
            ModelProfile::Cheap => Some(&self.cheap_model),
            ModelProfile::Default => self.default_model.as_deref(),
        }
    }

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
    /// Where engines other than Claude Code live, by driver id
    /// ([`BridgeConfig::engine_home`]). Claude Code's home is always
    /// `claude_program` and `claude_projects`, whatever this holds. An
    /// engine with no home here is not enabled on this host
    /// (`SPAWN_ENGINES`): a start naming it is refused.
    pub engines: Vec<(String, EngineHome)>,
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
    /// Whether the first [`Bridge::recover`] takes ownership of `worker` and
    /// makes its previous process's unfinished leases claimable at once
    /// ([`CommandQueue::recover_worker`]). The resident `spawn loop` does; a
    /// one-shot `spawn tick` does not, so it neither holds the worker nor
    /// expires a lease a live holder (a loop, or a claim made without the
    /// ownership protocol) still has: those wait out their TTL, as before.
    pub recover_worker_leases: bool,
    /// Whether the Bridge answers an engine's startup trust dialog itself
    /// (audited, as `key` commands of [`RESOLVE_PRINCIPAL`]). `true` by
    /// default; `false` under `SPAWN_AUTO_APPROVE=off`, where nothing is
    /// answered automatically: a start that shows the dialog waits, until
    /// its readiness limit, for someone to answer it in the engine's
    /// terminal itself (a key sent through Spawn queues behind the start);
    /// if nobody does, the start fails naming the dialog and the switch
    /// ([`TRUST_OFF`]).
    pub auto_trust: bool,
    /// Whether an engine with no deny rules of its own
    /// ([`crate::driver::Guard::Sandbox`]) runs inside a filesystem view
    /// (decision C2, N6; `SPAWN_ENGINE_VIEW`). Required by default, and
    /// with no view: such an engine is refused (`engine_unavailable`)
    /// until the host hands one over. Claude Code never runs in one.
    pub engine_view: EngineViewPolicy,
}

/// The prefix of the reason a start of an engine this host cannot run
/// safely is refused with: the delivery's escalation names the same.
pub const ENGINE_UNAVAILABLE: &str = "engine_unavailable";

/// Whether a [`crate::driver::Guard::Sandbox`] engine runs inside a
/// filesystem view ([`BridgeConfig::engine_view`]).
#[derive(Debug, Clone)]
pub enum EngineViewPolicy {
    /// Only inside this view (`SPAWN_ENGINE_VIEW=required`, the default);
    /// the error says why this host has none, and such an engine does not
    /// start.
    Required(Result<Arc<dyn EngineView>, String>),
    /// Without one (`SPAWN_ENGINE_VIEW=off`, the owner's explicit choice):
    /// the engine sees whatever its uid sees, as before N6.
    Off,
}

impl EngineViewPolicy {
    /// Required, with no view yet: the default of a [`BridgeConfig`].
    pub fn unconfigured() -> Self {
        Self::Required(Err("no filesystem view is configured".to_string()))
    }
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
            engines: Vec::new(),
            watch: None,
            launch_environment: None,
            recover_worker_leases: true,
            auto_trust: true,
            engine_view: EngineViewPolicy::unconfigured(),
        }
    }

    /// Where the engine of driver `id` lives: Claude Code's from
    /// `claude_program` and `claude_projects`, another's from `engines`;
    /// `None` for an engine with no home here.
    pub fn engine_home(&self, id: &str) -> Option<EngineHome> {
        if id == crate::driver::DEFAULT {
            return Some(EngineHome {
                program: self.claude_program.clone(),
                sessions: self.claude_projects.clone(),
                model: None,
            });
        }
        self.engines
            .iter()
            .find(|(known, _)| known == id)
            .map(|(_, home)| home.clone())
    }

    /// The home of `driver`, else Claude Code's.
    fn home_of(&self, driver: &dyn AgentDriver) -> EngineHome {
        self.engine_home(driver.id()).unwrap_or_else(|| EngineHome {
            program: self.claude_program.clone(),
            sessions: self.claude_projects.clone(),
            model: None,
        })
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
    /// The driver of the engine it runs, from its `execution.started`.
    driver: &'static dyn AgentDriver,
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
    /// When its `execution.started` was recorded (the record's time, epoch
    /// ms), the same after a restart: a prompt of an engine that names its
    /// own session ([`Bridge::bind_reported`]) older than this (less
    /// [`BIND_SKEW_MS`]) was not typed into this execution.
    started_at: u64,
    /// The conversations below the engine's home
    /// ([`AgentDriver::session_scan_dir`]) that were there before a fresh
    /// start launched it: none of them is this execution's, so the scan
    /// never binds one. Empty after a restart (the start's listing is gone)
    /// and for a resume.
    preexisting_natives: HashSet<String>,
}

impl Active {
    /// An execution as its `execution.started` record has it, recorded at
    /// `started_at`.
    fn started(
        execution_id: ExecutionId,
        workspace: WorkspaceId,
        transcript_from: Option<u64>,
        driver: &'static dyn AgentDriver,
        started_at: crate::time::Timestamp,
    ) -> Self {
        Self {
            execution_id,
            workspace,
            driver,
            started_at: u64::try_from(started_at.0).unwrap_or(0),
            preexisting_natives: HashSet::new(),
            checkpoint: None,
            reported_end: None,
            unmounting: false,
            // The execution's lines are the ones after where the transcript
            // ended when it started, after a restart too. A resume runs a
            // session already named: its engine reports no new one.
            follow: Follow {
                offset: transcript_from.unwrap_or(0),
                binding: Binding::resumed(transcript_from.is_some()),
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

/// Whether an execution's native session is known: named by the Bridge, or
/// resumed, or (an engine that names its own, [`SessionBinding::Reported`])
/// reported by the host and checked ([`Bridge::bind_reported`]). Until it
/// is, such an engine's transcript is not read: the session's last one is
/// another execution's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum Binding {
    /// Not known yet.
    #[default]
    Unbound,
    /// Known; the native session when this Bridge bound it itself, before
    /// the log's record of it is folded.
    Bound(Option<String>),
}

impl Binding {
    /// Known from the start for a resume (its session is the one resumed).
    fn resumed(resume: bool) -> Self {
        if resume {
            Self::Bound(None)
        } else {
            Self::Unbound
        }
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
    /// after which a transcript that never appeared is reported, or the next
    /// pass after a read that came back full (more waits to be read).
    due_at: Option<i64>,
    /// Bytes of the transcript read so far.
    offset: u64,
    /// What the engine's driver keeps between transcript lines.
    cursor: LineCursor,
    /// The last bytes read before `offset`, at most [`FOLLOW_OVERLAP`]:
    /// what the file must still hold there for the offset to mean anything.
    /// A file that is shorter than the offset, or holds other bytes before
    /// it, shrank or was replaced (a rewrite, a restore, a file renamed over
    /// it), and is read again from the start ([`Bridge::follow_output`]).
    /// Empty after a restart: then only the length is checked.
    tail: Vec<u8>,
    /// The last screen state recorded, and when.
    screen: Option<(ScreenState, i64)>,
    /// A state read once and not recorded yet: a screen is recorded when
    /// two readings in a row agree, so a frame mid-redraw is not a fact.
    seen: Option<ScreenState>,
    /// When this Bridge first followed the execution, by its own clock.
    first_seen: Option<i64>,
    /// Whether the execution's native session is known ([`Binding`]).
    binding: Binding,
    /// When [`Bridge::bind_reported`] last scanned the engine's
    /// conversations for this execution's, by the Bridge's clock: at most
    /// once per [`SCAN_INTERVAL_MS`], since each scan may read up to
    /// [`crate::port::LIST_DIRS_LIMIT`] transcripts.
    scanned_at: Option<i64>,
    /// The host's agent reading (its `seq`) a recorded screen was last
    /// keyed by: a reading recorded once is never keyed again.
    keyed_seq: Option<u64>,
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
    /// Why the end's last attempt failed (its kill, or its final
    /// checkpoint), until one gets through. A stop that finds its end not
    /// done says this, whichever pass of the Bridge read the failure: the
    /// ticks go on with the end too, and may read a kill's result before
    /// the stop is claimed again.
    end_error: Option<String>,
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
    /// The key ([`failure_key`]) of the commit failure this execution's
    /// checkpoints are in, while one is (objective 205): a failing commit
    /// is retried every pass, but logged only when it starts failing or
    /// fails for another cause, and once more ("commit available again")
    /// when a commit gets through after it. In memory only: a restarted
    /// loop logs a failure that is still there once more.
    commit_failed: Option<String>,
    /// The engine's version, once read from this execution's transcript
    /// and recorded.
    version: Option<String>,
    /// What the Bridge saw when it found no runner of its own running the
    /// execution (the terminal gone, or there and hosting no runner): the
    /// end record's `evidence`.
    evidence: Option<String>,
    /// Where the transcript archive stands with its transcript.
    archiving: Archiving,
}

/// How many bytes before its read position the Bridge keeps of a
/// transcript to tell a file that went on from one that shrank or was
/// replaced, as the archive's copier does with its copies
/// (transcripts.rs `OVERLAP`).
const FOLLOW_OVERLAP: usize = 4096;

/// The least time between two scans of an engine's conversations for an
/// execution's own ([`Bridge::bind_reported`]), in milliseconds.
const SCAN_INTERVAL_MS: i64 = 2_000;

/// How much older than its execution's `execution.started` a prompt of an
/// engine that names its own session may say it is and still bind it
/// ([`Bridge::bind_reported`]): the engine's clock and the store's may
/// differ by this much, in milliseconds.
const BIND_SKEW_MS: u64 = 5_000;

/// An RFC 3339 date-time (`2026-11-25T14:25:59Z`, with fractional seconds
/// and a `Z` or `±HH:MM` offset) as epoch milliseconds; `None` for anything
/// else. Fractions beyond milliseconds are truncated.
fn rfc3339_ms(text: &str) -> Option<i64> {
    fn number(bytes: &[u8]) -> Option<i64> {
        if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
            return None;
        }
        Some(
            bytes
                .iter()
                .fold(0, |sum, digit| sum * 10 + i64::from(digit - b'0')),
        )
    }
    let bytes = text.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year = number(&bytes[0..4])?;
    let month = number(&bytes[5..7])?;
    let day = number(&bytes[8..10])?;
    let hour = number(&bytes[11..13])?;
    let minute = number(&bytes[14..16])?;
    let second = number(&bytes[17..19])?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    // A leap second (60) reads as the next second's start.
    if day < 1 || day > days_in_month || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut rest = &bytes[19..];
    let mut millis = 0;
    if let Some(fraction) = rest.strip_prefix(b".") {
        let digits = fraction.iter().take_while(|byte| byte.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        let mut padded = [b'0'; 3];
        for (slot, digit) in padded.iter_mut().zip(&fraction[..digits]) {
            *slot = *digit;
        }
        millis = number(&padded)?;
        rest = &fraction[digits..];
    }
    let offset = match rest {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let hours = number(&[*h1, *h2])?;
            let minutes = number(&[*m1, *m2])?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let offset = (hours * 60 + minutes) * 60;
            if *sign == b'-' { -offset } else { offset }
        }
        _ => return None,
    };
    // Days since 1970-01-01 of the civil date (Hinnant's days_from_civil).
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted.div_euclid(400);
    let year_of_era = shifted - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset;
    Some(seconds * 1_000 + millis)
}

impl Follow {
    /// The log records the execution's native session: it is known, as
    /// this Bridge bound it or not.
    fn recorded_binding(&mut self) {
        if self.binding == Binding::Unbound {
            self.binding = Binding::Bound(None);
        }
    }

    /// The driver reads the lines of `native`: its keys begin with the
    /// session whose lines they are, so another session starts afresh.
    fn reading(&mut self, native: &str) {
        if self.cursor.native != native {
            self.cursor = LineCursor {
                native: native.to_string(),
                ..LineCursor::default()
            };
        }
    }

    /// How many bytes before the offset a read checks: the tail kept, or,
    /// with none (after a restart), one, which says the file still reaches
    /// the offset.
    fn kept(&self) -> usize {
        if self.tail.is_empty() {
            usize::from(self.offset > 0)
        } else {
            self.tail.len()
        }
    }

    /// `bytes`, read from the transcript at `offset`, are dealt with: the
    /// offset moves past them, and the tail keeps their last bytes.
    fn advance(&mut self, bytes: &[u8]) {
        self.offset += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if bytes.len() >= FOLLOW_OVERLAP {
            self.tail = bytes[bytes.len() - FOLLOW_OVERLAP..].to_vec();
        } else {
            self.tail.extend_from_slice(bytes);
            let over = self.tail.len().saturating_sub(FOLLOW_OVERLAP);
            self.tail.drain(..over);
        }
    }
}

/// How far the archive has an execution's transcript ([`Bridge::keep_archive`]),
/// as written to its progress record ([`ArchiveProgress`]).
#[derive(Debug, Clone)]
struct Archiving {
    /// The transcript may hold bytes the archive lacks (it was read
    /// growing, or a copy failed): a copy is made on this pass.
    due: bool,
    /// How far into the transcript the archive reaches after the last copy
    /// that succeeded ([`transcripts::Archived::offset`]).
    archived: Option<u64>,
    /// The time of the first line the archive lacks, after a copy failed.
    behind_ms: Option<i64>,
    /// Until when the transcript is known ([`ArchiveProgress::known_ms`]).
    known_ms: Option<i64>,
    /// The record last written.
    written: Option<ArchiveProgress>,
    /// The last copy's failure, warned when it changes.
    error: Option<String>,
}

impl Default for Archiving {
    fn default() -> Self {
        Self {
            // Nothing is known of the file yet: the first pass copies.
            due: true,
            archived: None,
            behind_ms: None,
            known_ms: None,
            written: None,
            error: None,
        }
    }
}

/// What one copy of a transcript into the archive did (objective 179).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pass {
    /// It copied: how far the transcript's archived offset moved.
    Copied(u64),
    /// It copied nothing, failing so (the loop's warning's words).
    Failed(String),
    /// The transcript is gone (Claude Code's cleanup deleted it) while the
    /// archive lacks lines of it: nothing can be copied, nor ever resumed.
    Gone,
}

impl Pass {
    /// The failure the loop warns of, if the pass did not copy.
    fn error(&self) -> Option<String> {
        match self {
            Self::Copied(_) => None,
            Self::Failed(error) => Some(error.clone()),
            Self::Gone => Some(ARCHIVE_GONE.to_string()),
        }
    }
}

/// The `archive.failed` reason of a transcript removed before the archive
/// had every line of it (objective 179 R4).
pub const ARCHIVE_GONE: &str = "the transcript was removed before the archive had every line of it";

/// What the log was told of one transcript's archive state
/// ([`Bridge::record_archive_pass`]).
#[derive(Debug, Clone, Default)]
struct ArchiveEvents {
    /// The reason of the `archive.failed` the log holds last, `None` when it
    /// holds none or an `archive.resumed` after it.
    failed: Option<String>,
    /// The last pass failed with [`transcripts::MASK_CHANGED`]: a second in
    /// a row is a failure (a rotation between the mask and the read is
    /// copied on the next pass).
    stale: bool,
    /// A change tried and not yet taken by the log: tried again each pass
    /// under its key until it is, or until a newer change replaces it.
    pending: Option<PendingArchiveEvent>,
}

/// An archive event waiting for the log: its key, the state it tells
/// (`failed` as in [`ArchiveEvents`]) and the event.
#[derive(Debug, Clone)]
struct PendingArchiveEvent {
    key: String,
    failed: Option<String>,
    kind: EventKind,
}

/// `reason` cut to [`ARCHIVE_REASON_MAX`] characters, on a character
/// boundary.
fn archive_reason(reason: &str) -> String {
    reason.chars().take(ARCHIVE_REASON_MAX).collect()
}

/// What a pass wants the log told of a transcript's archive state.
enum ArchiveWanted {
    /// Nothing new: a first stale mask, or a transcript gone whose failure is
    /// already told or waiting to be.
    Nothing,
    /// The state `failed` as in [`ArchiveEvents`].
    State(Option<String>),
}

/// What `pass` wants the log told, given what it was told (`entry`).
fn wanted_archive_state(entry: &mut ArchiveEvents, pass: &Pass) -> ArchiveWanted {
    match pass {
        Pass::Copied(_) => {
            entry.stale = false;
            ArchiveWanted::State(None)
        }
        Pass::Failed(reason) if reason == transcripts::MASK_CHANGED && !entry.stale => {
            entry.stale = true;
            ArchiveWanted::Nothing
        }
        Pass::Failed(reason) => {
            entry.stale = reason == transcripts::MASK_CHANGED;
            ArchiveWanted::State(Some(archive_reason(reason)))
        }
        Pass::Gone => {
            entry.stale = false;
            // A gone transcript never reads resumed.
            if entry.pending.as_ref().is_some_and(|p| p.failed.is_none()) {
                entry.pending = None;
            }
            let told = entry.failed.is_some()
                || entry.pending.as_ref().is_some_and(|p| p.failed.is_some());
            if told {
                ArchiveWanted::Nothing
            } else {
                ArchiveWanted::State(Some(ARCHIVE_GONE.to_string()))
            }
        }
    }
}

/// An ended execution whose transcript the archive may still lack
/// ([`Bridge::archive_ended`]): its last record, and the copy's last failure.
#[derive(Debug, Clone)]
struct EndedArchive {
    progress: ArchiveProgress,
    error: Option<String>,
    /// The record as last written, if it was.
    written: Option<ArchiveProgress>,
}

/// How much earlier than a read of a transcript Spawn takes it to be known:
/// a line is stamped before it is written, and the watch tells of a write a
/// moment after it.
pub const ARCHIVE_SLACK_MS: i64 = 2_000;

/// How often a live execution's progress record is written again when only
/// the time it is known until moved.
pub const PROGRESS_REFRESH_MS: i64 = 2_000;

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
    /// Commands accepted but not finished. An unfinished start keeps the
    /// mount its log records so a reclaimed command can use it.
    unfinished: HashSet<CommandId>,
    /// The native session the latest execution ran, which a resume
    /// continues.
    native: Option<String>,
    /// Every native session an execution of this session was bound to: a
    /// fresh start never binds one of them again ([`Bridge::bind_reported`]).
    natives: HashSet<String>,
    /// The log holds events read from that session's transcript: it
    /// existed, and a resume must not start without it.
    transcript_read: bool,
    /// The profile the session's last execution started with: what a start
    /// or resume that names none runs on.
    model: ModelProfile,
    /// The engine the session's last execution started: what a resume
    /// runs.
    engine: String,
    /// The model label an engine told its model at launch (agy) last
    /// started on, as its `execution.started` records it (the launch's
    /// `model`): what a resume runs, so a resumed worker stays on its
    /// preset's model. None for Claude Code, and for an execution that
    /// recorded none (the host's `SPAWN_AGY_MODEL` then).
    model_label: Option<String>,
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
    /// Where each unfinished command's `command.accepted` is in the log.
    accepted_at: HashMap<CommandId, GlobalPosition>,
    /// Where the session's latest `pressed` or `typed` step is: what a
    /// resolver's key checks nothing reached the pane after its own
    /// acceptance ([`Bridge::press`]).
    last_effect: Option<GlobalPosition>,
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
    /// How many `CommandStep::Mounted` events were recorded for this command.
    mounts: u32,
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

fn mounted_key(command: CommandId, generation: u32) -> String {
    if generation <= 1 {
        format!("{command}:mounted")
    } else {
        format!("{command}:mounted:{generation}")
    }
}

fn mount_released_key(command: CommandId, generation: u32) -> String {
    if generation <= 1 {
        format!("{command}:mount_released")
    } else {
        format!("{command}:mount_released:{generation}")
    }
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

/// The model label an execution of `engine` was launched on, as its
/// `execution.started` records it (`launch`'s `model`): an engine besides
/// Claude Code's only ([`SessionView::model_label`]).
fn launched_label(engine: &str, launch: Option<&LaunchSummary>) -> Option<String> {
    launch
        .and_then(|launch| launch.model.clone())
        .filter(|_| engine != crate::driver::DEFAULT)
}

impl SessionView {
    /// Start a session's view from its first event.
    fn from_event(event: &NewEvent) -> Self {
        Self {
            agent_id: event.agent_id.clone(),
            unfinished: HashSet::new(),
            native: None,
            natives: HashSet::new(),
            transcript_read: false,
            model: ModelProfile::Default,
            engine: crate::driver::DEFAULT.to_string(),
            model_label: None,
            active: None,
            recorded_head: None,
            latest: None,
            steps: HashMap::new(),
            last_message: None,
            inputs: Inputs::default(),
            accepted_at: HashMap::new(),
            last_effect: None,
        }
    }

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
                    CommandStep::Mounted => {
                        steps.mounted.clone_from(workspace);
                        steps.released = false;
                        steps.mounts = steps.mounts.saturating_add(1);
                    }
                    CommandStep::MountReleased => {
                        if workspace.is_none() || steps.mounted.as_ref() == workspace.as_ref() {
                            steps.released = true;
                        }
                    }
                    CommandStep::Typing => steps.typing_at = Some(record.position),
                    CommandStep::Typed => {
                        steps.typed = true;
                        self.last_effect = Some(record.position);
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
                    CommandStep::Pressed => {
                        steps.press = Press::Pressed;
                        self.last_effect = Some(record.position);
                    }
                }
            }
            EventKind::CommandFinished { command_id, .. } => {
                self.unfinished.remove(command_id);
                self.inputs.waiting.remove(command_id);
                self.accepted_at.remove(command_id);
            }
            EventKind::CommandAccepted { command_id } => {
                self.unfinished.insert(*command_id);
                self.accepted_at.insert(*command_id, record.position);
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
    /// Command ids claimed in this pass, in claim order.
    pub command_ids: Vec<CommandId>,
    /// Non-fatal problems, for example a failed commit.
    pub warnings: Vec<String>,
    /// What the Bridge measured and says for the log, not a problem: how
    /// long each end's kill waited for the engine's processes to exit.
    pub notes: Vec<String>,
}

/// What a failing commit is logged by (objective 205): its kind
/// (`unavailable`, or `refused <code>`) and its message's first line, with
/// each run of seven or more hex digits read as `<id>`. A commit that keeps
/// failing for one cause keeps its key: `update-ref` names the pass's fresh
/// commit id, and `git add`'s later lines name the files it failed at. A
/// checkpoint logs a failure only when the key changes.
pub fn failure_key(error: &WorkspaceError) -> String {
    let (kind, message) = match error {
        WorkspaceError::Refused { code, message } => (format!("refused {code}"), message),
        WorkspaceError::Unavailable(text) => ("unavailable".to_string(), text),
    };
    let first = message.lines().next().unwrap_or_default();
    let mut masked = String::with_capacity(first.len());
    let mut run = String::new();
    let flush = |run: &mut String, masked: &mut String| {
        if run.len() >= 7 {
            masked.push_str("<id>");
        } else {
            masked.push_str(run);
        }
        run.clear();
    };
    for character in first.chars() {
        if character.is_ascii_hexdigit() {
            run.push(character);
        } else {
            flush(&mut run, &mut masked);
            masked.push(character);
        }
    }
    flush(&mut run, &mut masked);
    format!("{kind}: {masked}")
}

/// The start settings chosen before its mount began. They are kept while
/// the command waits so its one `execution.started` has one launch summary.
#[derive(Clone)]
struct PreparedStart {
    /// The engine's driver.
    driver: &'static dyn AgentDriver,
    /// The model relay the engine goes through: the one that was reachable
    /// when the start was prepared, else none.
    relay: Option<ModelRelay>,
    /// The engine's configuration directory ([`Bridge::claude_config_dir`]).
    config_dir: Option<String>,
    launch: LaunchSummary,
    model: ModelProfile,
    /// The model label the engine is launched with: the start's
    /// `model_label`, else the host's for that engine; none for Claude Code.
    model_label: Option<String>,
    fork: Option<Fork>,
}

/// One engine prompt's progress, retained between non-blocking attempts.
struct PromptProgress {
    attempts: usize,
    answered_trust: bool,
    prompt_seen: bool,
    dialog: Option<String>,
    last_state: ScreenState,
    last_pane: Option<String>,
    transient: Option<String>,
}

impl Default for PromptProgress {
    fn default() -> Self {
        Self {
            attempts: 0,
            answered_trust: false,
            prompt_seen: false,
            dialog: None,
            last_state: ScreenState::Unknown,
            last_pane: None,
            transient: None,
        }
    }
}

/// A workspace mount running off the Bridge thread. The worker only mounts;
/// its result is joined and recorded by the Bridge thread.
struct PendingMount {
    agent_id: AgentId,
    session_id: SessionId,
    branch: BranchName,
    cancel: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<Result<WorkspaceId, WorkspaceError>>,
    /// Another Bridge finished the command or took over the mount. Join and
    /// release this worker's unrecorded result as soon as it finishes, freeing
    /// its bounded slot.
    cancelled: bool,
}

impl PendingMount {
    fn cancel(&mut self) {
        self.cancelled = true;
        self.cancel.store(true, Ordering::SeqCst);
    }
}

/// An unrecorded workspace from a cancelled or dropped mount worker whose
/// unmount failed. Retained for retry on later passes and persisted via
/// `CommandStep::Mounted` when the command has no mounted record yet so a
/// restart's [`Bridge::release_orphan_mounts`] can recover it.
struct AbandonedMount {
    agent_id: AgentId,
    session_id: SessionId,
    command_id: CommandId,
    workspace: WorkspaceId,
    /// True once `self.workspaces.unmount(&workspace)` succeeded, so a later
    /// retry only needs to append `{command_id}:mount_released`.
    unmounted: bool,
    /// Mount generation (`1` for the first mount, `2+` for remounts) when this
    /// Bridge inserted `mounted` for this workspace, so a later retry records
    /// the matching `mount_released`.
    recorded_generation: Option<u32>,
}

/// How often an unfinished mount is polled.
const MOUNT_POLL_RETRY: Duration = Duration::from_millis(25);

/// The number of workspace copies that may run at once.
const MAX_CONCURRENT_MOUNTS: usize = 8;

/// Bounded parallelism when committing turn checkpoints across sessions.
const MAX_CONCURRENT_CHECKPOINTS: usize = 8;

/// One transcript tail read returned by [`Bridge::read_followed`].
type FollowedRead = Result<Option<(Vec<u8>, usize)>, TerminalError>;

/// Pre-committed turn checkpoints keyed by session id.
type PrecommittedMap = HashMap<SessionId, Result<Commit, WorkspaceError>>;

/// Active session snapshot gathered at the start of [`Bridge::act`].
struct RunningSession {
    session: SessionId,
    agent_id: AgentId,
    native: Option<String>,
    active: Active,
}

/// Whose a pre-committed checkpoint is, to record one no `advance` took
/// ([`Bridge::keep_unconsumed_precommits`]).
struct PrecommitOwner {
    agent_id: AgentId,
    execution: ExecutionId,
    reason: CheckpointReason,
}

/// Prefetched transcript reads and pre-committed checkpoints for one pass.
struct PrefetchedPass {
    registered: HashSet<SessionId>,
    reads: HashMap<SessionId, FollowedRead>,
    precommitted: PrecommittedMap,
}

/// Watch-registration state and prefetched artifacts passed into [`Bridge::advance`].
struct AdvancePrefetch<'a> {
    register_watch: bool,
    read: Option<FollowedRead>,
    precommitted: Option<&'a mut PrecommittedMap>,
}

/// Prefetched transcript read and pre-committed checkpoints passed into [`Bridge::follow_output`].
struct FollowPrefetch<'a> {
    read: Option<FollowedRead>,
    precommitted: Option<&'a mut PrecommittedMap>,
}

/// Session identity and transcript path passed into [`Bridge::ingest_followed_bytes`].
struct TranscriptTarget<'a> {
    agent_id: &'a AgentId,
    session: &'a SessionId,
    native: &'a str,
    path: &'a str,
}

enum MountProgress {
    Ready(WorkspaceId),
    Pending,
    Failed(String),
}

/// Agent Bridge.
pub struct Bridge {
    log: Arc<dyn EventLog>,
    queue: Arc<dyn CommandQueue>,
    /// Crash-released ownership of this worker, acquired by the first recover.
    ownership: Option<Box<dyn crate::port::CommandOwnership>>,
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
    /// The credential mask every copy goes through (objective 175): without
    /// it a pass copies nothing. `None`: none is configured, copies are the
    /// transcript's bytes, as before 175.
    archive_mask: Option<Arc<dyn crate::port::ArchiveMask>>,
    /// Ended executions whose transcripts the archive may lack, per agent
    /// and native session: copied again on every pass until the copy is
    /// made. Rebuilt after a restart from the progress records
    /// ([`Bridge::recover`]): never only in memory.
    ended_archives: std::sync::Mutex<BTreeMap<(AgentId, String), EndedArchive>>,
    /// Per agent and native session, what the log was told of the
    /// transcript's archive state and what waits to be told (objective 179,
    /// [`Bridge::record_archive_pass`]); seeded from the log as it is folded.
    archive_events: std::sync::Mutex<HashMap<(AgentId, String), ArchiveEvents>>,
    /// The archive events' keys made by this Bridge, for their `<n>`.
    archive_keys: std::sync::atomic::AtomicU64,
    /// When this pass began, by the Bridge's clock: a transcript the watch
    /// says nothing of is known unchanged until then.
    pass_started: i64,
    /// The latest claim of each command this Bridge left waiting, so that an
    /// execution's end fails its waiting inputs in the same tick. Only a
    /// shortcut: a restarted Bridge, which has none, fails them when it
    /// claims them next (the log says which wait and how the execution ended).
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
    /// Start settings retained while their workspace mounts.
    prepared: HashMap<CommandId, PreparedStart>,
    /// Prompt readiness retained per command between claims.
    prompting: HashMap<CommandId, PromptProgress>,
    /// The conversations a fresh start found below its engine's home before
    /// the launch, until its `execution.started` is folded into the view
    /// ([`Active::preexisting_natives`]).
    preexisting: HashMap<ExecutionId, HashSet<String>>,
    /// Workspace mounts running off the Bridge thread, bounded by
    /// [`MAX_CONCURRENT_MOUNTS`].
    mounting: HashMap<CommandId, PendingMount>,
    /// Unrecorded mounts whose cleanup unmount failed, kept for retry on later
    /// passes after recording them for orphan recovery when possible.
    abandoned_mounts: Vec<AbandonedMount>,
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
        // Retry earlier abandoned cleanups (including pending `mount_released`
        // appends) before joining current workers, after checking the log so a
        // peer Bridge's live or recorded workspace is never unmounted.
        if !self.abandoned_mounts.is_empty() {
            let caught_up = self.catch_up().is_ok();
            self.retry_abandoned_mounts(caught_up);
        }
        // No mount worker outlives the store and runtime it borrowed. Signal
        // cancellation on every pending worker up front, then join each one on
        // the Bridge thread and release any unrecorded mount unless the log
        // shows a live execution or peer owns it. Do not retry newly failed
        // mounts from `self.mounting` here so an unfinished command's freshly
        // persisted workspace stays mounted on disk for reuse.
        let mounting = std::mem::take(&mut self.mounting);
        for pending in mounting.values() {
            pending.cancel.store(true, Ordering::SeqCst);
        }
        for (command_id, pending) in mounting {
            if let Ok(Ok(workspace)) = pending.handle.join() {
                let caught_up = self.catch_up().is_ok();
                self.release_abandoned_mount(
                    pending.agent_id,
                    pending.session_id,
                    command_id,
                    workspace,
                    caught_up,
                );
            }
        }
        for mut abandoned in std::mem::take(&mut self.abandoned_mounts) {
            if abandoned.unmounted {
                eprintln!(
                    "bridge: dropped after unmounting workspace {} for {} without recording mount_released",
                    abandoned.workspace, abandoned.command_id
                );
                continue;
            }
            if abandoned.recorded_generation.is_none() {
                abandoned.recorded_generation = self.persist_abandoned_mount(
                    &abandoned.agent_id,
                    &abandoned.session_id,
                    abandoned.command_id,
                    &abandoned.workspace,
                );
            }
            eprintln!(
                "bridge: dropped with unmounted workspace {} for {} ({})",
                abandoned.workspace,
                abandoned.command_id,
                if abandoned.recorded_generation.is_some() {
                    "recorded for orphan recovery"
                } else {
                    "not recorded for orphan recovery"
                }
            );
        }
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
    /// It stays unfinished and is claimed again after this command's own
    /// retry delay. Start and Resume waits deliberately record no
    /// `CommandStep::Waiting`: they still block their session's next command.
    Waiting { retry: Duration },
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
            ownership: None,
            workspaces,
            terminals,
            config,
            cursor: GlobalPosition(0),
            sessions: HashMap::new(),
            clock: Arc::new(SystemClock),
            unrecorded_commits: std::sync::Mutex::default(),
            warned: std::sync::Mutex::default(),
            transcripts: None,
            archive_mask: None,
            ended_archives: std::sync::Mutex::default(),
            archive_events: std::sync::Mutex::default(),
            archive_keys: std::sync::atomic::AtomicU64::new(0),
            pass_started: i64::MIN,
            held: HashMap::new(),
            touched: HashSet::new(),
            backstops: std::sync::atomic::AtomicUsize::new(0),
            runners: None,
            prepared: HashMap::new(),
            prompting: HashMap::new(),
            preexisting: HashMap::new(),
            mounting: HashMap::new(),
            abandoned_mounts: Vec::new(),
            kill_threads: std::sync::Mutex::new(Vec::new()),
            stray_notes: Vec::new(),
        }
    }

    /// Copy every engine's transcript into `archive` on the passes it grew,
    /// at turn ends and execution ends, and restore a missing one from it
    /// before a resume.
    #[must_use]
    pub fn with_transcript_archive(mut self, archive: Arc<dyn TranscriptArchive>) -> Self {
        self.transcripts = Some(archive);
        self
    }

    /// Copy every transcript through the credential mask `mask` gives
    /// (objective 175): each copy takes the mask first, and without it the
    /// pass copies nothing and fails as a copy failure does (warned when it
    /// changes, tried again every pass, the progress record's `behind_ms`
    /// saying from when the archive lacks lines). A resume's restore is
    /// given it too.
    #[must_use]
    pub fn with_archive_mask(mut self, mask: Arc<dyn crate::port::ArchiveMask>) -> Self {
        self.archive_mask = Some(mask);
        self
    }

    /// [`Self::with_archive_mask`] on a Bridge already built (tests).
    #[cfg(test)]
    fn set_archive_mask(&mut self, mask: Arc<dyn crate::port::ArchiveMask>) {
        self.archive_mask = Some(mask);
    }

    /// Use `clock` for screen refreshes instead of the system clock.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Wait until an engine's watched files may have changed. `Ok(None)`
    /// means this Bridge has no watch that can wait; `Ok(Some(false))` means
    /// the timeout elapsed. Polling and reading the files stay in [`Self::poll`].
    pub fn wait_for_change(&self, timeout: Duration) -> Result<Option<bool>, WatchError> {
        match &self.config.watch {
            Some(watch) => watch.wait(timeout),
            None => Ok(None),
        }
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

    /// Whether a start or resume waits on an off-thread mount or on its
    /// engine's prompt readiness: a one-shot caller (`spawn tick`) ticks
    /// again until it finishes, as it does for [`Self::ends_pending`].
    pub fn starts_pending(&self) -> bool {
        self.mounting.values().any(|pending| !pending.cancelled)
            || !self.prompting.is_empty()
            || self.held.values().any(|claimed| {
                matches!(
                    claimed.action(),
                    Ok(CommandAction::Start { .. } | CommandAction::Resume { .. })
                )
            })
    }

    /// Rebuild state from the whole log. The first recovery exclusively owns
    /// this worker and makes the previous owner's unfinished leases claimable;
    /// a still-live previous owner makes this fail instead of duplicating work.
    /// With [`BridgeConfig::recover_worker_leases`] off it takes no ownership
    /// and expires no lease.
    pub fn recover(&mut self) -> Result<(), StoreError> {
        if self.config.recover_worker_leases && self.ownership.is_none() {
            let (ownership, recovered) = self.queue.recover_worker(&self.config.worker)?;
            self.ownership = Some(ownership);
            if recovered > 0 {
                eprintln!(
                    "spawn: {} recovered {recovered} unfinished command lease(s) left by its previous process",
                    self.config.worker
                );
            }
        }
        self.cursor = GlobalPosition(0);
        self.sessions.clear();
        self.preexisting.clear();
        self.catch_up()?;
        self.recover_archives();
        self.rederive_unrecorded();
        self.release_orphan_mounts()?;
        Ok(())
    }

    /// A start that mounted its workspace and died before its execution was
    /// recorded (or before the mount's release was) left a mount no
    /// execution owns. After a restart, an unfinished command keeps its mount
    /// for the reclaim; any other recorded mount with no execution started and
    /// no release goes, unless an execution runs in that workspace, and its
    /// release is recorded. An unmount that fails is left for the next restart.
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
                    && !view.unfinished.contains(command)
                    && !used.contains(&self.workspaces.canonical(workspace))
                {
                    orphans.push((
                        view.agent_id.clone(),
                        session.clone(),
                        *command,
                        workspace.clone(),
                        steps.mounts.max(1),
                    ));
                }
            }
        }
        for (agent_id, session, command, workspace, mounts) in orphans {
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
                Some(mount_released_key(command, mounts)),
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
        self.reap_cancelled_mounts();
        let mut report = TickReport::default();
        // Waiting commands keep their lease until the claims of this tick
        // are done, so commands on other sessions (and behind waiting inputs)
        // are claimed now. Each is then released for its own retry delay.
        let mut waited: Vec<(Claimed, i64, Duration)> = Vec::new();
        while report.commands < self.config.max_commands_per_tick {
            let Some(claimed) = self
                .queue
                .claim(&self.config.worker, self.config.lease_ttl)?
            else {
                break;
            };
            let command_id = claimed.command_id;
            match self.handle(&claimed, &mut report)? {
                Handled::Finished(outcome) => {
                    self.held.remove(&claimed.command_id);
                    self.prepared.remove(&claimed.command_id);
                    self.prompting.remove(&claimed.command_id);
                    if let Some(pending) = self.mounting.get_mut(&claimed.command_id) {
                        pending.cancel();
                    }
                    self.reap_cancelled_mounts();
                    self.queue.finish(&claimed, &outcome)?;
                }
                Handled::Waiting { retry } => {
                    self.held.insert(claimed.command_id, claimed.clone());
                    waited.push((claimed, self.clock.now().0, retry));
                }
            }
            report.commands += 1;
            report.command_ids.push(command_id);
            self.catch_up()?;
        }
        let released_at = self.clock.now().0;
        for (claimed, handled_at, retry) in &waited {
            // One its execution's end failed meanwhile is not released.
            if self.held.contains_key(&claimed.command_id) {
                let elapsed = u64::try_from(released_at.saturating_sub(*handled_at)).unwrap_or(0);
                let remaining = retry.saturating_sub(Duration::from_millis(elapsed));
                self.queue.release(claimed, remaining)?;
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
        self.reap_cancelled_mounts();
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
        report.notes.append(&mut self.stray_notes);
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
                self.fold_archive_event(&record.event);
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

    fn fold_finished_command(&mut self, event: &NewEvent) {
        let EventKind::CommandFinished { command_id, .. } = &event.kind else {
            return;
        };
        self.held.remove(command_id);
        self.prepared.remove(command_id);
        self.prompting.remove(command_id);
        if let Some(pending) = self.mounting.get_mut(command_id) {
            pending.cancel();
        }
    }

    fn fold(&mut self, record: &EventRecord) {
        let event = &record.event;
        self.fold_finished_command(event);
        // The control stream is not an agent's session.
        if event.kind.is_control() {
            return;
        }
        let view = self
            .sessions
            .entry(event.session_id.clone())
            .or_insert_with(|| SessionView::from_event(event));
        view.fold_branch(record);
        view.fold_steps(record);
        view.transcript_read |= transcripts::read_from_transcript(event);
        let current = view
            .active
            .as_ref()
            .is_some_and(|active| event.execution_id.as_ref() == Some(&active.execution_id));
        match &event.kind {
            EventKind::ExecutionStarted {
                engine,
                workspace,
                model,
                transcript_from,
                launch,
                ..
            } => {
                view.model = *model;
                view.engine.clone_from(engine);
                view.model_label = launched_label(engine, launch.as_ref());
                if let Some(execution_id) = event.execution_id.clone() {
                    // As the store names it now: a record from before the
                    // root was resolved names the same directory.
                    let workspace = self.workspaces.canonical(workspace);
                    let mut active = Active::started(
                        execution_id,
                        workspace,
                        *transcript_from,
                        driver_or_default(engine),
                        record.recorded_at,
                    );
                    // A fresh start of this Bridge's own listed what was
                    // there before its launch; a restart has no such list.
                    if let Some(names) = self.preexisting.remove(&active.execution_id) {
                        active.preexisting_natives = names;
                    }
                    view.active = Some(active);
                }
            }
            // After a restart, the last recorded screen. The Bridge's own
            // readings keep its own clock, which the refresh is timed by.
            EventKind::ScreenObserved { state, .. } if current => {
                if let Some(active) = view.active.as_mut() {
                    fold_screen(active, *state, record);
                }
            }
            EventKind::ExecutionEnded { .. } if current && event.source == Source::Bridge => {
                if let Some(active) = view.active.take() {
                    let archives = self.transcripts.is_some().then_some(&self.ended_archives);
                    let watch = self.config.watch.as_deref();
                    ended(watch, archives, view, &event.session_id, &active);
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
                view.natives.insert(native_session_id.clone());
                if current && let Some(active) = view.active.as_mut() {
                    active.follow.recorded_binding();
                }
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

    fn prefetch_and_precommit(
        &self,
        running: &mut [RunningSession],
        report: &mut TickReport,
    ) -> PrefetchedPass {
        let mut registered: HashSet<SessionId> = HashSet::new();
        let mut reads: HashMap<SessionId, FollowedRead> = HashMap::new();
        let mut commit_jobs: Vec<(SessionId, WorkspaceId, String)> = Vec::new();
        for item in running {
            if self.has_unrecorded_commits(&item.session) {
                continue;
            }
            let can_prefetch = item.active.driver.traits().session != SessionBinding::Reported
                || !matches!(item.active.follow.binding, Binding::Unbound);
            let mut has_ends_turn = false;
            if can_prefetch {
                let bound_native = match &item.active.follow.binding {
                    Binding::Bound(Some(bound)) => Some(bound.clone()),
                    _ => item.native.clone(),
                };
                self.register_watch(
                    &item.session,
                    bound_native.as_deref(),
                    &mut item.active,
                    report,
                );
                registered.insert(item.session.clone());
                let watched = self.config.watch.is_some() && !item.active.follow.watch.poll;
                let should_read = !(watched
                    && !item.active.follow.watch.dirty
                    && !self.due_to_read(&item.active));
                if should_read && let Some(native) = bound_native.as_deref() {
                    item.active.follow.reading(native);
                    let path =
                        self.transcript_of(item.active.driver, &item.active.workspace, native);
                    let read_res =
                        self.read_followed(&item.session, &path, &mut item.active, report);
                    has_ends_turn = matches!(
                        &read_res,
                        Ok(Some((bytes, _)))
                            if Self::bytes_end_turn(
                                item.active.driver,
                                &item.active.follow.cursor,
                                bytes,
                            )
                    );
                    reads.insert(item.session.clone(), read_res);
                }
            }
            if !item.active.unmounting
                && (has_ends_turn
                    || (item.active.checkpoint.is_some() && item.active.reported_end.is_none()))
            {
                commit_jobs.push((
                    item.session.clone(),
                    item.active.workspace.clone(),
                    format!("spawn checkpoint {}", item.active.execution_id),
                ));
            }
        }
        let precommitted = if commit_jobs.len() >= 2 {
            let workers = commit_jobs.len().min(MAX_CONCURRENT_CHECKPOINTS);
            let workspaces = &self.workspaces;
            let next = std::sync::atomic::AtomicUsize::new(0);
            let results = std::sync::Mutex::new(HashMap::with_capacity(commit_jobs.len()));
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    scope.spawn(|| {
                        loop {
                            let idx = next.fetch_add(1, Ordering::Relaxed);
                            let Some((session, workspace, message)) = commit_jobs.get(idx) else {
                                break;
                            };
                            let outcome = workspaces.commit(workspace, message);
                            results
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .insert(session.clone(), outcome);
                        }
                    });
                }
            });
            results
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        } else {
            HashMap::new()
        };
        PrefetchedPass {
            registered,
            reads,
            precommitted,
        }
    }

    fn act(&mut self, report: &mut TickReport) -> Result<(), StoreError> {
        self.pass_started = self.clock.now().0;
        // Sessions with no running execution have no `advance` to do it.
        self.flush_unrecorded(report);
        let mut running: Vec<RunningSession> = self
            .sessions
            .iter()
            .filter_map(|(session, view)| {
                let active = view.active.clone()?;
                Some(RunningSession {
                    session: session.clone(),
                    agent_id: view.agent_id.clone(),
                    native: view.native.clone(),
                    active,
                })
            })
            .collect();
        let mut prefetched = self.prefetch_and_precommit(&mut running, report);
        let owners: HashMap<SessionId, PrecommitOwner> = running
            .iter()
            .filter(|item| prefetched.precommitted.contains_key(&item.session))
            .map(|item| {
                let owner = PrecommitOwner {
                    agent_id: item.agent_id.clone(),
                    execution: item.active.execution_id.clone(),
                    reason: item
                        .active
                        .checkpoint
                        .unwrap_or(CheckpointReason::TurnStopped),
                };
                (item.session.clone(), owner)
            })
            .collect();
        let advanced = self.advance_running(running, &mut prefetched, report);
        // Whether the pass went through or stopped early, no precommit's
        // record is dropped with the map.
        self.keep_unconsumed_precommits(std::mem::take(&mut prefetched.precommitted), owners);
        let ended_now = advanced?;
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
        self.archive_ended(report);
        Ok(())
    }

    /// [`Self::advance`] every running session of this pass, in turn, with
    /// what [`Self::prefetch_and_precommit`] read and committed for it.
    /// Returns the executions that ended. An error stops the pass at that
    /// session; the precommits of the sessions after it stay in `prefetched`.
    fn advance_running(
        &mut self,
        running: Vec<RunningSession>,
        prefetched: &mut PrefetchedPass,
        report: &mut TickReport,
    ) -> Result<Vec<(SessionId, ExecutionId)>, StoreError> {
        let mut ended_now = Vec::new();
        // Terminals whose agent the host saw change (a host that detects
        // agents, Herdr): their panes are read on this pass.
        let changed: HashSet<TerminalName> = self.terminals.agent_changes().into_iter().collect();
        for RunningSession {
            session,
            agent_id,
            native,
            mut active,
        } in running
        {
            if changed.contains(&active.terminal()) {
                active.follow.again = true;
            }
            let prefetch = AdvancePrefetch {
                register_watch: !prefetched.registered.contains(&session),
                read: prefetched.reads.remove(&session),
                precommitted: Some(&mut prefetched.precommitted),
            };
            let ended = self.advance(
                &agent_id,
                &session,
                native.as_deref(),
                &mut active,
                prefetch,
                report,
            )?;
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
        Ok(ended_now)
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
        mut prefetch: AdvancePrefetch<'_>,
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
        let bound = self.native_of(agent_id, session, native, active)?;
        let native = bound.as_deref();
        if prefetch.register_watch {
            self.register_watch(session, native, active, report);
        }
        self.follow_output(
            agent_id,
            session,
            native,
            active,
            FollowPrefetch {
                read: prefetch.read,
                precommitted: prefetch.precommitted.as_deref_mut(),
            },
            report,
        )?;
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
            if let Some(reason) = active.checkpoint
                && let Some(precommit) = prefetch
                    .precommitted
                    .as_mut()
                    .and_then(|map| map.remove(session))
                && self.checkpoint(agent_id, session, active, reason, Some(precommit), report)?
            {
                active.checkpoint = None;
            }
            // What the engine wrote last comes before its end.
            self.follow_output(
                agent_id,
                session,
                native,
                active,
                FollowPrefetch {
                    read: None,
                    precommitted: None,
                },
                report,
            )?;
            self.keep_archive(agent_id, session, active, true, report);
            return self.end(agent_id, session, active, ending, false, report);
        }
        if let Some(reason) = active.checkpoint {
            let precommit = prefetch
                .precommitted
                .as_mut()
                .and_then(|map| map.remove(session));
            // A commit that failed was reported; the checkpoint stays due.
            if self.checkpoint(agent_id, session, active, reason, precommit, report)? {
                active.checkpoint = None;
            }
        }
        // What the engine wrote is in the archive within a pass, a running
        // turn's model calls too, not only at the turn's end.
        self.keep_archive(agent_id, session, active, true, report);
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
    /// reads again: a line is never lost to the moment it was written. So
    /// does a read that came back full (objective 241): a backlog over one
    /// read ([`READ_LIMIT`]), such as a long transcript read again from its
    /// execution's start after a restart, is read through in passes, never
    /// left waiting for the next change of the file (LOOK 4082: an idle
    /// session's new turn was never reached, its message read as not taken).
    ///
    /// A transcript that shrank below the offset, or was replaced (other
    /// bytes than the ones read just before the offset: a rewrite, a
    /// restore, a file renamed over it, of any length), is read again from
    /// the start, warned once per time it is found. Without this the offset
    /// pointed past the end of the new file, every read came back empty and
    /// its lines were never seen (incident 2026-11-12, D1: markers never
    /// taken, turns and usage missed). Reading again appends nothing twice:
    /// a line's events carry its key (`source_event_id`), which the log
    /// takes once per session.
    fn bytes_end_turn(driver: &dyn AgentDriver, cursor: &LineCursor, bytes: &[u8]) -> bool {
        let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') else {
            return false;
        };
        let mut probe = cursor.clone();
        bytes[..end].split(|byte| *byte == b'\n').any(|line| {
            let line = String::from_utf8_lossy(line);
            driver
                .read_line(&line, &mut probe)
                .events
                .iter()
                .any(|event| event.kind.checkpoint_reason() == Some(CheckpointReason::TurnStopped))
        })
    }

    fn ingest_followed_bytes(
        &self,
        target: &TranscriptTarget<'_>,
        active: &mut Active,
        bytes: &[u8],
        mut precommitted: Option<&mut PrecommittedMap>,
        report: &mut TickReport,
    ) -> Result<bool, StoreError> {
        let mut read = 0_usize;
        // One checkpoint per read: the workspace does not change while the
        // lines are read, so a later turn end in the same read finds the
        // one taken for the first, and a transcript read again from the
        // start commits once, not at each of its old turns (rev-g on S15).
        let mut checkpointed = false;
        for line in bytes.split(|byte| *byte == b'\n') {
            let start = read;
            read += line.len() + 1;
            let line = String::from_utf8_lossy(line);
            // A model call's usage, for the cost step, comes before the
            // line's other events, so a turn's end follows its last call's
            // usage ([`AgentDriver::read_line`]).
            let facts = active.driver.read_line(&line, &mut active.follow.cursor);
            let events = facts.events;
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
            if ends_turn && !checkpointed {
                let precommit = precommitted
                    .as_mut()
                    .and_then(|map| map.remove(target.session));
                if !self.checkpoint(
                    target.agent_id,
                    target.session,
                    active,
                    CheckpointReason::TurnStopped,
                    precommit,
                    report,
                )? {
                    active.follow.advance(&bytes[..start]);
                    // The commit stays due: the line is read again next pass.
                    return Ok(false);
                }
                checkpointed = true;
                active.checkpoint = None;
            }
            if active.follow.version.is_none()
                && let Some(version) = facts.version
            {
                self.record_version(
                    target.agent_id,
                    target.session,
                    target.native,
                    active,
                    target.path,
                    version,
                )?;
            }
            for event in events {
                self.log.append(NewEvent {
                    agent_id: target.agent_id.clone(),
                    session_id: target.session.clone(),
                    execution_id: Some(active.execution_id.clone()),
                    source: Source::Claude,
                    source_event_id: event.source_event_id,
                    kind: event.kind,
                })?;
            }
        }
        Ok(true)
    }

    fn follow_output(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        native: Option<&str>,
        active: &mut Active,
        prefetch: FollowPrefetch<'_>,
        report: &mut TickReport,
    ) -> Result<(), StoreError> {
        let watched = self.config.watch.is_some() && !active.follow.watch.poll;
        if watched && !active.follow.watch.dirty && !self.due_to_read(active) {
            return Ok(());
        }
        let Some(native) = native else {
            return Ok(());
        };
        let path = self.transcript_of(active.driver, &active.workspace, native);
        active.follow.reading(native);
        // Read past the offset only while the file holds what was read there
        // ([`Bridge::read_followed`]); `limit`: a full read's bytes past it.
        let read_result = match prefetch.read {
            Some(result) => result,
            None => self.read_followed(session, &path, active, report),
        };
        let (bytes, limit) = match read_result {
            Ok(Some(read)) => read,
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
                // Nothing is known of what it holds: the archive reads it.
                active.follow.archiving.due = true;
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
        // The file is there (no grace to wait for): due next pass only when
        // this read came back full, so more of it waits to be read.
        active.follow.due_at = (bytes.len() >= limit).then(|| self.clock.now().0);
        if !bytes.is_empty() {
            // The transcript grew: the archive copies it on this pass.
            active.follow.archiving.due = true;
            // Its screen is looked at once it holds still, not while it is
            // being written.
            active.follow.unread_since = Some(self.clock.now().0);
        }
        let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') else {
            // A full read without a newline is one line longer than the
            // limit: skip what was read rather than stall on it forever (the
            // rest of it parses as nothing). Anything shorter is a line
            // still being written: it is read next pass.
            if bytes.len() >= limit {
                active.follow.advance(&bytes);
                active.follow.watch.dirty = false;
                report.warnings.push(format!(
                    "{session}: skipped a line over {READ_LIMIT} bytes in {path}"
                ));
            }
            return Ok(());
        };
        let target = TranscriptTarget {
            agent_id,
            session,
            native,
            path: &path,
        };
        if self.ingest_followed_bytes(
            &target,
            active,
            &bytes[..end],
            prefetch.precommitted,
            report,
        )? {
            active.follow.advance(&bytes[..=end]);
            active.follow.watch.dirty = false;
        }
        Ok(())
    }

    /// The transcript at `path` past the offset, and how many bytes a full
    /// read holds there. It is read from [`Follow::kept`] bytes before the
    /// offset, and those must be what was read there. When they are not
    /// (the file shrank below the offset, or was replaced) it is said, and
    /// the file is read again from the start ([`Bridge::follow_output`]).
    fn read_followed(
        &self,
        session: &SessionId,
        path: &str,
        active: &mut Active,
        report: &mut TickReport,
    ) -> Result<Option<(Vec<u8>, usize)>, TerminalError> {
        let kept = active.follow.kept();
        let from = active
            .follow
            .offset
            .saturating_sub(u64::try_from(kept).unwrap_or(0));
        let Some(mut bytes) = self.terminals.read_from(&active.workspace, path, from)? else {
            return Ok(None);
        };
        let tail = &active.follow.tail;
        if bytes.len() >= kept && (tail.is_empty() || bytes[..kept] == tail[..]) {
            bytes.drain(..kept);
            return Ok(Some((bytes, READ_LIMIT.saturating_sub(kept))));
        }
        // Lines read before are not appended twice (their events' keys are
        // taken), so reading it again loses nothing and repeats nothing.
        report.warnings.push(format!(
            "{session}: the transcript {path} shrank below the {} bytes read, \
             or was replaced: reading it again from the start",
            active.follow.offset
        ));
        active.follow.offset = 0;
        active.follow.tail.clear();
        Ok(self
            .terminals
            .read_from(&active.workspace, path, 0)?
            .map(|bytes| (bytes, READ_LIMIT)))
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
            paths.push(self.transcript_of(active.driver, &active.workspace, native));
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

    /// Whether an execution is waiting on its start or resume prompt, so
    /// [`Self::follow_screen`] leaves the pane to `type_prompt` until the
    /// initial prompt is typed.
    fn prompting_execution(&self, execution: &ExecutionId) -> bool {
        self.prompting
            .keys()
            .any(|id| ExecutionId::for_command(*id) == *execution)
    }

    /// Why this pass wants the pane read, if it does.
    fn screen_wanted(&self, session: &SessionId, active: &Active) -> Wanted {
        if self.prompting_execution(&active.execution_id) {
            return Wanted::Nothing;
        }
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
                engine: active.driver.id().to_string(),
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
        let mut reading = active.driver.read_screen(&pane);
        // The host's own reading of the agent, when it has one (Herdr): a
        // second witness. Without one (tmux, in memory) nothing below
        // changes. Claude Code's screens under Herdr (its screen-reader
        // layout) are not captured yet against Herdr's claude manifest
        // (design §9.2): until they are, Claude Code goes by its own
        // readings alone, on any host, and the host is not even asked.
        let witness = (active.driver.id() != crate::driver::DEFAULT)
            .then(|| {
                self.terminals
                    .agent(&active.terminal(), &active.workspace)
                    .ok()
                    .flatten()
            })
            .flatten();
        let mut agreed = false;
        if let Some(witness) = &witness {
            match merge_witness(active.driver, witness, &reading) {
                Merged::Agrees(state, evidence) => {
                    reading.state = state;
                    reading.evidence = evidence;
                    agreed = true;
                }
                Merged::Own => {}
                // The engine may have exited: its exit status is read on
                // the next pass, and the screen goes by its own readings.
                Merged::Gone => active.follow.watch.dirty = true,
                // Another agent in the engine's pane: nothing is known.
                Merged::Foreign => {
                    active.follow.seen = None;
                    active.follow.again = true;
                    return Ok(());
                }
            }
        }
        // A reading that accepts input is the screen showing the turn over:
        // what a turn's end in the transcript was waiting for.
        if reading.state.accepts_input() {
            active.follow.settling = None;
        }
        let confirmed = agreed || active.follow.seen == Some(reading.state);
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
        // Keyed by the host's reading when there is one, so a reading
        // recorded once (by a Bridge before a restart too) is recorded once;
        // a busy screen recorded again after `screen_refresh` is the same
        // reading, and goes unkeyed so it is never taken for a repeat.
        let changed = active
            .follow
            .screen
            .is_none_or(|(state, _)| state != reading.state);
        let key = match (&witness, self.terminals.server()) {
            (Some(witness), ServerSeen::Answers(Some(server)))
                if changed && active.follow.keyed_seq != Some(witness.seq) =>
            {
                active.follow.keyed_seq = Some(witness.seq);
                Some(format!(
                    "{}:hs:{}.{}:{}",
                    active.execution_id, server.pid, server.started_at_ms, witness.seq
                ))
            }
            _ => None,
        };
        active.follow.screen = Some((reading.state, now));
        self.append(
            agent_id,
            session,
            Some(&active.execution_id),
            Source::Bridge,
            key,
            EventKind::ScreenObserved {
                state: reading.state,
                evidence: reading.evidence,
            },
        )?;
        Ok(())
    }

    /// The native session whose transcript the execution writes, when it is
    /// known: the session's, for an engine the Bridge names the session of
    /// (or a resume); for an engine that names its own
    /// ([`SessionBinding::Reported`]) only once this execution's is bound
    /// ([`Self::bind_reported`]), never the session's last one, which is an
    /// earlier execution's.
    fn native_of(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        native: Option<&str>,
        active: &mut Active,
    ) -> Result<Option<String>, StoreError> {
        if active.driver.traits().session != SessionBinding::Reported {
            return Ok(native.map(str::to_string));
        }
        match &active.follow.binding {
            Binding::Bound(bound) => Ok(bound.clone().or_else(|| native.map(str::to_string))),
            Binding::Unbound => self.bind_reported(agent_id, session, active),
        }
    }

    /// Bind the native session an engine named itself
    /// ([`SessionBinding::Reported`]): its transcript is there below the
    /// engine's home, and a prompt in it holds the marker of a command Spawn
    /// typed into this session (its `typing` step is this session's) for
    /// this execution or later (its start or after).
    ///
    /// The candidate sessions are, first, the one the host reports for the
    /// execution's own pane ([`TerminalHost::agent`]) as the engine's own
    /// agent's (`agent` is the driver's id); then, when the host reports
    /// none (or the one it reports is not this execution's) and something
    /// was typed into this execution, the conversations below the engine's
    /// home ([`AgentDriver::session_scan_dir`], [`TerminalHost::list_dirs`],
    /// newest first). That fallback is what binds agy inside its filesystem
    /// view, where its hooks (`~/.gemini/config/hooks.json`) and the host's
    /// socket stay masked, so it never reports a session to the host. A
    /// pane the host says runs another agent binds nothing.
    ///
    /// The marker is the proof, whichever way a candidate came: a session
    /// reported for the pane by anything else (`pane.report_agent_session`
    /// is open to any client of the server), or a conversation directory
    /// another execution or anything else made, whose transcript quotes
    /// another session's marker, or one never typed, is never bound. Then
    /// `session.native_started` is recorded, and the transcript is read from
    /// its start. Until then nothing is bound, and asked again next pass.
    ///
    /// A marker is only a store-local command id, so another store's or an
    /// older run's conversation in the same shared home may quote the same
    /// one (D1). Three guards keep those out: the prompt that holds the
    /// marker must be no older than the execution's `execution.started`
    /// (its `created_at`, less [`BIND_SKEW_MS`]; missing or unreadable
    /// binds nothing); a conversation that was below the home before a
    /// fresh start launched the engine ([`Active::preexisting_natives`]) is
    /// never scanned into; and a native session any execution in the store
    /// was already bound to is never bound again.
    fn bind_reported(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &mut Active,
    ) -> Result<Option<String>, StoreError> {
        let Ok(reading) = self.terminals.agent(&active.terminal(), &active.workspace) else {
            return Ok(None);
        };
        // Another agent in the engine's pane: nothing is known.
        if reading
            .as_ref()
            .and_then(|reading| reading.agent.as_deref())
            .is_some_and(|agent| agent != active.driver.id())
        {
            return Ok(None);
        }
        let reported = reading
            .filter(|reading| reading.agent.as_deref() == Some(active.driver.id()))
            .and_then(|reading| reading.session)
            .filter(|native| plain_session_id(native));
        let first = active
            .execution_id
            .as_str()
            .strip_prefix("exec_")
            .and_then(|command| command.parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        let view = self.sessions.get(session);
        let ours = |command: &CommandId| {
            command.0 >= first
                && view.is_some_and(|view| {
                    view.steps
                        .get(command)
                        .is_some_and(|steps| steps.typing_at.is_some())
                })
        };
        let driver = active.driver;
        let workspace = active.workspace.clone();
        let home = self.config.home_of(driver);
        let root = format!("{}/", home.sessions.trim_end_matches('/'));
        let not_before = active.started_at.saturating_sub(BIND_SKEW_MS);
        // Bound already, by any execution of any session in the store.
        let taken = |native: &str| {
            self.sessions
                .values()
                .any(|view| view.natives.contains(native))
        };
        // The transcript of `native`, when it is below the engine's home and
        // a prompt in it, written since this execution started, holds a
        // marker typed into this execution.
        let proven = |native: &str| -> Option<String> {
            if taken(native) {
                return None;
            }
            let path = driver.transcript_path(&home, &workspace, native);
            if !path.starts_with(&root) {
                return None;
            }
            let Ok(Some(bytes)) = self.terminals.read_from(&workspace, &path, 0) else {
                return None;
            };
            let typed = String::from_utf8_lossy(&bytes).lines().any(|line| {
                serde_json::from_str::<serde_json::Value>(line).is_ok_and(|value| {
                    value["type"] == "USER_INPUT"
                        && value["created_at"]
                            .as_str()
                            .and_then(rfc3339_ms)
                            .is_some_and(|created| {
                                u64::try_from(created).is_ok_and(|created| created >= not_before)
                            })
                        && value["content"].as_str().is_some_and(|content| {
                            crate::message::markers(content).iter().any(ours)
                        })
                })
            });
            typed.then_some(path)
        };
        let mut bound = reported
            .as_deref()
            .and_then(|native| proven(native).map(|path| (native.to_string(), path)));
        let now = self.clock.now().0;
        if bound.is_none()
            && let Some(dir) = driver.session_scan_dir(&home)
            && view.is_some_and(|view| {
                view.steps
                    .iter()
                    .any(|(command, steps)| command.0 >= first && steps.typing_at.is_some())
            })
            && active
                .follow
                .scanned_at
                .is_none_or(|at| now.saturating_sub(at) >= SCAN_INTERVAL_MS)
            && let Ok(names) = self.terminals.list_dirs(&workspace, &dir)
        {
            active.follow.scanned_at = Some(now);
            bound = names
                .iter()
                .filter(|native| plain_session_id(native))
                .filter(|native| reported.as_deref() != Some(native.as_str()))
                .filter(|native| !active.preexisting_natives.contains(native.as_str()))
                .take(crate::port::LIST_DIRS_LIMIT)
                .find_map(|native| proven(native).map(|path| (native.clone(), path)));
        }
        let Some((native, path)) = bound else {
            return Ok(None);
        };
        self.append(
            agent_id,
            session,
            Some(&active.execution_id),
            Source::Bridge,
            Some(format!("{}:native", active.execution_id)),
            EventKind::NativeSessionStarted {
                engine: active.driver.id().to_string(),
                native_session_id: native.clone(),
                transcript_path: Some(path),
                engine_version: None,
            },
        )?;
        active.follow.binding = Binding::Bound(Some(native.clone()));
        active.follow.offset = 0;
        active.follow.tail.clear();
        active.follow.watch.dirty = true;
        Ok(Some(native))
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
                let error = format!("terminate {}: {error}", active.terminal());
                active.follow.end_error = Some(error.clone());
                report.warnings.push(error);
                return Ok(false);
            }
            active.follow.kill_failed_at = None;
            active.follow.end_error = None;
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
        active.follow.end_error = None;
        let committed = self.checkpoint(
            agent_id,
            session,
            active,
            CheckpointReason::ExecutionEnded,
            None,
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
        // A copy that fails is made on a later pass ([`Self::archive_ended`]).
        self.keep_archive(agent_id, session, active, false, report);
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
            // The host ends the processes left beneath the workspace too (a
            // daemon the engine detached), by pid, before it returns: the
            // straggler check runs before the final checkpoint below.
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
        active.follow.end_error = Some(error.clone());
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
    /// execution ends); false: the commit failed, and was reported if it
    /// started failing or failed for another cause ([`failure_key`]). A
    /// branch the store made again from the workspace ([`Commit::Recreated`])
    /// is logged once and recorded before the checkpoint
    /// ([`Self::record_recreated`]).
    fn checkpoint(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &mut Active,
        reason: CheckpointReason,
        precommitted: Option<Result<Commit, WorkspaceError>>,
        report: &mut TickReport,
    ) -> Result<bool, StoreError> {
        let message = format!("spawn checkpoint {}", active.execution_id);
        let outcome = match precommitted {
            Some(outcome) if reason != CheckpointReason::ExecutionEnded => outcome,
            _ => self.workspaces.commit(&active.workspace, &message),
        };
        if outcome.is_ok() && active.follow.commit_failed.take().is_some() {
            report
                .notes
                .push(format!("{session}: commit available again"));
        }
        match outcome {
            Ok(Commit::Unchanged) => {}
            Ok(Commit::Created(commit)) => {
                self.record_checkpoint(agent_id, session, active, reason, commit)?;
            }
            Ok(Commit::Recreated { commit, parent }) => {
                let branch = BranchName::for_agent(agent_id);
                report.warnings.push(match &parent {
                    Some(parent) => format!(
                        "{session}: branch {branch} was gone from its store; made again at \
                         {commit} from the workspace, on {parent}"
                    ),
                    None => format!(
                        "{session}: branch {branch} was gone from its store; made again at \
                         {commit} from the workspace, with no parent (the store has no main)"
                    ),
                });
                self.record_recreated(agent_id, session, &active.execution_id, &commit, parent);
                self.record_checkpoint(agent_id, session, active, reason, commit)?;
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
            Err(error) => {
                let key = failure_key(&error);
                if active.follow.commit_failed.as_deref() != Some(key.as_str()) {
                    report.warnings.push(match &error {
                        WorkspaceError::Refused { code, message } => {
                            format!("commit refused {code}: {message}")
                        }
                        WorkspaceError::Unavailable(text) => format!("commit unavailable: {text}"),
                    });
                }
                active.follow.commit_failed = Some(key);
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

    /// Record the checkpoint `commit` of the active execution
    /// (`artifact.committed`, [`Self::record_commit`]).
    fn record_checkpoint(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &Active,
        reason: CheckpointReason,
        commit: CommitId,
    ) -> Result<(), StoreError> {
        self.queue_commit(self.checkpoint_event(
            agent_id,
            session,
            &active.execution_id,
            reason,
            commit,
        ));
        self.record_unrecorded_commit(session)
    }

    /// The `artifact.committed` of checkpoint `commit` of `execution`, keyed
    /// `commit:<id>`, so a second append of one that did land is a no-op.
    fn checkpoint_event(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        execution: &ExecutionId,
        reason: CheckpointReason,
        commit: CommitId,
    ) -> NewEvent {
        NewEvent {
            agent_id: agent_id.clone(),
            session_id: session.clone(),
            execution_id: Some(execution.clone()),
            source: Source::Bridge,
            source_event_id: Some(format!("commit:{commit}")),
            kind: EventKind::ArtifactCommitted {
                reason,
                artifact: ArtifactRef {
                    project: self.workspaces.project().to_string(),
                    branch: BranchName::for_agent(agent_id),
                    commit,
                },
                files: Vec::new(),
            },
        }
    }

    /// Queue the records of every precommit of this pass no `advance` took
    /// ([`Self::prefetch_and_precommit`]): its branch already moved, so a
    /// pass that stopped early (an append the log refused for an earlier
    /// session) must not lose the commit's `artifact.committed`, which the
    /// next pass would read as [`Commit::Unchanged`] and so never record
    /// before the turn's `turn.stopped`. It waits with the session's commit
    /// records and is appended before anything else of that session. A
    /// failed or empty precommit records nothing: its checkpoint stays due.
    fn keep_unconsumed_precommits(
        &self,
        precommitted: PrecommittedMap,
        mut owners: HashMap<SessionId, PrecommitOwner>,
    ) {
        for (session, outcome) in precommitted {
            let Some(owner) = owners.remove(&session) else {
                continue;
            };
            let commit = match outcome {
                Ok(Commit::Created(commit)) => commit,
                Ok(Commit::Recreated { commit, parent }) => {
                    self.record_recreated(
                        &owner.agent_id,
                        &session,
                        &owner.execution,
                        &commit,
                        parent,
                    );
                    commit
                }
                Ok(Commit::Unchanged) | Err(_) => continue,
            };
            self.queue_commit(self.checkpoint_event(
                &owner.agent_id,
                &session,
                &owner.execution,
                owner.reason,
                commit,
            ));
        }
    }

    /// Queue the `artifact.branch_recreated` of a branch the store made
    /// again at `commit` (objective 205): the branch already moved, so it
    /// waits with the session's commit records ([`Self::queue_commit`]), and
    /// the checkpoint's own `artifact.committed`, recorded next, appends
    /// both in order. A log down at the first append loses neither: the
    /// next pass appends both. Keyed `recreated:<id>`, so a second append
    /// of one that did land is a no-op.
    fn record_recreated(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        execution: &ExecutionId,
        commit: &CommitId,
        parent: Option<CommitId>,
    ) {
        self.queue_commit(NewEvent {
            agent_id: agent_id.clone(),
            session_id: session.clone(),
            execution_id: Some(execution.clone()),
            source: Source::Bridge,
            source_event_id: Some(format!("recreated:{commit}")),
            kind: EventKind::ArtifactBranchRecreated {
                artifact: ArtifactRef {
                    project: self.workspaces.project().to_string(),
                    branch: BranchName::for_agent(agent_id),
                    commit: commit.clone(),
                },
                parent,
            },
        });
    }

    /// Put `event` behind the session's waiting commit records, without
    /// appending it yet ([`Self::record_unrecorded_commit`] appends them).
    fn queue_commit(&self, event: NewEvent) {
        self.unrecorded_commits_lock()
            .entry(event.session_id.clone())
            .or_default()
            .push(event);
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
        self.queue_commit(event);
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

    fn has_unrecorded_commits(&self, session: &SessionId) -> bool {
        self.unrecorded_commits_lock()
            .get(session)
            .is_some_and(|waiting| !waiting.is_empty())
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
            return Ok(Handled::Waiting {
                retry: self.config.input_wait_retry,
            });
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
            (_, Some(Settled::Pending)) => {
                return Ok(Handled::Waiting {
                    retry: self.config.input_wait_retry,
                });
            }
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
        match action {
            CommandAction::Start {
                engine,
                prompt,
                parent_session_id,
                from,
                model,
                // The control layer's record of the charter it gave: nothing
                // the Bridge does depends on it.
                charter: _,
                model_label,
            } => self.start(
                claimed,
                &Begin {
                    prompt: &prompt,
                    parent: parent_session_id.as_ref(),
                    native: None,
                    from: from.as_deref(),
                    model,
                    model_label: model_label.as_deref(),
                    driver: driver_or_default(&engine),
                },
                report,
            ),
            CommandAction::Input { text, .. } => self.input(claimed, &text),
            CommandAction::Key { key } => self.press(claimed, key).map(Handled::Finished),
            CommandAction::Stop => self.stop(claimed, false, report),
            CommandAction::Discard => self.stop(claimed, true, report),
            CommandAction::Resume { prompt, model } => {
                self.resume(claimed, prompt.as_deref(), model, report)
            }
        }
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
            engine,
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
            driver_or_default(engine),
            record.recorded_at,
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
            held.follow.end_error.clone_from(&active.follow.end_error);
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
    ) -> Result<Handled, StoreError> {
        let Begin {
            prompt,
            parent,
            native,
            driver,
            ..
        } = *begin;
        let finished = |outcome| Ok(Handled::Finished(outcome));
        // Another Bridge may have started the session since this tick read
        // the log; the log refuses a second start either way.
        self.catch_up()?;
        if let Some(handled) = self.started_before(claimed, prompt) {
            return handled;
        }
        let branch = BranchName::for_agent(&claimed.agent_id);
        let prepared = match self.prepare_start(claimed, &branch, begin, report)? {
            Ok(prepared) => prepared,
            Err(why) => return finished(self.fail_prepared_start(claimed, why)),
        };
        let workspace = match self.mount_for_start(claimed, &branch)? {
            MountProgress::Ready(workspace) => workspace,
            MountProgress::Pending => {
                return Ok(Handled::Waiting {
                    retry: MOUNT_POLL_RETRY,
                });
            }
            MountProgress::Failed(why) => return finished(Outcome::Failed(why)),
        };
        if let Some(why) = self.no_agent_host(claimed, driver, &workspace) {
            return finished(self.give_up_mount(claimed, &workspace, why));
        }
        // A resumed session's transcript holds the earlier executions' lines:
        // this execution's are the ones after its length now.
        let transcript_from = match native
            .map(|native| self.resumed_transcript(claimed, (driver, &workspace), native, report))
            .transpose()
        {
            Ok(length) => length.flatten(),
            Err(why) => return finished(self.give_up_mount(claimed, &workspace, why)),
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
            driver: prepared.driver,
            checkpoint: None,
            reported_end: None,
            unmounting: false,
            follow: Follow {
                binding: Binding::resumed(native.is_some()),
                ..Follow::default()
            },
            started_at: u64::try_from(self.clock.now().0).unwrap_or(0),
            preexisting_natives: HashSet::new(),
        };
        // A fresh start of an engine that names its own session: the
        // conversations already below its home are none of this
        // execution's ([`Self::bind_reported`]). Listed before the launch,
        // handed to the view's execution when its record is folded.
        if native.is_none()
            && prepared.driver.traits().session == SessionBinding::Reported
            && let Some(dir) = prepared
                .driver
                .session_scan_dir(&self.config.home_of(prepared.driver))
            && let Ok(names) = self.terminals.list_dirs(&active.workspace, &dir)
        {
            self.preexisting
                .insert(active.execution_id.clone(), names.into_iter().collect());
        }
        // The event goes first: a crash after it leaves a recorded execution
        // whose missing terminal the next tick ends as lost.
        let recorded = self.append(
            &claimed.agent_id,
            &claimed.session_id,
            Some(&active.execution_id),
            Source::Bridge,
            Some(format!("{}:started", active.execution_id)),
            EventKind::ExecutionStarted {
                engine: prepared.driver.id().to_string(),
                workspace: active.workspace.clone(),
                command_id: claimed.command_id,
                fork: prepared.fork.clone(),
                model: prepared.model,
                transcript_from,
                launch: (!prepared.launch.is_empty()).then(|| prepared.launch.clone()),
            },
        );
        match recorded {
            Ok(()) => {}
            Err(StoreError::Rejected(why)) => {
                return self
                    .refused_start(claimed, &active.workspace, why)
                    .map(Handled::Finished);
            }
            // Whether it reached the log is unknown: the mount goes either
            // way. A recorded execution without its terminal or workspace
            // ends as lost on the next tick.
            Err(error) => {
                self.give_up_mount(claimed, &active.workspace, error.to_string());
                return Err(error);
            }
        }
        self.prepared.remove(&claimed.command_id);
        self.launch_and_prompt(claimed, &active, native, &prepared, prompt, report)
    }

    /// Why `driver`'s engine cannot start on this host for want of a
    /// filesystem view: it has no deny rules of its own
    /// ([`crate::driver::Guard::Sandbox`]), the host requires a view for
    /// it ([`BridgeConfig::engine_view`]) and has none. Checked before
    /// anything is mounted; the view of the launch itself is made at the
    /// launch ([`Self::launch`]).
    fn no_engine_view(&self, driver: &dyn AgentDriver) -> Option<String> {
        if driver.traits().guard != crate::driver::Guard::Sandbox {
            return None;
        }
        match &self.config.engine_view {
            EngineViewPolicy::Required(Err(why)) => Some(format!(
                "{ENGINE_UNAVAILABLE}: engine {} has no deny rules of its own and runs only inside \
                 a filesystem view (SPAWN_ENGINE_VIEW=required), which this host has none of: {why}",
                driver.id()
            )),
            EngineViewPolicy::Required(Ok(_)) | EngineViewPolicy::Off => None,
        }
    }

    /// Why `driver`'s engine cannot run on this terminal host: an engine
    /// other than Claude Code is followed through the host's own agent
    /// readings (its session, its state), which a host with none (tmux)
    /// cannot give.
    fn no_agent_host(
        &self,
        claimed: &Claimed,
        driver: &dyn AgentDriver,
        workspace: &WorkspaceId,
    ) -> Option<String> {
        if driver.id() == crate::driver::DEFAULT {
            return None;
        }
        let terminal = TerminalName::for_execution(&ExecutionId::for_command(claimed.command_id));
        (!matches!(self.terminals.agent(&terminal, workspace), Ok(Some(_)))).then(|| {
            format!(
                "engine {} needs the herdr terminal backend (SPAWN_TERMINAL_BACKEND=herdr): this \
                 terminal host reads no agent",
                driver.id()
            )
        })
    }

    fn fail_prepared_start(&self, claimed: &Claimed, why: String) -> Outcome {
        let steps = self.recorded_steps(claimed);
        if let Some(workspace) = steps.mounted
            && !steps.released
        {
            if self.workspaces.is_mounted(&workspace) {
                return self.give_up_mount(claimed, &workspace, why);
            }
            let _ = self.append(
                &claimed.agent_id,
                &claimed.session_id,
                None,
                Source::Bridge,
                Some(mount_released_key(claimed.command_id, steps.mounts.max(1))),
                EventKind::CommandStepped {
                    command_id: claimed.command_id,
                    step: CommandStep::MountReleased,
                    workspace: Some(workspace),
                },
            );
        }
        Outcome::Failed(why)
    }

    /// Choose and retain everything a start decided before its mount. On a
    /// later claim the same model, fork, settings and launch summary are used.
    fn prepare_start(
        &mut self,
        claimed: &Claimed,
        branch: &BranchName,
        begin: &Begin<'_>,
        report: &mut TickReport,
    ) -> Result<Result<PreparedStart, String>, StoreError> {
        if let Some(prepared) = self.prepared.get(&claimed.command_id) {
            return Ok(Ok(prepared.clone()));
        }
        let Begin {
            from,
            model,
            model_label,
            driver,
            ..
        } = *begin;
        // Only the engines this host enables (`SPAWN_ENGINES`) start, and
        // one other than Claude Code only with its model named, by the
        // start (`model_label`) or the host (`SPAWN_<ENGINE>_MODEL`): an
        // engine left to pick its own may pick its costliest.
        let mut label = None;
        if driver.id() != crate::driver::DEFAULT {
            match self.config.engine_home(driver.id()) {
                None => {
                    return Ok(Err(format!(
                        "engine {} is not enabled on this host: SPAWN_ENGINES does not name it",
                        driver.id()
                    )));
                }
                Some(home) => match model_label.map(str::to_string).or(home.model.clone()) {
                    Some(named) => label = Some(named),
                    None => {
                        return Ok(Err(format!(
                            "engine {} has no model named: the start names no model_label and \
                             this host none (SPAWN_{}_MODEL)",
                            driver.id(),
                            driver.id().to_ascii_uppercase()
                        )));
                    }
                },
            }
        }
        if let Some(why) = self.no_engine_view(driver) {
            return Ok(Err(why));
        }
        let model = model.unwrap_or_else(|| {
            self.sessions
                .get(&claimed.session_id)
                .map_or(ModelProfile::Default, |view| view.model)
        });
        let (relay, config_dir, mut launch) = match self.engine_settings(claimed, model, report) {
            Ok(settings) => settings,
            Err(why) => return Ok(Err(why)),
        };
        // An engine told its model at launch runs on no relay of Claude
        // Code's: its launch summary is the model it runs, which a resume
        // of the session reads back (`SessionView::model_label`).
        if label.is_some() {
            launch = LaunchSummary {
                model: label.clone(),
                ..LaunchSummary::default()
            };
        }
        let fork = match self.ensure_branch(claimed, branch, from, report) {
            Ok(fork) => fork,
            Err(why) => return Ok(Err(why)),
        };
        self.merge_main_at_start(claimed, branch, report)?;
        let prepared = PreparedStart {
            driver,
            relay,
            config_dir,
            launch,
            model,
            model_label: label,
            fork,
        };
        self.prepared.insert(claimed.command_id, prepared.clone());
        Ok(Ok(prepared))
    }

    /// Reuse this command's recorded mount, or advance its one mount worker.
    /// No event, queue operation or session mutation happens on the worker.
    fn mount_for_start(
        &mut self,
        claimed: &Claimed,
        branch: &BranchName,
    ) -> Result<MountProgress, StoreError> {
        self.reap_cancelled_mounts();
        if let Some(progress) = self.reuse_recorded_mount(claimed, branch)? {
            return Ok(progress);
        }
        if !self.mounting.contains_key(&claimed.command_id) {
            if self.mounting.len() >= MAX_CONCURRENT_MOUNTS
                || self
                    .mounting
                    .values()
                    .any(|pending| pending.branch == *branch)
            {
                return Ok(MountProgress::Pending);
            }
            self.spawn_mount_worker(claimed, branch);
        }
        if self
            .mounting
            .get(&claimed.command_id)
            .is_some_and(|pending| !pending.handle.is_finished())
        {
            return Ok(MountProgress::Pending);
        }
        let pending = self
            .mounting
            .remove(&claimed.command_id)
            .expect("the mount was inserted above");
        let workspace = match pending.handle.join() {
            Ok(Ok(workspace)) => workspace,
            Ok(Err(WorkspaceError::Refused { code, .. })) if code == MOUNT_CANCELLED => {
                self.catch_up()?;
                if self.recorded_steps(claimed).started {
                    return Ok(MountProgress::Pending);
                }
                return Ok(self
                    .reuse_recorded_mount(claimed, branch)?
                    .unwrap_or(MountProgress::Pending));
            }
            Ok(Err(error)) => {
                return Ok(MountProgress::Failed(format!("mount {branch}: {error}")));
            }
            Err(_) => {
                return Ok(MountProgress::Failed(format!(
                    "mount {branch}: the mount worker panicked"
                )));
            }
        };
        let generation = self.recorded_steps(claimed).mounts.saturating_add(1);
        // Recorded, so a restart that finds no execution using it can retain
        // it for this unfinished command or release a genuinely orphaned one
        // ([`Self::release_orphan_mounts`]).
        if let Err(error) = self.append(
            &claimed.agent_id,
            &claimed.session_id,
            None,
            Source::Bridge,
            Some(mounted_key(claimed.command_id, generation)),
            EventKind::CommandStepped {
                command_id: claimed.command_id,
                step: CommandStep::Mounted,
                workspace: Some(workspace.clone()),
            },
        ) {
            self.give_up_mount(claimed, &workspace, error.to_string());
            return Err(error);
        }
        self.catch_up()?;
        drop(self.workspaces.hold(&workspace));
        if !self.workspaces.is_mounted(&workspace) {
            return Ok(MountProgress::Pending);
        }
        Ok(MountProgress::Ready(workspace))
    }

    fn reuse_recorded_mount(
        &mut self,
        claimed: &Claimed,
        branch: &BranchName,
    ) -> Result<Option<MountProgress>, StoreError> {
        let steps = self.recorded_steps(claimed);
        let Some(workspace) = steps.mounted else {
            return Ok(None);
        };
        if steps.released {
            return Ok(None);
        }
        if self.workspaces.is_mounted(&workspace) {
            for pending in self.mounting.values_mut() {
                if pending.branch == *branch {
                    pending.cancel();
                }
            }
            self.reap_cancelled_mounts();
            if self
                .mounting
                .values()
                .any(|pending| pending.branch == *branch)
            {
                return Ok(Some(MountProgress::Pending));
            }
            drop(self.workspaces.hold(&workspace));
            if !self.workspaces.is_mounted(&workspace) {
                return Ok(Some(MountProgress::Pending));
            }
            return Ok(Some(MountProgress::Ready(workspace)));
        }
        self.append(
            &claimed.agent_id,
            &claimed.session_id,
            None,
            Source::Bridge,
            Some(mount_released_key(claimed.command_id, steps.mounts.max(1))),
            EventKind::CommandStepped {
                command_id: claimed.command_id,
                step: CommandStep::MountReleased,
                workspace: Some(workspace),
            },
        )?;
        self.catch_up()?;
        Ok(None)
    }

    fn spawn_mount_worker(&mut self, claimed: &Claimed, branch: &BranchName) {
        let workspaces = Arc::clone(&self.workspaces);
        let log = Arc::clone(&self.log);
        let worker_branch = branch.clone();
        let agent_id = claimed.agent_id.clone();
        let command_id = claimed.command_id;
        let after = self.cursor;
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let handle = std::thread::spawn(move || {
            workspaces.mount_when(&worker_branch, &|| {
                mount_still_wanted(&log, &worker_cancel, &agent_id, command_id, after)
            })
        });
        self.mounting.insert(
            command_id,
            PendingMount {
                agent_id: claimed.agent_id.clone(),
                session_id: claimed.session_id.clone(),
                branch: branch.clone(),
                cancel,
                handle,
                cancelled: false,
            },
        );
    }

    /// Join canceled commands' finished mount workers and retry any earlier
    /// cancelled/dropped mounts whose unmount or release record failed.
    fn reap_cancelled_mounts(&mut self) {
        let has_done_cancelled = self
            .mounting
            .values()
            .any(|pending| pending.cancelled && pending.handle.is_finished());
        if self.abandoned_mounts.is_empty() && !has_done_cancelled {
            return;
        }
        let caught_up = self.catch_up().is_ok();
        self.retry_abandoned_mounts(caught_up);
        let done: Vec<CommandId> = self
            .mounting
            .iter()
            .filter(|(_, pending)| pending.cancelled && pending.handle.is_finished())
            .map(|(id, _)| *id)
            .collect();
        for id in done {
            let pending = self.mounting.remove(&id).expect("listed above");
            if let Ok(Ok(workspace)) = pending.handle.join() {
                self.release_abandoned_mount(
                    pending.agent_id,
                    pending.session_id,
                    id,
                    workspace,
                    caught_up,
                );
            }
        }
    }

    /// Whether `workspace` (after canonicalization) currently backs a live
    /// execution or an unfinished, unreleased, unstarted command's recorded
    /// mount. `ignore_recorded_for` skips only this Bridge's own failed
    /// abandoned-mount record during `retry_abandoned_mounts`.
    fn workspace_in_use(
        &self,
        workspace: &WorkspaceId,
        ignore_recorded_for: Option<(CommandId, u32)>,
    ) -> bool {
        let canonical = self.workspaces.canonical(workspace);
        self.sessions.values().any(|view| {
            view.active
                .as_ref()
                .is_some_and(|active| active.workspace == canonical)
                || view.steps.iter().any(|(command, steps)| {
                    steps.mounted.as_ref().is_some_and(|mounted| {
                        !steps.released
                            && !steps.started
                            && view.unfinished.contains(command)
                            && ignore_recorded_for != Some((*command, steps.mounts))
                            && self.workspaces.canonical(mounted) == canonical
                    })
                })
        })
    }

    /// Unmount an unrecorded mount from a cancelled or dropped worker unless
    /// the canonical workspace backs a live execution or a peer's recorded
    /// mount. If the log could not be caught up or unmounting fails, retain it
    /// for later retry and (when caught up) persist `CommandStep::Mounted` for
    /// restart recovery.
    fn release_abandoned_mount(
        &mut self,
        agent_id: AgentId,
        session_id: SessionId,
        command_id: CommandId,
        workspace: WorkspaceId,
        caught_up: bool,
    ) {
        if !caught_up {
            self.abandoned_mounts.push(AbandonedMount {
                agent_id,
                session_id,
                command_id,
                workspace,
                unmounted: false,
                recorded_generation: None,
            });
            return;
        }
        if self.workspace_in_use(&workspace, None) {
            return;
        }
        let after = self.cursor;
        let log = Arc::clone(&self.log);
        let workspaces = Arc::clone(&self.workspaces);
        match self.workspaces.unmount_when(&workspace, &|| {
            unmount_still_safe(&log, &workspaces, &workspace, after)
        }) {
            Ok(()) => {}
            Err(WorkspaceError::Refused { code, .. }) if code == UNMOUNT_IN_USE => {}
            Err(error) => {
                let recorded_generation =
                    self.persist_abandoned_mount(&agent_id, &session_id, command_id, &workspace);
                self.stray_notes.push(format!(
                    "unmount {workspace} for cancelled {command_id}: {error}"
                ));
                self.abandoned_mounts.push(AbandonedMount {
                    agent_id,
                    session_id,
                    command_id,
                    workspace,
                    unmounted: false,
                    recorded_generation,
                });
            }
        }
    }

    /// Retry unmounting abandoned workspaces retained after transient failures,
    /// and retry recording `MountReleased` if an earlier unmount succeeded but
    /// appending `MountReleased` failed. Retains entries until both complete,
    /// skipping any workspace now owned by a live execution or peer mount.
    fn retry_abandoned_mounts(&mut self, caught_up: bool) {
        for mut abandoned in std::mem::take(&mut self.abandoned_mounts) {
            if !abandoned.unmounted {
                if !caught_up {
                    self.abandoned_mounts.push(abandoned);
                    continue;
                }
                let ignore = abandoned
                    .recorded_generation
                    .map(|generation| (abandoned.command_id, generation));
                if self.workspace_in_use(&abandoned.workspace, ignore) {
                    continue;
                }
                let after = self.cursor;
                let log = Arc::clone(&self.log);
                let workspaces = Arc::clone(&self.workspaces);
                let target = abandoned.workspace.clone();
                match self.workspaces.unmount_when(&target, &|| {
                    unmount_still_safe(&log, &workspaces, &target, after)
                }) {
                    Ok(()) => abandoned.unmounted = true,
                    Err(WorkspaceError::Refused { code, .. }) if code == UNMOUNT_IN_USE => {
                        continue;
                    }
                    Err(_) => {
                        self.abandoned_mounts.push(abandoned);
                        continue;
                    }
                }
            }
            if let Some(generation) = abandoned.recorded_generation
                && self
                    .append(
                        &abandoned.agent_id,
                        &abandoned.session_id,
                        None,
                        Source::Bridge,
                        Some(mount_released_key(abandoned.command_id, generation)),
                        EventKind::CommandStepped {
                            command_id: abandoned.command_id,
                            step: CommandStep::MountReleased,
                            workspace: Some(abandoned.workspace.clone()),
                        },
                    )
                    .is_err()
            {
                self.abandoned_mounts.push(abandoned);
            }
        }
    }

    /// Persist `CommandStep::Mounted` for an abandoned mount that could not yet
    /// be unmounted. Returns `Some(generation)` only when this Bridge inserted
    /// the record for `command_id` (not when an unreleased `mounted` step is
    /// already active or a peer Bridge recorded its own).
    fn persist_abandoned_mount(
        &self,
        agent_id: &AgentId,
        session_id: &SessionId,
        command_id: CommandId,
        workspace: &WorkspaceId,
    ) -> Option<u32> {
        let steps = self
            .sessions
            .get(session_id)
            .and_then(|view| view.steps.get(&command_id));
        if steps.is_some_and(|steps| steps.mounted.is_some() && !steps.released) {
            return None;
        }
        let generation = steps.map_or(1, |steps| steps.mounts.saturating_add(1));
        match self.log.append(NewEvent {
            agent_id: agent_id.clone(),
            session_id: session_id.clone(),
            execution_id: None,
            source: Source::Bridge,
            source_event_id: Some(mounted_key(command_id, generation)),
            kind: EventKind::CommandStepped {
                command_id,
                step: CommandStep::Mounted,
                workspace: Some(workspace.clone()),
            },
        }) {
            Ok(appended) if appended.inserted => Some(generation),
            _ => None,
        }
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
    ) -> Result<Handled, StoreError> {
        let finished = |outcome| Ok(Handled::Finished(outcome));
        // Nothing is typed into a terminal that does not host the runner.
        match self.terminals.runner(&active.terminal(), &active.workspace) {
            Ok(Runner::Running) => {}
            Ok(Runner::Gone) => {
                return finished(Outcome::Failed(format!(
                    "{}: the engine's terminal is gone; the execution ends as lost",
                    claimed.session_id
                )));
            }
            Ok(Runner::Untracked(seen)) => {
                return finished(Outcome::Failed(format!(
                    "{}: {seen}; the execution ends as lost",
                    claimed.session_id
                )));
            }
            Err(error) => {
                return finished(Outcome::Failed(format!(
                    "runner {}: {error}",
                    active.terminal()
                )));
            }
        }
        if prompt.is_empty() {
            return finished(Outcome::Succeeded);
        }
        match self.typing_of(claimed, &active.execution_id) {
            Typing::Done => finished(Outcome::Succeeded),
            Typing::Unknown => self
                .abandon(
                    claimed,
                    active,
                    format!("the prompt {MAY_HAVE_TYPED}"),
                    false,
                )
                .map(Handled::Finished),
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
    ) -> Option<Result<Handled, StoreError>> {
        if let Some(active) = self.running(&claimed.session_id) {
            if active.execution_id == ExecutionId::for_command(claimed.command_id) {
                return Some(self.go_on_starting(claimed, &active, prompt));
            }
            return Some(Ok(Handled::Finished(Outcome::Failed(format!(
                "{} already runs an execution",
                claimed.session_id
            )))));
        }
        self.recorded_steps(claimed).started.then(|| {
            Ok(Handled::Finished(Outcome::Failed(format!(
                "{}: this command's execution already ran and ended",
                claimed.session_id
            ))))
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
        // Pressed already (the Bridge's audited startup answer, or a press
        // before a restart): done, whatever runs now.
        if self.recorded_steps(claimed).press == Press::Pressed {
            return Ok(Outcome::Succeeded);
        }
        let Some(active) = self.running(&claimed.session_id) else {
            return Ok(Outcome::Failed(format!(
                "{} runs no execution: a key needs a running engine; resume the session",
                claimed.session_id
            )));
        };
        if claimed.principal == RESOLVE_PRINCIPAL
            && self.recorded_steps(claimed).press == Press::Due
            && !self.answer_still_due(claimed, &active)
        {
            return Ok(Outcome::Failed(GONE_DIALOG.to_string()));
        }
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

    /// Whether a resolver's key may still go (design section 8, PR 3
    /// pitfall 2: a person and the resolver may both answer one dialog):
    /// the pane, read now, still shows a dialog to the engine's driver
    /// (`Permission`, or its startup trust dialog), and no other command of
    /// the session was recorded `pressed` or `typed` since this one was
    /// accepted. A pane that cannot be read is no dialog.
    fn answer_still_due(&self, claimed: &Claimed, active: &Active) -> bool {
        let Ok(pane) =
            self.terminals
                .capture_tail(&active.terminal(), &active.workspace, TAIL_LINES)
        else {
            return false;
        };
        let shows = active.driver.read_screen(&pane).state == ScreenState::Permission
            || active.driver.startup_answer(&pane).is_some();
        let view = self.sessions.get(&claimed.session_id);
        let accepted = view.and_then(|view| view.accepted_at.get(&claimed.command_id));
        let moved = match (accepted, view.and_then(|view| view.last_effect)) {
            (Some(accepted), Some(effect)) => effect > *accepted,
            (Some(_), None) => false,
            // Not seen accepted: whether anything went since cannot be told.
            (None, _) => true,
        };
        shows && !moved
    }

    /// Text into the engine's input box, at most once, ending with the
    /// command's marker ([`crate::message`], recorded `message.typed`): the
    /// text is staged first (no key goes), the intent recorded before the keys go and the
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
        // The message's id, at the end of the whole text: the transcript
        // line that holds it is the message taken ([`crate::message`]). On
        // the line of a one-line text, whatever the pane reads: in a pane
        // that reads lines, a marker on a line of its own would make one
        // input four submissions at the paste (the text, a blank line, the
        // marker, the answer line), and an engine that acts on the first
        // line could exit
        // before the Enter that follows the paste (003, rev-e's
        // analysis-003-flake.md).
        let one_line = !text.contains('\n');
        let text = crate::message::marked(&text, claimed.command_id, one_line);
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
            // The text about to be typed carries the marker: the record that
            // tells this command's message from one an older Spawn typed
            // without it, which no `message.taken` can ever name.
            self.append(
                &claimed.agent_id,
                &claimed.session_id,
                Some(&active.execution_id),
                Source::Bridge,
                Some(format!("{}:message", claimed.command_id)),
                EventKind::MessageTyped {
                    command_id: claimed.command_id,
                },
            )?;
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
        prepared: &PreparedStart,
        prompt: &str,
        report: &mut TickReport,
    ) -> Result<Handled, StoreError> {
        let fresh = self.name_session(claimed, native, active)?;
        let launched = self.launch(
            claimed,
            active,
            &EngineLaunch {
                native: fresh.as_deref().or(native),
                resume: native.is_some(),
                relay: prepared.relay.as_ref(),
                profile: prepared.model,
                config_dir: prepared.config_dir.as_deref(),
                cwd: Some(active.workspace.path()),
                model: prepared.model_label.as_deref(),
            },
        );
        let launch = match launched {
            Ok(launch) => launch,
            Err(why) => {
                return self
                    .abandon(claimed, active, why, true)
                    .map(Handled::Finished);
            }
        };
        if let Err(error) = self.terminals.start(&launch) {
            return self
                .abandon(claimed, active, format!("start terminal: {error}"), true)
                .map(Handled::Finished);
        }
        self.record_terminal_server(claimed, active, report);
        if prompt.is_empty() {
            return Ok(Handled::Finished(Outcome::Succeeded));
        }
        self.prompt_engine(claimed, active, prompt)
    }

    /// Record the terminal server the execution's terminal just started on
    /// (`execution.terminal_started`): the server that runs it, told by its
    /// identity, which the recovery compares with the one that answers when
    /// the execution ends. Read after the start, which may be what started
    /// the server. A host that cannot tell its server records nothing; a
    /// server gone already, or a record that failed, is warned once, naming
    /// the execution: the recovery then reads the server's start against
    /// the execution's record.
    fn record_terminal_server(&self, claimed: &Claimed, active: &Active, report: &mut TickReport) {
        let server = match self.terminals.server() {
            ServerSeen::Answers(Some(server)) => server,
            ServerSeen::Answers(None) => return,
            ServerSeen::Gone => {
                report.warnings.push(format!(
                    "{}: {}: record its terminal server: no terminal server answers after its start",
                    claimed.session_id, active.execution_id
                ));
                return;
            }
        };
        if let Err(error) = self.append(
            &claimed.agent_id,
            &claimed.session_id,
            Some(&active.execution_id),
            Source::Bridge,
            Some(format!("{}:terminal_started", active.execution_id)),
            EventKind::TerminalStarted { server },
        ) {
            report.warnings.push(format!(
                "{}: {}: record its terminal server (pid {}, started at {} ms): {error}",
                claimed.session_id, active.execution_id, server.pid, server.started_at_ms
            ));
        }
    }

    /// Advance a started engine's prompt by at most one capture, answering
    /// its trust dialog first when it shows ([`Self::prompt_step`]). An
    /// engine that never got its prompt must not look alive: it is abandoned.
    fn prompt_engine(
        &mut self,
        claimed: &Claimed,
        active: &Active,
        prompt: &str,
    ) -> Result<Handled, StoreError> {
        match self.prompt_step(claimed, active, prompt)? {
            Handled::Finished(Outcome::Failed(why)) => {
                let discard = self.never_typed(claimed, active)?;
                self.abandon(claimed, active, why, discard)
                    .map(Handled::Finished)
            }
            Handled::Finished(outcome) => {
                self.catch_up()?;
                Ok(Handled::Finished(outcome))
            }
            waiting @ Handled::Waiting { .. } => Ok(waiting),
        }
    }

    /// Before a resume of `native`: its transcript is where the engine
    /// reads it, put back from the archive when it went missing or lost its
    /// head, or why the resume cannot go on ([`transcripts::restore`]).
    fn ready_transcript(
        &self,
        claimed: &Claimed,
        (driver, workspace): (&dyn AgentDriver, &WorkspaceId),
        native: &str,
        report: &mut TickReport,
    ) -> Result<(), String> {
        let path = self.transcript_of(driver, workspace, native);
        let read_before = self
            .sessions
            .get(&claimed.session_id)
            .is_some_and(|view| view.transcript_read);
        let had = self
            .archive_mask
            .as_deref()
            .map(crate::port::ArchiveMask::mask);
        let current = |mask: &Arc<crate::mask::Mask>| {
            self.archive_mask
                .as_deref()
                .is_some_and(|source| source.current(mask))
        };
        let (mask, why) = match &had {
            None => (None, None),
            Some(Ok(mask)) => (Some(mask.clone()), None),
            Some(Err(why)) => (None, Some(format!("the credential mask: {why}"))),
        };
        let still = || mask.as_ref().is_some_and(&current);
        let keeper = self
            .transcripts
            .as_deref()
            .map(|archive| transcripts::Keeper {
                archive,
                mask: match (&mask, &why) {
                    (_, Some(why)) => Err(why.as_str()),
                    (Some(mask), None) => Ok(Some(transcripts::Masking {
                        mask: mask.clone(),
                        current: &still,
                    })),
                    (None, None) => Ok(None),
                },
            });
        let restored = transcripts::restore(
            self.terminals.as_ref(),
            keeper,
            workspace,
            &path,
            &claimed.agent_id,
            native,
            read_before,
        )?;
        match restored {
            Some(transcripts::Restored::Written { bytes }) => report.warnings.push(format!(
                "{}: {path} was missing; restored {bytes} bytes from the transcript archive",
                claimed.session_id
            )),
            Some(transcripts::Restored::Replaced { bytes, had }) => report.warnings.push(format!(
                "{}: {path} held {had} bytes and the transcript archive more (it lost its head); \
                 restored {bytes} bytes, every byte it held among them",
                claimed.session_id
            )),
            Some(transcripts::Restored::Left { had }) => report.warnings.push(format!(
                "{}: {path} ({had} bytes) was not restored from the transcript archive: it is \
                 not the archive's latest copy of it, or a copy of it is masked (a present file \
                 is never replaced by masked bytes), or the credential mask could not be had to \
                 archive it first{}; it is resumed as it is, and the archive keeps every copy it \
                 made",
                claimed.session_id,
                why.as_deref()
                    .map(|why| format!(" ({why})"))
                    .unwrap_or_default()
            )),
            None => {}
        }
        Ok(())
    }

    /// Copy what the engine appended to its transcript into the archive,
    /// if there is one, and record where the archive stands
    /// ([`ArchiveProgress`]). A copy is made when the transcript was read
    /// growing on this pass, a copy failed before, or the execution is
    /// ending (`live` false); otherwise nothing was added and the
    /// transcript is only known unchanged until this pass. A failure is
    /// warned when it changes, and the copy is made again next pass; the
    /// record then says from when the archive lacks lines.
    fn keep_archive(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &mut Active,
        live: bool,
        report: &mut TickReport,
    ) {
        let native = self
            .sessions
            .get(session)
            .and_then(|view| view.native.clone());
        let (Some(archive), Some(native)) = (self.transcripts.as_deref(), native) else {
            return;
        };
        let path = self.transcript_of(active.driver, &active.workspace, &native);
        let state = &mut active.follow.archiving;
        if state.due || !live {
            let pass = self.copy_transcript(
                archive,
                agent_id,
                &native,
                &active.workspace,
                &path,
                (
                    &mut state.archived,
                    &mut state.behind_ms,
                    &mut state.known_ms,
                ),
            );
            self.record_archive_pass(
                agent_id,
                session,
                Some(&active.execution_id),
                &native,
                &pass,
                report,
            );
            let error = pass.error();
            state.due = error.is_some();
            if let Some(error) = &error
                && state.error.as_ref() != Some(error)
            {
                report
                    .warnings
                    .push(format!("{session}: archive the transcript: {error}"));
            }
            state.error = error;
        } else {
            // Nothing was added: read empty, or the watch told of no write.
            let known = self.pass_started.saturating_sub(ARCHIVE_SLACK_MS);
            state.known_ms = Some(state.known_ms.map_or(known, |was| was.max(known)));
        }
        let progress = ArchiveProgress {
            agent: agent_id.clone(),
            native,
            session: session.clone(),
            execution: active.execution_id.clone(),
            workspace: active.workspace.clone(),
            live,
            known_ms: state.known_ms.unwrap_or(i64::MIN),
            archived: state.archived,
            behind_ms: unknown_behind(state.behind_ms, state.known_ms, live),
        };
        let fresh = state.written.as_ref().is_some_and(|written| {
            ArchiveProgress {
                known_ms: written.known_ms,
                ..progress.clone()
            } == *written
                && progress.known_ms.saturating_sub(written.known_ms) < PROGRESS_REFRESH_MS
        });
        if fresh && live {
            return;
        }
        match archive.put_progress(&progress, !live) {
            Ok(()) => state.written = Some(progress.clone()),
            Err(error) => report.warnings.push(format!(
                "{session}: record the transcript archive's progress: {error}"
            )),
        }
        if !live && progress.behind_ms.is_some() {
            self.ended_archives
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    (agent_id.clone(), progress.native.clone()),
                    EndedArchive {
                        written: Some(progress.clone()),
                        progress,
                        error: state.error.clone(),
                    },
                );
        }
    }

    /// One copy of the transcript at `path` into the archive: on success the
    /// archive's length and nothing behind, known until the read; on
    /// failure the time of the first line the archive lacks, if the
    /// transcript can be read. A transcript gone while the archive lacks
    /// lines of it (`behind_ms` set) keeps `behind_ms`: nothing is behind
    /// only once every line was copied, never because the file went
    /// (objective 179 R3). What the pass did.
    fn copy_transcript(
        &self,
        archive: &dyn TranscriptArchive,
        agent_id: &AgentId,
        native: &str,
        workspace: &WorkspaceId,
        path: &str,
        (archived, behind_ms, known_ms): (&mut Option<u64>, &mut Option<i64>, &mut Option<i64>),
    ) -> Pass {
        let read_at = self.clock.now().0.saturating_sub(ARCHIVE_SLACK_MS);
        let known = |known_ms: &mut Option<i64>| {
            *known_ms = Some(known_ms.map_or(read_at, |was| was.max(read_at)));
        };
        // The mask first: without it nothing is copied (objective 175).
        let copied = match self.archive_mask.as_deref() {
            None => transcripts::archive_to(
                self.terminals.as_ref(),
                archive,
                None,
                workspace,
                path,
                agent_id,
                native,
            ),
            Some(source) => match source.mask() {
                Err(why) => Err(format!("the credential mask: {why}")),
                Ok(mask) => {
                    let current = || source.current(&mask);
                    let masking = transcripts::Masking {
                        mask: mask.clone(),
                        current: &current,
                    };
                    transcripts::archive_to(
                        self.terminals.as_ref(),
                        archive,
                        Some(&masking),
                        workspace,
                        path,
                        agent_id,
                        native,
                    )
                }
            },
        };
        match copied {
            Ok(done) if done.gone && behind_ms.is_some() => {
                known(known_ms);
                Pass::Gone
            }
            Ok(done) => {
                let moved = done
                    .offset
                    .map_or(0, |offset| offset.saturating_sub(archived.unwrap_or(0)));
                if done.offset.is_some() {
                    *archived = done.offset;
                }
                *behind_ms = None;
                known(known_ms);
                Pass::Copied(moved)
            }
            Err(error) => {
                // The first line it lacks stays the first while nothing more
                // is archived: read once, once it reads a time. "No time that
                // reads" (`i64::MIN`, which holds every window open) is read
                // again each pass: the line may have been half written (S33).
                if behind_ms.is_none_or(|at| at == i64::MIN) {
                    match transcripts::first_line_ms(
                        self.terminals.as_ref(),
                        workspace,
                        path,
                        archived.unwrap_or(0),
                    ) {
                        Ok(at) => {
                            *behind_ms = at;
                            known(known_ms);
                        }
                        Err(_) => return Pass::Failed(error),
                    }
                } else {
                    known(known_ms);
                }
                Pass::Failed(error)
            }
        }
    }

    /// Record a change of one transcript's archive state (objective 179),
    /// after each pass that tried a copy, on the session's own stream:
    /// - `archive.failed {native, reason}` when the pass failed for a reason
    ///   other than the one recorded last (cut to [`ARCHIVE_REASON_MAX`]);
    ///   a first [`transcripts::MASK_CHANGED`] records nothing (a rotation
    ///   between the mask and the read is copied next pass), a second in a
    ///   row does;
    /// - `archive.resumed {native, copied}` at the first pass that copied
    ///   after a recorded failure;
    /// - for a transcript gone with lines the archive lacks ([`Pass::Gone`]),
    ///   never a resume: a recorded failure, or one still waiting, stands;
    ///   with neither, the loss is recorded once ([`ARCHIVE_GONE`]);
    /// - nothing while the state is what the log holds.
    ///
    /// A change gets its key (`archive:<native>:<ms>:<n>`) when it is first
    /// tried and keeps it until the log takes it: a failed append is warned
    /// and tried again next pass under the same key, so a log that took it
    /// and lost its answer stores it once; a newer change replaces it. What
    /// the log holds is also read back as it is folded
    /// ([`Self::fold_archive_event`]), so a restarted Bridge neither says a
    /// failure again nor leaves one ended while it was down unended.
    fn record_archive_pass(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        execution: Option<&ExecutionId>,
        native: &str,
        pass: &Pass,
        report: &mut TickReport,
    ) {
        let mut events = self
            .archive_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = events
            .entry((agent_id.clone(), native.to_string()))
            .or_default();
        if let ArchiveWanted::State(wanted) = wanted_archive_state(entry, pass) {
            if wanted == entry.failed {
                entry.pending = None;
            } else if entry
                .pending
                .as_ref()
                .is_none_or(|pending| pending.failed != wanted)
            {
                let n = self
                    .archive_keys
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let kind = match (&wanted, pass) {
                    (Some(reason), _) => EventKind::ArchiveFailed {
                        native: native.to_string(),
                        reason: reason.clone(),
                    },
                    (None, Pass::Copied(copied)) => EventKind::ArchiveResumed {
                        native: native.to_string(),
                        copied: *copied,
                    },
                    // Only a copy wants the state resumed.
                    (None, _) => return,
                };
                entry.pending = Some(PendingArchiveEvent {
                    key: format!("archive:{native}:{}:{n}", self.clock.now().0),
                    failed: wanted,
                    kind,
                });
            }
        }
        if let Some(pending) = entry.pending.clone() {
            match self.append(
                agent_id,
                session,
                execution,
                Source::Bridge,
                Some(pending.key),
                pending.kind,
            ) {
                Ok(()) => {
                    entry.failed = pending.failed;
                    entry.pending = None;
                }
                Err(error) => report.warnings.push(format!(
                    "{session}: record the transcript archive's state: {error}"
                )),
            }
        }
    }

    /// What the log holds of a transcript's archive state, read as it is
    /// folded (objective 179 R2): its last `archive.failed` stands until an
    /// `archive.resumed` of it. The keys' `<n>` go on past every one the log
    /// holds, so a restarted Bridge whose clock reads the same millisecond
    /// never makes a key again.
    fn fold_archive_event(&self, event: &NewEvent) {
        let (native, failed) = match &event.kind {
            EventKind::ArchiveFailed { native, reason } => (native, Some(reason.clone())),
            EventKind::ArchiveResumed { native, .. } => (native, None),
            _ => return,
        };
        if let Some(n) = event
            .source_event_id
            .as_deref()
            .and_then(|key| key.rsplit(':').next())
            .and_then(|n| n.parse::<u64>().ok())
        {
            self.archive_keys
                .fetch_max(n.saturating_add(1), std::sync::atomic::Ordering::Relaxed);
        }
        self.archive_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry((event.agent_id.clone(), native.clone()))
            .or_default()
            .failed = failed;
    }

    /// The ended executions whose transcripts the archive may lack, copied
    /// again: each copy made is recorded (its record says it ended, nothing
    /// behind) and the execution leaves the list; one that fails is warned
    /// when its failure changes and stays for the next pass. A native
    /// session a live execution runs again is that execution's to copy.
    fn archive_ended(&self, report: &mut TickReport) {
        let Some(archive) = self.transcripts.as_deref() else {
            return;
        };
        let ended: Vec<((AgentId, String), EndedArchive)> = self
            .ended_archives
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        for (key, mut entry) in ended {
            let live = self.sessions.values().any(|view| {
                view.active.is_some()
                    && view.agent_id == key.0
                    && view.native.as_deref() == Some(key.1.as_str())
            });
            let progress = &mut entry.progress;
            let done = if live {
                true
            } else {
                let path = self.transcript_of(
                    self.session_driver(&progress.session),
                    &progress.workspace,
                    &progress.native,
                );
                let mut known = (progress.known_ms != i64::MIN).then_some(progress.known_ms);
                let pass = self.copy_transcript(
                    archive,
                    &progress.agent,
                    &progress.native,
                    &progress.workspace,
                    &path,
                    (&mut progress.archived, &mut progress.behind_ms, &mut known),
                );
                self.record_archive_pass(
                    &progress.agent,
                    &progress.session,
                    Some(&progress.execution),
                    &progress.native,
                    &pass,
                    report,
                );
                let error = pass.error();
                if let Some(error) = &error
                    && entry.error.as_ref() != Some(error)
                {
                    report.warnings.push(format!(
                        "{}: archive the transcript of ended {}: {error}",
                        progress.session, progress.execution
                    ));
                }
                entry.error = error;
                progress.live = false;
                progress.known_ms = known.unwrap_or(i64::MIN);
                progress.behind_ms = unknown_behind(progress.behind_ms, known, false);
                // A copy that keeps failing writes its record once.
                let unchanged = entry.written.as_ref().is_some_and(|written| {
                    ArchiveProgress {
                        known_ms: written.known_ms,
                        ..progress.clone()
                    } == *written
                });
                let put = if unchanged {
                    Ok(())
                } else {
                    archive.put_progress(progress, true)
                };
                match &put {
                    Ok(()) => entry.written = Some(progress.clone()),
                    Err(put) => report.warnings.push(format!(
                        "{}: record the transcript archive's progress: {put}",
                        progress.session
                    )),
                }
                put.is_ok() && progress.behind_ms.is_none()
            };
            let mut list = self
                .ended_archives
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if done {
                list.remove(&key);
            } else {
                list.insert(key, entry);
            }
        }
    }

    /// After a restart, where the archive stood is read back from its
    /// progress records: a live execution's copy goes on from its length,
    /// and an ended one whose record is not settled (it says the archive
    /// lacks lines, or that it was live, its end unrecorded there) is
    /// copied again until the copy is made ([`Self::archive_ended`]).
    fn recover_archives(&mut self) {
        let mut list = self
            .ended_archives
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // What folding the whole log put there is every end ever: the
        // records say which of them are not settled.
        list.clear();
        let Some(archive) = self.transcripts.as_deref() else {
            return;
        };
        let records = match archive.progress() {
            Ok(records) => records,
            Err(error) => {
                eprintln!("spawn: the transcript archive's progress: {error}");
                return;
            }
        };
        for record in records {
            let running = self.sessions.values_mut().find_map(|view| {
                (view.agent_id == record.agent
                    && view.native.as_deref() == Some(record.native.as_str()))
                .then_some(view.active.as_mut())
                .flatten()
            });
            if let Some(active) = running {
                active.follow.archiving.archived = record.archived;
                continue;
            }
            if record.live || record.behind_ms.is_some() {
                list.insert(
                    (record.agent.clone(), record.native.clone()),
                    EndedArchive {
                        written: Some(record.clone()),
                        progress: record,
                        error: None,
                    },
                );
            }
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
        (driver, workspace): (&dyn AgentDriver, &WorkspaceId),
        native: &str,
        report: &mut TickReport,
    ) -> Result<Option<u64>, String> {
        self.ready_transcript(claimed, (driver, workspace), native, report)?;
        self.transcript_length(driver, workspace, native)
    }

    /// How long the transcript of `native` is, as the engine will see it;
    /// `None` when it does not exist (yet).
    fn transcript_length(
        &self,
        driver: &dyn AgentDriver,
        workspace: &WorkspaceId,
        native: &str,
    ) -> Result<Option<u64>, String> {
        let path = self.transcript_of(driver, workspace, native);
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
        let generation = self.recorded_steps(claimed).mounts.max(1);
        match self.append(
            &claimed.agent_id,
            &claimed.session_id,
            None,
            Source::Bridge,
            Some(mount_released_key(claimed.command_id, generation)),
            EventKind::CommandStepped {
                command_id: claimed.command_id,
                step: CommandStep::MountReleased,
                workspace: Some(workspace.clone()),
            },
        ) {
            Ok(()) => Outcome::Failed(why),
            Err(error) => Outcome::Failed(format!("{why}; record the unmount: {error}")),
        }
    }

    /// What an engine on `model` launches with: the relay it goes through
    /// and its configuration directory, which its driver makes its settings
    /// of ([`AgentDriver::launch`], Claude Code's inline settings), with
    /// the summary for the record ([`LaunchSummary`]), or why it cannot
    /// start. The default profile falls back on Claude Code's own
    /// settings (the gateway, its own small model) when the relay is down,
    /// and its summary names no relay; the cheap profile has no fallback,
    /// and fails where its sender sees it.
    fn engine_settings(
        &self,
        claimed: &Claimed,
        model: ModelProfile,
        report: &mut TickReport,
    ) -> Result<(Option<ModelRelay>, Option<String>, LaunchSummary), String> {
        let relay = self.config.model_relay.as_ref();
        let reachable = relay.filter(|relay| relay.reachable());
        let config_dir = self.claude_config_dir(&claimed.session_id);
        let settings = |relay: Option<&ModelRelay>| {
            let summary = LaunchSummary {
                relay_url: relay.and_then(|relay| LaunchSummary::relay_origin(&relay.url)),
                model: relay
                    .and_then(|relay| relay.main_model(model))
                    .map(str::to_string),
                small_model: relay.map(|relay| relay.small_model.clone()),
                config_dir: config_dir.clone(),
            };
            (relay.cloned(), config_dir.clone(), summary)
        };
        match (model, relay, reachable) {
            (_, _, Some(relay)) => Ok(settings(Some(relay))),
            (ModelProfile::Default, Some(relay), None) => {
                report.warnings.push(format!(
                    "{}: model relay {} unreachable; the default profile runs on Claude Code's \
                     own settings",
                    claimed.session_id,
                    LaunchSummary::relay_origin(&relay.url)
                        .unwrap_or_else(|| "(address withheld)".to_string())
                ));
                Ok(settings(None))
            }
            (ModelProfile::Default, None, None) => Ok(settings(None)),
            (ModelProfile::Cheap, Some(relay), None) => Err(format!(
                "the cheap profile needs the model relay, and {} is unreachable",
                LaunchSummary::relay_origin(&relay.url)
                    .unwrap_or_else(|| "(address withheld)".to_string())
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
                held.follow.end_error.clone_from(&ending.follow.end_error);
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
        let Some(id) = active.driver.new_session_id(&active.execution_id) else {
            return Ok(None);
        };
        let home = self.config.home_of(active.driver);
        self.append(
            &claimed.agent_id,
            &claimed.session_id,
            Some(&active.execution_id),
            Source::Bridge,
            Some(format!("{}:native", active.execution_id)),
            EventKind::NativeSessionStarted {
                engine: active.driver.id().to_string(),
                native_session_id: id.clone(),
                transcript_path: Some(active.driver.transcript_path(&home, &active.workspace, &id)),
                engine_version: None,
            },
        )?;
        self.catch_up()?;
        Ok(Some(id))
    }

    /// Where `driver`'s engine writes the transcript of `native` when it
    /// runs in `workspace`, from its home ([`BridgeConfig::engine_home`]).
    fn transcript_of(
        &self,
        driver: &dyn AgentDriver,
        workspace: &WorkspaceId,
        native: &str,
    ) -> String {
        driver.transcript_path(&self.config.home_of(driver), workspace, native)
    }

    /// The driver of the engine `session` last started
    /// ([`driver_or_default`]: Claude Code's for a session that never ran
    /// one, or ran one no driver is registered for).
    fn session_driver(&self, session: &SessionId) -> &'static dyn AgentDriver {
        driver_or_default(
            self.sessions
                .get(session)
                .map_or(crate::driver::DEFAULT, |view| view.engine.as_str()),
        )
    }

    /// The Claude Code directory the engine of `session` runs with
    /// (`CLAUDE_CONFIG_DIR` in the environment it gets, issue #79), when it
    /// is not the user's own `~/.claude`.
    fn claude_config_dir(&self, session: &SessionId) -> Option<String> {
        let overrides = self
            .config
            .session_environment
            .iter()
            .filter(|(known, _)| known == session.as_str())
            .flat_map(|(_, overrides)| overrides.iter());
        self.config
            .environment
            .iter()
            .chain(overrides)
            .filter(|(name, _)| name == "CLAUDE_CONFIG_DIR")
            .map(|(_, dir)| dir.clone())
            .next_back()
            .filter(|dir| !dir.is_empty())
    }

    /// The terminal launch of `active`'s engine: the runner (`spawn run`)
    /// of the engine's program, with the environment of the host, the
    /// session and the engine. An engine with no deny rules of its own
    /// ([`crate::driver::Guard::Sandbox`]) runs inside the host's
    /// filesystem view ([`BridgeConfig::engine_view`]): the launch's
    /// program is the view's (`bwrap`), and the runner is what it runs.
    /// Why the view cannot be made is the error, and nothing starts.
    fn launch(
        &self,
        claimed: &Claimed,
        active: &Active,
        run: &EngineLaunch<'_>,
    ) -> Result<Launch, String> {
        let home = self.config.home_of(active.driver);
        let plan = active.driver.launch(&home, run);
        let mut arguments = vec!["run".to_string(), "--".to_string(), plan.program];
        arguments.extend(plan.arguments);
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
        // What the engine needs of its own (none for Claude Code).
        for (name, value) in plan.environment {
            environment.retain(|(known, _)| *known != name);
            environment.push((name, value));
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
        // Where the agent's answers to its messages go (`spawn ack`): its
        // workspace, which the loop reads, whatever directory its shell is
        // in. No login either.
        environment.push((
            "SPAWN_WORKSPACE".to_string(),
            active.workspace.path().to_string(),
        ));
        let mut program = self.config.spawn_program.clone();
        if active.driver.traits().guard == crate::driver::Guard::Sandbox {
            match &self.config.engine_view {
                EngineViewPolicy::Off => {}
                EngineViewPolicy::Required(Err(why)) => {
                    return Err(format!(
                        "{ENGINE_UNAVAILABLE}: engine {} runs only inside a filesystem view, \
                         which this host has none of: {why}",
                        active.driver.id()
                    ));
                }
                EngineViewPolicy::Required(Ok(view)) => {
                    let words = view
                        .wrap(&ViewedLaunch {
                            engine: active.driver.id(),
                            workspace: active.workspace.path(),
                            engine_home: &home.sessions,
                        })
                        .map_err(|why| {
                            format!(
                                "{ENGINE_UNAVAILABLE}: engine {}: its filesystem view ({}) \
                                 cannot be made: {why}",
                                active.driver.id(),
                                view.name()
                            )
                        })?;
                    let Some((view_program, view_arguments)) = words.split_first() else {
                        return Err(format!(
                            "{ENGINE_UNAVAILABLE}: engine {}: its filesystem view ({}) gave \
                             no program",
                            active.driver.id(),
                            view.name()
                        ));
                    };
                    let mut wrapped = view_arguments.to_vec();
                    wrapped.push(std::mem::take(&mut program));
                    wrapped.extend(arguments);
                    arguments = wrapped;
                    program.clone_from(view_program);
                }
            }
        }
        Ok(Launch {
            terminal: active.terminal(),
            workspace: active.workspace.clone(),
            cwd: active.workspace.path().to_string(),
            program,
            arguments,
            environment,
        })
    }

    /// Advance the wait for the engine's input box by one capture, then type
    /// `prompt` if it is ready. Progress is retained between claims; no
    /// `waiting` step is written, so this Start or Resume still blocks later
    /// commands on its session.
    ///
    /// The Bridge exported the workspace itself, for a fresh session and a
    /// resume alike, so it answers the engine's trust dialog in both: trust
    /// lives in the engine's own config (`~/.claude.json`), which can forget
    /// a workspace its session ran in while the transcript survives.
    fn type_prompt(
        &mut self,
        claimed: &Claimed,
        active: &Active,
        prompt: &str,
    ) -> Result<Handled, StoreError> {
        let mut progress = self
            .prompting
            .remove(&claimed.command_id)
            .unwrap_or_default();
        let result = (|| {
            if progress.attempts >= self.config.ready_attempts {
                return Ok(self.prompt_retry(&progress));
            }
            // The trust dialog comes before the engine starts up, so answering
            // it restarts the budget instead of spending it. A failed capture,
            // or staging refused before any key went, spends one attempt: only
            // a terminal that is gone ends the wait early.
            progress.attempts += 1;
            let pane =
                match self
                    .terminals
                    .capture_tail(&active.terminal(), &active.workspace, TAIL_LINES)
                {
                    Ok(pane) => {
                        progress.last_pane = Some(active.driver.excerpt_source(&pane));
                        pane
                    }
                    Err(error) => {
                        if self.terminal_gone(active) {
                            return Ok(Handled::Finished(Outcome::Failed(format!(
                                "terminal disappeared: {error}"
                            ))));
                        }
                        progress.transient = Some(format!("capture: {error}"));
                        return Ok(self.prompt_retry(&progress));
                    }
                };
            // The engine says it will not run what it was started on (agy's
            // unknown model): the start fails now, never waits on it.
            if let Some(why) = active.driver.start_refusal(&pane) {
                return Ok(Handled::Finished(Outcome::Failed(why)));
            }
            let reading = active.driver.read_screen(&pane);
            progress.prompt_seen |= reading.state != ScreenState::Unknown;
            // The Bridge exported this directory itself, so it trusts it,
            // whether the session is fresh or resumed. Answer once (a second
            // answer would land in the input box), and only the dialog itself:
            // on two equal reads, before any prompt.
            if !self.config.auto_trust
                && !progress.prompt_seen
                && active.driver.startup_answer(&pane).is_some()
            {
                // SPAWN_AUTO_APPROVE=off: nothing answers it but a person.
                progress.transient = Some(TRUST_OFF.to_string());
                return Ok(self.prompt_retry(&progress));
            }
            if !progress.answered_trust
                && !progress.prompt_seen
                && let Some(keys) = active.driver.startup_answer(&pane)
            {
                if progress.dialog.as_deref() != Some(pane.as_str()) {
                    progress.dialog = Some(pane);
                    return Ok(self.prompt_retry(&progress));
                }
                progress.answered_trust = true;
                if let Err(error) = self.answer_trust(claimed, active, keys)? {
                    return Ok(Handled::Finished(Outcome::Failed(format!(
                        "answer trust dialog: {error}"
                    ))));
                }
                progress.attempts = 1;
                return Ok(self.prompt_retry(&progress));
            }
            progress.dialog = None;
            progress.last_state = reading.state;
            if !reading.state.accepts_input() {
                return Ok(self.prompt_retry(&progress));
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
                Ok(()) => Ok(Handled::Finished(Outcome::Succeeded)),
                Err(Untyped::Refused(error)) => {
                    progress.transient =
                        Some(format!("submit prompt refused before typing: {error}"));
                    Ok(self.prompt_retry(&progress))
                }
                Err(Untyped::Unknown(error)) => Ok(Handled::Finished(Outcome::Failed(format!(
                    "submit prompt: {error}; the prompt {MAY_HAVE_TYPED}"
                )))),
            }
        })();
        match result {
            Ok(waiting @ Handled::Waiting { .. }) => {
                self.prompting.insert(claimed.command_id, progress);
                Ok(waiting)
            }
            Err(error) => {
                // A lost lease can reclaim the command. Keep the screen
                // progress, especially the trust answer already sent.
                self.prompting.insert(claimed.command_id, progress);
                Err(error)
            }
            finished => finished,
        }
    }

    /// The startup trust dialog's answer, audited: each key is a `key`
    /// command of [`RESOLVE_PRINCIPAL`] (keyed [`trust_answer_key`]),
    /// submitted, recorded `pressing`, pressed with the others in one send
    /// (as before: the dialog reads keys, and they go together), and
    /// recorded `pressed`. The commands queue behind this start, so the
    /// Bridge claims them once the start is done, finds them pressed and
    /// finishes them without pressing anything again. A key recorded
    /// `pressing` or `pressed` already (a restarted Bridge on the same
    /// dialog) presses nothing. A key Spawn has no command for (none of
    /// today's) is pressed unaudited, as before.
    fn answer_trust(
        &self,
        claimed: &Claimed,
        active: &Active,
        keys: &[&str],
    ) -> Result<Result<(), TerminalError>, StoreError> {
        let named: Option<Vec<NamedKey>> = keys
            .iter()
            .map(|key| {
                NamedKey::ALL
                    .iter()
                    .copied()
                    .find(|named| named.tmux() == *key)
            })
            .collect();
        let Some(named) = named else {
            return Ok(self
                .terminals
                .send_keys(&active.terminal(), &active.workspace, keys));
        };
        let principal = Principal::new(RESOLVE_PRINCIPAL)
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let mut audited = Vec::with_capacity(named.len());
        for (at, key) in named.iter().enumerate() {
            let idempotency_key =
                IdempotencyKey::new(trust_answer_key(&active.execution_id, at + 1))
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
            let command_id = self.queue.submit(
                &principal,
                &Submission {
                    agent_id: claimed.agent_id.clone(),
                    session_id: claimed.session_id.clone(),
                    idempotency_key,
                    action: CommandAction::Key { key: *key },
                },
            )?;
            audited.push(Claimed {
                command_id,
                attempt: 0,
                agent_id: claimed.agent_id.clone(),
                session_id: claimed.session_id.clone(),
                principal: RESOLVE_PRINCIPAL.to_string(),
                action: serde_json::Value::Null,
            });
        }
        if audited
            .iter()
            .any(|key| self.recorded_steps(key).press != Press::Due)
        {
            return Ok(Ok(()));
        }
        for key in &audited {
            self.step(key, Some(&active.execution_id), CommandStep::Pressing, None)?;
        }
        let sent = self
            .terminals
            .send_keys(&active.terminal(), &active.workspace, keys);
        if sent.is_ok() {
            for key in &audited {
                self.step(key, Some(&active.execution_id), CommandStep::Pressed, None)?;
            }
        }
        Ok(sent)
    }

    /// The resumable prompt entry point: one non-blocking readiness step per
    /// claim, with progress kept by `type_prompt` between calls.
    fn prompt_step(
        &mut self,
        claimed: &Claimed,
        active: &Active,
        prompt: &str,
    ) -> Result<Handled, StoreError> {
        self.type_prompt(claimed, active, prompt)
    }

    /// Either retain another prompt attempt or produce the wait's established
    /// detailed final failure after the last one.
    fn prompt_retry(&self, progress: &PromptProgress) -> Handled {
        if progress.attempts < self.config.ready_attempts {
            let delay = match progress.attempts {
                0..=1 => Duration::from_millis(25),
                2 => Duration::from_millis(50),
                3 => Duration::from_millis(100),
                _ => self.config.ready_interval,
            };
            return Handled::Waiting {
                retry: self.config.ready_interval.min(delay),
            };
        }
        let why = progress
            .transient
            .as_deref()
            .map_or_else(String::new, |why| format!("; last failure {why}"));
        let seen = progress
            .last_pane
            .as_deref()
            .and_then(format_pane_tail_excerpt)
            .map_or_else(String::new, |excerpt| format!("; last pane: {excerpt}"));
        Handled::Finished(Outcome::Failed(format!(
            "pane never became ready, last state {:?}{why}{seen}",
            progress.last_state
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
            let reading = active.driver.read_screen(&pane);
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
        Ok(Handled::Waiting {
            retry: self.config.input_wait_retry,
        })
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
            held.follow.end_error.clone_from(&active.follow.end_error);
            held.follow.killing.clone_from(&active.follow.killing);
            held.follow.evidence = active.follow.evidence;
        }
        if active.follow.killing.is_some() {
            // Its kill still waits for the engine's processes, off the loop:
            // the stop waits, claimed again after `input_wait_retry`, and
            // succeeds once the end that the ticks go on with is done.
            tick.warnings.append(&mut report.warnings);
            return Ok(Handled::Waiting {
                retry: self.config.input_wait_retry,
            });
        }
        if report.warnings.is_empty() {
            // Nothing failed in this attempt: the end waits on a failure an
            // earlier one met (the kill's, read by a tick's pass over the
            // session, or the checkpoint's), and says which.
            report
                .warnings
                .push(if active.follow.kill_failed_at.is_some() {
                    format!(
                        "{}: the end waits: {}; the kill is tried again within {} s",
                        claimed.session_id,
                        active
                            .follow
                            .end_error
                            .as_deref()
                            .unwrap_or("the runner outlived its kill"),
                        END_RETRY_MS / 1000
                    )
                } else {
                    format!(
                        "{}: the end is stalled and its final checkpoint retried every {} s{}; a \
                     discard command gives the uncommitted work up",
                        claimed.session_id,
                        END_RETRY_MS / 1000,
                        active
                            .follow
                            .end_error
                            .as_deref()
                            .map(|error| format!(" ({error})"))
                            .unwrap_or_default()
                    )
                });
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
    ) -> Result<Handled, StoreError> {
        // A retried resume whose execution started goes on in `start`.
        let ours = ExecutionId::for_command(claimed.command_id);
        if self
            .running(&claimed.session_id)
            .is_some_and(|active| active.execution_id != ours)
        {
            return Ok(Handled::Finished(Outcome::Failed(format!(
                "{} still runs an execution",
                claimed.session_id
            ))));
        }
        let Some((native, engine, label)) =
            self.sessions.get(&claimed.session_id).and_then(|view| {
                view.native
                    .clone()
                    .map(|native| (native, view.engine.clone(), view.model_label.clone()))
            })
        else {
            return Ok(Handled::Finished(Outcome::Failed(
                "no native session to resume".to_string(),
            )));
        };
        self.start(
            claimed,
            &Begin {
                prompt: prompt.unwrap_or(""),
                parent: None,
                native: Some(native.as_str()),
                from: None,
                model,
                // A resume runs the model label its session last started on
                // (its `execution.started`), the host's only when that
                // recorded none.
                model_label: label.as_deref(),
                driver: driver_or_default(&engine),
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
/// waits for its turn ([`CommandStep::Waiting`]). The resolver
/// ([`RESOLVE_PRINCIPAL`]) submits only `key` commands, which never wait in
/// any case: listing it changes no input rule.
pub const LOOP_PRINCIPALS: [&str; 4] = [
    "spawn-dispatch",
    "spawn-integrate",
    "spawn-liveness",
    RESOLVE_PRINCIPAL,
];

/// The principal of the automatic answers to an engine's dialogs: the
/// run-time resolver of `spawn loop` (`spawn-cli`'s `resolve.rs`) and the
/// Bridge's own answer to a startup trust dialog, each a `key` command on
/// the record (design section 6.3, "What the audit shows").
pub const RESOLVE_PRINCIPAL: &str = "spawn-resolve";

/// Why a start's wait for its prompt did not end under
/// `SPAWN_AUTO_APPROVE=off` ([`BridgeConfig::auto_trust`]).
pub const TRUST_OFF: &str = "the engine shows its startup trust dialog and SPAWN_AUTO_APPROVE=off: \
     nothing answers it automatically. A key from the agent page or /api/approvals waits behind \
     this start and cannot reach the dialog: answer it in the engine's terminal itself, or set \
     SPAWN_AUTO_APPROVE=startup and resume the session";

/// How a resolver's key ends when the dialog it answered is gone, or a
/// person (or another command) acted on the pane since it was submitted:
/// nothing pressed, no `pressing` recorded ([`Bridge::press`]).
pub const GONE_DIALOG: &str = "the dialog this answer was for is gone: nothing pressed";

/// The idempotency key of the `n`-th key (from 1) of the Bridge's answer
/// to the startup trust dialog of `execution`: `resolve:<execution>:trust:1:<n>`,
/// the resolver's key shape with the dialog named `trust`. A restarted
/// Bridge that sees the dialog again submits the same keys, finds them
/// pressed, and presses nothing twice.
pub fn trust_answer_key(execution: &ExecutionId, n: usize) -> String {
    format!("resolve:{execution}:trust:1:{n}")
}

/// Whether an input from `principal` waits for its turn instead of being
/// refused while the pane cannot take it.
pub fn waits(principal: &str) -> bool {
    !LOOP_PRINCIPALS.contains(&principal)
}

/// A `screen.observed` of the running execution, read back after a
/// restart: the last recorded screen, and the host reading the newest keyed
/// screen was keyed by (carry list N12). A restarted Bridge never keys that
/// reading again, so a screen recorded unkeyed under it before the restart
/// (a draft over a ready pane, the same `seq`) is followed by one recorded
/// unkeyed after it, not by a keyed one the log refuses as a repeat. The
/// Bridge's own readings keep its own clock, which the refresh is timed by.
fn fold_screen(active: &mut Active, state: ScreenState, record: &EventRecord) {
    if active.follow.screen.map(|(known, _)| known) != Some(state) {
        active.follow.screen = Some((state, record.recorded_at.0));
    }
    if let Some(seq) = record
        .event
        .source_event_id
        .as_deref()
        .and_then(|key| keyed_screen_seq(key, &active.execution_id))
    {
        active.follow.keyed_seq = Some(seq);
    }
}

/// The host reading's `seq` a keyed `screen.observed` of `execution` was
/// recorded under: its key is `<execution>:hs:<server pid>.<server start>:<seq>`
/// ([`Bridge::follow_screen`]).
fn keyed_screen_seq(key: &str, execution: &ExecutionId) -> Option<u64> {
    let rest = key.strip_prefix(execution.as_str())?.strip_prefix(":hs:")?;
    let (_, seq) = rest.rsplit_once(':')?;
    seq.parse().ok()
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
///   API keys), `~/.config/cloudflare`, `~/.hermes`, `~/.claude` (to
///   Bash all of it; to the Read and Edit tools its sensitive part,
///   [`CLAUDE_DIR_SENSITIVE`]) and `~/.claude.json*`, `~/.ssh`,
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
/// - The accidental prints of a password (url-leak-sources.md): a URL
///   that holds one anywhere in a command (`*://*:*@*`), the whole
///   environment printed (`env`, `printenv`, `export -p`, `declare -x`,
///   `set` and their kin, alone or behind a pipe, `;` or `&&`), process
///   listings with their arguments (`ps -ef`, `ps aux`, `ps -o args`,
///   `pgrep -a`/`-l`, `top -c`, `pstree -a`, `/proc/<pid>/cmdline`), and
///   Claude Code's settings by any path (`cd ~/.claude`). Not `echo $X`,
///   `env -u X` with no command or an interpreter that prints the
///   environment: a glob cannot tell them from what must run, and an
///   engine's environment with no password closes those at the source.
///   The task's prompt says what to do instead (deliver's credentials
///   line): a deny rule carries no message of its own.
///
/// The rest of `~/.claude` is open to the Read, Edit, Grep and Glob tools
/// (issue #78): `memory/`, a project's `memory/` and `workflows/` (workflow
/// transcripts), `plugins/`, `skills/`, `themes/`, `cache/`,
/// `statusline.py` and `CLAUDE.md`. Bash stays closed on every path that
/// names `.claude/`, `/.claude` or `cd .claude`: a glob (`~/.claude/*.json`,
/// `s?ttings.json`), a quote (`se''ttings.json`), an escape or a `cd` then
/// `..` spells any file in it, and a narrower Bash rule cannot tell them
/// apart. Text that names `.claude` with no slash before or after it
/// (`grep -rn "\.claude" crates/`, `git commit -m "fix .claude"`) runs.
///
/// Claude Code's own files under `~/.claude` are its internal writes, not
/// tool calls: a session, its transcript and a resume work with the
/// sensitive part denied (checked, below, with all of it denied).
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
///
/// Keep's delivery folders (`/run/keep`, and `/run/keep-test` for its test
/// deliveries) hold credentials as `~/.config/spawn` does (issue #98), so
/// they are denied the same way, and as there, the files it delivers are
/// denied by name too: a command that spells the folder another way
/// (`cd /run && less keep/typesafe.api-key`, `/run//keep/...`) still names
/// the file. The folder rule names `run/keep`, which the checkout's
/// `scripts/keep/` does not.
pub const CREDENTIAL_DENY: &[&str] = &[
    "Read(~/.local/share/spawn-rsi/*.env*)",
    "Read(~/.local/share/spawn-rsi/env.sh)",
    "Read(~/.local/share/spawn-rsi/backups/**)",
    "Read(~/.local/share/spawn-rsi/fleet-pg/**)",
    "Read(~/.config/spawn/**)",
    "Read(~/.config/cloudflare/**)",
    "Read(~/.hermes/**)",
    "Read(~/.claude/settings*)",
    "Read(~/.claude/.credentials*)",
    "Read(~/.claude/**/*credential*)",
    "Read(~/.claude/.claude.json*)",
    "Read(~/.claude/backups/**)",
    "Read(~/.claude/sessions/**)",
    "Read(~/.claude/session-env/**)",
    "Read(~/.claude/ssh/**)",
    "Read(~/.claude/history.jsonl*)",
    "Read(~/.claude/mcp-needs-auth-cache.json*)",
    "Read(~/.claude/ide/**)",
    "Read(~/.claude/shell-snapshots/**)",
    "Read(~/.claude/debug/**)",
    "Read(~/.claude/file-history/**)",
    "Read(~/.claude/paste-cache/**)",
    "Read(~/.claude/dev-mods/**)",
    "Read(~/.claude/seed-admin/**)",
    "Read(~/.claude/projects/*/*.jsonl*)",
    "Read(~/.claude/projects/*/*/subagents/**)",
    "Read(~/.claude/projects/*/*/tool-results/**)",
    "Read(~/.claude/**/*.key)",
    "Read(~/.claude/**/*.pem)",
    "Read(~/.claude/**/*.env*)",
    "Read(~/.claude.json*)",
    "Read(~/.ssh/**)",
    "Read(~/.gnupg/**)",
    "Read(//run/keep/**)",
    "Read(//run/keep-test/**)",
    "Read(//data/tmp/ccd/**)",
    "Read(//data/tmp/ccd-pg/**)",
    "Read(//var/tmp/spawn/**/*.sh)",
    "Read(~/.pgpass)",
    "Edit(~/.local/share/spawn-rsi/*.env*)",
    "Edit(~/.local/share/spawn-rsi/env.sh)",
    "Edit(~/.local/share/spawn-rsi/backups/**)",
    "Edit(~/.local/share/spawn-rsi/fleet-pg/**)",
    "Edit(~/.config/spawn/**)",
    "Edit(~/.config/cloudflare/**)",
    "Edit(~/.hermes/**)",
    "Edit(~/.claude/settings*)",
    "Edit(~/.claude/.credentials*)",
    "Edit(~/.claude/**/*credential*)",
    "Edit(~/.claude/.claude.json*)",
    "Edit(~/.claude/backups/**)",
    "Edit(~/.claude/sessions/**)",
    "Edit(~/.claude/session-env/**)",
    "Edit(~/.claude/ssh/**)",
    "Edit(~/.claude/history.jsonl*)",
    "Edit(~/.claude/mcp-needs-auth-cache.json*)",
    "Edit(~/.claude/ide/**)",
    "Edit(~/.claude/shell-snapshots/**)",
    "Edit(~/.claude/debug/**)",
    "Edit(~/.claude/file-history/**)",
    "Edit(~/.claude/paste-cache/**)",
    "Edit(~/.claude/dev-mods/**)",
    "Edit(~/.claude/seed-admin/**)",
    "Edit(~/.claude/projects/*/*.jsonl*)",
    "Edit(~/.claude/projects/*/*/subagents/**)",
    "Edit(~/.claude/projects/*/*/tool-results/**)",
    "Edit(~/.claude/**/*.key)",
    "Edit(~/.claude/**/*.pem)",
    "Edit(~/.claude/**/*.env*)",
    "Edit(~/.claude.json*)",
    "Edit(~/.ssh/**)",
    "Edit(~/.gnupg/**)",
    "Edit(//run/keep/**)",
    "Edit(//run/keep-test/**)",
    "Edit(//data/tmp/ccd/**)",
    "Edit(//data/tmp/ccd-pg/**)",
    "Edit(//var/tmp/spawn/**/*.sh)",
    "Edit(~/.pgpass)",
    // The workspaces live under spawn-rsi too, so these name files, not
    // the directory.
    "Bash(*spawn.env*)",
    "Bash(*spawn-rsi/env.sh*)",
    "Bash(*spawn-rsi/backups*)",
    "Bash(*fleet-pg*)",
    "Bash(*.config/spawn*)",
    // Keep's delivery folders, `/run/keep` and `/run/keep-test` both, and
    // the files Keep delivers, by name.
    "Bash(*run/keep*)",
    "Bash(*typesafe.api-key*)",
    "Bash(*ccd-db.url*)",
    "Bash(*ccd-db.pw*)",
    // A passfile by its variable or by its name (113): engines need not
    // name it, since libpq and spawn read it themselves. `echo $PGPASSFILE`
    // is refused too; a search of the code uses Grep, which these miss.
    "Bash(*PGPASSFILE*)",
    "Bash(*.pgpass*)",
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
    // A URL that holds a password, anywhere in a command: it shows in
    // other agents' process listings (url-leak-sources.md, 2026-11-06..08).
    "Bash(*://*:*@*)",
    // The whole environment printed: engines' environments hold the test
    // database's URL, and Claude Code hands every tool shell the env block
    // of ~/.claude/settings.json (the gateway key). Alone, or behind a
    // pipe, `;` or `&&`, so the refusal does not depend on Claude Code
    // splitting the command. `printenv HOME`, `set -e`, `export FOO=bar`
    // and `env VAR=x cmd` still run.
    "Bash(env)",
    "Bash(env |*)",
    "Bash(env|*)",
    "Bash(*; env)",
    "Bash(*&& env)",
    "Bash(*; env |*)",
    "Bash(*&& env |*)",
    "Bash(printenv)",
    "Bash(printenv |*)",
    "Bash(printenv|*)",
    "Bash(*; printenv)",
    "Bash(*&& printenv)",
    "Bash(*; printenv |*)",
    "Bash(*&& printenv |*)",
    "Bash(export -p)",
    "Bash(export -p |*)",
    "Bash(export -p|*)",
    "Bash(*; export -p)",
    "Bash(*&& export -p)",
    "Bash(*; export -p |*)",
    "Bash(*&& export -p |*)",
    "Bash(declare -x)",
    "Bash(declare -x |*)",
    "Bash(declare -x|*)",
    "Bash(*; declare -x)",
    "Bash(*&& declare -x)",
    "Bash(*; declare -x |*)",
    "Bash(*&& declare -x |*)",
    "Bash(set)",
    "Bash(set |*)",
    "Bash(set|*)",
    "Bash(*; set)",
    "Bash(*&& set)",
    "Bash(*; set |*)",
    "Bash(*&& set |*)",
    "Bash(export)",
    "Bash(export |*)",
    "Bash(export|*)",
    "Bash(*; export)",
    "Bash(*&& export)",
    "Bash(*; export |*)",
    "Bash(*&& export |*)",
    "Bash(declare)",
    "Bash(declare |*)",
    "Bash(declare|*)",
    "Bash(*; declare)",
    "Bash(*&& declare)",
    "Bash(*; declare |*)",
    "Bash(*&& declare |*)",
    "Bash(declare -p)",
    "Bash(declare -p |*)",
    "Bash(declare -p|*)",
    "Bash(*; declare -p)",
    "Bash(*&& declare -p)",
    "Bash(*; declare -p |*)",
    "Bash(*&& declare -p |*)",
    "Bash(typeset)",
    "Bash(typeset |*)",
    "Bash(typeset|*)",
    "Bash(*; typeset)",
    "Bash(*&& typeset)",
    "Bash(*; typeset |*)",
    "Bash(*&& typeset |*)",
    "Bash(typeset -p)",
    "Bash(typeset -p |*)",
    "Bash(typeset -p|*)",
    "Bash(*; typeset -p)",
    "Bash(*&& typeset -p)",
    "Bash(*; typeset -p |*)",
    "Bash(*&& typeset -p |*)",
    "Bash(typeset -x)",
    "Bash(typeset -x |*)",
    "Bash(typeset -x|*)",
    "Bash(*; typeset -x)",
    "Bash(*&& typeset -x)",
    "Bash(*; typeset -x |*)",
    "Bash(*&& typeset -x |*)",
    "Bash(env -0)",
    "Bash(env -0 |*)",
    "Bash(env -0|*)",
    "Bash(*; env -0)",
    "Bash(*&& env -0)",
    "Bash(*; env -0 |*)",
    "Bash(*&& env -0 |*)",
    "Bash(env --null)",
    "Bash(env --null |*)",
    "Bash(env --null|*)",
    "Bash(*; env --null)",
    "Bash(*&& env --null)",
    "Bash(*; env --null |*)",
    "Bash(*&& env --null |*)",
    "Bash(/usr/bin/env)",
    "Bash(/usr/bin/env |*)",
    "Bash(/usr/bin/env|*)",
    "Bash(*; /usr/bin/env)",
    "Bash(*&& /usr/bin/env)",
    "Bash(*; /usr/bin/env |*)",
    "Bash(*&& /usr/bin/env |*)",
    "Bash(command env)",
    "Bash(command env |*)",
    "Bash(command env|*)",
    "Bash(*; command env)",
    "Bash(*&& command env)",
    "Bash(*; command env |*)",
    "Bash(*&& command env |*)",
    // Process listings with their arguments: a URL on another agent's
    // command line shows in them. `ps -p <pid>`, `pgrep -f` (pids only),
    // `top -b -n 1` and `pstree -p` still run.
    "Bash(*ps -ef*)",
    "Bash(*ps -fe*)",
    "Bash(*ps -eF*)",
    "Bash(*ps -f *)",
    "Bash(*ps -fp*)",
    "Bash(*ps aux*)",
    "Bash(*ps axu*)",
    "Bash(*ps -aux*)",
    "Bash(*ps *args*)",
    "Bash(*ps *cmd*)",
    "Bash(*ps *command*)",
    "Bash(*pgrep* -a*)",
    "Bash(*pgrep* -fa*)",
    "Bash(*pgrep* -af*)",
    "Bash(*pgrep* -l*)",
    "Bash(*pgrep* -fl*)",
    "Bash(*pgrep* -lf*)",
    "Bash(*pgrep* --list*)",
    "Bash(*ps ax*)",
    // `ps x` and `top … -c` at a command position only (113, rev-e's N3
    // on 108): anywhere, they refused `ls groups x`, `jq '.ps x'` and
    // `systemctl --user stop x; sh -c true`. Not after `||` or a newline
    // (Claude Code checks each part of a compound command), `(` or a
    // backtick (open).
    "Bash(ps x*)",
    "Bash(*; ps x*)",
    "Bash(*&& ps x*)",
    "Bash(*| ps x*)",
    "Bash(*$(ps x*)",
    "Bash(*ps -e -f*)",
    "Bash(*ps -A -f*)",
    "Bash(*ps -Af*)",
    "Bash(*ps -wwf*)",
    "Bash(*ps -wf*)",
    "Bash(top *-c*)",
    "Bash(*; top *-c*)",
    "Bash(*&& top *-c*)",
    "Bash(*| top *-c*)",
    "Bash(*$(top *-c*)",
    "Bash(*pstree -a*)",
    "Bash(*pstree -pa*)",
    "Bash(*pstree -ap*)",
    "Bash(*pstree * -a*)",
    "Bash(*/cmdline*)",
    "Read(//proc/**)",
    // Claude Code's settings by any path (they hold the gateway key):
    // `cd ~/.claude && grep … settings.json` names no file the rules above
    // know. A workspace's own .claude is refused too. Bash stays closed on
    // all of `.claude` (issue #78's review): a glob, a quote or an escape
    // spells any file in it, so only the Read and Edit rules are narrow.
    "Bash(*/.claude*)",
    "Bash(*cd .claude*)",
    "Bash(*settings.local.json*)",
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

/// What under a Claude Code directory holds a secret (issue #78), relative
/// to it, as Read and Edit rules name it: settings (the gateway key), the
/// login (`.credentials.json`, `.claude.json`), backups, the peer sessions'
/// records and environments, keys, the prompt history, MCP auth state, the
/// IDE locks (their auth tokens), the shell snapshots (the environment of
/// the user's shell), debug logs, the file and paste histories, the owner's
/// dev mods and admin seed, and the sessions' own transcripts (with their
/// peer copies, `<session>.jsonl.rebuilt-pending`), subagent transcripts
/// and tool results (whatever a session ever printed). A
/// project's `memory/` and `workflows/` are not among them.
pub const CLAUDE_DIR_SENSITIVE: &[&str] = &[
    "settings*",
    ".credentials*",
    "**/*credential*",
    ".claude.json*",
    "backups/**",
    "sessions/**",
    "session-env/**",
    "ssh/**",
    "history.jsonl*",
    "mcp-needs-auth-cache.json*",
    "ide/**",
    "shell-snapshots/**",
    "debug/**",
    "file-history/**",
    "paste-cache/**",
    "dev-mods/**",
    "seed-admin/**",
    "projects/*/*.jsonl*",
    "projects/*/*/subagents/**",
    "projects/*/*/tool-results/**",
    "**/*.key",
    "**/*.pem",
    "**/*.env*",
];

/// Deny rules for a Claude Code directory `dir` other than `~/.claude`
/// (`CLAUDE_CONFIG_DIR`, which `spawn loop` takes only as an absolute
/// path): its sensitive files ([`CLAUDE_DIR_SENSITIVE`]) to the Read and
/// Edit tools when `dir` is absolute, and every Bash command that names
/// it, fail-closed as `.claude`'s: `dir` itself, and its last component
/// after a `/`, before one, or after `cd ` (`~/.claude-fleet/…`,
/// `$HOME//.claude-fleet`, `cd .claude-fleet`). `Bash(*CLAUDE_CONFIG_DIR*)`
/// refuses a command whose text names the variable (`cat
/// $CLAUDE_CONFIG_DIR/…`); like every Bash rule it matches the command's
/// text, not what the shell expands. A short last component refuses more
/// than the directory: a name of its own (`.claude-fleet`) is the one to
/// choose.
pub fn claude_dir_deny(dir: &str) -> Vec<String> {
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() {
        return Vec::new();
    }
    let mut rules = if dir.starts_with('/') {
        claude_dir_rules(&format!("/{dir}"))
    } else {
        Vec::new()
    };
    rules.push(format!("Bash(*{dir}*)"));
    let base = dir.rsplit('/').next().unwrap_or_default();
    if !base.is_empty() && base != dir {
        rules.push(format!("Bash(*/{base}*)"));
        rules.push(format!("Bash(*{base}/*)"));
        rules.push(format!("Bash(*cd {base}*)"));
    }
    rules.push("Bash(*CLAUDE_CONFIG_DIR*)".to_string());
    rules
}

/// The Read and Edit rules of a Claude Code directory, `path` as the rules
/// name it (`~/.claude`, `//data/fleet-claude`).
pub fn claude_dir_rules(path: &str) -> Vec<String> {
    ["Read", "Edit"]
        .iter()
        .flat_map(|tool| {
            CLAUDE_DIR_SENSITIVE
                .iter()
                .map(move |below| format!("{tool}({path}/{below})"))
        })
        .collect()
}

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
/// slot (Claude Code's small model for background work, also
/// `ANTHROPIC_SMALL_FAST_MODEL`) is the relay's small model. The cheap
/// profile pins every slot (issue #79): the top-level `model`, the main,
/// sonnet, opus and subagent slots to its main model, so neither a model
/// the user's own settings name nor a subagent's leaves the cheap
/// provider. The default profile pins only what the relay names
/// ([`ModelRelay::default_model`] and the per-slot overrides): with none,
/// Claude Code's own main model stands. Without a relay nothing about
/// models is set: the cheap profile needs one, and the Bridge refuses to
/// start it without.
///
/// No credential is ever set here: `ANTHROPIC_AUTH_TOKEN` stays the one
/// the user's settings give (the relay forwards it to the gateway and
/// holds none itself).
///
/// Every engine is denied the host's credential files and the programs
/// that read them ([`CREDENTIAL_DENY`]): a fleet worker read the loop's
/// environment file once (2026-11-08). Every engine also refuses
/// cross-session peer messages ([`CROSS_SESSION_INBOUND`]).
pub fn claude_settings(relay: Option<&ModelRelay>, profile: ModelProfile) -> String {
    claude_settings_in(relay, profile, None)
}

/// [`claude_settings`] for an engine whose Claude Code directory is
/// `config_dir` (`CLAUDE_CONFIG_DIR`, issue #79) rather than `~/.claude`:
/// its sensitive files are denied as `~/.claude`'s are
/// ([`claude_dir_deny`]), on top of [`CREDENTIAL_DENY`].
pub fn claude_settings_in(
    relay: Option<&ModelRelay>,
    profile: ModelProfile,
    config_dir: Option<&str>,
) -> String {
    let mut deny: Vec<String> = CREDENTIAL_DENY
        .iter()
        .map(|rule| (*rule).to_string())
        .collect();
    if let Some(dir) = config_dir {
        deny.extend(claude_dir_deny(dir));
    }
    let mut settings = json!({
        "permissions": {
            "defaultMode": "bypassPermissions",
            "deny": deny,
        },
        "skipDangerousModePermissionPrompt": true,
        "crossSessionInbound": CROSS_SESSION_INBOUND,
    });
    if let Some(relay) = relay {
        let mut env = serde_json::Map::new();
        let mut pin = |slot: &str, model: Option<&str>| {
            if let Some(model) = model {
                env.insert(slot.to_string(), json!(model));
            }
        };
        pin("ANTHROPIC_BASE_URL", Some(&relay.url));
        let main = relay.main_model(profile);
        let (sonnet, opus, subagent) = match profile {
            ModelProfile::Cheap => (main, main, main),
            ModelProfile::Default => (
                relay.sonnet_model.as_deref().or(main),
                relay.opus_model.as_deref().or(main),
                relay.subagent_model.as_deref().or(main),
            ),
        };
        pin("ANTHROPIC_MODEL", main);
        pin("ANTHROPIC_DEFAULT_OPUS_MODEL", opus);
        pin("ANTHROPIC_DEFAULT_SONNET_MODEL", sonnet);
        pin("CLAUDE_CODE_SUBAGENT_MODEL", subagent);
        pin("ANTHROPIC_DEFAULT_HAIKU_MODEL", Some(&relay.small_model));
        pin("ANTHROPIC_SMALL_FAST_MODEL", Some(&relay.small_model));
        settings["env"] = serde_json::Value::Object(env);
        if let Some(main) = main {
            settings["model"] = json!(main);
        }
    }
    settings.to_string()
}

/// What every engine does with a cross-session peer message (objective
/// 206, issue #42): Claude Code's `crossSessionInbound`, "refuse". Every
/// Claude Code session of this user lists the others from the shared
/// `~/.claude/sessions` records and can `SendMessage` into one; the engines
/// skip permission prompts, so under Claude Code's default (mode parity) a
/// message from another bypassing session would be delivered with no one
/// approving it. Refused, its sender gets a "refused" receipt. The inline
/// `--settings` value wins over the user's own file (only managed settings
/// override it); a Claude Code without the setting ignores the key. Spawn's
/// own input, typed into the engine's pane, is no peer message and is
/// unchanged. Claude Code enforces it; Spawn only hands it over.
pub const CROSS_SESSION_INBOUND: &str = "refuse";

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

/// An execution that ended, as the log says: its files are no longer
/// watched, and with an archive it is copied on the next pass and recorded
/// as ended ([`Bridge::archive_ended`]), on every pass until the copy is
/// made, however it ended (an end that already put it there keeps its
/// own).
fn ended(
    watch: Option<&dyn FileWatch>,
    archives: Option<&std::sync::Mutex<BTreeMap<(AgentId, String), EndedArchive>>>,
    view: &SessionView,
    session: &SessionId,
    active: &Active,
) {
    if let Some(watch) = watch {
        watch.unwatch(&watch_key(&active.execution_id));
    }
    let (Some(list), Some(native)) = (archives, view.native.clone()) else {
        return;
    };
    let progress = ArchiveProgress {
        agent: view.agent_id.clone(),
        native: native.clone(),
        session: session.clone(),
        execution: active.execution_id.clone(),
        workspace: active.workspace.clone(),
        live: false,
        known_ms: i64::MIN,
        archived: active.follow.archiving.archived,
        behind_ms: active.follow.archiving.behind_ms,
    };
    list.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry((view.agent_id.clone(), native))
        .or_insert(EndedArchive {
            progress,
            error: None,
            written: None,
        });
}

/// What a record says is behind: a transcript never known (no read of it
/// succeeded) of an execution that ended is taken as lacking everything,
/// since no later read will tell (a live one says so by its `known_ms`).
fn unknown_behind(behind_ms: Option<i64>, known_ms: Option<i64>, live: bool) -> Option<i64> {
    if known_ms.is_none() && !live {
        return Some(behind_ms.unwrap_or(i64::MIN));
    }
    behind_ms
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

/// How many non-empty lines of the last pane a failed ready wait quotes.
const PANE_TAIL_EXCERPT_LINES: usize = 6;

/// The longest quote of that pane, in bytes, its `...` included.
const PANE_TAIL_EXCERPT_MAX_BYTES: usize = 400;

/// What a TUI draws rules and boxes with. A line made only of these (at
/// least 3, spaces aside) is decoration, not text: a 120-column border of
/// `─` alone would cost 360 of the excerpt's bytes.
const PANE_RULE_CHARS: [char; 23] = [
    '\u{2500}', '\u{2501}', '\u{2550}', '-', '_', '=', '*', '\u{b7}', '\u{2502}', '\u{2503}',
    '\u{2551}', '\u{256d}', '\u{256e}', '\u{2570}', '\u{256f}', '\u{250c}', '\u{2510}', '\u{2514}',
    '\u{2518}', '\u{2554}', '\u{2557}', '\u{255a}', '\u{255d}',
];

/// A box's side borders (`│`, `┃`, `║`), trimmed from both ends of a line.
const PANE_SIDE_BORDERS: [char; 3] = ['\u{2502}', '\u{2503}', '\u{2551}'];

/// Horizontal box-drawing characters (`─`, `━`, `═`): a run of more than 3
/// of one inside a line is kept as 3.
const PANE_RUN_CHARS: [char; 3] = ['\u{2500}', '\u{2501}', '\u{2550}'];

/// What the host's agent reading ([`AgentReading`]) and the driver's own
/// reading of the pane make together ([`Bridge::follow_screen`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Merged {
    /// Both witnesses say this: a fact at once, without the second capture
    /// a reading alone needs.
    Agrees(ScreenState, String),
    /// They do not agree, or the host cannot tell: the screen goes by the
    /// driver's readings alone, two in a row, as without a host reading.
    Own,
    /// The host sees no agent in the pane: the engine may have exited (its
    /// exit status says), and the screen goes by the driver's readings.
    Gone,
    /// The host sees another agent than the engine's in its pane: nothing
    /// is known, and nothing is recorded.
    Foreign,
}

/// The two witnesses of an engine's screen, merged. The host's reading
/// never decides alone what the driver reads otherwise, and a state is
/// taken at once only where both agree:
///
/// - `working` with the driver reading a running turn (or nothing it
///   knows): [`ScreenState::Busy`];
/// - `blocked` by a rule that saw a dialog on screen, with the driver
///   reading a dialog (or nothing it knows): [`ScreenState::Permission`];
/// - `idle` with the driver reading its prompt (a positive reading: the
///   host's idle is often its fallback, for any screen no rule matched, so
///   an unknown popup is never taken for a prompt): the driver's state;
/// - anything else, `unknown` included: the driver's own readings.
fn merge_witness(
    driver: &dyn AgentDriver,
    witness: &AgentReading,
    reading: &crate::screen::ScreenReading,
) -> Merged {
    let Some(agent) = witness.agent.as_deref() else {
        return Merged::Gone;
    };
    if agent != driver.id() {
        return Merged::Foreign;
    }
    let said = |what: &str| {
        let rule = witness
            .rule
            .as_deref()
            .map_or_else(String::new, |rule| format!(" ({rule})"));
        format!("herdr {agent}: {what}{rule}")
    };
    match (witness.status, reading.state) {
        (AgentStatus::Working, ScreenState::Busy) => {
            Merged::Agrees(ScreenState::Busy, reading.evidence.clone())
        }
        (AgentStatus::Working, ScreenState::Unknown) => {
            Merged::Agrees(ScreenState::Busy, said("working"))
        }
        (AgentStatus::Blocked, ScreenState::Permission)
            if witness.rule.is_some() && witness.blocker_visible =>
        {
            Merged::Agrees(ScreenState::Permission, reading.evidence.clone())
        }
        (AgentStatus::Blocked, ScreenState::Unknown)
            if witness.rule.is_some() && witness.blocker_visible =>
        {
            Merged::Agrees(ScreenState::Permission, said("blocked"))
        }
        (AgentStatus::Idle, state) if state.accepts_input() => {
            Merged::Agrees(state, reading.evidence.clone())
        }
        _ => Merged::Own,
    }
}

/// The tail of `pane` as one bounded line for a failed ready wait's error,
/// or `None` when it holds no text. Credentials go first
/// ([`crate::redact::redact`]: Claude Code's API-key dialog prints part of
/// the key, and this text reaches the log). Each line then has its control
/// characters made spaces, its runs of whitespace made one space, its box
/// side borders trimmed and its runs of `─`, `━` or `═` kept to 3; a line
/// left empty, or made only of rule characters ([`PANE_RULE_CHARS`]), is
/// dropped. The last [`PANE_TAIL_EXCERPT_LINES`] lines left are joined with
/// ` | `. Past [`PANE_TAIL_EXCERPT_MAX_BYTES`] the front is cut, on a
/// character boundary and marked `...`: the bottom of the pane (a dialog's
/// question and its choices) is what the reader needs.
fn format_pane_tail_excerpt(pane: &str) -> Option<String> {
    let clean = crate::redact::redact(pane);
    let lines: Vec<String> = clean
        .lines()
        .map(clean_pane_line)
        .filter(|line| !line.is_empty() && !is_pane_rule(line))
        .collect();
    if lines.is_empty() {
        return None;
    }
    let joined = lines[lines.len().saturating_sub(PANE_TAIL_EXCERPT_LINES)..].join(" | ");
    if joined.len() <= PANE_TAIL_EXCERPT_MAX_BYTES {
        return Some(joined);
    }
    let mut start = joined.len() - (PANE_TAIL_EXCERPT_MAX_BYTES - "...".len());
    while !joined.is_char_boundary(start) {
        start += 1;
    }
    Some(format!("...{}", &joined[start..]))
}

/// One pane line for [`format_pane_tail_excerpt`]: control characters
/// made spaces, runs of whitespace made one space, box side borders trimmed
/// from both ends, and a run of more than 3 of one of [`PANE_RUN_CHARS`]
/// kept as 3.
fn clean_pane_line(line: &str) -> String {
    let flat: String = line
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let words = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = words.trim_matches(|c: char| c.is_whitespace() || PANE_SIDE_BORDERS.contains(&c));
    let mut out = String::with_capacity(trimmed.len());
    let mut previous = None;
    let mut run = 0;
    for c in trimmed.chars() {
        run = if previous == Some(c) { run + 1 } else { 1 };
        previous = Some(c);
        if run <= 3 || !PANE_RUN_CHARS.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// Whether a cleaned pane line is a rule or a box border: at least 3
/// characters besides spaces, every one of them in [`PANE_RULE_CHARS`].
/// `--help`, `-1` or `2. No` hold other characters and stay.
fn is_pane_rule(line: &str) -> bool {
    let mut marks = line.chars().filter(|c| !c.is_whitespace());
    let mut count = 0;
    marks.all(|c| {
        count += 1;
        PANE_RULE_CHARS.contains(&c)
    }) && count >= 3
}

/// Read log events appended after `after` and check whether a background mount
/// for `(agent_id, command_id)` is still wanted (neither locally cancelled,
/// finished, nor superseded by another Bridge's recorded mount or open
/// execution on that agent's branch).
fn mount_still_wanted(
    log: &Arc<dyn EventLog>,
    cancel: &AtomicBool,
    agent_id: &AgentId,
    command_id: CommandId,
    after: GlobalPosition,
) -> Result<(), String> {
    const BATCH: usize = 200;
    if cancel.load(Ordering::SeqCst) {
        return Err(format!("mount for {command_id} cancelled"));
    }
    let mut cursor = after;
    let mut mounted: HashMap<CommandId, Option<WorkspaceId>> = HashMap::new();
    let mut open_executions: HashSet<ExecutionId> = HashSet::new();
    loop {
        let batch = log
            .read_global(cursor, BATCH)
            .map_err(|error| format!("check mount readiness for {command_id}: {error}"))?;
        if batch.is_empty() {
            break;
        }
        for record in &batch {
            let event = &record.event;
            if matches!(
                event.kind,
                EventKind::CommandFinished { command_id: id, .. } if id == command_id
            ) {
                return Err(format!("{command_id} already finished"));
            }
            if &event.agent_id != agent_id {
                continue;
            }
            fold_mount_liveness(event, &mut mounted, &mut open_executions);
        }
        cursor = batch.last().expect("non-empty batch").position;
        if batch.len() < BATCH {
            break;
        }
    }
    if !open_executions.is_empty() || !mounted.is_empty() {
        return Err(format!("peer already holds workspace for {agent_id}"));
    }
    Ok(())
}

fn fold_mount_liveness(
    event: &NewEvent,
    mounted: &mut HashMap<CommandId, Option<WorkspaceId>>,
    open_executions: &mut HashSet<ExecutionId>,
) {
    match &event.kind {
        EventKind::CommandStepped {
            command_id,
            step: CommandStep::Mounted,
            workspace,
        } if event.source == Source::Bridge => {
            mounted.insert(*command_id, workspace.clone());
        }
        EventKind::CommandStepped {
            command_id,
            step: CommandStep::MountReleased,
            workspace,
        } if event.source == Source::Bridge => {
            let release = mounted.get(command_id).is_some_and(|recorded| {
                workspace.is_none() || recorded.as_ref() == workspace.as_ref()
            });
            if release {
                mounted.remove(command_id);
            }
        }
        EventKind::ExecutionStarted { command_id, .. } => {
            mounted.remove(command_id);
            if let Some(execution_id) = event.execution_id.clone() {
                open_executions.insert(execution_id);
            }
        }
        EventKind::ExecutionEnded { .. } if event.source == Source::Bridge => {
            if let Some(execution_id) = &event.execution_id {
                open_executions.remove(execution_id);
            }
        }
        _ => {}
    }
}

/// Re-check the event log under the store's branch lock before unmounting an
/// abandoned workspace: refuses with [`UNMOUNT_IN_USE`] if a newer mount or
/// execution for the same canonical workspace appeared after `after`, and
/// fails closed (returning [`WorkspaceError::Unavailable`]) if the log cannot
/// be read.
fn unmount_still_safe(
    log: &Arc<dyn EventLog>,
    workspaces: &Arc<dyn WorkspaceStore>,
    workspace: &WorkspaceId,
    after: GlobalPosition,
) -> Result<(), WorkspaceError> {
    const BATCH: usize = 200;
    let canonical = workspaces.canonical(workspace);
    let mut cursor = after;
    let mut mounted: HashMap<CommandId, WorkspaceId> = HashMap::new();
    let mut open_executions: HashSet<ExecutionId> = HashSet::new();
    loop {
        let batch = log.read_global(cursor, BATCH).map_err(|error| {
            WorkspaceError::Unavailable(format!("check log before unmounting {workspace}: {error}"))
        })?;
        if batch.is_empty() {
            break;
        }
        for record in &batch {
            fold_unmount_liveness(
                &record.event,
                workspaces,
                &canonical,
                &mut mounted,
                &mut open_executions,
            );
        }
        cursor = batch.last().expect("non-empty batch").position;
        if batch.len() < BATCH {
            break;
        }
    }
    if !open_executions.is_empty() || !mounted.is_empty() {
        return Err(WorkspaceError::Refused {
            code: UNMOUNT_IN_USE.to_string(),
            message: format!("{workspace} is in use by a newer mount or execution"),
        });
    }
    Ok(())
}

fn fold_unmount_liveness(
    event: &NewEvent,
    workspaces: &Arc<dyn WorkspaceStore>,
    canonical: &WorkspaceId,
    mounted: &mut HashMap<CommandId, WorkspaceId>,
    open_executions: &mut HashSet<ExecutionId>,
) {
    match &event.kind {
        EventKind::CommandStepped {
            command_id,
            step: CommandStep::Mounted,
            workspace: Some(ws),
        } if event.source == Source::Bridge && workspaces.canonical(ws) == *canonical => {
            mounted.insert(*command_id, ws.clone());
        }
        EventKind::CommandStepped {
            command_id,
            step: CommandStep::MountReleased,
            workspace: ws,
        } if event.source == Source::Bridge => {
            let release = mounted.get(command_id).is_some_and(|recorded| {
                ws.is_none()
                    || ws
                        .as_ref()
                        .is_some_and(|w| workspaces.canonical(w) == workspaces.canonical(recorded))
            });
            if release {
                mounted.remove(command_id);
            }
        }
        EventKind::CommandFinished { command_id, .. } => {
            mounted.remove(command_id);
        }
        EventKind::ExecutionStarted {
            command_id,
            workspace: ws,
            ..
        } => {
            mounted.remove(command_id);
            if workspaces.canonical(ws) == *canonical
                && let Some(execution_id) = event.execution_id.clone()
            {
                open_executions.insert(execution_id);
            }
        }
        EventKind::ExecutionEnded { .. } if event.source == Source::Bridge => {
            if let Some(execution_id) = &event.execution_id {
                open_executions.remove(execution_id);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};

    use super::{Bridge, BridgeConfig, RESOLVE_PRINCIPAL, mounted_key, trust_answer_key, waits};
    use super::{ENGINE_UNAVAILABLE, EngineViewPolicy, ViewedLaunch, rfc3339_ms};
    use crate::command::{CommandAction, NamedKey, Outcome, Submission};
    use crate::event::{CommandStep, ENGINE, EventKind, GlobalPosition, NewEvent, Source};
    use crate::id::{
        AgentId, BranchName, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId,
        TerminalName, WorkspaceId,
    };
    use crate::memory::{MemoryStore, MemoryWatch};
    use crate::port::{
        Branch, CommandQueue, Commit, EventLog, Launch, StoreError, TerminalError, TerminalHost,
        WorkspaceError, WorkspaceStore,
    };
    use crate::screen::ScreenState;
    use crate::time::{ManualClock, Timestamp};
    use std::time::Duration;

    /// Claude Code's documented Bash rule: `*` matches any text.
    fn glob(pattern: &str, text: &str) -> bool {
        let (pattern, text): (Vec<char>, Vec<char>) =
            (pattern.chars().collect(), text.chars().collect());
        let mut reach = vec![vec![false; text.len() + 1]; pattern.len() + 1];
        reach[0][0] = true;
        for i in 1..=pattern.len() {
            for j in 0..=text.len() {
                reach[i][j] = if pattern[i - 1] == '*' {
                    reach[i - 1][j] || (j > 0 && reach[i][j - 1])
                } else {
                    j > 0 && reach[i - 1][j - 1] && pattern[i - 1] == text[j - 1]
                };
            }
        }
        reach[pattern.len()][text.len()]
    }

    #[test]
    fn engines_are_refused_the_accidental_prints_of_a_password() {
        let refused = |command: &str| {
            super::CREDENTIAL_DENY.iter().any(|rule| {
                rule.strip_prefix("Bash(")
                    .and_then(|body| body.strip_suffix(')'))
                    .is_some_and(|body| glob(body, command))
            })
        };
        let parts = |command: &str| {
            command
                .split(['|', ';', '&', '\n'])
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        for command in [
            "env",
            "env | grep -i spawn",
            "env|grep PG",
            "cd /tmp && env",
            "cd /tmp; env | sort",
            "printenv | grep DATABASE",
            "export -p",
            "declare -x",
            "set | grep SPAWN",
            "typeset -p",
            "env --null | tr '\\0' '\\n'",
            "/usr/bin/env",
            "command env",
            "cat /proc/self/environ",
            "psql 'postgres://bb:made-up-pw@localhost/bb' -c 'select 1'",
            "cd /tmp; SPAWN_TEST_DATABASE_URL=postgres://bb:pw@localhost/bb cargo test",
            "ps -ef | grep cargo",
            "ps aux",
            "ps -eo pid,args",
            "ps -o cmd= -p 1",
            "pgrep -af cargo",
            "pgrep -u 501 -a sh",
            "cat /proc/1/cmdline",
            "top -b -n 1 -c",
            "pstree -ap",
            "cd ~/.claude && grep TOKEN settings.json",
            "cd; cd .claude; less settings.json",
            "python3 -c \"open('/home/bb/.claude/settings.local.json')\"",
            "strings ~/.local/share/spawn-rsi/spawn.env.bak-relay",
            "cat $PGPASSFILE",
            "cat ~/.pgpass",
            "ps x",
            "cd /tmp && ps x",
            "uptime; ps x",
            "echo $(ps xww)",
            "pgrep -f cargo | ps x",
            "uptime && top -b -n 1 -c",
            "echo $(top -b -n 1 -c)",
        ] {
            assert!(refused(command), "{command}");
        }
        for command in [
            "env VAR=x cargo test",
            "env -u FOO sh -c true",
            "env -i PATH=/usr/bin sh -c true",
            "printenv HOME",
            "set -euo pipefail; make",
            "export FOO=bar",
            "declare -a list",
            "declare -p FOO",
            "psql 'postgres://bb@localhost/bb' -c 'select 1'",
            "curl -s http://127.0.0.1:8787/api/rsi",
            "ps -p 123",
            "ps -o pid,etime -p 123",
            "pgrep -f cargo",
            "pgrep -x tmux",
            "kill 123",
            "cat /proc/loadavg",
            "ls ~/.config",
            "top -b -n 1",
            "pstree -p",
            "cd env && ls",
            "ls groups x",
            "cargo test -- steps x",
            "jq '.ps x' report.json",
            "systemctl --user stop x; sh -c true",
        ] {
            assert!(!refused(command), "{command}");
            for part in parts(command) {
                assert!(!refused(&part), "{command}: {part}");
            }
        }
        assert!(super::CREDENTIAL_DENY.contains(&"Read(//proc/**)"));
    }

    /// Claude Code's Read and Edit rules (gitignore style): `**/` is any
    /// number of directories, a trailing `**` anything, `*` anything but a
    /// `/`.
    fn path_glob(pattern: &str, path: &str) -> bool {
        if let Some(rest) = pattern.strip_prefix("**/") {
            return path_glob(rest, path)
                || path
                    .match_indices('/')
                    .any(|(at, _)| path_glob(rest, &path[at + 1..]));
        }
        if pattern == "**" {
            return true;
        }
        match pattern.chars().next() {
            None => path.is_empty(),
            Some('*') => {
                let rest = &pattern[1..];
                path_glob(rest, path)
                    || path
                        .char_indices()
                        .take_while(|(_, c)| *c != '/')
                        .any(|(at, c)| path_glob(rest, &path[at + c.len_utf8()..]))
            }
            Some(c) => path
                .strip_prefix(c)
                .is_some_and(|tail| path_glob(&pattern[c.len_utf8()..], tail)),
        }
    }

    fn denied_by(tool: &str, path: &str) -> bool {
        super::CREDENTIAL_DENY.iter().any(|rule| {
            rule.strip_prefix(tool)
                .and_then(|body| body.strip_prefix('('))
                .and_then(|body| body.strip_suffix(')'))
                .is_some_and(|pattern| path_glob(pattern, path))
        })
    }

    fn bash_refused(command: &str) -> bool {
        super::CREDENTIAL_DENY.iter().any(|rule| {
            rule.strip_prefix("Bash(")
                .and_then(|body| body.strip_suffix(')'))
                .is_some_and(|body| glob(body, command))
        })
    }

    #[test]
    fn the_path_glob_reads_rules_as_claude_code_does() {
        assert!(path_glob("~/.claude/**/*.pem", "~/.claude/a.pem"));
        assert!(path_glob("~/.claude/**/*.pem", "~/.claude/x/y/a.pem"));
        assert!(!path_glob("~/.claude/*.pem", "~/.claude/x/a.pem"));
        assert!(path_glob(
            "~/.claude/sessions/**",
            "~/.claude/sessions/1.json"
        ));
        assert!(!path_glob(
            "~/.claude/settings*",
            "~/.claude/projects/settings.json"
        ));
    }

    /// Issue #98: Keep's delivery folders are denied to the Read and Edit
    /// tools and to every Bash command that names them; the checkout's
    /// `scripts/keep/` and a mere `keep` still run.
    #[test]
    fn engines_are_denied_keep_s_delivery_folders() {
        for path in [
            "//run/keep/typesafe.api-key",
            "//run/keep/a/b",
            "//run/keep-test/VERSION",
        ] {
            assert!(denied_by("Read", path), "Read {path}");
            assert!(denied_by("Edit", path), "Edit {path}");
        }
        for path in ["//run/keeper/x", "//run/user/1000/keep"] {
            assert!(!denied_by("Read", path), "Read {path}");
        }
        for command in [
            "cat /run/keep/typesafe.api-key",
            "ls -la /run/keep",
            "ls /run/keep-test/",
            "cat /run/keep-test/VERSION",
            "python3 -c \"open('/run/keep/typesafe.api-key')\"",
            "cd /run && less keep/typesafe.api-key",
            "less /run//keep/typesafe.api-key",
            "cd /run/./keep && cat ccd-db.url",
            "cd / && cat run//keep/ccd-db.pw",
        ] {
            assert!(bash_refused(command), "{command}");
        }
        for rule in [
            "Bash(*typesafe.api-key*)",
            "Bash(*ccd-db.url*)",
            "Bash(*ccd-db.pw*)",
        ] {
            assert!(super::CREDENTIAL_DENY.contains(&rule), "{rule}");
        }
        for command in [
            "bash scripts/keep/credential-health.sh",
            "bash scripts/keep/test-credential-health.sh",
            "ls scripts/keep",
            "git log -- scripts/keep/",
            "cargo test -p spawn-core keep",
            "cargo test -p spawn-core --lib -- engines_are_denied_keep_s_delivery_folders",
            "systemctl --user status spawn-jev-relay",
            "bash rsi/scenarios/nightly/201.sh",
        ] {
            assert!(!bash_refused(command), "{command}");
        }
    }

    /// Issue #78: Claude Code's sensitive files under `~/.claude` stay
    /// denied to the Read and Edit tools; its memory, a project's memory
    /// and workflow transcripts, plugins, skills, themes, cache, status
    /// line and `CLAUDE.md` are theirs to use. Bash is refused all of
    /// `~/.claude`, either way.
    #[test]
    fn engines_keep_claude_code_s_secrets_and_get_its_working_files() {
        let sensitive = [
            "~/.claude/settings.json",
            "~/.claude/settings.local.json",
            "~/.claude/settings.json.bak",
            "~/.claude/settings.json.bak-2026-11-01",
            "~/.claude/.credentials.json",
            "~/.claude/.claude.json",
            "~/.claude/backups/settings.json.1",
            "~/.claude/backups/a/b/.credentials.json",
            "~/.claude/sessions/1234.json",
            "~/.claude/session-env/c",
            "~/.claude/ssh/id_ed25519",
            "~/.claude/history.jsonl",
            "~/.claude/mcp-needs-auth-cache.json",
            "~/.claude/ide/41234.lock",
            "~/.claude/shell-snapshots/snapshot-bash-1.sh",
            "~/.claude/debug/abc.txt",
            "~/.claude/file-history/a",
            "~/.claude/paste-cache/b",
            "~/.claude/dev-mods/m.js",
            "~/.claude/seed-admin/seed.json",
            "~/.claude/projects/-proj/abc.jsonl",
            "~/.claude/projects/-proj/abc.jsonl.rebuilt-pending",
            "~/.claude/projects/-proj/abc/subagents/x.jsonl",
            "~/.claude/projects/-proj/abc/tool-results/x",
            "~/.claude/gateway.key",
            "~/.claude/plugins/x/server.pem",
            "~/.claude/projects/p/.env",
            "~/.claude/projects/p/prod.env.local",
            "~/.claude/plugins/x/credentials.toml",
            "~/.claude.json",
            "~/.claude.json.backup",
        ];
        for path in sensitive {
            assert!(denied_by("Read", path), "Read {path}");
            assert!(denied_by("Edit", path), "Edit {path}");
        }
        let working = [
            "~/.claude/memory/MEMORY.md",
            "~/.claude/memory/notes/a.md",
            "~/.claude/projects/-proj/memory/MEMORY.md",
            "~/.claude/projects/-proj/workflows/w.jsonl",
            "~/.claude/plugins/p/index.js",
            "~/.claude/plugins/marketplace/plugin.json",
            "~/.claude/skills/review/SKILL.md",
            "~/.claude/themes/dark.json",
            "~/.claude/cache/changelog.md",
            "~/.claude/statusline.py",
            "~/.claude/CLAUDE.md",
        ];
        for path in working {
            assert!(!denied_by("Read", path), "Read {path}");
            assert!(!denied_by("Edit", path), "Edit {path}");
        }
        for path in sensitive.iter().chain(&working) {
            for command in [
                format!("cat {path}"),
                format!("python3 -c \"open('{}')\"", path.replace('~', "/home/bb")),
                format!("less \"$HOME/{}\"", &path[2..]),
            ] {
                assert!(bash_refused(&command), "{command}");
            }
        }
    }

    /// Issue #78's review (F1, F2): Bash stays closed on `~/.claude`,
    /// since a glob, a quote, an escape or a `cd` then `..` reaches any
    /// file in it; text that names `.claude` with no slash runs.
    #[test]
    fn engines_cannot_spell_their_way_to_claude_code_s_secrets() {
        for command in [
            "cd ~/.claude && grep TOKEN settings.json",
            "cd; cd .claude; less settings.json",
            "cd .claude && cat settings.json",
            "cd \"$HOME/.claude\" && cat settings.json",
            "cd $HOME/.claude; cat settings.json",
            "pushd ~/.claude >/dev/null; cat settings.json",
            "ls ~/.claude",
            "grep -r TOKEN ~/.claude/",
            "grep -r TOKEN ~/.claude*",
            "find ~/.claude -name 'settings*'",
            "cat ~/.claude/*.json",
            "cat ~/.claude/s*",
            "cat ~/.claude/set?ings.json",
            "cat ~/.claude/se''ttings.json",
            "cat ~/.claude/s\\ettings.json",
            "tar czf /tmp/x.tgz ~/.claude/*",
            "cat ~/.claude/./settings.json",
            "cat ~/.claude/memory/../settings.json",
            "cd ~/.claude/memory && cat ../settings.json",
            "cat ~/.claude//settings.json",
            "cat ~/.claude/{settings,x}.json",
            "cat \"$HOME/.claude/\"settings.json",
            "python3 -c \"open('/home/bb/.claude/settings.local.json')\"",
            "python3 -c \"open('/home/bb/.claude/settings.json')\"",
            "python3 -c \"import os; os.chdir('/home/bb/.claude'); print(open('settings.json').read())\"",
            "cat ~/.claude.json",
            "cat .claude/settings.json",
        ] {
            assert!(bash_refused(command), "{command}");
        }
        for command in [
            "grep -rn \"\\.claude\" crates/",
            "git commit -m \"fix .claude\"",
            "cargo test -p spawn-core | jq .key",
            "cd .. && ls",
        ] {
            assert!(!bash_refused(command), "{command}");
        }
    }

    /// One list of what is sensitive under a Claude Code directory, read in
    /// two places: the `~/.claude` Read and Edit rules of
    /// [`super::CREDENTIAL_DENY`] are exactly [`super::claude_dir_rules`]
    /// of `~/.claude`, both ways.
    #[test]
    fn the_claude_directory_rules_are_the_credential_rules_for_claude_s_own() {
        let listed: std::collections::BTreeSet<String> = super::CREDENTIAL_DENY
            .iter()
            .filter(|rule| {
                rule.starts_with("Read(~/.claude/") || rule.starts_with("Edit(~/.claude/")
            })
            .map(|rule| (*rule).to_string())
            .collect();
        let generated: std::collections::BTreeSet<String> =
            super::claude_dir_rules("~/.claude").into_iter().collect();
        assert_eq!(listed, generated);
        assert_eq!(generated.len(), 2 * super::CLAUDE_DIR_SENSITIVE.len());
    }

    /// Issue #79's review (F4): a Claude Code directory of the engine's own
    /// is denied its secrets to Read and Edit, `.claude.json` included, and
    /// every Bash command that names it.
    #[test]
    fn an_engines_own_claude_directory_is_closed_to_bash() {
        let rules = super::claude_dir_deny("/home/bb/.claude-fleet/");
        let denied = |tool: &str, path: &str| {
            rules.iter().any(|rule| {
                rule.strip_prefix(tool)
                    .and_then(|body| body.strip_prefix('('))
                    .and_then(|body| body.strip_suffix(')'))
                    .is_some_and(|pattern| path_glob(pattern, path))
            })
        };
        let refused = |command: &str| {
            rules.iter().any(|rule| {
                rule.strip_prefix("Bash(")
                    .and_then(|body| body.strip_suffix(')'))
                    .is_some_and(|body| glob(body, command))
            })
        };
        for path in [
            "//home/bb/.claude-fleet/settings.json",
            "//home/bb/.claude-fleet/.claude.json",
            "//home/bb/.claude-fleet/.credentials.json",
            "//home/bb/.claude-fleet/projects/-proj/abc.jsonl",
            "//home/bb/.claude-fleet/projects/-proj/abc.jsonl.rebuilt-pending",
        ] {
            assert!(denied("Read", path), "Read {path}");
            assert!(denied("Edit", path), "Edit {path}");
        }
        for path in [
            "//home/bb/.claude-fleet/memory/MEMORY.md",
            "//home/bb/.claude-fleet/projects/-proj/workflows/w.jsonl",
        ] {
            assert!(!denied("Read", path), "Read {path}");
        }
        for command in [
            "cat /home/bb/.claude-fleet/*.json",
            "cat ~/.claude-fleet/settings.json",
            "cat $HOME/.claude-fleet/settings.json",
            "cat /home/bb//.claude-fleet/settings.json",
            "cd ~ && cd .claude-fleet && cat settings.json",
            "cat .claude-fleet/settings.json",
            "cat $CLAUDE_CONFIG_DIR/settings.json",
        ] {
            assert!(refused(command), "{command}");
        }
        assert!(!refused("cargo test -p spawn-core"));
        assert!(super::claude_dir_deny("").is_empty());
        assert!(super::claude_dir_deny("/").is_empty());
        // A relative directory gets no Read or Edit rule: `spawn loop`
        // refuses one before any engine starts.
        assert!(
            super::claude_dir_deny("fleet-claude")
                .iter()
                .all(|rule| rule.starts_with("Bash("))
        );
    }

    /// Claude Code's API-key dialog, as a pane shows it: it prints the key
    /// elided, its tail still part of the secret.
    const API_KEY_DIALOG_PANE: &str = "\
 Detected a custom API key in your environment\n\
\n\
 ANTHROPIC_API_KEY: sk-ant-...Zq9xW3vT0aBcDeFgHiJk\n\
\n\
 Do you want to use this API key?\n\
\n\
 \u{276f} 1. Yes\n\
   2. No (recommended)\n\
\n\
 Enter to confirm \u{b7} Esc to exit\n";

    #[test]
    fn a_pane_tail_excerpt_quotes_the_dialog_s_last_lines_with_the_key_redacted() {
        let excerpt =
            super::format_pane_tail_excerpt(API_KEY_DIALOG_PANE).expect("the dialog has text");
        assert!(
            excerpt.contains("Do you want to use this API key?"),
            "{excerpt}"
        );
        assert!(excerpt.contains("2. No (recommended)"), "{excerpt}");
        assert!(
            excerpt.contains(&format!("ANTHROPIC_API_KEY: {}", crate::redact::REDACTED)),
            "{excerpt}"
        );
        assert!(
            !excerpt.contains("Zq9x"),
            "the key's tail leaked: {excerpt}"
        );
        assert!(!excerpt.contains('\n'), "{excerpt}");
        // Its six non-empty lines, in order, blank lines dropped.
        assert!(
            excerpt.starts_with("Detected a custom API key"),
            "{excerpt}"
        );
        assert_eq!(excerpt.matches(" | ").count(), 5, "{excerpt}");
        assert!(
            excerpt.ends_with("Enter to confirm \u{b7} Esc to exit"),
            "{excerpt}"
        );
        // A longer pane: the last six non-empty lines only.
        let longer = format!("first line\nsecond line\n{API_KEY_DIALOG_PANE}");
        let excerpt = super::format_pane_tail_excerpt(&longer).expect("text");
        assert!(!excerpt.contains("first line"), "{excerpt}");
        assert!(!excerpt.contains("second line"), "{excerpt}");
        assert!(
            excerpt.starts_with("Detected a custom API key"),
            "{excerpt}"
        );
    }

    #[test]
    fn a_pane_tail_excerpt_redacts_other_secrets_and_flattens_control_characters() {
        let pane = "curl -H 'Authorization: Bearer abcdefghijklmnop0123'\n\
                    DATABASE_URL=postgres://bb:hunter2secret@db/bb\n\
                    key sk-0123456789abcdefghij\there\x1b[0m done\n";
        let excerpt = super::format_pane_tail_excerpt(pane).expect("text");
        for secret in [
            "abcdefghijklmnop0123",
            "hunter2secret",
            "sk-0123456789abcdefghij",
        ] {
            assert!(!excerpt.contains(secret), "{secret} leaked: {excerpt}");
        }
        assert!(!excerpt.chars().any(char::is_control), "{excerpt:?}");
    }

    #[test]
    fn a_pane_tail_excerpt_of_long_multibyte_text_is_cut_on_a_char_boundary() {
        let line = "命令失败：没有这个文件 ✽ ".repeat(40);
        let pane = format!("{line}\n{line}\n{line}\n最后一行：选择 1 或 2\n");
        let excerpt = super::format_pane_tail_excerpt(&pane).expect("text");
        assert!(
            excerpt.len() <= super::PANE_TAIL_EXCERPT_MAX_BYTES,
            "{}",
            excerpt.len()
        );
        // The front is cut, so the bottom of the pane stays whole.
        assert!(excerpt.starts_with("..."), "{excerpt}");
        assert!(
            excerpt.ends_with("没有这个文件 ✽ | 最后一行：选择 1 或 2"),
            "{excerpt}"
        );
        // A short pane is not cut.
        assert_eq!(
            super::format_pane_tail_excerpt(" short \n").as_deref(),
            Some("short")
        );
    }

    #[test]
    fn a_pane_tail_excerpt_drops_a_wide_tui_border_and_keeps_the_dialog_s_bottom() {
        const WIDTH: usize = 120;
        let rule = "\u{2500}".repeat(WIDTH - 2);
        let mut lines = vec![format!("\u{256d}{rule}\u{256e}")];
        for text in API_KEY_DIALOG_PANE.lines() {
            lines.push(format!("\u{2502}{text:<width$}\u{2502}", width = WIDTH - 2));
            lines.push(format!("\u{2502}{rule}\u{2502}"));
        }
        lines.push(format!("\u{2570}{rule}\u{256f}"));
        lines.push("\u{2501}".repeat(WIDTH));
        let pane = lines.join("\n");
        let excerpt = super::format_pane_tail_excerpt(&pane).expect("text");
        assert!(
            excerpt.len() <= super::PANE_TAIL_EXCERPT_MAX_BYTES,
            "{}",
            excerpt.len()
        );
        for text in [
            "Do you want to use this API key?",
            "\u{276f} 1. Yes",
            "2. No (recommended)",
        ] {
            assert!(excerpt.contains(text), "{text} missing: {excerpt}");
        }
        assert!(
            excerpt.ends_with("Enter to confirm \u{b7} Esc to exit"),
            "{excerpt}"
        );
        assert!(!excerpt.contains('\u{2500}'), "{excerpt}");
        assert!(!excerpt.contains('\u{2502}'), "{excerpt}");
        assert!(
            !excerpt.contains("Zq9x"),
            "the key's tail leaked: {excerpt}"
        );
        // Short lines of text that start with a rule character stay.
        let cli = "usage: spawn --help\n--help\n-1\n42\n---\n";
        assert_eq!(
            super::format_pane_tail_excerpt(cli).as_deref(),
            Some("usage: spawn --help | --help | -1 | 42")
        );
        // A run of `─` inside a line is kept to 3.
        assert_eq!(
            super::format_pane_tail_excerpt(
                "title \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500} end\n"
            )
            .as_deref(),
            Some("title \u{2500}\u{2500}\u{2500} end")
        );
    }

    #[test]
    fn a_blank_pane_has_no_tail_excerpt() {
        for pane in ["", "\n\n", "   \n\t\n \x1b \n"] {
            assert_eq!(super::format_pane_tail_excerpt(pane), None, "{pane:?}");
        }
    }

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
        delayed: Mutex<HashSet<TerminalName>>,
        keys: Mutex<Vec<String>>,
    }

    impl Counted {
        fn over(inner: Arc<MemoryStore>) -> Arc<Self> {
            Arc::new(Self {
                inner,
                captures: Mutex::new(0),
                delayed: Mutex::new(HashSet::new()),
                keys: Mutex::new(Vec::new()),
            })
        }

        fn captures(&self) -> usize {
            *self.captures.lock().expect("lock")
        }

        fn delay_next(&self, terminal: TerminalName) {
            self.delayed.lock().expect("delayed").insert(terminal);
        }

        fn keys(&self) -> Vec<String> {
            self.keys.lock().expect("keys").clone()
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
            self.keys
                .lock()
                .expect("keys")
                .extend(keys.iter().map(|key| (*key).to_string()));
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
            if self.delayed.lock().expect("delayed").remove(terminal) {
                return Ok(BUSY.to_string());
            }
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

    fn resumable_agent(number: u64) -> AgentId {
        AgentId::new(format!("agent_start_{number}")).expect("agent")
    }

    fn resumable_session(number: u64) -> SessionId {
        SessionId::new(format!("session_start_{number}")).expect("session")
    }

    fn submit_resumable_start(
        store: &Arc<MemoryStore>,
        number: u64,
        prompt: &str,
        pane: &str,
    ) -> CommandId {
        let id = store
            .submit(
                &Principal::new("tester").expect("principal"),
                &Submission {
                    agent_id: resumable_agent(number),
                    session_id: resumable_session(number),
                    idempotency_key: IdempotencyKey::new(format!("start-{number}")).expect("key"),
                    action: CommandAction::Start {
                        engine: ENGINE.to_string(),
                        prompt: prompt.to_string(),
                        parent_session_id: None,
                        from: None,
                        model: None,
                        charter: None,
                        model_label: None,
                    },
                },
            )
            .expect("submit");
        store.set_pane(
            &TerminalName::for_execution(&ExecutionId::for_command(id)),
            pane,
        );
        id
    }

    fn command_outcome(store: &Arc<MemoryStore>, id: CommandId) -> Option<Outcome> {
        store
            .read_global(GlobalPosition(0), 100_000)
            .expect("read")
            .into_iter()
            .find_map(|record| match record.event.kind {
                EventKind::CommandFinished {
                    command_id,
                    succeeded,
                    error,
                    ..
                } if command_id == id => Some(if succeeded {
                    Outcome::Succeeded
                } else {
                    Outcome::Failed(error.unwrap_or_default())
                }),
                _ => None,
            })
    }

    /// Drive released claims with no wall-clock sleep. The memory clock is
    /// deliberately bypassed by this test-only queue helper.
    fn drive_work(bridge: &mut Bridge, store: &Arc<MemoryStore>, mut done: impl FnMut() -> bool) {
        drive_until(bridge, store, |_| done());
    }

    fn drive_until(
        bridge: &mut Bridge,
        store: &Arc<MemoryStore>,
        mut done: impl FnMut(&Bridge) -> bool,
    ) {
        for attempt in 0..1_024 {
            if done(bridge) {
                return;
            }
            bridge.work().expect("work");
            store.expire_leases();
            if attempt < 4 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert!(
            done(bridge),
            "the condition was not reached in 1,024 claims"
        );
    }

    /// Block until every mount worker the Bridge started has returned (over
    /// a store that never gates a mount), so the next pass finds each one
    /// finished whatever the load. No clock advances while it waits; the
    /// real-time bound only turns a hang into a failure.
    fn finish_mount_workers(bridge: &Bridge) {
        let deadline = std::time::Instant::now() + Duration::from_mins(2);
        while bridge
            .mounting
            .values()
            .any(|pending| !pending.handle.is_finished())
        {
            assert!(
                std::time::Instant::now() < deadline,
                "a mount worker never returned"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    type UnmountHook = Box<dyn FnOnce() + Send>;

    /// A workspace store whose selected mounts wait at a test gate. It also
    /// counts the largest number of mount calls simultaneously in flight.
    struct GatedMounts {
        inner: Arc<MemoryStore>,
        held: Mutex<HashSet<BranchName>>,
        moved: Condvar,
        active: AtomicUsize,
        peak: AtomicUsize,
        calls: AtomicUsize,
        completed: AtomicUsize,
        fail_unmounts: AtomicBool,
        check_ready_after_gate: AtomicBool,
        preset_mounts: Mutex<HashMap<BranchName, WorkspaceId>>,
        canonical_aliases: Mutex<HashMap<WorkspaceId, WorkspaceId>>,
        before_unmount_ready: Mutex<Option<UnmountHook>>,
    }

    impl GatedMounts {
        fn new(inner: Arc<MemoryStore>) -> Self {
            Self {
                inner,
                held: Mutex::new(HashSet::new()),
                moved: Condvar::new(),
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
                completed: AtomicUsize::new(0),
                fail_unmounts: AtomicBool::new(false),
                check_ready_after_gate: AtomicBool::new(false),
                preset_mounts: Mutex::new(HashMap::new()),
                canonical_aliases: Mutex::new(HashMap::new()),
                before_unmount_ready: Mutex::new(None),
            }
        }

        fn on_unmount_before_ready(&self, hook: impl FnOnce() + Send + 'static) {
            *self.before_unmount_ready.lock().expect("hook") = Some(Box::new(hook));
        }

        fn check_ready_after_gate(&self, enabled: bool) {
            self.check_ready_after_gate.store(enabled, Ordering::SeqCst);
        }

        fn mount_with(
            &self,
            branch: &BranchName,
            ready: Option<&dyn Fn() -> Result<(), String>>,
        ) -> Result<WorkspaceId, WorkspaceError> {
            let check_after = self.check_ready_after_gate.load(Ordering::SeqCst);
            if !check_after
                && let Some(ready) = ready
                && let Err(why) = ready()
            {
                return Err(WorkspaceError::Refused {
                    code: crate::port::MOUNT_CANCELLED.to_string(),
                    message: why,
                });
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            let mut held = self.held.lock().expect("held");
            // Under the gate's lock, so a test waiting on the counters
            // cannot miss this call reaching the gate.
            self.moved.notify_all();
            while held.contains(branch) {
                held = self.moved.wait(held).expect("held");
            }
            drop(held);
            if check_after
                && let Some(ready) = ready
                && let Err(why) = ready()
            {
                self.finished();
                return Err(WorkspaceError::Refused {
                    code: crate::port::MOUNT_CANCELLED.to_string(),
                    message: why,
                });
            }
            let mounted = self
                .preset_mounts
                .lock()
                .expect("preset")
                .remove(branch)
                .map_or_else(|| self.inner.mount(branch), Ok);
            self.finished();
            mounted
        }

        /// A mount call left the gate: counted, and any test waiting on the
        /// counters told, under the gate's lock.
        fn finished(&self) {
            let _held = self.held.lock().expect("held");
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.completed.fetch_add(1, Ordering::SeqCst);
            self.moved.notify_all();
        }

        /// Block until the mount workers' counters (`calls` at the gate,
        /// `completed` past it) read as `until` says: a condvar wait on the
        /// workers themselves, never on the clock or a number of passes.
        /// The real-time bound only turns a hang into a failure.
        fn wait_for(&self, what: &str, until: impl Fn(&Self) -> bool) {
            let deadline = std::time::Instant::now() + Duration::from_mins(2);
            let mut held = self.held.lock().expect("held");
            while !until(self) {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    // Unlocked first: a panic holding the gate would poison
                    // it, and `ReleaseMounts`' drop would panic again.
                    drop(held);
                    panic!("the mount workers never reached: {what}");
                }
                held = self.moved.wait_timeout(held, left).expect("held").0;
            }
        }

        fn preset_mount(&self, branch: BranchName, workspace: WorkspaceId) {
            self.preset_mounts
                .lock()
                .expect("preset")
                .insert(branch, workspace);
        }

        fn canonical_alias(&self, alias: WorkspaceId, canonical: WorkspaceId) {
            self.canonical_aliases
                .lock()
                .expect("canonical")
                .insert(alias, canonical);
        }

        fn fail_unmounts(&self, fail: bool) {
            self.fail_unmounts.store(fail, Ordering::SeqCst);
        }

        fn hold(&self, branch: BranchName) {
            self.held.lock().expect("held").insert(branch);
        }

        fn release(&self, branch: &BranchName) {
            self.held.lock().expect("held").remove(branch);
            self.moved.notify_all();
        }

        fn release_all(&self) {
            self.held.lock().expect("held").clear();
            self.moved.notify_all();
        }
    }

    struct ReleaseMounts<'a>(&'a GatedMounts);

    impl Drop for ReleaseMounts<'_> {
        fn drop(&mut self) {
            self.0.release_all();
        }
    }

    impl WorkspaceStore for GatedMounts {
        fn project(&self) -> &str {
            self.inner.project()
        }

        fn ensure_branch(
            &self,
            branch: &BranchName,
            from: Option<&str>,
        ) -> Result<Branch, WorkspaceError> {
            self.inner.ensure_branch(branch, from)
        }

        fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError> {
            self.mount_with(branch, None)
        }

        fn mount_when(
            &self,
            branch: &BranchName,
            ready: &dyn Fn() -> Result<(), String>,
        ) -> Result<WorkspaceId, WorkspaceError> {
            self.mount_with(branch, Some(ready))
        }

        fn commit(&self, workspace: &WorkspaceId, message: &str) -> Result<Commit, WorkspaceError> {
            self.inner.commit(workspace, message)
        }

        fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
            if self.fail_unmounts.load(Ordering::SeqCst) {
                return Err(WorkspaceError::Unavailable(
                    "transient unmount failure".to_string(),
                ));
            }
            self.inner.unmount(workspace)
        }

        fn unmount_when(
            &self,
            workspace: &WorkspaceId,
            ready: &dyn Fn() -> Result<(), WorkspaceError>,
        ) -> Result<(), WorkspaceError> {
            let hook = self.before_unmount_ready.lock().expect("hook").take();
            if let Some(hook) = hook {
                hook();
            }
            ready()?;
            self.unmount(workspace)
        }

        fn is_mounted(&self, workspace: &WorkspaceId) -> bool {
            self.inner.is_mounted(workspace)
        }

        fn canonical(&self, workspace: &WorkspaceId) -> WorkspaceId {
            self.canonical_aliases
                .lock()
                .expect("canonical")
                .get(workspace)
                .cloned()
                .unwrap_or_else(|| self.inner.canonical(workspace))
        }

        fn notices(&self, workspace: &WorkspaceId) -> Vec<String> {
            self.inner.notices(workspace)
        }

        fn notices_told(&self, workspace: &WorkspaceId, notices: &[String]) {
            self.inner.notices_told(workspace, notices);
        }

        fn branch_head(
            &self,
            branch: &BranchName,
        ) -> Result<Option<crate::id::CommitId>, WorkspaceError> {
            self.inner.branch_head(branch)
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

        /// A Bridge that copies the transcripts into the store's archive.
        fn with_archive() -> Self {
            Self::build_with(Arc::new(MemoryWatch::new()), true, true)
        }

        fn config(watch: &Arc<MemoryWatch>) -> BridgeConfig {
            let mut config = BridgeConfig::new("tester");
            config.claude_projects = "/projects".to_string();
            config.screen_quiet = Duration::from_millis(500);
            config.screen_refresh = Duration::from_secs(30);
            config.watch = Some(watch.clone());
            config
        }

        /// The loop restarted: a new Bridge over the same store, archive and
        /// clock, rebuilt from the log (and the archive's records).
        fn restart(&mut self) {
            let watch = Arc::new(MemoryWatch::new());
            self.watch = watch.clone();
            self.bridge = Bridge::new(
                self.store.clone(),
                self.store.clone(),
                self.store.clone(),
                self.terminal.clone(),
                Self::config(&watch),
            )
            .with_clock(self.clock.clone())
            .with_transcript_archive(self.store.clone());
            self.bridge.recover().expect("recover");
        }

        fn build(watch: Arc<MemoryWatch>, settle: bool) -> Self {
            Self::build_with(watch, settle, false)
        }

        fn build_with(watch: Arc<MemoryWatch>, settle: bool, archive: bool) -> Self {
            let clock: Arc<ManualClock> = Arc::new(ManualClock::new(Timestamp(1_000)));
            let store = Arc::new(MemoryStore::new("shop", clock.clone()));
            let terminal = Arc::new(Counted {
                inner: store.clone(),
                captures: Mutex::new(0),
                delayed: Mutex::new(HashSet::new()),
                keys: Mutex::new(Vec::new()),
            });
            let mut bridge = Bridge::new(
                store.clone(),
                store.clone(),
                store.clone(),
                terminal.clone(),
                Self::config(&watch),
            )
            .with_clock(clock.clone());
            if archive {
                bridge = bridge.with_transcript_archive(store.clone());
            }
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
                            model_label: None,
                        },
                    },
                )
                .expect("submit");
            let mut first = Vec::new();
            let path = loop {
                if let Some(path) = store
                    .read_global(GlobalPosition(0), 100)
                    .expect("read")
                    .into_iter()
                    .find_map(|record| match record.event.kind {
                        EventKind::NativeSessionStarted {
                            transcript_path, ..
                        } => transcript_path,
                        _ => None,
                    })
                {
                    break path;
                }
                first.extend(bridge.tick().expect("the start's tick").warnings);
                // The mount runs on a worker thread: wait for it before the
                // next pass, so the number of passes, and so the clock this
                // world starts at, do not depend on how soon the thread ran.
                finish_mount_workers(&bridge);
                clock.advance(super::MOUNT_POLL_RETRY);
            };
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

    /// What a2f1ab397 records for [`claude_on_a_host_without_agent_readings_records_what_it_did_before`].
    const PINNED_SEQUENCE: [&str; 13] = [
        "command.accepted Gateway - cmd_1:accepted",
        "command.step Bridge - cmd_1:mounted",
        "execution.started Bridge exec_1 exec_1:started",
        "session.native_started Bridge exec_1 exec_1:native",
        "command.finished Gateway - cmd_1:finished",
        "screen.observed Bridge exec_1 - Ready \"❯\"",
        "tool.started Claude exec_1 toolu_1:started",
        "screen.observed Bridge exec_1 - Busy \"✽ Effecting… (esc to interrupt)\"",
        "screen.observed Bridge exec_1 - Permission \"❯ 1. main\"",
        "screen.observed Bridge exec_1 - Busy \"✽ Effecting… (esc to interrupt)\"",
        "screen.observed Bridge exec_1 - Busy \"✽ Effecting… (esc to interrupt)\"",
        "turn.stopped Claude exec_1 t1",
        "screen.observed Bridge exec_1 - Ready \"❯\"",
    ];

    /// Issue #91 P2 PR 2: on a host that reads no agent (tmux, in memory),
    /// Claude Code's execution records exactly what it did before the host's
    /// second witness existed: every event, in order, with its source and
    /// key, through a start, a turn that runs, a dialog, the turn's end and
    /// a busy screen recorded again after `screen_refresh`. The expected
    /// sequence is the one the base (a2f1ab397) records.
    #[test]
    fn claude_on_a_host_without_agent_readings_records_what_it_did_before() {
        let mut world = World::new();
        world.set_pane(BUSY);
        world.engine_writes(TOOL_LINE);
        for _ in 0..4 {
            world.advance(600);
            world.tick();
        }
        world.set_pane(DIALOG);
        for _ in 0..3 {
            world.advance(31_000);
            world.tick();
        }
        world.set_pane(BUSY);
        for _ in 0..3 {
            world.advance(31_000);
            world.tick();
        }
        world.engine_writes(TURN_LINE);
        world.set_pane(READY);
        for _ in 0..4 {
            world.advance(600);
            world.tick();
        }
        let sequence: Vec<String> = world
            .store
            .read_global(GlobalPosition(0), 1_000)
            .expect("read")
            .into_iter()
            .map(|record| {
                let state = match &record.event.kind {
                    EventKind::ScreenObserved { state, evidence } => {
                        format!(" {state:?} {evidence:?}")
                    }
                    _ => String::new(),
                };
                format!(
                    "{} {:?} {} {}{state}",
                    record.event.kind.name(),
                    record.event.source,
                    record
                        .event
                        .execution_id
                        .as_ref()
                        .map_or("-", |execution| execution.as_str()),
                    record.event.source_event_id.as_deref().unwrap_or("-"),
                )
            })
            .collect();
        let expected: &[&str] = &PINNED_SEQUENCE;
        assert_eq!(sequence, expected, "actual:\n{sequence:#?}");
    }

    /// The memory terminal with a host's agent readings (as Herdr gives
    /// them), one for every terminal.
    struct Witnessed {
        inner: Arc<MemoryStore>,
        reading: Mutex<Option<crate::port::AgentReading>>,
        asked: AtomicUsize,
        /// What [`TerminalHost::list_dirs`] answers for each directory
        /// (newest first, as a host orders them), and the directories asked.
        dirs: Mutex<HashMap<String, Vec<String>>>,
        listed: Mutex<Vec<String>>,
    }

    impl Witnessed {
        fn over(inner: Arc<MemoryStore>) -> Arc<Self> {
            Arc::new(Self {
                inner,
                reading: Mutex::new(None),
                asked: AtomicUsize::new(0),
                dirs: Mutex::new(HashMap::new()),
                listed: Mutex::new(Vec::new()),
            })
        }

        fn set(&self, reading: crate::port::AgentReading) {
            *self.reading.lock().expect("reading") = Some(reading);
        }

        fn set_dirs(&self, dir: &str, names: &[&str]) {
            self.dirs.lock().expect("dirs").insert(
                dir.to_string(),
                names.iter().map(|name| (*name).to_string()).collect(),
            );
        }
    }

    impl TerminalHost for Witnessed {
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
        fn server(&self) -> crate::port::ServerSeen {
            self.inner.server()
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
        fn list_dirs(
            &self,
            _workspace: &WorkspaceId,
            dir: &str,
        ) -> Result<Vec<String>, TerminalError> {
            self.listed.lock().expect("listed").push(dir.to_string());
            Ok(self
                .dirs
                .lock()
                .expect("dirs")
                .get(dir)
                .cloned()
                .unwrap_or_default())
        }
        fn agent(
            &self,
            _terminal: &TerminalName,
            _workspace: &WorkspaceId,
        ) -> Result<Option<crate::port::AgentReading>, TerminalError> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            Ok(self.reading.lock().expect("reading").clone())
        }
    }

    const AGY_RULE: &str = "──────────────────────────────";

    fn agy_idle() -> String {
        format!("Antigravity CLI 1.5.14\n{AGY_RULE}\n>\n{AGY_RULE}\n? for shortcuts\n")
    }

    fn agy_working() -> String {
        format!("⠋ Thinking\n{AGY_RULE}\n>\n{AGY_RULE}\nesc to cancel\n")
    }

    fn agy_reading(
        agent: Option<&str>,
        status: crate::port::AgentStatus,
        seq: u64,
    ) -> crate::port::AgentReading {
        crate::port::AgentReading {
            agent: agent.map(str::to_string),
            status,
            rule: None,
            blocker_visible: false,
            session: None,
            seq,
            manifest: Some("2026.10.05.1".to_string()),
        }
    }

    const AGY_HOME: &str = "/home/agent/.gemini/antigravity-cli";

    fn agy_config(model: Option<&str>) -> BridgeConfig {
        let mut config = BridgeConfig::new("tester");
        config.engines = vec![(
            "agy".to_string(),
            crate::driver::EngineHome {
                program: "/opt/agy/bin/agy".to_string(),
                sessions: AGY_HOME.to_string(),
                model: model.map(str::to_string),
            },
        )];
        config.engine_view = EngineViewPolicy::Required(Ok(Arc::new(FakeView::default())));
        config
    }

    /// A filesystem view that wraps every launch in a made-up `bwrap`
    /// (or refuses it), and keeps what it was asked.
    #[derive(Debug, Default)]
    struct FakeView {
        refuse: Option<String>,
        asked: std::sync::Mutex<Vec<(String, String, String)>>,
    }

    impl crate::port::EngineView for FakeView {
        fn name(&self) -> &'static str {
            "bwrap"
        }

        fn wrap(&self, launch: &ViewedLaunch<'_>) -> Result<Vec<String>, String> {
            self.asked.lock().expect("asked").push((
                launch.engine.to_string(),
                launch.workspace.to_string(),
                launch.engine_home.to_string(),
            ));
            if let Some(why) = &self.refuse {
                return Err(why.clone());
            }
            Ok([
                "/usr/bin/bwrap",
                "--unshare-pid",
                "--chdir",
                launch.workspace,
                "--",
            ]
            .map(str::to_string)
            .to_vec())
        }
    }

    /// agy has no deny rules of its own (`Guard::Sandbox`): with the view
    /// required (the default) and none on the host, its start is refused
    /// as `engine_unavailable` before anything is mounted or launched.
    #[test]
    fn agy_without_a_filesystem_view_is_refused_as_engine_unavailable() {
        let mut config = agy_config(Some("gemini-3.8-flash-low"));
        config.engine_view = EngineViewPolicy::unconfigured();
        let why = agy_failure(config, true);
        assert!(why.starts_with(ENGINE_UNAVAILABLE), "{why}");
        assert!(why.contains("filesystem view"), "{why}");
        let mut config = agy_config(Some("gemini-3.8-flash-low"));
        config.engine_view = EngineViewPolicy::Required(Err("bwrap is not installed".to_string()));
        let why = agy_failure(config, true);
        assert!(why.contains("bwrap is not installed"), "{why}");
    }

    /// A view that cannot be made for one launch fails that start as
    /// `engine_unavailable`: the terminal is never started.
    #[test]
    fn a_view_refused_at_the_launch_starts_nothing() {
        let mut config = agy_config(Some("gemini-3.8-flash-low"));
        config.engine_view = EngineViewPolicy::Required(Ok(Arc::new(FakeView {
            refuse: Some("the workspace holds a mask".to_string()),
            ..FakeView::default()
        })));
        let why = agy_failure(config, true);
        assert!(why.starts_with(ENGINE_UNAVAILABLE), "{why}");
        assert!(why.contains("the workspace holds a mask"), "{why}");
    }

    /// agy runs inside the view: the launch's program is the view's, and
    /// the runner with agy's own words follows its `--`; the view is asked
    /// for agy's workspace and home. Under `SPAWN_ENGINE_VIEW=off` it runs
    /// as before, with no view.
    #[test]
    fn agy_runs_inside_the_filesystem_view_unless_it_is_off() {
        for off in [false, true] {
            let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
            let store = Arc::new(MemoryStore::new("shop", clock.clone()));
            let host = Witnessed::over(store.clone());
            host.set(agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 1));
            let id = submit_agy_start_on(&store, "hello", Some("gemini-3.8-pro"));
            let view = Arc::new(FakeView::default());
            let mut config = agy_config(None);
            config.engine_view = if off {
                EngineViewPolicy::Off
            } else {
                EngineViewPolicy::Required(Ok(view.clone()))
            };
            let mut bridge = Bridge::new(
                store.clone(),
                store.clone(),
                store.clone(),
                host.clone(),
                config,
            )
            .with_clock(clock);
            drive_work(&mut bridge, &store, || {
                command_outcome(&store, id).is_some()
            });
            assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
            let launch = store.launches().pop().expect("launched");
            if off {
                assert_eq!(launch.program, "spawn");
                assert_eq!(launch.arguments[..3], ["run", "--", "/opt/agy/bin/agy"]);
                assert!(view.asked.lock().expect("asked").is_empty());
                continue;
            }
            assert_eq!(launch.program, "/usr/bin/bwrap");
            assert_eq!(
                launch.arguments[..7],
                [
                    "--unshare-pid",
                    "--chdir",
                    launch.cwd.as_str(),
                    "--",
                    "spawn",
                    "run",
                    "--"
                ]
            );
            assert_eq!(launch.arguments[7], "/opt/agy/bin/agy");
            assert!(!launch.arguments.iter().any(|word| word == "--new-session"));
            let asked = view.asked.lock().expect("asked").clone();
            assert_eq!(
                asked,
                vec![("agy".to_string(), launch.cwd.clone(), AGY_HOME.to_string())]
            );
        }
    }

    fn submit_agy_start(store: &Arc<MemoryStore>, prompt: &str) -> CommandId {
        submit_agy_start_on(store, prompt, None)
    }

    fn submit_agy_start_on(
        store: &Arc<MemoryStore>,
        prompt: &str,
        model_label: Option<&str>,
    ) -> CommandId {
        let id = store
            .submit(
                &Principal::new("tester").expect("principal"),
                &Submission {
                    agent_id: AgentId::new("agent_agy").expect("agent"),
                    session_id: SessionId::new("session_agy").expect("session"),
                    idempotency_key: IdempotencyKey::new("start-agy").expect("key"),
                    action: CommandAction::Start {
                        engine: "agy".to_string(),
                        prompt: prompt.to_string(),
                        parent_session_id: None,
                        from: None,
                        model: None,
                        charter: None,
                        model_label: model_label.map(str::to_string),
                    },
                },
            )
            .expect("submit");
        store.set_pane(
            &TerminalName::for_execution(&ExecutionId::for_command(id)),
            &agy_idle(),
        );
        id
    }

    fn agy_failure(config: BridgeConfig, witness: bool) -> String {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let terminals: Arc<dyn TerminalHost> = if witness {
            let host = Witnessed::over(store.clone());
            host.set(agy_reading(None, crate::port::AgentStatus::Unknown, 0));
            host
        } else {
            store.clone()
        };
        let id = submit_agy_start(&store, "hello");
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            terminals,
            config,
        )
        .with_clock(clock);
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, id).is_some()
        });
        assert!(
            store.launches().is_empty(),
            "a refused engine is never launched"
        );
        match command_outcome(&store, id) {
            Some(Outcome::Failed(why)) => why,
            other => panic!("refused, not {other:?}"),
        }
    }

    /// agy runs only where the host enables it (`SPAWN_ENGINES`), with its
    /// model named, on a host that reads agents (Herdr): landing it starts
    /// nothing anywhere else.
    #[test]
    fn agy_is_refused_unless_enabled_named_a_model_and_on_an_agent_host() {
        let why = agy_failure(BridgeConfig::new("tester"), true);
        assert!(why.contains("engine agy is not enabled"), "{why}");
        let why = agy_failure(agy_config(None), true);
        assert!(why.contains("SPAWN_AGY_MODEL"), "{why}");
        let why = agy_failure(agy_config(Some("gemini-3.8-flash-low")), false);
        assert!(
            why.contains("engine agy needs the herdr terminal backend"),
            "{why}"
        );
    }

    /// The start's `model_label` (the task's preset's) is agy's `--model`,
    /// on a host that names none (`SPAWN_AGY_MODEL` unset) or another.
    #[test]
    fn agy_starts_on_the_starts_model_label_over_the_hosts() {
        for host_model in [None, Some("gemini-3.8-flash-low")] {
            let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
            let store = Arc::new(MemoryStore::new("shop", clock.clone()));
            let host = Witnessed::over(store.clone());
            host.set(agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 1));
            let id = submit_agy_start_on(&store, "hello", Some("gemini-3.8-pro"));
            let mut bridge = Bridge::new(
                store.clone(),
                store.clone(),
                store.clone(),
                host.clone(),
                agy_config(host_model),
            )
            .with_clock(clock);
            drive_work(&mut bridge, &store, || {
                command_outcome(&store, id).is_some()
            });
            assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
            let launch = store.launches().pop().expect("launched");
            let arguments = launch.arguments.join(" ");
            assert!(
                arguments.contains("/opt/agy/bin/agy --model gemini-3.8-pro --dangerously"),
                "{host_model:?}: {arguments}"
            );
        }
    }

    /// agy's "is not recognized as a known model" line fails the start at
    /// once: it never waits on, nor types into, an engine on a model nobody
    /// chose.
    #[test]
    fn agys_unknown_model_line_fails_the_start() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let host = Witnessed::over(store.clone());
        host.set(agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 1));
        let id = submit_agy_start_on(&store, "hello", Some("gemini-9"));
        store.set_pane(
            &TerminalName::for_execution(&ExecutionId::for_command(id)),
            &format!(
                "Warning: model \"gemini-9\" is not recognized as a known model\n{}",
                agy_idle()
            ),
        );
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            host.clone(),
            agy_config(None),
        )
        .with_clock(clock);
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, id).is_some()
        });
        match command_outcome(&store, id) {
            Some(Outcome::Failed(why)) => {
                assert!(why.contains("is not recognized as a known model"), "{why}");
            }
            other => panic!("failed, not {other:?}"),
        }
        assert!(
            store.submitted_held().is_empty(),
            "nothing typed into an engine on an unchosen model"
        );
    }

    /// An agy session whose execution started on `gemini-3.8-pro` (its
    /// launch summary), bound to `conv-1`, and stopped.
    fn seed_stopped_agy(store: &Arc<MemoryStore>) -> (AgentId, SessionId) {
        let agent = AgentId::new("agent_agy").expect("agent");
        let session = SessionId::new("session_agy").expect("session");
        let earlier = ExecutionId::for_command(CommandId(900));
        let event = |key: &str, kind: EventKind| NewEvent {
            agent_id: agent.clone(),
            session_id: session.clone(),
            execution_id: Some(earlier.clone()),
            source: Source::Bridge,
            source_event_id: Some(format!("{earlier}:{key}")),
            kind,
        };
        store
            .append(event(
                "started",
                EventKind::ExecutionStarted {
                    engine: "agy".to_string(),
                    workspace: WorkspaceId::new("agent_agy:/workspaces/agent_agy")
                        .expect("workspace"),
                    command_id: CommandId(900),
                    fork: None,
                    model: crate::event::ModelProfile::Default,
                    transcript_from: None,
                    launch: Some(crate::event::LaunchSummary {
                        model: Some("gemini-3.8-pro".to_string()),
                        ..crate::event::LaunchSummary::default()
                    }),
                },
            ))
            .expect("started");
        store
            .append(event(
                "native",
                EventKind::NativeSessionStarted {
                    engine: "agy".to_string(),
                    native_session_id: "conv-1".to_string(),
                    transcript_path: None,
                    engine_version: None,
                },
            ))
            .expect("native");
        store
            .append(event(
                "ended",
                EventKind::ExecutionEnded {
                    cause: crate::event::EndCause::Stopped,
                    exit_code: Some(0),
                    signal: None,
                    evidence: None,
                },
            ))
            .expect("ended");
        (agent, session)
    }

    /// Review 589, must-fix 1: a resumed agy session runs the model label
    /// its last execution started on (its `execution.started` launch
    /// model), over the host's `SPAWN_AGY_MODEL` and with it unset: a
    /// revision stays on its preset's model.
    #[test]
    fn a_resumed_agy_session_runs_the_label_it_last_started_on() {
        for host_model in [None, Some("gemini-3.8-flash-low")] {
            let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
            let store = Arc::new(MemoryStore::new("shop", clock.clone()));
            let host = Witnessed::over(store.clone());
            host.set(agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 1));
            let (agent, session) = seed_stopped_agy(&store);
            let id = store
                .submit(
                    &Principal::new("tester").expect("principal"),
                    &Submission {
                        agent_id: agent.clone(),
                        session_id: session.clone(),
                        idempotency_key: IdempotencyKey::new("resume-agy").expect("key"),
                        action: CommandAction::Resume {
                            prompt: None,
                            model: None,
                        },
                    },
                )
                .expect("submit");
            store.set_pane(
                &TerminalName::for_execution(&ExecutionId::for_command(id)),
                &agy_idle(),
            );
            let mut bridge = Bridge::new(
                store.clone(),
                store.clone(),
                store.clone(),
                host.clone(),
                agy_config(host_model),
            )
            .with_clock(clock);
            drive_work(&mut bridge, &store, || {
                command_outcome(&store, id).is_some()
            });
            assert_eq!(
                command_outcome(&store, id),
                Some(Outcome::Succeeded),
                "{host_model:?}"
            );
            let launch = store.launches().pop().expect("launched");
            let arguments = launch.arguments.join(" ");
            assert!(
                arguments.contains("--conversation conv-1 --model gemini-3.8-pro --dangerously"),
                "{host_model:?}: {arguments}"
            );
            // The resume records the label it ran on, so the next resume
            // runs it too.
            let resumed = records(&store)
                .into_iter()
                .filter_map(|record| match record.event.kind {
                    EventKind::ExecutionStarted { launch, .. } => launch,
                    _ => None,
                })
                .next_back()
                .expect("a launch summary");
            assert_eq!(resumed.model.as_deref(), Some("gemini-3.8-pro"));
            assert_eq!(resumed.relay_url, None);
        }
    }

    fn records(store: &Arc<MemoryStore>) -> Vec<crate::event::EventRecord> {
        store.read_global(GlobalPosition(0), 10_000).expect("read")
    }

    /// agy names its own conversation: the host reports it, and it is
    /// bound only once its full transcript holds the prompt Spawn typed
    /// (its marker); then the transcript is read from its start, and the
    /// turn's end comes from it.
    #[allow(clippy::too_many_lines)] // one binding, refused case by case, then bound and read
    #[test]
    fn agy_binds_the_session_the_host_reports_and_reads_its_full_transcript() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let host = Witnessed::over(store.clone());
        host.set(agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 1));
        // An earlier conversation of the session would be another's lines.
        let id = submit_agy_start(&store, "hello");
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            host.clone(),
            agy_config(Some("gemini-3.8-flash-low")),
        )
        .with_clock(clock.clone());
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, id).is_some()
        });
        assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
        let launch = store.launches().pop().expect("launched");
        let workspace = launch.cwd.clone();
        let arguments = launch.arguments.join(" ");
        assert!(
            arguments.contains(
                "/opt/agy/bin/agy --model gemini-3.8-flash-low --dangerously-skip-permissions --settings"
            ),
            "{arguments}"
        );
        assert!(
            arguments.contains(&format!("\"trustedWorkspaces\":[\"{workspace}\"]")),
            "{arguments}"
        );
        let natives = |store: &Arc<MemoryStore>| {
            records(store)
                .into_iter()
                .filter_map(|record| match record.event.kind {
                    EventKind::NativeSessionStarted {
                        engine,
                        native_session_id,
                        transcript_path,
                        ..
                    } => Some((engine, native_session_id, transcript_path)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert!(natives(&store).is_empty(), "agy names its own session");

        // The host reports a session whose transcript is not there yet, and
        // then one whose prompt holds no marker of this execution's: nothing
        // is bound.
        let conversation = "11111111-2222-4333-8444-555555555555";
        let path =
            format!("{AGY_HOME}/brain/{conversation}/.system_generated/logs/transcript_full.jsonl");
        let mut reported = agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 2);
        reported.session = Some(conversation.to_string());
        host.set(reported.clone());
        bridge.poll().expect("poll");
        assert!(natives(&store).is_empty());
        let user = |text: &str| {
            format!(
                "{{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-11-25T14:25:59Z\",\"content\":{}}}\n",
                serde_json::to_string(&format!("<USER_REQUEST>\n{text}\n</USER_REQUEST>"))
                    .expect("json")
            )
        };
        store.append_file(&path, &user("someone else's prompt"));
        bridge.poll().expect("poll");
        assert!(natives(&store).is_empty());
        // A forged report (`pane.report_agent_session` is open to any client
        // of the server): its transcript quotes a marker of a later command,
        // which this session never typed (another session's). Never bound.
        store.replace_file(
            &path,
            user(&crate::message::marked(
                "forged",
                CommandId(id.0 + 50),
                true,
            ))
            .as_bytes(),
        );
        bridge.poll().expect("poll");
        bridge.poll().expect("poll");
        assert!(
            natives(&store).is_empty(),
            "a marker this session never typed"
        );
        // A foreign agent's session in the pane is never bound either.
        let mut foreign = reported.clone();
        foreign.agent = Some("claude".to_string());
        host.set(foreign);
        store.replace_file(
            &path,
            user(&crate::message::marked("hello", id, true)).as_bytes(),
        );
        bridge.poll().expect("poll");
        assert!(natives(&store).is_empty());
        // The engine's own, with the typed marker: bound, read from 0.
        host.set(reported);
        bridge.poll().expect("poll");
        assert_eq!(
            natives(&store),
            vec![(
                "agy".to_string(),
                conversation.to_string(),
                Some(path.clone())
            )]
        );
        let taken = |store: &Arc<MemoryStore>| {
            records(store)
                .into_iter()
                .filter_map(
                    |record| match (record.event.kind, record.event.source_event_id) {
                        (EventKind::MessageTaken { command_id }, Some(key)) => {
                            Some((command_id, key))
                        }
                        _ => None,
                    },
                )
                .collect::<Vec<_>>()
        };
        assert_eq!(
            taken(&store),
            vec![(id, format!("{conversation}:0:taken:{}", id.0))]
        );
        store.append_file(
            &path,
            "{\"step_index\":1,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"created_at\":\"2026-11-25T14:26:00Z\",\"model\":\"gemini-3.8-flash-low\",\"input_tokens\":10,\"output_tokens\":2,\"cache_read_tokens\":0,\"content\":\"hi\"}\n",
        );
        bridge.poll().expect("poll");
        bridge.poll().expect("poll");
        let stopped: Vec<_> = records(&store)
            .into_iter()
            .filter(|record| matches!(record.event.kind, EventKind::TurnStopped { .. }))
            .map(|record| record.event.source_event_id)
            .collect();
        assert_eq!(stopped, vec![Some(format!("{conversation}:1:stopped"))]);
        assert_eq!(taken(&store).len(), 1, "read once");
        // A new Bridge (a restart) folds the binding back and reads nothing twice.
        let mut again = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            host.clone(),
            agy_config(Some("gemini-3.8-flash-low")),
        )
        .with_clock(clock);
        again.recover().expect("recover");
        again.poll().expect("poll");
        assert_eq!(natives(&store).len(), 1);
        assert_eq!(taken(&store).len(), 1);
    }

    /// One `USER_INPUT` line of agy's full transcript holding `text`.
    fn agy_user_line(text: &str) -> String {
        format!(
            "{{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-11-25T14:25:59Z\",\"content\":{}}}\n",
            serde_json::to_string(&format!("<USER_REQUEST>\n{text}\n</USER_REQUEST>"))
                .expect("json")
        )
    }

    fn agy_transcript(conversation: &str) -> String {
        format!("{AGY_HOME}/brain/{conversation}/.system_generated/logs/transcript_full.jsonl")
    }

    fn agy_natives(store: &Arc<MemoryStore>) -> Vec<(String, String, Option<String>)> {
        records(store)
            .into_iter()
            .filter_map(|record| match record.event.kind {
                EventKind::NativeSessionStarted {
                    engine,
                    native_session_id,
                    transcript_path,
                    ..
                } => Some((engine, native_session_id, transcript_path)),
                _ => None,
            })
            .collect()
    }

    fn submit_agy(store: &Arc<MemoryStore>, key: &str, action: CommandAction) -> CommandId {
        store
            .submit(
                &Principal::new("tester").expect("principal"),
                &Submission {
                    agent_id: AgentId::new("agent_agy").expect("agent"),
                    session_id: SessionId::new("session_agy").expect("session"),
                    idempotency_key: IdempotencyKey::new(key).expect("key"),
                    action,
                },
            )
            .expect("submit")
    }

    /// F5 (O55): inside its filesystem view agy's hooks and the host's
    /// socket stay masked, so the host reports `agent=agy, session=None`.
    /// The Bridge then scans `<agy_home>/brain` itself, newest first, and
    /// binds the one conversation whose full transcript holds the marker
    /// Spawn typed into this execution, passing over a newer conversation of
    /// someone else's, one that quotes a marker this session never typed,
    /// one with no transcript and a name that is not a plain id. Bound, the
    /// transcript is read from its start (usage, the turn's end), and the
    /// session resumes on that conversation.
    #[allow(clippy::too_many_lines)] // one fallback binding, read, then resumed
    #[test]
    fn agy_with_no_reported_session_binds_the_brain_conversation_it_typed_into() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let host = Witnessed::over(store.clone());
        host.set(agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 1));
        let id = submit_agy_start(&store, "hello");
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            host.clone(),
            agy_config(Some("gemini-3.8-flash-low")),
        )
        .with_clock(clock.clone());
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, id).is_some()
        });
        assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
        bridge.poll().expect("poll");
        assert!(agy_natives(&store).is_empty(), "no conversation yet");

        let brain = format!("{AGY_HOME}/brain");
        let conversation = "11111111-2222-4333-8444-555555555555";
        let stale = "99999999-0000-4000-8000-000000000000";
        let forged = "88888888-0000-4000-8000-000000000000";
        store.append_file(
            &agy_transcript(stale),
            &agy_user_line("someone else's prompt"),
        );
        store.append_file(
            &agy_transcript(forged),
            &agy_user_line(&crate::message::marked(
                "forged",
                CommandId(id.0 + 50),
                true,
            )),
        );
        store.append_file(
            &format!("{AGY_HOME}/evil/.system_generated/logs/transcript_full.jsonl"),
            &agy_user_line(&crate::message::marked("hello", id, true)),
        );
        // Typed into this execution, but its transcript not written yet.
        host.set_dirs(
            &brain,
            &[stale, "..", "../evil", forged, "empty-0", conversation],
        );
        clock.advance(Duration::from_secs(3));
        bridge.poll().expect("poll");
        let scans = host.listed.lock().expect("listed").len();
        assert!(scans > 0, "the brain was scanned");
        // At most one scan per interval.
        bridge.poll().expect("poll");
        bridge.poll().expect("poll");
        assert_eq!(
            host.listed.lock().expect("listed").len(),
            scans,
            "throttled"
        );
        assert!(agy_natives(&store).is_empty(), "no transcript of ours yet");
        assert!(
            host.listed
                .lock()
                .expect("listed")
                .iter()
                .all(|dir| *dir == brain),
            "only agy's brain is scanned"
        );
        let path = agy_transcript(conversation);
        store.append_file(
            &path,
            &agy_user_line(&crate::message::marked("hello", id, true)),
        );
        clock.advance(Duration::from_secs(3));
        bridge.poll().expect("poll");
        assert_eq!(
            agy_natives(&store),
            vec![(
                "agy".to_string(),
                conversation.to_string(),
                Some(path.clone())
            )]
        );
        store.append_file(
            &path,
            "{\"step_index\":1,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"created_at\":\"2026-11-25T14:26:00Z\",\"model\":\"gemini-3.8-flash-low\",\"input_tokens\":10,\"output_tokens\":2,\"cache_read_tokens\":0,\"content\":\"hi\"}\n",
        );
        bridge.poll().expect("poll");
        bridge.poll().expect("poll");
        let usage = records(&store)
            .into_iter()
            .filter(|record| matches!(record.event.kind, EventKind::ModelUsage { .. }))
            .count();
        assert_eq!(usage, 1, "the transcript's usage is ingested");
        let stopped: Vec<_> = records(&store)
            .into_iter()
            .filter(|record| matches!(record.event.kind, EventKind::TurnStopped { .. }))
            .map(|record| record.event.source_event_id)
            .collect();
        assert_eq!(stopped, vec![Some(format!("{conversation}:1:stopped"))]);
        // Nothing of the decoys was bound or read, and it stays one binding.
        bridge.poll().expect("poll");
        assert_eq!(agy_natives(&store).len(), 1);

        // Stopped, the session resumes on the conversation found.
        let stop = submit_agy(&store, "stop-agy", CommandAction::Stop);
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, stop).is_some()
        });
        assert_eq!(command_outcome(&store, stop), Some(Outcome::Succeeded));
        let resume = submit_agy(
            &store,
            "resume-agy",
            CommandAction::Resume {
                prompt: None,
                model: None,
            },
        );
        store.set_pane(
            &TerminalName::for_execution(&ExecutionId::for_command(resume)),
            &agy_idle(),
        );
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, resume).is_some()
        });
        assert_eq!(command_outcome(&store, resume), Some(Outcome::Succeeded));
        let arguments = store
            .launches()
            .pop()
            .expect("launched")
            .arguments
            .join(" ");
        assert!(
            arguments.contains(&format!("--conversation {conversation} --model")),
            "{arguments}"
        );
    }

    /// F5's anti-forgery: a conversation below agy's brain binds only on a
    /// marker typed into this session for this execution. Conversations
    /// that hold someone else's prompt, a marker of a command never typed,
    /// or a marker of an earlier execution's command, bind nothing; nor
    /// does this execution's own conversation while the host says another
    /// agent holds the pane.
    #[test]
    fn agy_brain_conversations_without_this_executions_marker_never_bind() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let host = Witnessed::over(store.clone());
        host.set(agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 1));
        let id = submit_agy_start(&store, "hello");
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            host.clone(),
            agy_config(Some("gemini-3.8-flash-low")),
        )
        .with_clock(clock.clone());
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, id).is_some()
        });
        assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
        let brain = format!("{AGY_HOME}/brain");
        let decoys = [
            (
                "aaaaaaaa-0000-4000-8000-000000000001",
                agy_user_line("someone else's prompt"),
            ),
            (
                "aaaaaaaa-0000-4000-8000-000000000002",
                agy_user_line(&crate::message::marked(
                    "forged",
                    CommandId(id.0 + 50),
                    true,
                )),
            ),
            (
                "aaaaaaaa-0000-4000-8000-000000000003",
                agy_user_line(&crate::message::marked(
                    "earlier",
                    CommandId(id.0 - 1),
                    true,
                )),
            ),
            // This execution's marker, but not in a prompt line.
            (
                "aaaaaaaa-0000-4000-8000-000000000004",
                format!(
                    "{{\"step_index\":1,\"type\":\"PLANNER_RESPONSE\",\"content\":{}}}\n",
                    serde_json::to_string(&crate::message::marked("quoted", id, true))
                        .expect("json")
                ),
            ),
        ];
        for (conversation, line) in &decoys {
            store.append_file(&agy_transcript(conversation), line);
        }
        let names: Vec<&str> = decoys.iter().map(|(name, _)| *name).collect();
        host.set_dirs(&brain, &names);
        for _ in 0..4 {
            clock.advance(Duration::from_secs(3));
            bridge.poll().expect("poll");
        }
        assert!(agy_natives(&store).is_empty(), "no decoy binds");
        assert!(
            !host.listed.lock().expect("listed").is_empty(),
            "the brain was scanned"
        );

        // Its own conversation, while the host says Claude Code holds the
        // pane: nothing is known, nothing is scanned or bound.
        let ours = "bbbbbbbb-0000-4000-8000-000000000000";
        store.append_file(
            &agy_transcript(ours),
            &agy_user_line(&crate::message::marked("hello", id, true)),
        );
        let mut all = names.clone();
        all.push(ours);
        host.set_dirs(&brain, &all);
        host.set(agy_reading(
            Some("claude"),
            crate::port::AgentStatus::Idle,
            2,
        ));
        host.listed.lock().expect("listed").clear();
        for _ in 0..4 {
            clock.advance(Duration::from_secs(3));
            bridge.poll().expect("poll");
        }
        assert!(
            agy_natives(&store).is_empty(),
            "a foreign agent's pane binds nothing"
        );
        assert!(
            host.listed.lock().expect("listed").is_empty(),
            "nor is scanned"
        );
        // The engine's own pane again: its conversation binds.
        host.set(agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 3));
        clock.advance(Duration::from_secs(3));
        bridge.poll().expect("poll");
        assert_eq!(
            agy_natives(&store),
            vec![(
                "agy".to_string(),
                ours.to_string(),
                Some(agy_transcript(ours))
            )]
        );
    }

    /// A prompt line of agy's full transcript written at `created_at`.
    fn agy_user_line_at(text: &str, created_at: &str) -> String {
        format!(
            "{{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":{},\"content\":{}}}\n",
            serde_json::to_string(created_at).expect("json"),
            serde_json::to_string(&format!("<USER_REQUEST>\n{text}\n</USER_REQUEST>"))
                .expect("json")
        )
    }

    /// The store's clock at 2026-11-26T10:00:00Z, so a transcript's
    /// `created_at` can be older than an execution's start.
    fn d1_clock() -> Arc<ManualClock> {
        Arc::new(ManualClock::new(Timestamp(
            rfc3339_ms("2026-11-26T10:00:00Z").expect("time"),
        )))
    }

    fn d1_bridge(
        clock: &Arc<ManualClock>,
        store: &Arc<MemoryStore>,
        host: &Arc<Witnessed>,
    ) -> Bridge {
        Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            host.clone(),
            agy_config(Some("gemini-3.8-flash-low")),
        )
        .with_clock(clock.clone())
    }

    fn d1_polls(clock: &ManualClock, bridge: &mut Bridge) {
        for _ in 0..4 {
            clock.advance(Duration::from_secs(3));
            bridge.poll().expect("poll");
        }
    }

    fn submit_agy_start_in(store: &Arc<MemoryStore>, session: &str) -> CommandId {
        let id = store
            .submit(
                &Principal::new("tester").expect("principal"),
                &Submission {
                    agent_id: AgentId::new("agent_agy").expect("agent"),
                    session_id: SessionId::new(session).expect("session"),
                    idempotency_key: IdempotencyKey::new(format!("start-{session}"))
                        .expect("key"),
                    action: CommandAction::Start {
                        engine: "agy".to_string(),
                        prompt: "hello".to_string(),
                        parent_session_id: None,
                        from: None,
                        model: None,
                        charter: None,
                        model_label: None,
                    },
                },
            )
            .expect("submit");
        store.set_pane(
            &TerminalName::for_execution(&ExecutionId::for_command(id)),
            &agy_idle(),
        );
        id
    }

    /// D1 (O55 C3): a conversation in agy's shared brain from another store
    /// or an older run may quote this execution's `[spawn message #N]`
    /// (only a store-local id). Its prompt, older than the execution's
    /// `execution.started`, never binds: not by the Bridge that started it,
    /// nor after a restart (which has no list of what was there before the
    /// start). A conversation whose prompt was written since does.
    #[test]
    fn agy_stale_brain_conversation_with_a_colliding_marker_never_binds() {
        let clock = d1_clock();
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let host = Witnessed::over(store.clone());
        host.set(agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 1));
        let id = submit_agy_start(&store, "hello");
        let mut bridge = d1_bridge(&clock, &store, &host);
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, id).is_some()
        });
        assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
        let brain = format!("{AGY_HOME}/brain");
        let stale = "cccccccc-0000-4000-8000-000000000001";
        store.append_file(
            &agy_transcript(stale),
            &agy_user_line_at(
                &crate::message::marked("old", id, true),
                "2026-11-26T09:00:00Z",
            ),
        );
        // Just past the skew tolerance, with an offset.
        let skewed = "cccccccc-0000-4000-8000-000000000002";
        store.append_file(
            &agy_transcript(skewed),
            &agy_user_line_at(
                &crate::message::marked("old", id, true),
                "2026-11-26T11:59:54.999+02:00",
            ),
        );
        // No time at all: fails closed.
        let timeless = "cccccccc-0000-4000-8000-000000000003";
        store.append_file(
            &agy_transcript(timeless),
            &format!(
                "{{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":{}}}\n",
                serde_json::to_string(&crate::message::marked("old", id, true)).expect("json")
            ),
        );
        host.set_dirs(&brain, &[stale, skewed, timeless]);
        d1_polls(&clock, &mut bridge);
        assert!(agy_natives(&store).is_empty(), "a stale conversation binds nothing");

        // Restarted: what was there before the start is not known, and the
        // stale conversation still binds nothing.
        let mut bridge = d1_bridge(&clock, &store, &host);
        bridge.recover().expect("recover");
        d1_polls(&clock, &mut bridge);
        assert!(agy_natives(&store).is_empty(), "nor after a restart");

        // Written since the start (within the skew, in another offset).
        let fresh = "cccccccc-0000-4000-8000-000000000004";
        store.append_file(
            &agy_transcript(fresh),
            &agy_user_line_at(
                &crate::message::marked("hello", id, true),
                "2026-11-26T04:59:57-05:00",
            ),
        );
        host.set_dirs(&brain, &[stale, skewed, timeless, fresh]);
        d1_polls(&clock, &mut bridge);
        assert_eq!(
            agy_natives(&store),
            vec![(
                "agy".to_string(),
                fresh.to_string(),
                Some(agy_transcript(fresh))
            )]
        );
    }

    /// D1: a conversation that was below agy's brain before a fresh start
    /// launched the engine is none of this execution's: the scan never binds
    /// it, even with this execution's marker in a prompt written since.
    #[test]
    fn agy_brain_conversation_there_before_the_start_never_binds() {
        let clock = d1_clock();
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let host = Witnessed::over(store.clone());
        host.set(agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 1));
        let brain = format!("{AGY_HOME}/brain");
        let before = "dddddddd-0000-4000-8000-000000000001";
        host.set_dirs(&brain, &[before]);
        let id = submit_agy_start(&store, "hello");
        let mut bridge = d1_bridge(&clock, &store, &host);
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, id).is_some()
        });
        assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
        store.append_file(
            &agy_transcript(before),
            &agy_user_line_at(
                &crate::message::marked("hello", id, true),
                "2026-11-26T10:00:10Z",
            ),
        );
        d1_polls(&clock, &mut bridge);
        assert!(
            agy_natives(&store).is_empty(),
            "a conversation there before the start binds nothing"
        );
        let ours = "dddddddd-0000-4000-8000-000000000002";
        store.append_file(
            &agy_transcript(ours),
            &agy_user_line_at(
                &crate::message::marked("hello", id, true),
                "2026-11-26T10:00:20Z",
            ),
        );
        host.set_dirs(&brain, &[ours, before]);
        d1_polls(&clock, &mut bridge);
        assert_eq!(
            agy_natives(&store),
            vec![(
                "agy".to_string(),
                ours.to_string(),
                Some(agy_transcript(ours))
            )]
        );
    }

    /// D1: a native session one session of the store is bound to is never
    /// bound by another, though its transcript quotes the other's marker
    /// (command ids collide across stores) and the host reports it for the
    /// other's pane.
    #[test]
    fn agy_native_session_bound_by_another_session_never_binds_again() {
        let clock = d1_clock();
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let host = Witnessed::over(store.clone());
        host.set(agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 1));
        let brain = format!("{AGY_HOME}/brain");
        let first = submit_agy_start_in(&store, "session_agy_a");
        let mut bridge = d1_bridge(&clock, &store, &host);
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, first).is_some()
        });
        assert_eq!(command_outcome(&store, first), Some(Outcome::Succeeded));
        let taken = "eeeeeeee-0000-4000-8000-000000000001";
        store.append_file(
            &agy_transcript(taken),
            &agy_user_line_at(
                &crate::message::marked("hello", first, true),
                "2026-11-26T10:00:05Z",
            ),
        );
        host.set_dirs(&brain, &[taken]);
        d1_polls(&clock, &mut bridge);
        assert_eq!(agy_natives(&store).len(), 1, "the first session binds");

        // The second session starts with nothing listed before its start.
        host.set_dirs(&brain, &[]);
        let second = submit_agy_start_in(&store, "session_agy_b");
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, second).is_some()
        });
        assert_eq!(command_outcome(&store, second), Some(Outcome::Succeeded));
        store.append_file(
            &agy_transcript(taken),
            &agy_user_line_at(
                &crate::message::marked("hello", second, true),
                "2026-11-26T10:01:00Z",
            ),
        );
        host.set_dirs(&brain, &[taken]);
        let mut reported = agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 2);
        reported.session = Some(taken.to_string());
        host.set(reported);
        d1_polls(&clock, &mut bridge);
        // And after a restart.
        let mut bridge = d1_bridge(&clock, &store, &host);
        bridge.recover().expect("recover");
        d1_polls(&clock, &mut bridge);
        let natives: Vec<String> = agy_natives(&store)
            .into_iter()
            .map(|(_, native, _)| native)
            .collect();
        assert_eq!(natives, vec![taken.to_string()], "bound once only");

        let ours = "eeeeeeee-0000-4000-8000-000000000002";
        store.append_file(
            &agy_transcript(ours),
            &agy_user_line_at(
                &crate::message::marked("hello", second, true),
                "2026-11-26T10:01:10Z",
            ),
        );
        host.set(agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 3));
        host.set_dirs(&brain, &[ours, taken]);
        d1_polls(&clock, &mut bridge);
        let natives: Vec<String> = agy_natives(&store)
            .into_iter()
            .map(|(_, native, _)| native)
            .collect();
        assert_eq!(natives, vec![taken.to_string(), ours.to_string()]);
    }

    #[test]
    fn rfc3339_ms_reads_utc_fractions_and_offsets() {
        assert_eq!(rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_ms("2026-11-25T14:25:59Z"), Some(1_795_616_759_000));
        assert_eq!(rfc3339_ms("2026-11-25T14:25:59.5Z"), Some(1_795_616_759_500));
        assert_eq!(
            rfc3339_ms("2026-11-25T14:25:59.123456789Z"),
            Some(1_795_616_759_123)
        );
        assert_eq!(rfc3339_ms("2026-11-25t14:25:59z"), Some(1_795_616_759_000));
        assert_eq!(
            rfc3339_ms("2026-11-25T22:25:59+08:00"),
            Some(1_795_616_759_000)
        );
        assert_eq!(
            rfc3339_ms("2026-11-25T09:25:59.250-05:00"),
            Some(1_795_616_759_250)
        );
        assert_eq!(rfc3339_ms("2024-02-29T00:00:00Z"), Some(1_709_164_800_000));
        assert_eq!(rfc3339_ms("2000-03-01T00:00:00Z"), Some(951_868_800_000));
    }

    #[test]
    fn rfc3339_ms_refuses_malformed_times() {
        for text in [
            "",
            "2026-11-25",
            "2026-11-25T14:25:59",
            "2026-11-25T14:25:59+0800",
            "2026-11-25T14:25:59.Z",
            "2026-11-25T14:25:59 Z",
            "2026-13-01T00:00:00Z",
            "2026-00-01T00:00:00Z",
            "2023-02-29T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-11-25T24:00:00Z",
            "2026-11-25T14:60:00Z",
            "2026-11-25T14:25:59+24:00",
            "2026/11/25T14:25:59Z",
            "+026-11-25T14:25:59Z",
            "2026-11-25T14:25:59Zjunk",
            "2026-11-25T14:25:59.12a3Z",
            "2026-11-25T14:25:5\u{e9}Z",
        ] {
            assert_eq!(rfc3339_ms(text), None, "{text:?}");
        }
    }

    fn screens_with_keys(store: &Arc<MemoryStore>) -> Vec<(ScreenState, Option<String>)> {
        records(store)
            .into_iter()
            .filter_map(|record| match record.event.kind {
                EventKind::ScreenObserved { state, .. } => {
                    Some((state, record.event.source_event_id))
                }
                _ => None,
            })
            .collect()
    }

    /// A Bridge over the agy start of [`submit_agy_start`] with a witness at
    /// `seq` 1, started until the start is done and the ready pane keyed.
    fn n12_rig() -> (
        Arc<ManualClock>,
        Arc<MemoryStore>,
        Arc<Witnessed>,
        TerminalName,
        Bridge,
    ) {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        store.set_terminal_server(crate::port::ServerSeen::Answers(Some(
            crate::port::TerminalServer {
                pid: 77,
                started_at_ms: 1_700,
            },
        )));
        let host = Witnessed::over(store.clone());
        host.set(agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 1));
        let id = submit_agy_start(&store, "");
        let mut bridge = n12_bridge(&clock, &store, &host);
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, id).is_some()
        });
        bridge.poll().expect("poll");
        let terminal = TerminalName::for_execution(&ExecutionId::for_command(id));
        (clock, store, host, terminal, bridge)
    }

    fn n12_bridge(
        clock: &Arc<ManualClock>,
        store: &Arc<MemoryStore>,
        host: &Arc<Witnessed>,
    ) -> Bridge {
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            host.clone(),
            agy_config(Some("gemini-3.8-flash-low")),
        )
        .with_clock(clock.clone());
        bridge.recover().expect("recover");
        bridge
    }

    fn agy_drafted() -> String {
        format!(
            "Antigravity CLI 1.5.14\n{AGY_RULE}\n> half a thought\n{AGY_RULE}\n? for shortcuts\n"
        )
    }

    /// Carry list N12 (a): ready (keyed by `seq` 1), then a draft under the
    /// same `seq` (unkeyed), then a restart and the pane ready again: the
    /// log's last screen is `Ready`, recorded unkeyed, never a keyed repeat
    /// the log refuses (which left the log at `Drafted` while the pane was
    /// ready).
    #[test]
    fn n12_ready_drafted_restart_ready_leaves_the_log_at_ready() {
        let (clock, store, host, terminal, mut bridge) = n12_rig();
        store.set_pane(&terminal, &agy_drafted());
        bridge.poll().expect("poll");
        bridge.poll().expect("poll");
        assert_eq!(
            screens_with_keys(&store),
            vec![
                (ScreenState::Ready, Some("exec_1:hs:77.1700:1".to_string())),
                (ScreenState::Drafted, None),
            ]
        );
        drop(bridge);
        let mut restarted = n12_bridge(&clock, &store, &host);
        store.set_pane(&terminal, &agy_idle());
        restarted.poll().expect("poll");
        restarted.poll().expect("poll");
        let screens = screens_with_keys(&store);
        assert_eq!(
            screens.last(),
            Some(&(ScreenState::Ready, None)),
            "{screens:?}"
        );
        assert_eq!(screens.len(), 3, "{screens:?}");
    }

    /// Carry list N12 (b): a restart over an unchanged pane records nothing
    /// new, and never a keyed duplicate.
    #[test]
    fn n12_a_restart_over_an_unchanged_pane_adds_no_keyed_duplicate() {
        let (clock, store, host, _terminal, bridge) = n12_rig();
        let before = screens_with_keys(&store);
        drop(bridge);
        let mut restarted = n12_bridge(&clock, &store, &host);
        restarted.poll().expect("poll");
        restarted.poll().expect("poll");
        let after = screens_with_keys(&store);
        assert!(after.len() <= before.len() + 1, "{after:?}");
        let keyed: Vec<&Option<String>> = after
            .iter()
            .map(|(_, key)| key)
            .filter(|key| key.is_some())
            .collect();
        let mut unique = keyed.clone();
        unique.dedup();
        assert_eq!(keyed, unique, "no keyed duplicate: {after:?}");
        assert_eq!(after, before, "nothing changed on the pane: {after:?}");
    }

    /// Carry list N12 (c): a new `seq` after a restart is keyed again.
    #[test]
    fn n12_a_new_seq_after_a_restart_is_keyed_again() {
        let (clock, store, host, terminal, bridge) = n12_rig();
        drop(bridge);
        let mut restarted = n12_bridge(&clock, &store, &host);
        store.set_pane(&terminal, &agy_working());
        host.set(agy_reading(
            Some("agy"),
            crate::port::AgentStatus::Working,
            2,
        ));
        restarted.poll().expect("poll");
        assert_eq!(
            screens_with_keys(&store).last(),
            Some(&(ScreenState::Busy, Some("exec_1:hs:77.1700:2".to_string())))
        );
    }

    /// With the host's reading, one capture that agrees with it is a fact,
    /// keyed by the host's server and reading; a busy screen recorded again
    /// after `screen_refresh` is unkeyed, so the log's unique keys never
    /// swallow it; another agent in the pane records nothing.
    #[test]
    fn a_host_reading_that_agrees_records_at_once_keyed_by_its_seq() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        store.set_terminal_server(crate::port::ServerSeen::Answers(Some(
            crate::port::TerminalServer {
                pid: 77,
                started_at_ms: 1_700,
            },
        )));
        let host = Witnessed::over(store.clone());
        host.set(agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 1));
        let id = submit_agy_start(&store, "");
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            host.clone(),
            agy_config(Some("gemini-3.8-flash-low")),
        )
        .with_clock(clock.clone());
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, id).is_some()
        });
        let terminal = TerminalName::for_execution(&ExecutionId::for_command(id));
        bridge.poll().expect("poll");
        let screens = screens_with_keys(&store);
        assert_eq!(
            screens,
            vec![(ScreenState::Ready, Some("exec_1:hs:77.1700:1".to_string()))],
            "one agreeing capture is the fact"
        );
        store.set_pane(&terminal, &agy_working());
        host.set(agy_reading(
            Some("agy"),
            crate::port::AgentStatus::Working,
            2,
        ));
        bridge.poll().expect("poll");
        clock.advance(Duration::from_secs(31));
        bridge.poll().expect("poll");
        let screens = screens_with_keys(&store);
        assert_eq!(
            screens[1..],
            [
                (ScreenState::Busy, Some("exec_1:hs:77.1700:2".to_string())),
                (ScreenState::Busy, None),
            ]
        );
        // The host idle by its fallback while the pane shows no prompt: no
        // Ready, whatever the host says.
        store.set_pane(&terminal, "╭──────╮\n│ new! │\n╰──────╯\n");
        host.set(agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 3));
        bridge.poll().expect("poll");
        bridge.poll().expect("poll");
        assert_eq!(screens_with_keys(&store).len(), 3);
        // Another agent in the pane: nothing, even at a prompt.
        store.set_pane(&terminal, &agy_idle());
        host.set(agy_reading(
            Some("claude"),
            crate::port::AgentStatus::Idle,
            4,
        ));
        bridge.poll().expect("poll");
        bridge.poll().expect("poll");
        assert_eq!(screens_with_keys(&store).len(), 3);
        // The engine's again, at its prompt.
        host.set(agy_reading(Some("agy"), crate::port::AgentStatus::Idle, 5));
        bridge.poll().expect("poll");
        assert_eq!(
            screens_with_keys(&store)[3],
            (ScreenState::Ready, Some("exec_1:hs:77.1700:5".to_string()))
        );
        assert!(host.asked.load(Ordering::SeqCst) > 0);
    }

    /// Claude Code's screens under Herdr are not captured yet against
    /// Herdr's claude manifest (design §9.2): on a host that reads agents,
    /// Claude Code is not merged with the host's reading, nor keyed by it,
    /// and the host is never asked. Its screen goes by two readings in a
    /// row, as on tmux.
    #[test]
    fn claude_on_an_agent_host_stays_on_its_own_readings_for_now() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        store.set_terminal_server(crate::port::ServerSeen::Answers(Some(
            crate::port::TerminalServer {
                pid: 77,
                started_at_ms: 1_700,
            },
        )));
        let host = Witnessed::over(store.clone());
        host.set(agy_reading(
            Some("claude"),
            crate::port::AgentStatus::Idle,
            1,
        ));
        let id = submit_resumable_start(&store, 1, "", READY);
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            host.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock.clone());
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, id).is_some()
        });
        assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
        let terminal = TerminalName::for_execution(&ExecutionId::for_command(id));
        store.set_pane(&terminal, BUSY);
        host.set(agy_reading(
            Some("claude"),
            crate::port::AgentStatus::Working,
            2,
        ));
        bridge.poll().expect("poll");
        let busy = |store: &Arc<MemoryStore>| {
            screens_with_keys(store)
                .into_iter()
                .filter(|(state, _)| *state == ScreenState::Busy)
                .collect::<Vec<_>>()
        };
        assert!(
            busy(&store).is_empty(),
            "one reading is no fact, whatever the host says"
        );
        bridge.poll().expect("poll");
        assert_eq!(busy(&store), vec![(ScreenState::Busy, None)], "unkeyed");
        assert!(
            screens_with_keys(&store)
                .iter()
                .all(|(_, key)| key.is_none()),
            "{:?}",
            screens_with_keys(&store)
        );
        assert_eq!(
            host.asked.load(Ordering::SeqCst),
            0,
            "the host is not asked"
        );
    }

    /// The merge, case by case: a state is taken at once only where both
    /// witnesses agree.
    #[test]
    fn the_witnesses_merge_only_where_they_agree() {
        use super::{Merged, merge_witness};
        use crate::port::AgentStatus::{Blocked, Idle, Unknown, Working};
        use crate::screen::ScreenReading;
        let driver = crate::driver::driver("agy").expect("agy");
        let screen = |state: ScreenState| ScreenReading {
            state,
            evidence: "e".to_string(),
        };
        let reading = |status, rule: Option<&str>, blocker: bool| crate::port::AgentReading {
            rule: rule.map(str::to_string),
            blocker_visible: blocker,
            ..agy_reading(Some("agy"), status, 9)
        };
        let agrees = |state| Merged::Agrees(state, "e".to_string());
        let cases = [
            (
                reading(Working, None, false),
                ScreenState::Busy,
                agrees(ScreenState::Busy),
            ),
            (
                reading(Working, None, false),
                ScreenState::Ready,
                Merged::Own,
            ),
            (
                reading(Blocked, Some("permission_prompt"), true),
                ScreenState::Permission,
                agrees(ScreenState::Permission),
            ),
            (
                reading(Blocked, None, true),
                ScreenState::Permission,
                Merged::Own,
            ),
            (
                reading(Blocked, Some("permission_prompt"), false),
                ScreenState::Unknown,
                Merged::Own,
            ),
            (
                reading(Idle, None, false),
                ScreenState::Ready,
                agrees(ScreenState::Ready),
            ),
            (
                reading(Idle, None, false),
                ScreenState::Error,
                agrees(ScreenState::Error),
            ),
            (
                reading(Idle, None, false),
                ScreenState::Unknown,
                Merged::Own,
            ),
            (reading(Idle, None, false), ScreenState::Busy, Merged::Own),
            (
                reading(Unknown, None, false),
                ScreenState::Ready,
                Merged::Own,
            ),
        ];
        for (witness, state, merged) in cases {
            assert_eq!(
                merge_witness(driver, &witness, &screen(state)),
                merged,
                "{witness:?} {state:?}"
            );
        }
        assert_eq!(
            merge_witness(
                driver,
                &reading(Working, None, false),
                &screen(ScreenState::Unknown)
            ),
            Merged::Agrees(ScreenState::Busy, "herdr agy: working".to_string())
        );
        assert_eq!(
            merge_witness(
                driver,
                &reading(Blocked, Some("question_prompt"), true),
                &screen(ScreenState::Unknown)
            ),
            Merged::Agrees(
                ScreenState::Permission,
                "herdr agy: blocked (question_prompt)".to_string()
            )
        );
        let gone = agy_reading(None, Working, 1);
        assert_eq!(
            merge_witness(driver, &gone, &screen(ScreenState::Busy)),
            Merged::Gone
        );
        let foreign = agy_reading(Some("claude"), Working, 1);
        assert_eq!(
            merge_witness(driver, &foreign, &screen(ScreenState::Busy)),
            Merged::Foreign
        );
    }

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

    /// `mib` MiB of lines that carry no event (the engine's queue: no line
    /// id), then `last`: one write, as a long-idle transcript holds them.
    fn backlog(mib: usize, last: &str) -> String {
        let line = format!(
            "{{\"type\":\"queue-operation\",\"pad\":\"{}\"}}\n",
            "x".repeat(1000)
        );
        let mut text = line.repeat((mib << 20) / line.len() + 1);
        text.push_str(last);
        text
    }

    /// A backlog over one read is read through in passes (objective 241): a
    /// full read leaves the execution due, so the next pass reads again with
    /// no further change of the file; nothing is read twice.
    #[test]
    fn a_backlog_over_one_read_is_read_through_without_another_change() {
        let mut world = World::new();
        world.engine_writes(&backlog(20, TOOL_LINE));
        let mut passes = 0;
        while !world.engine_events().contains(&"tool.started") {
            passes += 1;
            assert!(passes <= 4, "20 MiB not read through in 4 passes");
            world.tick();
        }
        for _ in 0..3 {
            world.advance(500);
            world.tick();
        }
        assert_eq!(
            world.engine_events(),
            vec!["tool.started"],
            "nothing read twice"
        );
    }

    /// After a restart the follow starts again from the execution's start: a
    /// transcript longer than one read is read through to the line the
    /// engine writes next, whose event is logged; the old one not twice.
    #[test]
    fn after_a_restart_a_long_transcript_is_read_through_to_its_new_lines() {
        let mut world = World::new();
        world.engine_writes(&backlog(20, TOOL_LINE));
        for _ in 0..4 {
            world.tick();
        }
        assert_eq!(world.engine_events(), vec!["tool.started"]);
        world.restart();
        world.tick();
        world.engine_writes(TURN_LINE);
        for _ in 0..5 {
            world.advance(500);
            world.tick();
        }
        assert_eq!(
            world.engine_events(),
            vec!["tool.started", "turn.stopped"],
            "the new line is read, the old one not twice"
        );
    }

    /// A terminal killed from outside ends its execution on the next pass,
    /// as before: the fleet's liveness is one call per pass, not a capture
    /// and not a call per agent.
    /// A model call at `seconds` past the epoch, as Claude Code writes one.
    fn call_line(id: &str, seconds: u32) -> String {
        format!(
            "{{\"type\":\"assistant\",\"uuid\":\"{id}\",\"timestamp\":\"1970-01-01T00:00:{seconds:02}.000Z\",\
             \"message\":{{\"id\":\"msg_{id}\",\"model\":\"m\",\"usage\":{{\"input_tokens\":10,\
             \"output_tokens\":1}}}}}}\n"
        )
    }

    impl World {
        fn agent() -> AgentId {
            AgentId::new("agent_one").expect("agent")
        }

        /// The archive's one progress record.
        fn progress(&self) -> crate::transcripts::ArchiveProgress {
            let mut records =
                crate::port::TranscriptArchive::progress(self.store.as_ref()).expect("progress");
            assert_eq!(records.len(), 1, "one native session: {records:?}");
            records.remove(0)
        }

        fn settling(&self, until_ms: i64) -> bool {
            crate::transcripts::settling(
                &crate::port::TranscriptArchive::progress(self.store.as_ref()).expect("progress"),
                until_ms,
            )
        }

        /// What the archive holds of the session's transcript.
        fn archived(&self) -> String {
            let native = self.progress().native;
            let copies = crate::port::TranscriptArchive::copies(
                self.store.as_ref(),
                &Self::agent(),
                &native,
            )
            .expect("copies");
            copies
                .iter()
                .enumerate()
                .map(|(copy, _)| {
                    let bytes = crate::port::TranscriptArchive::read(
                        self.store.as_ref(),
                        &Self::agent(),
                        &native,
                        copy,
                        0,
                        usize::MAX,
                    )
                    .expect("read");
                    String::from_utf8(bytes).expect("utf-8")
                })
                .collect()
        }
    }

    /// Objective 47 (a): a model call of a turn that is still running is in
    /// the archive within a pass of being written, not at the turn's end;
    /// nothing is copied when nothing was added, and the progress record
    /// says until when the transcript is known.
    #[test]
    fn a_running_turn_is_archived_within_a_pass() {
        let mut world = World::with_archive();
        world.advance(20_000);
        let first = call_line("c1", 18);
        world.engine_writes(&first);
        world.tick();
        assert!(
            world.archived().contains(&first),
            "the call is archived mid-turn"
        );
        let progress = world.progress();
        assert!(progress.live);
        assert_eq!(progress.behind_ms, None);
        assert_eq!(
            progress.known_ms,
            crate::time::Clock::now(world.clock.as_ref()).0 - super::ARCHIVE_SLACK_MS
        );
        // Nothing added: nothing copied, and the window up to the read is
        // settled while a later one is not yet known.
        let length = world.archived().len();
        world.tick();
        assert_eq!(world.archived().len(), length);
        assert!(!world.settling(19_000));
        assert!(world.settling(22_000), "not read since the window's end");
        // An idle execution is known on as the passes go: it holds no
        // window open once read past its end.
        world.advance(5_000);
        world.tick();
        assert!(!world.settling(22_000), "{:?}", world.progress());
        let second = call_line("c2", 27);
        world.engine_writes(&second);
        world.tick();
        assert!(world.archived().ends_with(&second));
        assert_eq!(world.archived().matches("msg_c1").count(), 1, "copied once");
    }

    /// A made-up credential mask that can be had or not, as a credential
    /// file readable or not makes the collector's.
    struct Flip {
        mask: Arc<crate::mask::Mask>,
        had: Mutex<bool>,
    }

    impl crate::port::ArchiveMask for Flip {
        fn mask(&self) -> Result<Arc<crate::mask::Mask>, String> {
            if *self.had.lock().expect("lock") {
                Ok(self.mask.clone())
            } else {
                Err("/made-up/cred/spawn.env is unreadable".to_string())
            }
        }

        fn current(&self, mask: &Arc<crate::mask::Mask>) -> bool {
            *self.had.lock().expect("lock") && Arc::ptr_eq(mask, &self.mask)
        }
    }

    /// Objective 175: with a credential mask, what the archive copies is
    /// masked; without it a pass copies nothing, says why, and the record
    /// says from when the archive lacks lines; once it is had, the waiting
    /// lines are copied masked.
    #[test]
    fn the_archive_masks_values_and_copies_nothing_without_the_mask() {
        const VALUE: &str = "made-up-token-0123456789";
        let mut world = World::with_archive();
        let flip = Arc::new(Flip {
            mask: Arc::new(crate::mask::Mask::new(
                &[("spawn.env:API_TOKEN".to_string(), VALUE.as_bytes().to_vec())],
                crate::transcripts::ARCHIVE_MARKER,
            )),
            had: Mutex::new(false),
        });
        world.bridge.set_archive_mask(flip.clone());
        world.advance(20_000);
        let said =
            call_line("c1", 18).replace("\"model\":\"m\"", &format!("\"model\":\"{VALUE}\""));
        assert!(said.contains(VALUE));
        world.engine_writes(&said);
        let warnings = world.tick();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("the credential mask: /made-up/cred/spawn.env")),
            "{warnings:?}"
        );
        assert_eq!(world.archived(), "", "nothing copied without the mask");
        assert!(world.progress().behind_ms.is_some());
        *flip.had.lock().expect("lock") = true;
        world.watch.changed(&world.path.clone());
        world.tick();
        let archived = world.archived();
        assert!(
            archived.contains("[REDACTED:spawn.env:API_TOKEN]") && !archived.contains(VALUE),
            "{archived}"
        );
        assert_eq!(world.progress().behind_ms, None);
        assert!(
            String::from_utf8(
                world
                    .store
                    .read_from(&world.progress().workspace, &world.path, 0)
                    .expect("read")
                    .expect("present")
            )
            .expect("utf-8")
            .contains(VALUE),
            "the transcript is never written"
        );
    }

    /// S33: a failing copy whose first read of what the archive lacks meets
    /// a line still being written holds every window open for that pass
    /// ("no time that reads"), never settles one early, and does not keep
    /// that answer for as long as the copy fails: it reads the line again,
    /// and once the line is whole a window ending before its call reads
    /// settled while one ending after it stays open.
    #[test]
    fn a_failed_copy_over_a_half_line_reads_settled_once_the_line_is_whole() {
        let mut world = World::with_archive();
        world.advance(20_000);
        world.store.hold_archive(&World::agent(), true);
        let line = call_line("c1", 19);
        let (half, rest) = line.split_at(20);
        world.engine_writes(half);
        world.tick();
        assert_eq!(world.progress().behind_ms, Some(i64::MIN), "no time yet");
        assert!(world.settling(18_500), "a half line settles no window");
        assert!(world.settling(19_500), "a half line settles no window");
        world.engine_writes(rest);
        world.tick();
        let progress = world.progress();
        assert_eq!(progress.behind_ms, Some(19_000), "{progress:?}");
        assert!(
            !world.settling(18_500),
            "a window ending before the call reads settled: {progress:?}"
        );
        assert!(
            world.settling(19_500),
            "a call before the end is not archived"
        );
    }

    /// Objective 47 (a), (b): a copy that fails is warned once and made
    /// again on every pass, live, after the execution ended, and after a
    /// restart, which reads what is still to copy from the records; while
    /// it fails, a window ending after the first line the archive lacks is
    /// settling, and one ending before it is not.
    #[test]
    fn a_failed_copy_holds_the_window_open_until_it_is_made_across_an_end_and_a_restart() {
        let mut world = World::with_archive();
        world.advance(20_000);
        world.store.hold_archive(&World::agent(), true);
        world.engine_writes(&call_line("c1", 19));
        let warnings = world.tick();
        assert_eq!(
            warnings
                .iter()
                .filter(|warning| warning.contains("archive the transcript"))
                .count(),
            1,
            "{warnings:?}"
        );
        assert!(
            !world
                .tick()
                .iter()
                .any(|warning| warning.contains("archive the transcript")),
            "the same failure is warned once"
        );
        let progress = world.progress();
        assert_eq!(progress.behind_ms, Some(19_000));
        assert!(
            world.settling(19_500),
            "a call before the end is not archived"
        );
        assert!(
            !world.settling(18_500),
            "a call after the end holds nothing open"
        );
        // The execution ends, its transcript still not archived.
        world.store.untrack(
            &TerminalName::for_execution(&ExecutionId::for_command(CommandId(1))),
            "gone",
        );
        world.tick();
        world.tick();
        let progress = world.progress();
        assert!(!progress.live, "the record says it ended");
        assert_eq!(progress.behind_ms, Some(19_000));
        assert!(world.settling(19_500));
        assert!(!world.settling(18_500));
        // The loop restarts: what is still to copy is read back.
        world.restart();
        world.tick();
        assert!(world.settling(19_500), "{:?}", world.progress());
        // The archive takes writes again: the next pass copies and settles.
        world.store.hold_archive(&World::agent(), false);
        world.tick();
        assert!(world.archived().contains("msg_c1"));
        assert!(!world.settling(19_500), "{:?}", world.progress());
        assert!(
            !world.settling(i64::MAX),
            "an ended execution, all archived"
        );
        assert!(world.bridge.ended_archives.lock().expect("lock").is_empty());
    }

    /// What a scripted credential mask answers (objective 179's tests).
    #[derive(Clone)]
    enum MaskMode {
        Had,
        Unreadable(String),
        /// Had, but stale by the time the lines are masked.
        Stale,
    }

    /// A made-up credential mask whose answer the test sets.
    struct Scripted {
        mask: Arc<crate::mask::Mask>,
        mode: Mutex<MaskMode>,
    }

    impl Scripted {
        fn new(mode: MaskMode) -> Arc<Self> {
            Arc::new(Self {
                mask: Arc::new(crate::mask::Mask::new(
                    &[(
                        "spawn.env:API_TOKEN".to_string(),
                        b"made-up-token-0123456789".to_vec(),
                    )],
                    crate::transcripts::ARCHIVE_MARKER,
                )),
                mode: Mutex::new(mode),
            })
        }

        fn set(&self, mode: MaskMode) {
            *self.mode.lock().expect("lock") = mode;
        }
    }

    impl crate::port::ArchiveMask for Scripted {
        fn mask(&self) -> Result<Arc<crate::mask::Mask>, String> {
            match &*self.mode.lock().expect("lock") {
                MaskMode::Unreadable(why) => Err(why.clone()),
                MaskMode::Had | MaskMode::Stale => Ok(self.mask.clone()),
            }
        }

        fn current(&self, mask: &Arc<crate::mask::Mask>) -> bool {
            matches!(*self.mode.lock().expect("lock"), MaskMode::Had)
                && Arc::ptr_eq(mask, &self.mask)
        }
    }

    impl World {
        /// A Bridge with an archive whose credential mask the test scripts,
        /// a call of the session's written and not yet archived.
        fn with_scripted_mask(mode: MaskMode) -> (Self, Arc<Scripted>) {
            let mut world = Self::with_archive();
            let mask = Scripted::new(mode);
            world.bridge.set_archive_mask(mask.clone());
            world.advance(20_000);
            world.engine_writes(&call_line("c1", 18));
            (world, mask)
        }

        /// The archive events the log holds, in order.
        fn archive_events(&self) -> Vec<EventKind> {
            self.store
                .read_global(GlobalPosition(0), 10_000)
                .expect("read")
                .into_iter()
                .map(|record| record.event.kind)
                .filter(|kind| {
                    matches!(
                        kind,
                        EventKind::ArchiveFailed { .. } | EventKind::ArchiveResumed { .. }
                    )
                })
                .collect()
        }

        /// How many `archive.failed` and `archive.resumed` the log holds.
        fn archive_counts(&self) -> (usize, usize) {
            let events = self.archive_events();
            let failed = events
                .iter()
                .filter(|kind| matches!(kind, EventKind::ArchiveFailed { .. }))
                .count();
            (failed, events.len() - failed)
        }

        /// The execution ends: its terminal is gone.
        fn end(&mut self) {
            self.store.untrack(
                &TerminalName::for_execution(&ExecutionId::for_command(CommandId(1))),
                "gone",
            );
            self.tick();
            self.tick();
        }
    }

    /// Objective 179: a failing copy is recorded once per reason, on a
    /// change only, and its end once, with how far the copy moved.
    #[test]
    fn an_archive_failure_is_recorded_on_a_change_and_its_end_once() {
        let (mut world, mask) =
            World::with_scripted_mask(MaskMode::Unreadable("/made-up/cred/a".to_string()));
        world.tick();
        world.tick();
        let native = world.progress().native;
        assert_eq!(
            world.archive_events(),
            vec![EventKind::ArchiveFailed {
                native: native.clone(),
                reason: "the credential mask: /made-up/cred/a".to_string(),
            }],
            "one failure, the loop's words"
        );
        mask.set(MaskMode::Unreadable("/made-up/cred/b".to_string()));
        world.tick();
        world.tick();
        assert_eq!(world.archive_counts(), (2, 0), "a new reason is a change");
        mask.set(MaskMode::Had);
        world.tick();
        let length = u64::try_from(world.archived().len()).expect("length");
        assert!(length > 0);
        assert_eq!(
            world.archive_events().last(),
            Some(&EventKind::ArchiveResumed {
                native,
                copied: length,
            })
        );
        world.engine_writes(&call_line("c2", 19));
        world.tick();
        world.tick();
        assert_eq!(world.archive_counts(), (2, 1), "its end recorded once");
    }

    /// Objective 179: one stale mask records nothing (the next pass copies),
    /// two in a row record a failure.
    #[test]
    fn one_stale_mask_records_nothing_and_two_in_a_row_a_failure() {
        let (mut world, mask) = World::with_scripted_mask(MaskMode::Stale);
        world.tick();
        assert_eq!(world.archive_counts(), (0, 0), "a first stale mask");
        mask.set(MaskMode::Had);
        world.tick();
        assert!(world.archived().contains("msg_c1"));
        assert_eq!(world.archive_counts(), (0, 0), "copied: nothing to end");
        mask.set(MaskMode::Stale);
        world.engine_writes(&call_line("c2", 19));
        world.tick();
        assert_eq!(world.archive_counts(), (0, 0), "not two in a row");
        world.tick();
        assert_eq!(
            world.archive_events(),
            vec![EventKind::ArchiveFailed {
                native: world.progress().native,
                reason: crate::transcripts::MASK_CHANGED.to_string(),
            }],
            "two in a row"
        );
    }

    /// Objective 179: a reason is cut to ARCHIVE_REASON_MAX characters, on a
    /// character boundary.
    #[test]
    fn an_archive_failure_reason_is_cut_to_its_limit() {
        let (mut world, _mask) = World::with_scripted_mask(MaskMode::Unreadable("é".repeat(600)));
        world.tick();
        let events = world.archive_events();
        let [EventKind::ArchiveFailed { reason, .. }] = events.as_slice() else {
            panic!("one failure: {events:?}");
        };
        assert_eq!(reason.chars().count(), crate::event::ARCHIVE_REASON_MAX);
        assert!(reason.starts_with("the credential mask: éé"), "{reason}");
    }

    /// Objective 179: an archive event the log took without its answer
    /// coming back is warned, tried again under the same key, and stored
    /// once.
    #[test]
    fn a_failed_archive_record_is_retried_under_the_same_key() {
        let (mut world, mask) =
            World::with_scripted_mask(MaskMode::Unreadable("/made-up/cred/a".to_string()));
        world.store.lose_next_archive_answer();
        let warnings = world.tick();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("record the transcript archive's state")),
            "{warnings:?}"
        );
        world.tick();
        world.tick();
        assert_eq!(world.archive_counts(), (1, 0), "stored once");
        world.store.lose_next_archive_answer();
        mask.set(MaskMode::Had);
        world.tick();
        world.tick();
        assert_eq!(world.archive_counts(), (1, 1), "its end stored once");
    }

    /// Objective 179 R2: a failure recorded before a restart is read back:
    /// still failing the same way records nothing, and the first copying
    /// pass after a later restart, on the ended execution's retry, records
    /// its end.
    #[test]
    fn a_failure_recorded_before_a_restart_is_ended_by_the_restarted_bridge() {
        let why = MaskMode::Unreadable("/made-up/cred/a".to_string());
        let (mut world, mask) = World::with_scripted_mask(why);
        world.tick();
        assert_eq!(world.archive_counts(), (1, 0));
        world.restart();
        world.bridge.set_archive_mask(mask.clone());
        world.tick();
        world.tick();
        assert_eq!(world.archive_counts(), (1, 0), "still failing: nothing");
        world.end();
        assert_eq!(world.archive_counts(), (1, 0), "nor once ended");
        world.restart();
        world.bridge.set_archive_mask(mask.clone());
        mask.set(MaskMode::Had);
        world.tick();
        world.tick();
        assert!(world.archived().contains("msg_c1"));
        assert_eq!(world.archive_counts(), (1, 1), "its end, once");
        assert!(matches!(
            world.archive_events().last(),
            Some(EventKind::ArchiveResumed { copied, .. }) if *copied > 0
        ));
    }

    /// Objective 179 R3: a transcript removed while the archive lacks lines
    /// of it stays behind and never reads resumed.
    #[test]
    fn a_transcript_gone_with_lines_unarchived_never_reads_resumed() {
        let (mut world, mask) =
            World::with_scripted_mask(MaskMode::Unreadable("/made-up/cred/a".to_string()));
        world.tick();
        assert_eq!(world.archive_counts(), (1, 0));
        world.store.remove_file(&world.path.clone());
        mask.set(MaskMode::Had);
        world.tick();
        world.tick();
        assert_eq!(world.archive_counts(), (1, 0), "no resume, nothing new");
        assert!(world.progress().behind_ms.is_some(), "still behind");
        world.end();
        assert_eq!(world.archive_counts(), (1, 0));
        assert!(world.progress().behind_ms.is_some(), "still behind");
    }

    /// Objective 179 R4: a transcript removed with lines the archive lacks is
    /// a failure whatever the log holds: a failure still waiting for the log
    /// stands and is stored once; with none told, the loss is recorded once.
    #[test]
    fn a_transcript_removed_with_lines_unarchived_is_a_failure_whatever_the_log_holds() {
        // A lost answer: the failure waits, under its key.
        let (mut world, mask) =
            World::with_scripted_mask(MaskMode::Unreadable("/made-up/cred/a".to_string()));
        world.store.lose_next_archive_answer();
        world.tick();
        world.store.remove_file(&world.path.clone());
        mask.set(MaskMode::Had);
        world.tick();
        world.tick();
        let events = world.archive_events();
        assert_eq!(world.archive_counts(), (1, 0), "{events:?}");
        assert!(matches!(
            events.last(),
            Some(EventKind::ArchiveFailed { reason, .. }) if reason.contains("/made-up/cred/a")
        ));
        // A first stale mask: nothing told, then the transcript goes.
        let (mut world, mask) = World::with_scripted_mask(MaskMode::Stale);
        world.tick();
        assert_eq!(world.archive_counts(), (0, 0));
        assert!(world.progress().behind_ms.is_some());
        world.store.remove_file(&world.path.clone());
        mask.set(MaskMode::Had);
        world.tick();
        world.tick();
        world.end();
        assert_eq!(
            world.archive_events(),
            vec![EventKind::ArchiveFailed {
                native: world.progress().native,
                reason: super::ARCHIVE_GONE.to_string(),
            }]
        );
        assert!(world.progress().behind_ms.is_some(), "still behind");
    }

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

    #[test]
    fn an_unready_start_does_not_delay_another_sessions_prompt() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let host = Counted::over(store.clone());
        let mut config = BridgeConfig::new("tester");
        config.max_commands_per_tick = 16;
        config.ready_attempts = 40;
        let a = submit_resumable_start(&store, 1, "A's prompt", BUSY);
        let b = submit_resumable_start(&store, 2, "B's prompt", READY);
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            host.clone(),
            config,
        )
        .with_clock(clock);

        drive_work(&mut bridge, &store, || command_outcome(&store, b).is_some());
        assert_eq!(command_outcome(&store, b), Some(Outcome::Succeeded));
        assert_eq!(command_outcome(&store, a), None, "A is still unready");
        assert!(
            host.captures() < 40,
            "B did not wait for A's whole readiness budget: {} captures",
            host.captures()
        );
        let records = store.read_global(GlobalPosition(0), 1_000).expect("read");
        assert!(!records.iter().any(|record| {
            matches!(
                record.event.kind,
                EventKind::CommandStepped {
                    command_id,
                    step: CommandStep::Waiting,
                    ..
                } if command_id == a || command_id == b
            )
        }));
        let pane = host
            .inner
            .capture_tail(
                &TerminalName::for_execution(&ExecutionId::for_command(b)),
                &WorkspaceId::new("1:/workspaces/1").expect("workspace"),
                super::TAIL_LINES,
            )
            .expect("pane");
        assert!(pane.contains("B's prompt"), "{pane}");

        store.set_pane(
            &TerminalName::for_execution(&ExecutionId::for_command(a)),
            READY,
        );
        drive_work(&mut bridge, &store, || command_outcome(&store, a).is_some());
        assert_eq!(command_outcome(&store, a), Some(Outcome::Succeeded));
    }

    #[test]
    fn a_trust_dialog_is_confirmed_once_across_resumable_claims() {
        const TRUST: &str = "Quick safety check: Is this a project you created or one you trust?\n\
                             y. Yes, I trust this folder\n\
                             n. No, exit\n\
                             Enter y/n:\n\
                             Enter to confirm · Esc to cancel\n";
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let host = Counted::over(store.clone());
        let id = submit_resumable_start(&store, 1, "work", TRUST);
        let terminal = TerminalName::for_execution(&ExecutionId::for_command(id));
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            host.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        drive_work(&mut bridge, &store, || {
            host.keys().len() == crate::screen::TRUST_KEYS.len()
        });
        assert_eq!(command_outcome(&store, id), None);

        // Another claim sees the same dialog. The retained `answered_trust`
        // bit prevents a second confirmation landing in the future input box.
        bridge.work().expect("one more claim");
        store.expire_leases();
        assert_eq!(host.keys().len(), crate::screen::TRUST_KEYS.len());
        store.set_pane(&terminal, READY);
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, id).is_some()
        });
        assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
        let expected: Vec<String> = crate::screen::TRUST_KEYS
            .iter()
            .map(|key| (*key).to_string())
            .collect();
        assert_eq!(host.keys(), expected);
        assert!(!bridge.prompting.contains_key(&id));

        // The answer is on the record (PR 3): one `key` command a key, of
        // `spawn-resolve`, keyed by the execution, recorded pressing and
        // pressed, then finished once the start was done, with nothing
        // pressed again. Submitting a key again returns the same command.
        let execution = ExecutionId::for_command(id);
        let principal = Principal::new(RESOLVE_PRINCIPAL).expect("principal");
        for (at, key) in crate::screen::TRUST_KEYS.iter().enumerate() {
            let named = NamedKey::ALL
                .iter()
                .copied()
                .find(|named| named.tmux() == *key)
                .expect("a named key");
            let command = store
                .submit(
                    &principal,
                    &Submission {
                        agent_id: resumable_agent(1),
                        session_id: resumable_session(1),
                        idempotency_key: IdempotencyKey::new(trust_answer_key(&execution, at + 1))
                            .expect("key"),
                        action: CommandAction::Key { key: named },
                    },
                )
                .expect("the same command");
            drive_work(&mut bridge, &store, || {
                command_outcome(&store, command).is_some()
            });
            assert_eq!(command_outcome(&store, command), Some(Outcome::Succeeded));
            let steps: Vec<CommandStep> = store
                .read_global(GlobalPosition(0), 100_000)
                .expect("read")
                .into_iter()
                .filter_map(|record| match record.event.kind {
                    EventKind::CommandStepped {
                        command_id, step, ..
                    } if command_id == command => Some(step),
                    _ => None,
                })
                .collect();
            assert_eq!(steps, vec![CommandStep::Pressing, CommandStep::Pressed]);
        }
        assert_eq!(host.keys(), expected, "nothing pressed twice");
    }

    /// `SPAWN_AUTO_APPROVE=off` (`auto_trust = false`): the trust dialog is
    /// not answered, no command is submitted for it, and the wait fails
    /// naming the dialog and the switch; once a person answers it (the pane
    /// ready), the prompt is typed.
    #[test]
    fn with_auto_trust_off_the_trust_dialog_is_left_to_a_person() {
        const TRUST: &str = "Quick safety check: Is this a project you created or one you trust?\n\
                             y. Yes, I trust this folder\n\
                             n. No, exit\n\
                             Enter y/n:\n\
                             Enter to confirm · Esc to cancel\n";
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let host = Counted::over(store.clone());
        let id = submit_resumable_start(&store, 1, "work", TRUST);
        let terminal = TerminalName::for_execution(&ExecutionId::for_command(id));
        let mut config = BridgeConfig::new("tester");
        config.auto_trust = false;
        config.ready_attempts = 6;
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            host.clone(),
            config,
        )
        .with_clock(clock.clone());
        for _ in 0..3 {
            bridge.work().expect("work");
            store.expire_leases();
        }
        assert!(host.keys().is_empty(), "nothing pressed");
        assert_eq!(command_outcome(&store, id), None, "still waiting");
        store.set_pane(&terminal, READY);
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, id).is_some()
        });
        assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
        assert!(host.keys().is_empty(), "still nothing pressed");
        let resolver_commands = records(&store)
            .into_iter()
            .filter(|record| matches!(record.event.kind, EventKind::CommandAccepted { .. }))
            .count();
        assert_eq!(resolver_commands, 1, "the start alone: no key command");

        // Never answered: the wait fails naming the dialog and the switch.
        let other = submit_resumable_start(&store, 2, "work", TRUST);
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, other).is_some()
        });
        match command_outcome(&store, other) {
            Some(Outcome::Failed(why)) => {
                assert!(
                    why.contains("trust dialog") && why.contains("SPAWN_AUTO_APPROVE=off"),
                    "{why}"
                );
            }
            outcome => panic!("a failed wait, not {outcome:?}"),
        }
        assert!(host.keys().is_empty());
    }

    /// A person and the resolver both answer one dialog (design section 8,
    /// PR 3 pitfall 2), on the in-memory host: the person's key, pressed
    /// first, moves the dialog on, so the resolver's key, submitted after
    /// it, presses nothing (`Failed`, no `pressing`), although the pane
    /// still draws the dialog. Inputs of `spawn-resolve` would be refused
    /// like any loop's, but a key never waits for a turn: listing it changes
    /// no input rule.
    #[test]
    fn a_human_and_the_resolver_answering_one_dialog_each_press_once_in_order() {
        let (store, host, mut bridge, execution) = resolver_rig(PROCEED);
        let human = submit_key(&store, "human", "approve-1:1", NamedKey::Digit1);
        let resolver_key = format!("resolve:{execution}:session_1:0000000000000001:1:1");
        let resolver = submit_key(&store, RESOLVE_PRINCIPAL, &resolver_key, NamedKey::Digit1);
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, resolver).is_some()
        });
        assert_eq!(command_outcome(&store, human), Some(Outcome::Succeeded));
        assert_eq!(
            command_outcome(&store, resolver),
            Some(Outcome::Failed(super::GONE_DIALOG.to_string()))
        );
        assert_eq!(host.keys(), vec!["1".to_string()], "the person's key only");
        assert!(steps_of(&store, resolver).is_empty(), "no pressing");
        assert_eq!(
            submit_key(&store, RESOLVE_PRINCIPAL, &resolver_key, NamedKey::Digit1),
            resolver
        );
        bridge.work().expect("one more claim");
        assert_eq!(host.keys().len(), 1, "a resubmitted answer presses nothing");
        assert!(!waits(RESOLVE_PRINCIPAL), "a loop principal");
        assert!(waits("human"));
    }

    /// A resolver's key whose dialog is gone (the pane is at its prompt)
    /// presses nothing and records no `pressing`; one whose dialog still
    /// shows, with nothing pressed since, is pressed.
    #[test]
    fn a_resolver_key_whose_dialog_is_gone_presses_nothing() {
        let (store, host, mut bridge, execution) = resolver_rig(READY);
        let gone = submit_key(
            &store,
            RESOLVE_PRINCIPAL,
            &format!("resolve:{execution}:session_1:0000000000000002:1:1"),
            NamedKey::Digit1,
        );
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, gone).is_some()
        });
        assert_eq!(
            command_outcome(&store, gone),
            Some(Outcome::Failed(super::GONE_DIALOG.to_string()))
        );
        assert!(host.keys().is_empty());
        assert!(steps_of(&store, gone).is_empty());
        store.set_pane(&TerminalName::for_execution(&execution), PROCEED);
        let shown = submit_key(
            &store,
            RESOLVE_PRINCIPAL,
            &format!("resolve:{execution}:session_1:0000000000000003:1:1"),
            NamedKey::Digit1,
        );
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, shown).is_some()
        });
        assert_eq!(command_outcome(&store, shown), Some(Outcome::Succeeded));
        assert_eq!(host.keys(), vec!["1".to_string()]);
        assert_eq!(
            steps_of(&store, shown),
            vec![CommandStep::Pressing, CommandStep::Pressed]
        );
    }

    /// Claude Code's numbered dialog at its prompt position.
    const PROCEED: &str = "Do you want to proceed?\n$ 1. Yes\n  2. No\n";

    /// A started Claude Code execution (session 1) whose pane then shows
    /// `pane`, and the execution.
    fn resolver_rig(pane: &str) -> (Arc<MemoryStore>, Arc<Counted>, Bridge, ExecutionId) {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let host = Counted::over(store.clone());
        let id = submit_resumable_start(&store, 1, "work", READY);
        let terminal = TerminalName::for_execution(&ExecutionId::for_command(id));
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            host.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, id).is_some()
        });
        assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
        store.set_pane(&terminal, pane);
        (store, host, bridge, ExecutionId::for_command(id))
    }

    fn submit_key(
        store: &Arc<MemoryStore>,
        principal: &str,
        key: &str,
        named: NamedKey,
    ) -> CommandId {
        store
            .submit(
                &Principal::new(principal).expect("principal"),
                &Submission {
                    agent_id: resumable_agent(1),
                    session_id: resumable_session(1),
                    idempotency_key: IdempotencyKey::new(key).expect("key"),
                    action: CommandAction::Key { key: named },
                },
            )
            .expect("submit")
    }

    fn steps_of(store: &Arc<MemoryStore>, command: CommandId) -> Vec<CommandStep> {
        store
            .read_global(GlobalPosition(0), 100_000)
            .expect("read")
            .into_iter()
            .filter_map(|record| match record.event.kind {
                EventKind::CommandStepped {
                    command_id, step, ..
                } if command_id == command => Some(step),
                _ => None,
            })
            .collect()
    }

    /// Claude Code keeps its deny rules (`Guard::Settings`) and never runs
    /// in the filesystem view, whatever the host's view policy: with no
    /// view at all (the default) it still starts, as the plain runner.
    #[test]
    fn claude_code_never_runs_in_the_filesystem_view() {
        let policies = [
            EngineViewPolicy::unconfigured(),
            EngineViewPolicy::Required(Ok(Arc::new(FakeView::default()))),
        ];
        for (number, policy) in (1_u64..).zip(policies) {
            let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
            let store = Arc::new(MemoryStore::new("shop", clock.clone()));
            let id = submit_resumable_start(&store, number, "hello", READY);
            let mut config = BridgeConfig::new("tester");
            config.engine_view = policy;
            let mut bridge = Bridge::new(
                store.clone(),
                store.clone(),
                store.clone(),
                store.clone(),
                config,
            )
            .with_clock(clock);
            drive_work(&mut bridge, &store, || {
                command_outcome(&store, id).is_some()
            });
            assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
            let launch = store.launches().pop().expect("launched");
            assert_eq!(launch.program, "spawn");
            assert_eq!(launch.arguments[..2], ["run", "--"]);
        }
    }

    #[test]
    fn a_reclaimed_start_launches_and_types_only_once() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let id = submit_resumable_start(&store, 1, "only once", BUSY);
        let terminal = TerminalName::for_execution(&ExecutionId::for_command(id));
        let config = BridgeConfig::new("tester");
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            store.clone(),
            config.clone(),
        )
        .with_clock(clock.clone());
        drive_work(&mut bridge, &store, || store.launches().len() == 1);
        assert!(bridge.prompting.contains_key(&id));
        assert_eq!(store.launches().len(), 1);
        assert_eq!(command_outcome(&store, id), None);
        drop(bridge);

        store.expire_leases();
        store.set_pane(&terminal, READY);
        let mut reclaimed = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            store.clone(),
            config,
        )
        .with_clock(clock);
        reclaimed.recover().expect("recover");
        drive_work(&mut reclaimed, &store, || {
            command_outcome(&store, id).is_some()
        });
        assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
        assert_eq!(store.launches().len(), 1, "no second terminal");
        let typed = store
            .read_global(GlobalPosition(0), 1_000)
            .expect("read")
            .into_iter()
            .filter(|record| {
                matches!(
                    record.event.kind,
                    EventKind::CommandStepped {
                        command_id,
                        step: CommandStep::Typed,
                        ..
                    } if command_id == id
                )
            })
            .count();
        assert_eq!(typed, 1, "no second prompt");
    }

    #[test]
    fn a_resumable_prompt_with_unknown_typing_retains_may_have_typed() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let id = submit_resumable_start(&store, 1, "uncertain", READY);
        store.fail_next_submit();
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            store.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, id).is_some()
        });
        let Some(Outcome::Failed(error)) = command_outcome(&store, id) else {
            panic!("the uncertain typing failed")
        };
        assert!(error.contains(super::MAY_HAVE_TYPED), "{error}");
        assert!(!bridge.prompting.contains_key(&id));
    }

    #[test]
    fn a_blocked_mount_does_not_hold_another_sessions_start() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let mounts = Arc::new(GatedMounts::new(store.clone()));
        let a = submit_resumable_start(&store, 1, "", READY);
        let b = submit_resumable_start(&store, 2, "", READY);
        let a_branch = BranchName::for_agent(&resumable_agent(1));
        mounts.hold(a_branch.clone());
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            mounts.clone(),
            store.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        let _release = ReleaseMounts(&mounts);
        // One pass claims both starts and gives each its mount worker; then
        // wait on the workers, not on time: A's is at its gate, B's done.
        bridge.work().expect("work");
        mounts.wait_for("A held at the gate, B mounted", |mounts| {
            mounts.calls.load(Ordering::SeqCst) == 2
                && mounts.active.load(Ordering::SeqCst) == 1
                && mounts.completed.load(Ordering::SeqCst) == 1
        });
        drive_work(&mut bridge, &store, || command_outcome(&store, b).is_some());
        assert_eq!(command_outcome(&store, b), Some(Outcome::Succeeded));
        assert_eq!(command_outcome(&store, a), None);
        assert_eq!(mounts.calls.load(Ordering::SeqCst), 2);
        assert_eq!(mounts.active.load(Ordering::SeqCst), 1);
        mounts.release(&a_branch);
        mounts.wait_for("A mounted once released", |mounts| {
            mounts.completed.load(Ordering::SeqCst) == 2
        });
        drive_work(&mut bridge, &store, || command_outcome(&store, a).is_some());
        assert_eq!(command_outcome(&store, a), Some(Outcome::Succeeded));
    }

    #[test]
    fn concurrent_mount_workers_are_bounded_to_eight() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let mounts = Arc::new(GatedMounts::new(store.clone()));
        let mut ids = Vec::new();
        for number in 1..=16 {
            let id = submit_resumable_start(&store, number, "", READY);
            ids.push(id);
            mounts.hold(BranchName::for_agent(&resumable_agent(number)));
        }
        let mut config = BridgeConfig::new("tester");
        config.max_commands_per_tick = 64;
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            mounts.clone(),
            store.clone(),
            config,
        )
        .with_clock(clock);
        let _release = ReleaseMounts(&mounts);
        drive_work(&mut bridge, &store, || {
            mounts.calls.load(Ordering::SeqCst) == super::MAX_CONCURRENT_MOUNTS
        });
        assert_eq!(mounts.active.load(Ordering::SeqCst), 8);
        assert_eq!(mounts.peak.load(Ordering::SeqCst), 8);
        mounts.release_all();
        drive_work(&mut bridge, &store, || {
            ids.iter().all(|id| command_outcome(&store, *id).is_some())
        });
        assert_eq!(mounts.peak.load(Ordering::SeqCst), 8);
        assert!(
            ids.iter()
                .all(|id| { command_outcome(&store, *id) == Some(Outcome::Succeeded) })
        );
    }

    #[test]
    fn a_reclaimed_mounted_step_reuses_its_workspace() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let id = submit_resumable_start(&store, 1, "", READY);
        let branch = BranchName::for_agent(&resumable_agent(1));
        store.ensure_branch(&branch, None).expect("branch");
        let workspace = store.mount(&branch).expect("mount");
        store
            .append(NewEvent {
                agent_id: resumable_agent(1),
                session_id: resumable_session(1),
                execution_id: None,
                source: Source::Bridge,
                source_event_id: Some(format!("{id}:mounted")),
                kind: EventKind::CommandStepped {
                    command_id: id,
                    step: CommandStep::Mounted,
                    workspace: Some(workspace.clone()),
                },
            })
            .expect("the crash state's mounted record");
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            store.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        bridge.recover().expect("recover");
        assert_eq!(
            store
                .workspace_calls()
                .iter()
                .filter(|call| *call == "unmount")
                .count(),
            0,
            "recovery kept the unfinished start's mount"
        );
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, id).is_some()
        });
        assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
        let records = store.read_global(GlobalPosition(0), 1_000).expect("read");
        let started = records.iter().find_map(|record| match &record.event.kind {
            EventKind::ExecutionStarted {
                workspace: started, ..
            } => Some(started),
            _ => None,
        });
        assert_eq!(started, Some(&workspace));
        assert_eq!(
            store
                .workspace_calls()
                .iter()
                .filter(|call| call.starts_with("mount "))
                .count(),
            1,
            "no remount"
        );
        assert_eq!(
            store
                .workspace_calls()
                .iter()
                .filter(|call| *call == "unmount")
                .count(),
            0
        );
    }

    #[test]
    fn dropping_a_bridge_joins_an_in_flight_mount() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let mounts = Arc::new(GatedMounts::new(store.clone()));
        let _id = submit_resumable_start(&store, 1, "", READY);
        let branch = BranchName::for_agent(&resumable_agent(1));
        mounts.hold(branch.clone());
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            mounts.clone(),
            store.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        let _release = ReleaseMounts(&mounts);
        drive_work(&mut bridge, &store, || {
            mounts.calls.load(Ordering::SeqCst) == 1
        });
        assert_eq!(bridge.mounting.len(), 1);
        mounts.release(&branch);
        drop(bridge);
        assert_eq!(mounts.completed.load(Ordering::SeqCst), 1);
        assert_eq!(mounts.active.load(Ordering::SeqCst), 0);
        assert_eq!(
            store
                .workspace_calls()
                .iter()
                .filter(|call| *call == "unmount")
                .count(),
            1,
            "the unrecorded mount was released after its worker joined"
        );
    }

    #[test]
    fn thirty_two_delayed_starts_complete_without_blocking_sleeps() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let host = Counted::over(store.clone());
        let mut ids = Vec::new();
        for number in 1..=32 {
            let id = submit_resumable_start(&store, number, "work", READY);
            host.delay_next(TerminalName::for_execution(&ExecutionId::for_command(id)));
            ids.push(id);
        }
        let mut config = BridgeConfig::new("tester");
        config.max_commands_per_tick = 64;
        config.ready_attempts = 4;
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            host.clone(),
            config,
        )
        .with_clock(clock);
        drive_work(&mut bridge, &store, || {
            ids.iter().all(|id| command_outcome(&store, *id).is_some())
        });
        assert!(
            ids.iter()
                .all(|id| { command_outcome(&store, *id) == Some(Outcome::Succeeded) })
        );
        assert_eq!(store.launches().len(), 32);
        assert!(
            host.captures() >= 64,
            "each engine was unready once and ready on a later claim: {}",
            host.captures()
        );
        assert!(bridge.prompting.is_empty());
        assert!(bridge.mounting.is_empty());
    }

    #[test]
    fn starts_pending_tracks_mounting_and_prompting_but_not_waiting_inputs() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let mounts = Arc::new(GatedMounts::new(store.clone()));
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            mounts.clone(),
            store.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        assert!(!bridge.starts_pending());

        let start_id = submit_resumable_start(&store, 1, "go", BUSY);
        let branch = BranchName::for_agent(&resumable_agent(1));
        mounts.hold(branch.clone());
        let _release = ReleaseMounts(&mounts);
        drive_work(&mut bridge, &store, || {
            mounts.calls.load(Ordering::SeqCst) == 1
        });
        assert!(
            bridge.starts_pending(),
            "pending mount worker marks starts_pending"
        );

        mounts.release(&branch);
        drive_until(&mut bridge, &store, |bridge| !bridge.prompting.is_empty());
        assert!(bridge.mounting.is_empty());
        assert!(
            bridge.starts_pending(),
            "pending prompt readiness marks starts_pending"
        );

        let terminal = TerminalName::for_execution(&ExecutionId::for_command(start_id));
        store.set_pane(&terminal, READY);
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, start_id).is_some()
        });
        assert_eq!(command_outcome(&store, start_id), Some(Outcome::Succeeded));
        assert!(
            !bridge.starts_pending(),
            "finished start clears starts_pending"
        );

        store.set_pane(&terminal, BUSY);
        let input_id = store
            .submit(
                &Principal::new("tester").expect("principal"),
                &Submission {
                    agent_id: resumable_agent(1),
                    session_id: resumable_session(1),
                    idempotency_key: IdempotencyKey::new("waiting-input").expect("key"),
                    action: CommandAction::Input {
                        text: "still busy".to_string(),
                        tags: Vec::new(),
                    },
                },
            )
            .expect("submit input");
        bridge.work().expect("work holds waiting input");
        assert!(bridge.held.contains_key(&input_id));
        assert!(
            !bridge.starts_pending(),
            "waiting Input is not a pending start"
        );
        assert!(!bridge.ends_pending(), "waiting Input is not a pending end");
    }

    #[test]
    fn another_bridges_finish_clears_held_and_reaps_cancelled_mount_without_new_starts() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let mounts = Arc::new(GatedMounts::new(store.clone()));
        let id = submit_resumable_start(&store, 1, "", READY);
        let branch = BranchName::for_agent(&resumable_agent(1));
        mounts.hold(branch.clone());
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            mounts.clone(),
            store.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        let _release = ReleaseMounts(&mounts);
        drive_work(&mut bridge, &store, || {
            mounts.calls.load(Ordering::SeqCst) == 1
        });
        assert!(bridge.held.contains_key(&id));
        assert!(bridge.starts_pending());
        assert_eq!(bridge.mounting.len(), 1);

        let claimed = store
            .claim("other", Duration::from_mins(1))
            .expect("claim")
            .expect("claimed");
        assert_eq!(claimed.command_id, id);
        store
            .finish(
                &claimed,
                &Outcome::Failed("cancelled elsewhere".to_string()),
            )
            .expect("finish");

        bridge.catch_up().expect("catch_up");
        assert!(
            !bridge.held.contains_key(&id),
            "fold_finished_command clears held"
        );
        assert!(
            !bridge.starts_pending(),
            "cancelled mount no longer counts as pending start"
        );
        assert!(
            bridge
                .mounting
                .get(&id)
                .is_some_and(|pending| pending.cancelled)
        );

        mounts.release(&branch);
        while !bridge
            .mounting
            .values()
            .all(|pending| pending.handle.is_finished())
        {
            std::thread::yield_now();
        }
        bridge.work().expect("work reaps without new starts");
        assert!(
            bridge.mounting.is_empty(),
            "work reaped cancelled mount without new starts"
        );
        assert_eq!(
            store
                .workspace_calls()
                .iter()
                .filter(|call| *call == "unmount")
                .count(),
            1
        );
    }

    #[test]
    fn follow_screen_does_not_record_ready_while_start_is_prompting() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let id = submit_resumable_start(&store, 1, "delayed prompt", BUSY);
        let terminal = TerminalName::for_execution(&ExecutionId::for_command(id));
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            store.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        drive_until(&mut bridge, &store, |bridge| !bridge.prompting.is_empty());

        store.set_pane(&terminal, READY);
        bridge.poll().expect("first poll while prompting");
        bridge.poll().expect("second poll while prompting");
        let ready_before_typing = store
            .read_global(GlobalPosition(0), 1_000)
            .expect("read")
            .into_iter()
            .filter(|record| {
                matches!(
                    record.event.kind,
                    EventKind::ScreenObserved {
                        state: ScreenState::Ready,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            ready_before_typing, 0,
            "follow_screen did not record Ready while type_prompt was still pending"
        );

        drive_work(&mut bridge, &store, || {
            command_outcome(&store, id).is_some()
        });
        assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
        let pane = store
            .capture_tail(
                &terminal,
                &WorkspaceId::new("1:/workspaces/1").expect("workspace"),
                super::TAIL_LINES,
            )
            .expect("pane");
        assert!(pane.contains("delayed prompt"), "{pane}");
    }

    #[test]
    fn failed_cancelled_mount_unmount_retries_and_records_release() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let mounts = Arc::new(GatedMounts::new(store.clone()));
        let id = submit_resumable_start(&store, 1, "", READY);
        let branch = BranchName::for_agent(&resumable_agent(1));
        mounts.hold(branch.clone());
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            mounts.clone(),
            store.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        let _release = ReleaseMounts(&mounts);
        drive_work(&mut bridge, &store, || {
            mounts.calls.load(Ordering::SeqCst) == 1
        });

        let claimed = store
            .claim("other", Duration::from_mins(1))
            .expect("claim")
            .expect("claimed");
        store
            .finish(&claimed, &Outcome::Failed("cancelled".to_string()))
            .expect("finish");

        mounts.fail_unmounts(true);
        mounts.release(&branch);
        while !bridge
            .mounting
            .values()
            .all(|pending| pending.handle.is_finished())
        {
            std::thread::yield_now();
        }
        let (work_report, _) = bridge.work().expect("work reaps failed unmount");
        assert!(
            work_report
                .notes
                .iter()
                .any(|note| note.contains("transient unmount failure")),
            "{:?}",
            work_report.notes
        );
        assert!(bridge.mounting.is_empty());
        assert_eq!(bridge.abandoned_mounts.len(), 1);
        assert!(bridge.abandoned_mounts[0].recorded_generation.is_some());
        assert!(
            store
                .read_global(GlobalPosition(0), 1_000)
                .expect("read")
                .iter()
                .any(|record| matches!(
                    record.event.kind,
                    EventKind::CommandStepped {
                        command_id,
                        step: CommandStep::Mounted,
                        ..
                    } if command_id == id
                ))
        );

        mounts.fail_unmounts(false);
        bridge.poll().expect("poll retries failed unmount");
        assert!(bridge.abandoned_mounts.is_empty());
        assert_eq!(
            store
                .workspace_calls()
                .iter()
                .filter(|call| *call == "unmount")
                .count(),
            1
        );
        assert!(
            store
                .read_global(GlobalPosition(0), 1_000)
                .expect("read")
                .iter()
                .any(|record| matches!(
                    record.event.kind,
                    EventKind::CommandStepped {
                        command_id,
                        step: CommandStep::MountReleased,
                        ..
                    } if command_id == id
                ))
        );
    }

    #[test]
    fn failed_drop_mount_unmount_is_recovered_on_restart() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let mounts = Arc::new(GatedMounts::new(store.clone()));
        let id = submit_resumable_start(&store, 1, "", READY);
        let branch = BranchName::for_agent(&resumable_agent(1));
        mounts.hold(branch.clone());
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            mounts.clone(),
            store.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock.clone());
        let _release = ReleaseMounts(&mounts);
        drive_work(&mut bridge, &store, || {
            mounts.calls.load(Ordering::SeqCst) == 1
        });

        let claimed = store
            .claim("other", Duration::from_mins(1))
            .expect("claim")
            .expect("claimed");
        store
            .finish(&claimed, &Outcome::Failed("cancelled".to_string()))
            .expect("finish");

        mounts.fail_unmounts(true);
        mounts.release(&branch);
        drop(bridge);
        assert_eq!(mounts.completed.load(Ordering::SeqCst), 1);
        assert_eq!(
            store
                .workspace_calls()
                .iter()
                .filter(|call| *call == "unmount")
                .count(),
            0
        );

        mounts.fail_unmounts(false);
        let mut restarted = Bridge::new(
            store.clone(),
            store.clone(),
            mounts.clone(),
            store.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        restarted.recover().expect("recover");
        assert_eq!(
            store
                .workspace_calls()
                .iter()
                .filter(|call| *call == "unmount")
                .count(),
            1
        );
        assert!(
            store
                .read_global(GlobalPosition(0), 1_000)
                .expect("read")
                .iter()
                .any(|record| matches!(
                    record.event.kind,
                    EventKind::CommandStepped {
                        command_id,
                        step: CommandStep::MountReleased,
                        ..
                    } if command_id == id
                ))
        );
    }

    #[test]
    fn cancelled_mount_does_not_release_another_bridges_recorded_mount() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let mounts = Arc::new(GatedMounts::new(store.clone()));
        let id = submit_resumable_start(&store, 1, "", READY);
        let branch = BranchName::for_agent(&resumable_agent(1));
        mounts.hold(branch.clone());
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            mounts.clone(),
            store.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        let _release = ReleaseMounts(&mounts);
        drive_work(&mut bridge, &store, || {
            mounts.calls.load(Ordering::SeqCst) == 1
        });

        let claimed = store
            .claim("other", Duration::from_mins(1))
            .expect("claim")
            .expect("claimed");
        let peer_workspace = WorkspaceId::new("99:/workspaces/peer").expect("ws");
        store
            .append(NewEvent {
                agent_id: resumable_agent(1),
                session_id: resumable_session(1),
                execution_id: None,
                source: Source::Bridge,
                source_event_id: Some(format!("{id}:mounted")),
                kind: EventKind::CommandStepped {
                    command_id: id,
                    step: CommandStep::Mounted,
                    workspace: Some(peer_workspace.clone()),
                },
            })
            .expect("peer recorded mounted");
        store
            .finish(&claimed, &Outcome::Failed("cancelled".to_string()))
            .expect("finish");

        mounts.fail_unmounts(true);
        mounts.release(&branch);
        while !bridge
            .mounting
            .values()
            .all(|pending| pending.handle.is_finished())
        {
            std::thread::yield_now();
        }
        bridge.work().expect("work reaps failed unmount");
        assert_eq!(bridge.abandoned_mounts.len(), 1);
        assert!(
            bridge.abandoned_mounts[0].recorded_generation.is_none(),
            "did not claim ownership of peer Bridge's mounted step"
        );

        mounts.fail_unmounts(false);
        bridge.poll().expect("poll retries unmount");
        assert!(bridge.abandoned_mounts.is_empty());
        assert_eq!(
            store
                .workspace_calls()
                .iter()
                .filter(|call| *call == "unmount")
                .count(),
            1
        );
        assert!(
            !store
                .read_global(GlobalPosition(0), 1_000)
                .expect("read")
                .iter()
                .any(|record| matches!(
                    record.event.kind,
                    EventKind::CommandStepped {
                        command_id,
                        step: CommandStep::MountReleased,
                        ..
                    } if command_id == id
                )),
            "peer Bridge's mounted step must not be marked released"
        );
        assert_ne!(peer_workspace.as_str(), "1:/workspaces/1");
    }

    struct FailReleaseLog {
        inner: Arc<MemoryStore>,
        fail_release: AtomicBool,
    }

    impl FailReleaseLog {
        fn new(inner: Arc<MemoryStore>) -> Self {
            Self {
                inner,
                fail_release: AtomicBool::new(false),
            }
        }

        fn fail_release(&self, fail: bool) {
            self.fail_release.store(fail, Ordering::SeqCst);
        }
    }

    impl EventLog for FailReleaseLog {
        fn append(
            &self,
            event: NewEvent,
        ) -> Result<crate::event::Appended, crate::port::StoreError> {
            if self.fail_release.load(Ordering::SeqCst)
                && matches!(
                    event.kind,
                    EventKind::CommandStepped {
                        step: CommandStep::MountReleased,
                        ..
                    }
                )
            {
                return Err(crate::port::StoreError::Backend(
                    "transient mount_released append failure".to_string(),
                ));
            }
            self.inner.append(event)
        }

        fn read_global(
            &self,
            after: GlobalPosition,
            limit: usize,
        ) -> Result<Vec<crate::event::EventRecord>, crate::port::StoreError> {
            self.inner.read_global(after, limit)
        }
    }

    fn unmount_calls(store: &MemoryStore) -> usize {
        store
            .workspace_calls()
            .iter()
            .filter(|call| *call == "unmount")
            .count()
    }

    fn has_command_step(
        records: &[crate::event::EventRecord],
        id: CommandId,
        target: CommandStep,
        workspace: &WorkspaceId,
    ) -> bool {
        records.iter().any(|record| {
            matches!(
                &record.event.kind,
                EventKind::CommandStepped {
                    command_id,
                    step,
                    workspace: Some(ws),
                } if *command_id == id && *step == target && ws == workspace
            )
        })
    }

    #[test]
    fn failed_mount_released_append_retries_and_remounts_unfinished_start_on_restart() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let log = Arc::new(FailReleaseLog::new(store.clone()));
        let mounts = Arc::new(GatedMounts::new(store.clone()));
        let id = submit_resumable_start(&store, 1, "", READY);
        let branch = BranchName::for_agent(&resumable_agent(1));
        mounts.hold(branch.clone());
        let mut bridge = Bridge::new(
            log.clone(),
            store.clone(),
            mounts.clone(),
            store.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock.clone());
        let _release = ReleaseMounts(&mounts);
        drive_work(&mut bridge, &store, || {
            mounts.calls.load(Ordering::SeqCst) == 1
        });

        bridge.mounting.get_mut(&id).expect("mounting").cancel();
        mounts.fail_unmounts(true);
        mounts.release(&branch);
        while !bridge
            .mounting
            .values()
            .all(|pending| pending.handle.is_finished())
        {
            std::thread::yield_now();
        }
        bridge
            .poll()
            .expect("poll persists mounted after failed unmount");
        assert_eq!(bridge.abandoned_mounts.len(), 1);
        assert!(bridge.abandoned_mounts[0].recorded_generation.is_some());
        assert!(!bridge.abandoned_mounts[0].unmounted);
        let ws1 = bridge.abandoned_mounts[0].workspace.clone();
        assert!(mounts.is_mounted(&ws1));

        mounts.fail_unmounts(false);
        log.fail_release(true);
        bridge
            .poll()
            .expect("poll unmounts ws1 but retains entry after MountReleased failure");
        assert_eq!(unmount_calls(&store), 1);
        assert!(!mounts.is_mounted(&ws1));
        assert_eq!(bridge.abandoned_mounts.len(), 1);
        assert!(bridge.abandoned_mounts[0].unmounted);

        bridge
            .poll()
            .expect("second poll retries only MountReleased without re-unmounting");
        assert_eq!(unmount_calls(&store), 1);
        assert_eq!(bridge.abandoned_mounts.len(), 1);

        drop(bridge);
        assert!(!mounts.is_mounted(&ws1));

        log.fail_release(false);
        let mut restarted = Bridge::new(
            log,
            store.clone(),
            mounts.clone(),
            store.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        restarted.recover().expect("recover");
        store.expire_leases();
        drive_work(&mut restarted, &store, || {
            command_outcome(&store, id).is_some()
        });
        assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));

        let records = store.read_global(GlobalPosition(0), 1_000).expect("read");
        let started_ws = records
            .iter()
            .find_map(|record| match &record.event.kind {
                EventKind::ExecutionStarted {
                    command_id,
                    workspace,
                    ..
                } if *command_id == id => Some(workspace.clone()),
                _ => None,
            })
            .expect("execution started");
        assert_ne!(started_ws, ws1, "reclaimed start remounted fresh workspace");
        assert!(mounts.is_mounted(&started_ws));
        assert_eq!(mounts.calls.load(Ordering::SeqCst), 2);
        assert!(has_command_step(
            &records,
            id,
            CommandStep::MountReleased,
            &ws1
        ));
        assert!(has_command_step(
            &records,
            id,
            CommandStep::Mounted,
            &started_ws
        ));
    }

    #[test]
    fn failed_mount_released_append_recovers_on_next_poll_without_remounting_or_reunmounting() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let log = Arc::new(FailReleaseLog::new(store.clone()));
        let mounts = Arc::new(GatedMounts::new(store.clone()));
        let id = submit_resumable_start(&store, 1, "", READY);
        let branch = BranchName::for_agent(&resumable_agent(1));
        mounts.hold(branch.clone());
        let mut bridge = Bridge::new(
            log.clone(),
            store.clone(),
            mounts.clone(),
            store.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        let _release = ReleaseMounts(&mounts);
        drive_work(&mut bridge, &store, || {
            mounts.calls.load(Ordering::SeqCst) == 1
        });

        let claimed = store
            .claim("other", Duration::from_mins(1))
            .expect("claim")
            .expect("claimed");
        store
            .finish(&claimed, &Outcome::Failed("cancelled".to_string()))
            .expect("finish");

        mounts.fail_unmounts(true);
        mounts.release(&branch);
        while !bridge
            .mounting
            .values()
            .all(|pending| pending.handle.is_finished())
        {
            std::thread::yield_now();
        }
        bridge.work().expect("work retains failed unmount");
        let ws1 = bridge.abandoned_mounts[0].workspace.clone();

        mounts.fail_unmounts(false);
        log.fail_release(true);
        bridge
            .poll()
            .expect("poll unmounts but retains failed append");
        assert_eq!(bridge.abandoned_mounts.len(), 1);
        assert!(bridge.abandoned_mounts[0].unmounted);
        assert_eq!(unmount_calls(&store), 1);

        log.fail_release(false);
        bridge.poll().expect("poll retries MountReleased append");
        assert!(bridge.abandoned_mounts.is_empty());
        assert_eq!(unmount_calls(&store), 1);
        let records = store.read_global(GlobalPosition(0), 1_000).expect("read");
        assert!(has_command_step(
            &records,
            id,
            CommandStep::MountReleased,
            &ws1
        ));
    }

    fn record_peer_mount(
        store: &MemoryStore,
        number: u64,
        command_id: CommandId,
        workspace: &WorkspaceId,
    ) {
        store
            .append(NewEvent {
                agent_id: resumable_agent(number),
                session_id: resumable_session(number),
                execution_id: None,
                source: Source::Bridge,
                source_event_id: Some(mounted_key(command_id, 1)),
                kind: EventKind::CommandStepped {
                    command_id,
                    step: CommandStep::Mounted,
                    workspace: Some(workspace.clone()),
                },
            })
            .expect("peer mounted");
    }

    #[test]
    fn reaping_duplicate_same_id_mount_does_not_unmount_reused_workspace() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let mounts = Arc::new(GatedMounts::new(store.clone()));
        let id = submit_resumable_start(&store, 1, "", READY);
        let branch = BranchName::for_agent(&resumable_agent(1));
        mounts.hold(branch.clone());
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            mounts.clone(),
            store.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        let _release = ReleaseMounts(&mounts);
        drive_work(&mut bridge, &store, || {
            mounts.calls.load(Ordering::SeqCst) == 1
        });

        let ws1 = store.mount(&branch).expect("peer mount");
        record_peer_mount(&store, 1, id, &ws1);
        store.expire_leases();
        bridge.work().expect("work cancels duplicate worker");
        assert_eq!(command_outcome(&store, id), None);
        let pending = bridge.mounting.get(&id).expect("still mounting");
        assert!(pending.cancelled);
        assert!(pending.cancel.load(Ordering::SeqCst));

        mounts.preset_mount(branch.clone(), ws1.clone());
        mounts.release(&branch);
        while !bridge
            .mounting
            .values()
            .all(|pending| pending.handle.is_finished())
        {
            std::thread::yield_now();
        }

        store.expire_leases();
        drive_work(&mut bridge, &store, || {
            command_outcome(&store, id).is_some()
        });
        assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
        assert_eq!(unmount_calls(&store), 0);
        assert!(mounts.is_mounted(&ws1));
        assert!(bridge.mounting.is_empty());
        assert!(bridge.abandoned_mounts.is_empty());
    }

    #[test]
    fn delayed_duplicate_same_id_mount_does_not_unmount_active_execution_on_poll_or_drop() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store1 = Arc::new(MemoryStore::new("shop", clock.clone()));
        let mounts1 = Arc::new(GatedMounts::new(store1.clone()));
        let id1 = submit_resumable_start(&store1, 1, "", READY);
        let branch1 = BranchName::for_agent(&resumable_agent(1));
        mounts1.hold(branch1.clone());
        let mut bridge1 = Bridge::new(
            store1.clone(),
            store1.clone(),
            mounts1.clone(),
            store1.clone(),
            BridgeConfig::new("bridge-1"),
        )
        .with_clock(clock.clone());
        let _release1 = ReleaseMounts(&mounts1);
        drive_work(&mut bridge1, &store1, || {
            mounts1.calls.load(Ordering::SeqCst) == 1
        });

        let ws1 = store1.mount(&branch1).expect("peer mount 1");
        record_peer_mount(&store1, 1, id1, &ws1);
        store1.expire_leases();
        let mut bridge_peer = Bridge::new(
            store1.clone(),
            store1.clone(),
            mounts1.clone(),
            store1.clone(),
            BridgeConfig::new("bridge-peer"),
        )
        .with_clock(clock.clone());
        drive_work(&mut bridge_peer, &store1, || {
            command_outcome(&store1, id1).is_some()
        });
        assert_eq!(command_outcome(&store1, id1), Some(Outcome::Succeeded));

        mounts1.preset_mount(branch1.clone(), ws1.clone());
        mounts1.release(&branch1);
        while !bridge1
            .mounting
            .values()
            .all(|pending| pending.handle.is_finished())
        {
            std::thread::yield_now();
        }
        bridge1.work().expect("work reaps delayed duplicate");
        bridge1.poll().expect("poll reaps delayed worker");
        assert_eq!(unmount_calls(&store1), 0);
        assert!(mounts1.is_mounted(&ws1));
        assert!(bridge1.mounting.is_empty());
        assert!(bridge1.abandoned_mounts.is_empty());

        let store2 = Arc::new(MemoryStore::new("shop", clock.clone()));
        let mounts2 = Arc::new(GatedMounts::new(store2.clone()));
        let id2 = submit_resumable_start(&store2, 2, "", READY);
        let branch2 = BranchName::for_agent(&resumable_agent(2));
        mounts2.hold(branch2.clone());
        let mut bridge2 = Bridge::new(
            store2.clone(),
            store2.clone(),
            mounts2.clone(),
            store2.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        let _release2 = ReleaseMounts(&mounts2);
        drive_work(&mut bridge2, &store2, || {
            mounts2.calls.load(Ordering::SeqCst) == 1
        });

        let ws2 = store2.mount(&branch2).expect("peer mount 2");
        record_peer_mount(&store2, 2, id2, &ws2);
        let ws2_alias = WorkspaceId::new("2:/symlink/workspaces/2").expect("alias");
        mounts2.canonical_alias(ws2_alias.clone(), ws2.clone());
        mounts2.preset_mount(branch2.clone(), ws2_alias);
        mounts2.release(&branch2);
        drop(bridge2);
        assert_eq!(unmount_calls(&store2), 0);
        assert!(mounts2.is_mounted(&ws2));
    }

    #[test]
    fn late_mount_when_cancel_refuses_without_failing_command_when_peer_mounted() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let mounts = Arc::new(GatedMounts::new(store.clone()));
        mounts.check_ready_after_gate(true);
        let id = submit_resumable_start(&store, 1, "", READY);
        let branch = BranchName::for_agent(&resumable_agent(1));
        mounts.hold(branch.clone());
        let mut bridge_a = Bridge::new(
            store.clone(),
            store.clone(),
            mounts.clone(),
            store.clone(),
            BridgeConfig::new("bridge-a"),
        )
        .with_clock(clock);
        let _release = ReleaseMounts(&mounts);
        drive_work(&mut bridge_a, &store, || {
            mounts.calls.load(Ordering::SeqCst) == 1
        });

        let ws1 = store.mount(&branch).expect("peer mount");
        record_peer_mount(&store, 1, id, &ws1);
        mounts.release(&branch);
        while !bridge_a
            .mounting
            .values()
            .all(|pending| pending.handle.is_finished())
        {
            std::thread::yield_now();
        }

        store.expire_leases();
        drive_work(&mut bridge_a, &store, || {
            command_outcome(&store, id).is_some()
        });
        let inner_mounts = store
            .workspace_calls()
            .iter()
            .filter(|call| call.starts_with("mount "))
            .count();
        assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
        assert_eq!(inner_mounts, 1);
        assert_eq!(unmount_calls(&store), 0);
        assert!(mounts.is_mounted(&ws1));
        assert!(bridge_a.mounting.is_empty());
    }

    #[test]
    fn abandoned_unmount_rechecks_log_under_lock_and_spares_peer_workspace() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let mounts = Arc::new(GatedMounts::new(store.clone()));
        let id = submit_resumable_start(&store, 1, "", READY);
        let branch = BranchName::for_agent(&resumable_agent(1));
        mounts.hold(branch.clone());
        let mut bridge_a = Bridge::new(
            store.clone(),
            store.clone(),
            mounts.clone(),
            store.clone(),
            BridgeConfig::new("bridge-a"),
        )
        .with_clock(clock);
        let _release = ReleaseMounts(&mounts);
        drive_work(&mut bridge_a, &store, || {
            mounts.calls.load(Ordering::SeqCst) == 1
        });

        let ws1 = store.mount(&branch).expect("peer mount");
        mounts.preset_mount(branch.clone(), ws1.clone());
        let store_hook = Arc::clone(&store);
        let ws_hook = ws1.clone();
        mounts.on_unmount_before_ready(move || {
            record_peer_mount(&store_hook, 1, id, &ws_hook);
        });
        mounts.release(&branch);
        while !bridge_a
            .mounting
            .values()
            .all(|pending| pending.handle.is_finished())
        {
            std::thread::yield_now();
        }

        drop(bridge_a);
        assert_eq!(unmount_calls(&store), 0);
        assert!(mounts.is_mounted(&ws1));
    }

    #[test]
    fn tick_report_records_single_retried_start_as_one_distinct_command_id() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let mounts = Arc::new(GatedMounts::new(store.clone()));
        let id = submit_resumable_start(&store, 1, "say hello", READY);
        let branch = BranchName::for_agent(&resumable_agent(1));
        mounts.hold(branch.clone());
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            mounts.clone(),
            store.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        let _release = ReleaseMounts(&mounts);

        let (first, _) = bridge.work().expect("pass 1");
        assert_eq!(first.commands, 1);
        assert_eq!(first.command_ids, vec![id]);
        assert!(bridge.starts_pending());

        let mut seen: BTreeSet<CommandId> = first.command_ids.iter().copied().collect();
        let mut total_claims = first.commands;
        store.expire_leases();
        mounts.release(&branch);
        for attempt in 0..1_024 {
            if command_outcome(&store, id).is_some() {
                break;
            }
            let (report, _) = bridge.work().expect("follow-up pass");
            assert_eq!(report.command_ids.len(), report.commands);
            total_claims += report.commands;
            seen.extend(report.command_ids.iter().copied());
            store.expire_leases();
            if attempt < 4 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        assert_eq!(command_outcome(&store, id), Some(Outcome::Succeeded));
        assert!(
            total_claims >= 2,
            "expected multiple claims, got {total_claims}"
        );
        assert_eq!(seen, BTreeSet::from([id]));
    }

    #[test]
    fn tick_report_records_two_distinct_command_ids_for_queued_starts_on_one_session() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let mounts = Arc::new(GatedMounts::new(store.clone()));
        let start_1 = submit_resumable_start(&store, 1, "say hello", READY);
        let start_2 = store
            .submit(
                &Principal::new("tester").expect("principal"),
                &Submission {
                    agent_id: resumable_agent(1),
                    session_id: resumable_session(1),
                    idempotency_key: IdempotencyKey::new("start-2-queued").expect("key"),
                    action: CommandAction::Start {
                        engine: ENGINE.to_string(),
                        prompt: "never".to_string(),
                        parent_session_id: None,
                        from: None,
                        model: None,
                        charter: None,
                        model_label: None,
                    },
                },
            )
            .expect("submit start-2");
        let branch = BranchName::for_agent(&resumable_agent(1));
        mounts.hold(branch.clone());
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            mounts.clone(),
            store.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(clock);
        let _release = ReleaseMounts(&mounts);

        let (first, _) = bridge.work().expect("pass 1");
        assert_eq!(first.commands, 1);
        assert_eq!(first.command_ids, vec![start_1]);
        assert!(bridge.starts_pending());
        assert_eq!(command_outcome(&store, start_2), None);

        let mut seen: BTreeSet<CommandId> = first.command_ids.iter().copied().collect();
        store.expire_leases();
        mounts.release(&branch);
        for attempt in 0..1_024 {
            if command_outcome(&store, start_1).is_some()
                && command_outcome(&store, start_2).is_some()
            {
                break;
            }
            let (report, _) = bridge.work().expect("follow-up pass");
            assert_eq!(report.command_ids.len(), report.commands);
            seen.extend(report.command_ids.iter().copied());
            store.expire_leases();
            if attempt < 4 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        assert_eq!(command_outcome(&store, start_1), Some(Outcome::Succeeded));
        let Some(Outcome::Failed(reason)) = command_outcome(&store, start_2) else {
            panic!("expected start-2 to fail");
        };
        assert!(reason.contains("already runs an execution"), "{reason}");
        assert_eq!(seen, BTreeSet::from([start_1, start_2]));
    }

    struct ConcurrentCommits {
        inner: Arc<MemoryStore>,
        active: AtomicUsize,
        peak: AtomicUsize,
        entered: Mutex<usize>,
        cv: Condvar,
    }

    impl ConcurrentCommits {
        fn new(inner: Arc<MemoryStore>) -> Arc<Self> {
            Arc::new(Self {
                inner,
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                entered: Mutex::new(0),
                cv: Condvar::new(),
            })
        }
    }

    impl WorkspaceStore for ConcurrentCommits {
        fn project(&self) -> &str {
            self.inner.project()
        }
        fn ensure_branch(
            &self,
            branch: &BranchName,
            from: Option<&str>,
        ) -> Result<crate::port::Branch, WorkspaceError> {
            self.inner.ensure_branch(branch, from)
        }
        fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError> {
            self.inner.mount(branch)
        }
        fn commit(
            &self,
            workspace: &WorkspaceId,
            message: &str,
        ) -> Result<crate::port::Commit, WorkspaceError> {
            let cur = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(cur, Ordering::SeqCst);
            {
                let mut entered = self.entered.lock().expect("entered");
                *entered += 1;
                self.cv.notify_all();
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while *entered < super::MAX_CONCURRENT_CHECKPOINTS {
                    let rem = deadline.saturating_duration_since(std::time::Instant::now());
                    if rem.is_zero() {
                        break;
                    }
                    let (next, _) = self.cv.wait_timeout(entered, rem).expect("wait");
                    entered = next;
                }
            }
            let out = self.inner.commit(workspace, message);
            self.active.fetch_sub(1, Ordering::SeqCst);
            out
        }
        fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
            self.inner.unmount(workspace)
        }
        fn is_mounted(&self, workspace: &WorkspaceId) -> bool {
            self.inner.is_mounted(workspace)
        }
        fn notices(&self, workspace: &WorkspaceId) -> Vec<String> {
            self.inner.notices(workspace)
        }
        fn notices_told(&self, workspace: &WorkspaceId, notices: &[String]) {
            self.inner.notices_told(workspace, notices);
        }
        fn branch_head(
            &self,
            branch: &BranchName,
        ) -> Result<Option<crate::id::CommitId>, WorkspaceError> {
            self.inner.branch_head(branch)
        }
    }

    #[test]
    fn act_runs_turn_checkpoints_concurrently_across_sessions_bounded_to_eight() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let commits = ConcurrentCommits::new(store.clone());
        let watch = Arc::new(MemoryWatch::new());
        let mut config = BridgeConfig::new("tester");
        config.max_commands_per_tick = 16;
        config.watch = Some(watch.clone());
        let mut ids = Vec::new();
        for number in 1..=10 {
            ids.push(submit_resumable_start(&store, number, "", READY));
        }
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            commits.clone(),
            store.clone(),
            config,
        )
        .with_clock(clock);

        drive_work(&mut bridge, &store, || {
            ids.iter().all(|id| command_outcome(&store, *id).is_some())
        });

        let mut paths: HashMap<SessionId, String> = HashMap::new();
        let mut workspaces: HashMap<SessionId, WorkspaceId> = HashMap::new();
        for record in store.read_global(GlobalPosition(0), 1_000).expect("read") {
            match record.event.kind {
                EventKind::NativeSessionStarted {
                    transcript_path: Some(path),
                    ..
                } => {
                    paths.insert(record.event.session_id, path);
                }
                EventKind::ExecutionStarted { workspace, .. } => {
                    workspaces.insert(record.event.session_id, workspace);
                }
                _ => {}
            }
        }
        assert_eq!(paths.len(), 10);
        assert_eq!(workspaces.len(), 10);

        // Dirty every workspace so every commit produces Commit::Created.
        for number in 1..=10 {
            let session = resumable_session(number);
            store.mark_dirty(&workspaces[&session]);
            let path = &paths[&session];
            store.append_file(
                path,
                &format!(
                    "{{\"type\":\"system\",\"subtype\":\"turn_duration\",\"uuid\":\"t-{number}\"}}\n"
                ),
            );
            watch.changed(path);
        }

        let _ = bridge.poll().expect("concurrent poll");
        let peak = commits.peak.load(Ordering::SeqCst);
        assert!(peak >= 2, "expected concurrent commits, saw peak {peak}");
        assert_eq!(
            peak,
            super::MAX_CONCURRENT_CHECKPOINTS,
            "expected peak bounded to MAX_CONCURRENT_CHECKPOINTS (8)"
        );

        let events = store.read_global(GlobalPosition(0), 2_000).expect("events");
        for number in 1..=10 {
            let session = resumable_session(number);
            let sess_events: Vec<&EventKind> = events
                .iter()
                .filter(|r| r.event.session_id == session)
                .map(|r| &r.event.kind)
                .collect();
            let art_pos = sess_events
                .iter()
                .position(|k| {
                    matches!(
                        k,
                        EventKind::ArtifactCommitted {
                            reason: crate::event::CheckpointReason::TurnStopped,
                            ..
                        }
                    )
                })
                .expect("ArtifactCommitted present");
            let stop_pos = sess_events
                .iter()
                .position(|k| matches!(k, EventKind::TurnStopped { .. }))
                .expect("TurnStopped present");
            assert!(
                art_pos < stop_pos,
                "{session}: ArtifactCommitted ({art_pos}) must precede TurnStopped ({stop_pos})"
            );
        }
    }

    /// A log that, once armed, refuses the next `artifact.committed` with a
    /// backend failure (not an unreachable database), then takes everything.
    struct RefuseCommitLog {
        inner: Arc<MemoryStore>,
        armed: AtomicBool,
    }

    impl EventLog for RefuseCommitLog {
        fn append(
            &self,
            event: NewEvent,
        ) -> Result<crate::event::Appended, crate::port::StoreError> {
            if matches!(event.kind, EventKind::ArtifactCommitted { .. })
                && self.armed.swap(false, Ordering::SeqCst)
            {
                return Err(crate::port::StoreError::Backend(
                    "refused artifact.committed".to_string(),
                ));
            }
            self.inner.append(event)
        }

        fn read_global(
            &self,
            after: GlobalPosition,
            limit: usize,
        ) -> Result<Vec<crate::event::EventRecord>, crate::port::StoreError> {
            self.inner.read_global(after, limit)
        }
    }

    /// The branch head of resumable agent `number`.
    fn agent_head(store: &MemoryStore, number: u64) -> crate::id::CommitId {
        store
            .branch_head(&BranchName::for_agent(&resumable_agent(number)))
            .expect("head")
            .expect("branch")
    }

    /// Dirty each running session's workspace and end its turn in its
    /// transcript, told to the watch. Returns the transcripts' paths.
    fn end_turns_on_dirty_workspaces(
        store: &MemoryStore,
        watch: &MemoryWatch,
        numbers: &[u64],
    ) -> HashMap<SessionId, String> {
        let mut paths: HashMap<SessionId, String> = HashMap::new();
        let mut workspaces: HashMap<SessionId, WorkspaceId> = HashMap::new();
        for record in store.read_global(GlobalPosition(0), 1_000).expect("read") {
            match record.event.kind {
                EventKind::NativeSessionStarted {
                    transcript_path: Some(path),
                    ..
                } => {
                    paths.insert(record.event.session_id, path);
                }
                EventKind::ExecutionStarted { workspace, .. } => {
                    workspaces.insert(record.event.session_id, workspace);
                }
                _ => {}
            }
        }
        for number in numbers {
            let session = resumable_session(*number);
            store.mark_dirty(&workspaces[&session]);
            store.append_file(
                &paths[&session],
                &format!(
                    "{{\"type\":\"system\",\"subtype\":\"turn_duration\",\"uuid\":\"t-{number}\"}}\n"
                ),
            );
            watch.changed(&paths[&session]);
        }
        paths
    }

    #[test]
    fn act_that_stops_early_keeps_the_later_sessions_precommitted_checkpoints() {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let log = Arc::new(RefuseCommitLog {
            inner: store.clone(),
            armed: AtomicBool::new(false),
        });
        let watch = Arc::new(MemoryWatch::new());
        let mut config = BridgeConfig::new("tester");
        config.watch = Some(watch.clone());
        let numbers = [1_u64, 2, 3];
        let ids: Vec<CommandId> = numbers
            .iter()
            .map(|number| submit_resumable_start(&store, *number, "", READY))
            .collect();
        let mut bridge = Bridge::new(
            log.clone(),
            store.clone(),
            store.clone(),
            store.clone(),
            config,
        )
        .with_clock(clock);
        drive_work(&mut bridge, &store, || {
            ids.iter().all(|id| command_outcome(&store, *id).is_some())
        });
        let paths = end_turns_on_dirty_workspaces(&store, &watch, &numbers);

        let recorded = || {
            store
                .read_global(GlobalPosition(0), 2_000)
                .expect("events")
                .into_iter()
                .filter(|record| matches!(record.event.kind, EventKind::ArtifactCommitted { .. }))
                .count()
        };
        let before = recorded();
        // Every turn end is precommitted in parallel; the first session's
        // record is refused, so the pass stops there.
        log.armed.store(true, Ordering::SeqCst);
        assert!(
            matches!(bridge.poll(), Err(StoreError::Backend(_))),
            "the refused append stops the pass"
        );
        assert_eq!(recorded(), before, "no checkpoint was recorded yet");
        // Every session's branch moved, and every session's commit waits to
        // be recorded, the later ones' too.
        for number in numbers {
            let session = resumable_session(number);
            let head = agent_head(&store, number);
            let waiting = bridge
                .unrecorded_commits_lock()
                .get(&session)
                .cloned()
                .unwrap_or_default();
            assert!(
                waiting.iter().any(|event| matches!(
                    &event.kind,
                    EventKind::ArtifactCommitted { artifact, .. } if artifact.commit == head
                )),
                "{session}: its precommitted {head} waits to be recorded: {waiting:?}"
            );
        }

        // The next pass records each checkpoint before its turn's end.
        for number in numbers {
            watch.changed(&paths[&resumable_session(number)]);
        }
        bridge.poll().expect("next poll");
        let events = store.read_global(GlobalPosition(0), 2_000).expect("events");
        for number in numbers {
            let session = resumable_session(number);
            let head = agent_head(&store, number);
            let kinds: Vec<&EventKind> = events
                .iter()
                .filter(|record| record.event.session_id == session)
                .map(|record| &record.event.kind)
                .collect();
            let committed = kinds
                .iter()
                .position(|kind| {
                    matches!(
                        kind,
                        EventKind::ArtifactCommitted { artifact, .. } if artifact.commit == head
                    )
                })
                .unwrap_or_else(|| panic!("{session}: {head} recorded"));
            let stopped = kinds
                .iter()
                .position(|kind| matches!(kind, EventKind::TurnStopped { .. }))
                .unwrap_or_else(|| panic!("{session}: turn.stopped recorded"));
            assert!(
                committed < stopped,
                "{session}: artifact.committed ({committed}) precedes turn.stopped ({stopped})"
            );
        }
    }

    struct RecordedQueue {
        inner: Arc<MemoryStore>,
        releases: Mutex<Vec<(CommandId, Duration)>>,
    }

    impl RecordedQueue {
        fn new(inner: Arc<MemoryStore>) -> Arc<Self> {
            Arc::new(Self {
                inner,
                releases: Mutex::new(Vec::new()),
            })
        }

        fn take_releases(&self) -> Vec<(CommandId, Duration)> {
            std::mem::take(&mut *self.releases.lock().expect("releases"))
        }
    }

    impl CommandQueue for RecordedQueue {
        fn recover_worker(
            &self,
            worker: &str,
        ) -> Result<(Box<dyn crate::port::CommandOwnership>, usize), StoreError> {
            self.inner.recover_worker(worker)
        }

        fn submit(
            &self,
            principal: &Principal,
            submission: &crate::command::Submission,
        ) -> Result<CommandId, StoreError> {
            self.inner.submit(principal, submission)
        }
        fn claim(
            &self,
            worker: &str,
            ttl: Duration,
        ) -> Result<Option<crate::command::Claimed>, StoreError> {
            self.inner.claim(worker, ttl)
        }
        fn finish(
            &self,
            claimed: &crate::command::Claimed,
            outcome: &Outcome,
        ) -> Result<(), StoreError> {
            self.inner.finish(claimed, outcome)
        }
        fn release(
            &self,
            claimed: &crate::command::Claimed,
            retry_in: Duration,
        ) -> Result<(), StoreError> {
            self.releases
                .lock()
                .expect("releases")
                .push((claimed.command_id, retry_in));
            self.inner.release(claimed, retry_in)
        }
    }

    struct SteppingTerminal {
        inner: Arc<MemoryStore>,
        clock: Arc<ManualClock>,
        step_on: Mutex<Option<(TerminalName, u64)>>,
    }

    impl SteppingTerminal {
        fn new(inner: Arc<MemoryStore>, clock: Arc<ManualClock>) -> Arc<Self> {
            Arc::new(Self {
                inner,
                clock,
                step_on: Mutex::new(None),
            })
        }

        fn arm(&self, terminal: TerminalName, millis: u64) {
            *self.step_on.lock().expect("step_on") = Some((terminal, millis));
        }
    }

    impl TerminalHost for SteppingTerminal {
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
            let advance_ms = {
                let mut slot = self.step_on.lock().expect("step_on");
                if slot.as_ref().is_some_and(|(target, _)| target == terminal) {
                    slot.take().map(|(_, millis)| millis)
                } else {
                    None
                }
            };
            if let Some(millis) = advance_ms {
                self.clock.advance(Duration::from_millis(millis));
            }
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

    fn assert_prompt_backoff_sequence(ready_interval_ms: u64, expected_ms: &[u64]) {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let queue = RecordedQueue::new(store.clone());
        let mut config = BridgeConfig::new("tester");
        config.ready_interval = Duration::from_millis(ready_interval_ms);
        config.ready_attempts = expected_ms.len() + 2;
        let cmd_id = submit_resumable_start(&store, 1, "work", BUSY);
        let mut bridge = Bridge::new(
            store.clone(),
            queue.clone(),
            store.clone(),
            store.clone(),
            config,
        )
        .with_clock(clock);
        drive_until(&mut bridge, &store, |b| b.prompting.contains_key(&cmd_id));
        let mut observed: Vec<u64> = queue
            .take_releases()
            .into_iter()
            .filter(|(id, _)| *id == cmd_id)
            .map(|(_, d)| u64::try_from(d.as_millis()).expect("ms"))
            .next_back()
            .into_iter()
            .collect();
        while observed.len() < expected_ms.len() {
            store.expire_leases();
            bridge.work().expect("work");
            for (id, d) in queue.take_releases() {
                if id == cmd_id {
                    observed.push(u64::try_from(d.as_millis()).expect("ms"));
                }
            }
        }
        assert_eq!(observed, expected_ms, "ready_interval={ready_interval_ms}");
    }

    #[test]
    fn prompt_retry_uses_progressive_backoff_and_work_subtracts_elapsed_on_release() {
        // Part A: progressive backoff sequence capped by ready_interval
        assert_prompt_backoff_sequence(250, &[25, 50, 100, 250, 250]);
        assert_prompt_backoff_sequence(40, &[25, 40, 40]);

        // Part B: elapsed time during work() is subtracted when releasing earlier waiting commands
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        let queue = RecordedQueue::new(store.clone());
        let terminals = SteppingTerminal::new(store.clone(), clock.clone());
        let mut config = BridgeConfig::new("tester");
        config.max_commands_per_tick = 16;
        config.ready_interval = Duration::from_millis(250);
        config.ready_attempts = 30;
        let cmd1 = submit_resumable_start(&store, 1, "work-1", BUSY);
        let cmd2 = submit_resumable_start(&store, 2, "work-2", BUSY);
        let mut bridge = Bridge::new(
            store.clone(),
            queue.clone(),
            store.clone(),
            terminals.clone(),
            config,
        )
        .with_clock(clock);

        drive_until(&mut bridge, &store, |b| {
            b.prompting.get(&cmd1).is_some_and(|p| p.attempts >= 4)
                && b.prompting.get(&cmd2).is_some_and(|p| p.attempts >= 4)
        });
        let _ = queue.take_releases();
        store.expire_leases();
        terminals.arm(
            TerminalName::for_execution(&ExecutionId::for_command(cmd2)),
            15,
        );
        let (report, _) = bridge.work().expect("work with elapsed advance");
        assert_eq!(report.commands, 2);
        let rels: HashMap<CommandId, u64> = queue
            .take_releases()
            .into_iter()
            .map(|(id, d)| (id, u64::try_from(d.as_millis()).expect("ms")))
            .collect();
        // Both are past attempt 4 so prompt_retry returns ready_interval (250ms); cmd1 waited 15ms while cmd2 ran capture_tail.
        assert_eq!(rels.get(&cmd1), Some(&235));
        assert_eq!(rels.get(&cmd2), Some(&250));
    }
}
