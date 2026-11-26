//! Herdr adapter: an opt-in alternative to tmux (`SPAWN_TERMINAL_BACKEND=herdr`).
//!
//! Herdr (<https://herdr.dev>) is a terminal workspace manager whose server
//! holds workspaces, tabs and panes, and answers a JSON API on a Unix socket
//! (one request per line, one answer per line). Each terminal is a Herdr
//! workspace of its own, labelled with the terminal's name, whose one pane
//! runs the launch.
//!
//! The host runs on a named Herdr session of its own (`--session`, Spawn's
//! `SPAWN_HERDR_SESSION`, `spawn` by default), never the user's default or
//! interactive one, and starts that session's headless server
//! (`herdr --session <name> server`) when a terminal starts and none
//! answers. The `herdr` program is asked where the session's socket is
//! (`status`) and starts the server; every other call goes to the socket:
//! a prompt typed into a pane is never on a command line, which every local
//! user reads in /proc and the kernel caps in size.
//!
//! A Herdr pane starts a shell and takes no command, so the launch reaches it
//! as a script: a private file (mode 600) holding the launch's environment and
//! `exec` of its program, which the pane's shell is told to `exec`. The
//! script removes itself before the program runs. The pane's shell leads a
//! session of its own (Herdr starts it with `setsid`), and `exec` keeps its
//! pid: the leader recorded at start is the runner's.
//!
//! Herdr names a pane `w1:p1`, not by the terminal's name: which pane a
//! terminal is, which server it is on and which process leads it are
//! recorded at start in a private directory under `$XDG_RUNTIME_DIR`
//! ([`HerdrHost::with_runtime_under`] in a test), so every later call, from
//! any process, finds them there.
//!
//! Herdr also detects the agent a pane runs and reads its state from the
//! screen by rules of its own (its agent manifests). The host hands that
//! reading to the Bridge as a second witness ([`TerminalHost::agent`]):
//! `agent.get` on every reading, `agent.explain` (several KB) only when the
//! status changed or is `blocked`. A thread of the host's subscribes to the
//! agents' status changes (`events.subscribe`) and queues the terminals
//! that changed ([`TerminalHost::agent_changes`]); a dropped subscription
//! is made again, the session's snapshot asked, and every followed terminal
//! counted as changed, since what changed meanwhile is unknown.
//!
//! The host speaks Herdr's protocol [`PROTOCOL`] and refuses a server that
//! speaks another, naming both. The first time it meets a server (by its
//! pid and start time) it closes the workspaces of Spawn's terminals that
//! server holds and no record of this host's names on it: workspaces a
//! restarted server restored, idle shells of a session gone with the old
//! server, which would otherwise pile up.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::{
    AgentReading, AgentStatus, Launch, Runner, ServerSeen, TerminalError, TerminalHost,
    TerminalServer,
};

use crate::process::ProcessId;
use crate::tmux::{HANGUP_WAIT, KILL_WAIT};

/// The Herdr session the host runs on when none is named.
pub const DEFAULT_SESSION: &str = "spawn";

/// How long one API call may take before it is failed: a server that
/// stopped answering must not hold the caller's tick.
const CALL_LIMIT: Duration = Duration::from_secs(10);

/// How long a screen read may take (tmux's `capture-pane` limit).
const CAPTURE_LIMIT: Duration = Duration::from_secs(5);

/// How long a server this host started has to answer.
const SERVER_START_LIMIT: Duration = Duration::from_secs(10);

/// The most bytes of staged text one `pane.send_text` carries: a long prompt
/// goes in pieces, each a whole number of characters.
const TEXT_CHUNK: usize = 16 * 1024;

/// The pause after each key [`TerminalHost::send_keys`] presses: a dialog
/// reads the first key and redraws, and keys sent in one burst can arrive
/// before it listens again (as tmux's).
const KEY_PAUSE: Duration = Duration::from_millis(200);

/// What the session's server reads as its configuration when this host
/// starts it: a plain `/bin/sh` that reads no profile in each pane (the
/// launch script is all a pane runs), no agent resumed by Herdr itself
/// after a restart (a terminal of Spawn's is Spawn's to resume), and no
/// update of Herdr nor of its agent manifests fetched behind Spawn's back:
/// the rules the agents' states are read by stay the ones Spawn was
/// checked against until the owner moves them.
const SERVER_CONFIG: &str = "[terminal]\n\
default_shell = \"/bin/sh\"\n\
shell_mode = \"non_login\"\n\
\n\
[session]\n\
resume_agents_on_restore = false\n\
\n\
[update]\n\
version_check = false\n\
manifest_check = false\n";

/// The Herdr API protocol this host speaks (herdr 0.9.5): a server that
/// answers `ping` with another is refused.
pub const PROTOCOL: u64 = 22;

/// The agent detection manifests Spawn's readings were checked against
/// (Herdr 0.9.5's bundled ones, which its cached remote ones equal on the
/// VM, 2026-11-26): a server whose active version of one of them differs
/// is refused, naming both (carry list N8). The `[update]` pin of
/// [`SERVER_CONFIG`] holds only for a server this host started; this check
/// holds for any. Moving a pin is a deliberate change: `herdr server
/// update-agent-manifests`, the fixtures run again, then this list.
pub const MANIFESTS: [(&str, &str); 2] = [("agy", "2026.10.05.1"), ("claude", "2026.10.11.1")];

/// Why a `server.agent_manifests` answer (`listed`) does not hold the
/// `pinned` manifests, naming each pinned and actual version; `None` when
/// every pinned agent's active version is the pinned one.
fn manifest_mismatch(session: &str, pinned: &[(String, String)], listed: &Value) -> Option<String> {
    let active = |agent: &str| {
        listed["manifests"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|manifest| manifest["agent"].as_str() == Some(agent))
            .and_then(|manifest| manifest["active_version"].as_str())
            .map(str::to_string)
    };
    let differ: Vec<String> = pinned
        .iter()
        .filter_map(|(agent, version)| {
            let actual = active(agent);
            (actual.as_deref() != Some(version.as_str())).then(|| {
                format!(
                    "{agent} pinned {version}, actual {}",
                    actual.as_deref().unwrap_or("none")
                )
            })
        })
        .collect();
    (!differ.is_empty()).then(|| {
        format!(
            "herdr session {session} reads agents by other detection manifests than Spawn was \
             checked against ({}): move the manifests back, or re-run the fixtures and move \
             Spawn's pins",
            differ.join("; ")
        )
    })
}

/// How long after an agent started in a pane, or was released from it, a
/// reading that Herdr still marks `launch_pending` is no reading: the
/// detection has not settled.
const LAUNCH_SETTLE: Duration = Duration::from_secs(1);

/// How long the subscriber waits for an event before it looks whether its
/// host is gone or the panes it follows changed.
const EVENT_POLL: Duration = Duration::from_millis(500);

/// The largest `agent.prompt` request line this host sends, newline
/// included: Herdr 0.9.5 resets the connection of a request line over
/// 1 MiB (probed 2026-11-26: 1 000 000 bytes of text taken, 1 100 000
/// reset), so a quarter of it. A longer text goes in [`TEXT_CHUNK`] pieces
/// through `pane.send_text`.
const PROMPT_LINE_LIMIT: usize = 256 * 1024;

/// Whether `text` goes to `pane`'s agent in one `agent.prompt`: its request
/// line, as [`call_at`] writes it, is at most [`PROMPT_LINE_LIMIT`].
pub(crate) fn prompt_fits(pane: &str, text: &str) -> bool {
    let request = json!({
        "id": "spawn:agent.prompt",
        "method": "agent.prompt",
        "params": { "target": pane, "text": text },
    });
    request.to_string().len() < PROMPT_LINE_LIMIT
}

/// Herdr's error codes that refuse input to an agent: not ready yet (it was
/// started moments ago) or blocked on a dialog. When `agent.get` answers with
/// one, nothing was typed, and the caller may try again. In practice
/// `agent.get` answers neither: `agent_not_ready` and `agent_blocked` come
/// from Herdr's agent input calls; `submit_staged` uses `agent.prompt` and
/// maps both to a refusal before typing (the text stays staged). Because
/// the Bridge records a typing before it submits, such a refusal there is
/// still read as one that may have typed: the guard that keeps it rare is
/// this check at staging. A blocked agent's status, read from
/// `agent.get`, refuses nothing here: the Bridge types only into a pane its
/// own reading accepts input on.
const NOT_TYPED: [&str; 2] = ["agent_not_ready", "agent_blocked"];

/// Whether a Herdr error code refuses input before any key went
/// ([`NOT_TYPED`]).
pub(crate) fn refused_before_typing(code: &str) -> bool {
    NOT_TYPED.contains(&code)
}

/// The last `agent.explain` of a pane, kept until its status changes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Explained {
    status: AgentStatus,
    rule: Option<String>,
    blocker: bool,
    manifest: Option<String>,
}

/// What the host and its subscriber thread share.
#[derive(Debug, Default)]
struct Agents {
    /// Followed panes, by pane id, and the terminal each is.
    panes: HashMap<String, TerminalName>,
    /// Bumped whenever `panes` changes: the subscriber subscribes again.
    generation: u64,
    /// Terminals whose agent changed, not yet drained.
    changed: Vec<TerminalName>,
    /// The last explanation of each pane's status.
    explained: HashMap<String, Explained>,
    /// When an agent was last detected in (started), or released from, a
    /// pane.
    pushed: HashMap<String, Instant>,
    /// Whether the subscriber thread runs.
    subscribed: bool,
    /// The server whose protocol was checked and whose restored workspaces
    /// were swept.
    checked: Option<TerminalServer>,
}

impl Agents {
    fn mark(&mut self, terminal: &TerminalName) {
        if !self.changed.contains(terminal) {
            self.changed.push(terminal.clone());
        }
    }

    fn mark_all(&mut self) {
        let all: Vec<TerminalName> = self.panes.values().cloned().collect();
        for terminal in &all {
            self.mark(terminal);
        }
    }
}

fn lock(agents: &Mutex<Agents>) -> std::sync::MutexGuard<'_, Agents> {
    agents
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Herdr host.
pub struct HerdrHost {
    program: String,
    session: String,
    /// [`HANGUP_WAIT`] and [`KILL_WAIT`], shorter in a test.
    waits: (Duration, Duration),
    /// What this host keeps in `$XDG_RUNTIME_DIR` goes here instead
    /// ([`HerdrHost::with_runtime_under`]): a test's own.
    runtime_base: Option<PathBuf>,
    /// The engines' projects root, pinned ([`HerdrHost::with_files_beneath`]).
    beneath: Option<crate::beneath::Beneath>,
    /// The session's API socket, once `status` said where it is.
    socket: OnceLock<PathBuf>,
    /// The protocol a server must answer `ping` with ([`PROTOCOL`]).
    protocol: u64,
    /// The manifests a server must have active ([`MANIFESTS`]).
    manifests: Vec<(String, String)>,
    /// The agents followed, shared with the subscriber thread.
    agents: Arc<Mutex<Agents>>,
}

/// What [`HerdrHost::start`] recorded of a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    /// The pane that runs the launch (`w1:p1`).
    pane: String,
    /// The workspace that holds it (`w1`).
    workspace: String,
    /// The pane's shell, which became the runner (`exec`).
    leader: ProcessId,
    /// The server the pane is on: a pane id is that server's only.
    server: TerminalServer,
}

impl Record {
    fn to_text(&self) -> String {
        json!({
            "pane": self.pane,
            "workspace": self.workspace,
            "leader": self.leader.record(),
            "server": self.server,
        })
        .to_string()
    }

    /// What [`HerdrHost::start`] writes before `workspace.create` (carry
    /// list N3): the terminal is being made on `server`, and its workspace
    /// is Spawn's even before it is named, so a sweep that lists it in
    /// between keeps it. Replaced by the full record once the pane is known.
    fn pending_text(server: TerminalServer) -> String {
        json!({ "pending": true, "server": server }).to_string()
    }

    /// The server a pending record ([`Record::pending_text`]) names.
    fn pending_server(text: &str) -> Option<TerminalServer> {
        let value: Value = serde_json::from_str(text).ok()?;
        (value["pending"] == true)
            .then(|| serde_json::from_value(value["server"].clone()).ok())
            .flatten()
    }

    fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        Some(Self {
            pane: value["pane"].as_str()?.to_string(),
            workspace: value["workspace"].as_str()?.to_string(),
            leader: ProcessId::parse(value["leader"].as_str()?)?,
            server: serde_json::from_value(value["server"].clone()).ok()?,
        })
    }
}

/// How one API call failed.
#[derive(Debug)]
enum Call {
    /// No server listens on the session's socket.
    NoServer,
    /// The server answered with an error (`pane_not_found`, ...).
    Refused { code: String, message: String },
    /// Anything else: says nothing about the terminals.
    Failed(String),
}

impl Call {
    fn into_error(self, what: &str) -> TerminalError {
        match self {
            Self::NoServer => TerminalError(format!("{what}: no herdr server is running")),
            Self::Refused { code, message } => {
                TerminalError(format!("{what}: herdr refused ({code}): {message}"))
            }
            Self::Failed(why) => TerminalError(format!("{what}: {why}")),
        }
    }

    fn gone(&self) -> bool {
        match self {
            Self::NoServer => true,
            Self::Refused { code, .. } => code.ends_with("_not_found"),
            Self::Failed(_) => false,
        }
    }
}

/// A key as Herdr takes it: a logical key name (`enter`, `ctrl+c`), or a
/// character typed as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Press {
    Key(String),
    Text(String),
}

/// One of Spawn's tmux key names (`Enter`, `Escape`, `C-c`, `BTab`, `y`) as
/// Herdr presses it, or why it has none. Every key of a call is translated
/// before the first is pressed, so an unknown one presses nothing. Herdr
/// 0.9.5 has no Home, End, page, Delete or Insert key: those are refused.
/// The digits of a numbered permission or selection prompt (spawn-core
/// `NamedKey::Digit1` to `Digit9`, tmux `1` to `9`) are single characters,
/// typed as text below, exactly as a person's keyboard sends them: Claude Code, Codex
/// CLI, AGY CLI and OpenCode pick the numbered choice on that byte.
pub(crate) fn herdr_key(key: &str) -> Result<Press, TerminalError> {
    let named = match key {
        "Enter" | "KPEnter" => Some("enter"),
        "Escape" => Some("esc"),
        "Tab" => Some("tab"),
        "BTab" => Some("shift+tab"),
        "BSpace" => Some("backspace"),
        "Space" => Some("space"),
        "Up" => Some("up"),
        "Down" => Some("down"),
        "Left" => Some("left"),
        "Right" => Some("right"),
        _ => None,
    };
    if let Some(named) = named {
        return Ok(Press::Key(named.to_string()));
    }
    let unknown = || TerminalError(format!("herdr has no key for the tmux key {key:?}"));
    if let Some(rest) = key.strip_prefix('F')
        && let Ok(number) = rest.parse::<u8>()
        && (1..=12).contains(&number)
    {
        return Ok(Press::Key(format!("f{number}")));
    }
    for (prefix, modifier) in [("C-", "ctrl"), ("M-", "alt")] {
        if let Some(rest) = key.strip_prefix(prefix) {
            let mut characters = rest.chars();
            return match (characters.next(), characters.next()) {
                (Some(character), None) if character.is_ascii_graphic() => Ok(Press::Key(format!(
                    "{modifier}+{}",
                    character.to_ascii_lowercase()
                ))),
                _ => Err(unknown()),
            };
        }
    }
    let mut characters = key.chars();
    match (characters.next(), characters.next()) {
        (Some(character), None) if !character.is_control() => Ok(Press::Text(key.to_string())),
        _ => Err(unknown()),
    }
}

/// Whether a variable of the caller's stays out of every `herdr` call (and
/// so out of the server it starts, whose environment each pane inherits):
/// the controller's settings and secrets, as for tmux ([`crate::tmux::scrub`]),
/// and every `HERDR_*` one. Spawn run inside a Herdr pane has that pane's
/// `HERDR_SOCKET_PATH`, which would aim a call at the user's own session.
pub(crate) fn scrub(name: &std::ffi::OsStr) -> bool {
    crate::tmux::scrub(name) || name.to_str().is_some_and(|name| name.starts_with("HERDR_"))
}

/// Whether `name` may name the host's Herdr session: a plain word that is
/// not Herdr's default session's.
pub(crate) fn valid_session(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "default"
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

impl HerdrHost {
    /// Host on the Herdr session `session` of the `herdr` at `program`.
    ///
    /// # Errors
    ///
    /// When `session` is no plain word (letters, digits, `_`, `-`), or is
    /// `default`: the host never runs on the user's own session.
    pub fn on_session(program: &str, session: &str) -> Result<Self, TerminalError> {
        if !valid_session(session) {
            return Err(TerminalError(format!(
                "{session:?} cannot name Spawn's herdr session: use letters, digits, `_` and `-`, \
                 and not `default` (the user's own session)"
            )));
        }
        Ok(Self {
            program: program.to_string(),
            session: session.to_string(),
            waits: (HANGUP_WAIT, KILL_WAIT),
            runtime_base: None,
            beneath: None,
            socket: OnceLock::new(),
            protocol: PROTOCOL,
            manifests: MANIFESTS
                .iter()
                .map(|(agent, version)| ((*agent).to_string(), (*version).to_string()))
                .collect(),
            agents: Arc::new(Mutex::new(Agents::default())),
        })
    }

    /// The same host, expecting a server that speaks `protocol` instead of
    /// [`PROTOCOL`]: how a test shows a server of another protocol refused.
    #[must_use]
    pub fn expecting_protocol(mut self, protocol: u64) -> Self {
        self.protocol = protocol;
        self
    }

    /// The same host, expecting a server whose active manifests are
    /// `manifests` (agent, version) instead of [`MANIFESTS`]: how a test
    /// shows a server of other manifests refused.
    #[must_use]
    pub fn expecting_manifests(mut self, manifests: &[(&str, &str)]) -> Self {
        self.manifests = manifests
            .iter()
            .map(|(agent, version)| ((*agent).to_string(), (*version).to_string()))
            .collect();
        self
    }

    /// The same host, reading, restoring and replacing every file below
    /// `projects` through it, as [`crate::TmuxHost::with_files_beneath`].
    #[must_use]
    pub fn with_files_beneath(mut self, projects: crate::beneath::Beneath) -> Self {
        self.beneath = Some(projects);
        self
    }

    /// The same host, waiting `hangup` for a closed terminal's processes to
    /// exit and `kill` after SIGKILL ([`TerminalHost::terminate`]).
    #[must_use]
    pub fn with_terminate_waits(mut self, hangup: Duration, kill: Duration) -> Self {
        self.waits = (hangup, kill);
        self
    }

    /// The same host, keeping under `base` what it would keep in
    /// `$XDG_RUNTIME_DIR` (its records, staged texts, launch scripts and the
    /// server's configuration): a test's own directory.
    #[must_use]
    pub fn with_runtime_under(mut self, base: PathBuf) -> Self {
        self.runtime_base = Some(base);
        self
    }

    /// The Herdr session the host runs on.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// The `herdr` program the host runs.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// What the session's running server says of itself, for
    /// `GET /api/capabilities`: `{protocol, expected_protocol, manifests,
    /// pinned_manifests}`, `protocol` as `ping` answers it and `manifests`
    /// as `server.agent_manifests` lists them (each `{agent,
    /// active_version}`). Two read-only calls; it never starts a server,
    /// and fails when none answers.
    pub fn server_facts(&self) -> Result<Value, TerminalError> {
        let pong = self
            .call("ping", &json!({}), CALL_LIMIT)
            .map_err(|call| call.into_error("ping"))?;
        let listed = self
            .call("server.agent_manifests", &json!({}), CALL_LIMIT)
            .map_err(|call| call.into_error("server.agent_manifests"))?;
        let manifests: Vec<Value> = listed["manifests"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|manifest| {
                json!({
                    "agent": manifest["agent"],
                    "active_version": manifest["active_version"],
                })
            })
            .collect();
        let pinned: Vec<Value> = self
            .manifests
            .iter()
            .map(|(agent, version)| json!({ "agent": agent, "version": version }))
            .collect();
        Ok(json!({
            "protocol": pong["protocol"],
            "expected_protocol": self.protocol,
            "manifests": manifests,
            "pinned_manifests": pinned,
        }))
    }

    /// Where this host keeps its files: a private directory per Herdr this
    /// host runs (its program and session) under the runtime directory, so
    /// a host on another session never reads a record of a terminal of the
    /// same name. Only while the base and both levels below it are this
    /// user's alone ([`crate::process::private_to_us`]): a record names a
    /// session that terminate SIGKILLs. `create` makes the two levels
    /// (0700) when missing.
    fn state_directory(&self, create: bool) -> Result<PathBuf, TerminalError> {
        let (base, user, directory) = self.state_levels()?;
        private(&base)?;
        for level in [&user, &directory] {
            if create {
                use std::os::unix::fs::DirBuilderExt;
                let _ = std::fs::DirBuilder::new().mode(0o700).create(level);
            }
            private(level)?;
        }
        Ok(directory)
    }

    /// The state directory for a reader: `None` only when a level below the
    /// base is missing (nothing was ever recorded). An unset or shared base,
    /// a level that is there but not this user's alone, or any other stat
    /// error is an error: it must not read as "every terminal is gone".
    fn state(&self) -> Result<Option<PathBuf>, TerminalError> {
        let (base, user, directory) = self.state_levels()?;
        private(&base)?;
        for level in [&user, &directory] {
            match std::fs::symlink_metadata(level) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(TerminalError(format!("read {}: {error}", level.display())));
                }
                Ok(_) => private(level)?,
            }
        }
        Ok(Some(directory))
    }

    /// The runtime base, and the user and host levels below it.
    fn state_levels(&self) -> Result<(PathBuf, PathBuf, PathBuf), TerminalError> {
        let base = self
            .runtime_base
            .clone()
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
            .ok_or_else(|| {
                TerminalError(
                    "the herdr host keeps its terminals in $XDG_RUNTIME_DIR, which is not set"
                        .to_string(),
                )
            })?;
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for word in [self.program.as_str(), self.session.as_str()] {
            for byte in word.bytes().chain([0]) {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        let user = base.join(format!("spawn-herdr-{}", crate::process::current_uid()));
        let directory = user.join(format!("{hash:016x}"));
        Ok((base, user, directory))
    }

    /// The `herdr` command on this host's session, with the caller's
    /// settings kept out of it ([`scrub`]).
    fn herdr(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(&self.program);
        command.arg("--session").arg(&self.session).args(arguments);
        for (name, _) in std::env::vars_os() {
            if scrub(&name) {
                command.env_remove(name);
            }
        }
        command
    }

    /// The session's API socket, as `herdr status` names it (once).
    fn socket(&self) -> Result<PathBuf, TerminalError> {
        if let Some(socket) = self.socket.get() {
            return Ok(socket.clone());
        }
        let output = self
            .herdr(&["status"])
            .stdin(Stdio::null())
            .output()
            .map_err(|error| TerminalError(format!("run {} status: {error}", self.program)))?;
        let text = String::from_utf8_lossy(&output.stdout);
        let socket = socket_of_status(&text).ok_or_else(|| {
            TerminalError(format!(
                "{} --session {} status named no socket: {}",
                self.program,
                self.session,
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        })?;
        Ok(self.socket.get_or_init(|| socket).clone())
    }

    /// One API call: `method` with `params`, the result, within `limit`.
    fn call(&self, method: &str, params: &Value, limit: Duration) -> Result<Value, Call> {
        let socket = self.socket().map_err(|error| Call::Failed(error.0))?;
        call_at(&socket, method, params, limit)
    }

    /// The server `server` checked once: it speaks [`Self::protocol`], and
    /// the workspaces of Spawn's terminals it restored are closed
    /// ([`Self::sweep`]). Asked before the first start on it and the first
    /// agent reading from it.
    fn checked(&self, server: TerminalServer) -> Result<(), TerminalError> {
        if lock(&self.agents).checked == Some(server) {
            return Ok(());
        }
        let pong = self
            .call("ping", &json!({}), CALL_LIMIT)
            .map_err(|call| call.into_error("ping"))?;
        let spoken = pong["protocol"].as_u64();
        if spoken != Some(self.protocol) {
            return Err(TerminalError(format!(
                "herdr session {} speaks protocol {}, and Spawn expects protocol {} (herdr \
                 0.9.5): install the herdr Spawn was checked against, or move Spawn to this one",
                self.session,
                spoken.map_or_else(|| "none".to_string(), |protocol| protocol.to_string()),
                self.protocol
            )));
        }
        let listed = self
            .call("server.agent_manifests", &json!({}), CALL_LIMIT)
            .map_err(|call| call.into_error("server.agent_manifests"))?;
        if let Some(refusal) = manifest_mismatch(&self.session, &self.manifests, &listed) {
            return Err(TerminalError(refusal));
        }
        self.sweep(server)?;
        lock(&self.agents).checked = Some(server);
        Ok(())
    }

    /// Close every workspace on `server` that is labelled as a terminal of
    /// Spawn's (`spawn-…`) and that no record of this host's names there:
    /// one a restarted server restored (an idle shell, its session gone
    /// with the old server), or one left by a host whose records are gone
    /// (a reboot empties the runtime directory). The record is read again
    /// right before the close, so a start that recorded its terminal since
    /// the list keeps it.
    fn sweep(&self, server: TerminalServer) -> Result<(), TerminalError> {
        let listed = self
            .call("workspace.list", &json!({}), CALL_LIMIT)
            .map_err(|call| call.into_error("workspace.list"))?;
        let state = self.state()?;
        let ours = |terminal: &TerminalName, workspace: &str| {
            state.as_deref().is_some_and(|state| {
                Self::record_in(state, terminal)
                    .ok()
                    .flatten()
                    .is_some_and(|record| record.server == server && record.workspace == workspace)
                    || Self::pending_in(state, terminal) == Some(server)
            })
        };
        for workspace in listed["workspaces"].as_array().into_iter().flatten() {
            let (Some(label), Some(id)) = (
                workspace["label"].as_str(),
                workspace["workspace_id"].as_str(),
            ) else {
                continue;
            };
            let Some(terminal) = label
                .starts_with("spawn-")
                .then(|| TerminalName::new(label).ok())
                .flatten()
            else {
                continue;
            };
            if ours(&terminal, id) {
                continue;
            }
            match self.call(
                "workspace.close",
                &json!({ "workspace_id": id }),
                CALL_LIMIT,
            ) {
                Ok(_) => {}
                Err(call) if call.gone() => {}
                Err(call) => return Err(call.into_error("workspace.close")),
            }
        }
        Ok(())
    }

    /// Follow the agent of `pane` (`terminal`): its status changes reach
    /// [`TerminalHost::agent_changes`]. Starts the subscriber thread the
    /// first time.
    fn follow_agent(&self, pane: &str, terminal: &TerminalName) {
        let start = {
            let mut agents = lock(&self.agents);
            if agents.panes.get(pane) != Some(terminal) {
                agents.panes.insert(pane.to_string(), terminal.clone());
                agents.generation = agents.generation.wrapping_add(1);
            }
            !std::mem::replace(&mut agents.subscribed, true)
        };
        if !start {
            return;
        }
        let Ok(socket) = self.socket() else {
            lock(&self.agents).subscribed = false;
            return;
        };
        let agents = Arc::downgrade(&self.agents);
        let spawned = std::thread::Builder::new()
            .name("spawn-herdr-events".to_string())
            .spawn(move || subscribe(&socket, &agents));
        if spawned.is_err() {
            lock(&self.agents).subscribed = false;
        }
    }

    /// Stop following `terminal`'s pane.
    fn unfollow(&self, terminal: &TerminalName) {
        let mut agents = lock(&self.agents);
        let panes: Vec<String> = agents
            .panes
            .iter()
            .filter(|(_, known)| *known == terminal)
            .map(|(pane, _)| pane.clone())
            .collect();
        for pane in &panes {
            agents.panes.remove(pane);
            agents.explained.remove(pane);
            agents.pushed.remove(pane);
        }
        if !panes.is_empty() {
            agents.generation = agents.generation.wrapping_add(1);
        }
    }

    /// The explanation of `pane`'s status: kept from the last
    /// `agent.explain` while the status is the same and not `blocked`,
    /// asked again otherwise. An explanation that fails says nothing (no
    /// rule, no blocker), and is asked again next time.
    fn explain(&self, pane: &str, status: AgentStatus) -> Explained {
        if status != AgentStatus::Blocked
            && let Some(known) = lock(&self.agents).explained.get(pane)
            && known.status == status
        {
            return known.clone();
        }
        let Ok(answer) = self.call("agent.explain", &json!({ "target": pane }), CALL_LIMIT) else {
            lock(&self.agents).explained.remove(pane);
            return Explained {
                status,
                rule: None,
                blocker: false,
                manifest: None,
            };
        };
        let explained = explanation(&answer["explain"], status);
        lock(&self.agents)
            .explained
            .insert(pane.to_string(), explained.clone());
        explained
    }

    /// Whether `pane`'s agent started or was released within
    /// [`LAUNCH_SETTLE`].
    fn settling(&self, pane: &str) -> bool {
        lock(&self.agents)
            .pushed
            .get(pane)
            .is_some_and(|at| at.elapsed() < LAUNCH_SETTLE)
    }

    /// Before text is staged for `terminal`: whether its agent refuses
    /// input now, as `agent.get` says: an agent started moments ago, still
    /// `launch_pending` with no status, is refused as `agent_not_ready`, and
    /// so is an `agent.get` that fails with one of [`NOT_TYPED`]. A `blocked`
    /// status is not refused here (the Bridge's own reading of the pane
    /// guards it), nor is a pane with no agent Herdr knows, or no pane: the
    /// typing finds out.
    fn refuses_input(&self, terminal: &TerminalName) -> Result<(), TerminalError> {
        let Some(record) = self.record(terminal)? else {
            return Ok(());
        };
        if !matches!(self.identity(), ServerSeen::Answers(Some(server)) if server == record.server)
        {
            return Ok(());
        }
        let refused = |code: &str| {
            TerminalError(format!(
                "herdr refused before typing ({code}): the agent in {terminal} does not take \
                 input yet; nothing was typed"
            ))
        };
        match self.call("agent.get", &json!({ "target": record.pane }), CALL_LIMIT) {
            Ok(info)
                if info["agent"]["launch_pending"].as_bool() == Some(true)
                    && AgentStatus::from_herdr(
                        info["agent"]["agent_status"].as_str().unwrap_or_default(),
                    ) == AgentStatus::Unknown =>
            {
                Err(refused("agent_not_ready"))
            }
            Err(Call::Refused { code, .. }) if refused_before_typing(&code) => Err(refused(&code)),
            _ => Ok(()),
        }
    }

    /// The server that answers on the session's socket now: its pid (the
    /// socket's peer, `SO_PEERCRED`) and when it started. Never starts one.
    fn identity(&self) -> ServerSeen {
        let Ok(socket) = self.socket() else {
            return ServerSeen::Answers(None);
        };
        match std::os::unix::net::UnixStream::connect(&socket) {
            Ok(stream) => ServerSeen::Answers(peer_server(&stream)),
            Err(error) if no_listener(&error) => ServerSeen::Gone,
            Err(_) => ServerSeen::Answers(None),
        }
    }

    /// The server, started (headless, detached, on this host's
    /// configuration) when none answers.
    fn ensure_server(&self, state: &Path) -> Result<TerminalServer, TerminalError> {
        match self.identity() {
            ServerSeen::Answers(Some(server)) => return Ok(server),
            ServerSeen::Answers(None) => {
                return Err(TerminalError(format!(
                    "herdr session {} answers, but not as a server of this user's",
                    self.session
                )));
            }
            ServerSeen::Gone => {}
        }
        crate::process::write_private(state, "config.toml", SERVER_CONFIG)
            .map_err(|error| TerminalError(format!("write the herdr configuration: {error}")))?;
        let mut command = self.herdr(&["server"]);
        command
            .env("HERDR_CONFIG_PATH", state.join("config.toml"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Its own process group: a signal to the caller's group (Ctrl-C in
        // the terminal that ran a command) does not reach it.
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command.spawn().map_err(|error| {
            TerminalError(format!(
                "start {} --session {} server: {error}",
                self.program, self.session
            ))
        })?;
        let deadline = Instant::now() + SERVER_START_LIMIT;
        loop {
            if let ServerSeen::Answers(Some(server)) = self.identity() {
                // Reaped when it exits, for as long as this process lives.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(server);
            }
            if let Ok(Some(status)) = child.try_wait() {
                // Another process's server may have won the socket.
                if let ServerSeen::Answers(Some(server)) = self.identity() {
                    return Ok(server);
                }
                return Err(TerminalError(format!(
                    "herdr server for session {} exited with {status}",
                    self.session
                )));
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(TerminalError(format!(
                    "herdr server for session {} did not answer within {} s",
                    self.session,
                    SERVER_START_LIMIT.as_secs()
                )));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn record_path(state: &Path, terminal: &TerminalName) -> PathBuf {
        state.join("terminals").join(terminal.as_str())
    }

    fn staged_path(state: &Path, terminal: &TerminalName) -> PathBuf {
        state.join("staged").join(terminal.as_str())
    }

    /// What [`TerminalHost::start`] recorded of `terminal`, if anything.
    /// Only a missing state directory or record file is "nothing recorded";
    /// a state or record that cannot be read is an error.
    fn record(&self, terminal: &TerminalName) -> Result<Option<Record>, TerminalError> {
        match self.state()? {
            None => Ok(None),
            Some(state) => Self::record_in(&state, terminal),
        }
    }

    /// The record of `terminal` in the state directory `state`.
    fn record_in(state: &Path, terminal: &TerminalName) -> Result<Option<Record>, TerminalError> {
        let path = Self::record_path(state, terminal);
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Record::parse(&text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(TerminalError(format!("read {}: {error}", path.display()))),
        }
    }

    /// The server a pending record of `terminal` in `state` names: a start
    /// that wrote it and has not named its pane yet, or one that stopped
    /// in between (carry list N3).
    fn pending_in(state: &Path, terminal: &TerminalName) -> Option<TerminalServer> {
        std::fs::read_to_string(Self::record_path(state, terminal))
            .ok()
            .and_then(|text| Record::pending_server(&text))
    }

    /// Close every workspace on the server now that is labelled `terminal`:
    /// what a start that stopped between `workspace.create` and its record
    /// left (the pending record says one may exist).
    fn close_labelled(&self, terminal: &TerminalName) -> Result<(), TerminalError> {
        let listed = match self.call("workspace.list", &json!({}), CALL_LIMIT) {
            Ok(listed) => listed,
            Err(call) if call.gone() => return Ok(()),
            Err(call) => return Err(call.into_error("workspace.list")),
        };
        for workspace in listed["workspaces"].as_array().into_iter().flatten() {
            if workspace["label"].as_str() != Some(terminal.as_str()) {
                continue;
            }
            let Some(id) = workspace["workspace_id"].as_str() else {
                continue;
            };
            match self.call(
                "workspace.close",
                &json!({ "workspace_id": id }),
                CALL_LIMIT,
            ) {
                Ok(_) => {}
                Err(call) if call.gone() => {}
                Err(call) => return Err(call.into_error("workspace.close")),
            }
        }
        Ok(())
    }

    /// Remove what this host keeps of `terminal`.
    fn forget(&self, terminal: &TerminalName) {
        if let Ok(state) = self.state_directory(false) {
            let _ = std::fs::remove_file(Self::record_path(&state, terminal));
            let _ = std::fs::remove_file(Self::staged_path(&state, terminal));
        }
    }

    /// The pane that is `terminal`: recorded, on the server that answers
    /// now. `None` when it is gone (or never was).
    fn pane(&self, terminal: &TerminalName) -> Result<Option<Record>, TerminalError> {
        let Some(record) = self.record(terminal)? else {
            return Ok(None);
        };
        match self.identity() {
            ServerSeen::Gone => return Ok(None),
            ServerSeen::Answers(Some(server)) if server != record.server => return Ok(None),
            ServerSeen::Answers(Some(_)) => {}
            ServerSeen::Answers(None) => {
                return Err(TerminalError(format!(
                    "cannot tell which herdr server holds {terminal}"
                )));
            }
        }
        match self.call("pane.get", &json!({ "pane_id": record.pane }), CALL_LIMIT) {
            Ok(_) => Ok(Some(record)),
            Err(call) if call.gone() => Ok(None),
            Err(call) => Err(call.into_error("pane.get")),
        }
    }

    /// The pane that is `terminal`, or an error that says it is gone.
    fn live_pane(&self, terminal: &TerminalName) -> Result<Record, TerminalError> {
        self.pane(terminal)?
            .ok_or_else(|| TerminalError(format!("{terminal} does not exist")))
    }

    /// Type `text` into `pane`, literally, in pieces of whole characters.
    fn send_text(&self, pane: &str, text: &str) -> Result<(), TerminalError> {
        for piece in chunks(text, TEXT_CHUNK) {
            self.call(
                "pane.send_text",
                &json!({ "pane_id": pane, "text": piece }),
                CALL_LIMIT,
            )
            .map_err(|call| call.into_error("pane.send_text"))?;
        }
        Ok(())
    }

    fn press(&self, pane: &str, press: &Press) -> Result<(), TerminalError> {
        match press {
            Press::Key(key) => self
                .call(
                    "pane.send_keys",
                    &json!({ "pane_id": pane, "keys": [key] }),
                    CALL_LIMIT,
                )
                .map(|_| ())
                .map_err(|call| call.into_error(&format!("pane.send_keys {key}"))),
            Press::Text(text) => self.send_text(pane, text),
        }
    }

    /// The pane's new workspace for `launch`, its shell told to run the
    /// launch script, and the record of it: what [`TerminalHost::start`]
    /// does once the server answers.
    fn launch(
        &self,
        state: &Path,
        launch: &Launch,
        server: TerminalServer,
        script: &Path,
    ) -> Result<Record, TerminalError> {
        // The record first (carry list N3): a sweep that lists the new
        // workspace before its pane is recorded keeps it.
        crate::process::write_private(
            &state.join("terminals"),
            launch.terminal.as_str(),
            &Record::pending_text(server),
        )
        .map_err(|error| TerminalError(format!("record {}: {error}", launch.terminal)))?;
        let created = self
            .call(
                "workspace.create",
                &json!({ "cwd": launch.cwd, "label": launch.terminal.as_str(), "focus": false }),
                CALL_LIMIT,
            )
            .map_err(|call| {
                let _ = std::fs::remove_file(Self::record_path(state, &launch.terminal));
                call.into_error("workspace.create")
            })?;
        let pane = created["root_pane"]["pane_id"].as_str().map(str::to_string);
        let workspace = created["workspace"]["workspace_id"]
            .as_str()
            .map(str::to_string);
        let (Some(pane), Some(workspace)) = (pane, workspace) else {
            let _ = std::fs::remove_file(Self::record_path(state, &launch.terminal));
            return Err(TerminalError(format!(
                "workspace.create named no pane: {created}"
            )));
        };
        let launched = (|| {
            let info = self
                .call("pane.process_info", &json!({ "pane_id": pane }), CALL_LIMIT)
                .map_err(|call| call.into_error("pane.process_info"))?;
            let shell = info["process_info"]["shell_pid"]
                .as_u64()
                .and_then(|pid| u32::try_from(pid).ok())
                .ok_or_else(|| TerminalError(format!("pane {pane} has no shell: {info}")))?;
            let record = Record {
                pane: pane.clone(),
                workspace: workspace.clone(),
                leader: ProcessId::of(shell),
                server,
            };
            crate::process::write_private(
                &state.join("terminals"),
                launch.terminal.as_str(),
                &record.to_text(),
            )
            .map_err(|error| TerminalError(format!("record {}: {error}", launch.terminal)))?;
            let line = format!(
                "exec /bin/sh {}",
                spawn_core::shell::shell_quote(&script.to_string_lossy())
            );
            self.send_text(&pane, &line)?;
            self.press(&pane, &Press::Key("enter".to_string()))?;
            Ok(record)
        })();
        if launched.is_err() {
            let _ = self.call("pane.close", &json!({ "pane_id": pane }), CALL_LIMIT);
            let _ = std::fs::remove_file(Self::record_path(state, &launch.terminal));
        }
        launched
    }
}

/// One API call on the socket at `socket`: `method` with `params`, the
/// result, within `limit`.
fn call_at(socket: &Path, method: &str, params: &Value, limit: Duration) -> Result<Value, Call> {
    let stream = match std::os::unix::net::UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(error) if no_listener(&error) => return Err(Call::NoServer),
        Err(error) => {
            return Err(Call::Failed(format!(
                "connect to {}: {error}",
                socket.display()
            )));
        }
    };
    let failed = |error: std::io::Error| Call::Failed(format!("herdr {method}: {error}"));
    stream.set_read_timeout(Some(limit)).map_err(failed)?;
    stream.set_write_timeout(Some(limit)).map_err(failed)?;
    let request = json!({ "id": format!("spawn:{method}"), "method": method, "params": params });
    let mut line = request.to_string();
    line.push('\n');
    (&stream).write_all(line.as_bytes()).map_err(failed)?;
    let mut answer = String::new();
    std::io::BufReader::new(&stream)
        .read_line(&mut answer)
        .map_err(failed)?;
    let answer: Value = serde_json::from_str(&answer)
        .map_err(|error| Call::Failed(format!("herdr {method} answered no JSON: {error}")))?;
    if let Some(error) = answer.get("error") {
        return Err(Call::Refused {
            code: error["code"].as_str().unwrap_or("unknown").to_string(),
            message: error["message"].as_str().unwrap_or_default().to_string(),
        });
    }
    Ok(answer["result"].clone())
}

/// The subscriber thread: subscribes to every agent detection and to the
/// status changes of each followed pane, and queues the terminal of each
/// change. The panes it follows changed: it subscribes again. The
/// connection dropped (the server restarted, or stopped): it connects
/// again, asks the session's snapshot, and counts every followed terminal
/// as changed, since what changed meanwhile is unknown. It ends once its
/// host is gone.
fn subscribe(socket: &Path, agents: &Weak<Mutex<Agents>>) {
    // Whether the next subscription follows a dropped connection (what
    // changed meanwhile is unknown), not the first one nor one made again
    // because the followed panes changed (carry list N7).
    let mut reconnected = false;
    loop {
        let Some(shared) = agents.upgrade() else {
            return;
        };
        let (generation, panes) = {
            let held = lock(&shared);
            (
                held.generation,
                held.panes.keys().cloned().collect::<Vec<_>>(),
            )
        };
        drop(shared);
        let Ok(stream) = std::os::unix::net::UnixStream::connect(socket) else {
            reconnected = true;
            std::thread::sleep(EVENT_POLL);
            continue;
        };
        let mut subscriptions = vec![json!({ "type": "pane.agent_detected" })];
        subscriptions.extend(
            panes
                .iter()
                .map(|pane| json!({ "type": "pane.agent_status_changed", "pane_id": pane })),
        );
        let request = json!({
            "id": "spawn:events",
            "method": "events.subscribe",
            "params": { "subscriptions": subscriptions },
        });
        let mut line = request.to_string();
        line.push('\n');
        if stream.set_read_timeout(Some(EVENT_POLL)).is_err()
            || (&stream).write_all(line.as_bytes()).is_err()
        {
            reconnected = true;
            std::thread::sleep(EVENT_POLL);
            continue;
        }
        if std::mem::take(&mut reconnected) {
            // What changed while no subscription held is unknown.
            let _ = call_at(socket, "session.snapshot", &json!({}), CALL_LIMIT);
            if let Some(shared) = agents.upgrade() {
                lock(&shared).mark_all();
            }
        }
        let mut reader = std::io::BufReader::new(stream);
        let mut line = String::new();
        let ended = loop {
            match reader.read_line(&mut line) {
                Ok(0) => break Ended::Dropped,
                Ok(_) if line.ends_with('\n') => {
                    let Some(shared) = agents.upgrade() else {
                        return;
                    };
                    pushed(&mut lock(&shared), &line);
                    line.clear();
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    let Some(shared) = agents.upgrade() else {
                        return;
                    };
                    if lock(&shared).generation != generation {
                        break Ended::Moved;
                    }
                }
                Err(_) => break Ended::Dropped,
            }
        };
        reconnected = resubscribes_after(ended);
    }
}

/// Why a subscription's read ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// The connection dropped (the server stopped or restarted).
    Dropped,
    /// The followed panes changed: the same server, subscribed again.
    Moved,
}

/// Whether the subscription made after `ended` is a real reconnect, which
/// asks the snapshot and marks every followed terminal changed (carry list
/// N7). A change of the followed panes is no reconnect: nothing was missed,
/// and marking every terminal changed would make the Bridge read each pane
/// again on every start and end.
fn resubscribes_after(ended: Ended) -> bool {
    ended == Ended::Dropped
}

/// One line the subscription pushed: a detection or a status change of a
/// followed pane queues its terminal. A detection is an agent's start, and
/// a status change to none its release: the readings of the next
/// [`LAUNCH_SETTLE`] are not trusted while Herdr still marks the launch
/// pending.
fn pushed(agents: &mut Agents, line: &str) {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let event = value["event"].as_str().unwrap_or_default();
    let Some(pane) = value["data"]["pane_id"].as_str() else {
        return;
    };
    let detected = matches!(event, "pane_agent_detected" | "pane.agent_detected");
    let status = event == "pane.agent_status_changed";
    if !detected && !status {
        return;
    }
    if detected || (status && value["data"]["agent_status"].is_null()) {
        agents.pushed.insert(pane.to_string(), Instant::now());
    }
    if let Some(terminal) = agents.panes.get(pane).cloned() {
        agents.mark(&terminal);
    }
}

/// The explanation an `agent.explain` answer holds for a pane whose status
/// is `status`: the rule that matched (none for a fallback), whether it saw
/// a dialog, and the manifest's version.
fn explanation(explain: &Value, status: AgentStatus) -> Explained {
    Explained {
        status,
        rule: explain["matched_rule"]["id"].as_str().map(str::to_string),
        blocker: explain["visible_blocker"].as_bool().unwrap_or(false),
        manifest: explain["manifest_version"].as_str().map(str::to_string),
    }
}

/// The agent an `agent.get` answer describes, as a reading: its status and
/// session, without the explanation.
fn reading_of(agent: &Value) -> AgentReading {
    let session = agent["agent_session"]
        .as_object()
        .filter(|session| session.get("kind").and_then(Value::as_str) == Some("id"))
        .and_then(|session| session.get("value"))
        .and_then(Value::as_str)
        .map(str::to_string);
    AgentReading {
        agent: agent["agent"].as_str().map(str::to_string),
        status: AgentStatus::from_herdr(agent["agent_status"].as_str().unwrap_or_default()),
        rule: None,
        blocker_visible: false,
        session,
        seq: agent["state_change_seq"].as_u64().unwrap_or(0),
        manifest: None,
    }
}

/// A reading of no agent: the pane runs none Herdr detects.
fn no_agent() -> AgentReading {
    AgentReading {
        agent: None,
        status: AgentStatus::Unknown,
        rule: None,
        blocker_visible: false,
        session: None,
        seq: 0,
        manifest: None,
    }
}

/// `text` in pieces of at most `limit` bytes, each whole characters.
pub(crate) fn chunks(text: &str, limit: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = rest.len().min(limit.max(4));
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (piece, after) = rest.split_at(end);
        pieces.push(piece);
        rest = after;
    }
    pieces
}

/// The launch as a script for `/bin/sh`: the environment exported, the
/// script removed, then the program `exec`ed with its arguments, none of
/// them read by a shell.
pub(crate) fn launch_script(launch: &Launch) -> Result<String, TerminalError> {
    let mut script = String::from("set -a\n");
    script.push_str(&crate::tmux::environment_text(&launch.environment)?);
    script.push_str("set +a\nrm -f -- \"$0\"\nexec ");
    let mut words = vec![launch.program.clone()];
    words.extend(launch.arguments.iter().cloned());
    script.push_str(&spawn_core::shell::shell_join(&words));
    script.push('\n');
    Ok(script)
}

/// The socket a `herdr status` answer names for the server.
fn socket_of_status(status: &str) -> Option<PathBuf> {
    let mut in_server = false;
    for line in status.lines() {
        if !line.starts_with(' ') {
            in_server = line.trim_end() == "server:";
            continue;
        }
        if in_server && let Some(path) = line.trim().strip_prefix("socket:") {
            let path = path.trim();
            return (!path.is_empty()).then(|| PathBuf::from(path));
        }
    }
    None
}

/// Whether a failed connect says nothing listens there.
fn no_listener(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

/// `path` is a directory only this user may enter, or the error that says
/// it is not ([`crate::process::private_to_us`]).
fn private(path: &Path) -> Result<(), TerminalError> {
    if crate::process::private_to_us(path) {
        Ok(())
    } else {
        Err(TerminalError(format!(
            "{} is not a directory only this user may enter",
            path.display()
        )))
    }
}

/// The server at the other end of `stream`, when it is this user's: its pid
/// and its start time, wall clock.
#[cfg(target_os = "linux")]
fn peer_server(stream: &std::os::unix::net::UnixStream) -> Option<TerminalServer> {
    let peer = rustix::net::sockopt::socket_peercred(stream).ok()?;
    if peer.uid.as_raw() != crate::process::current_uid() {
        return None;
    }
    let pid = u32::try_from(peer.pid.as_raw_nonzero().get()).ok()?;
    Some(TerminalServer {
        pid,
        started_at_ms: started_at_ms(crate::process::start_time(pid)?)?,
    })
}

/// No `SO_PEERCRED` (nor `/proc`) off Linux: the peer cannot be told to be
/// this user's server, so it never is, and every caller refuses it.
#[cfg(not(target_os = "linux"))]
fn peer_server(_stream: &std::os::unix::net::UnixStream) -> Option<TerminalServer> {
    None
}

/// A process start time (clock ticks after boot) as ms since the epoch.
#[cfg(target_os = "linux")]
fn started_at_ms(ticks: u64) -> Option<i64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let boot: i64 = stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    let hertz = rustix::param::clock_ticks_per_second().max(1);
    let after = i64::try_from(ticks.saturating_mul(1000) / hertz).ok()?;
    boot.checked_mul(1000)?.checked_add(after)
}

impl TerminalHost for HerdrHost {
    fn start(&self, launch: &Launch) -> Result<(), TerminalError> {
        let state = self.state_directory(true)?;
        if self.pane(&launch.terminal)?.is_some() {
            return Err(TerminalError(format!("{} already exists", launch.terminal)));
        }
        self.forget(&launch.terminal);
        let server = self.ensure_server(&state)?;
        self.checked(server)?;
        // The environment (a database login among it) never goes on a
        // command line, nor through the pane's screen: it is in the script,
        // which only this uid reads and which removes itself.
        let script = crate::tmux::write_private(
            &state,
            &launch.terminal,
            crate::tmux::LAUNCH,
            "sh",
            "the launch",
            &launch_script(launch)?,
        )?;
        let launched = self.launch(&state, launch, server, &script);
        match &launched {
            Ok(record) => {
                self.unfollow(&launch.terminal);
                self.follow_agent(&record.pane, &launch.terminal);
            }
            Err(_) => {
                let _ = std::fs::remove_file(&script);
            }
        }
        launched.map(|_| ())
    }

    /// The text goes to a private file of this host's: nothing reaches the
    /// pane. Herdr is only asked whether the pane's agent takes input now:
    /// an agent it is still starting refuses it before anything is typed
    /// ([`HerdrHost::refuses_input`]).
    fn stage_text(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
        text: &str,
    ) -> Result<(), TerminalError> {
        let state = self.state_directory(true)?;
        self.refuses_input(terminal)?;
        crate::process::write_private(&state.join("staged"), terminal.as_str(), text)
            .map_err(|error| TerminalError(format!("stage the text for {terminal}: {error}")))
    }

    fn submit_staged(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
    ) -> Result<(), TerminalError> {
        let state = self.state_directory(false)?;
        let staged = Self::staged_path(&state, terminal);
        let text = std::fs::read_to_string(&staged)
            .map_err(|error| TerminalError(format!("nothing staged for {terminal}: {error}")))?;
        let record = self.live_pane(terminal)?;
        // To the pane's agent first, through `agent.prompt` (one ordered,
        // paste-aware write, then its submission), never with `wait` (it
        // returns on a false idle): only while the request stays well under
        // Herdr's request line cap ([`prompt_fits`]). Its refusals of a
        // blocked or not-ready agent type nothing: the text stays staged.
        // A pane with no agent Herdr detects (`agent_not_found`, nothing
        // typed either) takes the text the old way, and so does an empty
        // text (Herdr refuses an empty prompt): Enter alone.
        if !text.is_empty() && prompt_fits(&record.pane, &text) {
            match self.call(
                "agent.prompt",
                &json!({ "target": record.pane, "text": text }),
                CALL_LIMIT,
            ) {
                Ok(_) => {
                    let _ = std::fs::remove_file(&staged);
                    return Ok(());
                }
                Err(Call::Refused { code, .. }) if refused_before_typing(&code) => {
                    return Err(TerminalError(format!(
                        "herdr refused before typing ({code}): the agent in {terminal} does not \
                         take input now; nothing was typed"
                    )));
                }
                Err(Call::Refused { code, .. }) if code == "agent_not_found" => {}
                Err(call) => {
                    let _ = std::fs::remove_file(&staged);
                    return Err(call.into_error("agent.prompt"));
                }
            }
        }
        // Typed as it is, then one Enter: a newline in the text is a line
        // feed, which an engine's prompt box reads as a new line, not as
        // its submission.
        let typed = self.send_text(&record.pane, &text);
        let _ = std::fs::remove_file(&staged);
        typed?;
        self.press(&record.pane, &Press::Key("enter".to_string()))
    }

    fn send_keys(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
        keys: &[&str],
    ) -> Result<(), TerminalError> {
        let presses = keys
            .iter()
            .map(|key| herdr_key(key))
            .collect::<Result<Vec<_>, _>>()?;
        let record = self.live_pane(terminal)?;
        for press in &presses {
            self.press(&record.pane, press)?;
            std::thread::sleep(KEY_PAUSE);
        }
        Ok(())
    }

    fn is_alive(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
    ) -> Result<bool, TerminalError> {
        Ok(self.pane(terminal)?.is_some())
    }

    /// The server on the session's socket, by its pid and start time. Only
    /// asks: never starts one.
    fn server(&self) -> ServerSeen {
        self.identity()
    }

    fn runner(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
    ) -> Result<Runner, TerminalError> {
        Ok(match self.pane(terminal)? {
            None => Runner::Gone,
            Some(record) => runner_of(terminal, &record),
        })
    }

    /// One `pane.list` for the whole fleet, not one call per terminal.
    fn runners(
        &self,
        terminals: &[(TerminalName, WorkspaceId)],
    ) -> Result<Vec<(TerminalName, Runner)>, TerminalError> {
        // Resolved once, before anything else: a state that cannot be read
        // is an error, never every terminal gone.
        let state = self.state()?;
        let gone = || {
            terminals
                .iter()
                .map(|(terminal, _)| (terminal.clone(), Runner::Gone))
                .collect()
        };
        let server = match self.identity() {
            ServerSeen::Gone => return Ok(gone()),
            ServerSeen::Answers(Some(server)) => server,
            ServerSeen::Answers(None) => {
                return Err(TerminalError(
                    "cannot tell which herdr server answers".to_string(),
                ));
            }
        };
        let listed = match self.call("pane.list", &json!({}), CALL_LIMIT) {
            Ok(listed) => listed,
            Err(Call::NoServer) => return Ok(gone()),
            Err(call) => return Err(call.into_error("pane.list")),
        };
        let panes: HashSet<&str> = listed["panes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|pane| pane["pane_id"].as_str())
            .collect();
        let Some(state) = state else {
            return Ok(gone());
        };
        terminals
            .iter()
            .map(|(terminal, _)| {
                let runner = match Self::record_in(&state, terminal)? {
                    Some(record)
                        if record.server == server && panes.contains(record.pane.as_str()) =>
                    {
                        runner_of(terminal, &record)
                    }
                    _ => Runner::Gone,
                };
                Ok((terminal.clone(), runner))
            })
            .collect()
    }

    fn capture_tail(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
        lines: usize,
    ) -> Result<String, TerminalError> {
        let record = self.live_pane(terminal)?;
        let read = self
            .call(
                "pane.read",
                &json!({
                    "pane_id": record.pane,
                    "source": "recent",
                    "lines": u32::try_from(lines).unwrap_or(u32::MAX),
                    "format": "text",
                }),
                CAPTURE_LIMIT,
            )
            .map_err(|call| call.into_error("pane.read"))?;
        read["read"]["text"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| TerminalError(format!("pane.read returned no text: {read}")))
    }

    /// `pane.close` (Herdr hangs up and kills what runs in the pane), then
    /// the wait until every process of the runner's session has exited, as
    /// tmux's terminate waits: the caller's final checkpoint comes next.
    fn terminate(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<(), TerminalError> {
        let Some(record) = self.record(terminal)? else {
            // A start that stopped between its pending record and its
            // pane (carry list N3): whatever workspace it made is closed.
            if let Some(state) = self.state()?
                && let Some(server) = Self::pending_in(&state, terminal)
            {
                if matches!(self.identity(), ServerSeen::Answers(Some(now)) if now == server) {
                    self.close_labelled(terminal)?;
                }
                self.forget(terminal);
            }
            return Ok(());
        };
        let leaders: Vec<u32> = if record.leader.leads_its_session() {
            vec![record.leader.pid]
        } else {
            Vec::new()
        };
        if matches!(self.identity(), ServerSeen::Answers(Some(server)) if server == record.server) {
            match self.call("pane.close", &json!({ "pane_id": record.pane }), CALL_LIMIT) {
                Ok(_) => {}
                Err(call) if call.gone() => {}
                Err(call) => return Err(call.into_error("pane.close")),
            }
            // Closing its last pane closes the workspace; one left is closed.
            let _ = self.call(
                "workspace.close",
                &json!({ "workspace_id": record.workspace }),
                CALL_LIMIT,
            );
        }
        crate::tmux::wait_sessions_gone(terminal, &leaders, self.waits, "pane.close")?;
        // What the engine detached from its session, still beneath the
        // workspace: ended by pid before the caller's final checkpoint.
        crate::tmux::end_stragglers(workspace.path(), self.waits)?;
        self.forget(terminal);
        self.unfollow(terminal);
        Ok(())
    }

    fn terminate_waits(&self) -> bool {
        true
    }

    /// `agent.get` on the terminal's pane, and `agent.explain` only when
    /// the status changed or is `blocked`. No pane, or no agent Herdr
    /// detects in it, is a reading of no agent. While Herdr still marks an
    /// agent's launch pending, within [`LAUNCH_SETTLE`] of its start or
    /// release, the reading says nothing ([`AgentStatus::Unknown`]).
    fn agent(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
    ) -> Result<Option<AgentReading>, TerminalError> {
        let Some(record) = self.pane(terminal)? else {
            return Ok(Some(no_agent()));
        };
        self.checked(record.server)?;
        self.follow_agent(&record.pane, terminal);
        let info = match self.call("agent.get", &json!({ "target": record.pane }), CALL_LIMIT) {
            Ok(info) => info,
            Err(Call::Refused { code, .. })
                if code == "agent_not_found" || refused_before_typing(&code) =>
            {
                return Ok(Some(no_agent()));
            }
            Err(call) if call.gone() => return Ok(Some(no_agent())),
            Err(call) => return Err(call.into_error("agent.get")),
        };
        let agent = &info["agent"];
        let mut reading = reading_of(agent);
        if agent["launch_pending"].as_bool() == Some(true) && self.settling(&record.pane) {
            reading.status = AgentStatus::Unknown;
            return Ok(Some(reading));
        }
        if reading.agent.is_some() {
            let explained = self.explain(&record.pane, reading.status);
            reading.rule = explained.rule;
            reading.blocker_visible = explained.blocker;
            reading.manifest = explained.manifest;
        }
        Ok(Some(reading))
    }

    fn agent_changes(&self) -> Vec<TerminalName> {
        std::mem::take(&mut lock(&self.agents).changed)
    }

    fn read_from(
        &self,
        workspace: &WorkspaceId,
        path: &str,
        offset: u64,
    ) -> Result<Option<Vec<u8>>, TerminalError> {
        crate::tmux::read_from_files(self.beneath.as_ref(), workspace, path, offset)
    }

    fn write_new(
        &self,
        _workspace: &WorkspaceId,
        path: &str,
        bytes: &[u8],
    ) -> Result<bool, TerminalError> {
        crate::tmux::write_new_file(self.beneath.as_ref(), path, bytes)
    }

    fn replace_grown(
        &self,
        _workspace: &WorkspaceId,
        path: &str,
        bytes: &[u8],
        current: &[u8],
    ) -> Result<bool, TerminalError> {
        crate::tmux::replace_grown_file(self.beneath.as_ref(), path, bytes, current)
    }
}

/// What a pane recorded as `terminal`'s, on its server, hosts: the runner
/// while the leader recorded at start lives (`exec` kept its pid), else
/// something that is not Spawn's.
fn runner_of(terminal: &TerminalName, record: &Record) -> Runner {
    if record.leader.alive() {
        Runner::Running
    } else {
        Runner::Untracked(format!(
            "the pane {} of {terminal} no longer runs the runner Spawn started there \
             (pid {} is gone)",
            record.pane, record.leader.pid
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Ended, MANIFESTS, PROMPT_LINE_LIMIT, Press, Record, chunks, herdr_key, launch_script,
        manifest_mismatch, prompt_fits, resubscribes_after, socket_of_status,
    };
    use spawn_core::id::{TerminalName, WorkspaceId};
    use spawn_core::port::{Launch, Runner, TerminalHost, TerminalServer};

    #[test]
    fn spawn_s_tmux_keys_become_herdr_keys() {
        let key = |name: &str| Press::Key(name.to_string());
        for (tmux, herdr) in [
            ("Enter", "enter"),
            ("Escape", "esc"),
            ("C-c", "ctrl+c"),
            ("C-d", "ctrl+d"),
            ("C-C", "ctrl+c"),
            ("M-x", "alt+x"),
            ("Up", "up"),
            ("Down", "down"),
            ("Left", "left"),
            ("Right", "right"),
            ("Tab", "tab"),
            ("BTab", "shift+tab"),
            ("BSpace", "backspace"),
            ("Space", "space"),
            ("F1", "f1"),
            ("F12", "f12"),
        ] {
            assert_eq!(herdr_key(tmux).expect(tmux), key(herdr), "{tmux}");
        }
        for character in ["y", "n", "1", "/", "é"] {
            assert_eq!(
                herdr_key(character).expect(character),
                Press::Text(character.to_string())
            );
        }
        for unknown in [
            "", "Return", "C-", "C-ab", "F13", "F0", "\u{1b}", "yes", "Home", "End", "PPage",
            "NPage", "DC", "IC",
        ] {
            assert!(herdr_key(unknown).is_err(), "{unknown:?}");
        }
    }

    /// Every key a person may press from the page (spawn-core `NamedKey`,
    /// the digits of a numbered prompt included) has a Herdr press: the
    /// named ones a logical key, `y`, `n` and the digits their character.
    #[test]
    fn every_named_key_a_person_presses_has_a_herdr_press() {
        use spawn_core::command::NamedKey;
        for key in NamedKey::ALL {
            let press = herdr_key(key.tmux()).unwrap_or_else(|error| panic!("{key:?}: {error}"));
            let expected = match key {
                NamedKey::Escape => Press::Key("esc".to_string()),
                NamedKey::Up => Press::Key("up".to_string()),
                NamedKey::Down => Press::Key("down".to_string()),
                NamedKey::Left => Press::Key("left".to_string()),
                NamedKey::Right => Press::Key("right".to_string()),
                NamedKey::Enter => Press::Key("enter".to_string()),
                NamedKey::Y => Press::Text("y".to_string()),
                NamedKey::N => Press::Text("n".to_string()),
                digit => Press::Text(digit.tmux().to_string()),
            };
            assert_eq!(press, expected, "{key:?}");
        }
    }

    /// An unknown key among several presses nothing: every key is
    /// translated before the host looks for the pane.
    #[test]
    fn an_unknown_key_presses_nothing() {
        let base = crate::Scratch::new("spawn-herdr-keys");
        let host = super::HerdrHost::on_session("/nonexistent/herdr", "spawn-unit")
            .expect("valid session")
            .with_runtime_under(base.to_path_buf());
        let terminal = TerminalName::new("spawn-exec_1").expect("terminal");
        let workspace = WorkspaceId::new("a:/tmp/a").expect("workspace");
        let error = host
            .send_keys(&terminal, &workspace, &["Enter", "Nope"])
            .expect_err("refused");
        assert!(error.0.contains("Nope"), "{error}");
    }

    #[test]
    fn the_controllers_settings_and_herdr_s_own_stay_out_of_herdr() {
        use std::ffi::OsStr;
        for name in [
            "SPAWN_DATABASE_URL",
            "SPAWN_SERVE_TOKEN",
            "SPAWN_TMUX_SOCKET",
            "SPAWN_HERDR_SESSION",
            "SPAWN_HERDR_PROGRAM",
            "PGPASSWORD",
            "TYPESAFE_API_KEY",
            "HERDR_SOCKET_PATH",
            "HERDR_SESSION",
            "HERDR_PANE_ID",
        ] {
            assert!(super::scrub(OsStr::new(name)), "{name}");
        }
        for name in ["PATH", "HOME", "LANG", "XDG_RUNTIME_DIR", "TERM"] {
            assert!(!super::scrub(OsStr::new(name)), "{name}");
        }
    }

    #[test]
    fn only_a_plain_session_of_spawn_s_own_is_taken() {
        for good in ["spawn", "spawn-test_1", "S2"] {
            assert!(
                super::HerdrHost::on_session("herdr", good).is_ok(),
                "{good}"
            );
        }
        for bad in ["", "default", "a b", "../x", "x/y", &"x".repeat(65)] {
            assert!(
                super::HerdrHost::on_session("herdr", bad).is_err(),
                "{bad:?}"
            );
        }
    }

    /// Staging writes a private file and nothing else: a host whose herdr
    /// does not even exist stages, and a stage that cannot be written (its
    /// runtime directory open to others) fails before anything is typed.
    #[test]
    fn staging_reaches_no_pane_and_replaces_what_was_held() {
        use std::os::unix::fs::PermissionsExt;
        let base = crate::Scratch::new("spawn-herdr-stage");
        std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(0o700))
            .expect("0700");
        let host = super::HerdrHost::on_session("/nonexistent/herdr", "spawn-unit")
            .expect("valid session")
            .with_runtime_under(base.to_path_buf());
        let terminal = TerminalName::new("spawn-exec_1").expect("terminal");
        let workspace = WorkspaceId::new("a:/tmp/a").expect("workspace");
        host.stage_text(&terminal, &workspace, "first")
            .expect("staged");
        host.stage_text(&terminal, &workspace, "second\nline")
            .expect("staged again");
        let state = host.state_directory(false).expect("state");
        let staged = super::HerdrHost::staged_path(&state, &terminal);
        assert_eq!(std::fs::read_to_string(&staged).unwrap(), "second\nline");
        assert_eq!(
            std::fs::metadata(&staged).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // No pane to type into: the submit fails, the text stays held.
        assert!(host.submit_staged(&terminal, &workspace).is_err());
        assert!(staged.exists());

        let open = crate::Scratch::new("spawn-herdr-open");
        std::fs::set_permissions(open.path(), std::fs::Permissions::from_mode(0o777))
            .expect("0777");
        let exposed = super::HerdrHost::on_session("/nonexistent/herdr", "spawn-unit")
            .expect("valid session")
            .with_runtime_under(open.to_path_buf());
        assert!(exposed.stage_text(&terminal, &workspace, "secret").is_err());
        assert_eq!(std::fs::read_dir(open.path()).unwrap().count(), 0);
    }

    /// A host over a private scratch runtime base, its herdr absent.
    fn host_under(base: &crate::Scratch) -> super::HerdrHost {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(0o700))
            .expect("0700");
        super::HerdrHost::on_session("/nonexistent/herdr", "spawn-unit")
            .expect("valid session")
            .with_runtime_under(base.to_path_buf())
    }

    /// Review #558 P2 #3: only a missing state directory means nothing was
    /// recorded. A fresh runtime base (no levels below it) reads as gone.
    #[test]
    fn a_runtime_base_with_nothing_recorded_reads_as_gone() {
        let base = crate::Scratch::new("spawn-herdr-fresh");
        let host = host_under(&base);
        let terminal = TerminalName::new("spawn-exec_1").expect("terminal");
        let workspace = WorkspaceId::new("a:/tmp/a").expect("workspace");
        assert!(matches!(
            host.runner(&terminal, &workspace).expect("runner"),
            Runner::Gone
        ));
        assert!(!host.is_alive(&terminal, &workspace).expect("alive"));
        host.terminate(&terminal, &workspace)
            .expect("nothing to end");
    }

    /// Review #558 P2 #3: a state directory no longer private to this user
    /// (after a record was written) is an error, never every terminal gone
    /// nor a terminate that "succeeded".
    #[test]
    fn a_state_that_cannot_be_trusted_is_an_error_not_gone() {
        use std::os::unix::fs::PermissionsExt;
        let base = crate::Scratch::new("spawn-herdr-shared");
        let host = host_under(&base);
        let terminal = TerminalName::new("spawn-exec_1").expect("terminal");
        let workspace = WorkspaceId::new("a:/tmp/a").expect("workspace");
        let state = host.state_directory(true).expect("state");
        let record = Record {
            pane: "w1:p1".to_string(),
            workspace: "w1".to_string(),
            leader: crate::process::ProcessId { pid: 42, start: 7 },
            server: TerminalServer {
                pid: 9,
                started_at_ms: 1_794_330_336_000,
            },
        };
        crate::process::write_private(
            &state.join("terminals"),
            terminal.as_str(),
            &record.to_text(),
        )
        .expect("recorded");
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o750)).expect("0750");
        assert!(host.runner(&terminal, &workspace).is_err());
        assert!(host.is_alive(&terminal, &workspace).is_err());
        assert!(
            host.runners(&[(terminal.clone(), workspace.clone())])
                .is_err()
        );
        assert!(host.terminate(&terminal, &workspace).is_err());
        // The record is kept for when the state is private again.
        assert!(super::HerdrHost::record_path(&state, &terminal).exists());
    }

    /// Review #558 P2 #3: a record file that is there but cannot be read is
    /// an error.
    #[test]
    fn a_record_that_cannot_be_read_is_an_error() {
        let base = crate::Scratch::new("spawn-herdr-unreadable");
        let host = host_under(&base);
        let terminal = TerminalName::new("spawn-exec_1").expect("terminal");
        let workspace = WorkspaceId::new("a:/tmp/a").expect("workspace");
        let state = host.state_directory(true).expect("state");
        // A directory where the record file belongs: reading it fails
        // whoever runs the test (root reads a 0000 file).
        std::fs::create_dir_all(super::HerdrHost::record_path(&state, &terminal)).expect("mkdir");
        assert!(host.runner(&terminal, &workspace).is_err());
        assert!(
            host.runners(&[(terminal.clone(), workspace.clone())])
                .is_err()
        );
        assert!(host.terminate(&terminal, &workspace).is_err());
    }

    #[test]
    fn a_launch_script_exports_its_environment_and_execs_its_words_unread() {
        let launch = Launch {
            terminal: TerminalName::new("spawn-exec_1").expect("terminal"),
            workspace: WorkspaceId::new("a:/tmp/a").expect("workspace"),
            cwd: "/tmp/a".to_string(),
            program: "/bin/spawn".to_string(),
            arguments: vec!["run".into(), "--".into(), "it's $HOME".into()],
            environment: vec![("SPAWN_AGENT".into(), "a'b".into())],
        };
        let script = launch_script(&launch).expect("script");
        assert_eq!(
            script,
            "set -a\nSPAWN_AGENT='a'\\''b'\nset +a\nrm -f -- \"$0\"\n\
             exec '/bin/spawn' 'run' '--' 'it'\\''s $HOME'\n"
        );
        let bad = Launch {
            environment: vec![("A B".into(), "x".into())],
            ..launch
        };
        assert!(launch_script(&bad).is_err());
    }

    #[test]
    fn long_text_goes_in_pieces_of_whole_characters() {
        let text = "aé".repeat(10);
        let pieces = chunks(&text, 4);
        assert!(pieces.iter().all(|piece| piece.len() <= 4));
        assert_eq!(pieces.concat(), text);
        assert!(chunks("", 4).is_empty());
    }

    #[test]
    fn status_names_the_server_s_socket() {
        let status = "client:\n  version: 0.9.5\n\nserver:\n  status: not running\n  \
                      socket: /home/u/.config/herdr/sessions/spawn/herdr.sock\n\nupdate:\n  \
                      restart_needed: no\n";
        assert_eq!(
            socket_of_status(status),
            Some("/home/u/.config/herdr/sessions/spawn/herdr.sock".into())
        );
        assert_eq!(socket_of_status("client:\n  socket: /x\n"), None);
        assert_eq!(socket_of_status(""), None);
    }

    #[test]
    fn a_record_reads_back_as_written() {
        let record = Record {
            pane: "w1:p1".to_string(),
            workspace: "w1".to_string(),
            leader: crate::process::ProcessId { pid: 42, start: 7 },
            server: TerminalServer {
                pid: 9,
                started_at_ms: 1_794_330_336_000,
            },
        };
        assert_eq!(Record::parse(&record.to_text()), Some(record));
        assert_eq!(Record::parse("{}"), None);
        assert_eq!(Record::parse("not json"), None);
    }

    /// Herdr's refusals of input before typing: an agent not ready (its
    /// first seconds after `agent.start`) and one blocked on a dialog. A
    /// gone pane or agent is no such refusal.
    #[test]
    fn not_ready_and_blocked_are_refusals_before_typing() {
        assert!(super::refused_before_typing("agent_not_ready"));
        assert!(super::refused_before_typing("agent_blocked"));
        for other in ["agent_not_found", "pane_not_found", "unknown", ""] {
            assert!(!super::refused_before_typing(other), "{other}");
        }
    }

    /// The server is started without update or manifest checks, and never
    /// resumes agents itself.
    #[test]
    fn the_server_s_configuration_pins_updates_and_manifests() {
        let config = super::SERVER_CONFIG;
        assert!(
            config.contains("[update]\nversion_check = false\nmanifest_check = false\n"),
            "{config}"
        );
        assert!(config.contains("resume_agents_on_restore = false"));
        assert_eq!(super::PROTOCOL, 22);
    }

    /// `agent.get` and `agent.explain` as herdr 0.9.5 answers them (live
    /// probes): `done` is idle, the session only when reported as an id,
    /// the fallback has no rule.
    #[test]
    fn readings_and_explanations_parse_as_herdr_answers() {
        use spawn_core::port::AgentStatus;
        let agent = serde_json::json!({
            "terminal_id": "term_1", "agent": "agy", "agent_status": "done",
            "pane_id": "w1:p1", "state_change_seq": 6, "launch_pending": false,
            "agent_session": {"source": "hook", "agent": "agy", "kind": "id", "value": "conv-1"},
        });
        let reading = super::reading_of(&agent);
        assert_eq!(reading.agent.as_deref(), Some("agy"));
        assert_eq!(reading.status, AgentStatus::Idle);
        assert_eq!(reading.seq, 6);
        assert_eq!(reading.session.as_deref(), Some("conv-1"));
        let by_path = serde_json::json!({
            "agent": "agy", "agent_status": "working",
            "agent_session": {"source": "hook", "agent": "agy", "kind": "path", "value": "/x"},
        });
        let reading = super::reading_of(&by_path);
        assert_eq!(reading.status, AgentStatus::Working);
        assert_eq!(reading.session, None);
        let pending = serde_json::json!({"agent_status": "unknown", "launch_pending": true});
        let reading = super::reading_of(&pending);
        assert_eq!(
            (reading.agent, reading.status),
            (None, AgentStatus::Unknown)
        );
        for (status, read) in [
            ("idle", AgentStatus::Idle),
            ("done", AgentStatus::Idle),
            ("working", AgentStatus::Working),
            ("blocked", AgentStatus::Blocked),
            ("unknown", AgentStatus::Unknown),
            ("new", AgentStatus::Unknown),
        ] {
            assert_eq!(AgentStatus::from_herdr(status), read, "{status}");
        }
        let blocked = serde_json::json!({
            "agent": "agy", "manifest_version": "2026.10.05.1", "state": "blocked",
            "matched_rule": {"id": "permission_prompt", "priority": 300, "state": "blocked"},
            "visible_blocker": true, "fallback_reason": null,
        });
        let explained = super::explanation(&blocked, AgentStatus::Blocked);
        assert_eq!(explained.rule.as_deref(), Some("permission_prompt"));
        assert!(explained.blocker);
        assert_eq!(explained.manifest.as_deref(), Some("2026.10.05.1"));
        let idle = serde_json::json!({
            "agent": "agy", "manifest_version": "2026.10.05.1", "state": "idle",
            "matched_rule": null, "visible_blocker": false,
            "fallback_reason": "default_known_agent_idle_fallback",
        });
        let explained = super::explanation(&idle, AgentStatus::Idle);
        assert_eq!(explained.rule, None);
        assert!(!explained.blocker);
    }

    /// What the subscription pushes (as herdr 0.9.5 names its events): a
    /// followed pane's detection or status change queues its terminal once;
    /// a detection or a release (a status of none) marks the pane settling;
    /// anything else is ignored.
    #[test]
    fn pushed_events_queue_their_terminal_once() {
        let mut agents = super::Agents::default();
        let terminal = TerminalName::new("spawn-exec_1").expect("terminal");
        agents.panes.insert("w1:p1".to_string(), terminal.clone());
        let status = r#"{"data":{"agent":"agy","agent_status":"working","pane_id":"w1:p1","workspace_id":"w1"},"event":"pane.agent_status_changed"}"#;
        super::pushed(&mut agents, status);
        super::pushed(&mut agents, status);
        assert_eq!(agents.changed, vec![terminal.clone()]);
        assert!(agents.pushed.is_empty(), "a status change is no start");
        let detected = r#"{"data":{"agent":"agy","pane_id":"w1:p1","type":"pane_agent_detected","workspace_id":"w1"},"event":"pane_agent_detected"}"#;
        super::pushed(&mut agents, detected);
        assert!(agents.pushed.contains_key("w1:p1"));
        agents.pushed.clear();
        let released = r#"{"data":{"agent_status":null,"pane_id":"w1:p1"},"event":"pane.agent_status_changed"}"#;
        super::pushed(&mut agents, released);
        assert!(agents.pushed.contains_key("w1:p1"));
        agents.changed.clear();
        for other in [
            r#"{"data":{"agent_status":"idle","pane_id":"w9:p1"},"event":"pane.agent_status_changed"}"#,
            r#"{"data":{"pane_id":"w1:p1"},"event":"pane.output_matched"}"#,
            r#"{"id":"spawn:events","result":{"type":"subscription_started"}}"#,
            "not json",
        ] {
            super::pushed(&mut agents, other);
        }
        assert!(agents.changed.is_empty(), "{:?}", agents.changed);
        agents.mark_all();
        assert_eq!(agents.changed, vec![terminal]);
    }

    /// `agent.prompt` carries a text only while its whole request line,
    /// JSON escapes counted, stays under the cap; above it the text goes in
    /// chunks through `pane.send_text`.
    #[test]
    fn agent_prompt_takes_a_text_only_well_under_the_request_line_cap() {
        assert!(prompt_fits("w1:p1", "hello"));
        assert!(prompt_fits("w1:p1", &"x".repeat(200 * 1024)));
        assert!(!prompt_fits("w1:p1", &"x".repeat(PROMPT_LINE_LIMIT)));
        // Escapes count: 60 KiB of quotes is 120 KiB on the line, 60 KiB of
        // control bytes is 360 KiB.
        assert!(prompt_fits("w1:p1", &"\"".repeat(60 * 1024)));
        assert!(!prompt_fits("w1:p1", &"\u{1}".repeat(60 * 1024)));
        const {
            assert!(
                PROMPT_LINE_LIMIT * 4 <= 1024 * 1024,
                "well under the probed 1 MiB cap"
            );
        };
    }

    /// Carry list N8: a server whose active manifest of a pinned agent
    /// differs, or that has none, is refused naming both versions.
    #[test]
    fn a_manifest_other_than_the_pinned_one_is_refused_naming_both() {
        let pinned: Vec<(String, String)> = MANIFESTS
            .iter()
            .map(|(agent, version)| ((*agent).to_string(), (*version).to_string()))
            .collect();
        let listed = |agy: &str| {
            serde_json::json!({ "type": "agent_manifest_status", "manifests": [
                { "agent": "pi", "active_version": "2026.10.01.1" },
                { "agent": "claude", "source": "bundled", "active_version": "2026.10.11.1" },
                { "agent": "agy", "source": "bundled", "active_version": agy },
            ]})
        };
        assert_eq!(
            manifest_mismatch("s", &pinned, &listed("2026.10.05.1")),
            None
        );
        let refused = manifest_mismatch("s", &pinned, &listed("2026.11.01.1")).expect("refused");
        assert!(
            refused.contains("agy pinned 2026.10.05.1, actual 2026.11.01.1"),
            "{refused}"
        );
        assert!(!refused.contains("claude pinned"), "{refused}");
        let none = manifest_mismatch("s", &pinned, &serde_json::json!({ "manifests": [] }))
            .expect("refused");
        assert!(
            none.contains("claude pinned 2026.10.11.1, actual none"),
            "{none}"
        );
        assert!(
            none.contains("agy pinned 2026.10.05.1, actual none"),
            "{none}"
        );
    }

    /// Carry list N7: only a dropped connection makes the next subscription
    /// ask the snapshot and mark every terminal changed.
    #[test]
    fn only_a_dropped_connection_marks_every_terminal_changed() {
        assert!(resubscribes_after(Ended::Dropped));
        assert!(!resubscribes_after(Ended::Moved));
    }

    /// Carry list N3: the record written before `workspace.create` names
    /// only the server, reads as no terminal, and is told apart.
    #[test]
    fn a_pending_record_names_its_server_and_no_terminal() {
        let server = TerminalServer {
            pid: 42,
            started_at_ms: 7,
        };
        let text = Record::pending_text(server);
        assert_eq!(Record::pending_server(&text), Some(server));
        assert_eq!(Record::parse(&text), None);
        let full = Record {
            pane: "w1:p1".to_string(),
            workspace: "w1".to_string(),
            leader: crate::process::ProcessId { pid: 42, start: 7 },
            server,
        };
        assert_eq!(Record::pending_server(&full.to_text()), None);
    }
}
