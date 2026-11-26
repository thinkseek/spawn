//! `submit` and `say`: writing commands to the queue with smart resolution.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::id::{AgentId, CommandId, IdempotencyKey, Principal, SessionId};
use spawn_core::port::CommandQueue;
use spawn_postgres::PostgresStore;

use crate::Flags;
use crate::config::{env_value, open_store};

static AUTO_KEY_SEQ: AtomicU64 = AtomicU64::new(1);

/// The usage of `spawn submit`, the only copy (main's help reads it).
pub(crate) const USAGE: &str = "usage: spawn submit --agent <agent id> [--session <session id>] [--key <idempotency key>] \
                                --action <command action JSON> [--principal <name>]\n      \
                                (prints command=<id>; omits --session to auto-resolve a single live session,\n       \
                                omits --key to auto-generate one; the principal is spawn unless named;\n       \
                                the actions: start, input, resume, stop, key, as the README lists them)";

/// The usage of `spawn say`, the only copy (main's help reads it).
pub(crate) const SAY_USAGE: &str = "usage: spawn say <agent> <message> [--session <sid>] [--key <idempotency-key>] [--json]\n      \
                                    (shorthand to submit {\"type\":\"input\",\"text\":\"<message>\"} to <agent>;\n       \
                                    omits --session to auto-resolve a single live session, omits --key to\n       \
                                    auto-generate an idempotency key; accepts optional --principal <name>)";

#[derive(Debug, PartialEq, Eq)]
struct SayArgs {
    agent: AgentId,
    message: String,
    session: Option<SessionId>,
    key: Option<IdempotencyKey>,
    principal: Principal,
    json: bool,
}

fn generate_idempotency_key(prefix: &str) -> Result<IdempotencyKey, String> {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_u128, |duration| duration.as_millis());
    let pid = std::process::id();
    let seq = AUTO_KEY_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut entropy = [0_u8; 4];
    SystemRandom::new()
        .fill(&mut entropy)
        .map_err(|_| "failed to generate random bytes for idempotency key".to_string())?;
    let mut hex = String::with_capacity(8);
    for byte in entropy {
        let _ = write!(hex, "{byte:02x}");
    }
    IdempotencyKey::new(format!("{prefix}-{ms}-{pid}-{seq}-{hex}"))
        .map_err(|error| error.to_string())
}

fn resolve_active_session_from_fleet(fleet: &Value, agent: &AgentId) -> Result<SessionId, String> {
    let rows = fleet.as_array().map_or(&[][..], Vec::as_slice);
    let mut active: BTreeSet<String> = BTreeSet::new();
    let mut inactive: BTreeSet<String> = BTreeSet::new();

    for row in rows {
        if row["agent_id"].as_str() != Some(agent.as_str()) {
            continue;
        }
        let Some(sid) = row["session_id"].as_str() else {
            continue;
        };
        if row["live"].as_bool() == Some(true) {
            active.insert(sid.to_string());
        } else {
            inactive.insert(sid.to_string());
        }
    }

    match active.len() {
        1 => {
            let sid = active.into_iter().next().expect("checked len == 1");
            SessionId::new(sid).map_err(|error| error.to_string())
        }
        0 => {
            if inactive.is_empty() {
                Err(format!(
                    "no active session found for {agent}; pass --session <session_id> explicitly"
                ))
            } else {
                let candidates: Vec<String> = inactive.into_iter().collect();
                Err(format!(
                    "no active session found for {agent} (inactive sessions: {}); start the agent or pass --session <session_id> explicitly",
                    candidates.join(", ")
                ))
            }
        }
        count => {
            let candidates: Vec<String> = active.into_iter().collect();
            Err(format!(
                "ambiguous active session for {agent}: found {count} active sessions ({}); pass --session <session_id> to choose one",
                candidates.join(", ")
            ))
        }
    }
}

fn resolve_session_with_store(
    store: &PostgresStore,
    agent: &AgentId,
    explicit: Option<SessionId>,
) -> Result<SessionId, String> {
    if let Some(session) = explicit {
        return Ok(session);
    }
    let fleet = store.fleet_json().map_err(|error| error.to_string())?;
    resolve_active_session_from_fleet(&fleet, agent)
}

fn submit_command(
    store: &PostgresStore,
    principal: &Principal,
    submission: &Submission,
) -> Result<CommandId, String> {
    // A restricted login (the supervisor's terminal) submits through the
    // gateway, which takes the principal from the login and checks grants.
    if env_value("SPAWN_GATEWAY").is_some() {
        store.submit_via_gateway(submission)
    } else {
        store.submit(principal, submission)
    }
    .map_err(|error| error.to_string())
}

fn parse_say_args(arguments: &[String]) -> Result<SayArgs, String> {
    let mut positionals: Vec<String> = Vec::new();
    let mut session: Option<String> = None;
    let mut key: Option<String> = None;
    let mut principal: Option<String> = None;
    let mut json = false;
    let mut literal_only = false;

    let mut iter = arguments.iter();
    while let Some(word) = iter.next() {
        if literal_only {
            positionals.push(word.clone());
            continue;
        }
        match word.as_str() {
            "--" => literal_only = true,
            "--json" => json = true,
            flag @ ("--session" | "--key" | "--principal") => {
                let Some(value) = iter.next() else {
                    return Err(format!("{flag} needs a value\n{SAY_USAGE}"));
                };
                if value.starts_with("--") {
                    return Err(format!("{flag} needs a value, found {value}\n{SAY_USAGE}"));
                }
                match flag {
                    "--session" => session = Some(value.clone()),
                    "--key" => key = Some(value.clone()),
                    "--principal" => principal = Some(value.clone()),
                    _ => unreachable!(),
                }
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag {other}\n{SAY_USAGE}"));
            }
            positional => positionals.push(positional.to_string()),
        }
    }

    if positionals.len() != 2 {
        return Err(format!(
            "say expects <agent> and <message> (got {} positional arguments)\n{SAY_USAGE}",
            positionals.len()
        ));
    }

    let raw_message = positionals.pop().expect("len == 2");
    let raw_agent = positionals.pop().expect("len == 2");
    if raw_message.trim().is_empty() {
        return Err(format!("message must not be empty\n{SAY_USAGE}"));
    }

    let agent = AgentId::new(raw_agent).map_err(|error| error.to_string())?;
    let session = session
        .map(SessionId::new)
        .transpose()
        .map_err(|error| error.to_string())?;
    let key = key
        .map(IdempotencyKey::new)
        .transpose()
        .map_err(|error| error.to_string())?;
    let principal = Principal::new(principal.unwrap_or_else(|| "human".to_string()))
        .map_err(|error| error.to_string())?;

    Ok(SayArgs {
        agent,
        message: raw_message,
        session,
        key,
        principal,
        json,
    })
}

/// `spawn submit`: write one command to the queue with optional smart `--session` and `--key`.
pub(crate) fn submit(flags: &Flags) -> Result<i32, String> {
    for name in flags.names() {
        if !matches!(name, "agent" | "session" | "key" | "action" | "principal") {
            return Err(format!("unknown flag --{name}\n{USAGE}"));
        }
    }
    let principal = Principal::new(
        flags
            .get("principal")
            .unwrap_or_else(|| "spawn".to_string()),
    )
    .map_err(|error| error.to_string())?;
    let agent = AgentId::new(flags.required("agent")?).map_err(|error| error.to_string())?;
    let explicit_session = flags
        .get("session")
        .map(SessionId::new)
        .transpose()
        .map_err(|error| error.to_string())?;
    let explicit_key = flags
        .get("key")
        .map(IdempotencyKey::new)
        .transpose()
        .map_err(|error| error.to_string())?;
    let action: CommandAction = serde_json::from_str(&flags.required("action")?)
        .map_err(|error| format!("--action is not a command action: {error}"))?;

    let store = open_store()?;
    let session = resolve_session_with_store(&store, &agent, explicit_session)?;
    let key = match explicit_key {
        Some(key) => key,
        None => generate_idempotency_key("submit")?,
    };
    let submission = Submission {
        agent_id: agent,
        session_id: session,
        idempotency_key: key,
        action,
    };
    let id = submit_command(&store, &principal, &submission)?;
    println!("command={}", id.0);
    Ok(0)
}

/// `spawn say`: shorthand to send human intervention input to `<agent>`.
pub(crate) fn say(rest: &[String]) -> Result<i32, String> {
    let args = parse_say_args(rest)?;
    let store = open_store()?;
    let session = resolve_session_with_store(&store, &args.agent, args.session)?;
    let key = match args.key {
        Some(key) => key,
        None => generate_idempotency_key("say")?,
    };
    let submission = Submission {
        agent_id: args.agent.clone(),
        session_id: session.clone(),
        idempotency_key: key.clone(),
        action: CommandAction::Input {
            text: args.message,
            tags: Vec::new(),
        },
    };
    let id = submit_command(&store, &args.principal, &submission)?;
    if args.json {
        let payload = json!({
            "schema_version": 1,
            "command_id": id.0,
            "agent_id": args.agent.as_str(),
            "session_id": session.as_str(),
            "idempotency_key": key.as_str(),
            "principal": args.principal.as_str(),
            "action": "input",
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string())
        );
    } else {
        println!(
            "command={} agent={} session={} key={}",
            id.0,
            args.agent.as_str(),
            session.as_str(),
            key.as_str()
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use spawn_core::id::{AgentId, Principal, SessionId};

    use super::{
        Flags, generate_idempotency_key, parse_say_args, resolve_active_session_from_fleet, submit,
    };

    fn words(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn resolve_active_session_succeeds_for_single_live_session() {
        let agent = AgentId::new("agent_supervisor").expect("valid agent");
        let fleet = json!([
            {"agent_id": "agent_supervisor", "session_id": "session_old", "live": false},
            {"agent_id": "agent_supervisor", "session_id": "session_live", "live": true},
            {"agent_id": "agent_other", "session_id": "session_other", "live": true},
        ]);
        let resolved =
            resolve_active_session_from_fleet(&fleet, &agent).expect("single live resolves");
        assert_eq!(resolved, SessionId::new("session_live").expect("valid sid"));
    }

    #[test]
    fn resolve_active_session_rejects_zero_and_lists_inactive_when_present() {
        let agent = AgentId::new("agent_supervisor").expect("valid agent");
        let empty = resolve_active_session_from_fleet(&json!([]), &agent)
            .expect_err("empty fleet must error");
        assert!(empty.contains("no active session found"), "{empty}");
        assert!(empty.contains("--session"), "{empty}");

        let fleet = json!([
            {"agent_id": "agent_supervisor", "session_id": "session_stopped_2", "live": false},
            {"agent_id": "agent_supervisor", "session_id": "session_stopped_1", "live": false},
        ]);
        let err = resolve_active_session_from_fleet(&fleet, &agent)
            .expect_err("only stopped sessions must error");
        assert!(err.contains("no active session found"), "{err}");
        assert!(
            err.contains("session_stopped_1") && err.contains("session_stopped_2"),
            "{err}"
        );
        assert!(err.contains("--session"), "{err}");
    }

    #[test]
    fn resolve_active_session_rejects_ambiguous_and_lists_candidates() {
        let agent = AgentId::new("agent_worker").expect("valid agent");
        let fleet = json!([
            {"agent_id": "agent_worker", "session_id": "session_b", "live": true},
            {"agent_id": "agent_worker", "session_id": "session_a", "live": true},
            {"agent_id": "agent_worker", "session_id": "session_old", "live": false},
        ]);
        let err = resolve_active_session_from_fleet(&fleet, &agent)
            .expect_err("multiple live sessions must error");
        assert!(err.contains("2 active sessions"), "{err}");
        assert!(
            err.contains("session_a") && err.contains("session_b"),
            "{err}"
        );
        assert!(!err.contains("session_old"), "{err}");
        assert!(err.contains("--session"), "{err}");
    }

    #[test]
    fn generated_idempotency_keys_are_unique_and_prefixed() {
        let k1 = generate_idempotency_key("say").expect("say key 1");
        let k2 = generate_idempotency_key("say").expect("say key 2");
        let k3 = generate_idempotency_key("submit").expect("submit key");
        assert!(k1.as_str().starts_with("say-"), "{k1}");
        assert!(k3.as_str().starts_with("submit-"), "{k3}");
        assert_ne!(k1, k2);
    }

    #[test]
    fn parse_say_args_handles_positionals_flags_and_errors() {
        let basic =
            parse_say_args(&words(&["agent_supervisor", "please check main"])).expect("basic say");
        assert_eq!(
            basic.agent,
            AgentId::new("agent_supervisor").expect("valid")
        );
        assert_eq!(basic.message, "please check main");
        assert_eq!(basic.session, None);
        assert_eq!(basic.key, None);
        assert_eq!(basic.principal, Principal::new("human").expect("valid"));
        assert!(!basic.json);

        let full = parse_say_args(&words(&[
            "--json",
            "agent_supervisor",
            "--session",
            "session_9",
            "--key",
            "k-custom-1",
            "--principal",
            "owner",
            "--",
            "--help literal text",
        ]))
        .expect("full say");
        assert!(full.json);
        assert_eq!(
            full.session,
            Some(SessionId::new("session_9").expect("sid"))
        );
        assert_eq!(
            full.key
                .as_ref()
                .map(spawn_core::id::IdempotencyKey::as_str),
            Some("k-custom-1")
        );
        assert_eq!(full.principal.as_str(), "owner");
        assert_eq!(full.message, "--help literal text");

        assert!(parse_say_args(&words(&["agent_supervisor"])).is_err());
        assert!(parse_say_args(&words(&["agent_supervisor", "   "])).is_err());
        assert!(parse_say_args(&words(&["not_an_agent", "hi"])).is_err());
        assert!(parse_say_args(&words(&["agent_supervisor", "hi", "--unknown"])).is_err());
        assert!(parse_say_args(&words(&["agent_supervisor", "hi", "--session"])).is_err());
    }

    #[test]
    fn submit_validates_flags_before_opening_store() {
        let unknown = Flags::parse(&[
            "--agent".to_string(),
            "agent_x".to_string(),
            "--action".to_string(),
            r#"{"type":"input","text":"hi"}"#.to_string(),
            "--extra".to_string(),
            "1".to_string(),
        ])
        .expect("flag syntax");
        assert!(submit(&unknown).is_err());

        let bad_action = Flags::parse(&[
            "--agent".to_string(),
            "agent_x".to_string(),
            "--action".to_string(),
            "not-json".to_string(),
        ])
        .expect("flag syntax");
        let err = submit(&bad_action).expect_err("bad action must fail without DB");
        assert!(err.contains("--action"), "{err}");

        let bad_agent = Flags::parse(&[
            "--agent".to_string(),
            "invalid".to_string(),
            "--action".to_string(),
            r#"{"type":"input","text":"hi"}"#.to_string(),
        ])
        .expect("flag syntax");
        assert!(submit(&bad_agent).is_err());
    }
}
