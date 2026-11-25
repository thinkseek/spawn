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

/// The usage of `spawn verify`, the only copy (main's help reads it).
pub(crate) const USAGE: &str = "usage: spawn verify <mission>/<objective> --by <name> --how <text> \
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
        list(
            &store
                .verification_facts()
                .map_err(|error| error.to_string())?,
            now_ms(),
        )
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
    Ok(
        json!({"type": "objective.verified", "mission": mission, "objective": objective,
              "by": by, "how": how, "result": result}),
    )
}

/// `<mission>/<objective>`, each a whole number from 1 (`9/28` and
/// `9/028` name the same objective; an objective fits an `integer`).
pub(crate) fn parse_name(text: &str) -> Result<(i64, i64), String> {
    let refused = || {
        format!("an objective is named <mission>/<objective>, whole numbers from 1, not '{text}'")
    };
    let (mission, objective) = text.split_once('/').ok_or_else(refused)?;
    let number = |part: &str, most: i64| {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        part.parse::<i64>()
            .ok()
            .filter(|number| (1..=most).contains(number))
    };
    match (
        number(mission, i64::MAX),
        number(objective, i64::from(i32::MAX)),
    ) {
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
        .map_or(0, |time| {
            i64::try_from(time.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{A_DAY_MS, LISTED_SINCE_MS, list, name, parse_name};

    const C: i64 = LISTED_SINCE_MS;
    const H: i64 = 3_600_000;

    fn landing(mission: i64, objective: i64, task: i64, landed_ms: i64) -> Value {
        json!({"mission": mission, "objective": objective, "task": task,
               "landed_ms": landed_ms, "verified_by": null})
    }

    fn seen(mission: i64, objective: i64, result: &str, at_ms: i64) -> Value {
        json!({"mission": mission, "objective": objective, "result": result, "by": "lead",
               "how": "read it", "at_ms": at_ms})
    }

    /// The listed objectives' names, in order.
    fn listed(answer: &Value) -> Vec<String> {
        answer["unverified"]
            .as_array()
            .expect("a list")
            .iter()
            .map(|entry| entry["objective"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    fn facts(landings: &[Value], verifications: &[Value]) -> Value {
        json!({"landings": landings, "verifications": verifications, "deploy": null})
    }

    #[test]
    fn a_pass_after_the_newest_landing_verifies_and_one_before_it_does_not() {
        let now = C + 10 * H;
        let answer = list(
            &facts(
                &[landing(4, 1, 10, C + H), landing(4, 2, 11, C + 2 * H)],
                &[seen(4, 1, "pass", C + 3 * H), seen(4, 2, "pass", C + H)],
            ),
            now,
        );
        assert_eq!(listed(&answer), ["4/002"], "{answer}");
        assert_eq!(answer["unverified"][0]["last"], Value::Null);
        assert_eq!(answer["unverified"][0]["task"], 11);
        assert_eq!(answer["unverified"][0]["landed_ms"], C + 2 * H);
        assert_eq!(answer["unverified"][0]["age_ms"], 8 * H);
    }

    #[test]
    fn the_newest_verification_decides() {
        let answer = list(
            &facts(
                &[landing(4, 1, 10, C + H), landing(4, 2, 11, C + H + 1)],
                &[
                    seen(4, 1, "pass", C + 2 * H),
                    seen(4, 2, "fail", C + 2 * H),
                    seen(4, 1, "fail", C + 3 * H),
                    seen(4, 2, "pass", C + 3 * H),
                ],
            ),
            C + 4 * H,
        );
        assert_eq!(listed(&answer), ["4/001"], "a fail after a pass: {answer}");
        assert_eq!(
            answer["unverified"][0]["last"],
            json!({"result": "fail", "by": "lead", "how": "read it", "at_ms": C + 3 * H})
        );
    }

    #[test]
    fn one_number_in_two_missions_is_two_objectives() {
        let answer = list(
            &facts(
                &[landing(9, 28, 1, C + 2 * H), landing(4, 28, 2, C + H)],
                &[seen(9, 28, "pass", C + 3 * H)],
            ),
            C + 4 * H,
        );
        assert_eq!(listed(&answer), ["4/028"], "{answer}");
        let answer = list(
            &facts(
                &[landing(9, 28, 1, C + 2 * H), landing(4, 28, 2, C + H)],
                &[],
            ),
            C + 4 * H,
        );
        assert_eq!(listed(&answer), ["4/028", "9/028"], "oldest landing first");
    }

    #[test]
    fn landings_before_the_listed_since_are_counted_not_listed() {
        let answer = list(
            &facts(
                &[
                    landing(4, 1, 1, C - 1),
                    landing(4, 2, 2, C - 2 * H),
                    landing(4, 3, 3, C - 3 * H),
                    landing(4, 4, 4, C),
                ],
                &[seen(4, 2, "pass", C - H), seen(4, 3, "fail", C - H)],
            ),
            C + H,
        );
        assert_eq!(listed(&answer), ["4/004"], "{answer}");
        assert_eq!(answer["older_unverified"], 2);
        assert_eq!(answer["listed_since_ms"], C);
    }

    #[test]
    fn over_a_day_is_an_age_of_24_hours_or_more() {
        let now = C + 30 * H;
        let answer = list(
            &facts(
                &[
                    landing(4, 1, 1, now - A_DAY_MS),
                    landing(4, 2, 2, now - A_DAY_MS + 1),
                ],
                &[],
            ),
            now,
        );
        assert_eq!(answer["unverified"][0]["over_a_day"], true, "{answer}");
        assert_eq!(answer["unverified"][1]["over_a_day"], false);
        assert_eq!(answer["over_a_day"], 1);
    }

    #[test]
    fn the_deploy_is_spawns_own_newest_or_null() {
        assert_eq!(list(&facts(&[], &[]), C)["deploy"], Value::Null);
        let mut with = facts(&[], &[]);
        with["deploy"] = json!({"at_ms": C, "head": "a".repeat(40), "ok": false,
                                "reason": "the build failed"});
        assert_eq!(
            list(&with, C)["deploy"],
            json!({"what": "Spawn's own deploys (deploy/deploy.sh)", "at_ms": C,
                   "head": "a".repeat(40), "ok": false, "reason": "the build failed"})
        );
    }

    #[test]
    fn a_name_is_mission_slash_objective_whole_numbers_from_1() {
        assert_eq!(parse_name("9/28"), Ok((9, 28)));
        assert_eq!(parse_name("9/028"), Ok((9, 28)));
        for bad in [
            "130",
            "0/130",
            "1/0",
            "1/",
            "/1",
            "a/1",
            "1/-2",
            "1/+2",
            "1/2/3",
            " 1/2",
            "1/2147483648",
        ] {
            assert!(parse_name(bad).is_err(), "{bad}");
        }
        assert_eq!(name(9, 28), "9/028");
        assert_eq!(name(4, 1133), "4/1133");
    }

    #[test]
    fn a_result_other_than_pass_or_fail_or_a_missing_flag_is_refused() {
        let flags = |words: &[&str]| {
            crate::Flags::parse(&words.iter().map(ToString::to_string).collect::<Vec<_>>())
                .expect("flags")
        };
        let good = super::record(
            "2/028",
            &flags(&["--by", "lead", "--how", "x", "--result", "pass"]),
        )
        .expect("a record");
        assert_eq!(
            good,
            json!({"type": "objective.verified", "mission": 2, "objective": 28, "by": "lead",
                   "how": "x", "result": "pass"})
        );
        assert!(
            super::record(
                "2/028",
                &flags(&["--by", "lead", "--how", "x", "--result", "maybe"])
            )
            .is_err()
        );
        assert!(super::record("2/028", &flags(&["--by", "lead", "--result", "pass"])).is_err());
        assert!(
            super::record(
                "2/028",
                &flags(&[
                    "--by", "lead", "--how", "x", "--result", "pass", "--at", "1"
                ])
            )
            .is_err()
        );
    }
}
