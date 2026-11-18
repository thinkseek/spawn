//! Control records stored masked (objective 170). A control record is
//! appended by `spawn_control_record` in SQL, never through
//! `EventLog::append`, so `spawn_core::redact::redact_event` never sees it:
//! a gate's output that printed a token is kept as written in `task.gated`'s
//! `log`, in the `note` the worker reads back, in the reason the page shows,
//! and so on for deploy, alert, backup and host records.
//!
//! [`masked`] passes every string of a record through
//! [`spawn_core::redact::redact`], except the strings at a path in
//! [`VERBATIM`]: the fields the folds and the Rust readers match (ids,
//! commits, enums, scenario names, the run's binding). In it `a.b` is key `b`
//! under `a`, an array's elements share its path, and `a.*` is everything
//! under `a` (strictly: `envelope` is not under `env`). A key not listed is
//! masked, so a new free-text field is safe by default. Masking changes
//! only credential-shaped text, so a field outside the list that holds none
//! is stored byte for byte.
//!
//! A few fields the store limits to 500 characters ([`LIMITED`]): when
//! masking lengthens one from at most 500 past 500, it is cut back to 500,
//! so masking never makes the store refuse a record it took before. No
//! other field is ever cut. [`stored_text`] is exactly what [`masked`]
//! stores for one top-level field: a reader that compares a stored value
//! with one it formats compares with that, never with a part of the
//! transformation.

use serde_json::Value;
use spawn_core::redact::redact;

/// The paths whose strings are stored as written, each group with the
/// readers that match it.
pub const VERBATIM: &[&str] = &[
    // The record's type: spawn_control_apply's dispatch, every reader's
    // filter (`kind -> 'detail' -> 'record' ->> 'type'`).
    "type",
    // Ids and joins: the folds' `where id = …`, the unique indexes
    // (task.held per task and hold, alert keys), the owner's request
    // outcomes (`request`), the halt and pause records, the digest's tags.
    "id",
    "mission_id",
    "agent_id",
    "session_id",
    "execution_id",
    "objective",
    "judging",
    "key",
    "subject",
    "digest",
    "halt",
    "task",
    "request",
    "hold",
    "command_id",
    "pause_event",
    "solved",
    "missions",
    "clear",
    "gone",
    "gates",
    "waiting",
    "without_baseline",
    "n",
    // Commits: compared with git and with each other (baselines, bases,
    // candidates, the stale rule, the commit map's readers, deploys).
    "base",
    "candidate",
    "pushed",
    "main",
    "commit",
    "ref",
    "head",
    "head_base",
    "main_now",
    "main_read",
    "from",
    "to",
    // Sources: joins on `sources ->> 0`, git, alert keys (objective 170's
    // known residual: a source URL with a password is stored as written).
    "source",
    "sources",
    // Enums and flags the folds branch on (verdict, state, cause, ...).
    "verdict",
    "head_verdict",
    "revision",
    "evidence",
    "class",
    "kind",
    "state",
    "cause",
    "by",
    "path",
    "judge",
    "name",
    "area",
    "result",
    "sealed_with",
    "escalate",
    "alert",
    "agrees",
    "chain",
    "yielded",
    "parked",
    "notice",
    "accepted",
    "ok",
    "gated",
    "urgent",
    "check",
    // Scenario names: the floor, the red main, load-sensitive parking, the
    // validator's defects.
    "scenario",
    "scenarios",
    "sensitive",
    "validator_defects",
    "scenario_names",
    "exits",
    "unequal",
    // A gate's fact: the host-fault cap reads on, tree and scenario
    // (TASK_HOST_FAULTS), the page its runs and failures count.
    "fact.on",
    "fact.scenario",
    "fact.runs",
    "fact.tree",
    "fact.failures",
    // An alert's evidence the alert step matches when it clears or keeps
    // an alert.
    "evidence.from_ms",
    "evidence.reason",
    "evidence.task",
    "evidence.credentials.path",
    // The run's binding: inputs and env are compared whole between a gate
    // and its re-judging (the adopted judging, warm starts).
    "inputs.*",
    "env.*",
    "head_inputs.*",
    "primary_env.*",
    // Backup names: object keys, bundles, manifests and hosts the drill
    // and the restore read back.
    "keys",
    "bundle",
    "manifest",
    "host",
    "account",
    // A mission's configuration, folded into mission_missions.
    "gate",
    "accept",
    "budget",
    "objectives.*",
    // Legacy records (a table row folded as written).
    "table",
    "row.*",
    // Assignment shapes the task fold copies into mission_tasks.
    "created_by",
    "model",
    "reissue_of",
    "continues",
    "reassign_of",
    "brief",
    "origin",
];

/// The (record type, top-level field) pairs the store limits to 500
/// characters (spawn_backup_record, spawn_deploy_record,
/// spawn_verification_record, spawn_objective_retire).
pub const LIMITED: &[(&str, &str)] = &[
    ("backup.drilled.transcripts", "reason"),
    ("backup.drilled.missions", "reason"),
    ("backup.drilled.controllers", "reason"),
    ("backup.drilled.sync", "reason"),
    ("deploy.finished", "reason"),
    ("objective.verified", "how"),
    ("objective.retired", "reason"),
];

/// The store's limit on a [`LIMITED`] field, in characters.
pub const LIMIT: usize = 500;

/// Whether the strings at `path` are stored as written.
fn verbatim(path: &str) -> bool {
    VERBATIM.iter().any(|entry| match entry.strip_suffix(".*") {
        Some(root) => {
            path == root
                || path
                    .strip_prefix(root)
                    .is_some_and(|rest| rest.starts_with('.'))
        }
        None => path == *entry,
    })
}

fn walk(value: &mut Value, path: &str) {
    match value {
        Value::String(text) if !verbatim(path) => {
            *text = redact(text);
        }
        Value::Array(items) => {
            for item in items {
                walk(item, path);
            }
        }
        Value::Object(fields) => {
            for (key, field) in fields.iter_mut() {
                let inner = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                walk(field, &inner);
            }
        }
        _ => {}
    }
}

/// `masked` text of a [`LIMITED`] field: cut back to [`LIMIT`] characters
/// when masking lengthened it from at most that past it.
fn limited(original: &str, masked: String) -> String {
    if original.chars().count() <= LIMIT && masked.chars().count() > LIMIT {
        masked.chars().take(LIMIT).collect()
    } else {
        masked
    }
}

/// `record` as it is stored: every string masked but those at a
/// [`VERBATIM`] path, a [`LIMITED`] field cut back as [`limited`] says.
#[must_use]
pub fn masked(record: &Value) -> Value {
    let mut stored = record.clone();
    walk(&mut stored, "");
    let kind = record["type"].as_str().unwrap_or_default();
    for (limited_kind, field) in LIMITED {
        if *limited_kind != kind {
            continue;
        }
        if let (Some(original), Some(Value::String(text))) =
            (record[*field].as_str(), stored.get_mut(*field))
        {
            *text = limited(original, std::mem::take(text));
        }
    }
    stored
}

/// Exactly what [`masked`] stores for the string `text` at top-level field
/// `field` of a record of type `kind`.
#[must_use]
pub fn stored_text(kind: &str, field: &str, text: &str) -> String {
    if verbatim(field) {
        return text.to_string();
    }
    let clean = redact(text);
    if LIMITED.contains(&(kind, field)) {
        limited(text, clean)
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::{LIMIT, LIMITED, VERBATIM, masked, stored_text};
    use serde_json::{Value, json};

    const TOKEN: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
    const BEARER: &str = "Bearer ghp_abcdefghijklmnopqrstuvwxyz0123456789";
    const URL: &str = "postgres://spawn:hunter2secret@db.example/spawn";

    fn free(text: &Value) -> String {
        text.as_str().expect("a string").to_string()
    }

    #[test]
    fn free_text_is_masked_wherever_it_is() {
        let record = json!({
            "type": "task.gated", "id": 7, "verdict": "rejected",
            "log": format!("| curl -H 'Authorization: {BEARER}'\n| PGPASSWORD=s3cret psql"),
            "note": format!("the gate printed {TOKEN}"),
            "reason": format!("connect {URL}"),
            "fact": {"on": "candidate", "reason": format!("token {TOKEN}")},
            "failures": [{"test": "t", "message": format!("PGPASSWORD=s3cret {TOKEN}"),
                          "class": "regression"}],
            "a_new_key": format!("api_key={TOKEN}"),
        });
        let stored = masked(&record);
        for text in [
            free(&stored["log"]),
            free(&stored["note"]),
            free(&stored["reason"]),
            free(&stored["fact"]["reason"]),
            free(&stored["failures"][0]["message"]),
            free(&stored["a_new_key"]),
        ] {
            assert!(text.contains("[redacted]"), "{text}");
            assert!(!text.contains(TOKEN), "{text}");
            assert!(!text.contains("s3cret"), "{text}");
            assert!(!text.contains("hunter2secret"), "{text}");
        }
        assert!(
            free(&stored["reason"]).contains("postgres://spawn:"),
            "the user stays"
        );
        assert_eq!(stored["verdict"], "rejected");
        assert_eq!(stored["id"], 7);
    }

    /// Every VERBATIM path keeps a credential-looking string byte for byte,
    /// at the top, nested and in an array. Not a prefix match: a key that
    /// only starts like one is masked.
    #[test]
    fn every_verbatim_path_keeps_its_bytes() {
        let secret = format!("PGPASSWORD=s3cret {BEARER} {URL}");
        for entry in VERBATIM {
            let path = entry
                .strip_suffix(".*")
                .map_or_else(|| (*entry).to_string(), |root| format!("{root}.inner"));
            let mut record = json!({"type": "x"});
            let mut cursor = &mut record;
            let parts: Vec<&str> = path.split('.').collect();
            for (index, part) in parts.iter().enumerate() {
                if index + 1 == parts.len() {
                    cursor[*part] = if *entry == "type" {
                        json!(secret)
                    } else {
                        json!([secret, secret])
                    };
                } else {
                    if cursor.get(*part).is_none() {
                        cursor[*part] = json!({});
                    }
                    cursor = &mut cursor[*part];
                }
            }
            let stored = masked(&record);
            assert_eq!(stored, record, "{entry}");
        }
        let record = json!({"type": "x", "idle": secret, "envelope": secret,
                            "fact": {"reason": secret}, "evidence": {"text": secret},
                            "inputsx": secret});
        let stored = masked(&record);
        for key in ["idle", "envelope", "inputsx"] {
            assert!(!free(&stored[key]).contains("s3cret"), "{key}");
        }
        assert!(!free(&stored["fact"]["reason"]).contains("s3cret"));
        assert!(!free(&stored["evidence"]["text"]).contains("s3cret"));
    }

    /// An explicit list apart from VERBATIM: a list that loses an entry
    /// fails here.
    #[test]
    fn the_keys_readers_need_whole_stay_whole() {
        let record = json!({
            "type": "alert.fired",
            "key": "source_away:https://bot:tok3n@example.com/x.git",
            "subject": "mission #4",
            "source": "https://bot:tok3n@example.com/x.git",
            "sources": ["https://bot:tok3n@example.com/x.git"],
            "env": {"PATH": "/usr/bin", "SPAWN_TOKEN_FILE": "/run/secret_token=abc"},
            "inputs": {"candidate": "a".repeat(40), "env": {"api_key": "x=y"}},
            "row": {"note": "PGPASSWORD=kept-as-the-legacy-row-was"},
            "gate": "/srv/gate?token=abc",
            "keys": ["postgres/hourly/spawn.dump.gpg?secret=abc"],
            "fact": {"on": "candidate", "scenario": "092.sh", "runs": 2, "tree": "candidate"},
            "judging": "j-PGPASSWORD=1",
            "base": "b".repeat(40), "candidate": "c".repeat(40), "main": "d".repeat(40),
            "head": "e".repeat(40), "from": "f".repeat(40), "to": "0".repeat(40),
        });
        assert_eq!(masked(&record), record);
        // Each would be masked anywhere else: the test holds credentials.
        for value in [
            &record["key"],
            &record["source"],
            &record["env"]["SPAWN_TOKEN_FILE"],
            &record["row"]["note"],
            &record["gate"],
            &record["keys"][0],
            &record["judging"],
        ] {
            let text = value.as_str().expect("a string");
            assert_ne!(spawn_core::redact::redact(text), text, "{text}");
        }
    }

    #[test]
    fn a_limited_reason_is_cut_back_to_its_limit_and_log_never() {
        let tail = " PWD=x";
        let reason = format!("{}{tail}", "r".repeat(LIMIT - tail.len()));
        assert_eq!(reason.chars().count(), LIMIT);
        let record = json!({"type": "deploy.finished", "ok": false, "head": "h", "by": "landing",
                            "reason": reason});
        let stored = masked(&record);
        let text = free(&stored["reason"]);
        assert_eq!(text.chars().count(), LIMIT, "masked, then cut back");
        assert!(!text.contains("PWD=x"), "{text}");
        let over = format!("{}{tail}", "r".repeat(LIMIT));
        let stored = masked(&json!({"type": "deploy.finished", "reason": over}));
        assert!(
            free(&stored["reason"]).chars().count() > LIMIT,
            "already over: not cut (the store refuses it as before)"
        );
        let log = format!("{}{tail}", "l".repeat(LIMIT - tail.len()));
        let stored = masked(&json!({"type": "deploy.finished", "log": log}));
        assert!(
            free(&stored["log"]).chars().count() > LIMIT,
            "log is never cut"
        );
    }

    #[test]
    fn only_a_limited_record_s_own_field_is_cut_and_stored_text_agrees() {
        let tail = " PWD=x";
        let text = format!("{}{tail}", "r".repeat(LIMIT - tail.len()));
        for (kind, field) in LIMITED {
            let stored = masked(&json!({"type": kind, (*field): text}));
            let value = free(&stored[*field]);
            assert_eq!(value.chars().count(), LIMIT, "{kind} {field}");
            assert!(!value.contains("PWD=x"));
            assert_eq!(stored_text(kind, field, &text), value, "{kind} {field}");
        }
        for (kind, field) in [
            ("source.push_no_verdict", "reason"),
            ("deploy.finished", "how"),
            ("objective.verified", "reason"),
            ("task.gated", "reason"),
        ] {
            let stored = masked(&json!({"type": kind, field: text}));
            let value = free(&stored[field]);
            assert!(value.chars().count() > LIMIT, "{kind} {field}: kept whole");
            assert!(!value.contains("PWD=x"));
            assert_eq!(stored_text(kind, field, &text), value, "{kind} {field}");
        }
        assert_eq!(
            stored_text("task.gated", "base", URL),
            URL,
            "a verbatim field"
        );
    }

    #[test]
    fn a_record_without_a_credential_is_unchanged() {
        let record = json!({"type": "task.gated", "id": 3, "verdict": "rejected",
                            "log": "| stage gate rejected 400ms regression: 092.sh must still pass",
                            "note": "092.sh failed", "reason": "regression",
                            "failures": [{"test": "a::b", "message": "assertion failed"}]});
        assert_eq!(masked(&record), record);
    }
}
