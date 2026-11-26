//! Embedded `herdr-core` adapter (`SPAWN_TERMINAL_BACKEND=herdr`, default engine `spawn-hc`).
//!
//! Communicates with an out-of-process, detached `spawn-hc` (`herdr-core`) daemon
//! over its session Unix domain socket using JSON-RPC 2.0 while running agent
//! detection outside any sandbox via [`crate::herdr_witness`].
//!
//! Because `spawn-hc serve` and `spawn-hc hold` run in their own session/service
//! independently of `spawn-loop`, killing or restarting `spawn-loop` leaves live
//! Agent PTYs and screens intact.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::{
    AgentReading, AgentStatus, Launch, Runner, ServerSeen, TerminalError, TerminalHost,
    TerminalServer,
};

use crate::herdr::{launch_script, no_listener, peer_server, private, scrub, valid_session};
use crate::herdr_witness;
use crate::process::ProcessId;
use crate::terminal_support::{HANGUP_WAIT, KILL_WAIT};

/// Default session name used when `SPAWN_HERDR_SESSION` is unset.
pub const DEFAULT_SESSION: &str = "spawn";

/// OpenRPC / wire generation expected from `spawn-hc`.
pub const EXPECTED_GENERATION: u32 = 1;

/// Fixed 8-character holder socket discriminator (`<session>.holder.<id>.sock` in `herdr-core`).
const HOLDER_ID: usize = 8;

/// Linux `sockaddr_un::sun_path` max path length excluding trailing NUL.
const MAX_SOCKET_PATH: usize = 107;

const CALL_LIMIT: Duration = Duration::from_secs(10);
const CAPTURE_LIMIT: Duration = Duration::from_secs(15);
const SEND_LIMIT: Duration = Duration::from_secs(20);
const SERVER_START_LIMIT: Duration = Duration::from_secs(20);
const LAUNCH_CLEAR_LIMIT: Duration = Duration::from_secs(2);
const LAUNCH_CLEAR_POLL: Duration = Duration::from_millis(15);
const EVENT_POLL: Duration = Duration::from_millis(500);
const KEY_PAUSE: Duration = Duration::from_millis(200);

const OWNER_LABEL: &str = "spawn_owner";
const TERMINAL_LABEL: &str = "spawn_terminal";
const WORKSPACE_LABEL: &str = "spawn_workspace";

#[derive(Debug, Clone)]
struct CorePaths {
    state: PathBuf,
    config: PathBuf,
    runtime: PathBuf,
    socket: PathBuf,
    holder_socket: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentSnapshotKey {
    agent: Option<String>,
    status: AgentStatus,
    rule: Option<String>,
    blocker_visible: bool,
}

#[derive(Debug, Clone)]
struct AgentCache {
    key: AgentSnapshotKey,
    seq: u64,
}

#[derive(Debug, Default)]
struct Agents {
    /// `herdr-core` address (`"t1"`) -> Spawn terminal name.
    addresses: HashMap<String, TerminalName>,
    /// Bumped when `addresses` changes.
    generation: u64,
    /// Terminals with pending agent changes to return from `agent_changes()`.
    changed: Vec<TerminalName>,
    /// Last witness reading per terminal, for monotonic `seq` and diff emission.
    last_readings: HashMap<TerminalName, AgentCache>,
    /// Whether the event subscriber thread has been started.
    subscribed: bool,
    /// Server identity already handshaken and swept.
    checked: Option<TerminalServer>,
}

impl Agents {
    fn mark(&mut self, terminal: &TerminalName) {
        if !self.changed.contains(terminal) {
            self.changed.push(terminal.clone());
        }
    }

    fn mark_all(&mut self) {
        let all: Vec<TerminalName> = self.addresses.values().cloned().collect();
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    address: String,
    leader: ProcessId,
    server: TerminalServer,
}

impl Record {
    fn to_text(&self) -> String {
        json!({
            "address": self.address,
            "leader": self.leader.record(),
            "server": self.server,
        })
        .to_string()
    }

    fn pending_text(server: TerminalServer) -> String {
        json!({ "pending": true, "server": server }).to_string()
    }

    fn pending_server(text: &str) -> Option<TerminalServer> {
        let value: Value = serde_json::from_str(text).ok()?;
        (value["pending"] == true)
            .then(|| serde_json::from_value(value["server"].clone()).ok())
            .flatten()
    }

    fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        Some(Self {
            address: value["address"].as_str()?.to_string(),
            leader: ProcessId::parse(value["leader"].as_str()?)?,
            server: serde_json::from_value(value["server"].clone()).ok()?,
        })
    }
}

#[derive(Debug)]
enum Call {
    NoServer,
    Refused { kind: String, message: String },
    Failed(String),
}

impl Call {
    fn into_error(self, what: &str) -> TerminalError {
        match self {
            Self::NoServer => TerminalError(format!("{what}: no herdr-core server is running")),
            Self::Refused { kind, message } => {
                TerminalError(format!("{what}: herdr-core refused ({kind}): {message}"))
            }
            Self::Failed(why) => TerminalError(format!("{what}: {why}")),
        }
    }

    fn gone(&self) -> bool {
        match self {
            Self::NoServer => true,
            Self::Refused { kind, .. } => matches!(kind.as_str(), "not_found" | "finished"),
            Self::Failed(_) => false,
        }
    }
}

/// Embedded `herdr-core` terminal host backed by sibling binary `spawn-hc`.
pub struct HerdrCoreHost {
    program: String,
    session: String,
    waits: (Duration, Duration),
    runtime_base: Option<PathBuf>,
    beneath: Option<crate::beneath::Beneath>,
    agents: Arc<Mutex<Agents>>,
}

impl HerdrCoreHost {
    /// Creates a host talking to `session` through the `spawn-hc` executable at `program`.
    pub fn on_session(program: &str, session: &str) -> Result<Self, TerminalError> {
        if !valid_session(session) {
            return Err(TerminalError(format!(
                "{session:?} cannot name Spawn's herdr session: use letters, digits, `_` and `-`, \
                 and not `default` (the user's own session)"
            )));
        }
        let host = Self {
            program: program.to_string(),
            session: session.to_string(),
            waits: (HANGUP_WAIT, KILL_WAIT),
            runtime_base: None,
            beneath: None,
            agents: Arc::new(Mutex::new(Agents::default())),
        };
        let _ = host.core_paths()?;
        Ok(host)
    }

    /// Reads, restores, and replaces every workspace file below `projects`.
    #[must_use]
    pub fn with_files_beneath(mut self, projects: crate::beneath::Beneath) -> Self {
        self.beneath = Some(projects);
        self
    }

    /// Configures terminate hangup/SIGKILL wait durations.
    #[must_use]
    pub fn with_terminate_waits(mut self, hangup: Duration, kill: Duration) -> Self {
        self.waits = (hangup, kill);
        self
    }

    /// Stores host state and `herdr-core` sockets/state under `base` (for isolated tests).
    #[must_use]
    pub fn with_runtime_under(mut self, base: PathBuf) -> Self {
        self.runtime_base = Some(base);
        self
    }

    /// Herdr session name for this host.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Path/name of the `spawn-hc` executable.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Read-only `hello` and `server.status` facts for `/api/capabilities`.
    /// Never starts a daemon.
    pub fn server_facts(&self) -> Result<Value, TerminalError> {
        let hello = self
            .call(
                "hello",
                &json!({ "generation": EXPECTED_GENERATION, "framed": false }),
                CALL_LIMIT,
            )
            .map_err(|call| call.into_error("hello"))?;
        let status = self
            .call("server.status", &json!({}), CALL_LIMIT)
            .map_err(|call| call.into_error("server.status"))?;
        Ok(json!({
            "engine": "herdr-core",
            "generation": hello["generation"],
            "expected_generation": EXPECTED_GENERATION,
            "version": status["version"],
            "pid": status["pid"],
            "uptime_ms": status["uptime_ms"],
            "live": status["live"],
            "finished": status["finished"],
            "holders": status["holders"],
            "witness": "host",
        }))
    }

    fn core_paths(&self) -> Result<CorePaths, TerminalError> {
        let paths = if let Some(base) = &self.runtime_base {
            let state = base.join("hcs").join("herdr-core").join(&self.session);
            let config = base.join("hcc").join("herdr-core").join("config.toml");
            let runtime = base.join("hcr");
            let socket = runtime.join(format!("{}.sock", self.session));
            let holder_socket = runtime.join(format!("{}.holder.sock", self.session));
            CorePaths {
                state,
                config,
                runtime,
                socket,
                holder_socket,
            }
        } else {
            let home = std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map_or_else(|| PathBuf::from("/"), PathBuf::from);
            let state_home = std::env::var_os("XDG_STATE_HOME")
                .filter(|v| !v.is_empty())
                .map_or_else(|| home.join(".local/state"), PathBuf::from);
            let config_home = std::env::var_os("XDG_CONFIG_HOME")
                .filter(|v| !v.is_empty())
                .map_or_else(|| home.join(".config"), PathBuf::from);
            let state = state_home.join("herdr-core").join(&self.session);
            let config = config_home.join("herdr-core").join("config.toml");
            let runtime = std::env::var_os("XDG_RUNTIME_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    let run_user = PathBuf::from(format!("/run/user/{}", crate::current_uid()));
                    run_user.is_dir().then_some(run_user)
                })
                .map_or_else(|| state.clone(), |run| run.join("herdr-core"));
            let socket = runtime.join(format!("{}.sock", self.session));
            let holder_socket = runtime.join(format!("{}.holder.sock", self.session));
            CorePaths {
                state,
                config,
                runtime,
                socket,
                holder_socket,
            }
        };
        let longest_holder = paths.holder_socket.as_os_str().len() + 1 + HOLDER_ID;
        let longest = paths.socket.as_os_str().len().max(longest_holder);
        if longest > MAX_SOCKET_PATH {
            return Err(TerminalError(format!(
                "herdr-core socket path for session {:?} exceeds {MAX_SOCKET_PATH} bytes ({longest} bytes)",
                self.session
            )));
        }
        Ok(paths)
    }

    fn state_levels(&self) -> Result<(PathBuf, PathBuf, PathBuf), TerminalError> {
        let base = self
            .runtime_base
            .clone()
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
            .ok_or_else(|| {
                TerminalError(
                    "herdr-core keeps Spawn terminal records in $XDG_RUNTIME_DIR, which is not set"
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
        let user = base.join(format!("spawn-hc-{}", crate::process::current_uid()));
        let directory = user.join(format!("{hash:016x}"));
        Ok((base, user, directory))
    }

    fn state_directory(&self, create: bool) -> Result<PathBuf, TerminalError> {
        let (base, user, directory) = self.state_levels()?;
        private(&base)?;
        for level in [&user, &directory] {
            if create {
                let _ = std::fs::DirBuilder::new().mode(0o700).create(level);
            }
            private(level)?;
        }
        Ok(directory)
    }

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

    fn hc_command(&self, arguments: &[&str]) -> Result<Command, TerminalError> {
        let paths = self.core_paths()?;
        let mut command = Command::new(&self.program);
        command.args(arguments);
        for (name, _) in std::env::vars_os() {
            if scrub(&name) {
                command.env_remove(name);
            }
        }
        if let Some(base) = &self.runtime_base {
            command
                .env("XDG_RUNTIME_DIR", &paths.runtime)
                .env("XDG_STATE_HOME", base.join("hcs"))
                .env("XDG_CONFIG_HOME", base.join("hcc"));
        }
        command
            .env("HC_SESSION", &self.session)
            .env("HC_SOCKET", &paths.socket);
        Ok(command)
    }

    fn identity(&self) -> ServerSeen {
        let Ok(paths) = self.core_paths() else {
            return ServerSeen::Answers(None);
        };
        match std::os::unix::net::UnixStream::connect(&paths.socket) {
            Ok(stream) => ServerSeen::Answers(peer_server(&stream)),
            Err(error) if no_listener(&error) => ServerSeen::Gone,
            Err(_) => ServerSeen::Answers(None),
        }
    }

    fn ensure_server(&self) -> Result<TerminalServer, TerminalError> {
        match self.identity() {
            ServerSeen::Answers(Some(server)) => return Ok(server),
            ServerSeen::Answers(None) => {
                return Err(TerminalError(format!(
                    "herdr-core session {} answers, but not as a server of this user's",
                    self.session
                )));
            }
            ServerSeen::Gone => {}
        }
        let paths = self.core_paths()?;
        for dir in [
            Some(paths.runtime.as_path()),
            Some(paths.state.as_path()),
            paths.config.parent(),
        ]
        .into_iter()
        .flatten()
        {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|error| {
                    TerminalError(format!("create {} for herdr-core: {error}", dir.display()))
                })?;
        }

        let mut command = self.hc_command(&["server", "start"])?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| {
            TerminalError(format!(
                "start {} server start for session {}: {error}",
                self.program, self.session
            ))
        })?;
        let deadline = Instant::now() + SERVER_START_LIMIT;
        let stderr = loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let mut err_buf = String::new();
                    if let Some(mut pipe) = child.stderr.take() {
                        let _ = std::io::Read::read_to_string(&mut pipe, &mut err_buf);
                    }
                    if let ServerSeen::Answers(Some(server)) = self.identity() {
                        return Ok(server);
                    }
                    return Err(TerminalError(format!(
                        "{} server start for session {} exited with {status}: {}",
                        self.program,
                        self.session,
                        err_buf.trim()
                    )));
                }
                Ok(None) => {
                    if let ServerSeen::Answers(Some(server)) = self.identity() {
                        let _ = child.wait();
                        return Ok(server);
                    }
                    if Instant::now() >= deadline {
                        break "timed out waiting for herdr-core server start";
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => {
                    return Err(TerminalError(format!(
                        "wait for {} server start: {error}",
                        self.program
                    )));
                }
            }
        };
        let _ = child.kill();
        let _ = child.wait();
        Err(TerminalError(format!(
            "herdr-core server for session {} did not answer within {} s ({stderr})",
            self.session,
            SERVER_START_LIMIT.as_secs()
        )))
    }

    fn call(&self, method: &str, params: &Value, limit: Duration) -> Result<Value, Call> {
        let paths = self.core_paths().map_err(|e| Call::Failed(e.0))?;
        call_at(&paths.socket, method, params, limit)
    }

    fn checked(&self, server: TerminalServer) -> Result<(), TerminalError> {
        if lock(&self.agents).checked == Some(server) {
            return Ok(());
        }
        let hello = self
            .call(
                "hello",
                &json!({ "generation": EXPECTED_GENERATION, "framed": false }),
                CALL_LIMIT,
            )
            .map_err(|call| call.into_error("hello"))?;
        let spoken = hello["generation"].as_u64();
        if spoken != Some(u64::from(EXPECTED_GENERATION)) {
            return Err(TerminalError(format!(
                "herdr-core session {} speaks generation {}, and Spawn expects generation {EXPECTED_GENERATION}",
                self.session,
                spoken.map_or_else(|| "none".to_string(), |g| g.to_string()),
            )));
        }
        self.sweep(server)?;
        lock(&self.agents).checked = Some(server);
        Ok(())
    }

    fn sweep(&self, server: TerminalServer) -> Result<(), TerminalError> {
        let listed = self
            .call(
                "terminal.list",
                &json!({ "all": false, "labels": { OWNER_LABEL: self.session } }),
                CALL_LIMIT,
            )
            .map_err(|call| call.into_error("terminal.list"))?;
        let state = self.state()?;
        let ours = |terminal: &TerminalName, address: &str, pid: Option<u32>| {
            state.as_deref().is_some_and(|state| {
                Self::record_in(state, terminal)
                    .ok()
                    .flatten()
                    .is_some_and(|record| {
                        record.address == address
                            && (record.server == server
                                || (record.leader.pid > 1
                                    && pid == Some(record.leader.pid)
                                    && record.leader.alive()))
                    })
                    || Self::pending_in(state, terminal) == Some(server)
            })
        };
        for item in listed["terminals"].as_array().into_iter().flatten() {
            let (Some(address), Some(label)) = (
                item["address"].as_str(),
                item["labels"][TERMINAL_LABEL].as_str(),
            ) else {
                continue;
            };
            let pid = item["pid"].as_u64().and_then(|p| u32::try_from(p).ok());
            let Some(terminal) = label
                .starts_with("spawn-")
                .then(|| TerminalName::new(label).ok())
                .flatten()
            else {
                continue;
            };
            if ours(&terminal, address, pid) {
                continue;
            }
            self.kill_and_remove(address)?;
        }
        Ok(())
    }

    fn kill_and_remove(&self, address: &str) -> Result<(), TerminalError> {
        match self.call("terminal.kill", &json!({ "terminal": address }), CALL_LIMIT) {
            Ok(_) => {}
            Err(call) if call.gone() => {}
            Err(call) => return Err(call.into_error("terminal.kill")),
        }
        let _ = self.call("terminal.rm", &json!({ "terminal": address }), CALL_LIMIT);
        Ok(())
    }

    fn record_path(state: &Path, terminal: &TerminalName) -> PathBuf {
        state.join("terminals").join(terminal.as_str())
    }

    fn staged_path(state: &Path, terminal: &TerminalName) -> PathBuf {
        state.join("staged").join(terminal.as_str())
    }

    fn record(&self, terminal: &TerminalName) -> Result<Option<Record>, TerminalError> {
        match self.state()? {
            None => Ok(None),
            Some(state) => Self::record_in(&state, terminal),
        }
    }

    fn record_in(state: &Path, terminal: &TerminalName) -> Result<Option<Record>, TerminalError> {
        let path = Self::record_path(state, terminal);
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Record::parse(&text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(TerminalError(format!("read {}: {error}", path.display()))),
        }
    }

    fn pending_in(state: &Path, terminal: &TerminalName) -> Option<TerminalServer> {
        std::fs::read_to_string(Self::record_path(state, terminal))
            .ok()
            .and_then(|text| Record::pending_server(&text))
    }

    fn close_labelled(&self, terminal: &TerminalName) -> Result<(), TerminalError> {
        let listed = match self.call(
            "terminal.list",
            &json!({
                "all": false,
                "labels": {
                    OWNER_LABEL: self.session,
                    TERMINAL_LABEL: terminal.as_str(),
                }
            }),
            CALL_LIMIT,
        ) {
            Ok(listed) => listed,
            Err(call) if call.gone() => return Ok(()),
            Err(call) => return Err(call.into_error("terminal.list")),
        };
        for item in listed["terminals"].as_array().into_iter().flatten() {
            if let Some(address) = item["address"].as_str() {
                self.kill_and_remove(address)?;
            }
        }
        Ok(())
    }

    fn forget(&self, terminal: &TerminalName) {
        if let Ok(state) = self.state_directory(false) {
            let _ = std::fs::remove_file(Self::record_path(&state, terminal));
            let _ = std::fs::remove_file(Self::staged_path(&state, terminal));
        }
    }

    fn live_info(
        &self,
        terminal: &TerminalName,
    ) -> Result<Option<(TerminalServer, Record, Value)>, TerminalError> {
        let Some(record) = self.record(terminal)? else {
            return Ok(None);
        };
        let server = match self.identity() {
            ServerSeen::Gone => return Ok(None),
            ServerSeen::Answers(Some(server)) => server,
            ServerSeen::Answers(None) => {
                return Err(TerminalError(format!(
                    "cannot tell which herdr-core server holds {terminal}"
                )));
            }
        };
        let info = match self.call(
            "terminal.get",
            &json!({ "terminal": record.address }),
            CALL_LIMIT,
        ) {
            Ok(info) => info,
            Err(call) if call.gone() => return Ok(None),
            Err(call) => return Err(call.into_error("terminal.get")),
        };
        let matches_terminal = info["state"].as_str() == Some("live")
            && info["labels"][TERMINAL_LABEL].as_str() == Some(terminal.as_str())
            && (record.leader.pid <= 1
                || info["pid"].as_u64() == Some(u64::from(record.leader.pid)));
        Ok(matches_terminal.then_some((server, record, info)))
    }

    fn live_record(&self, terminal: &TerminalName) -> Result<Option<Record>, TerminalError> {
        Ok(self.live_info(terminal)?.map(|(_, record, _)| record))
    }

    fn require_live_record(&self, terminal: &TerminalName) -> Result<Record, TerminalError> {
        self.live_record(terminal)?
            .ok_or_else(|| TerminalError(format!("{terminal} does not exist")))
    }

    fn follow(&self, address: &str, terminal: &TerminalName) {
        let start = {
            let mut agents = lock(&self.agents);
            if agents.addresses.get(address) != Some(terminal) {
                agents
                    .addresses
                    .insert(address.to_string(), terminal.clone());
                agents.generation = agents.generation.wrapping_add(1);
            }
            !std::mem::replace(&mut agents.subscribed, true)
        };
        if !start {
            return;
        }
        let Ok(paths) = self.core_paths() else {
            lock(&self.agents).subscribed = false;
            return;
        };
        let session = self.session.clone();
        let agents = Arc::downgrade(&self.agents);
        let spawned = std::thread::Builder::new()
            .name("spawn-hc-events".to_string())
            .spawn(move || subscribe(&paths.socket, &session, &agents));
        if spawned.is_err() {
            lock(&self.agents).subscribed = false;
        }
    }

    fn unfollow(&self, terminal: &TerminalName) {
        let mut agents = lock(&self.agents);
        let addrs: Vec<String> = agents
            .addresses
            .iter()
            .filter(|(_, known)| *known == terminal)
            .map(|(addr, _)| addr.clone())
            .collect();
        for addr in &addrs {
            agents.addresses.remove(addr);
        }
        agents.last_readings.remove(terminal);
        if !addrs.is_empty() {
            agents.generation = agents.generation.wrapping_add(1);
        }
    }

    fn launch_terminal(
        &self,
        state: &Path,
        launch: &Launch,
        server: TerminalServer,
        script: &Path,
    ) -> Result<Record, TerminalError> {
        crate::process::write_private(
            &state.join("terminals"),
            launch.terminal.as_str(),
            &Record::pending_text(server),
        )
        .map_err(|error| TerminalError(format!("record {}: {error}", launch.terminal)))?;

        let started = self
            .call(
                "terminal.run",
                &json!({
                    "argv": ["/bin/sh", script.to_string_lossy()],
                    "cwd": launch.cwd,
                    "size": { "cols": 200, "rows": 120 },
                    "labels": {
                        OWNER_LABEL: self.session,
                        TERMINAL_LABEL: launch.terminal.as_str(),
                        WORKSPACE_LABEL: launch.workspace.as_str(),
                    },
                    "no_parent": true,
                    "no_notify": true,
                    "keep": false,
                    "rm": false,
                    "wait": false,
                }),
                CALL_LIMIT,
            )
            .map_err(|call| {
                let _ = std::fs::remove_file(Self::record_path(state, &launch.terminal));
                call.into_error("terminal.run")
            })?;

        let Some(address) = started["address"].as_str().map(str::to_string) else {
            let _ = std::fs::remove_file(Self::record_path(state, &launch.terminal));
            return Err(TerminalError(format!(
                "terminal.run returned no address: {started}"
            )));
        };

        let result = (|| {
            let pid = self.resolve_started_pid(&address, &started)?;
            let record = Record {
                address: address.clone(),
                leader: ProcessId::of(pid),
                server,
            };
            crate::process::write_private(
                &state.join("terminals"),
                launch.terminal.as_str(),
                &record.to_text(),
            )
            .map_err(|error| TerminalError(format!("record {}: {error}", launch.terminal)))?;
            await_script_removed(script);
            Ok(record)
        })();

        if result.is_err() {
            let _ = self.kill_and_remove(&address);
            let _ = std::fs::remove_file(Self::record_path(state, &launch.terminal));
        }
        result
    }

    fn resolve_started_pid(&self, address: &str, initial: &Value) -> Result<u32, TerminalError> {
        if let Some(pid) = initial["pid"].as_u64().and_then(|p| u32::try_from(p).ok()) {
            return Ok(pid);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.call("terminal.get", &json!({ "terminal": address }), CALL_LIMIT) {
                Ok(info) => {
                    if let Some(pid) = info["pid"].as_u64().and_then(|p| u32::try_from(p).ok()) {
                        return Ok(pid);
                    }
                    if info["state"].as_str() == Some("finished") {
                        return Ok(0);
                    }
                }
                Err(call) if call.gone() => return Ok(0),
                Err(call) => return Err(call.into_error("terminal.get")),
            }
            if Instant::now() >= deadline {
                return Err(TerminalError(format!(
                    "herdr-core terminal {address} started without a pid"
                )));
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }
}

impl TerminalHost for HerdrCoreHost {
    fn start(&self, launch: &Launch) -> Result<(), TerminalError> {
        let state = self.state_directory(true)?;
        if self.live_record(&launch.terminal)?.is_some() {
            return Err(TerminalError(format!("{} already exists", launch.terminal)));
        }
        self.forget(&launch.terminal);
        let server = self.ensure_server()?;
        self.checked(server)?;
        let script = crate::terminal_support::write_private(
            &state,
            &launch.terminal,
            crate::terminal_support::LAUNCH,
            "sh",
            "the launch",
            &launch_script(launch)?,
        )?;
        let launched = self.launch_terminal(&state, launch, server, &script);
        match &launched {
            Ok(record) => {
                self.unfollow(&launch.terminal);
                self.follow(&record.address, &launch.terminal);
            }
            Err(_) => {
                let _ = std::fs::remove_file(&script);
            }
        }
        launched.map(|_| ())
    }

    fn stage_text(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
        text: &str,
    ) -> Result<(), TerminalError> {
        let state = self.state_directory(true)?;
        if let Some(record) = self.live_record(terminal)?
            && let Ok(info) = self.call(
                "terminal.get",
                &json!({ "terminal": record.address }),
                CALL_LIMIT,
            )
            && info["reason"].as_str() == Some("spawn_pending")
        {
            return Err(TerminalError(format!(
                "herdr-core refused before typing (agent_not_ready): the agent in {terminal} \
                 does not take input yet; nothing was typed"
            )));
        }
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
        let record = self.require_live_record(terminal)?;
        if text.is_empty() {
            let _ = std::fs::remove_file(&staged);
            return self
                .call(
                    "terminal.keys",
                    &json!({ "terminal": record.address, "keys": ["enter"] }),
                    CALL_LIMIT,
                )
                .map(|_| ())
                .map_err(|call| call.into_error("terminal.keys"));
        }
        match self.call(
            "terminal.send",
            &json!({
                "terminal": record.address,
                "text": text,
                "force": false,
                "wait": false,
            }),
            SEND_LIMIT,
        ) {
            Ok(_) => {
                let _ = std::fs::remove_file(&staged);
                Ok(())
            }
            Err(Call::Refused { ref kind, .. }) if kind == "blocked" => {
                Err(TerminalError(format!(
                    "herdr-core refused before typing (blocked): the agent in {terminal} is \
                     blocked on a dialog; nothing was typed"
                )))
            }
            Err(call) => {
                let _ = std::fs::remove_file(&staged);
                Err(call.into_error("terminal.send"))
            }
        }
    }

    fn send_keys(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
        keys: &[&str],
    ) -> Result<(), TerminalError> {
        let translated: Vec<String> = keys
            .iter()
            .map(|key| hc_key(key))
            .collect::<Result<Vec<_>, _>>()?;
        let record = self.require_live_record(terminal)?;
        for key in &translated {
            self.call(
                "terminal.keys",
                &json!({ "terminal": record.address, "keys": [key] }),
                CALL_LIMIT,
            )
            .map_err(|call| call.into_error(&format!("terminal.keys {key}")))?;
            std::thread::sleep(KEY_PAUSE);
        }
        Ok(())
    }

    fn is_alive(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
    ) -> Result<bool, TerminalError> {
        Ok(self.live_record(terminal)?.is_some())
    }

    fn server(&self) -> ServerSeen {
        self.identity()
    }

    fn runner(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
    ) -> Result<Runner, TerminalError> {
        Ok(match self.live_record(terminal)? {
            None => Runner::Gone,
            Some(record) => runner_of(terminal, &record),
        })
    }

    fn runners(
        &self,
        terminals: &[(TerminalName, WorkspaceId)],
    ) -> Result<Vec<(TerminalName, Runner)>, TerminalError> {
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
                    "cannot tell which herdr-core server answers".to_string(),
                ));
            }
        };
        let listed = match self.call(
            "terminal.list",
            &json!({ "all": false, "labels": { OWNER_LABEL: self.session } }),
            CALL_LIMIT,
        ) {
            Ok(listed) => listed,
            Err(Call::NoServer) => return Ok(gone()),
            Err(call) => return Err(call.into_error("terminal.list")),
        };
        let live: HashMap<&str, (Option<&str>, Option<u32>)> = listed["terminals"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["state"].as_str() == Some("live"))
            .filter_map(|t| {
                let addr = t["address"].as_str()?;
                let label = t["labels"][TERMINAL_LABEL].as_str();
                let pid = t["pid"].as_u64().and_then(|p| u32::try_from(p).ok());
                Some((addr, (label, pid)))
            })
            .collect();
        let Some(state) = state else {
            return Ok(gone());
        };
        terminals
            .iter()
            .map(|(terminal, _)| {
                let runner = match Self::record_in(&state, terminal)? {
                    Some(record) => match live.get(record.address.as_str()) {
                        Some(&(label, pid))
                            if label == Some(terminal.as_str())
                                && (record.server == server
                                    || (record.leader.pid > 1
                                        && pid == Some(record.leader.pid))) =>
                        {
                            runner_of(terminal, &record)
                        }
                        _ => Runner::Gone,
                    },
                    None => Runner::Gone,
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
        let record = self.require_live_record(terminal)?;
        let read = self
            .call(
                "terminal.read",
                &json!({ "terminal": record.address, "lines": lines }),
                CAPTURE_LIMIT,
            )
            .map_err(|call| call.into_error("terminal.read"))?;
        read["text"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| TerminalError(format!("terminal.read returned no text: {read}")))
    }

    fn terminate(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<(), TerminalError> {
        let Some(record) = self.record(terminal)? else {
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
        let mut killed_by_server = false;
        if let ServerSeen::Answers(Some(server)) = self.identity() {
            let owned_by_label = server != record.server
                && self
                    .call(
                        "terminal.get",
                        &json!({ "terminal": record.address }),
                        CALL_LIMIT,
                    )
                    .is_ok_and(|info| {
                        info["labels"][TERMINAL_LABEL].as_str() == Some(terminal.as_str())
                    });
            if server == record.server || owned_by_label {
                self.kill_and_remove(&record.address)?;
                killed_by_server = true;
            }
        }
        if !killed_by_server && !leaders.is_empty() {
            crate::terminal_support::signal_sessions(&leaders, rustix::process::Signal::TERM);
        }
        crate::terminal_support::wait_sessions_gone(
            terminal,
            &leaders,
            self.waits,
            "terminal.kill",
        )?;
        crate::terminal_support::end_stragglers(workspace.path(), self.waits)?;
        self.forget(terminal);
        self.unfollow(terminal);
        Ok(())
    }

    fn terminate_waits(&self) -> bool {
        true
    }

    fn agent(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
    ) -> Result<Option<AgentReading>, TerminalError> {
        let Some((server, record, info)) = self.live_info(terminal)? else {
            return Ok(Some(herdr_witness::no_agent()));
        };
        self.checked(server)?;
        self.follow(&record.address, terminal);
        let pid = info["pid"]
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
            .or((record.leader.pid > 1).then_some(record.leader.pid));
        let osc_status = herdr_witness::OscStatus::from_json(&info["status"]);
        let screen = match self.call(
            "terminal.read",
            &json!({ "terminal": record.address, "lines": spawn_core::screen::TAIL_LINES }),
            CAPTURE_LIMIT,
        ) {
            Ok(read) => read["text"].as_str().unwrap_or_default().to_string(),
            Err(call) if call.gone() => return Ok(Some(herdr_witness::no_agent())),
            Err(call) => return Err(call.into_error("terminal.read")),
        };
        let mut reading = herdr_witness::observe(pid, &screen, osc_status.as_ref());
        let key = AgentSnapshotKey {
            agent: reading.agent.clone(),
            status: reading.status,
            rule: reading.rule.clone(),
            blocker_visible: reading.blocker_visible,
        };
        let mut agents = lock(&self.agents);
        let next_seq = match agents.last_readings.get(terminal) {
            Some(prev) if prev.key == key => prev.seq,
            prev => {
                let had_prev = prev.is_some();
                let seq = prev.map_or(u64::from(reading.agent.is_some()), |p| p.seq + 1);
                agents
                    .last_readings
                    .insert(terminal.clone(), AgentCache { key, seq });
                if reading.agent.is_some() || had_prev {
                    agents.mark(terminal);
                }
                seq
            }
        };
        reading.seq = next_seq;
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
        crate::terminal_support::read_from_files(self.beneath.as_ref(), workspace, path, offset)
    }

    fn list_dirs(&self, _workspace: &WorkspaceId, dir: &str) -> Result<Vec<String>, TerminalError> {
        crate::terminal_support::list_dirs_files(self.beneath.as_ref(), dir)
    }

    fn write_new(
        &self,
        _workspace: &WorkspaceId,
        path: &str,
        bytes: &[u8],
    ) -> Result<bool, TerminalError> {
        crate::terminal_support::write_new_file(self.beneath.as_ref(), path, bytes)
    }

    fn replace_grown(
        &self,
        _workspace: &WorkspaceId,
        path: &str,
        bytes: &[u8],
        current: &[u8],
    ) -> Result<bool, TerminalError> {
        crate::terminal_support::replace_grown_file(self.beneath.as_ref(), path, bytes, current)
    }
}

fn runner_of(terminal: &TerminalName, record: &Record) -> Runner {
    if record.leader.alive() {
        Runner::Running
    } else {
        Runner::Untracked(format!(
            "the terminal {} of {terminal} no longer runs the runner Spawn started there \
             (pid {} is gone)",
            record.address, record.leader.pid
        ))
    }
}

fn await_script_removed(script: &Path) {
    let deadline = Instant::now() + LAUNCH_CLEAR_LIMIT;
    while script.exists() && Instant::now() < deadline {
        std::thread::sleep(LAUNCH_CLEAR_POLL);
    }
}

/// Translates a Spawn/tmux key name into a `herdr-core` `terminal.keys` key name.
pub fn hc_key(key: &str) -> Result<String, TerminalError> {
    let named = match key {
        "Enter" | "KPEnter" => Some("enter"),
        "Escape" => Some("esc"),
        "Tab" => Some("tab"),
        "BTab" => Some("shift+tab"),
        "BSpace" => Some("backspace"),
        "Space" | " " => Some("space"),
        "DC" => Some("delete"),
        "IC" => Some("insert"),
        "Home" => Some("home"),
        "End" => Some("end"),
        "PPage" => Some("pageup"),
        "NPage" => Some("pagedown"),
        "Up" => Some("up"),
        "Down" => Some("down"),
        "Left" => Some("left"),
        "Right" => Some("right"),
        _ => None,
    };
    if let Some(named) = named {
        return Ok(named.to_string());
    }
    let unknown = || TerminalError(format!("herdr-core has no key for the tmux key {key:?}"));
    if let Some(rest) = key.strip_prefix('F')
        && let Ok(number) = rest.parse::<u8>()
        && (1..=12).contains(&number)
    {
        return Ok(format!("f{number}"));
    }
    for (prefix, modifier) in [("C-", "ctrl"), ("M-", "alt")] {
        if let Some(rest) = key.strip_prefix(prefix) {
            let base = hc_modified_target(rest).ok_or_else(unknown)?;
            return Ok(format!("{modifier}+{base}"));
        }
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) if ch.is_ascii_uppercase() => {
            Ok(format!("shift+{}", ch.to_ascii_lowercase()))
        }
        (Some(ch), None) if is_hc_literal_key(ch) => Ok(ch.to_string()),
        _ => Err(unknown()),
    }
}

fn hc_modified_target(target: &str) -> Option<String> {
    if target == "\\" || target == "backslash" {
        return Some("backslash".to_string());
    }
    let mut chars = target.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) if is_hc_literal_key(ch) || ch.is_ascii_uppercase() => {
            Some(ch.to_ascii_lowercase().to_string())
        }
        _ => None,
    }
}

fn is_hc_literal_key(ch: char) -> bool {
    ch.is_ascii_lowercase()
        || ch.is_ascii_digit()
        || matches!(
            ch,
            '`' | '\\' | '[' | ']' | ',' | '=' | '-' | '.' | '\'' | ';' | '/'
        )
}

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
    let failed = |error: std::io::Error| Call::Failed(format!("herdr-core {method}: {error}"));
    stream.set_read_timeout(Some(limit)).map_err(failed)?;
    stream.set_write_timeout(Some(limit)).map_err(failed)?;
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let mut line = request.to_string();
    line.push('\n');
    (&stream).write_all(line.as_bytes()).map_err(failed)?;
    let mut answer = String::new();
    std::io::BufReader::new(&stream)
        .read_line(&mut answer)
        .map_err(failed)?;
    if answer.is_empty() {
        return Err(Call::NoServer);
    }
    let parsed: Value = serde_json::from_str(&answer)
        .map_err(|error| Call::Failed(format!("herdr-core {method} invalid JSON: {error}")))?;
    if let Some(error) = parsed.get("error") {
        let kind = error["data"]["kind"]
            .as_str()
            .unwrap_or("internal")
            .to_string();
        let message = error["message"].as_str().unwrap_or_default().to_string();
        return Err(Call::Refused { kind, message });
    }
    Ok(parsed["result"].clone())
}

fn subscribe(socket: &Path, session: &str, agents: &Weak<Mutex<Agents>>) {
    let mut reconnected = false;
    loop {
        let Some(shared) = agents.upgrade() else {
            return;
        };
        let generation = lock(&shared).generation;
        drop(shared);

        let Ok(stream) = std::os::unix::net::UnixStream::connect(socket) else {
            reconnected = true;
            std::thread::sleep(EVENT_POLL);
            continue;
        };
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "events.subscribe",
            "params": { "labels": { OWNER_LABEL: session } },
        });
        let mut req_line = request.to_string();
        req_line.push('\n');
        if stream.set_read_timeout(Some(EVENT_POLL)).is_err()
            || (&stream).write_all(req_line.as_bytes()).is_err()
        {
            reconnected = true;
            std::thread::sleep(EVENT_POLL);
            continue;
        }
        let mut reader = std::io::BufReader::new(stream);
        let mut first = String::new();
        if reader.read_line(&mut first).is_err() || first.is_empty() {
            reconnected = true;
            std::thread::sleep(EVENT_POLL);
            continue;
        }
        if std::mem::take(&mut reconnected)
            && let Some(shared) = agents.upgrade()
        {
            lock(&shared).mark_all();
        }
        let mut line = String::new();
        let dropped = loop {
            match reader.read_line(&mut line) {
                Ok(0) => break true,
                Ok(_) if line.ends_with('\n') => {
                    let Some(shared) = agents.upgrade() else {
                        return;
                    };
                    handle_event_line(&mut lock(&shared), &line);
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
                        // Addresses changed; existing wildcard session filter stays valid, just keep running.
                        let _ = generation;
                    }
                }
                Err(_) => break true,
            }
        };
        if dropped {
            reconnected = true;
        }
    }
}

fn handle_event_line(agents: &mut Agents, line: &str) {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let method = value["method"].as_str().unwrap_or_default();
    if method == "missed" {
        agents.mark_all();
        return;
    }
    if method != "event" {
        return;
    }
    let Some(address) = value["params"]["terminal"].as_str() else {
        return;
    };
    if let Some(terminal) = agents.addresses.get(address).cloned() {
        agents.mark(&terminal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_translation_covers_tmux_specials_modifiers_and_literals() {
        assert_eq!(hc_key("Enter").unwrap(), "enter");
        assert_eq!(hc_key("Escape").unwrap(), "esc");
        assert_eq!(hc_key("BTab").unwrap(), "shift+tab");
        assert_eq!(hc_key("DC").unwrap(), "delete");
        assert_eq!(hc_key("PPage").unwrap(), "pageup");
        assert_eq!(hc_key("NPage").unwrap(), "pagedown");
        assert_eq!(hc_key("F12").unwrap(), "f12");
        assert_eq!(hc_key("C-c").unwrap(), "ctrl+c");
        assert_eq!(hc_key("M-x").unwrap(), "alt+x");
        assert_eq!(hc_key("Y").unwrap(), "shift+y");
        assert_eq!(hc_key("1").unwrap(), "1");
        assert_eq!(hc_key("Space").unwrap(), "space");
        assert!(hc_key("F13").is_err());
        assert!(hc_key("bad-key").is_err());
    }

    #[test]
    fn valid_sessions_and_socket_bounds_are_enforced() {
        assert!(HerdrCoreHost::on_session("spawn-hc", "default").is_err());
        assert!(HerdrCoreHost::on_session("spawn-hc", "a/b").is_err());
        let host = HerdrCoreHost::on_session("spawn-hc", "spawn-hc-test")
            .unwrap()
            .with_runtime_under(PathBuf::from("/tmp/hc"));
        let paths = host.core_paths().unwrap();
        assert!(paths.socket.ends_with("hcr/spawn-hc-test.sock"));
        assert!(paths.state.ends_with("hcs/herdr-core/spawn-hc-test"));
        assert!(paths.config.ends_with("hcc/herdr-core/config.toml"));
    }
}
