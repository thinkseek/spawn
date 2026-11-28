//! In-memory Android device control relay for `spawn serve`.
//!
//! Connects enrolled Android devices (running Spawn Android's Accessibility
//! service and local guardrail gatekeeper) with operators on the Web console:
//!
//! - `GET  /api/devices` — list enrolled devices, readiness flags, latest
//!   redacted `UiNode` tree observation, and recent relay commands.
//! - `POST /api/devices/register` — register or refresh an Android device.
//! - `POST /api/devices/<id>/heartbeat` — update live status & optional screen tree.
//! - `POST /api/devices/<id>/commands` — enqueue one bounded direct action (`screen_observe`,
//!   `click_node`, `click_xy`, `input_text`, `scroll`, `global_action`, `launch_app`).
//! - `POST /api/devices/<id>/commands/poll` — claim queued pending commands for execution.
//! - `POST /api/devices/<id>/commands/<cmd_id>/result` — record execution verdict & observation.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, PoisonError};

use serde::Serialize;
use serde_json::{Value, json};
use tiny_http::{Method, Request};

use super::{
    AuthPrincipal, BAD_REQUEST, Shared, json_response, not_allowed, now_ms, percent_decoded,
    write_body,
};

const MAX_DEVICES: usize = 32;
const MAX_COMMANDS_PER_DEVICE: usize = 64;
const DEVICE_TTL_MS: i64 = 15 * 60_000;
const MAX_LABEL_BYTES: usize = 120;
const MAX_OBSERVATION_BYTES: usize = 256 * 1024;
const MAX_ACTION_PARAMS_BYTES: usize = 16 * 1024;
const DEFAULT_POLL_LIMIT: usize = 4;
const MAX_POLL_LIMIT: usize = 8;

const ALLOWED_ACTIONS: &[&str] = &[
    "screen_observe",
    "click_node",
    "click_xy",
    "input_text",
    "scroll",
    "global_action",
    "launch_app",
];

static NEXT_COMMAND_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CommandStatus {
    Pending,
    Dispatched,
    Executed,
    Denied,
    NeedsConfirmation,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct RelayCommand {
    pub command_id: String,
    pub action: String,
    pub params: Value,
    pub status: CommandStatus,
    pub created_at_ms: i64,
    pub dispatched_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub reason: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub(super) struct DeviceReadiness {
    pub control_enabled: bool,
    pub accessibility_ready: bool,
    pub notifications_ready: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub(super) struct DeviceRelayFlags {
    pub relay_enabled: bool,
    pub emergency_stopped: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct DeviceSnapshot {
    pub device_id: String,
    pub label: String,
    pub platform: String,
    pub app_version: Option<String>,
    pub active_package: Option<String>,
    #[serde(flatten)]
    pub readiness: DeviceReadiness,
    #[serde(flatten)]
    pub flags: DeviceRelayFlags,
    pub registered_at_ms: i64,
    pub last_seen_ms: i64,
    pub online: bool,
    pub latest_observation: Option<Value>,
    pub latest_observed_at_ms: Option<i64>,
    pub node_count: usize,
    pub password_redacted_count: usize,
    pub commands: Vec<RelayCommand>,
}

#[derive(Debug, Clone)]
struct DeviceEntry {
    device_id: String,
    label: String,
    platform: String,
    app_version: Option<String>,
    active_package: Option<String>,
    readiness: DeviceReadiness,
    flags: DeviceRelayFlags,
    registered_at_ms: i64,
    last_seen_ms: i64,
    latest_observation: Option<Value>,
    latest_observed_at_ms: Option<i64>,
    node_count: usize,
    password_redacted_count: usize,
    commands: VecDeque<RelayCommand>,
}

impl DeviceEntry {
    fn to_snapshot(&self, now: i64) -> DeviceSnapshot {
        let online = now.saturating_sub(self.last_seen_ms) <= 60_000;
        let commands = self.commands.iter().rev().take(20).cloned().collect();
        DeviceSnapshot {
            device_id: self.device_id.clone(),
            label: self.label.clone(),
            platform: self.platform.clone(),
            app_version: self.app_version.clone(),
            active_package: self.active_package.clone(),
            readiness: self.readiness,
            flags: self.flags,
            registered_at_ms: self.registered_at_ms,
            last_seen_ms: self.last_seen_ms,
            online,
            latest_observation: self.latest_observation.clone(),
            latest_observed_at_ms: self.latest_observed_at_ms,
            node_count: self.node_count,
            password_redacted_count: self.password_redacted_count,
            commands,
        }
    }
}

#[derive(Default)]
pub(super) struct DeviceRelayStore {
    devices: BTreeMap<String, DeviceEntry>,
}

impl DeviceRelayStore {
    fn prune_stale(&mut self, now: i64) {
        self.devices
            .retain(|_, entry| now.saturating_sub(entry.last_seen_ms) <= DEVICE_TTL_MS);
    }

    pub(super) fn list(&mut self, now: i64) -> Vec<DeviceSnapshot> {
        self.prune_stale(now);
        let mut list: Vec<DeviceSnapshot> = self
            .devices
            .values()
            .map(|entry| entry.to_snapshot(now))
            .collect();
        list.sort_by(|a, b| {
            b.last_seen_ms
                .cmp(&a.last_seen_ms)
                .then_with(|| a.device_id.cmp(&b.device_id))
        });
        list
    }

    fn evict_oldest_if_full(&mut self, device_id: &str) {
        if !self.devices.contains_key(device_id)
            && self.devices.len() >= MAX_DEVICES
            && let Some(oldest_key) = self
                .devices
                .iter()
                .min_by_key(|(_, entry)| entry.last_seen_ms)
                .map(|(k, _)| k.clone())
        {
            self.devices.remove(&oldest_key);
        }
    }

    pub(super) fn register(
        &mut self,
        body: &Value,
        now: i64,
    ) -> Result<DeviceSnapshot, (u16, String)> {
        self.prune_stale(now);
        let device_id = parse_device_id(
            body.get("device_id")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
        .ok_or_else(|| {
            (
                400,
                format!("{BAD_REQUEST}device_id must be 1..=96 [A-Za-z0-9._:-] characters"),
            )
        })?;
        self.evict_oldest_if_full(&device_id);

        let label = body
            .get("label")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(|| device_id.clone(), |s| truncate_utf8(s, MAX_LABEL_BYTES));
        let platform = body
            .get("platform")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(|| "android".to_string(), |s| truncate_utf8(s, 32));
        let entry = self
            .devices
            .entry(device_id.clone())
            .or_insert_with(|| DeviceEntry {
                device_id: device_id.clone(),
                label: label.clone(),
                platform: platform.clone(),
                app_version: None,
                active_package: None,
                readiness: DeviceReadiness::default(),
                flags: DeviceRelayFlags {
                    relay_enabled: true,
                    emergency_stopped: false,
                },
                registered_at_ms: now,
                last_seen_ms: now,
                latest_observation: None,
                latest_observed_at_ms: None,
                node_count: 0,
                password_redacted_count: 0,
                commands: VecDeque::new(),
            });

        entry.label = label;
        entry.platform = platform;
        apply_register_fields(entry, body, now);

        if let Some(obs) = body.get("observation").filter(|v| !v.is_null()) {
            apply_observation(entry, obs, body, now)?;
        }

        Ok(entry.to_snapshot(now))
    }

    pub(super) fn heartbeat(
        &mut self,
        device_id: &str,
        body: &Value,
        now: i64,
    ) -> Result<DeviceSnapshot, (u16, String)> {
        self.prune_stale(now);
        let entry = self
            .devices
            .get_mut(device_id)
            .ok_or_else(|| (404, format!("device {device_id} not registered")))?;
        entry.last_seen_ms = now;
        if let Some(pkg) = optional_short_text(body.get("active_package"), 160) {
            entry.active_package = Some(pkg);
        }
        apply_flag_updates(entry, body, now);
        if let Some(obs) = body.get("observation").filter(|v| !v.is_null()) {
            apply_observation(entry, obs, body, now)?;
        }
        Ok(entry.to_snapshot(now))
    }

    pub(super) fn enqueue_command(
        &mut self,
        device_id: &str,
        body: &Value,
        now: i64,
    ) -> Result<RelayCommand, (u16, String)> {
        self.prune_stale(now);
        let action = body
            .get("action")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if !ALLOWED_ACTIONS.contains(&action) {
            return Err((
                400,
                format!(
                    "{BAD_REQUEST}action must be one of {}",
                    ALLOWED_ACTIONS.join(", ")
                ),
            ));
        }
        let params = body.get("params").cloned().unwrap_or_else(|| json!({}));
        if !params.is_object() {
            return Err((400, format!("{BAD_REQUEST}params must be a JSON object")));
        }
        if serde_json::to_vec(&params).map_or(usize::MAX, |bytes| bytes.len())
            > MAX_ACTION_PARAMS_BYTES
        {
            return Err((
                413,
                format!("params exceeds {MAX_ACTION_PARAMS_BYTES} bytes"),
            ));
        }
        validate_action_params(action, &params)?;

        let entry = self
            .devices
            .get_mut(device_id)
            .ok_or_else(|| (404, format!("device {device_id} not registered")))?;
        if entry.flags.emergency_stopped {
            return Err((409, format!("device {device_id} is emergency stopped")));
        }
        if !entry.flags.relay_enabled {
            return Err((
                409,
                format!("device {device_id} has Spawn server relay turned off"),
            ));
        }

        if entry.commands.len() >= MAX_COMMANDS_PER_DEVICE {
            if let Some(pos) = entry.commands.iter().position(|cmd| {
                !matches!(
                    cmd.status,
                    CommandStatus::Pending | CommandStatus::Dispatched
                )
            }) {
                entry.commands.remove(pos);
            } else {
                return Err((
                    409,
                    format!(
                        "device {device_id} pending command queue full ({MAX_COMMANDS_PER_DEVICE})"
                    ),
                ));
            }
        }

        let seq = NEXT_COMMAND_SEQ.fetch_add(1, Ordering::Relaxed);
        let cmd = RelayCommand {
            command_id: format!("dcmd-{now}-{seq}"),
            action: action.to_string(),
            params,
            status: CommandStatus::Pending,
            created_at_ms: now,
            dispatched_at_ms: None,
            completed_at_ms: None,
            reason: None,
            message: None,
        };
        entry.commands.push_back(cmd.clone());
        Ok(cmd)
    }

    pub(super) fn poll_commands(
        &mut self,
        device_id: &str,
        body: &Value,
        now: i64,
    ) -> Result<(Vec<RelayCommand>, bool), (u16, String)> {
        self.prune_stale(now);
        let limit = body
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .map_or(DEFAULT_POLL_LIMIT, |n| n.clamp(1, MAX_POLL_LIMIT));
        let entry = self
            .devices
            .get_mut(device_id)
            .ok_or_else(|| (404, format!("device {device_id} not registered")))?;
        entry.last_seen_ms = now;
        if entry.flags.emergency_stopped || !entry.flags.relay_enabled {
            return Ok((Vec::new(), entry.flags.emergency_stopped));
        }
        let mut out = Vec::new();
        for cmd in &mut entry.commands {
            if cmd.status == CommandStatus::Pending {
                cmd.status = CommandStatus::Dispatched;
                cmd.dispatched_at_ms = Some(now);
                out.push(cmd.clone());
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok((out, entry.flags.emergency_stopped))
    }

    pub(super) fn report_result(
        &mut self,
        device_id: &str,
        command_id: &str,
        body: &Value,
        now: i64,
    ) -> Result<RelayCommand, (u16, String)> {
        self.prune_stale(now);
        let outcome = body
            .get("outcome")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let next_status = match outcome {
            "executed" => CommandStatus::Executed,
            "denied" => CommandStatus::Denied,
            "needs_confirmation" => CommandStatus::NeedsConfirmation,
            "failed" => CommandStatus::Failed,
            _ => {
                return Err((
                    400,
                    format!(
                        "{BAD_REQUEST}outcome must be executed, denied, needs_confirmation, or failed"
                    ),
                ));
            }
        };
        let reason = optional_short_text(body.get("reason"), 400);
        let message = optional_short_text(body.get("message"), 400);
        let active_pkg = optional_short_text(body.get("active_package"), 160);

        let entry = self
            .devices
            .get_mut(device_id)
            .ok_or_else(|| (404, format!("device {device_id} not registered")))?;
        entry.last_seen_ms = now;
        if let Some(pkg) = active_pkg {
            entry.active_package = Some(pkg);
        }
        if let Some(obs) = body.get("observation").filter(|v| !v.is_null()) {
            apply_observation(entry, obs, body, now)?;
        }

        let cmd = entry
            .commands
            .iter_mut()
            .find(|cmd| cmd.command_id == command_id)
            .ok_or_else(|| {
                (
                    404,
                    format!("command {command_id} not found for {device_id}"),
                )
            })?;
        cmd.status = next_status;
        cmd.completed_at_ms = Some(now);
        cmd.reason = reason;
        cmd.message = message;
        Ok(cmd.clone())
    }
}

fn cancel_pending_commands(commands: &mut VecDeque<RelayCommand>, now: i64, reason: &str) {
    for cmd in commands {
        if matches!(
            cmd.status,
            CommandStatus::Pending | CommandStatus::Dispatched
        ) {
            cmd.status = CommandStatus::Denied;
            cmd.completed_at_ms = Some(now);
            cmd.reason = Some(reason.to_string());
        }
    }
}

fn apply_observation(
    entry: &mut DeviceEntry,
    obs: &Value,
    envelope: &Value,
    now: i64,
) -> Result<(), (u16, String)> {
    let encoded_len = serde_json::to_vec(obs).map_or(usize::MAX, |bytes| bytes.len());
    if encoded_len > MAX_OBSERVATION_BYTES {
        return Err((
            413,
            format!("observation exceeds {MAX_OBSERVATION_BYTES} bytes"),
        ));
    }
    entry.latest_observation = Some(obs.clone());
    entry.latest_observed_at_ms = Some(now);
    if let Some(n) = envelope.get("node_count").and_then(Value::as_u64) {
        entry.node_count = usize::try_from(n).unwrap_or(usize::MAX);
    }
    if let Some(pw) = envelope
        .get("password_redacted_count")
        .and_then(Value::as_u64)
    {
        entry.password_redacted_count = usize::try_from(pw).unwrap_or(usize::MAX);
    }
    Ok(())
}

fn validate_action_params(action: &str, params: &Value) -> Result<(), (u16, String)> {
    match action {
        "screen_observe" => Ok(()),
        "click_node" => {
            if params
                .get("node_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .is_none_or(str::is_empty)
            {
                return Err((400, format!("{BAD_REQUEST}click_node requires node_id")));
            }
            Ok(())
        }
        "click_xy" => {
            let x = params.get("x").and_then(Value::as_i64);
            let y = params.get("y").and_then(Value::as_i64);
            if x.is_none_or(|v| v < 0) || y.is_none_or(|v| v < 0) {
                return Err((
                    400,
                    format!("{BAD_REQUEST}click_xy requires non-negative integer x and y"),
                ));
            }
            Ok(())
        }
        "input_text" => {
            if params
                .get("node_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .is_none_or(str::is_empty)
                || params.get("text").and_then(Value::as_str).is_none()
            {
                return Err((
                    400,
                    format!("{BAD_REQUEST}input_text requires node_id and text"),
                ));
            }
            Ok(())
        }
        "scroll" => {
            let dir = params
                .get("direction")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !matches!(dir, "up" | "down" | "left" | "right") {
                return Err((
                    400,
                    format!("{BAD_REQUEST}scroll direction must be up, down, left, or right"),
                ));
            }
            Ok(())
        }
        "global_action" => {
            let kind = params
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !matches!(kind, "back" | "home" | "recents") {
                return Err((
                    400,
                    format!("{BAD_REQUEST}global_action kind must be back, home, or recents"),
                ));
            }
            Ok(())
        }
        "launch_app" => {
            if params
                .get("package_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .is_none_or(str::is_empty)
            {
                return Err((
                    400,
                    format!("{BAD_REQUEST}launch_app requires package_name"),
                ));
            }
            Ok(())
        }
        _ => Err((400, format!("{BAD_REQUEST}unsupported action"))),
    }
}

fn parse_device_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 96 {
        return None;
    }
    if trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
    {
        Some(trimmed.to_string())
    } else {
        None
    }
}

fn optional_short_text(value: Option<&Value>, max_bytes: usize) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| truncate_utf8(s, max_bytes))
}

fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

pub(super) enum DeviceRoute {
    List,
    Register,
    Heartbeat {
        device_id: String,
    },
    EnqueueCommand {
        device_id: String,
    },
    PollCommands {
        device_id: String,
    },
    ReportResult {
        device_id: String,
        command_id: String,
    },
    Invalid,
}

fn decode_segment(raw: &str, max_len: usize) -> Option<String> {
    let decoded = percent_decoded(raw)?;
    if decoded.is_empty() || decoded.len() > max_len {
        return None;
    }
    if decoded
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
    {
        Some(decoded)
    } else {
        None
    }
}

pub(super) fn parse_device_route(path: &str) -> Option<DeviceRoute> {
    if path == "/api/devices" {
        return Some(DeviceRoute::List);
    }
    let rest = path.strip_prefix("/api/devices/")?;
    if rest == "register" {
        return Some(DeviceRoute::Register);
    }
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        [raw_id, "heartbeat"] => Some(match decode_segment(raw_id, 96) {
            Some(device_id) => DeviceRoute::Heartbeat { device_id },
            None => DeviceRoute::Invalid,
        }),
        [raw_id, "commands"] => Some(match decode_segment(raw_id, 96) {
            Some(device_id) => DeviceRoute::EnqueueCommand { device_id },
            None => DeviceRoute::Invalid,
        }),
        [raw_id, "commands", "poll"] => Some(match decode_segment(raw_id, 96) {
            Some(device_id) => DeviceRoute::PollCommands { device_id },
            None => DeviceRoute::Invalid,
        }),
        [raw_id, "commands", raw_cmd, "result"] => Some(
            match (decode_segment(raw_id, 96), decode_segment(raw_cmd, 96)) {
                (Some(device_id), Some(command_id)) => DeviceRoute::ReportResult {
                    device_id,
                    command_id,
                },
                _ => DeviceRoute::Invalid,
            },
        ),
        _ => None,
    }
}

static DEVICE_RELAY: LazyLock<Mutex<DeviceRelayStore>> =
    LazyLock::new(|| Mutex::new(DeviceRelayStore::default()));

fn apply_register_fields(entry: &mut DeviceEntry, body: &Value, now: i64) {
    if let Some(ver) = optional_short_text(body.get("app_version"), 48) {
        entry.app_version = Some(ver);
    }
    if let Some(pkg) = optional_short_text(body.get("active_package"), 160) {
        entry.active_package = Some(pkg);
    }
    entry.readiness = DeviceReadiness {
        control_enabled: body
            .get("control_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        accessibility_ready: body
            .get("accessibility_ready")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        notifications_ready: body
            .get("notifications_ready")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };
    let emergency_stopped = body
        .get("emergency_stopped")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    entry.flags = DeviceRelayFlags {
        relay_enabled: body
            .get("relay_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        emergency_stopped,
    };
    entry.last_seen_ms = now;
    if emergency_stopped {
        cancel_pending_commands(&mut entry.commands, now, "device emergency stopped");
    }
}

fn apply_flag_updates(entry: &mut DeviceEntry, body: &Value, now: i64) {
    if let Some(v) = body.get("control_enabled").and_then(Value::as_bool) {
        entry.readiness.control_enabled = v;
    }
    if let Some(v) = body.get("accessibility_ready").and_then(Value::as_bool) {
        entry.readiness.accessibility_ready = v;
    }
    if let Some(v) = body.get("notifications_ready").and_then(Value::as_bool) {
        entry.readiness.notifications_ready = v;
    }
    if let Some(v) = body.get("relay_enabled").and_then(Value::as_bool) {
        entry.flags.relay_enabled = v;
    }
    if let Some(v) = body.get("emergency_stopped").and_then(Value::as_bool) {
        entry.flags.emergency_stopped = v;
        if v {
            cancel_pending_commands(&mut entry.commands, now, "device emergency stopped");
        }
    }
}

pub(super) fn respond_device_route(
    request: Request,
    route: DeviceRoute,
    access: AuthPrincipal,
    shared: &Shared,
) -> Result<(), String> {
    match route {
        DeviceRoute::Invalid => request
            .respond(json_response(
                400,
                &json!({ "error": format!("{BAD_REQUEST}invalid device route identifier") }),
            ))
            .map_err(|error| error.to_string()),
        DeviceRoute::List => {
            if !matches!(request.method(), Method::Get | Method::Head) {
                return request
                    .respond(not_allowed("GET"))
                    .map_err(|error| error.to_string());
            }
            let now = now_ms();
            let devices = DEVICE_RELAY
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .list(now);
            request
                .respond(json_response(
                    200,
                    &json!({
                        "devices": devices,
                        "count": devices.len(),
                        "server_time_ms": now,
                    }),
                ))
                .map_err(|error| error.to_string())
        }
        DeviceRoute::Register => respond_post_json(
            request,
            access,
            shared,
            "registering a device",
            |store, parsed, now| {
                let device = store.register(parsed, now)?;
                Ok((200, json!({ "device": device, "server_time_ms": now })))
            },
        ),
        DeviceRoute::Heartbeat { device_id } => respond_post_json(
            request,
            access,
            shared,
            "updating a device heartbeat",
            |store, parsed, now| {
                let device = store.heartbeat(&device_id, parsed, now)?;
                Ok((200, json!({ "device": device, "server_time_ms": now })))
            },
        ),
        DeviceRoute::EnqueueCommand { device_id } => respond_post_json(
            request,
            access,
            shared,
            "queuing a device command",
            |store, parsed, now| {
                let command = store.enqueue_command(&device_id, parsed, now)?;
                Ok((202, json!({ "command": command, "server_time_ms": now })))
            },
        ),
        DeviceRoute::PollCommands { device_id } => respond_post_json(
            request,
            access,
            shared,
            "polling device commands",
            |store, parsed, now| {
                let (commands, emergency_stopped) = store.poll_commands(&device_id, parsed, now)?;
                Ok((
                    200,
                    json!({
                        "commands": commands,
                        "emergency_stopped": emergency_stopped,
                        "server_time_ms": now,
                    }),
                ))
            },
        ),
        DeviceRoute::ReportResult {
            device_id,
            command_id,
        } => respond_post_json(
            request,
            access,
            shared,
            "reporting a device command result",
            |store, parsed, now| {
                let command = store.report_result(&device_id, &command_id, parsed, now)?;
                Ok((200, json!({ "command": command, "server_time_ms": now })))
            },
        ),
    }
}

fn respond_post_json(
    mut request: Request,
    access: AuthPrincipal,
    shared: &Shared,
    what: &str,
    handler: impl FnOnce(&mut DeviceRelayStore, &Value, i64) -> Result<(u16, Value), (u16, String)>,
) -> Result<(), String> {
    if *request.method() != Method::Post {
        return request
            .respond(not_allowed("POST"))
            .map_err(|error| error.to_string());
    }
    let (status, body) = match parse_json_write(&mut request, access, shared, what) {
        Ok(parsed) => {
            let now = now_ms();
            let mut store = DEVICE_RELAY.lock().unwrap_or_else(PoisonError::into_inner);
            match handler(&mut store, &parsed, now) {
                Ok(pair) => pair,
                Err((code, msg)) => (code, json!({ "error": msg })),
            }
        }
        Err((code, msg)) => (code, json!({ "error": msg })),
    };
    request
        .respond(json_response(status, &body))
        .map_err(|error| error.to_string())
}

fn parse_json_write(
    request: &mut Request,
    access: AuthPrincipal,
    shared: &Shared,
    what: &str,
) -> Result<Value, (u16, String)> {
    let (raw_body, _) = write_body(request, access, shared, what)?;
    serde_json::from_slice::<Value>(&raw_body)
        .map_err(|err| (400, format!("{BAD_REQUEST}invalid JSON body: {err}")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{CommandStatus, DeviceRelayStore, MAX_COMMANDS_PER_DEVICE, MAX_DEVICES};

    #[test]
    fn device_relay_round_trip_and_emergency_stop() {
        let mut store = DeviceRelayStore::default();
        let now = 1_795_000_000_000i64;
        let reg = store
            .register(
                &json!({
                    "device_id": "pixel-tablet-1",
                    "label": "Pixel Tablet",
                    "control_enabled": true,
                    "accessibility_ready": true,
                    "notifications_ready": true,
                    "relay_enabled": true,
                    "observation": {"package_name": "com.example.notes", "nodes": 5},
                    "node_count": 5,
                    "password_redacted_count": 1,
                }),
                now,
            )
            .expect("register");
        assert_eq!(reg.device_id, "pixel-tablet-1");
        assert_eq!(reg.node_count, 5);
        assert_eq!(reg.password_redacted_count, 1);

        let enq = store
            .enqueue_command(
                "pixel-tablet-1",
                &json!({"action": "click_node", "params": {"node_id": "0.1"}}),
                now + 10,
            )
            .expect("enqueue");
        assert_eq!(enq.status, CommandStatus::Pending);

        let (polled, stopped) = store
            .poll_commands("pixel-tablet-1", &json!({"limit": 4}), now + 20)
            .expect("poll");
        assert!(!stopped);
        assert_eq!(polled.len(), 1);
        assert_eq!(polled[0].status, CommandStatus::Dispatched);

        let done = store
            .report_result(
                "pixel-tablet-1",
                &enq.command_id,
                &json!({
                    "outcome": "executed",
                    "message": "Clicked 0.1",
                    "active_package": "com.example.notes"
                }),
                now + 30,
            )
            .expect("report result");
        assert_eq!(done.status, CommandStatus::Executed);

        // Emergency stop denies any newly queued commands and blocks further enqueueing.
        let queued_before_stop = store
            .enqueue_command(
                "pixel-tablet-1",
                &json!({"action": "screen_observe", "params": {}}),
                now + 40,
            )
            .expect("enqueue before stop");
        store
            .heartbeat(
                "pixel-tablet-1",
                &json!({"emergency_stopped": true}),
                now + 50,
            )
            .expect("heartbeat stop");
        let (polled_after_stop, stopped_flag) = store
            .poll_commands("pixel-tablet-1", &json!({}), now + 60)
            .expect("poll after stop");
        assert!(stopped_flag);
        assert!(polled_after_stop.is_empty());

        let err = store
            .enqueue_command(
                "pixel-tablet-1",
                &json!({"action": "screen_observe", "params": {}}),
                now + 70,
            )
            .expect_err("enqueue during emergency stop fails");
        assert_eq!(err.0, 409);

        let list = store.list(now + 80);
        assert_eq!(list.len(), 1);
        let canceled = list[0]
            .commands
            .iter()
            .find(|c| c.command_id == queued_before_stop.command_id)
            .expect("canceled command present");
        assert_eq!(canceled.status, CommandStatus::Denied);
    }

    #[test]
    fn device_relay_enforces_max_devices_and_command_bounds() {
        let mut store = DeviceRelayStore::default();
        let now = 1_000_000;
        for idx in 0..=MAX_DEVICES {
            let offset = i64::try_from(idx).unwrap_or(0);
            store
                .register(
                    &json!({"device_id": format!("dev-{idx}"), "relay_enabled": true}),
                    now + offset,
                )
                .expect("register");
        }
        assert_eq!(store.list(now + 100).len(), MAX_DEVICES);

        for idx in 0..MAX_COMMANDS_PER_DEVICE {
            let offset = i64::try_from(idx).unwrap_or(0);
            store
                .enqueue_command(
                    "dev-1",
                    &json!({"action": "screen_observe", "params": {}}),
                    now + 200 + offset,
                )
                .expect("queue within cap");
        }
        let full = store
            .enqueue_command(
                "dev-1",
                &json!({"action": "screen_observe", "params": {}}),
                now + 300,
            )
            .expect_err("pending queue cap enforced");
        assert_eq!(full.0, 409);
    }
}
