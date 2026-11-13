//! An objective's verification after its landing (objective 133).
//!
//! An objective counts as done when its gate passes and it lands; the look
//! at the running product is a separate step. `spawn verify
//! <mission>/<objective> --by <name> --how <text> --result pass|fail`
//! records it (`objective.verified`, through the owner-only
//! `spawn_verification_record`), and `spawn verify list` (and
//! `/api/verifications`, through the reader login) lists what is landed
//! and not verified ([`list`]). Objectives are numbered per mission
//! (Flake Doctor's 028 is not Spawn's 028), so a name is
//! `<mission>/<objective>`, printed `<mission>/<NNN>`.

use serde_json::{Value, json};

use crate::Flags;

/// 2026-11-13 00:00 +08:00, when every objective's prompt began to carry
/// a "Verified by:" paragraph: the list names the objectives whose newest
/// landing is on or after it, and counts the older ones.
pub(crate) const LISTED_SINCE_MS: i64 = 1_794_499_200_000;

/// An unverified objective is over a day old from this age.
const A_DAY_MS: i64 = 24 * 3_600_000;

/// What the list says Spawn's own deploy record is (no claim about any one
/// landing or mission: a deploy record names neither).
const DEPLOYS: &str = "Spawn's own deploys (deploy/deploy.sh)";

const USAGE: &str = "usage: spawn verify <mission>/<objective> --by <name> --how <text> \
                     --result pass|fail\n       spawn verify list";

/// `spawn verify ...`: record one verification (prints `{event,
/// objective}`) or list what is landed and not verified.
pub(crate) fn command(rest: &[String]) -> Result<i32, String> {
    let Some(first) = rest.first() else {
        return Err(USAGE.to_string());
    };
    let answer = if first == "list" {
        if let Some(word) = rest.get(1) {
            return Err(format!("unexpected argument {word}\n{USAGE}"));
        }
        let store = crate::open_store()?;
        list(&store.verification_facts().map_err(|error| error.to_string())?, now_ms())
    } else {
        let record = record(first, &Flags::parse(&rest[1..])?)?;
        let store = crate::open_store()?;
        let event = store
            .verification_record(&record)
            .map_err(|error| error.to_string())?;
        json!({"event": event, "objective": name(
            record["mission"].as_i64().unwrap_or_default(),
            record["objective"].as_i64().unwrap_or_default())})
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&answer).unwrap_or_else(|_| "{}".to_string())
    );
    Ok(0)
}

/// The `objective.verified` record `spawn verify <objective> <flags>` asks
/// for: the name `<mission>/<objective>` ([`parse_name`]), `--by`, `--how`
/// and `--result` pass or fail, and no other flag. Refused before the
/// store.
fn record(objective: &str, flags: &Flags) -> Result<Value, String> {
    let (mission, objective) = parse_name(objective)?;
    if let Some(other) = flags
        .names()
        .find(|flag| !["by", "how", "result"].contains(flag))
    {
        return Err(format!("unknown flag --{other}\n{USAGE}"));
    }
    let required = |flag: &str| {
        flags
            .get(flag)
            .ok_or_else(|| format!("--{flag} is required\n{USAGE}"))
    };
    let (by, how, result) = (required("by")?, required("how")?, required("result")?);
    if result != "pass" && result != "fail" {
        return Err(format!("--result is pass or fail, not '{result}'"));
    }
    Ok(json!({"type": "objective.verified", "mission": mission, "objective": objective,
              "by": by, "how": how, "result": result}))
}

/// `<mission>/<objective>`, each a whole number from 1 (`9/28` and
/// `9/028` name the same objective; an objective fits an `integer`).
pub(crate) fn parse_name(text: &str) -> Result<(i64, i64), String> {
    let refused = || format!("an objective is named <mission>/<objective>, whole numbers from 1, not '{text}'");
    let (mission, objective) = text.split_once('/').ok_or_else(refused)?;
    let number = |part: &str, most: i64| {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        part.parse::<i64>()
            .ok()
            .filter(|number| (1..=most).contains(number))
    };
    match (number(mission, i64::MAX), number(objective, i64::from(i32::MAX))) {
        (Some(mission), Some(objective)) => Ok((mission, objective)),
        _ => Err(refused()),
    }
}

/// An objective's name as the list prints it: `<mission>/<NNN>` (`9/028`).
pub(crate) fn name(mission: i64, objective: i64) -> String {
    format!("{mission}/{objective:03}")
}

/// The list from the store's facts
/// ([`spawn_postgres::PostgresStore::verification_facts`]) at `now_ms`:
/// - `unverified`: the objectives whose newest landing is on or after
///   [`LISTED_SINCE_MS`] and whose newest verification recorded after that
///   landing is not a pass (none, or a fail: the newest decides), oldest
///   landing first, each `{objective, task, landed_ms, age_ms, over_a_day,
///   verified_by, last}` (`last`: that newest verification, a fail, or
///   null);
/// - `over_a_day`: how many of them are a day old or more;
/// - `older_unverified`: how many objectives whose newest landing is
///   before [`LISTED_SINCE_MS`] are not verified by the same rule;
/// - `listed_since_ms`;
/// - `deploy`: Spawn's own newest deploy record `{what, at_ms, head, ok,
///   reason}`, or null.
pub(crate) fn list(facts: &Value, now_ms: i64) -> Value {
    let verifications: &[Value] = facts["verifications"].as_array().map_or(&[], Vec::as_slice);
    let mut unverified: Vec<Value> = Vec::new();
    let mut older = 0_u64;
    for landing in facts["landings"].as_array().into_iter().flatten() {
        let landed_ms = landing["landed_ms"].as_i64().unwrap_or_default();
        let last = verifications
            .iter()
            .filter(|seen| {
                seen["mission"] == landing["mission"]
                    && seen["objective"] == landing["objective"]
                    && seen["at_ms"].as_i64().unwrap_or_default() > landed_ms
            })
            .max_by_key(|seen| seen["at_ms"].as_i64().unwrap_or_default());
        if last.is_some_and(|seen| seen["result"] == "pass") {
            continue;
        }
        if landed_ms < LISTED_SINCE_MS {
            older += 1;
            continue;
        }
        let age_ms = now_ms.saturating_sub(landed_ms).max(0);
        unverified.push(json!({
            "objective": name(
                landing["mission"].as_i64().unwrap_or_default(),
                landing["objective"].as_i64().unwrap_or_default()),
            "task": landing["task"],
            "landed_ms": landed_ms,
            "age_ms": age_ms,
            "over_a_day": age_ms >= A_DAY_MS,
            "verified_by": landing["verified_by"],
            "last": last.map_or(Value::Null, |seen| json!({
                "result": seen["result"], "by": seen["by"], "how": seen["how"],
                "at_ms": seen["at_ms"]})),
        }));
    }
    unverified.sort_by_key(|entry| {
        (
            entry["landed_ms"].as_i64().unwrap_or_default(),
            entry["task"].as_i64().unwrap_or_default(),
        )
    });
    let over_a_day = unverified
        .iter()
        .filter(|entry| entry["over_a_day"] == true)
        .count();
    let deploy = match facts.get("deploy") {
        Some(deploy) if deploy.is_object() => json!({
            "what": DEPLOYS, "at_ms": deploy["at_ms"], "head": deploy["head"],
            "ok": deploy["ok"], "reason": deploy["reason"]}),
        _ => Value::Null,
    };
    json!({
        "unverified": unverified,
        "over_a_day": over_a_day,
        "older_unverified": older,
        "listed_since_ms": LISTED_SINCE_MS,
        "deploy": deploy,
    })
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| i64::try_from(time.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests;
