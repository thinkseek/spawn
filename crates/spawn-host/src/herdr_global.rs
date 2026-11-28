//! Zero-invasion global Herdr (`herdr-core` / `spawn-hc`) discovery and control client.
//!
//! Connects directly to any `herdr-core` UDS socket without filtering by
//! `spawn_owner` labels, letting Spawn discover, capture styled VT snapshots
//! (via passive `terminal.attach(observe: true)` with plain-text fallback), drive
//! via a single [`TuiAction`] interface guarded by optimistic pre-flight screen
//! digests, interrupt, and wait on both Spawn-managed and external/human terminals.

use std::collections::BTreeMap;
use std::io::{BufRead as _, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use spawn_core::port::{AgentReading, AgentStatus, TerminalError};
use spawn_core::tui::{
    DiscoveredTerminal, ExitCause, InterruptOutcome, InterruptReport, KeyChord, RawPlainSnapshot,
    RawStyledSnapshot, ScreenGuard, SnapshotObservation, TerminalAddress, TerminalLiveState,
    TerminalOrigin, TuiAction, TuiActionOutcome, TuiSnapshot, WaitOutcome, WaitReason,
    is_reserved_spawn_label,
};

use crate::herdr_witness::{OscStatus, observe};

const CALL_LIMIT: Duration = Duration::from_secs(10);
const CAPTURE_LIMIT: Duration = Duration::from_secs(15);
const WRITE_LIMIT: Duration = Duration::from_secs(20);
const WAIT_BUFFER: Duration = Duration::from_secs(5);

const WIRE_HEADER_LEN: usize = 9;
const WIRE_DATA: u8 = 0;
const WIRE_CREDIT: u8 = 1;
const WIRE_CLOSE: u8 = 2;
const MAX_WIRE_PAYLOAD: usize = 16 << 20;
const MAX_PAINT_BYTES: usize = 2 << 20;
const MAX_HELLO_LINE_BYTES: usize = 64 * 1024;

/// Visibility filter applied to global Herdr terminal discovery and snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryScope {
    /// Include both Spawn-managed and external/human terminals.
    All,
    /// Restrict visibility strictly to Spawn-managed terminals (`spawn_owner` + `spawn_terminal`).
    SpawnManagedOnly,
}

/// Closed error classification returned by [`HerdrGlobalClient`] and [`SocketScope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalErrorKind {
    /// Target terminal or socket resource was not found.
    NotFound,
    /// Caller supplied invalid parameters, terminal address, or reserved label key.
    InvalidParams,
    /// Caller lacks permission to access an external non-Spawn terminal.
    ForbiddenExternal,
    /// Optimistic [`ScreenGuard`] did not match the live screen digest.
    GuardMismatch,
    /// Guarded write refused because fidelity was [`spawn_core::tui::SnapshotFidelity::PlainTextOnly`].
    UnobservableSelection,
    /// Terminal state prevents the requested operation (`blocked`, `status_mismatch`, `finished`, `busy`).
    StateConflict,
    /// Bare terminal address matched multiple live Herdr control sockets.
    AmbiguousAddress,
    /// Daemon control socket was unreachable, timed out, or unavailable.
    Unavailable,
    /// Unexpected daemon, protocol, or internal failure.
    Internal,
}

/// Typed error returned by [`HerdrGlobalClient`] and [`SocketScope`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalHerdrError {
    /// Machine-readable closed error classification.
    pub kind: GlobalErrorKind,
    /// Human-readable diagnostic message.
    pub message: String,
}

impl GlobalHerdrError {
    /// Constructs a [`GlobalHerdrError`] from its classification and detail message.
    pub fn new(kind: GlobalErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for GlobalHerdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GlobalHerdrError {}

impl From<GlobalHerdrError> for TerminalError {
    fn from(err: GlobalHerdrError) -> Self {
        Self(err.message)
    }
}

#[derive(Debug)]
enum RpcFailure {
    Refused { kind: String, message: String },
    ConnectUnreachable(GlobalHerdrError),
    Other(GlobalHerdrError),
}

impl RpcFailure {
    fn into_error(self, method: &str) -> GlobalHerdrError {
        match self {
            Self::Refused { kind, message } => {
                let mapped = match kind.as_str() {
                    "not_found" => GlobalErrorKind::NotFound,
                    "invalid_params" => GlobalErrorKind::InvalidParams,
                    "blocked" | "status_mismatch" | "finished" | "busy" => {
                        GlobalErrorKind::StateConflict
                    }
                    "unavailable" | "timeout" => GlobalErrorKind::Unavailable,
                    _ => GlobalErrorKind::Internal,
                };
                GlobalHerdrError::new(
                    mapped,
                    format!("herdr-core {method} refused ({kind}): {message}"),
                )
            }
            Self::ConnectUnreachable(err) | Self::Other(err) => err,
        }
    }

    fn is_transient_missing(&self) -> bool {
        matches!(
            self,
            Self::Refused { kind, .. } if matches!(kind.as_str(), "not_found" | "finished")
        )
    }

    fn is_not_found(&self) -> bool {
        matches!(self, Self::Refused { kind, .. } if kind == "not_found")
    }
}

/// Process environment inputs controlling global `herdr-core` socket resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HerdrDiscoveryEnv {
    /// Explicit socket path override from `HC_SOCKET` (when non-empty).
    pub hc_socket: Option<PathBuf>,
    /// User runtime base directory from `$XDG_RUNTIME_DIR`.
    pub xdg_runtime_dir: Option<PathBuf>,
}

/// Target set of `herdr-core` UDS sockets resolved for global discovery or routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketScope {
    /// Explicit single control socket (`--socket` or `HC_SOCKET`).
    Explicit(PathBuf),
    /// Automatically discovered candidate sockets from standard Herdr runtime directories.
    Discovered(Vec<PathBuf>),
}

impl SocketScope {
    /// Resolves `explicit` socket path, falling back to `env.hc_socket` (`HC_SOCKET`),
    /// or else scanning [`HerdrGlobalClient::default_sockets`].
    pub fn resolve(
        explicit: Option<&Path>,
        env: &HerdrDiscoveryEnv,
    ) -> Result<Self, GlobalHerdrError> {
        if let Some(path) = explicit {
            if path.as_os_str().is_empty() {
                return Err(GlobalHerdrError::new(
                    GlobalErrorKind::InvalidParams,
                    "invalid_params: --socket path must not be empty",
                ));
            }
            return Ok(Self::Explicit(path.to_path_buf()));
        }
        if let Some(env_sock) = env.hc_socket.as_ref().filter(|v| !v.as_os_str().is_empty()) {
            return Ok(Self::Explicit(env_sock.clone()));
        }
        Ok(Self::Discovered(HerdrGlobalClient::default_sockets(
            env.xdg_runtime_dir.as_deref(),
        )?))
    }
}

/// Synchronous, zero-invasion client over a `herdr-core` / `spawn-hc` UDS socket.
#[derive(Debug, Clone)]
pub struct HerdrGlobalClient {
    socket: PathBuf,
}

impl HerdrGlobalClient {
    /// Connects subsequent calls to the `herdr-core` UDS endpoint at `socket`.
    pub fn at_socket(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// Configured UDS socket path.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Scans `runtime_dir` for live `herdr-core` control sockets (`*.sock`, excluding
    /// `*.holder.sock` and `*.holder.<id>.sock`), sorted deterministically by path.
    pub fn discover_sockets_in(runtime_dir: &Path) -> Result<Vec<PathBuf>, GlobalHerdrError> {
        let entries = match std::fs::read_dir(runtime_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(GlobalHerdrError::new(
                    GlobalErrorKind::Internal,
                    format!("read herdr runtime dir {}: {err}", runtime_dir.display()),
                ));
            }
        };
        let mut sockets = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| {
                GlobalHerdrError::new(
                    GlobalErrorKind::Internal,
                    format!("iterate herdr runtime dir {}: {err}", runtime_dir.display()),
                )
            })?;
            let path = entry.path();
            let is_sock = path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("sock"));
            if is_sock && !is_holder_socket(&path) {
                sockets.push(path);
            }
        }
        sockets.sort();
        Ok(sockets)
    }

    /// Discovers candidate `herdr-core` UDS sockets from standard runtime locations
    /// (`$XDG_RUNTIME_DIR/herdr-core` and `/run/user/<uid>/herdr-core`).
    pub fn default_sockets(
        xdg_runtime_dir: Option<&Path>,
    ) -> Result<Vec<PathBuf>, GlobalHerdrError> {
        let mut dirs = Vec::new();
        if let Some(xdg) = xdg_runtime_dir.filter(|v| !v.as_os_str().is_empty()) {
            dirs.push(xdg.join("herdr-core"));
        }
        let run_user = PathBuf::from(format!("/run/user/{}/herdr-core", crate::current_uid()));
        if !dirs.contains(&run_user) {
            dirs.push(run_user);
        }
        let mut sockets = Vec::new();
        for dir in dirs {
            for sock in Self::discover_sockets_in(&dir)? {
                if !sockets.contains(&sock) {
                    sockets.push(sock);
                }
            }
        }
        sockets.sort();
        Ok(sockets)
    }

    /// Discovers terminals across every socket in `scope` honoring `visibility`.
    ///
    /// For [`SocketScope::Explicit`], fails closed if the socket is unreachable.
    /// For [`SocketScope::Discovered`], stale sockets returning `NotFound` or
    /// `ConnectionRefused` on connect are ignored while RPC/protocol failures still
    /// fail closed. Results are sorted deterministically by terminal `address`.
    pub fn discover_in_scope(
        scope: &SocketScope,
        include_exited: bool,
        visibility: DiscoveryScope,
    ) -> Result<Vec<DiscoveredTerminal>, GlobalHerdrError> {
        match scope {
            SocketScope::Explicit(sock) => {
                Self::at_socket(sock).discover_scoped(include_exited, visibility)
            }
            SocketScope::Discovered(socks) => {
                let mut merged = Vec::new();
                for sock in socks {
                    match Self::at_socket(sock).discover_raw(include_exited, visibility) {
                        Ok(items) => merged.extend(items),
                        Err(RpcFailure::ConnectUnreachable(_)) => {}
                        Err(err) => return Err(err.into_error("terminal.list")),
                    }
                }
                merged.sort_by(|a, b| a.address.cmp(&b.address));
                Ok(merged)
            }
        }
    }

    /// Resolves the [`HerdrGlobalClient`] owning `address` within `scope`.
    ///
    /// Fails closed on malformed addresses (`InvalidParams`), missing terminals
    /// (`NotFound`), or bare addresses claimed by multiple sockets (`AmbiguousAddress`).
    pub fn client_for_address(
        scope: &SocketScope,
        address: &str,
    ) -> Result<Self, GlobalHerdrError> {
        TerminalAddress::parse(address)
            .map_err(|msg| GlobalHerdrError::new(GlobalErrorKind::InvalidParams, msg))?;
        match scope {
            SocketScope::Explicit(sock) => Ok(Self::at_socket(sock)),
            SocketScope::Discovered(socks) => {
                if socks.is_empty() {
                    return Err(GlobalHerdrError::new(
                        GlobalErrorKind::NotFound,
                        format!(
                            "not_found: no live herdr-core socket found for terminal {address}"
                        ),
                    ));
                }
                let mut matches = Vec::new();
                for sock in socks {
                    let client = Self::at_socket(sock);
                    match client.call_rpc_raw(
                        "terminal.get",
                        &json!({ "terminal": address }),
                        CALL_LIMIT,
                    ) {
                        Ok(_) => matches.push(client),
                        Err(RpcFailure::ConnectUnreachable(_)) => {}
                        Err(err) if err.is_not_found() => {}
                        Err(err) => return Err(err.into_error("terminal.get")),
                    }
                }
                match matches.len() {
                    0 => Err(GlobalHerdrError::new(
                        GlobalErrorKind::NotFound,
                        format!(
                            "not_found: terminal {address} not found on any live herdr-core socket"
                        ),
                    )),
                    1 => Ok(matches.into_iter().next().expect("checked length 1")),
                    _ => Err(GlobalHerdrError::new(
                        GlobalErrorKind::AmbiguousAddress,
                        format!(
                            "ambiguous_address: terminal {address} exists on multiple herdr-core sockets; specify --socket <path>"
                        ),
                    )),
                }
            }
        }
    }

    /// Discovers every terminal visible on this `herdr-core` (and federated peers)
    /// without applying any `spawn_owner` label filter.
    pub fn discover_all(
        &self,
        include_exited: bool,
    ) -> Result<Vec<DiscoveredTerminal>, GlobalHerdrError> {
        self.discover_scoped(include_exited, DiscoveryScope::All)
    }

    /// Discovers terminals visible on this `herdr-core` honoring `visibility` before
    /// reading any terminal screen.
    pub fn discover_scoped(
        &self,
        include_exited: bool,
        visibility: DiscoveryScope,
    ) -> Result<Vec<DiscoveredTerminal>, GlobalHerdrError> {
        self.discover_raw(include_exited, visibility)
            .map_err(|err| err.into_error("terminal.list"))
    }

    fn discover_raw(
        &self,
        include_exited: bool,
        visibility: DiscoveryScope,
    ) -> Result<Vec<DiscoveredTerminal>, RpcFailure> {
        let listed = self.call_rpc_raw(
            "terminal.list",
            &json!({ "all": include_exited }),
            CALL_LIMIT,
        )?;
        let Some(items) = listed["terminals"].as_array() else {
            return Err(RpcFailure::Other(GlobalHerdrError::new(
                GlobalErrorKind::Internal,
                format!("terminal.list missing terminals array: {listed}"),
            )));
        };
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            if let Some(entry) = self.build_discovered_terminal(item, visibility)? {
                out.push(entry);
            }
        }
        Ok(out)
    }

    fn build_discovered_terminal(
        &self,
        item: &Value,
        visibility: DiscoveryScope,
    ) -> Result<Option<DiscoveredTerminal>, RpcFailure> {
        let Some(address) = item["address"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
        else {
            return Err(RpcFailure::Other(GlobalHerdrError::new(
                GlobalErrorKind::Internal,
                format!("terminal.list item missing non-empty address: {item}"),
            )));
        };
        let labels = parse_labels(&item["labels"]);
        let origin = TerminalOrigin::from_labels(&labels);
        if visibility == DiscoveryScope::SpawnManagedOnly
            && !matches!(origin, TerminalOrigin::SpawnManaged { .. })
        {
            return Ok(None);
        }
        let machine = split_machine(&address);
        let state = parse_live_state(item).map_err(RpcFailure::Other)?;
        let pid = match state {
            TerminalLiveState::Live { pid } => pid,
            TerminalLiveState::Exited { .. } => None,
        };
        let is_live = matches!(state, TerminalLiveState::Live { .. });
        let osc_status = OscStatus::from_json(&item["status"]);
        let reading = if is_live && machine.is_none() {
            // `lines: 0` reads only the visible screen so discovery never scrolls alt-screen TUIs.
            match self.call_rpc_raw(
                "terminal.read",
                &json!({ "terminal": address, "lines": 0 }),
                CAPTURE_LIMIT,
            ) {
                Ok(read) => resolve_reading(
                    pid,
                    read["text"].as_str().unwrap_or_default(),
                    osc_status.as_ref(),
                ),
                Err(err) if err.is_transient_missing() => osc_status
                    .as_ref()
                    .map_or_else(crate::herdr_witness::no_agent, osc_fallback_reading),
                Err(err) => return Err(RpcFailure::Other(err.into_error("terminal.read"))),
            }
        } else {
            osc_status
                .as_ref()
                .map_or_else(crate::herdr_witness::no_agent, osc_fallback_reading)
        };

        Ok(Some(DiscoveredTerminal {
            address,
            machine,
            name: item["name"].as_str().map(str::to_string),
            origin,
            state,
            cwd: item["cwd"].as_str().unwrap_or_default().to_string(),
            title: item["title"].as_str().unwrap_or_default().to_string(),
            argv: item["argv"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            labels,
            cols: u16::try_from(item["cols"].as_u64().unwrap_or(0)).unwrap_or(0),
            rows: u16::try_from(item["rows"].as_u64().unwrap_or(0)).unwrap_or(0),
            agent: reading.agent,
            agent_status: reading.status,
            turn: item["turn"].as_u64().unwrap_or(0),
            keep: item["keep"].as_bool().unwrap_or(false),
        }))
    }

    /// Captures a structured [`TuiSnapshot`] across any local or federated terminal.
    pub fn snapshot(&self, address: &str) -> Result<TuiSnapshot, GlobalHerdrError> {
        self.snapshot_scoped(address, DiscoveryScope::All)
    }

    /// Captures a structured [`TuiSnapshot`] while enforcing `visibility` before reading
    /// any terminal screen buffer.
    ///
    /// Returns [`GlobalErrorKind::ForbiddenExternal`] when `visibility` is
    /// [`DiscoveryScope::SpawnManagedOnly`] and the target terminal lacks Spawn labels.
    pub fn snapshot_scoped(
        &self,
        address: &str,
        visibility: DiscoveryScope,
    ) -> Result<TuiSnapshot, GlobalHerdrError> {
        TerminalAddress::parse(address)
            .map_err(|msg| GlobalHerdrError::new(GlobalErrorKind::InvalidParams, msg))?;
        let info = self.call_rpc("terminal.get", &json!({ "terminal": address }), CALL_LIMIT)?;
        let origin = TerminalOrigin::from_labels(&parse_labels(&info["labels"]));
        if visibility == DiscoveryScope::SpawnManagedOnly
            && !matches!(origin, TerminalOrigin::SpawnManaged { .. })
        {
            return Err(GlobalHerdrError::new(
                GlobalErrorKind::ForbiddenExternal,
                format!("forbidden: read-only viewer cannot snapshot external terminal {address}"),
            ));
        }
        let resolved_addr = info["address"].as_str().unwrap_or(address);
        let cols = u16::try_from(info["cols"].as_u64().unwrap_or(0)).unwrap_or(0);
        let rows = u16::try_from(info["rows"].as_u64().unwrap_or(0)).unwrap_or(0);
        let live_state = parse_live_state(&info)?;
        let (is_live, pid) = match &live_state {
            TerminalLiveState::Live { pid } => (true, *pid),
            TerminalLiveState::Exited { .. } => (false, None),
        };
        let osc_status = OscStatus::from_json(&info["status"]);

        if is_live && let Some(raw_vt) = self.capture_styled_vt(resolved_addr, cols, rows)? {
            return Ok(TuiSnapshot::from_styled_vt_observed(
                RawStyledSnapshot {
                    address: resolved_addr,
                    cols,
                    rows,
                    live_state,
                    title: info["title"].as_str(),
                    raw_vt: &raw_vt,
                },
                |plain_lines| {
                    let plain_text = plain_lines.join("\n");
                    let reading = resolve_reading(pid, &plain_text, osc_status.as_ref());
                    SnapshotObservation {
                        agent: reading.agent,
                        agent_status: reading.status,
                        rule: reading.rule,
                        blocker_visible: reading.blocker_visible,
                    }
                },
            ));
        }

        let read = self.call_rpc(
            "terminal.read",
            &json!({ "terminal": address, "lines": 0 }),
            CAPTURE_LIMIT,
        )?;
        let raw_text = read["text"].as_str().unwrap_or_default();
        let reading = resolve_reading(pid, raw_text, osc_status.as_ref());
        Ok(TuiSnapshot::from_plain_text(RawPlainSnapshot {
            address: resolved_addr,
            cols,
            rows,
            live_state,
            how: read["how"].as_str().unwrap_or("screen"),
            complete: read["complete"].as_bool().unwrap_or(true),
            why: read["why"].as_str(),
            title: info["title"].as_str(),
            raw_text,
            agent: reading.agent.as_deref(),
            agent_status: reading.status,
            rule: reading.rule.as_deref(),
            blocker_visible: reading.blocker_visible,
        }))
    }

    /// Executes a single [`TuiAction`] against a live terminal.
    ///
    /// When [`TuiAction::SendText`] (`SendPolicy::OnlyIf`) or [`TuiAction::SendKeys`]
    /// specifies `expected_guard: Some(want)`, performs an optimistic pre-flight
    /// verification by taking a fresh [`Self::snapshot`] right before issuing the write
    /// RPC (optimistic read-then-write verification across separate RPCs).
    /// Under `SnapshotFidelity::PlainTextOnly`, all guarded sends/keys fail closed with
    /// [`GlobalErrorKind::UnobservableSelection`]; guard mismatches return
    /// [`GlobalErrorKind::GuardMismatch`].
    pub fn execute(
        &self,
        address: &str,
        action: &TuiAction,
    ) -> Result<TuiActionOutcome, GlobalHerdrError> {
        TerminalAddress::parse(address)
            .map_err(|msg| GlobalHerdrError::new(GlobalErrorKind::InvalidParams, msg))?;
        match action {
            TuiAction::SendText {
                text,
                policy,
                wait_timeout_ms,
            } => {
                self.verify_optimistic_guard(address, policy.expected_guard())?;
                let (force, only_if) = policy.to_herdr_params();
                let mut params = serde_json::Map::new();
                params.insert("terminal".to_string(), json!(address));
                params.insert("text".to_string(), json!(text));
                params.insert("force".to_string(), json!(force));
                if let Some(cond) = only_if {
                    params.insert("if".to_string(), json!(cond));
                }
                let rpc_limit = match wait_timeout_ms {
                    Some(ms) => {
                        let dur = Duration::from_millis(*ms);
                        params.insert("wait".to_string(), json!(true));
                        params.insert("timeout".to_string(), json!(dur.as_secs_f64()));
                        dur.saturating_add(WAIT_BUFFER)
                    }
                    None => WRITE_LIMIT,
                };
                let result = self.call_rpc("terminal.send", &Value::Object(params), rpc_limit)?;
                Ok(TuiActionOutcome::TextSent {
                    status: result["status"].as_str().map(str::to_string),
                    turn: result["turn"].as_u64().unwrap_or(0),
                    started: result["started"].as_bool(),
                })
            }
            TuiAction::SendKeys {
                keys,
                expected_guard,
            } => {
                self.verify_optimistic_guard(address, *expected_guard)?;
                let specs: Vec<String> = keys
                    .as_slice()
                    .iter()
                    .map(KeyChord::to_herdr_spec)
                    .collect();
                let result = self.call_rpc(
                    "terminal.keys",
                    &json!({ "terminal": address, "keys": specs }),
                    CALL_LIMIT,
                )?;
                Ok(TuiActionOutcome::KeysSent {
                    status: result["status"].as_str().map(str::to_string),
                    turn: result["turn"].as_u64().unwrap_or(0),
                })
            }
            TuiAction::Interrupt { timeout_ms } => {
                let mut params = serde_json::Map::new();
                params.insert("terminal".to_string(), json!(address));
                let rpc_limit = match timeout_ms {
                    Some(ms) => {
                        let dur = Duration::from_millis(*ms);
                        params.insert("timeout".to_string(), json!(dur.as_secs_f64()));
                        dur.saturating_add(WAIT_BUFFER)
                    }
                    None => CALL_LIMIT.saturating_add(WAIT_BUFFER),
                };
                let result =
                    self.call_rpc("terminal.interrupt", &Value::Object(params), rpc_limit)?;
                Ok(TuiActionOutcome::Interrupted(InterruptReport {
                    outcome: InterruptOutcome::parse(
                        result["outcome"].as_str().unwrap_or_default(),
                    ),
                    key_sent: result["key"].as_str().map(str::to_string),
                }))
            }
            TuiAction::WaitSettled {
                after_turn,
                output_contains,
                timeout_ms,
            } => self
                .wait_settled_inner(
                    address,
                    *after_turn,
                    output_contains.as_deref(),
                    Duration::from_millis(*timeout_ms),
                )
                .map(TuiActionOutcome::Waited),
        }
    }

    /// Attaches or updates non-reserved observation metadata labels (`""` removes a key)
    /// without restarting or disrupting the terminal.
    ///
    /// Fails closed before issuing any RPC with [`GlobalErrorKind::InvalidParams`] if
    /// `labels` is empty, contains an invalid key, or attempts to write any
    /// Spawn-reserved key (`spawn_*`).
    pub fn tag_observe(
        &self,
        address: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>, GlobalHerdrError> {
        TerminalAddress::parse(address)
            .map_err(|msg| GlobalHerdrError::new(GlobalErrorKind::InvalidParams, msg))?;
        if labels.is_empty() {
            return Err(GlobalHerdrError::new(
                GlobalErrorKind::InvalidParams,
                "invalid_params: tag_observe requires at least one label",
            ));
        }
        for key in labels.keys() {
            if is_reserved_spawn_label(key) {
                return Err(GlobalHerdrError::new(
                    GlobalErrorKind::InvalidParams,
                    format!(
                        "reserved_label: cannot mutate Spawn-managed label {key:?} via tag_observe"
                    ),
                ));
            }
            if key.is_empty() || key.contains('=') || key.chars().any(char::is_whitespace) {
                return Err(GlobalHerdrError::new(
                    GlobalErrorKind::InvalidParams,
                    format!("invalid_label_key: {key:?}"),
                ));
            }
        }
        let result = self.call_rpc(
            "terminal.label",
            &json!({ "terminal": address, "labels": labels }),
            CALL_LIMIT,
        )?;
        Ok(parse_labels(&result["labels"]))
    }

    fn verify_optimistic_guard(
        &self,
        address: &str,
        expected_guard: Option<ScreenGuard>,
    ) -> Result<(), GlobalHerdrError> {
        let Some(want) = expected_guard else {
            return Ok(());
        };
        let actual = self.snapshot(address)?;
        if !actual.selection_observable() {
            return Err(GlobalHerdrError::new(
                GlobalErrorKind::UnobservableSelection,
                format!(
                    "unobservable_selection: terminal {address} guarded mutation rejected under PlainTextOnly fidelity"
                ),
            ));
        }
        let got = actual.guard();
        if got != want {
            return Err(GlobalHerdrError::new(
                GlobalErrorKind::GuardMismatch,
                format!(
                    "guard_mismatch: terminal {address} screen changed (expected {want}, got {got})"
                ),
            ));
        }
        Ok(())
    }

    fn wait_settled_inner(
        &self,
        address: &str,
        after_turn: Option<u64>,
        output_contains: Option<&str>,
        timeout: Duration,
    ) -> Result<WaitOutcome, GlobalHerdrError> {
        if let Some(prev_turn) = after_turn
            && output_contains.is_none()
            && let Ok(info) =
                self.call_rpc("terminal.get", &json!({ "terminal": address }), CALL_LIMIT)
        {
            let cur_turn = info["turn"].as_u64().unwrap_or(0);
            let cur_status = info["status"]["state"].as_str().map(str::to_string);
            let resolved = info["address"].as_str().unwrap_or(address).to_string();
            match parse_live_state(&info)? {
                TerminalLiveState::Exited { cause } => {
                    return Ok(WaitOutcome {
                        terminal: resolved,
                        reason: WaitReason::Exit { cause },
                        status: cur_status,
                        turn: cur_turn,
                    });
                }
                TerminalLiveState::Live { .. }
                    if cur_turn > prev_turn && cur_status.as_deref() != Some("working") =>
                {
                    return Ok(WaitOutcome {
                        terminal: resolved,
                        reason: WaitReason::Status,
                        status: cur_status,
                        turn: cur_turn,
                    });
                }
                TerminalLiveState::Live { .. } => {}
            }
        }
        let mut params = serde_json::Map::new();
        params.insert("terminal".to_string(), json!(address));
        params.insert("timeout".to_string(), json!(timeout.as_secs_f64()));
        if let Some(needle) = output_contains {
            params.insert("output".to_string(), json!(needle));
        }
        let result = self.call_rpc(
            "terminal.wait",
            &Value::Object(params),
            timeout.saturating_add(WAIT_BUFFER),
        )?;
        let exit_code = result["exit_code"]
            .as_i64()
            .and_then(|v| i32::try_from(v).ok());
        let signal = result["signal"]
            .as_i64()
            .and_then(|v| i32::try_from(v).ok());
        let from = result["from"].as_str().map(str::to_string);
        let wire_ended = result["ended"].as_bool().ok_or_else(|| {
            GlobalHerdrError::new(
                GlobalErrorKind::Internal,
                format!("terminal.wait missing boolean ended: {result}"),
            )
        })?;
        let raw_reason = result["reason"].as_str().unwrap_or("unknown");
        let reason = WaitReason::from_herdr(raw_reason, wire_ended, exit_code, signal, from)
            .map_err(|msg| GlobalHerdrError::new(GlobalErrorKind::Internal, msg))?;
        Ok(WaitOutcome {
            terminal: result["terminal"].as_str().unwrap_or(address).to_string(),
            reason,
            status: result["status"].as_str().map(str::to_string),
            turn: result["turn"].as_u64().unwrap_or(0),
        })
    }

    fn capture_styled_vt(
        &self,
        address: &str,
        cols: u16,
        rows: u16,
    ) -> Result<Option<String>, GlobalHerdrError> {
        let deadline = Instant::now() + CAPTURE_LIMIT;
        let ctx = "herdr-core terminal.attach";
        let mut stream = UnixStream::connect(&self.socket).map_err(|err| {
            GlobalHerdrError::new(
                GlobalErrorKind::Unavailable,
                format!("{ctx} connect {}: {err}", self.socket.display()),
            )
        })?;

        let hello_req = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "hello",
            "params": { "generation": 1, "framed": true },
        })
        .to_string();
        write_all_deadline(
            &mut stream,
            format!("{hello_req}\n").as_bytes(),
            deadline,
            ctx,
        )?;

        let hello_line = read_line_unbuffered_deadline(&mut stream, deadline, ctx)?;
        if hello_line.is_empty() {
            return Err(GlobalHerdrError::new(
                GlobalErrorKind::Internal,
                "herdr-core hello closed socket without reply",
            ));
        }
        let hello_resp: Value = serde_json::from_str(&hello_line).map_err(|err| {
            GlobalHerdrError::new(
                GlobalErrorKind::Internal,
                format!("herdr-core hello invalid JSON: {err}"),
            )
        })?;
        if hello_resp.get("error").is_some() || hello_resp["result"]["framed"] != true {
            return Ok(None);
        }

        let attach_req = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "terminal.attach",
            "params": {
                "terminal": address,
                "observe": true,
                "history": 0,
                "cols": cols,
                "rows": rows,
            }
        }))
        .map_err(|err| {
            GlobalHerdrError::new(
                GlobalErrorKind::Internal,
                format!("encode terminal.attach: {err}"),
            )
        })?;
        write_wire_frame_deadline(&mut stream, 0, WIRE_DATA, &attach_req, deadline, ctx)?;

        read_attach_first_paint(&mut stream, deadline)
    }

    fn call_rpc(
        &self,
        method: &str,
        params: &Value,
        limit: Duration,
    ) -> Result<Value, GlobalHerdrError> {
        self.call_rpc_raw(method, params, limit)
            .map_err(|err| err.into_error(method))
    }

    fn call_rpc_raw(
        &self,
        method: &str,
        params: &Value,
        limit: Duration,
    ) -> Result<Value, RpcFailure> {
        let deadline = Instant::now() + limit;
        let ctx = format!("herdr-core {method}");
        let mut stream = UnixStream::connect(&self.socket).map_err(|err| {
            let unreachable = matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            );
            let global_err = GlobalHerdrError::new(
                GlobalErrorKind::Unavailable,
                format!("{ctx} connect {}: {err}", self.socket.display()),
            );
            if unreachable {
                RpcFailure::ConnectUnreachable(global_err)
            } else {
                RpcFailure::Other(global_err)
            }
        })?;
        let mut line = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        })
        .to_string();
        line.push('\n');
        write_all_deadline(&mut stream, line.as_bytes(), deadline, &ctx)
            .map_err(RpcFailure::Other)?;
        let answer = read_rpc_line_deadline(&stream, deadline, &ctx).map_err(RpcFailure::Other)?;
        if answer.is_empty() {
            return Err(RpcFailure::Other(GlobalHerdrError::new(
                GlobalErrorKind::Unavailable,
                format!("{ctx}: socket closed without reply"),
            )));
        }
        let parsed: Value = serde_json::from_str(&answer).map_err(|err| {
            RpcFailure::Other(GlobalHerdrError::new(
                GlobalErrorKind::Internal,
                format!("{ctx} invalid JSON: {err}"),
            ))
        })?;
        if let Some(error) = parsed.get("error") {
            return Err(RpcFailure::Refused {
                kind: error["data"]["kind"]
                    .as_str()
                    .unwrap_or("internal")
                    .to_string(),
                message: error["message"].as_str().unwrap_or_default().to_string(),
            });
        }
        Ok(parsed["result"].clone())
    }
}

fn remaining_until(deadline: Instant, context: &str) -> Result<Duration, GlobalHerdrError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|dur| !dur.is_zero())
        .ok_or_else(|| {
            GlobalHerdrError::new(
                GlobalErrorKind::Unavailable,
                format!("{context}: total socket deadline exceeded"),
            )
        })
}

fn classify_io_error(err: &std::io::Error, context: &str) -> GlobalHerdrError {
    let kind = match err.kind() {
        std::io::ErrorKind::TimedOut
        | std::io::ErrorKind::WouldBlock
        | std::io::ErrorKind::NotFound
        | std::io::ErrorKind::ConnectionRefused => GlobalErrorKind::Unavailable,
        _ => GlobalErrorKind::Internal,
    };
    GlobalHerdrError::new(kind, format!("{context}: {err}"))
}

fn write_all_deadline(
    stream: &mut UnixStream,
    mut buf: &[u8],
    deadline: Instant,
    context: &str,
) -> Result<(), GlobalHerdrError> {
    while !buf.is_empty() {
        let rem = remaining_until(deadline, context)?;
        stream
            .set_write_timeout(Some(rem))
            .map_err(|err| classify_io_error(&err, context))?;
        match stream.write(buf) {
            Ok(0) => {
                return Err(GlobalHerdrError::new(
                    GlobalErrorKind::Unavailable,
                    format!("{context}: socket write returned 0 bytes"),
                ));
            }
            Ok(n) => buf = &buf[n..],
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(classify_io_error(&err, context)),
        }
    }
    Ok(())
}

struct DeadlineReader<'a> {
    stream: &'a UnixStream,
    deadline: Instant,
}

impl std::io::Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let rem = self
                .deadline
                .checked_duration_since(Instant::now())
                .filter(|dur| !dur.is_zero())
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "total socket deadline exceeded",
                    )
                })?;
            self.stream.set_read_timeout(Some(rem))?;
            match (&*self.stream).read(buf) {
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                other => return other,
            }
        }
    }
}

fn read_rpc_line_deadline(
    stream: &UnixStream,
    deadline: Instant,
    context: &str,
) -> Result<String, GlobalHerdrError> {
    let mut reader = std::io::BufReader::new(DeadlineReader { stream, deadline });
    let mut out = Vec::new();
    loop {
        let chunk = reader
            .fill_buf()
            .map_err(|err| classify_io_error(&err, context))?;
        if chunk.is_empty() {
            break;
        }
        let nl = chunk.iter().position(|&b| b == b'\n');
        let take = nl.map_or(chunk.len(), |idx| idx + 1);
        if out.len() + take > MAX_WIRE_PAYLOAD {
            return Err(GlobalHerdrError::new(
                GlobalErrorKind::Internal,
                format!("{context}: RPC response exceeded {MAX_WIRE_PAYLOAD} bytes"),
            ));
        }
        out.extend_from_slice(&chunk[..take]);
        reader.consume(take);
        if nl.is_some() {
            break;
        }
    }
    String::from_utf8(out).map_err(|err| {
        GlobalHerdrError::new(
            GlobalErrorKind::Internal,
            format!("{context}: non-UTF8 reply: {err}"),
        )
    })
}

fn read_attach_first_paint(
    stream: &mut UnixStream,
    deadline: Instant,
) -> Result<Option<String>, GlobalHerdrError> {
    let ctx = "herdr-core terminal.attach";
    let mut target_channel: Option<u32> = None;
    let mut paint_bytes = Vec::new();
    while let Some((channel, kind, payload)) = read_wire_frame_deadline(stream, deadline)? {
        if channel == 0 && kind == WIRE_DATA {
            let reply: Value = serde_json::from_slice(&payload).map_err(|err| {
                GlobalHerdrError::new(
                    GlobalErrorKind::Internal,
                    format!("{ctx} invalid JSON: {err}"),
                )
            })?;
            if let Some(error) = reply.get("error") {
                let err_kind = error["data"]["kind"].as_str().unwrap_or("internal");
                if matches!(err_kind, "finished" | "not_found" | "needs_framed") {
                    return Ok(None);
                }
                let message = error["message"].as_str().unwrap_or_default();
                return Err(RpcFailure::Refused {
                    kind: err_kind.to_string(),
                    message: message.to_string(),
                }
                .into_error("terminal.attach"));
            }
            let ch = reply["result"]["channel"]
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| {
                    GlobalHerdrError::new(
                        GlobalErrorKind::Internal,
                        format!("terminal.attach missing channel: {reply}"),
                    )
                })?;
            target_channel = Some(ch);
            continue;
        }
        if target_channel == Some(channel) {
            if kind == WIRE_DATA {
                let take = payload
                    .len()
                    .min(MAX_PAINT_BYTES.saturating_sub(paint_bytes.len()));
                paint_bytes.extend_from_slice(&payload[..take]);
                let credit = u32::try_from(payload.len()).unwrap_or(u32::MAX);
                let _ = write_wire_frame_deadline(
                    stream,
                    channel,
                    WIRE_CREDIT,
                    &credit.to_le_bytes(),
                    deadline,
                    ctx,
                );
                if contains_bytes(&paint_bytes, b"\x1b[?9001h")
                    || contains_bytes(&paint_bytes, b"\x1b[?9001l")
                    || paint_bytes.len() >= MAX_PAINT_BYTES
                {
                    return Ok(Some(String::from_utf8_lossy(&paint_bytes).into_owned()));
                }
            } else if kind == WIRE_CLOSE {
                return Ok((!paint_bytes.is_empty())
                    .then(|| String::from_utf8_lossy(&paint_bytes).into_owned()));
            }
        }
    }
    Ok((!paint_bytes.is_empty()).then(|| String::from_utf8_lossy(&paint_bytes).into_owned()))
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|win| win == needle)
}

fn read_line_unbuffered_deadline(
    stream: &mut UnixStream,
    deadline: Instant,
    context: &str,
) -> Result<String, GlobalHerdrError> {
    let mut reader = DeadlineReader { stream, deadline };
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0_u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) if buf.is_empty() => return Ok(String::new()),
            Ok(0) => {
                return Err(GlobalHerdrError::new(
                    GlobalErrorKind::Unavailable,
                    format!("{context}: unexpected EOF reading hello"),
                ));
            }
            Ok(_) => {
                buf.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
                if buf.len() >= MAX_HELLO_LINE_BYTES {
                    return Err(GlobalHerdrError::new(
                        GlobalErrorKind::Internal,
                        "herdr-core hello line exceeded limit",
                    ));
                }
            }
            Err(err) => return Err(classify_io_error(&err, context)),
        }
    }
    String::from_utf8(buf).map_err(|err| {
        GlobalHerdrError::new(
            GlobalErrorKind::Internal,
            format!("{context}: hello line invalid UTF-8: {err}"),
        )
    })
}

fn write_wire_frame_deadline(
    stream: &mut UnixStream,
    channel: u32,
    kind: u8,
    payload: &[u8],
    deadline: Instant,
    context: &str,
) -> Result<(), GlobalHerdrError> {
    let len = u32::try_from(payload.len()).map_err(|_| {
        GlobalHerdrError::new(
            GlobalErrorKind::InvalidParams,
            format!("{context}: frame too large"),
        )
    })?;
    let mut frame = Vec::with_capacity(WIRE_HEADER_LEN + payload.len());
    frame.extend_from_slice(&channel.to_le_bytes());
    frame.push(kind);
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(payload);
    write_all_deadline(stream, &frame, deadline, context)
}

fn read_exact_or_eof_start(
    reader: &mut DeadlineReader<'_>,
    buf: &mut [u8],
    context: &str,
) -> Result<bool, GlobalHerdrError> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => {
                return Err(GlobalHerdrError::new(
                    GlobalErrorKind::Unavailable,
                    format!("{context}: unexpected EOF"),
                ));
            }
            Ok(n) => filled += n,
            Err(err) => return Err(classify_io_error(&err, context)),
        }
    }
    Ok(true)
}

fn read_wire_frame_deadline(
    stream: &UnixStream,
    deadline: Instant,
) -> Result<Option<(u32, u8, Vec<u8>)>, GlobalHerdrError> {
    let mut reader = DeadlineReader { stream, deadline };
    let mut header = [0_u8; WIRE_HEADER_LEN];
    if !read_exact_or_eof_start(&mut reader, &mut header, "herdr-core read framed header")? {
        return Ok(None);
    }
    let channel = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let kind = header[4];
    let len = u32::from_le_bytes([header[5], header[6], header[7], header[8]]) as usize;
    if len > MAX_WIRE_PAYLOAD {
        return Err(GlobalHerdrError::new(
            GlobalErrorKind::Internal,
            format!("herdr-core framed payload {len} exceeds {MAX_WIRE_PAYLOAD}"),
        ));
    }
    let mut payload = vec![0_u8; len];
    if !read_exact_or_eof_start(&mut reader, &mut payload, "herdr-core read framed payload")?
        && !payload.is_empty()
    {
        return Err(GlobalHerdrError::new(
            GlobalErrorKind::Unavailable,
            "herdr-core read framed payload: unexpected EOF",
        ));
    }
    Ok(Some((channel, kind, payload)))
}

fn parse_live_state(item: &Value) -> Result<TerminalLiveState, GlobalHerdrError> {
    if item["state"].as_str() == Some("live") {
        let pid = item["pid"].as_u64().and_then(|v| u32::try_from(v).ok());
        return Ok(TerminalLiveState::Live { pid });
    }
    let code = item["exit_code"]
        .as_i64()
        .and_then(|v| i32::try_from(v).ok());
    let signal = item["signal"].as_i64().and_then(|v| i32::try_from(v).ok());
    let reason = item["reason"].as_str().map(str::to_string);
    let cause = ExitCause::from_herdr(code, signal, reason)
        .map_err(|msg| GlobalHerdrError::new(GlobalErrorKind::Internal, msg))?;
    Ok(TerminalLiveState::Exited { cause })
}

fn is_holder_socket(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    let mut parts = stem.rsplit('.');
    match (parts.next(), parts.next()) {
        // `<session>.holder.sock`
        (Some(last), Some(prefix)) if !prefix.is_empty() && last.eq_ignore_ascii_case("holder") => {
            true
        }
        // `<session>.holder.<id>.sock`
        (Some(id), Some(prev)) => {
            !id.is_empty()
                && parts.next().is_some_and(|prefix| !prefix.is_empty())
                && prev.eq_ignore_ascii_case("holder")
        }
        _ => false,
    }
}

fn osc_fallback_reading(osc: &OscStatus) -> AgentReading {
    let status = AgentStatus::from_herdr(&osc.state);
    AgentReading {
        agent: (!osc.app.is_empty()).then(|| osc.app.clone()),
        status,
        rule: osc.kind.clone(),
        blocker_visible: status == AgentStatus::Blocked,
        session: None,
        seq: 0,
        manifest: None,
    }
}

fn resolve_reading(pid: Option<u32>, text: &str, osc_status: Option<&OscStatus>) -> AgentReading {
    let reading = observe(pid, text, osc_status);
    if reading.agent.is_none()
        && let Some(osc) = osc_status
    {
        return osc_fallback_reading(osc);
    }
    reading
}

fn split_machine(address: &str) -> Option<String> {
    address
        .split_once('/')
        .map(|(machine, _)| machine.to_string())
}

fn parse_labels(value: &Value) -> BTreeMap<String, String> {
    value
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use spawn_core::tui::{KeyChordSequence, SendPolicy, SnapshotFidelity};

    use super::*;

    static SEQ: AtomicU64 = AtomicU64::new(1);

    struct TempDirGuard(PathBuf);

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(prefix: &str) -> TempDirGuard {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = Path::new("/tmp").join(format!("{prefix}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("0700");
        TempDirGuard(dir)
    }

    #[test]
    fn discover_sockets_in_skips_holder_and_sorts() {
        let dir = temp_dir("shg-sock");
        std::fs::write(dir.0.join("b.sock"), b"").expect("b.sock");
        std::fs::write(dir.0.join("a.sock"), b"").expect("a.sock");
        std::fs::write(dir.0.join("a.holder.sock"), b"").expect("holder.sock");
        std::fs::write(dir.0.join("a.holder.75f572c8.sock"), b"").expect("holder.id.sock");
        std::fs::write(dir.0.join("notes.txt"), b"").expect("notes");

        let found = HerdrGlobalClient::discover_sockets_in(&dir.0).expect("discover");
        assert_eq!(found, vec![dir.0.join("a.sock"), dir.0.join("b.sock")]);
    }

    type MockLog = Arc<Mutex<Vec<(String, Value)>>>;

    fn mock_list_result() -> Value {
        json!({
            "terminals": [
                {
                    "address": "t1",
                    "name": "managed-1",
                    "labels": {
                        "spawn_owner": "loop-a",
                        "spawn_terminal": "spawn-exec_1"
                    },
                    "argv": ["claude"],
                    "cwd": "/repo",
                    "state": "live",
                    "pid": 111,
                    "cols": 200,
                    "rows": 50,
                    "title": "claude",
                    "turn": 2,
                    "keep": false
                },
                {
                    "address": "t2",
                    "name": null,
                    "labels": {},
                    "argv": ["python3", "menu.py"],
                    "cwd": "/tmp",
                    "state": "live",
                    "pid": 222,
                    "cols": 120,
                    "rows": 40,
                    "title": "menu",
                    "status": { "state": "blocked", "kind": "question" },
                    "turn": 0,
                    "keep": false
                },
                {
                    "address": "n252/t7",
                    "name": "remote-agy",
                    "labels": {},
                    "argv": ["agy"],
                    "cwd": "/work",
                    "state": "live",
                    "pid": 333,
                    "cols": 160,
                    "rows": 48,
                    "title": "agy",
                    "status": { "state": "working" },
                    "turn": 5,
                    "keep": true
                }
            ],
            "hidden_finished": 0,
            "machines": []
        })
    }

    fn mock_rpc_reply(method: &str, params: &Value) -> Value {
        match method {
            "hello" => json!({ "generation": 1, "framed": false }),
            "terminal.list" => mock_list_result(),
            "terminal.get" => {
                let term = params["terminal"].as_str().unwrap_or_default();
                let labels = if term == "t1" {
                    json!({
                        "spawn_owner": "loop-a",
                        "spawn_terminal": "spawn-exec_1"
                    })
                } else {
                    json!({})
                };
                json!({
                    "address": term,
                    "labels": labels,
                    "state": "live",
                    "pid": if term == "t1" { 101 } else { 222 },
                    "cols": 120,
                    "rows": 40,
                    "title": "menu",
                    "turn": 0
                })
            }
            "terminal.read" => {
                let text = match params["terminal"].as_str() {
                    Some("t_unnumbered") => "Choose backend:\n  Claude Code\n  OpenAI Codex\n",
                    Some("t_glyph") => "Menu:\n  ❯ 1. First option\n  2. Second option\n",
                    _ => "Menu:\n  1. First option\n  2. Second option\n",
                };
                json!({
                    "text": text,
                    "complete": true,
                    "how": "screen"
                })
            }
            "terminal.keys" => json!({ "status": "idle", "turn": 1 }),
            "terminal.send" => json!({ "status": "working", "turn": 1, "started": true }),
            "terminal.interrupt" => json!({ "outcome": "stopped", "key": "esc" }),
            "terminal.wait" => json!({
                "terminal": params["terminal"],
                "ended": true,
                "reason": "output",
                "status": "idle",
                "turn": 1
            }),
            "terminal.label" => {
                json!({ "address": params["terminal"], "labels": params["labels"] })
            }
            other => panic!("unexpected rpc {other}"),
        }
    }

    fn spawn_mock_rpc_server(sock: &Path) -> (std::thread::JoinHandle<()>, MockLog) {
        let listener = UnixListener::bind(sock).expect("bind mock socket");
        let recorded = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let recorded_srv = Arc::clone(&recorded);
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    break;
                };
                let mut line = String::new();
                if std::io::BufReader::new(&stream)
                    .read_line(&mut line)
                    .is_err()
                    || line.is_empty()
                {
                    continue;
                }
                let req: Value = serde_json::from_str(&line).expect("valid rpc request");
                let method = req["method"].as_str().unwrap_or_default().to_string();
                let params = req["params"].clone();
                recorded_srv
                    .lock()
                    .expect("lock")
                    .push((method.clone(), params.clone()));
                if method == "__stop" {
                    let _ = (&stream).write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n");
                    break;
                }
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": req["id"],
                    "result": mock_rpc_reply(&method, &params),
                });
                (&stream)
                    .write_all(format!("{resp}\n").as_bytes())
                    .expect("write rpc response");
            }
        });
        (handle, recorded)
    }

    #[test]
    fn mock_rpc_covers_unfiltered_discovery_unobservable_guard_actions_and_reserved_labels() {
        let dir = temp_dir("shg-mock");
        let sock = dir.0.join("core.sock");
        let (srv, recorded) = spawn_mock_rpc_server(&sock);

        let client = HerdrGlobalClient::at_socket(&sock);
        let found = client.discover_all(false).expect("discover_all");
        assert_eq!(found.len(), 3);
        assert!(matches!(
            found[0].origin,
            TerminalOrigin::SpawnManaged { .. }
        ));
        assert_eq!(found[1].origin, TerminalOrigin::External);
        assert_eq!(found[1].agent_status, AgentStatus::Blocked);
        assert_eq!(found[2].machine.as_deref(), Some("n252"));
        assert_eq!(found[2].agent_status, AgentStatus::Working);

        let plain_snap = client.snapshot("t2").expect("snapshot");
        assert_plain_text_blocks_guarded_mutations(&client, &plain_snap);

        let keys_outcome = client
            .execute(
                "t2",
                &TuiAction::SendKeys {
                    keys: KeyChordSequence::new(vec![
                        KeyChord::parse("down").expect("down"),
                        KeyChord::parse("enter").expect("enter"),
                    ])
                    .expect("non-empty keys"),
                    expected_guard: None,
                },
            )
            .expect("unguarded send_keys succeeds");
        assert!(matches!(
            keys_outcome,
            TuiActionOutcome::KeysSent { turn: 1, .. }
        ));

        let send_outcome = client
            .execute(
                "t2",
                &TuiAction::SendText {
                    text: "hello".to_string(),
                    policy: SendPolicy::OnlyIfIdle,
                    wait_timeout_ms: Some(2000),
                },
            )
            .expect("send_text succeeds");
        assert!(matches!(
            send_outcome,
            TuiActionOutcome::TextSent {
                started: Some(true),
                ..
            }
        ));

        let int_outcome = client
            .execute(
                "t2",
                &TuiAction::Interrupt {
                    timeout_ms: Some(2000),
                },
            )
            .expect("interrupt");
        assert_eq!(
            int_outcome,
            TuiActionOutcome::Interrupted(InterruptReport {
                outcome: InterruptOutcome::Stopped,
                key_sent: Some("esc".to_string()),
            })
        );

        assert_mock_wait_and_labels(&client, &sock, srv, &recorded);
    }

    fn assert_plain_text_blocks_guarded_mutations(
        client: &HerdrGlobalClient,
        plain_snap: &TuiSnapshot,
    ) {
        assert!(matches!(
            plain_snap.fidelity(),
            SnapshotFidelity::PlainTextOnly {
                complete: true,
                why: None,
                ..
            }
        ));
        assert!(!plain_snap.selection_observable());

        let enter_keys = KeyChordSequence::single(KeyChord::parse("enter").expect("enter"));
        let err = client
            .execute(
                "t2",
                &TuiAction::SendKeys {
                    keys: enter_keys,
                    expected_guard: Some(plain_snap.guard()),
                },
            )
            .expect_err("PlainTextOnly unmarked menu must fail closed on guarded keys");
        assert_eq!(err.kind, GlobalErrorKind::UnobservableSelection, "{err}");

        // Unnumbered menu (zero parsed choices) and glyph menu under PlainTextOnly MUST
        // equally fail closed on both guarded `SendKeys` and guarded `SendText` (`OnlyIf`).
        for addr in ["t_unnumbered", "t_glyph"] {
            let snap = client.snapshot(addr).expect("plain snapshot");
            assert!(!snap.selection_observable(), "{addr}");
            let keys_err = client
                .execute(
                    addr,
                    &TuiAction::SendKeys {
                        keys: KeyChordSequence::single(KeyChord::parse("enter").expect("enter")),
                        expected_guard: Some(snap.guard()),
                    },
                )
                .expect_err("PlainTextOnly guarded SendKeys must fail closed");
            assert_eq!(
                keys_err.kind,
                GlobalErrorKind::UnobservableSelection,
                "{addr}: {keys_err}"
            );
            let send_err = client
                .execute(
                    addr,
                    &TuiAction::SendText {
                        text: "confirm".to_string(),
                        policy: SendPolicy::OnlyIf {
                            expected_guard: snap.guard(),
                            status: None,
                        },
                        wait_timeout_ms: None,
                    },
                )
                .expect_err("PlainTextOnly guarded SendText must fail closed");
            assert_eq!(
                send_err.kind,
                GlobalErrorKind::UnobservableSelection,
                "{addr}: {send_err}"
            );
        }
    }

    fn assert_mock_wait_and_labels(
        client: &HerdrGlobalClient,
        _sock: &Path,
        srv: std::thread::JoinHandle<()>,
        recorded: &MockLog,
    ) {
        let wait_outcome = client
            .execute(
                "t2",
                &TuiAction::WaitSettled {
                    after_turn: None,
                    output_contains: Some("PICKED_2".to_string()),
                    timeout_ms: 2000,
                },
            )
            .expect("wait_settled");
        assert!(matches!(
            wait_outcome,
            TuiActionOutcome::Waited(ref w) if w.ended() && w.reason == WaitReason::Output
        ));

        for forbidden in [
            "spawn_owner",
            "spawn_terminal",
            "spawn_workspace",
            "spawn_custom",
            "SPAWN_OWNER",
        ] {
            let mut bad = BTreeMap::new();
            bad.insert(forbidden.to_string(), "evil".to_string());
            let err = client
                .tag_observe("t2", &bad)
                .expect_err("reserved label must fail closed before RPC");
            assert_eq!(
                err.kind,
                GlobalErrorKind::InvalidParams,
                "{forbidden}: {err}"
            );
            assert!(err.message.contains("reserved_label"), "{forbidden}: {err}");
        }

        let mut tags = BTreeMap::new();
        tags.insert("observed_by".to_string(), "test".to_string());
        let saved = client.tag_observe("t2", &tags).expect("tag_observe");
        assert_eq!(saved.get("observed_by").map(String::as_str), Some("test"));

        let _ = client.call_rpc("__stop", &json!({}), CALL_LIMIT);
        srv.join().expect("mock server thread");

        assert_recorded_mock_rpcs(recorded);
    }

    #[test]
    fn discover_all_fails_closed_on_missing_address_and_contradictory_exit() {
        for bad_list in [
            json!({ "terminals": [{ "state": "live", "labels": {} }] }),
            json!({
                "terminals": [{
                    "address": "t9",
                    "state": "finished",
                    "exit_code": 0,
                    "signal": 9,
                    "labels": {}
                }]
            }),
        ] {
            let dir = temp_dir("shg-bad");
            let sock = dir.0.join("bad.sock");
            let listener = UnixListener::bind(&sock).expect("bind");
            let payload = json!({ "jsonrpc": "2.0", "id": 1, "result": bad_list });
            let srv = std::thread::spawn(move || {
                if let Ok((stream, _)) = listener.accept() {
                    let mut line = String::new();
                    let _ = std::io::BufReader::new(&stream).read_line(&mut line);
                    let _ = (&stream).write_all(format!("{payload}\n").as_bytes());
                }
            });
            let err = HerdrGlobalClient::at_socket(&sock)
                .discover_all(true)
                .expect_err("malformed terminal.list entry must fail closed");
            assert_eq!(err.kind, GlobalErrorKind::Internal, "{err}");
            assert!(
                err.message.contains("missing non-empty address")
                    || err.message.contains("contradictory Herdr exit state"),
                "{err}"
            );
            srv.join().expect("server");
        }
    }

    fn assert_recorded_mock_rpcs(recorded: &MockLog) {
        let calls = recorded.lock().expect("lock");
        let list_call = calls
            .iter()
            .find(|(m, _)| m == "terminal.list")
            .expect("terminal.list recorded");
        assert!(
            list_call.1.get("labels").is_none(),
            "global discovery must never filter by spawn_owner labels: {:?}",
            list_call.1
        );
        let read_calls: Vec<_> = calls.iter().filter(|(m, _)| m == "terminal.read").collect();
        assert!(!read_calls.is_empty(), "terminal.read must be recorded");
        for (_, params) in read_calls {
            assert_eq!(
                params["lines"],
                json!(0),
                "zero-invasion read must request lines=0: {params}"
            );
        }
        let keys_call = calls
            .iter()
            .find(|(m, _)| m == "terminal.keys")
            .expect("terminal.keys recorded");
        assert_eq!(keys_call.1["keys"], json!(["down", "enter"]));
        for (method, params) in calls.iter() {
            if method == "terminal.label" {
                let obj = params["labels"].as_object().expect("labels object");
                assert!(obj.keys().all(|k| !is_reserved_spawn_label(k)));
            }
        }
    }

    fn find_sibling_spawn_hc() -> Option<String> {
        let exe = std::env::current_exe().ok()?;
        for dir in [exe.parent(), exe.parent().and_then(Path::parent)]
            .into_iter()
            .flatten()
        {
            let candidate = dir.join("spawn-hc");
            if candidate.is_file() {
                return Some(candidate.display().to_string());
            }
        }
        None
    }

    fn require_spawn_hc_binary() -> String {
        if let Some(prog) = std::env::var_os("SPAWN_HERDR_CORE_PROGRAM")
            .map(|v| v.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
        {
            if prog.contains(std::path::MAIN_SEPARATOR) {
                assert!(
                    Path::new(&prog).is_file(),
                    "SPAWN_HERDR_CORE_PROGRAM={prog:?} does not exist"
                );
            }
            return prog;
        }
        if let Some(found) = find_sibling_spawn_hc() {
            return found;
        }

        let exe = std::env::current_exe().expect("current_exe");
        let profile_dir = exe.parent().and_then(Path::parent);
        let is_release = profile_dir
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            == Some("release");
        let target_dir = profile_dir.and_then(Path::parent);
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("workspace root");

        let mut build = std::process::Command::new(env!("CARGO"));
        build
            .current_dir(&workspace_root)
            .args(["build", "--locked", "-p", "spawn-hc"]);
        if std::env::var("CARGO_NET_OFFLINE").is_ok_and(|v| v.eq_ignore_ascii_case("true")) {
            build.arg("--offline");
        }
        if is_release {
            build.arg("--release");
        }
        if let Some(dir) = target_dir {
            build.env("CARGO_TARGET_DIR", dir);
        }
        let output = build
            .output()
            .expect("invoke cargo build --locked -p spawn-hc");
        assert!(
            output.status.success(),
            "cargo build --locked -p spawn-hc failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        find_sibling_spawn_hc().unwrap_or_else(|| {
            panic!("spawn-hc binary still missing after cargo build in {workspace_root:?}")
        })
    }

    struct HcEnvGuard {
        root: PathBuf,
        socket: PathBuf,
        state_home: PathBuf,
        config_home: PathBuf,
        runtime_dir: PathBuf,
        program: String,
    }

    impl HcEnvGuard {
        fn new(program: String) -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let root = Path::new("/tmp").join(format!("shg-real-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let state_home = root.join("st");
            let config_home = root.join("cfg");
            let runtime_dir = root.join("run");
            for d in [&root, &state_home, &config_home, &runtime_dir] {
                std::fs::create_dir_all(d).expect("mkdir");
                std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).expect("0700");
            }
            let socket = runtime_dir.join("hc.sock");
            Self {
                root,
                socket,
                state_home,
                config_home,
                runtime_dir,
                program,
            }
        }
    }

    impl Drop for HcEnvGuard {
        fn drop(&mut self) {
            let client = HerdrGlobalClient::at_socket(&self.socket);
            let _ = client.call_rpc("server.stop", &json!({}), Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(100));
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    // Reverse-video ONLY selection (`\x1b[7m...\x1b[0m`), zero text glyphs like `❯`.
    // Plain lines remain byte-for-byte identical whether item 1 or item 2 is selected.
    const REVERSE_VIDEO_MENU_SCRIPT: &str = r#"import sys, termios, tty, time
fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
sel = 0
items = ["Allow action", "Deny action"]
def draw():
    sys.stdout.write("\x1b[2J\x1b[1;1HSelect action:\r\n")
    for idx, label in enumerate(items):
        if idx == sel:
            sys.stdout.write(f"  \x1b[7m{idx + 1}. {label}\x1b[0m\r\n")
        else:
            sys.stdout.write(f"  {idx + 1}. {label}\r\n")
    sys.stdout.flush()
try:
    tty.setraw(fd)
    draw()
    while True:
        ch = sys.stdin.read(1)
        if ch == "\x1b":
            seq = sys.stdin.read(2)
            if seq == "[B":
                sel = min(1, sel + 1)
                draw()
            elif seq == "[A":
                sel = max(0, sel - 1)
                draw()
        elif ch in ("\r", "\n"):
            time.sleep(0.15)
            sys.stdout.write(f"\r\nPICKED_{sel + 1}\r\n")
            sys.stdout.flush()
            break
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
time.sleep(30)
"#;

    fn launch_real_menu_terminal(env: &HcEnvGuard) -> String {
        let script_path = env.root.join("menu.py");
        std::fs::write(&script_path, REVERSE_VIDEO_MENU_SCRIPT).expect("write menu script");
        let mut cmd = std::process::Command::new(&env.program);
        for (name, _) in std::env::vars_os() {
            if name
                .to_str()
                .is_some_and(|n| n.starts_with("HERDR_") || n.starts_with("HC_"))
            {
                cmd.env_remove(name);
            }
        }
        let output = cmd
            .stdin(std::process::Stdio::null())
            .env("HC_SOCKET", &env.socket)
            .env("HC_SESSION", "ar1real")
            .env("XDG_STATE_HOME", &env.state_home)
            .env("XDG_CONFIG_HOME", &env.config_home)
            .env("XDG_RUNTIME_DIR", &env.runtime_dir)
            .args([
                "--json",
                "run",
                "--bg",
                "--",
                "python3",
                &script_path.to_string_lossy(),
            ])
            .output()
            .expect("launch external terminal via spawn-hc");
        assert!(
            output.status.success(),
            "spawn-hc run failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let run_json: Value =
            serde_json::from_slice(&output.stdout).expect("parse spawn-hc run --json output");
        run_json["address"]
            .as_str()
            .expect("terminal address")
            .to_string()
    }

    fn await_menu_snapshot(
        client: &HerdrGlobalClient,
        addr: &str,
        selected_idx: usize,
    ) -> TuiSnapshot {
        for _ in 0..40 {
            let cur = client.snapshot(addr).expect("snapshot");
            let choices = cur.choices();
            if choices.len() == 2
                && choices[selected_idx].selected
                && !choices[1 - selected_idx].selected
            {
                return cur;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("reverse-video menu choice {selected_idx} never highlighted");
    }

    fn verify_real_labels_and_interrupt(client: &HerdrGlobalClient, addr: &str) {
        let mut forbidden = BTreeMap::new();
        forbidden.insert("spawn_owner".to_string(), "spoofed".to_string());
        let label_err = client
            .tag_observe(addr, &forbidden)
            .expect_err("spawn_owner must be rejected fail-closed");
        assert_eq!(
            label_err.kind,
            GlobalErrorKind::InvalidParams,
            "{label_err}"
        );
        assert!(label_err.message.contains("reserved_label"), "{label_err}");

        let mut labels = BTreeMap::new();
        labels.insert("observed_by".to_string(), "ar1_r2".to_string());
        let saved = client.tag_observe(addr, &labels).expect("tag_observe");
        assert_eq!(saved.get("observed_by").map(String::as_str), Some("ar1_r2"));
        let after_tag = client.discover_all(false).expect("discover after tag");
        let still_external = after_tag
            .iter()
            .find(|t| t.address == addr)
            .expect("still present");
        assert_eq!(still_external.origin, TerminalOrigin::External);

        let int_outcome = client
            .execute(
                addr,
                &TuiAction::Interrupt {
                    timeout_ms: Some(5000),
                },
            )
            .expect("interrupt");
        assert!(
            matches!(
                int_outcome,
                TuiActionOutcome::Interrupted(ref r)
                    if r.key_sent.is_some()
                        && matches!(
                            r.outcome,
                            InterruptOutcome::Exited
                                | InterruptOutcome::Stopped
                                | InterruptOutcome::Unreported
                        )
            ),
            "unexpected interrupt report: {int_outcome:?}"
        );
    }

    #[test]
    fn socket_scope_skips_stale_sockets_and_detects_ambiguous_addresses() {
        let dir = temp_dir("shg-scope");
        let stale_sock = dir.0.join("01-stale.sock");
        {
            let _dead = UnixListener::bind(&stale_sock).expect("bind stale socket");
        }
        let live_sock = dir.0.join("02-live.sock");
        let (srv1, _) = spawn_mock_rpc_server(&live_sock);
        let live_sock_2 = dir.0.join("03-live2.sock");
        let (srv2, _) = spawn_mock_rpc_server(&live_sock_2);

        let discovered = HerdrGlobalClient::discover_sockets_in(&dir.0).expect("discover sockets");
        assert_eq!(
            discovered,
            vec![stale_sock.clone(), live_sock.clone(), live_sock_2.clone()]
        );

        let single_scope = SocketScope::Discovered(vec![stale_sock.clone(), live_sock.clone()]);
        let items = HerdrGlobalClient::discover_in_scope(&single_scope, false, DiscoveryScope::All)
            .expect("skips stale ConnectionRefused socket");
        assert_eq!(items.len(), 3);

        let picked = HerdrGlobalClient::client_for_address(&single_scope, "t2")
            .expect("resolves unique live socket");
        assert_eq!(picked.socket(), live_sock.as_path());

        let bad_addr = HerdrGlobalClient::client_for_address(&single_scope, "../escape")
            .expect_err("traversal address must fail closed");
        assert_eq!(bad_addr.kind, GlobalErrorKind::InvalidParams, "{bad_addr}");

        let dup_scope =
            SocketScope::Discovered(vec![stale_sock, live_sock.clone(), live_sock_2.clone()]);
        let amb_err = HerdrGlobalClient::client_for_address(&dup_scope, "t2")
            .expect_err("duplicate terminal across sockets must fail closed");
        assert_eq!(amb_err.kind, GlobalErrorKind::AmbiguousAddress, "{amb_err}");

        let _ = HerdrGlobalClient::at_socket(&live_sock).call_rpc("__stop", &json!({}), CALL_LIMIT);
        let _ =
            HerdrGlobalClient::at_socket(&live_sock_2).call_rpc("__stop", &json!({}), CALL_LIMIT);
        srv1.join().expect("srv1");
        srv2.join().expect("srv2");
    }

    #[test]
    fn spawn_managed_only_scope_hides_and_forbids_external_terminals_without_reading_screens() {
        let dir = temp_dir("shg-managed-only");
        let sock = dir.0.join("core.sock");
        let (srv, recorded) = spawn_mock_rpc_server(&sock);
        let scope = SocketScope::Explicit(sock.clone());
        let client = HerdrGlobalClient::at_socket(&sock);

        let managed =
            HerdrGlobalClient::discover_in_scope(&scope, true, DiscoveryScope::SpawnManagedOnly)
                .expect("discover SpawnManagedOnly");
        assert_eq!(managed.len(), 1);
        assert_eq!(managed[0].address, "t1");
        assert!(matches!(
            managed[0].origin,
            TerminalOrigin::SpawnManaged { .. }
        ));

        let managed_snap = client
            .snapshot_scoped("t1", DiscoveryScope::SpawnManagedOnly)
            .expect("snapshot SpawnManaged under SpawnManagedOnly");
        assert_eq!(managed_snap.address(), "t1");

        let ext_err = client
            .snapshot_scoped("t2", DiscoveryScope::SpawnManagedOnly)
            .expect_err("external terminal must be forbidden under SpawnManagedOnly");
        assert_eq!(
            ext_err.kind,
            GlobalErrorKind::ForbiddenExternal,
            "{ext_err}"
        );

        let _ = client.call_rpc("__stop", &json!({}), CALL_LIMIT);
        srv.join().expect("srv");

        let calls = recorded.lock().expect("lock");
        let read_targets: Vec<Option<&str>> = calls
            .iter()
            .filter(|(m, _)| m == "terminal.read")
            .map(|(_, p)| p["terminal"].as_str())
            .collect();
        assert_eq!(read_targets, vec![Some("t1"), Some("t1")]);
    }

    #[test]
    fn slow_drip_socket_hits_total_deadline_and_returns_unavailable() {
        let dir = temp_dir("shg-drip");
        let sock = dir.0.join("drip.sock");
        let listener = UnixListener::bind(&sock).expect("bind drip");
        let srv = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut line = String::new();
                let _ = std::io::BufReader::new(&stream).read_line(&mut line);
                for byte in b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n" {
                    if (&stream).write_all(&[*byte]).is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        });
        let started = Instant::now();
        let err = HerdrGlobalClient::at_socket(&sock)
            .call_rpc("terminal.list", &json!({}), Duration::from_millis(100))
            .expect_err("slow-drip response must exceed total 100ms deadline");
        assert_eq!(err.kind, GlobalErrorKind::Unavailable, "{err}");
        assert!(
            started.elapsed() < Duration::from_millis(600),
            "elapsed {:?}",
            started.elapsed()
        );
        srv.join().expect("drip srv");
    }

    #[test]
    fn real_spawn_hc_global_styled_vt_guard_keys_reserved_labels_and_wait() {
        let program = require_spawn_hc_binary();
        let env = HcEnvGuard::new(program);
        let addr = launch_real_menu_terminal(&env);

        let sockets =
            HerdrGlobalClient::discover_sockets_in(&env.runtime_dir).expect("scan sockets");
        assert_eq!(sockets, vec![env.socket.clone()]);

        let client = HerdrGlobalClient::at_socket(&env.socket);
        let found = client.discover_all(false).expect("discover_all");
        let ext = found
            .iter()
            .find(|t| t.address == addr)
            .expect("unlabeled external terminal discovered");
        assert_eq!(ext.origin, TerminalOrigin::External);
        assert!(matches!(
            ext.state,
            TerminalLiveState::Live { pid: Some(_) }
        ));

        let snap1 = await_menu_snapshot(&client, &addr, 0);
        assert!(matches!(
            snap1.fidelity(),
            SnapshotFidelity::StyledVt {
                cursor: Some(_),
                alt_screen: false,
                styled_lines,
            } if !styled_lines.is_empty()
        ));
        assert!(snap1.selection_observable());

        client
            .execute(
                &addr,
                &TuiAction::SendKeys {
                    keys: KeyChordSequence::single(KeyChord::parse("down").expect("down")),
                    expected_guard: Some(snap1.guard()),
                },
            )
            .expect("guarded down succeeds");

        let snap2 = await_menu_snapshot(&client, &addr, 1);
        assert_eq!(snap2.plain_lines(), snap1.plain_lines());
        assert_ne!(
            snap2.guard(),
            snap1.guard(),
            "reverse-video selection change 1 -> 2 must invalidate stale guard"
        );

        let stale_err = client
            .execute(
                &addr,
                &TuiAction::SendKeys {
                    keys: KeyChordSequence::single(KeyChord::parse("enter").expect("enter")),
                    expected_guard: Some(snap1.guard()),
                },
            )
            .expect_err("stale snap1 guard must reject enter");
        assert_eq!(
            stale_err.kind,
            GlobalErrorKind::GuardMismatch,
            "{stale_err}"
        );

        let watch_sock = env.socket.clone();
        let watch_addr = addr.clone();
        let watch_handle = std::thread::spawn(move || {
            HerdrGlobalClient::at_socket(&watch_sock).execute(
                &watch_addr,
                &TuiAction::WaitSettled {
                    after_turn: None,
                    output_contains: Some("PICKED_2".to_string()),
                    timeout_ms: 5000,
                },
            )
        });
        std::thread::sleep(Duration::from_millis(80));

        client
            .execute(
                &addr,
                &TuiAction::SendKeys {
                    keys: KeyChordSequence::single(KeyChord::parse("enter").expect("enter")),
                    expected_guard: Some(snap2.guard()),
                },
            )
            .expect("fresh snap2 guard allows enter");

        let wait_outcome = watch_handle
            .join()
            .expect("wait thread")
            .expect("wait_settled on PICKED_2");
        assert!(
            matches!(
                wait_outcome,
                TuiActionOutcome::Waited(ref w) if w.ended() && w.reason == WaitReason::Output
            ),
            "expected PICKED_2 wait output: {wait_outcome:?}"
        );

        verify_real_labels_and_interrupt(&client, &addr);
        eprintln!("REAL_SPAWN_HC_GLOBAL_VERIFIED address={addr} styled_vt=true");
    }
}
