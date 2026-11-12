//! The storage line.

use serde_json::{Value, json};

use super::{Figure, Line};

/// The storage line, from health.storage.
pub(super) fn storage_line(storage: &Value) -> Line {
    let database = &storage["database"];
    let ratio = match (database["bytes"].as_f64(), database["log_bytes"].as_f64()) {
        (Some(bytes), Some(log)) if log > 0.0 => json!(bytes / log),
        _ => Value::Null,
    };
    // The largest readable test database, and the most schemas.
    let largest = |field: &str| {
        storage["test_databases"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|reading| reading[field].as_u64())
            .max()
            .map_or(Value::Null, |most| json!(most))
    };
    Line {
        name: "storage",
        north_star: Figure::new(
            "storage.days_left",
            "Days until the disk is full",
            "days",
            storage["days_left"].clone(),
        ),
        guardrails: vec![
            Figure::new(
                "storage.free_bytes",
                "Free disk",
                "bytes",
                storage["free_bytes"].clone(),
            ),
            Figure::new(
                "storage.archive_growth",
                "Transcript archive growth",
                "bytes_per_day",
                storage["growth_bytes_per_day"].clone(),
            ),
            Figure::new(
                "storage.database_ratio",
                "Database against its log",
                "ratio",
                ratio,
            ),
            Figure::new(
                "storage.test_database_bytes",
                "Largest test database",
                "bytes",
                largest("bytes"),
            ),
            Figure::new(
                "storage.test_database_schemas",
                "Most test schemas",
                "count",
                largest("schemas"),
            ),
            // Objective 094, after the frame's five: read in the background
            // and kept ([`unowned`]), set on `storage` by [`lines_at`].
            Figure::new(
                "storage.unowned_scratch_bytes",
                "Scratch nobody owns",
                "bytes",
                storage["unowned_scratch_bytes"].clone(),
            ),
            Figure::new(
                "storage.leaked_test_schemas",
                "Leaked test schemas",
                "count",
                storage["leaked_test_schemas"].clone(),
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use serde_json::{Value, json};

    #[test]
    fn the_storage_line_reads_health_storage() {
        let storage = json!({
            "days_left": 1234, "free_bytes": 104, "growth_bytes_per_day": 1_048_576,
            "database": {"bytes": 300, "log_bytes": 100},
            "test_databases": [
                {"role": "test", "bytes": 7, "schemas": 3},
                {"role": "fleet_test", "bytes": null, "schemas": null},
                {"role": "x", "bytes": 9, "schemas": 1},
            ],
            "unowned_scratch_bytes": 401_000, "leaked_test_schemas": 4,
        });
        let view = answer(
            &lines(
                &storage,
                &Value::Null,
                &Observed::default(),
                Value::Null,
                Vec::new(),
            ),
            &json!("/ws/core-lines.json"),
            Ok(None),
            &std::collections::BTreeMap::new(),
        );
        let names: Vec<&str> = view["lines"]
            .as_array()
            .expect("lines")
            .iter()
            .map(|line| line["line"].as_str().expect("a name"))
            .collect();
        assert_eq!(
            names,
            ["performance", "storage", "observability", "recoverability"]
        );
        let storage_line = &view["lines"][1];
        let values: Vec<(&str, Value)> = std::iter::once(&storage_line["north_star"])
            .chain(storage_line["guardrails"].as_array().expect("guardrails"))
            .map(|figure| {
                (
                    figure["key"].as_str().expect("a key"),
                    figure["value"].clone(),
                )
            })
            .collect();
        assert_eq!(
            values,
            [
                ("storage.days_left", json!(1234)),
                ("storage.free_bytes", json!(104)),
                ("storage.archive_growth", json!(1_048_576)),
                ("storage.database_ratio", json!(3.0)),
                ("storage.test_database_bytes", json!(9)),
                ("storage.test_database_schemas", json!(3)),
                ("storage.unowned_scratch_bytes", json!(401_000)),
                ("storage.leaked_test_schemas", json!(4)),
            ]
        );
        // No log size: no ratio; no readable test database: none.
        let bare = lines(
            &json!({"database": {"bytes": 300, "log_bytes": 0}, "test_databases": []}),
            &Value::Null,
            &Observed::default(),
            Value::Null,
            Vec::new(),
        );
        assert_eq!(bare[1].guardrails[2].value, Value::Null);
        assert_eq!(bare[1].guardrails[3].value, Value::Null);
        // The performance line without its figures: nothing collected.
        assert_eq!(view["lines"][0]["north_star"]["state"], "not_collected");
        assert_eq!(
            view["lines"][0]["guardrails"].as_array().map(Vec::len),
            Some(4)
        );
        // The other north stars are not collected.
        for index in [2, 3] {
            assert_eq!(view["lines"][index]["north_star"]["state"], "not_collected");
        }
        assert_eq!(view["lines"][3]["guardrails"], json!([]));
        assert_eq!(view["ranges_error"], Value::Null);
        assert_eq!(view["ignored_ranges"], json!([]));
    }
}
