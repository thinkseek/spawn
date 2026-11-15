//! The storage line.

use serde_json::{Value, json};

use super::{Figure, Line};

/// The storage line, from health.storage, days left from the disk's own
/// fall as [`super::disk::with_disk`] sets them (objective 141).
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
        north_star: Figure {
            measuring_since_ms: storage["days_left_measuring_since_ms"].clone(),
            ..Figure::new(
                "storage.days_left",
                "Days until the disk is full",
                "days",
                storage["days_left"].clone(),
            )
        },
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
            // Objective 141, after the seven: the disk's own fall and the
            // store that grew most, said with its store.
            Figure::new(
                "storage.disk_fall",
                "Free disk lost a day",
                "bytes_per_day",
                storage["disk_fall_bytes_per_day"].clone(),
            ),
            Figure {
                store: storage["grew_most"]["store"].as_str().map(str::to_string),
                ..Figure::new(
                    "storage.grew_most",
                    "Store that grew most",
                    "bytes_per_day",
                    storage["grew_most"]["bytes_per_day"].clone(),
                )
            },
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
            "disk_fall_bytes_per_day": 2_000_000,
            "grew_most": {"store": "unattributed", "bytes_per_day": 1_500_000},
        });
        let readings = lines(
            &storage,
            &Value::Null,
            &Observed::default(),
            Value::Null,
            Vec::new(),
        );
        let view = answer(
            &adopted(&readings),
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
        assert_eq!(names, ["storage", "performance", "sustainability"]);
        // The readings keep every storage figure (other pages read them);
        // the owner's set (objective 176) picks from them by key.
        let storage_line = &readings[1];
        let values: Vec<(&str, Value)> = std::iter::once(&storage_line.north_star)
            .chain(&storage_line.guardrails)
            .map(|figure| (figure.key, figure.value.clone()))
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
                ("storage.disk_fall", json!(2_000_000)),
                ("storage.grew_most", json!(1_500_000)),
            ]
        );
        // The store that grew most is said beside it (objective 141), now a
        // diagnostic; no other figure names one.
        let diagnostic = |key: &str| {
            view["diagnostics"]
                .as_array()
                .expect("diagnostics")
                .iter()
                .find(|figure| figure["key"] == key)
                .cloned()
                .expect(key)
        };
        assert_eq!(diagnostic("storage.grew_most")["store"], "unattributed");
        assert_eq!(diagnostic("storage.grew_most")["unit"], "bytes_per_day");
        assert_eq!(diagnostic("storage.disk_fall")["unit"], "bytes_per_day");
        assert_eq!(diagnostic("storage.disk_fall").get("store"), None);
        assert_eq!(view["alert_figures"][0]["key"], "storage.free_bytes");
        assert_eq!(view["alert_figures"][0]["value"], 104);
        assert_eq!(view["alert_figures"][0]["line"], "storage");
        assert_eq!(view["lines"][0]["north_star"]["state"], "not_measured_yet");
        still_measuring();
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
        assert_eq!(view["lines"][1]["north_star"]["state"], "not_collected");
        assert_eq!(
            view["lines"][1]["guardrails"].as_array().map(Vec::len),
            Some(3)
        );
        // The sustainability north star and restores are not collected.
        assert_eq!(view["lines"][2]["north_star"]["state"], "not_collected");
        assert_eq!(view["lines"][2]["guardrails"][0]["state"], "not_collected");
        assert_eq!(view["ranges_error"], Value::Null);
        assert_eq!(view["ignored_ranges"], json!([]));
    }

    /// Still measuring (objective 141): days left say from when, and no
    /// store is said.
    fn still_measuring() {
        let measuring = lines(
            &json!({"free_bytes": 104, "days_left": null,
                    "days_left_measuring_since_ms": 86_400_000,
                    "disk_fall_bytes_per_day": null, "grew_most": null}),
            &Value::Null,
            &Observed::default(),
            Value::Null,
            Vec::new(),
        );
        let days = figure_json(&measuring[1].north_star, None);
        assert_eq!(days["measuring_since_ms"], 86_400_000);
        assert_eq!(days["state"], "not_collected");
        let grew = figure_json(&measuring[1].guardrails[8], None);
        assert_eq!(
            (grew["value"].clone(), grew.get("store")),
            (Value::Null, None)
        );
    }
}
