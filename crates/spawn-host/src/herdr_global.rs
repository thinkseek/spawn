//! Zero-invasion global Herdr (`herdr-core` / `spawn-hc`) discovery and control client.
//!
//! Connects directly to any `herdr-core` UDS socket without filtering by
//! `spawn_owner` labels, letting Spawn discover, capture styled VT snapshots
//! (via passive `terminal.attach(observe: true)` with plain-text fallback), drive
//! via a single [`TuiAction`] interface guarded by optimistic pre-flight screen
//! digests, interrupt, and wait on both Spawn-managed and external/human terminals.

use std::collections::BTreeMap;
use std::io::{BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use spawn_core::port::{AgentReading, AgentStatus, TerminalError};
use spawn_core::tui::{
    DiscoveredTerminal, ExitCause, InterruptOutcome, InterruptReport, KeyChord, RawPlainSnapshot,
    RawStyledSnapshot, ScreenGuard, TerminalLiveState, TerminalOrigin, TuiAction, TuiActionOutcome,
    TuiSnapshot, WaitOutcome, WaitReason, is_reserved_spawn_label,
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

#[derive(Debug)]
enum RpcFailure {
    Refused { kind: String, message: String },
    Other(TerminalError),
}

impl RpcFailure {
    fn into_terminal_error(self, method: &str) -> TerminalError {
        match self {
            Self::Refused { kind, message } => {
                TerminalError(format!("herdr-core {method} refused ({kind}): {message}"))
            }
            Self::Other(err) => err,
        }
    }

    fn is_transient_missing(&self) -> bool {
        matches!(
            self,
            Self::Refused { kind, .. } if matches!(kind.as_str(), "not_found" | "finished")
        )
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
    pub fn discover_sockets_in(runtime_dir: &Path) -> Result<Vec<PathBuf>, TerminalError> {
        let entries = match std::fs::read_dir(runtime_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(TerminalError(format!(
                    "read herdr runtime dir {}: {err}",
                    runtime_dir.display()
                )));
            }
        };
        let mut sockets = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| {
                TerminalError(format!(
                    "iterate herdr runtime dir {}: {err}",
                    runtime_dir.display()
                ))
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
    pub fn default_sockets() -> Result<Vec<PathBuf>, TerminalError> {
        let mut dirs = Vec::new();
        if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
            dirs.push(PathBuf::from(xdg).join("herdr-core"));
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

    /// Discovers every terminal visible on this `herdr-core` (and federated peers)
    /// without applying any `spawn_owner` label filter.
    ///
    /// Fails closed on malformed entries missing an `address`, contradictory exit
    /// metadata, or unexpected local `terminal.read` errors (allowing only concurrent
    /// `not_found`/`finished` disappearance to degrade to OSC metadata).
    pub fn discover_all(
        &self,
        include_exited: bool,
    ) -> Result<Vec<DiscoveredTerminal>, TerminalError> {
        let listed = self.call_rpc(
            "terminal.list",
            &json!({ "all": include_exited }),
            CALL_LIMIT,
        )?;
        let Some(items) = listed["terminals"].as_array() else {
            return Err(TerminalError(format!(
                "terminal.list missing terminals array: {listed}"
            )));
        };
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let Some(address) = item["address"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
            else {
                return Err(TerminalError(format!(
                    "terminal.list item missing non-empty address: {item}"
                )));
            };
            let machine = split_machine(&address);
            let labels = parse_labels(&item["labels"]);
            let origin = TerminalOrigin::from_labels(&labels);
            let state = parse_live_state(item)?;
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
                    Err(err) => return Err(err.into_terminal_error("terminal.read")),
                }
            } else {
                osc_status
                    .as_ref()
                    .map_or_else(crate::herdr_witness::no_agent, osc_fallback_reading)
            };

            out.push(DiscoveredTerminal {
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
            });
        }
        Ok(out)
    }

    /// Captures a structured [`TuiSnapshot`] of the current visible screen (without
    /// scrolling alt-screen history) for any local or peer terminal.
    ///
    /// For live terminals on framed `herdr-core` servers, passively observes via a
    /// temporary `terminal.attach(observe: true, history: 0)` paint stream so SGR
    /// styles (including reverse-video and color highlights), cursor coordinates,
    /// and alt-screen state are preserved. Falls back to `terminal.read(lines: 0)`
    /// (`SnapshotFidelity::PlainTextOnly`) when the terminal has finished or the
    /// server does not speak framed mode.
    pub fn snapshot(&self, address: &str) -> Result<TuiSnapshot, TerminalError> {
        let info = self.call_rpc("terminal.get", &json!({ "terminal": address }), CALL_LIMIT)?;
        let resolved_addr = info["address"].as_str().unwrap_or(address);
        let cols = u16::try_from(info["cols"].as_u64().unwrap_or(0)).unwrap_or(0);
        let rows = u16::try_from(info["rows"].as_u64().unwrap_or(0)).unwrap_or(0);
        let is_live = info["state"].as_str() == Some("live");
        let pid = is_live
            .then(|| info["pid"].as_u64().and_then(|v| u32::try_from(v).ok()))
            .flatten();
        let osc_status = OscStatus::from_json(&info["status"]);

        if is_live && let Some(raw_vt) = self.capture_styled_vt(resolved_addr, cols, rows)? {
            let mut snap = TuiSnapshot::from_styled_vt(RawStyledSnapshot {
                address: resolved_addr,
                cols,
                rows,
                title: info["title"].as_str(),
                raw_vt: &raw_vt,
                agent: None,
                agent_status: AgentStatus::Unknown,
                rule: None,
                blocker_visible: false,
            });
            let plain_text = snap.plain_lines.join("\n");
            let reading = resolve_reading(pid, &plain_text, osc_status.as_ref());
            snap.agent = reading.agent;
            snap.agent_status = reading.status;
            snap.rule = reading.rule;
            snap.blocker_visible = reading.blocker_visible;
            return Ok(snap);
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
    /// When [`TuiAction::SendText`] or [`TuiAction::SendKeys`] specifies
    /// `expected_guard: Some(want)`, performs an optimistic pre-flight verification by
    /// taking a fresh [`Self::snapshot`] right before issuing the write RPC (this is
    /// optimistic read-then-write verification across separate RPCs, not server-side
    /// atomic CAS). Refuses fail-closed if the terminal's menu selection cannot be
    /// observed (`unobservable_selection`) or if `snap.guard() != want` (`guard_mismatch`).
    pub fn execute(
        &self,
        address: &str,
        action: &TuiAction,
    ) -> Result<TuiActionOutcome, TerminalError> {
        match action {
            TuiAction::SendText {
                text,
                policy,
                expected_guard,
                wait_timeout_ms,
            } => {
                self.verify_optimistic_guard(address, *expected_guard)?;
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
    /// Fails closed before issuing any RPC if `labels` is empty, contains an empty,
    /// whitespace-containing, or `'='`-containing key, or attempts to write any
    /// Spawn-reserved key (`spawn_owner`, `spawn_terminal`, `spawn_workspace`, or any
    /// `spawn_*` prefix).
    pub fn tag_observe(
        &self,
        address: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>, TerminalError> {
        if labels.is_empty() {
            return Err(TerminalError(
                "tag_observe requires at least one label".to_string(),
            ));
        }
        for key in labels.keys() {
            if is_reserved_spawn_label(key) {
                return Err(TerminalError(format!(
                    "reserved_label: cannot mutate Spawn-managed label {key:?} via tag_observe"
                )));
            }
            if key.is_empty() || key.contains('=') || key.chars().any(char::is_whitespace) {
                return Err(TerminalError(format!("invalid_label_key: {key:?}")));
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
    ) -> Result<(), TerminalError> {
        let Some(want) = expected_guard else {
            return Ok(());
        };
        let actual = self.snapshot(address)?;
        if !actual.selection_observable() {
            return Err(TerminalError(format!(
                "unobservable_selection: terminal {address} menu selection cannot be verified under PlainTextOnly fidelity"
            )));
        }
        let got = actual.guard();
        if got != want {
            return Err(TerminalError(format!(
                "guard_mismatch: terminal {address} screen changed (expected {want}, got {got})"
            )));
        }
        Ok(())
    }

    fn wait_settled_inner(
        &self,
        address: &str,
        after_turn: Option<u64>,
        output_contains: Option<&str>,
        timeout: Duration,
    ) -> Result<WaitOutcome, TerminalError> {
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
            TerminalError(format!("terminal.wait missing boolean ended: {result}"))
        })?;
        let raw_reason = result["reason"].as_str().unwrap_or("unknown");
        let reason = WaitReason::from_herdr(raw_reason, wire_ended, exit_code, signal, from)
            .map_err(TerminalError)?;
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
    ) -> Result<Option<String>, TerminalError> {
        let mut stream = UnixStream::connect(&self.socket).map_err(|err| {
            TerminalError(format!(
                "herdr-core terminal.attach connect {}: {err}",
                self.socket.display()
            ))
        })?;
        let map_io =
            |err: std::io::Error| TerminalError(format!("herdr-core terminal.attach: {err}"));
        stream
            .set_read_timeout(Some(CAPTURE_LIMIT))
            .map_err(map_io)?;
        stream
            .set_write_timeout(Some(CAPTURE_LIMIT))
            .map_err(map_io)?;

        let hello_req = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "hello",
            "params": { "generation": 1, "framed": true },
        })
        .to_string();
        stream
            .write_all(format!("{hello_req}\n").as_bytes())
            .map_err(map_io)?;

        let hello_line = read_line_unbuffered(&mut stream).map_err(map_io)?;
        if hello_line.is_empty() {
            return Err(TerminalError(
                "herdr-core hello closed socket without reply".to_string(),
            ));
        }
        let hello_resp: Value = serde_json::from_str(&hello_line)
            .map_err(|err| TerminalError(format!("herdr-core hello invalid JSON: {err}")))?;
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
        .map_err(|err| TerminalError(format!("encode terminal.attach: {err}")))?;
        write_wire_frame(&mut stream, 0, WIRE_DATA, &attach_req).map_err(map_io)?;

        read_attach_first_paint(&mut stream)
    }

    fn call_rpc(
        &self,
        method: &str,
        params: &Value,
        limit: Duration,
    ) -> Result<Value, TerminalError> {
        self.call_rpc_raw(method, params, limit)
            .map_err(|err| err.into_terminal_error(method))
    }

    fn call_rpc_raw(
        &self,
        method: &str,
        params: &Value,
        limit: Duration,
    ) -> Result<Value, RpcFailure> {
        let stream = UnixStream::connect(&self.socket).map_err(|err| {
            RpcFailure::Other(TerminalError(format!(
                "herdr-core {method} connect {}: {err}",
                self.socket.display()
            )))
        })?;
        let map_io = |err: std::io::Error| {
            RpcFailure::Other(TerminalError(format!("herdr-core {method}: {err}")))
        };
        stream.set_read_timeout(Some(limit)).map_err(map_io)?;
        stream.set_write_timeout(Some(limit)).map_err(map_io)?;
        let mut line = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        })
        .to_string();
        line.push('\n');
        (&stream).write_all(line.as_bytes()).map_err(map_io)?;
        let mut answer = String::new();
        std::io::BufReader::new(&stream)
            .read_line(&mut answer)
            .map_err(map_io)?;
        if answer.is_empty() {
            return Err(RpcFailure::Other(TerminalError(format!(
                "herdr-core {method}: socket closed without reply"
            ))));
        }
        let parsed: Value = serde_json::from_str(&answer).map_err(|err| {
            RpcFailure::Other(TerminalError(format!(
                "herdr-core {method} invalid JSON: {err}"
            )))
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

fn read_attach_first_paint(stream: &mut UnixStream) -> Result<Option<String>, TerminalError> {
    let mut target_channel: Option<u32> = None;
    let mut paint_bytes = Vec::new();
    while let Some((channel, kind, payload)) = read_wire_frame(stream)? {
        if channel == 0 && kind == WIRE_DATA {
            let reply: Value = serde_json::from_slice(&payload).map_err(|err| {
                TerminalError(format!("herdr-core terminal.attach invalid JSON: {err}"))
            })?;
            if let Some(error) = reply.get("error") {
                let err_kind = error["data"]["kind"].as_str().unwrap_or("internal");
                if matches!(err_kind, "finished" | "not_found" | "needs_framed") {
                    return Ok(None);
                }
                let message = error["message"].as_str().unwrap_or_default();
                return Err(TerminalError(format!(
                    "herdr-core terminal.attach refused ({err_kind}): {message}"
                )));
            }
            let ch = reply["result"]["channel"]
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| {
                    TerminalError(format!("terminal.attach missing channel: {reply}"))
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
                let _ = write_wire_frame(stream, channel, WIRE_CREDIT, &credit.to_le_bytes());
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

fn read_line_unbuffered(stream: &mut UnixStream) -> std::io::Result<String> {
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0_u8; 1];
    loop {
        match stream.read_exact(&mut byte) {
            Ok(()) => {
                buf.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
                if buf.len() >= MAX_HELLO_LINE_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "herdr-core hello line exceeded limit",
                    ));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof && buf.is_empty() => {
                return Ok(String::new());
            }
            Err(err) => return Err(err),
        }
    }
    String::from_utf8(buf)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))
}

fn write_wire_frame(
    stream: &mut UnixStream,
    channel: u32,
    kind: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut frame = Vec::with_capacity(WIRE_HEADER_LEN + payload.len());
    frame.extend_from_slice(&channel.to_le_bytes());
    frame.push(kind);
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(payload);
    stream.write_all(&frame)
}

fn read_wire_frame(stream: &mut UnixStream) -> Result<Option<(u32, u8, Vec<u8>)>, TerminalError> {
    let mut header = [0_u8; WIRE_HEADER_LEN];
    match stream.read_exact(&mut header) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => {
            return Err(TerminalError(format!(
                "herdr-core read framed header: {err}"
            )));
        }
    }
    let channel = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let kind = header[4];
    let len = u32::from_le_bytes([header[5], header[6], header[7], header[8]]) as usize;
    if len > MAX_WIRE_PAYLOAD {
        return Err(TerminalError(format!(
            "herdr-core framed payload {len} exceeds {MAX_WIRE_PAYLOAD}"
        )));
    }
    let mut payload = vec![0_u8; len];
    stream
        .read_exact(&mut payload)
        .map_err(|err| TerminalError(format!("herdr-core read framed payload: {err}")))?;
    Ok(Some((channel, kind, payload)))
}

fn parse_live_state(item: &Value) -> Result<TerminalLiveState, TerminalError> {
    if item["state"].as_str() == Some("live") {
        let pid = item["pid"].as_u64().and_then(|v| u32::try_from(v).ok());
        return Ok(TerminalLiveState::Live { pid });
    }
    let code = item["exit_code"]
        .as_i64()
        .and_then(|v| i32::try_from(v).ok());
    let signal = item["signal"].as_i64().and_then(|v| i32::try_from(v).ok());
    let reason = item["reason"].as_str().map(str::to_string);
    let cause = ExitCause::from_herdr(code, signal, reason).map_err(TerminalError)?;
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

    fn mock_rpc_reply(method: &str, params: &Value) -> Value {
        match method {
            "hello" => json!({ "generation": 1, "framed": false }),
            "terminal.list" => json!({
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
            }),
            "terminal.get" => json!({
                "address": params["terminal"],
                "state": "live",
                "pid": 222,
                "cols": 120,
                "rows": 40,
                "title": "menu",
                "turn": 0
            }),
            "terminal.read" => json!({
                "text": "Menu:\n  1. First option\n  2. Second option\n",
                "complete": true,
                "how": "screen"
            }),
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
        assert_eq!(plain_snap.fidelity, SnapshotFidelity::PlainTextOnly);
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
            .expect_err("PlainTextOnly unmarked menu must fail closed on guarded mutation");
        assert!(err.0.contains("unobservable_selection"), "{err}");

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
                    expected_guard: None,
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
            assert!(err.0.contains("reserved_label"), "{forbidden}: {err}");
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
            assert!(
                err.0.contains("missing non-empty address")
                    || err.0.contains("contradictory Herdr exit state"),
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
            if cur.choices.len() == 2
                && cur.choices[selected_idx].selected
                && !cur.choices[1 - selected_idx].selected
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
        assert!(label_err.0.contains("reserved_label"), "{label_err}");

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
            snap1.fidelity,
            SnapshotFidelity::StyledVt {
                cursor: Some(_),
                alt_screen: false
            }
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
        assert_eq!(snap2.plain_lines, snap1.plain_lines);
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
        assert!(stale_err.0.contains("guard_mismatch"), "{stale_err}");

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
