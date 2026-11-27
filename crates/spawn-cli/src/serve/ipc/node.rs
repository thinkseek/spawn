//! Typed `/ipc/v1/node/*` bridges backing Go `spawn-node-gw` over UDS
//! (`docs/spawn-ipc-v1.md` §3.3).
//!
//! SQL writes run through `spawn_node_gateway` (`node_gw_*`); CAS metadata/pin
//! operations run inside a single `spawn_cas::Store::lock_gc()` flock.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use spawn_cas::{Digest, Store, is_valid_name};
use spawn_core::node_proto::codec::{is_lower_hex, unhex};
use spawn_postgres::NodeEnroll;

use super::IpcState;
use super::http::{IpcRequest, IpcResponse};

static PIN_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeKeyReq {
    node_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeEnrollReq {
    token_sha256_hex: String,
    pubkey_b64: String,
    boot_id: String,
    spec_sha256: String,
    spec: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeBeatReq {
    node_id: String,
    boot_id: String,
    seq: i64,
    beat: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeSpecReq {
    node_id: String,
    boot_id: String,
    spec_sha256: String,
    spec: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeResultReq {
    node_id: String,
    job_id: String,
    lease_epoch: i64,
    result: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeCasReq {
    action: String,
    digest: String,
    #[serde(default = "default_cas_namespace")]
    namespace: String,
    #[serde(default = "default_pin_owner")]
    owner: String,
    #[serde(default, rename = "ref")]
    pin_ref: Option<String>,
}

fn default_cas_namespace() -> String {
    "artifacts".to_string()
}

fn default_pin_owner() -> String {
    "node-gw".to_string()
}

fn err_json(status: u16, code: &str, msg: impl Into<String>) -> IpcResponse {
    IpcResponse::json(status, &json!({ "error": msg.into(), "code": code }))
}

fn decode_hex32(text: &str, field: &str) -> Result<[u8; 32], IpcResponse> {
    let bytes = unhex(text, 64).ok_or_else(|| {
        err_json(
            400,
            "bad_request",
            format!("{field} must be 64 lowercase hex"),
        )
    })?;
    bytes
        .try_into()
        .map_err(|_| err_json(400, "bad_request", format!("{field} length mismatch")))
}

fn decode_pubkey32(text: &str) -> Result<[u8; 32], IpcResponse> {
    let trimmed = text.trim();
    let decoded = STANDARD
        .decode(trimmed)
        .or_else(|_| URL_SAFE_NO_PAD.decode(trimmed))
        .map_err(|_| err_json(400, "bad_request", "pubkey_b64 is invalid base64"))?;
    decoded
        .try_into()
        .map_err(|_| err_json(400, "bad_request", "pubkey_b64 must decode to 32 bytes"))
}

pub(super) fn handle_node_route(pattern: &str, req: &IpcRequest, state: &IpcState) -> IpcResponse {
    let Some(_permit) = state.pools.node_gate.try_acquire() else {
        return err_json(503, "busy", "busy: node IPC pool saturated");
    };
    match pattern {
        "/ipc/v1/node/key" => handle_node_key(&req.body, state),
        "/ipc/v1/node/enroll" => handle_node_enroll(&req.body, state),
        "/ipc/v1/node/beat" => handle_node_beat(&req.body, state),
        "/ipc/v1/node/spec" => handle_node_spec(&req.body, state),
        "/ipc/v1/node/result" => handle_node_result(&req.body, state),
        "/ipc/v1/node/cas" => handle_node_cas(&req.body, &state.cas_root),
        _ => err_json(404, "not_found", "unknown node IPC route"),
    }
}

fn handle_node_key(body: &[u8], state: &IpcState) -> IpcResponse {
    let parsed: NodeKeyReq = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return err_json(400, "bad_request", format!("bad node/key body: {e}")),
    };
    if !is_lower_hex(&parsed.node_id, 64) {
        return err_json(400, "bad_request", "node_id must be 64 lowercase hex");
    }
    let gw_conn = match state.pools.node.checkout() {
        Ok(c) => c,
        Err(e) => return err_json(500, "internal", e),
    };
    // Probe spawn_node_gw_spec inside a transaction that ALWAYS rolls back
    // (`|_| false`): extracts pubkey (base64) and revoked status using only the
    // restricted `spawn_node_gateway` login without mutating `spawn_node_state`.
    let mut probed: Option<Value> = None;
    let dummy_boot = "00000000000000000000000000000000";
    if let Err(e) = gw_conn.node_gw_spec(
        &parsed.node_id,
        dummy_boot,
        "probe-only",
        &json!({}),
        |ans| {
            probed = Some(ans.clone());
            false
        },
    ) {
        return err_json(500, "internal", e.to_string());
    }
    let Some(ans) = probed else {
        return err_json(404, "unknown_node", "unknown node");
    };
    if ans["error"] == "unknown_node" {
        return err_json(404, "unknown_node", "unknown node");
    }
    let Some(pubkey_b64) = ans["pubkey"].as_str().map(|s| s.trim().to_string()) else {
        return err_json(404, "unknown_node", "node has no enrolled public key");
    };
    let revoked = ans["error"] == "revoked";
    let boot_id = state.pools.reader.checkout().ok().and_then(|reader_conn| {
        reader_conn
            .nodes_v1()
            .ok()?
            .into_iter()
            .find(|n| n.node_id.as_deref() == Some(&parsed.node_id))
            .and_then(|n| n.boot_id)
    });
    IpcResponse::json(
        200,
        &json!({
            "node_id": parsed.node_id,
            "pubkey_b64": pubkey_b64,
            "boot_id": boot_id,
            "revoked": revoked,
        }),
    )
}

fn handle_node_enroll(body: &[u8], state: &IpcState) -> IpcResponse {
    let parsed: NodeEnrollReq = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return err_json(400, "bad_request", format!("bad node/enroll body: {e}")),
    };
    let token_sha256 = match decode_hex32(&parsed.token_sha256_hex, "token_sha256_hex") {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let pubkey = match decode_pubkey32(&parsed.pubkey_b64) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if !is_lower_hex(&parsed.boot_id, 32) || !is_lower_hex(&parsed.spec_sha256, 64) {
        return err_json(400, "bad_request", "invalid boot_id or spec_sha256");
    }
    let gw_conn = match state.pools.node.checkout() {
        Ok(c) => c,
        Err(e) => return err_json(500, "internal", e),
    };
    let call = NodeEnroll {
        token_sha256: &token_sha256,
        pubkey: &pubkey,
        boot_id: &parsed.boot_id,
        spec_sha256: &parsed.spec_sha256,
        spec: &parsed.spec,
    };
    match gw_conn.node_gw_enroll(&call, |ans| ans["ok"] == true) {
        Ok(committed) => IpcResponse::json(200, &committed.answer),
        Err(e) => err_json(500, "internal", e.to_string()),
    }
}

fn handle_node_beat(body: &[u8], state: &IpcState) -> IpcResponse {
    let parsed: NodeBeatReq = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return err_json(400, "bad_request", format!("bad node/beat body: {e}")),
    };
    if !is_lower_hex(&parsed.node_id, 64) || !is_lower_hex(&parsed.boot_id, 32) || parsed.seq < 1 {
        return err_json(400, "bad_request", "invalid node_id, boot_id, or seq");
    }
    let gw_conn = match state.pools.node.checkout() {
        Ok(c) => c,
        Err(e) => return err_json(500, "internal", e),
    };
    match gw_conn.node_gw_beat(
        &parsed.node_id,
        &parsed.boot_id,
        parsed.seq,
        &parsed.beat,
        |ans| ans["ok"] == true,
    ) {
        Ok(committed) => IpcResponse::json(200, &committed.answer),
        Err(e) => err_json(500, "internal", e.to_string()),
    }
}

fn handle_node_spec(body: &[u8], state: &IpcState) -> IpcResponse {
    let parsed: NodeSpecReq = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return err_json(400, "bad_request", format!("bad node/spec body: {e}")),
    };
    if !is_lower_hex(&parsed.node_id, 64)
        || !is_lower_hex(&parsed.boot_id, 32)
        || !is_lower_hex(&parsed.spec_sha256, 64)
    {
        return err_json(
            400,
            "bad_request",
            "invalid node_id, boot_id, or spec_sha256",
        );
    }
    let gw_conn = match state.pools.node.checkout() {
        Ok(c) => c,
        Err(e) => return err_json(500, "internal", e),
    };
    match gw_conn.node_gw_spec(
        &parsed.node_id,
        &parsed.boot_id,
        &parsed.spec_sha256,
        &parsed.spec,
        |ans| ans["ok"] == true,
    ) {
        Ok(committed) => IpcResponse::json(200, &committed.answer),
        Err(e) => err_json(500, "internal", e.to_string()),
    }
}

fn handle_node_result(body: &[u8], state: &IpcState) -> IpcResponse {
    let parsed: NodeResultReq = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return err_json(400, "bad_request", format!("bad node/result body: {e}")),
    };
    if !is_lower_hex(&parsed.node_id, 64) || parsed.job_id.is_empty() || parsed.lease_epoch < 1 {
        return err_json(
            400,
            "bad_request",
            "invalid node_id, job_id, or lease_epoch",
        );
    }
    let gw_conn = match state.pools.node.checkout() {
        Ok(c) => c,
        Err(e) => return err_json(500, "internal", e),
    };
    match gw_conn.node_gw_result(
        &parsed.node_id,
        &parsed.job_id,
        parsed.lease_epoch,
        &parsed.result,
        |done| *done,
    ) {
        Ok(committed) => IpcResponse::json(
            200,
            &json!({
                "ok": committed.answer,
                "committed": committed.committed,
                "lease_fenced": !committed.answer,
            }),
        ),
        Err(e) => err_json(500, "internal", e.to_string()),
    }
}

enum CasLock {
    Shared { _lock: spawn_cas::GcSharedLock },
    Exclusive { _lock: spawn_cas::GcLock },
}

pub(super) fn handle_node_cas(body: &[u8], cas_root: &Path) -> IpcResponse {
    let parsed: NodeCasReq = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return err_json(400, "bad_request", format!("bad node/cas body: {e}")),
    };
    let action = parsed.action.as_str();
    if !matches!(action, "stat" | "pin" | "register") {
        return err_json(400, "bad_request", "action must be stat, pin, or register");
    }
    let digest = match Digest::parse(&parsed.digest) {
        Ok(d) => d,
        Err(e) => return err_json(400, "bad_request", e.to_string()),
    };
    if !is_valid_name(&parsed.namespace) || !is_valid_name(&parsed.owner) {
        return err_json(400, "bad_request", "invalid CAS namespace or owner");
    }
    if let Some(ref pin_name) = parsed.pin_ref
        && !is_valid_name(pin_name)
    {
        return err_json(400, "bad_request", "invalid pin ref name");
    }
    let store = match Store::open(cas_root) {
        Ok(s) => s,
        Err(e) => return err_json(500, "internal", format!("open CAS store: {e}")),
    };
    let lock_guard = if action == "stat" {
        store
            .lock_gc_shared()
            .map(|lock| CasLock::Shared { _lock: lock })
    } else {
        store
            .lock_gc()
            .map(|lock| CasLock::Exclusive { _lock: lock })
    };
    let Ok(_guard) = lock_guard else {
        return err_json(500, "internal", "failed to acquire CAS lock");
    };
    let size_bytes = match store.open_object(&digest) {
        Ok(file) => match file.metadata() {
            Ok(meta) => meta.len(),
            Err(e) => return err_json(500, "internal", format!("stat CAS object: {e}")),
        },
        Err(err) if err.is_not_found() => {
            return IpcResponse::json(
                200,
                &json!({
                    "status": "missing",
                    "digest": digest.to_string(),
                }),
            );
        }
        Err(e) => return err_json(400, "bad_request", format!("invalid CAS object: {e}")),
    };

    if action != "stat" {
        if let Err(e) = store.touch(&parsed.namespace, &digest) {
            return err_json(500, "internal", format!("touch CAS object: {e}"));
        }
        if let Some(pin_name) = parsed.pin_ref.as_deref()
            && let Err(e) = write_pin_locked(cas_root, &parsed.owner, pin_name, &digest)
        {
            return err_json(500, "internal", format!("pin CAS object: {e}"));
        }
    }
    IpcResponse::json(
        200,
        &json!({
            "status": "ok",
            "digest": digest.to_string(),
            "size_bytes": size_bytes,
        }),
    )
}

/// Write `<cas>/pins/<owner>/<name>` while already holding `Store::lock_gc()`
/// (calling `Store::pin()` inside `lock_gc()` would re-acquire the same `flock`).
fn write_pin_locked(
    cas_root: &Path,
    owner: &str,
    name: &str,
    digest: &Digest,
) -> std::io::Result<()> {
    let dir = cas_root.join("pins").join(owner);
    fs::create_dir_all(&dir)?;
    let seq = PIN_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".pin-{}-{seq}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    let content = format!("{digest}\n");
    let res = file
        .write_all(content.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, dir.join(name))
}
