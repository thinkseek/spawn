//! Static route table, PostgreSQL role enforcement, and `/ipc/v1/hello` +
//! `/api/*` handlers for `spawn-ipc-v1` (`docs/spawn-ipc-v1.md` §4).

use serde::Deserialize;
use serde_json::{Value, json};
use spawn_core::id::{IdempotencyKey, SessionId};

use super::IpcState;
use super::PoolError;
use super::events::{latest_cursor, serve_api_sse, serve_ndjson_events};
use super::http::{IpcRequest, IpcResponse, write_response};
use super::node::handle_node_route;
use crate::serve::{self, BAD_REQUEST, MAX_INPUT_TEXT, answers, approvals, route};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
pub(super) struct IpcRoute {
    pub(super) method: &'static str,
    pub(super) pattern: &'static str,
    #[cfg(test)]
    pub(super) pool: PoolKind,
    #[cfg(test)]
    pub(super) role: PgRole,
    #[cfg(test)]
    pub(super) idempotency_required: bool,
}

pub(super) const STATIC_IPC_ROUTES: &[IpcRoute] = &[
    IpcRoute {
        method: "GET",
        pattern: "/ipc/v1/hello",
        #[cfg(test)]
        pool: PoolKind::Reads,
        #[cfg(test)]
        role: PgRole::Reader,
        #[cfg(test)]
        idempotency_required: false,
    },
    IpcRoute {
        method: "GET",
        pattern: "/ipc/v1/events",
        #[cfg(test)]
        pool: PoolKind::Events,
        #[cfg(test)]
        role: PgRole::Reader,
        #[cfg(test)]
        idempotency_required: false,
    },
    IpcRoute {
        method: "POST",
        pattern: "/ipc/v1/node/key",
        #[cfg(test)]
        pool: PoolKind::Node,
        #[cfg(test)]
        role: PgRole::NodeGateway,
        #[cfg(test)]
        idempotency_required: true,
    },
    IpcRoute {
        method: "POST",
        pattern: "/ipc/v1/node/enroll",
        #[cfg(test)]
        pool: PoolKind::Node,
        #[cfg(test)]
        role: PgRole::NodeGateway,
        #[cfg(test)]
        idempotency_required: true,
    },
    IpcRoute {
        method: "POST",
        pattern: "/ipc/v1/node/beat",
        #[cfg(test)]
        pool: PoolKind::Node,
        #[cfg(test)]
        role: PgRole::NodeGateway,
        #[cfg(test)]
        idempotency_required: true,
    },
    IpcRoute {
        method: "POST",
        pattern: "/ipc/v1/node/spec",
        #[cfg(test)]
        pool: PoolKind::Node,
        #[cfg(test)]
        role: PgRole::NodeGateway,
        #[cfg(test)]
        idempotency_required: true,
    },
    IpcRoute {
        method: "POST",
        pattern: "/ipc/v1/node/result",
        #[cfg(test)]
        pool: PoolKind::Node,
        #[cfg(test)]
        role: PgRole::NodeGateway,
        #[cfg(test)]
        idempotency_required: true,
    },
    IpcRoute {
        method: "POST",
        pattern: "/ipc/v1/node/cas",
        #[cfg(test)]
        pool: PoolKind::Node,
        #[cfg(test)]
        role: PgRole::None,
        #[cfg(test)]
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

    if req.path == "/api/episodes" {
        if !matches!(req.method.as_str(), "GET" | "HEAD") {
            return IpcResponse::json(
                405,
                &json!({ "error": "method not allowed: this path takes GET only" }),
            )
            .with_header("Allow", "GET");
        }
        return handle_api_get("/api/episodes", req, state);
    }

    if let Some(agent_route) = serve::parse_agent_route(&req.path) {
        return handle_agent_ipc_route(agent_route, req, state);
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

fn ipc_agent_principal(req: &IpcRequest) -> serve::auth_token::AuthPrincipal {
    if let Some(role) = req.header("x-spawn-role")
        && !matches!(role.trim().to_ascii_lowercase().as_str(), "owner" | "admin")
    {
        return serve::auth_token::AuthPrincipal::ReadOnlyViewer {
            expires_at_ms: i64::MAX,
        };
    }
    if req
        .header("x-spawn-viewer")
        .is_some_and(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "false" | "0"))
        || req
            .header("authorization")
            .is_some_and(|v| v.contains("sv1."))
    {
        return serve::auth_token::AuthPrincipal::ReadOnlyViewer {
            expires_at_ms: i64::MAX,
        };
    }
    serve::auth_token::AuthPrincipal::LocalOwner
}

fn ipc_agent_answer_response(
    req: &IpcRequest,
    res: Result<answers::Answer, (u16, String)>,
) -> IpcResponse {
    match res {
        Ok(answer) => {
            if let Some(etag) = answer.etag.as_deref()
                && answers::not_modified(req.header("if-none-match"), etag)
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
            let mut resp =
                IpcResponse::text(answer.status, "application/json; charset=utf-8", body)
                    .with_header("Cache-Control", "no-cache");
            if let Some(etag) = answer.etag {
                resp = resp.with_header("ETag", etag);
            }
            resp
        }
        Err((status, message)) => {
            let mut resp = IpcResponse::json(status, &json!({ "error": message }));
            if req.method == "HEAD" {
                resp.body.clear();
            }
            resp
        }
    }
}

fn handle_agent_ipc_route(
    agent_route: serve::AgentRoute,
    req: &IpcRequest,
    state: &IpcState,
) -> IpcResponse {
    let access = ipc_agent_principal(req);
    if !access.can_mutate() && !matches!(req.method.as_str(), "GET" | "HEAD") {
        return IpcResponse::json(
            403,
            &json!({ "error": serve::auth_token::READONLY_MUTATION_ERROR }),
        );
    }
    let discovery_env = crate::config::herdr_discovery_env();
    let agent_socket = state.agent_socket.as_deref();
    match agent_route {
        serve::AgentRoute::InvalidAddress => IpcResponse::json(
            400,
            &json!({ "error": format!("{BAD_REQUEST}invalid agent terminal address") }),
        ),
        serve::AgentRoute::List => {
            if !matches!(req.method.as_str(), "GET" | "HEAD") {
                return IpcResponse::json(
                    405,
                    &json!({ "error": "method not allowed: this path takes GET only" }),
                )
                .with_header("Allow", "GET");
            }
            ipc_agent_answer_response(
                req,
                serve::agent_list_answer(&req.query, access, agent_socket, &discovery_env),
            )
        }
        serve::AgentRoute::Snapshot { address } => {
            if !matches!(req.method.as_str(), "GET" | "HEAD") {
                return IpcResponse::json(
                    405,
                    &json!({ "error": "method not allowed: this path takes GET only" }),
                )
                .with_header("Allow", "GET");
            }
            ipc_agent_answer_response(
                req,
                serve::agent_snapshot_answer(&address, access, agent_socket, &discovery_env),
            )
        }
        serve::AgentRoute::Events { address } => {
            if !matches!(req.method.as_str(), "GET" | "HEAD") {
                return IpcResponse::json(
                    405,
                    &json!({ "error": "method not allowed: this path takes GET only" }),
                )
                .with_header("Allow", "GET");
            }
            ipc_agent_answer_response(
                req,
                serve::agent_events_answer(
                    &address,
                    &req.query,
                    access,
                    agent_socket,
                    &discovery_env,
                ),
            )
        }
        serve::AgentRoute::Action { address } => {
            if req.method != "POST" {
                return IpcResponse::json(
                    405,
                    &json!({ "error": "method not allowed: this path takes POST only" }),
                )
                .with_header("Allow", "POST");
            }
            if !access.can_mutate() {
                return IpcResponse::json(
                    403,
                    &json!({ "error": serve::auth_token::READONLY_MUTATION_ERROR }),
                );
            }
            if req.body.len() > 16 * 1024 {
                return IpcResponse::json(413, &json!({ "error": "body exceeds 16384 bytes" }));
            }
            match serve::agent_action_checked_bytes(
                &req.body,
                &address,
                agent_socket,
                &discovery_env,
            ) {
                Ok((status, body)) => IpcResponse::json(status, &body),
                Err((status, message)) => IpcResponse::json(status, &json!({ "error": message })),
            }
        }
    }
}

fn handle_hello(state: &IpcState) -> IpcResponse {
    let (latest_id, _) = match super::with_read(&state.pools.reader, latest_cursor) {
        Ok(cur) => cur,
        Err((status, msg)) => return IpcResponse::json(status, &json!({ "error": msg })),
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
    let cache_control = req.header("cache-control").map(str::to_string);
    let if_none_match = req.header("if-none-match").map(str::to_string);

    if req.path == "/api/features/coverage" && serve::mission_query(&req.query).is_some() {
        let (status, value) = serve::run_with_reader_pool_status(
            &state.pools.reader,
            true,
            || serve::feature_coverage_for_query(&req.query),
            |(st, _)| *st == 500,
            |err| {
                let (st, msg) = err.status_and_message();
                (st, json!({ "error": msg }))
            },
        );
        let mut resp = IpcResponse::json(status, &value).with_header("Cache-Control", "no-cache");
        if req.method == "HEAD" {
            resp.body.clear();
        }
        return resp;
    }

    let answer = serve::run_with_reader_pool_status(
        &state.pools.reader,
        true,
        || {
            if req.path == "/api/features/truth" {
                serve::feature_map::feature_truth_answer(&req.query)
            } else if req.path == "/api/episodes" {
                serve::episodes_answer(&req.query)
            } else {
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
            }
        },
        |ans| ans.status == 500,
        |err| {
            let (status, msg) = err.status_and_message();
            answers::Answer {
                status,
                body: json!({ "error": msg }).to_string(),
                etag: None,
            }
        },
    );

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
    if pattern == "/api/features/scan" {
        let (status, value) = serve::run_with_reader_pool_status(
            &state.pools.reader,
            false,
            || serve::feature_scan_for_query(&req.query),
            |(st, _)| *st == 500,
            |err| {
                let (st, msg) = err.status_and_message();
                (st, json!({ "error": msg }))
            },
        );
        return IpcResponse::json(status, &value);
    }
    if pattern == "/api/features/associate" {
        let (status, value) = serve::run_with_reader_pool_status(
            &state.pools.reader,
            false,
            || serve::feature_associate_for_query(&req.query, &req.body),
            |(st, _)| *st == 500,
            |err| {
                let (st, msg) = err.status_and_message();
                (st, json!({ "error": msg }))
            },
        );
        return IpcResponse::json(status, &value);
    }

    let human = state.pools.human_pool();
    let header_key = req.idempotency_key.clone();
    let result = match pattern {
        "/api/sessions/<id>/input" => {
            serve::apply_input_body(&req.body, id, header_key, true, human)
        }
        "/api/sessions/<id>/key" => serve::apply_key_body(&req.body, id, header_key, human),
        "/api/sessions/<id>/restart" => {
            serve::apply_move_body(&req.body, id, "restart", header_key, human)
        }
        "/api/sessions/<id>/reassign" => {
            serve::apply_move_body(&req.body, id, "reassign", header_key, human)
        }
        "/api/missions/<id>/resume" => {
            serve::apply_resume_body(&req.body, id, header_key, true, human)
        }
        "/api/master/message" => serve::apply_master_body(&req.body, header_key, true, human),
        "/api/approvals/<session_id>/act" => apply_approval_act(&req.body, id, header_key, state),
        _ => Err((404, "not found".to_string())),
    };
    match result {
        Ok((status, body)) => IpcResponse::json(status, &body),
        Err((status, msg)) => IpcResponse::json(status, &json!({ "error": msg })),
    }
}

#[derive(Debug, Deserialize)]
struct IpcPromptOption {
    id: String,
    action: approvals::ChoiceAction,
    #[serde(default)]
    keys: Vec<String>,
}

impl IpcPromptOption {
    fn into_choice(self) -> Result<approvals::Choice, (u16, String)> {
        Ok(approvals::Choice {
            id: self.id,
            label: String::new(),
            keys: approvals::parse_option_keys(&self.keys)?,
            danger: false,
            action: self.action,
        })
    }
}

fn apply_approval_act(
    raw_body: &[u8],
    id: &str,
    header_key: Option<String>,
    state: &IpcState,
) -> Result<(u16, Value), (u16, String)> {
    let req = approvals::ActRequest::parse(raw_body)?;
    let session = serve::session_id(id).map_err(|error| (400, error))?;
    let (body_key, act) = match req {
        approvals::ActRequest::Probe => {
            if header_key.is_some() {
                return Err((
                    400,
                    format!("{BAD_REQUEST}{}", approvals::PROBE_EXTRA_FIELDS),
                ));
            }
            let _ = state
                .pools
                .human_pool()
                .checkout_verified()
                .map_err(PoolError::status_and_message)?;
            return Ok((200, approvals::probe_response(&session, MAX_INPUT_TEXT)));
        }
        approvals::ActRequest::Perform { key, act } => (key, act),
    };
    let key = serve::idempotency(body_key, header_key, "an act")?;
    if key.as_str().len() > spawn_postgres::MAX_MASTER_KEY_BYTES {
        return Err((
            400,
            format!(
                "{BAD_REQUEST}an act's idempotency key is at most {} bytes",
                spawn_postgres::MAX_MASTER_KEY_BYTES
            ),
        ));
    }
    let action = act.action_name();
    let sending = resolve_ipc_act(act, &session, state)?;
    submit_approval_sending(&session, action, &key, sending, state)
}

fn submit_approval_sending(
    session: &SessionId,
    action: &'static str,
    key: &IdempotencyKey,
    sending: approvals::Send,
    state: &IpcState,
) -> Result<(u16, Value), (u16, String)> {
    let no_agent = || {
        (
            404,
            format!(
                "no agent has used {}: there is nothing to answer",
                session.as_str()
            ),
        )
    };
    let ids = match &sending {
        approvals::Send::Keys(keys) => {
            let human_conn = state
                .pools
                .human_pool()
                .checkout_verified()
                .map_err(PoolError::status_and_message)?;
            let mut ids = Vec::new();
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
                    Err(error) => {
                        human_conn.mark_broken();
                        return Err((500, error.to_string()));
                    }
                }
            }
            ids
        }
        approvals::Send::Text(text) => {
            let outcome = super::with_write(state.pools.human_pool(), |human_conn| {
                human_conn.submit_human_input(session, key, text)
            })?;
            match outcome {
                spawn_postgres::HumanInput::Submitted(cmd) => vec![cmd.0],
                spawn_postgres::HumanInput::NoAgent => return Err(no_agent()),
                spawn_postgres::HumanInput::NotRunning(why)
                | spawn_postgres::HumanInput::KeyUsed(why) => return Err((409, why)),
            }
        }
    };
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

fn load_ipc_prompt_choices(
    session: &SessionId,
    wanted: &str,
    doing: &str,
    state: &IpcState,
) -> Result<Vec<approvals::Choice>, (u16, String)> {
    let payload = serve::run_with_reader_pool_status(
        &state.pools.reader,
        false,
        || approvals::approvals().map_err(|e| (500, e)),
        |res| matches!(res, Err((500, _))),
        |err| Err(err.status_and_message()),
    )?;
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
                    "no approval prompt shows on {} now: nothing to {doing}",
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
    item["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|opt| serde_json::from_value::<IpcPromptOption>(opt.clone()).ok())
        .map(IpcPromptOption::into_choice)
        .collect()
}

fn resolve_ipc_act(
    act: approvals::Act,
    session: &SessionId,
    state: &IpcState,
) -> Result<approvals::Send, (u16, String)> {
    let answer_prompt = |action: approvals::ChoiceAction, answer: approvals::Answer| {
        let choices =
            load_ipc_prompt_choices(session, &answer.approval_id, action.as_str(), state)?;
        approvals::answer_keys(&choices, session, action, answer.choice_id.as_deref())
            .map(approvals::Send::Keys)
    };
    match act {
        approvals::Act::Allow(answer) => answer_prompt(approvals::ChoiceAction::Allow, answer),
        approvals::Act::AllowAlways(answer) => {
            answer_prompt(approvals::ChoiceAction::AllowAlways, answer)
        }
        approvals::Act::Deny(answer) => answer_prompt(approvals::ChoiceAction::Deny, answer),
        approvals::Act::Keys(keys) => match keys {
            approvals::KeysAct::Bare { keys } => Ok(approvals::Send::Keys(keys)),
            approvals::KeysAct::ForApproval { approval_id, keys } => {
                let _ = load_ipc_prompt_choices(session, &approval_id, "press", state)?;
                Ok(approvals::Send::Keys(keys))
            }
            approvals::KeysAct::ForChoice {
                approval_id,
                choice_id,
                expected_keys,
            } => {
                let choices = load_ipc_prompt_choices(session, &approval_id, "press", state)?;
                approvals::choice_keys_checked(
                    &choices,
                    session,
                    &choice_id,
                    expected_keys.as_deref(),
                )
                .map(approvals::Send::Keys)
            }
        },
        text_act @ approvals::Act::Text { .. } => {
            approvals::what_to_send(text_act, session, &|_| Ok(None))
        }
    }
}

#[cfg(test)]
pub(in crate::serve) fn dispatch_ipc_with_pools_for_test(
    pools: Arc<super::ServePools>,
    method: &str,
    path: &str,
    query: &str,
    body: Vec<u8>,
) -> (u16, Value) {
    let state = IpcState {
        pools,
        cas_root: std::env::temp_dir(),
        epoch: "test-epoch".to_string(),
        agent_socket: None,
    };
    let req = IpcRequest {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        headers: std::collections::BTreeMap::new(),
        idempotency_key: None,
        body,
        close: false,
    };
    let resp = dispatch_buffered(&req, &state);
    let parsed = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
    (resp.status, parsed)
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
            let state = IpcState::with_test_wait_timeout(
                RoleUrls::from_explicit(
                    "postgres://spawn_reader@127.0.0.1:1/spawn".to_string(),
                    "postgres://spawn_human@127.0.0.1:1/spawn".to_string(),
                    "postgres://spawn_node_gateway@127.0.0.1:1/spawn".to_string(),
                ),
                dir.0.join("cas"),
                None,
                std::time::Duration::ZERO,
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
                permits.push(state.pools.reader.try_acquire_slot().unwrap());
            }
            let busy = dispatch_buffered(
                &ipc_request("GET", "/api/features/truth", "", vec![]),
                &state,
            );
            assert_eq!(busy.status, 503);
            drop(permits);
        });
    }

    #[test]
    fn episodes_route_rejects_post_with_405_saturates_reads_pool_with_503_and_validates_query() {
        let dir = ScratchDir::new("episodes-ipc");
        let state = IpcState::with_test_wait_timeout(
            RoleUrls::from_explicit(
                "postgres://spawn_reader@127.0.0.1:1/spawn".to_string(),
                "postgres://spawn_human@127.0.0.1:1/spawn".to_string(),
                "postgres://spawn_node_gateway@127.0.0.1:1/spawn".to_string(),
            ),
            dir.0.join("cas"),
            None,
            std::time::Duration::ZERO,
        );

        let post_resp =
            dispatch_buffered(&ipc_request("POST", "/api/episodes", "", vec![]), &state);
        assert_eq!(post_resp.status, 405);
        assert!(
            post_resp
                .headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("allow") && v == "GET"),
            "{:?}",
            post_resp.headers
        );

        let mut permits = Vec::new();
        for _ in 0..super::super::READS_POOL_SIZE {
            permits.push(state.pools.reader.try_acquire_slot().unwrap());
        }
        let busy = dispatch_buffered(&ipc_request("GET", "/api/episodes", "", vec![]), &state);
        assert_eq!(busy.status, 503);
        drop(permits);

        for bad in ["mission=abc", "limit=abc", "limit=-1", "limit=", "limit"] {
            let bad_query =
                dispatch_buffered(&ipc_request("GET", "/api/episodes", bad, vec![]), &state);
            assert_eq!(bad_query.status, 400, "{bad}");
        }

        for bad in ["mission=", "limit=abc"] {
            let bad_head =
                dispatch_buffered(&ipc_request("HEAD", "/api/episodes", bad, vec![]), &state);
            assert_eq!(bad_head.status, 400, "{bad}");
            assert!(bad_head.body.is_empty(), "{bad}");
        }
    }

    #[test]
    fn episodes_answer_emits_weak_etag_and_matches_if_none_match() {
        let summary = json!({
            "total": 0,
            "landed": 0,
            "accepted": 0,
            "rejected": 0,
            "cancelled": 0,
            "aborted": 0,
            "over_budget": 0,
            "in_flight": 0,
            "total_rework": 0,
            "total_reroutes": 0,
            "review_sync_points": 0,
            "gate_verdicts": 0,
            "warnings": 0,
        });
        let payload = serve::episodes_payload(Some(4), &[], &summary, true, &[]);
        let answer = answers::answer("/api/episodes", Ok(payload), BAD_REQUEST);
        assert_eq!(answer.status, 200);
        let etag = answer.etag.expect("weak ETag");
        assert!(etag.starts_with("W/\""), "expected weak ETag, got {etag}");
        assert!(answers::not_modified(Some(&etag), &etag));
        assert!(!answers::not_modified(Some("W/\"different\""), &etag));
    }

    #[test]
    fn precomputed_snapshot_ipc_serves_get_coverage_and_refuses_scan_and_associate() {
        use crate::config::FeatureCoverageSource;
        use spawn_core::feature_coverage::{
            BoundaryPolicy, FeatureKind, FeatureNode, FeatureProvenance, FeatureRegistry,
            SCHEMA_VERSION,
        };

        let dir = ScratchDir::new("snapshot-ipc");
        let repo_root = dir.0.join("repo");
        std::fs::create_dir_all(repo_root.join("src")).unwrap();
        std::fs::write(repo_root.join("src/lib.rs"), "pub fn ipc_ok() {}\n").unwrap();
        let reg_path = repo_root.join("docs/feature-map/registry.json");
        let reg = FeatureRegistry {
            schema_version: SCHEMA_VERSION,
            initiative_id: "t559u".to_string(),
            boundary: BoundaryPolicy::default_spawn_boundary(),
            features: vec![FeatureNode {
                id: "cap-ipc".to_string(),
                title: "IPC OK".to_string(),
                kind: FeatureKind::Capability,
                parent: None,
                summary: "IPC snapshot fixture".to_string(),
                status_note: None,
                files: vec!["src/lib.rs".to_string()],
                relations: Vec::new(),
                provenance: FeatureProvenance::default(),
            }],
        };
        crate::feature_inventory::save_registry_atomic(&reg_path, &reg).unwrap();
        let snapshot_payload =
            crate::feature_inventory::api_coverage_snapshot_payload(&repo_root, &reg_path).unwrap();
        let snapshot_path = dir.0.join("feature-coverage.json");
        std::fs::write(
            &snapshot_path,
            serde_json::to_string_pretty(&snapshot_payload).unwrap(),
        )
        .unwrap();
        std::fs::remove_dir_all(&repo_root).unwrap();

        let state = IpcState::with_test_wait_timeout(
            RoleUrls::from_explicit(
                "postgres://spawn_reader@127.0.0.1:1/spawn".to_string(),
                "postgres://spawn_human@127.0.0.1:1/spawn".to_string(),
                "postgres://spawn_node_gateway@127.0.0.1:1/spawn".to_string(),
            ),
            dir.0.join("cas"),
            None,
            std::time::Duration::ZERO,
        );

        serve::with_test_feature_coverage_source(
            FeatureCoverageSource::PrecomputedSnapshot { snapshot_path },
            || {
                let get_resp = dispatch_buffered(
                    &ipc_request("GET", "/api/features/coverage", "", vec![]),
                    &state,
                );
                assert_eq!(get_resp.status, 200);
                let get_body: Value = serde_json::from_slice(&get_resp.body).unwrap();
                assert_eq!(get_body["registry_writable"], json!(false));
                assert_eq!(get_body["report"]["counts"]["covered"], json!(1));

                for query in ["", "mission=1"] {
                    let scan_resp = dispatch_buffered(
                        &ipc_request("POST", "/api/features/scan", query, vec![]),
                        &state,
                    );
                    assert_eq!(scan_resp.status, 409, "scan {query}");
                    let scan_body: Value = serde_json::from_slice(&scan_resp.body).unwrap();
                    assert_eq!(scan_body["code"], "registry_read_only");
                    assert_eq!(scan_body["registry_writable"], json!(false));
                    assert!(
                        scan_body["read_only_reason"]
                            .as_str()
                            .is_some_and(|r| !r.trim().is_empty())
                    );

                    let mut assoc_req =
                        ipc_request("POST", "/api/features/associate", query, vec![]);
                    assoc_req.body = br#"{"path":"src/lib.rs","features":["cap-ipc"]}"#.to_vec();
                    let assoc_resp = dispatch_buffered(&assoc_req, &state);
                    assert_eq!(assoc_resp.status, 409, "assoc {query}");
                    let assoc_body: Value = serde_json::from_slice(&assoc_resp.body).unwrap();
                    assert_eq!(assoc_body["code"], "registry_read_only");
                    assert_eq!(assoc_body["registry_writable"], json!(false));
                }
            },
        );
    }

    fn spawn_mock_ipc_agent_uds(
        mock_sock: &std::path::Path,
        ext_reads: Arc<std::sync::atomic::AtomicUsize>,
        mutations: Arc<std::sync::atomic::AtomicUsize>,
    ) -> std::thread::JoinHandle<()> {
        use std::io::{BufRead as _, Write as _};
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::Ordering;

        let listener = UnixListener::bind(mock_sock).expect("bind mock agent sock");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let mut br = std::io::BufReader::new(stream.try_clone().expect("clone"));
                let mut line = String::new();
                if br.read_line(&mut line).is_err() || line.is_empty() {
                    continue;
                }
                let req: Value = serde_json::from_str(&line).expect("json rpc");
                let method = req["method"].as_str().unwrap_or_default();
                if method == "__stop" {
                    let _ = (&stream).write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n");
                    break;
                }
                let reply = match method {
                    "terminal.list" => json!({
                        "jsonrpc": "2.0",
                        "id": req["id"],
                        "result": {
                            "terminals": [
                                {
                                    "address": "t1",
                                    "labels": {"spawn_owner": "loop", "spawn_terminal": "spawn-exec_1"},
                                    "state": "live", "pid": 101, "cols": 80, "rows": 24,
                                    "title": "managed", "status": {"state": "working", "app": "claude"}, "turn": 1
                                },
                                {
                                    "address": "t2",
                                    "labels": {},
                                    "state": "live", "pid": 202, "cols": 80, "rows": 24,
                                    "title": "external", "status": {"state": "idle", "app": "codex"}, "turn": 1
                                }
                            ]
                        }
                    }),
                    "terminal.get" => {
                        let term = req["params"]["terminal"].as_str().unwrap_or_default();
                        let labels = if term == "t1" {
                            json!({"spawn_owner": "loop", "spawn_terminal": "spawn-exec_1"})
                        } else {
                            json!({})
                        };
                        json!({
                            "jsonrpc": "2.0",
                            "id": req["id"],
                            "result": {
                                "address": term, "labels": labels, "state": "live",
                                "pid": if term == "t1" { 101 } else { 202 },
                                "cols": 80, "rows": 24, "title": term,
                                "status": {"state": "idle", "app": "claude"}, "turn": 1
                            }
                        })
                    }
                    "terminal.read" => {
                        if req["params"]["terminal"].as_str() == Some("t2") {
                            ext_reads.fetch_add(1, Ordering::SeqCst);
                        }
                        json!({
                            "jsonrpc": "2.0",
                            "id": req["id"],
                            "result": { "text": "Select:\n  1. Yes\n  2. No\n", "complete": true, "how": "screen" }
                        })
                    }
                    "terminal.keys" => {
                        mutations.fetch_add(1, Ordering::SeqCst);
                        json!({ "jsonrpc": "2.0", "id": req["id"], "result": { "status": "idle", "turn": 2 } })
                    }
                    "hello" => json!({ "jsonrpc": "2.0", "id": req["id"], "result": {} }),
                    other => panic!("unexpected rpc method {other}"),
                };
                let _ = (&stream).write_all(format!("{reply}\n").as_bytes());
            }
        })
    }

    #[test]
    fn ipc_agent_routes_dispatch_list_snapshot_events_action_and_fail_closed_viewer() {
        use std::sync::atomic::AtomicUsize;

        let dir = ScratchDir::new("ipc-agents");
        let mock_sock = dir.0.join("hc.sock");
        let ext_reads = Arc::new(AtomicUsize::new(0));
        let mutations = Arc::new(AtomicUsize::new(0));
        let srv =
            spawn_mock_ipc_agent_uds(&mock_sock, Arc::clone(&ext_reads), Arc::clone(&mutations));

        let state = IpcState::with_test_wait_timeout(
            RoleUrls::from_explicit(
                "postgres://spawn_reader@127.0.0.1:1/spawn".to_string(),
                "postgres://spawn_human@127.0.0.1:1/spawn".to_string(),
                "postgres://spawn_node_gateway@127.0.0.1:1/spawn".to_string(),
            ),
            dir.0.join("cas"),
            None,
            std::time::Duration::ZERO,
        )
        .with_test_agent_socket(mock_sock.clone());

        assert_ipc_viewer_rbac(&state, &ext_reads, &mutations);
        assert_ipc_events_and_actions(&state, &mutations);

        if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&mock_sock) {
            use std::io::Write as _;
            let _ = stream.write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"__stop\",\"params\":{}}\n",
            );
        }
        srv.join().expect("join mock srv");
    }

    fn assert_ipc_viewer_rbac(
        state: &IpcState,
        ext_reads: &std::sync::atomic::AtomicUsize,
        mutations: &std::sync::atomic::AtomicUsize,
    ) {
        use std::sync::atomic::Ordering;

        // Viewer headers (including unknown role and bearer viewer token) fail closed.
        for (h_key, h_val) in [
            ("x-spawn-role", "viewer"),
            ("x-spawn-role", "untrusted_custom"),
            ("x-spawn-viewer", "true"),
            ("authorization", "Bearer sv1.123.sig"),
        ] {
            let list_resp = dispatch_buffered(
                &ipc_request("GET", "/api/agents", "all=true", vec![(h_key, h_val)]),
                state,
            );
            assert_eq!(list_resp.status, 200);
            let list_json: Value = serde_json::from_slice(&list_resp.body).unwrap();
            assert_eq!(list_json["count"], 1, "{h_key}={h_val}");
            assert_eq!(list_json["agents"][0]["address"], "t1");

            for path in ["/api/agents/t2/snapshot", "/api/agents/t2/events"] {
                let deny =
                    dispatch_buffered(&ipc_request("GET", path, "", vec![(h_key, h_val)]), state);
                assert_eq!(deny.status, 403, "{path} {h_key}={h_val}");
            }
            let mut deny_act =
                ipc_request("POST", "/api/agents/t1/action", "", vec![(h_key, h_val)]);
            deny_act.body = br#"{"action":"interrupt"}"#.to_vec();
            let deny_act_resp = dispatch_buffered(&deny_act, state);
            assert_eq!(deny_act_resp.status, 403);
        }
        assert_eq!(ext_reads.load(Ordering::SeqCst), 0);
        assert_eq!(mutations.load(Ordering::SeqCst), 0);
    }

    fn assert_ipc_events_and_actions(state: &IpcState, mutations: &std::sync::atomic::AtomicUsize) {
        use std::sync::atomic::Ordering;

        // LocalOwner full flow + ETag 304 + HEAD + POST action.
        let ev_resp = dispatch_buffered(
            &ipc_request("GET", "/api/agents/t2/events", "limit=10", vec![]),
            state,
        );
        assert_eq!(ev_resp.status, 200);
        let etag = ev_resp
            .headers
            .iter()
            .find(|(k, _)| *k == "ETag")
            .map(|(_, v)| v.clone())
            .expect("events ETag");
        let ev_body: Value = serde_json::from_slice(&ev_resp.body).unwrap();
        assert_eq!(ev_body["address"], "t2");
        assert!(ev_body["events"].is_array());
        assert!(ev_body["summary"]["health"]["health"].is_string());
        assert!(ev_body["summary"]["discovery_status"]["status"].is_string());
        let latest_key = ev_body["latest_key"].as_str().expect("latest_key cursor");

        let delta_empty = dispatch_buffered(
            &ipc_request(
                "GET",
                "/api/agents/t2/events",
                &format!("after_key={latest_key}"),
                vec![],
            ),
            state,
        );
        assert_eq!(delta_empty.status, 200);
        let delta_body: Value = serde_json::from_slice(&delta_empty.body).unwrap();
        assert_eq!(delta_body["count"], 0);
        assert_eq!(delta_body["cursor_reset"], false);

        let stale_reset = dispatch_buffered(
            &ipc_request(
                "GET",
                "/api/agents/t2/events",
                "after_key=stale-missing-key",
                vec![],
            ),
            state,
        );
        assert_eq!(stale_reset.status, 200);
        let reset_body: Value = serde_json::from_slice(&stale_reset.body).unwrap();
        assert_eq!(reset_body["cursor_reset"], true);

        assert_eq!(
            dispatch_buffered(
                &ipc_request("GET", "/api/agents/t2/events", "after_key=", vec![]),
                state,
            )
            .status,
            400
        );

        let ev_304 = dispatch_buffered(
            &ipc_request(
                "GET",
                "/api/agents/t2/events",
                "limit=10",
                vec![("if-none-match", &etag)],
            ),
            state,
        );
        assert_eq!(ev_304.status, 304);

        let head_ev = dispatch_buffered(
            &ipc_request("HEAD", "/api/agents/t2/events", "limit=10", vec![]),
            state,
        );
        assert_eq!(head_ev.status, 200);
        assert!(head_ev.body.is_empty());

        assert_eq!(
            dispatch_buffered(
                &ipc_request("GET", "/api/agents/t1/events", "limit=0", vec![]),
                state
            )
            .status,
            400
        );
        assert_eq!(
            dispatch_buffered(
                &ipc_request("POST", "/api/agents/t1/events", "", vec![]),
                state
            )
            .status,
            405
        );

        let mut act = ipc_request("POST", "/api/agents/t2/action", "", vec![]);
        act.body = br#"{"action":"send_keys","keys":["down"]}"#.to_vec();
        let act_resp = dispatch_buffered(&act, state);
        assert_eq!(act_resp.status, 200);
        assert_eq!(mutations.load(Ordering::SeqCst), 1);
    }
}
