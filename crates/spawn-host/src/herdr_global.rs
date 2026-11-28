//! Zero-invasion global Herdr (`herdr-core` / `spawn-hc`) discovery and control client.
//!
//! Connects directly to any `herdr-core` UDS socket without filtering by
//! `spawn_owner` labels, letting Spawn discover, snapshot (with deterministic CAS
//! hashes), drive with full VT key chords in a single RPC (zero artificial sleep),
//! interrupt, and wait on both Spawn-managed and external/human-launched terminals.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use spawn_core::port::{AgentReading, AgentStatus, TerminalError};
use spawn_core::tui::{
    DiscoveredTerminal, InterruptOutcome, InterruptReport, KeyChord, RawTuiSnapshot,
    SendPrecondition, TerminalLiveState, TerminalOrigin, TuiSnapshot, WaitOutcome, WaitReason,
};

use crate::herdr_witness::{OscStatus, observe};

const CALL_LIMIT: Duration = Duration::from_secs(10);
const CAPTURE_LIMIT: Duration = Duration::from_secs(15);
const WRITE_LIMIT: Duration = Duration::from_secs(20);
const WAIT_BUFFER: Duration = Duration::from_secs(5);

/// Synchronous, zero-invasion client over a `herdr-core` / `spawn-hc` UDS socket.
#[derive(Debug, Clone)]
pub struct HerdrGlobalClient {
    socket: PathBuf,
}

impl HerdrGlobalClient {
    /// Connects subsequent calls to the `herdr-core` UDS endpoint at `socket`.
    #[must_use]
    pub fn at_socket(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// Configured UDS socket path.
    #[must_use]
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
            let Some(address) = item["address"].as_str().map(str::to_string) else {
                continue;
            };
            let machine = split_machine(&address);
            let labels = parse_labels(&item["labels"]);
            let origin = TerminalOrigin::from_labels(&labels);
            let is_live = item["state"].as_str() == Some("live");
            let state = if is_live {
                TerminalLiveState::Live
            } else {
                TerminalLiveState::Exited {
                    code: item["exit_code"]
                        .as_i64()
                        .and_then(|v| i32::try_from(v).ok()),
                    signal: item["signal"].as_i64().and_then(|v| i32::try_from(v).ok()),
                    reason: item["reason"].as_str().map(str::to_string),
                }
            };
            let pid = item["pid"].as_u64().and_then(|v| u32::try_from(v).ok());
            let osc_status = OscStatus::from_json(&item["status"]);
            let reading = if is_live && machine.is_none() {
                // `lines: 0` reads only the visible screen so discovery never scrolls alt-screen TUIs.
                match self.call_rpc(
                    "terminal.read",
                    &json!({ "terminal": address, "lines": 0 }),
                    CAPTURE_LIMIT,
                ) {
                    Ok(read) => resolve_reading(
                        pid,
                        read["text"].as_str().unwrap_or_default(),
                        osc_status.as_ref(),
                    ),
                    Err(_) => osc_status
                        .as_ref()
                        .map_or_else(crate::herdr_witness::no_agent, osc_fallback_reading),
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
                pid,
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
    /// scrolling alt-screen history), including plain lines, choices, witness status,
    /// and deterministic CAS hash for any local or peer terminal by address or name.
    pub fn snapshot(&self, address: &str) -> Result<TuiSnapshot, TerminalError> {
        let info = self.call_rpc("terminal.get", &json!({ "terminal": address }), CALL_LIMIT)?;
        let read = self.call_rpc(
            "terminal.read",
            &json!({ "terminal": address, "lines": 0 }),
            CAPTURE_LIMIT,
        )?;
        let raw_text = read["text"].as_str().unwrap_or_default();
        let pid = info["pid"].as_u64().and_then(|v| u32::try_from(v).ok());
        let osc_status = OscStatus::from_json(&info["status"]);
        let reading = resolve_reading(pid, raw_text, osc_status.as_ref());
        let cols = u16::try_from(info["cols"].as_u64().unwrap_or(0)).unwrap_or(0);
        let rows = u16::try_from(info["rows"].as_u64().unwrap_or(0)).unwrap_or(0);
        let resolved_addr = info["address"].as_str().unwrap_or(address);
        let how = read["how"].as_str().unwrap_or("screen");
        let complete = read["complete"].as_bool().unwrap_or(true);
        Ok(TuiSnapshot::from_raw(RawTuiSnapshot {
            address: resolved_addr,
            cols,
            rows,
            how,
            complete,
            why: read["why"].as_str(),
            title: info["title"].as_str(),
            raw_text,
            agent: reading.agent.as_deref(),
            agent_status: reading.status,
            rule: reading.rule.as_deref(),
            blocker_visible: reading.blocker_visible,
        }))
    }

    /// Pastes `text` and submits Enter via `terminal.send`, optionally verifying the
    /// screen CAS digest first (`expected_hash`) and awaiting turn completion (`wait_timeout`).
    pub fn send_text(
        &self,
        address: &str,
        text: &str,
        force: bool,
        precondition: SendPrecondition,
        expected_hash: Option<u64>,
        wait_timeout: Option<Duration>,
    ) -> Result<Value, TerminalError> {
        self.verify_cas_hash(address, expected_hash)?;
        let mut params = serde_json::Map::new();
        params.insert("terminal".to_string(), json!(address));
        params.insert("text".to_string(), json!(text));
        params.insert("force".to_string(), json!(force));
        if let Some(cond) = precondition.as_herdr_if() {
            params.insert("if".to_string(), json!(cond));
        }
        let rpc_limit = match wait_timeout {
            Some(wait_dur) => {
                params.insert("wait".to_string(), json!(true));
                params.insert("timeout".to_string(), json!(wait_dur.as_secs_f64()));
                wait_dur.saturating_add(WAIT_BUFFER)
            }
            None => WRITE_LIMIT,
        };
        self.call_rpc("terminal.send", &Value::Object(params), rpc_limit)
    }

    /// Sends one or more [`KeyChord`]s in a single `terminal.keys` RPC without any
    /// artificial `KEY_PAUSE` sleep, optionally guarded by a screen CAS digest.
    pub fn send_keys(
        &self,
        address: &str,
        keys: &[KeyChord],
        expected_hash: Option<u64>,
    ) -> Result<Value, TerminalError> {
        if keys.is_empty() {
            return Err(TerminalError(
                "send_keys requires at least one KeyChord".to_string(),
            ));
        }
        self.verify_cas_hash(address, expected_hash)?;
        let specs: Vec<String> = keys.iter().map(KeyChord::to_herdr_spec).collect();
        self.call_rpc(
            "terminal.keys",
            &json!({ "terminal": address, "keys": specs }),
            CALL_LIMIT,
        )
    }

    /// Sends a native `terminal.interrupt` (`esc` for OSC-reporting programs, else `ctrl+c`).
    pub fn interrupt(
        &self,
        address: &str,
        timeout: Option<Duration>,
    ) -> Result<InterruptReport, TerminalError> {
        let mut params = serde_json::Map::new();
        params.insert("terminal".to_string(), json!(address));
        let rpc_limit = match timeout {
            Some(dur) => {
                params.insert("timeout".to_string(), json!(dur.as_secs_f64()));
                dur.saturating_add(WAIT_BUFFER)
            }
            None => CALL_LIMIT.saturating_add(WAIT_BUFFER),
        };
        let result = self.call_rpc("terminal.interrupt", &Value::Object(params), rpc_limit)?;
        Ok(InterruptReport {
            outcome: InterruptOutcome::parse(result["outcome"].as_str().unwrap_or("unknown")),
            key_sent: result["key"].as_str().map(str::to_string),
        })
    }

    /// Event-driven wait via `terminal.wait` until the terminal settles or `output_contains`
    /// appears, bounded by `timeout`. When `after_turn` is given and `terminal.get` shows
    /// the turn already advanced cleanly while no longer working, returns immediately.
    pub fn wait_settled(
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
            let live = info["state"].as_str() == Some("live");
            if !live || (cur_turn > prev_turn && cur_status.as_deref() != Some("working")) {
                return Ok(WaitOutcome {
                    terminal: info["address"].as_str().unwrap_or(address).to_string(),
                    ended: true,
                    reason: if live {
                        WaitReason::Status
                    } else {
                        WaitReason::Exit
                    },
                    status: cur_status,
                    turn: cur_turn,
                    exit_code: info["exit_code"]
                        .as_i64()
                        .and_then(|v| i32::try_from(v).ok()),
                    signal: info["signal"].as_i64().and_then(|v| i32::try_from(v).ok()),
                    from: None,
                });
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
        Ok(WaitOutcome {
            terminal: result["terminal"].as_str().unwrap_or(address).to_string(),
            ended: result["ended"].as_bool().unwrap_or(false),
            reason: WaitReason::parse(result["reason"].as_str().unwrap_or("unknown")),
            status: result["status"].as_str().map(str::to_string),
            turn: result["turn"].as_u64().unwrap_or(0),
            exit_code: result["exit_code"]
                .as_i64()
                .and_then(|v| i32::try_from(v).ok()),
            signal: result["signal"]
                .as_i64()
                .and_then(|v| i32::try_from(v).ok()),
            from: result["from"].as_str().map(str::to_string),
        })
    }

    /// Attaches or updates metadata labels (`""` removes a key) without restarting
    /// or disrupting the terminal.
    pub fn tag_observe(
        &self,
        address: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<Value, TerminalError> {
        if labels.is_empty() {
            return Err(TerminalError(
                "tag_observe requires at least one label".to_string(),
            ));
        }
        self.call_rpc(
            "terminal.label",
            &json!({ "terminal": address, "labels": labels }),
            CALL_LIMIT,
        )
    }

    fn verify_cas_hash(
        &self,
        address: &str,
        expected_hash: Option<u64>,
    ) -> Result<(), TerminalError> {
        let Some(want) = expected_hash else {
            return Ok(());
        };
        let actual = self.snapshot(address)?.snapshot_hash;
        if actual != want {
            return Err(TerminalError(format!(
                "cas_mismatch: terminal {address} screen changed (expected {want:#018x}, got {actual:#018x})"
            )));
        }
        Ok(())
    }

    fn call_rpc(
        &self,
        method: &str,
        params: &Value,
        limit: Duration,
    ) -> Result<Value, TerminalError> {
        let stream = UnixStream::connect(&self.socket).map_err(|err| {
            TerminalError(format!(
                "herdr-core {method} connect {}: {err}",
                self.socket.display()
            ))
        })?;
        let map_io = |err: std::io::Error| TerminalError(format!("herdr-core {method}: {err}"));
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
            return Err(TerminalError(format!(
                "herdr-core {method}: socket closed without reply"
            )));
        }
        let parsed: Value = serde_json::from_str(&answer)
            .map_err(|err| TerminalError(format!("herdr-core {method} invalid JSON: {err}")))?;
        if let Some(error) = parsed.get("error") {
            let kind = error["data"]["kind"].as_str().unwrap_or("internal");
            let message = error["message"].as_str().unwrap_or_default();
            return Err(TerminalError(format!(
                "herdr-core {method} refused ({kind}): {message}"
            )));
        }
        Ok(parsed["result"].clone())
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
                "text": "Menu:\n❯ 1. First option\n  2. Second option\n",
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
    fn mock_rpc_covers_unfiltered_discovery_cas_keys_interrupt_wait_and_label() {
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

        let snap = client.snapshot("t2").expect("snapshot");
        assert_eq!(snap.choices.len(), 2);
        assert!(snap.choices[0].selected);

        let err = client
            .send_keys(
                "t2",
                &[KeyChord::parse("down").expect("down")],
                Some(snap.snapshot_hash ^ 0x55),
            )
            .expect_err("wrong CAS hash must refuse");
        assert!(err.0.contains("cas_mismatch"), "{err}");

        client
            .send_keys(
                "t2",
                &[
                    KeyChord::parse("down").expect("down"),
                    KeyChord::parse("enter").expect("enter"),
                ],
                Some(snap.snapshot_hash),
            )
            .expect("send_keys with matching CAS succeeds in one RPC");
        client
            .send_text(
                "t2",
                "hello",
                true,
                SendPrecondition::OnlyIfIdle,
                None,
                Some(Duration::from_secs(2)),
            )
            .expect("send_text succeeds");

        let interrupt = client
            .interrupt("t2", Some(Duration::from_secs(2)))
            .expect("interrupt");
        assert_eq!(interrupt.outcome, InterruptOutcome::Stopped);
        assert_eq!(interrupt.key_sent.as_deref(), Some("esc"));

        let waited = client
            .wait_settled("t2", None, Some("PICKED_2"), Duration::from_secs(2))
            .expect("wait_settled");
        assert!(waited.ended);
        assert_eq!(waited.reason, WaitReason::Output);

        let mut tags = BTreeMap::new();
        tags.insert("observed_by".to_string(), "test".to_string());
        client.tag_observe("t2", &tags).expect("tag_observe");

        let _ = client.call_rpc("__stop", &json!({}), CALL_LIMIT);
        srv.join().expect("mock server thread");

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
    }

    fn spawn_hc_binary() -> Option<String> {
        if let Some(prog) = std::env::var_os("SPAWN_HERDR_CORE_PROGRAM")
            .map(|v| v.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
        {
            return Some(prog);
        }
        if let Ok(exe) = std::env::current_exe() {
            for dir in [exe.parent(), exe.parent().and_then(Path::parent)]
                .into_iter()
                .flatten()
            {
                let candidate = dir.join("spawn-hc");
                if candidate.is_file() {
                    return Some(candidate.display().to_string());
                }
            }
        }
        None
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

    const MENU_SCRIPT: &str = r#"import sys, termios, tty, time
fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
sel = 0
items = ["First Option", "Second Option"]
def draw():
    sys.stdout.write("\x1b[2J\x1b[1;1HSelect action:\r\n")
    for idx, label in enumerate(items):
        mark = "❯" if idx == sel else " "
        sys.stdout.write(f" {mark} {idx + 1}. {label}\r\n")
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
        std::fs::write(&script_path, MENU_SCRIPT).expect("write menu script");
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

    fn await_menu_snapshot(client: &HerdrGlobalClient, addr: &str) -> TuiSnapshot {
        for _ in 0..40 {
            let cur = client.snapshot(addr).expect("snapshot");
            if cur.choices.len() == 2 && cur.choices[0].selected {
                return cur;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("interactive menu choices never rendered");
    }

    #[test]
    fn real_spawn_hc_global_discover_snapshot_cas_keys_interrupt_and_wait() {
        let Some(program) = spawn_hc_binary() else {
            eprintln!("skipping real_spawn_hc_global_* test: spawn-hc binary not built yet");
            return;
        };
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
        assert_eq!(ext.state, TerminalLiveState::Live);
        assert!(ext.pid.is_some());

        let snap = await_menu_snapshot(&client, &addr);
        assert_eq!(snap.choices[0].index, Some(1));
        assert_eq!(snap.choices[1].index, Some(2));

        let mismatch = client
            .send_keys(
                &addr,
                &[KeyChord::parse("enter").expect("enter")],
                Some(snap.snapshot_hash ^ 1),
            )
            .expect_err("wrong CAS hash must be rejected");
        assert!(mismatch.0.contains("cas_mismatch"), "{mismatch}");

        let watch_sock = env.socket.clone();
        let watch_addr = addr.clone();
        let watch_handle = std::thread::spawn(move || {
            HerdrGlobalClient::at_socket(&watch_sock).wait_settled(
                &watch_addr,
                None,
                Some("PICKED_2"),
                Duration::from_secs(5),
            )
        });
        std::thread::sleep(Duration::from_millis(80));

        client
            .send_keys(
                &addr,
                &[
                    KeyChord::parse("down").expect("down"),
                    KeyChord::parse("enter").expect("enter"),
                ],
                Some(snap.snapshot_hash),
            )
            .expect("send_keys down+enter");

        let waited = watch_handle
            .join()
            .expect("wait thread")
            .expect("wait_settled on PICKED_2");
        assert!(waited.ended, "expected wait output match: {waited:?}");
        assert_eq!(waited.reason, WaitReason::Output);

        let mut labels = BTreeMap::new();
        labels.insert("observed_by".to_string(), "ar1_test".to_string());
        client.tag_observe(&addr, &labels).expect("tag_observe");
        let after_tag = client.discover_all(false).expect("discover after tag");
        let tagged = after_tag
            .iter()
            .find(|t| t.address == addr)
            .expect("still present");
        assert_eq!(
            tagged.labels.get("observed_by").map(String::as_str),
            Some("ar1_test")
        );

        let int_report = client
            .interrupt(&addr, Some(Duration::from_secs(5)))
            .expect("interrupt");
        assert!(
            int_report.key_sent.is_some(),
            "expected key_sent in InterruptReport: {int_report:?}"
        );
        assert!(matches!(
            int_report.outcome,
            InterruptOutcome::Exited | InterruptOutcome::Stopped | InterruptOutcome::Unknown
        ));
        eprintln!("REAL_SPAWN_HC_GLOBAL_VERIFIED address={addr}");
    }
}
