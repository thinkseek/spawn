//! Static route table, PostgreSQL role enforcement, and `/ipc/v1/hello` +
//! `/api/*` handlers for `spawn-ipc-v1` (`docs/spawn-ipc-v1.md` §4).

use serde::Deserialize;
use serde_json::{Value, json};
use spawn_core::id::{IdempotencyKey, SessionId};

use super::IpcState;
use super::events::{latest_cursor, serve_api_sse, serve_ndjson_events};
use super::http::{IpcRequest, IpcResponse, write_response};
use super::node::handle_node_route;
use crate::serve::{
    self, BAD_REQUEST, FeatureAssociateRefusal, MAX_INPUT_TEXT, answers, approvals, route,
};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) enum PoolKind {
    Reads,
    Writes,
    Node,
    Events,
}

#[cfg(test)]
impl PoolKind {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Reads => "reads",
            Self::Writes => "writes",
            Self::Node => "node",
            Self::Events => "events",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) enum PgRole {
    Reader,
    Human,
    NodeGateway,
    None,
}

#[cfg(test)]
impl PgRole {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Reader => "spawn_reader",
            Self::Human => "spawn_human",
            Self::NodeGateway => "spawn_node_gateway",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) struct IpcRoute {
    pub(super) method: &'static str,
    pub(super) pattern: &'static str,
    pub(super) pool: PoolKind,
    pub(super) role: PgRole,
    pub(super) idempotency_required: bool,
}

pub(super) const STATIC_IPC_ROUTES: &[IpcRoute] = &[
    IpcRoute {
        method: "GET",
        pattern: "/ipc/v1/hello",
        pool: PoolKind::Reads,
        role: PgRole::Reader,
        idempotency_required: false,
    },
    IpcRoute {
        method: "GET",
        pattern: "/ipc/v1/events",
        pool: PoolKind::Events,
        role: PgRole::Reader,
        idempotency_required: false,
    },
    IpcRoute {
        method: "POST",
        pattern: "/ipc/v1/node/key",
        pool: PoolKind::Node,
        role: PgRole::NodeGateway,
        idempotency_required: true,
    },
    IpcRoute {
        method: "POST",
        pattern: "/ipc/v1/node/enroll",
        pool: PoolKind::Node,
        role: PgRole::NodeGateway,
        idempotency_required: true,
    },
    IpcRoute {
        method: "POST",
        pattern: "/ipc/v1/node/beat",
        pool: PoolKind::Node,
        role: PgRole::NodeGateway,
        idempotency_required: true,
    },
    IpcRoute {
        method: "POST",
        pattern: "/ipc/v1/node/spec",
        pool: PoolKind::Node,
        role: PgRole::NodeGateway,
        idempotency_required: true,
    },
    IpcRoute {
        method: "POST",
        pattern: "/ipc/v1/node/result",
        pool: PoolKind::Node,
        role: PgRole::NodeGateway,
        idempotency_required: true,
    },
    IpcRoute {
        method: "POST",
        pattern: "/ipc/v1/node/cas",
        pool: PoolKind::Node,
        role: PgRole::None,
        idempotency_required: true,
    },
];

#[cfg(test)]
pub(super) fn canonical_ipc_routes() -> Vec<IpcRoute> {
    let mut out = STATIC_IPC_ROUTES.to_vec();
    for &(method, pattern) in crate::serve::ROUTES {
        let (pool, role, idempotency_required) = if method == "GET" {
            (PoolKind::Reads, PgRole::Reader, false)
        } else {
            let role = if matches!(pattern, "/api/features/scan" | "/api/features/associate") {
                PgRole::None
            } else {
                PgRole::Human
            };
            (PoolKind::Writes, role, true)
        };
        out.push(IpcRoute {
            method,
            pattern,
            pool,
            role,
            idempotency_required,
        });
    }
    out
}

pub(super) fn handle_connection_request(
    req: &IpcRequest,
    stream: &mut UnixStream,
    state: &Arc<IpcState>,
    shutdown: &AtomicBool,
) -> std::io::Result<bool> {
    if req.path == "/ipc/v1/events" {
        if req.method != "GET" {
            let resp = IpcResponse::json(405, &json!({ "error": "method not allowed" }))
                .with_header("Allow", "GET")
                .with_close(true);
            write_response(stream, &resp)?;
            return Ok(false);
        }
        let Ok(cloned) = stream.try_clone() else {
            return Ok(false);
        };
        serve_ndjson_events(cloned, &req.query, state, shutdown);
        return Ok(false);
    }

    if req.path == "/api/stream" {
        if req.method != "GET" {
            let resp = IpcResponse::json(405, &json!({ "error": "method not allowed" }))
                .with_header("Allow", "GET")
                .with_close(true);
            write_response(stream, &resp)?;
            return Ok(false);
        }
        let Ok(cloned) = stream.try_clone() else {
            return Ok(false);
        };
        serve_api_sse(
            cloned,
            &req.query,
            req.header("last-event-id"),
            state,
            shutdown,
        );
        return Ok(false);
    }

    let resp = dispatch_buffered(req, state).with_close(req.close);
    write_response(stream, &resp)?;
    Ok(!req.close)
}

fn dispatch_buffered(req: &IpcRequest, state: &IpcState) -> IpcResponse {
    if req.path == "/ipc/v1/hello" {
        if req.method != "GET" {
            return IpcResponse::json(405, &json!({ "error": "method not allowed" }))
                .with_header("Allow", "GET");
        }
        return handle_hello(state);
    }

    if let Some(static_route) = STATIC_IPC_ROUTES
        .iter()
        .find(|r| r.pattern == req.path && r.pattern.starts_with("/ipc/v1/node/"))
    {
        if req.method != static_route.method {
            return IpcResponse::json(405, &json!({ "error": "method not allowed" }))
                .with_header("Allow", static_route.method);
        }
        return handle_node_route(static_route.pattern, req, state);
    }

    if req.path == "/api/features/truth" {
        if !matches!(req.method.as_str(), "GET" | "HEAD") {
            return IpcResponse::json(
                405,
                &json!({ "error": "method not allowed: this path takes GET only" }),
            )
            .with_header("Allow", "GET");
        }
        return handle_api_get("/api/features/truth", req, state);
    }

    let Some((expected_method, pattern, values)) = route(&req.path) else {
        return IpcResponse::json(404, &json!({ "error": "not found", "code": "not_found" }));
    };
    if req.method != expected_method && !(expected_method == "GET" && req.method == "HEAD") {
        return IpcResponse::json(
            405,
            &json!({ "error": format!("method not allowed: this path takes {expected_method} only") }),
        )
        .with_header("Allow", expected_method);
    }
    let id = values.first().copied().unwrap_or_default();
    if expected_method == "POST" {
        return handle_api_post(pattern, id, req, state);
    }
    handle_api_get(pattern, req, state)
}

fn handle_hello(state: &IpcState) -> IpcResponse {
    let Some(_permit) = state.pools.reads_gate.try_acquire() else {
        return IpcResponse::json(
            503,
            &json!({ "error": "busy: reads pool saturated", "code": "busy" }),
        );
    };
    let reader_conn = match state.pools.reader.checkout() {
        Ok(c) => c,
        Err(e) => return IpcResponse::json(500, &json!({ "error": e })),
    };
    let latest_id = match latest_cursor(&reader_conn) {
        Ok((id, _)) => id,
        Err(e) => return IpcResponse::json(500, &json!({ "error": e })),
    };
    IpcResponse::json(
        200,
        &json!({
            "proto": "spawn-ipc-v1",
            "build": env!("CARGO_PKG_VERSION"),
            "schema": 1,
            "epoch": state.epoch,
            "latest_id": latest_id,
        }),
    )
    .with_header("Cache-Control", "no-store")
}

fn handle_api_get(_pattern: &str, req: &IpcRequest, state: &IpcState) -> IpcResponse {
    let Some(_permit) = state.pools.reads_gate.try_acquire() else {
        return IpcResponse::json(
            503,
            &json!({ "error": "busy: reads pool saturated", "code": "busy" }),
        );
    };
    let reader_url = state.urls.reader.as_str();
    let cache_control = req.header("cache-control").map(str::to_string);
    let if_none_match = req.header("if-none-match").map(str::to_string);

    if req.path == "/api/features/coverage" && serve::mission_query(&req.query).is_some() {
        let (status, value) =
            serve::with_reader_url(reader_url, || serve::feature_coverage_for_query(&req.query));
        let mut resp = IpcResponse::json(status, &value).with_header("Cache-Control", "no-cache");
        if req.method == "HEAD" {
            resp.body.clear();
        }
        return resp;
    }

    let answer = if req.path == "/api/features/truth" {
        serve::with_reader_url(reader_url, || {
            serve::feature_map::feature_truth_answer(&req.query)
        })
    } else {
        serve::with_reader_url(reader_url, || {
            let Some(read) = serve::reader(&req.path, &req.query) else {
                return answers::Answer {
                    status: 404,
                    body: json!({ "error": "not found" }).to_string(),
                    etag: None,
                };
            };
            let compute = || answers::answer(&req.path, read(), BAD_REQUEST);
            if answers::shared(&req.path) {
                answers::ANSWERS.get(
                    &answers::key(&req.path, &req.query),
                    answers::accepted_age(cache_control.as_deref()),
                    compute,
                )
            } else {
                std::sync::Arc::new(compute())
            }
            .as_ref()
            .clone()
        })
    };

    if let Some(etag) = answer.etag.as_deref()
        && answers::not_modified(if_none_match.as_deref(), etag)
    {
        return IpcResponse::empty(304)
            .with_header("ETag", etag)
            .with_header("Cache-Control", "no-cache");
    }

    let body = if req.method == "HEAD" {
        Vec::new()
    } else {
        answer.body.into_bytes()
    };
    let mut resp = IpcResponse::text(answer.status, "application/json; charset=utf-8", body)
        .with_header("Cache-Control", "no-cache");
    if let Some(etag) = answer.etag {
        resp = resp.with_header("ETag", etag);
    }
    resp
}

fn handle_api_post(pattern: &str, id: &str, req: &IpcRequest, state: &IpcState) -> IpcResponse {
    let Some(_permit) = state.pools.writes_gate.try_acquire() else {
        return IpcResponse::json(
            503,
            &json!({ "error": "busy: writes pool saturated", "code": "busy" }),
        );
    };
    if pattern == "/api/features/scan" {
        let (status, value) = serve::with_reader_url(&state.urls.reader, || {
            serve::feature_coverage_for_query(&req.query)
        });
        return IpcResponse::json(status, &value);
    }
    if pattern == "/api/features/associate" {
        let (status, value) = match serve::with_reader_url(&state.urls.reader, || {
            serve::feature_paths(&req.query)
        }) {
            Ok((root, registry)) => {
                match serve::feature_associate_at(&root, &registry, &req.body) {
                    Ok(ans) | Err(FeatureAssociateRefusal::Structured(ans)) => ans,
                    Err(FeatureAssociateRefusal::Plain((st, msg))) => (st, json!({ "error": msg })),
                }
            }
            Err(refusal) => refusal,
        };
        return IpcResponse::json(status, &value);
    }

    let open_human = || state.pools.human.checkout().map(serve::WriterConn::Pooled);
    let header_key = req.idempotency_key.clone();
    let result = match pattern {
        "/api/sessions/<id>/input" => {
            serve::apply_input_body(&req.body, id, header_key, true, &open_human)
        }
        "/api/sessions/<id>/key" => serve::apply_key_body(&req.body, id, header_key, &open_human),
        "/api/sessions/<id>/restart" => {
            serve::apply_move_body(&req.body, id, "restart", header_key, &open_human)
        }
        "/api/sessions/<id>/reassign" => {
            serve::apply_move_body(&req.body, id, "reassign", header_key, &open_human)
        }
        "/api/missions/<id>/resume" => {
            serve::apply_resume_body(&req.body, id, header_key, true, &open_human)
        }
        "/api/master/message" => serve::apply_master_body(&req.body, header_key, true, &open_human),
        "/api/approvals/<session_id>/act" => apply_approval_act(&req.body, id, header_key, state),
        _ => Err((404, "not found".to_string())),
    };
    match result {
        Ok((status, body)) => IpcResponse::json(status, &body),
        Err((status, msg)) => IpcResponse::json(status, &json!({ "error": msg })),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IpcActBody {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    approval_id: Option<String>,
    #[serde(default)]
    choice_id: Option<String>,
    #[serde(default)]
    keys: Option<Vec<String>>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    probe: bool,
}

fn apply_approval_act(
    raw_body: &[u8],
    id: &str,
    header_key: Option<String>,
    state: &IpcState,
) -> Result<(u16, Value), (u16, String)> {
    let body: IpcActBody = serde_json::from_slice(raw_body)
        .map_err(|error| (400, format!("{BAD_REQUEST}the body is not an act: {error}")))?;
    let session = serve::session_id(id).map_err(|error| (400, error))?;
    if body.probe {
        return probe_approval_act(&body, &session, state);
    }
    let key = serve::idempotency(body.key, header_key, "an act")?;
    if key.as_str().len() > spawn_postgres::MAX_MASTER_KEY_BYTES {
        return Err((
            400,
            format!(
                "{BAD_REQUEST}an act's idempotency key is at most {} bytes",
                spawn_postgres::MAX_MASTER_KEY_BYTES
            ),
        ));
    }
    let action = body
        .action
        .ok_or_else(|| (400, format!("{BAD_REQUEST}an act names its action")))?;
    let sending = if body.approval_id.is_some()
        || matches!(action.as_str(), "allow" | "allow_always" | "deny")
    {
        resolve_prompt_act(
            &session,
            &action,
            body.approval_id.as_deref(),
            body.choice_id.as_deref(),
            body.keys.as_deref(),
            body.text.as_deref(),
            state,
        )?
    } else {
        let asked = approvals::Asked {
            action: action.clone(),
            approval_id: None,
            choice_id: body.choice_id,
            keys: body.keys,
            text: body.text,
        };
        approvals::what_to_send(asked, &session, &|_| Ok(None))?
    };
    submit_approval_sending(&session, &action, &key, sending, state)
}

fn probe_approval_act(
    body: &IpcActBody,
    session: &SessionId,
    state: &IpcState,
) -> Result<(u16, Value), (u16, String)> {
    if body.action.is_some()
        || body.approval_id.is_some()
        || body.choice_id.is_some()
        || body.keys.is_some()
        || body.text.is_some()
        || body.key.is_some()
    {
        return Err((
            400,
            format!("{BAD_REQUEST}a probe carries no action, approval, choice, keys, text or key"),
        ));
    }
    let _ = state.pools.human.checkout().map_err(|e| (500, e))?;
    let keys: Vec<_> = spawn_core::command::NamedKey::ALL
        .iter()
        .copied()
        .map(spawn_core::command::NamedKey::name)
        .collect();
    Ok((
        200,
        json!({
            "writable": true,
            "session_id": session.as_str(),
            "actions": ["allow", "allow_always", "deny", "keys", "text"],
            "keys": keys,
            "max_keys": 8,
            "max_text_bytes": MAX_INPUT_TEXT,
        }),
    ))
}

fn submit_approval_sending(
    session: &SessionId,
    action: &str,
    key: &IdempotencyKey,
    sending: approvals::Send,
    state: &IpcState,
) -> Result<(u16, Value), (u16, String)> {
    let human_conn = state.pools.human.checkout().map_err(|e| (500, e))?;
    let no_agent = || {
        (
            404,
            format!(
                "no agent has used {}: there is nothing to answer",
                session.as_str()
            ),
        )
    };
    let mut ids = Vec::new();
    match &sending {
        approvals::Send::Keys(keys) => {
            for (at, name) in keys.iter().enumerate() {
                let derived = IdempotencyKey::new(format!("{}:{}", key.as_str(), at + 1))
                    .map_err(|error| (400, format!("{BAD_REQUEST}the key: {error}")))?;
                match human_conn.submit_human_key(session, &derived, *name) {
                    Ok(spawn_postgres::HumanKey::Submitted(cmd)) => ids.push(cmd.0),
                    Ok(spawn_postgres::HumanKey::NoAgent) => return Err(no_agent()),
                    Ok(
                        spawn_postgres::HumanKey::NotRunning(why)
                        | spawn_postgres::HumanKey::KeyUsed(why),
                    ) => return Err((409, why)),
                    Err(error) => return Err((500, error.to_string())),
                }
            }
        }
        approvals::Send::Text(text) => match human_conn.submit_human_input(session, key, text) {
            Ok(spawn_postgres::HumanInput::Submitted(cmd)) => ids.push(cmd.0),
            Ok(spawn_postgres::HumanInput::NoAgent) => return Err(no_agent()),
            Ok(
                spawn_postgres::HumanInput::NotRunning(why)
                | spawn_postgres::HumanInput::KeyUsed(why),
            ) => return Err((409, why)),
            Err(error) => return Err((500, error.to_string())),
        },
    }
    let (keys_json, text_json) = match sending {
        approvals::Send::Keys(keys) => (
            json!(keys.iter().map(|k| k.name()).collect::<Vec<_>>()),
            Value::Null,
        ),
        approvals::Send::Text(text) => (json!([]), json!(text)),
    };
    Ok((
        202,
        json!({
            "session_id": session.as_str(),
            "action": action,
            "command_ids": ids,
            "keys": keys_json,
            "text": text_json,
            "principal": "human",
        }),
    ))
}

fn parse_named_keys(names: &[String]) -> Result<Vec<spawn_core::command::NamedKey>, (u16, String)> {
    names
        .iter()
        .map(|n| {
            spawn_core::command::NamedKey::from_name(n)
                .map_err(|e| (400, format!("{BAD_REQUEST}{e}")))
        })
        .collect()
}

fn parse_option_keys(opt: &Value) -> Result<Vec<spawn_core::command::NamedKey>, (u16, String)> {
    let names: Vec<String> = opt["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    parse_named_keys(&names)
}

fn resolve_prompt_act(
    session: &SessionId,
    action: &str,
    approval_id: Option<&str>,
    choice_id: Option<&str>,
    keys: Option<&[String]>,
    text: Option<&str>,
    state: &IpcState,
) -> Result<approvals::Send, (u16, String)> {
    if action != "keys" && (keys.is_some() || text.is_some()) {
        return Err((
            400,
            format!("{BAD_REQUEST}{action} carries no keys and no text"),
        ));
    }
    let wanted = approval_id.ok_or_else(|| {
        (
            400,
            format!(
                "{BAD_REQUEST}{action} names the approval it answers (\"approval_id\", the item's id from GET /api/approvals)"
            ),
        )
    })?;
    let payload =
        serve::with_reader_url(&state.urls.reader, approvals::approvals).map_err(|e| (500, e))?;
    let item = payload["items"]
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|it| it["session_id"].as_str() == Some(session.as_str()))
        })
        .ok_or_else(|| {
            (
                409,
                format!(
                    "no approval prompt shows on {} now: nothing to {action}",
                    session.as_str()
                ),
            )
        })?;
    let shown_id = item["id"].as_str().unwrap_or_default();
    if shown_id != wanted {
        return Err((
            409,
            format!(
                "the prompt on {} is not approval {wanted} any more (now {shown_id}): nothing pressed; read GET /api/approvals again",
                session.as_str()
            ),
        ));
    }
    let options = item["options"].as_array().cloned().unwrap_or_default();
    if let Some(cid) = choice_id {
        return resolve_prompt_choice(session, action, cid, keys, &options);
    }
    if action == "keys" {
        let asked_keys =
            keys.ok_or_else(|| (400, format!("{BAD_REQUEST}keys names 1 to 8 keys")))?;
        if asked_keys.is_empty() || asked_keys.len() > 8 {
            return Err((400, format!("{BAD_REQUEST}keys names 1 to 8 keys")));
        }
        return Ok(approvals::Send::Keys(parse_named_keys(asked_keys)?));
    }
    let matching: Vec<_> = options
        .iter()
        .filter(|opt| opt["action"].as_str() == Some(action))
        .collect();
    if matching.is_empty() {
        return Err((
            409,
            format!(
                "the prompt on {} offers no {action} answer",
                session.as_str()
            ),
        ));
    }
    if matching.len() > 1 {
        return Err((
            409,
            format!(
                "the prompt on {} offers more than one {action} answer: name the one meant (\"choice_id\", the option's id from GET /api/approvals); nothing pressed",
                session.as_str()
            ),
        ));
    }
    Ok(approvals::Send::Keys(parse_option_keys(matching[0])?))
}

fn resolve_prompt_choice(
    session: &SessionId,
    action: &str,
    cid: &str,
    keys: Option<&[String]>,
    options: &[Value],
) -> Result<approvals::Send, (u16, String)> {
    let matched = options
        .iter()
        .find(|opt| {
            opt["id"].as_str() == Some(cid)
                && (action == "keys" || opt["action"].as_str() == Some(action))
        })
        .ok_or_else(|| {
            (
                409,
                format!(
                    "the prompt on {} offers no answer {cid:?} for {action}: nothing pressed; read GET /api/approvals again",
                    session.as_str()
                ),
            )
        })?;
    let parsed = parse_option_keys(matched)?;
    if let Some(given_keys) = keys
        && parse_named_keys(given_keys)? != parsed
    {
        return Err((
            409,
            format!(
                "answer {cid:?} on {} is not pressed with those keys any more: nothing pressed; read GET /api/approvals again",
                session.as_str()
            ),
        ));
    }
    Ok(approvals::Send::Keys(parsed))
}

#[cfg(test)]
mod truth_tests {
    use std::path::PathBuf;

    use serde_json::Value;

    use super::*;
    use crate::serve::feature_map::with_test_default_project;
    use crate::serve::ipc::RoleUrls;
    use crate::serve::ipc::http::IpcRequest;

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("spawn-ipc-truth-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn setup_fixture_repo(root: &std::path::Path) {
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("docs/feature-map")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod a;\npub mod b;\n").unwrap();
        std::fs::write(
            root.join("src/a.rs"),
            "use crate::b::val;\npub fn f() -> u32 { val() }\n",
        )
        .unwrap();
        std::fs::write(root.join("src/b.rs"), "pub fn val() -> u32 { 1 }\n").unwrap();
        let registry = json!({
            "schema_version": 1,
            "initiative_id": "fx",
            "boundary": {
                "project_name": "fixture-ipc-truth",
                "description": "test fixture",
                "included_categories": ["tracked-source"],
                "exclusions": []
            },
            "features": [
                {
                    "id": "cap-a",
                    "title": "Feature A",
                    "kind": "capability",
                    "summary": "A",
                    "files": ["src/a.rs"],
                    "relations": [],
                    "provenance": {}
                },
                {
                    "id": "cap-b",
                    "title": "Feature B",
                    "kind": "capability",
                    "summary": "B",
                    "files": ["Cargo.toml", "src/b.rs", "src/lib.rs"],
                    "relations": [],
                    "provenance": {}
                }
            ]
        });
        std::fs::write(
            root.join("docs/feature-map/registry.json"),
            serde_json::to_string_pretty(&registry).unwrap(),
        )
        .unwrap();
    }

    fn ipc_request(
        method: &str,
        path: &str,
        query: &str,
        headers: Vec<(&str, &str)>,
    ) -> IpcRequest {
        IpcRequest {
            method: method.to_string(),
            path: path.to_string(),
            query: query.to_string(),
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
                .collect(),
            idempotency_key: None,
            body: Vec::new(),
            close: false,
        }
    }

    #[test]
    fn truth_route_dispatches_via_reads_pool_supports_etag_304_head_and_rejects_post() {
        let dir = ScratchDir::new("dispatch");
        setup_fixture_repo(&dir.0);
        let registry_file = dir.0.join("docs/feature-map/registry.json");

        with_test_default_project(&dir.0, &registry_file, || {
            // Bind a strictly read-only role URL and unused writes/node URLs.
            let state = IpcState::new(
                RoleUrls::from_explicit(
                    "postgres://spawn_reader@127.0.0.1:1/spawn".to_string(),
                    "postgres://spawn_human@127.0.0.1:1/spawn".to_string(),
                    "postgres://spawn_node_gateway@127.0.0.1:1/spawn".to_string(),
                ),
                dir.0.join("cas"),
                None,
            );

            let resp = dispatch_buffered(
                &ipc_request("GET", "/api/features/truth", "", vec![]),
                &state,
            );
            assert_eq!(resp.status, 200);
            let etag = resp
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("etag"))
                .map(|(_, v)| v.clone())
                .expect("weak ETag header");
            assert!(etag.starts_with("W/\""), "expected weak ETag, got {etag}");
            let body: Value = serde_json::from_slice(&resp.body).unwrap();
            assert_eq!(body["project"], "fixture-ipc-truth");
            assert_eq!(body["counts"]["under_declared"], 1);
            assert_eq!(body["counts"]["enforced_violations"], 1);
            assert_eq!(body["purity"]["clean"], true);

            // Conditional request returns 304 Not Modified with empty body.
            let not_mod = dispatch_buffered(
                &ipc_request(
                    "GET",
                    "/api/features/truth",
                    "",
                    vec![("If-None-Match", &etag)],
                ),
                &state,
            );
            assert_eq!(not_mod.status, 304);
            assert!(not_mod.body.is_empty());

            // HEAD returns 200 with ETag and empty body.
            let head_resp = dispatch_buffered(
                &ipc_request("HEAD", "/api/features/truth", "", vec![]),
                &state,
            );
            assert_eq!(head_resp.status, 200);
            assert!(head_resp.body.is_empty());

            // POST is rejected with 405 Allow: GET.
            let post_resp = dispatch_buffered(
                &ipc_request("POST", "/api/features/truth", "", vec![]),
                &state,
            );
            assert_eq!(post_resp.status, 405);

            // Exhausting the reads pool saturates GET /api/features/truth (proving reads pool binding).
            let mut permits = Vec::new();
            for _ in 0..super::super::READS_POOL_SIZE {
                permits.push(state.pools.reads_gate.try_acquire().unwrap());
            }
            let busy = dispatch_buffered(
                &ipc_request("GET", "/api/features/truth", "", vec![]),
                &state,
            );
            assert_eq!(busy.status, 503);
            drop(permits);
        });
    }
}
