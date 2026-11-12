//! The recoverability line.

use std::path::Path;

use serde_json::{Value, json};

use super::Figure;

/// The nightly's fact files under `<workspace root>/nightly`, as
/// scripts/rsi/nightly.sh writes them (`<source>.json`; a source's name may
/// hold dots): every non-hidden `*.json` there, read and parsed. The
/// scenario logs beside them (`<source>.<scenario>.log`), the lock
/// (`.<source>.lock`) and the run files (`.run-*`) are not facts; a file
/// that cannot be read or parsed as an object is passed over. `None`
/// without the directory.
pub(super) fn nightly_facts(dir: Option<&Path>) -> Option<Vec<Value>> {
    let entries = std::fs::read_dir(dir?).ok()?;
    let mut facts = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with('.') || Path::new(name).extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Ok(fact @ Value::Object(_)) = serde_json::from_str::<Value>(&text) {
            facts.push(fact);
        }
    }
    Some(facts)
}

/// The recoverability guardrails at `now_ms`, from `backups` (health.backups
/// on `/api/fleet`: `{dump, bundle}`, each `{at_ms, ...}` or null) and the
/// nightly's fact files (`None`: no directory):
/// - the age of the newest database copy and of the newest bundle, null
///   without the record;
/// - the age of the oldest nightly judging (`judged_at`), a fact with none
///   passed over; null without one;
/// - the facts whose `red_since` is set (a non-empty string, as the
///   integrator's `nightly_red` holds a source's landings); null without a
///   fact.
pub(super) fn recoverability(backups: &Value, facts: Option<&[Value]>, now_ms: i64) -> Vec<Figure> {
    let age =
        |at: Option<i64>| at.map_or(Value::Null, |at| json!(now_ms.saturating_sub(at).max(0)));
    let facts = facts.filter(|facts| !facts.is_empty());
    let oldest = facts.and_then(|facts| {
        facts
            .iter()
            .filter_map(|fact| fact["judged_at"].as_i64())
            .min()
    });
    let red = facts.map_or(Value::Null, |facts| {
        json!(
            facts
                .iter()
                .filter(|fact| fact["red_since"]
                    .as_str()
                    .is_some_and(|since| !since.is_empty()))
                .count()
        )
    });
    vec![
        Figure::new(
            "recoverability.cloud_dump_age",
            "Age of the newest database copy",
            "ms",
            age(backups["dump"]["at_ms"].as_i64()),
        ),
        Figure::new(
            "recoverability.cloud_bundle_age",
            "Age of the newest bundle",
            "ms",
            age(backups["bundle"]["at_ms"].as_i64()),
        ),
        Figure::new(
            "recoverability.nightly_age",
            "Age of the oldest nightly judging",
            "ms",
            age(oldest),
        ),
        Figure::new(
            "recoverability.nightly_red",
            "Nightly sources held red",
            "count",
            red,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use serde_json::{Value, json};

    fn values(figures: &[Figure]) -> Vec<(&'static str, Value)> {
        figures
            .iter()
            .map(|figure| (figure.key, figure.value.clone()))
            .collect()
    }

    #[test]
    fn the_backup_ages_are_the_newest_records_ages_or_null_without_one() {
        let none = recoverability(&json!({"dump": null, "bundle": null}), None, 10_000);
        assert_eq!(
            values(&none),
            [
                ("recoverability.cloud_dump_age", Value::Null),
                ("recoverability.cloud_bundle_age", Value::Null),
                ("recoverability.nightly_age", Value::Null),
                ("recoverability.nightly_red", Value::Null),
            ]
        );
        assert!(none[..3].iter().all(|figure| figure.unit == "ms"));
        assert_eq!(none[3].unit, "count");
        let backups = json!({
            "dump": {"at_ms": 8_200, "key": "postgres/hourly/a.dump.gpg", "bytes": 9},
            "bundle": {"at_ms": 1_000, "key": "bundles/incr/b.tar.zst.gpg", "bytes": 5},
        });
        let both = recoverability(&backups, None, 10_000);
        assert_eq!(both[0].value, json!(1_800));
        assert_eq!(both[1].value, json!(9_000));
        // Only a bundle: the copy's age stays null.
        let bundle_only = recoverability(
            &json!({"dump": null, "bundle": {"at_ms": 4_000}}),
            None,
            10_000,
        );
        assert_eq!(bundle_only[0].value, Value::Null);
        assert_eq!(bundle_only[1].value, json!(6_000));
        // The line carries them as its guardrails, in order.
        let line = &lines(
            &json!({}),
            &Value::Null,
            &Observed::default(),
            Value::Null,
            both,
        )[3];
        assert_eq!(line.name, "recoverability");
        assert_eq!(
            line.guardrails
                .iter()
                .map(|figure| figure.key)
                .collect::<Vec<_>>(),
            [
                "recoverability.cloud_dump_age",
                "recoverability.cloud_bundle_age",
                "recoverability.nightly_age",
                "recoverability.nightly_red",
            ]
        );
    }

    #[test]
    fn the_nightly_figures_read_only_the_fact_files() {
        let dir = std::env::temp_dir().join(format!(
            "spawn-core-lines-nightly-{}-{}",
            std::process::id(),
            crate::serve::now_ms()
        ));
        let backups = json!({"dump": null, "bundle": null});
        // No directory: no figure.
        assert_eq!(nightly_facts(Some(&dir.join("absent"))), None);
        assert_eq!(nightly_facts(None), None);
        std::fs::create_dir_all(&dir).expect("a directory");
        // An empty directory: no fact, no figure.
        let empty = nightly_facts(Some(&dir));
        assert_eq!(empty, Some(Vec::new()));
        let figures = recoverability(&backups, empty.as_deref(), 100_000);
        assert_eq!(figures[2].value, Value::Null);
        assert_eq!(figures[3].value, Value::Null);
        // A green fact, with a scenario log, a lock and a run file beside it.
        std::fs::write(
            dir.join("spawn.json"),
            r#"{"state": "green", "red_since": null, "judged_at": 90000}"#,
        )
        .expect("a fact");
        std::fs::write(dir.join("spawn.040.sh.log"), "a log").expect("a log");
        std::fs::write(dir.join(".spawn.lock"), "").expect("a lock");
        std::fs::write(
            dir.join(".run-1.json"),
            r#"{"red_since": "x", "judged_at": 1}"#,
        )
        .expect("a run file");
        let facts = nightly_facts(Some(&dir));
        assert_eq!(facts.as_ref().map(Vec::len), Some(1), "the log is no fact");
        let figures = recoverability(&backups, facts.as_deref(), 100_000);
        assert_eq!(figures[2].value, json!(10_000));
        assert_eq!(figures[3].value, json!(0));
        // A fact with no judged_at, one that is not JSON, and a red one whose
        // name holds dots.
        std::fs::write(
            dir.join("never.json"),
            r#"{"state": "skipped", "red_since": null, "judged_at": null}"#,
        )
        .expect("a fact");
        std::fs::write(dir.join("broken.json"), "{\"state\": ").expect("a broken fact");
        std::fs::write(
            dir.join("sup.ply-2.json"),
            r#"{"state": "red", "red_since": "bbbb", "judged_at": 40000}"#,
        )
        .expect("a fact");
        let facts = nightly_facts(Some(&dir));
        assert_eq!(facts.as_ref().map(Vec::len), Some(3));
        let figures = recoverability(&backups, facts.as_deref(), 100_000);
        assert_eq!(figures[2].value, json!(60_000), "the oldest judging");
        assert_eq!(figures[3].value, json!(1), "one source held red");
        // Only facts with no judged_at: no age, but the count stands.
        let unjudged = [json!({"red_since": "c", "judged_at": null})];
        let figures = recoverability(&backups, Some(&unjudged), 100_000);
        assert_eq!(figures[2].value, Value::Null);
        assert_eq!(figures[3].value, json!(1));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
