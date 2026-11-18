//! The storage line.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

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
            // store that grew most, said with its store. The fall says its
            // basis, the week's fit or the last day (objective 222).
            Figure {
                basis: storage["disk_fall_basis"].as_str().map(str::to_string),
                ..Figure::new(
                    "storage.disk_fall",
                    "Free disk lost a day",
                    "bytes_per_day",
                    storage["disk_fall_bytes_per_day"].clone(),
                )
            },
            Figure {
                store: storage["grew_most"]["store"].as_str().map(str::to_string),
                ..Figure::new(
                    "storage.grew_most",
                    "Store that grew most",
                    "bytes_per_day",
                    storage["grew_most"]["bytes_per_day"].clone(),
                )
            },
            // Objective 185, after the nine: the storage north star and S1,
            // from the storage readings (objective 180) `lines_at` sets.
            bytes_per_landing(&storage["readings"]),
            rebuildable_bytes(&storage["readings"]),
        ],
    }
}

/// One hour, in milliseconds.
const HOUR_MS: i64 = 3_600_000;

/// The sources S1 adds up: what can be rebuilt.
const REBUILDABLE: [&str; 3] = ["scratch", "build_cache", "test_databases"];

/// A storage figure not measured, and why.
fn not_measured(key: &'static str, name: &'static str, note: String) -> Figure {
    Figure {
        note: Some(note),
        ..Figure::not_measured_yet(key, name, "bytes")
    }
}

/// A source's bytes in a reading's `sources`, when it was measured.
fn bytes_of(sources: &Value, name: &str) -> Option<i64> {
    sources[name]["bytes"].as_i64()
}

/// The storage north star, "Bytes a landing leaves behind" (objective 185):
/// over the sources measured in both readings (`readings`: the newest and the
/// newest a day older, `HOST_STORAGE_BYTES`), the newest's bytes less the
/// older's, each source's gain in `by_source`, divided by the landings
/// between them. Not measured without two readings, without a source
/// measured in both, or without a landing between them.
pub(super) fn bytes_per_landing(readings: &Value) -> Figure {
    const KEY: &str = "storage.bytes_per_landing";
    const NAME: &str = "Bytes a landing leaves behind";
    let newest = &readings["newest"];
    let Some(newest_at) = newest["at_ms"].as_i64() else {
        return not_measured(
            KEY,
            NAME,
            "not measured yet: no storage reading yet".to_string(),
        );
    };
    let older = &readings["older"];
    let Some(older_at) = older["at_ms"].as_i64() else {
        return Figure {
            as_of_ms: Some(newest_at),
            ..not_measured(
                KEY,
                NAME,
                "not measured yet: the figure needs two storage readings a day apart".to_string(),
            )
        };
    };
    let names: BTreeSet<&String> = [&newest["sources"], &older["sources"]]
        .into_iter()
        .filter_map(Value::as_object)
        .flat_map(Map::keys)
        .collect();
    let mut gains = Map::new();
    let mut one_only: Vec<&str> = Vec::new();
    let mut total: i64 = 0;
    for name in names {
        match (
            bytes_of(&newest["sources"], name),
            bytes_of(&older["sources"], name),
        ) {
            (Some(now), Some(then)) => {
                let gain = now.saturating_sub(then);
                total = total.saturating_add(gain);
                gains.insert(name.clone(), json!(gain));
            }
            _ => one_only.push(name),
        }
    }
    let hours = newest_at.saturating_sub(older_at) / HOUR_MS;
    let landings = readings["landings"].as_i64().unwrap_or(0);
    let left_out = if one_only.is_empty() {
        String::new()
    } else {
        format!("; not measured in both readings: {}", one_only.join(", "))
    };
    if gains.is_empty() {
        return Figure {
            as_of_ms: Some(newest_at),
            ..not_measured(
                KEY,
                NAME,
                format!("not measured: no source is measured in both readings{left_out}"),
            )
        };
    }
    let by_source = Some(Value::Object(gains));
    if landings <= 0 {
        return Figure {
            as_of_ms: Some(newest_at),
            by_source,
            ..not_measured(
                KEY,
                NAME,
                format!(
                    "not measured: no landing in the {hours} h between the two readings{left_out}"
                ),
            )
        };
    }
    Figure {
        as_of_ms: Some(newest_at),
        by_source,
        note: Some(format!(
            "{landings} landing(s) in the {hours} h between the two readings (the integrator's \
             and the owner's, on every mission and source){left_out}"
        )),
        ..Figure::new(KEY, NAME, "bytes", json!(total / landings))
    }
}

/// S1, "Rebuildable bytes on disk" (objective 185): the newest reading's
/// scratch, build cache and test databases, each in `by_source`; not
/// measured when one of them is not, its note giving each one's why.
pub(super) fn rebuildable_bytes(readings: &Value) -> Figure {
    const KEY: &str = "storage.rebuildable_bytes";
    const NAME: &str = "Rebuildable bytes";
    let newest = &readings["newest"];
    let Some(newest_at) = newest["at_ms"].as_i64() else {
        return not_measured(
            KEY,
            NAME,
            "not measured yet: no storage reading yet".to_string(),
        );
    };
    let sources = &newest["sources"];
    let mut by_source = Map::new();
    let mut whys: Vec<String> = Vec::new();
    let mut total: i64 = 0;
    for name in REBUILDABLE {
        match bytes_of(sources, name) {
            Some(bytes) => {
                total = total.saturating_add(bytes);
                by_source.insert(name.to_string(), json!(bytes));
            }
            None => whys.push(format!(
                "{name}: {}",
                sources[name]["not_measured"]
                    .as_str()
                    .unwrap_or("not in the reading")
            )),
        }
    }
    if !whys.is_empty() {
        return Figure {
            as_of_ms: Some(newest_at),
            ..not_measured(KEY, NAME, format!("not measured: {}", whys.join("; ")))
        };
    }
    Figure {
        as_of_ms: Some(newest_at),
        by_source: Some(Value::Object(by_source)),
        ..Figure::new(KEY, NAME, "bytes", json!(total))
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
            "disk_fall_bytes_per_day": 2_000_000, "disk_fall_basis": "day",
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
            &adopted(&readings, false),
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
                // Objective 185: no storage readings here, no value.
                ("storage.bytes_per_landing", Value::Null),
                ("storage.rebuildable_bytes", Value::Null),
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
        // The fall says its basis (objective 222); no other figure does.
        assert_eq!(diagnostic("storage.disk_fall")["basis"], "day");
        let with_basis: Vec<&Value> = view["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .chain(
                view["lines"]
                    .as_array()
                    .expect("lines")
                    .iter()
                    .flat_map(|line| {
                        std::iter::once(&line["north_star"])
                            .chain(line["guardrails"].as_array().into_iter().flatten())
                    }),
            )
            .filter(|figure| figure.get("basis").is_some())
            .map(|figure| &figure["key"])
            .collect();
        assert_eq!(with_basis, [&json!("storage.disk_fall")]);
        // Free disk is the first diagnostic, with no line (objective 182).
        assert_eq!(view["diagnostics"][0]["key"], "storage.free_bytes");
        assert_eq!(view["diagnostics"][0]["value"], 104);
        assert_eq!(view["diagnostics"][0].get("line"), None);
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
        // No fall: no basis said (objective 222).
        let fall = figure_json(&measuring[1].guardrails[7], None);
        assert_eq!(fall["key"], "storage.disk_fall");
        assert_eq!(fall.get("basis"), None);
        let grew = figure_json(&measuring[1].guardrails[8], None);
        assert_eq!(
            (grew["value"].clone(), grew.get("store")),
            (Value::Null, None)
        );
    }

    /// A `host.storage_bytes` reading's sources as the loop records them.
    fn sources(scratch: &Value, transcripts: u64, live: &Value) -> Value {
        json!({"transcripts": {"bytes": transcripts}, "build_cache": {"bytes": 1_048_576},
               "scratch": scratch, "test_databases": {"bytes": 0, "databases": 0},
               "session_backups": {"not_measured": "no status file"}, "live_database": live})
    }

    const MIB: i64 = 1_048_576;
    const H: i64 = 3_600_000;

    /// Objective 185: the gain of the sources measured in both readings over
    /// the landings between them, each source's gain by source, the sources
    /// measured in one reading only named; not measured, with why, without
    /// two readings, a source in both or a landing.
    #[test]
    fn the_storage_north_star_is_the_days_gain_over_its_landings() {
        let star = |readings: &Value| figure_json(&super::bytes_per_landing(readings), None);
        // No reading.
        let none = star(&Value::Null);
        assert_eq!(
            [&none["value"], &none["state"]],
            [&Value::Null, &json!("not_measured_yet")]
        );
        assert_eq!(none["note"], "not measured yet: no storage reading yet");
        // One reading.
        let newest = json!({"at_ms": 48 * H, "sources": sources(&json!({"bytes": 8 * MIB}), 512, &json!({"bytes": 999}))});
        let one = star(&json!({"newest": newest, "older": null, "landings": 0}));
        assert_eq!(one["state"], "not_measured_yet");
        assert_eq!(
            one["note"],
            "not measured yet: the figure needs two storage readings a day apart"
        );
        assert_eq!(one["as_of_ms"], 48 * H);
        // Two, 47 h apart, 3 landings: 6 MiB of scratch over 3.
        let older = json!({"at_ms": H, "sources": sources(&json!({"bytes": 2 * MIB}), 512, &json!({"not_measured": "seeded"}))});
        let two = star(&json!({"newest": newest, "older": older, "landings": 3}));
        assert_eq!(two["value"], 2 * MIB);
        assert_eq!(two["state"], "no_range");
        assert_eq!(
            two["by_source"],
            json!({"build_cache": 0, "scratch": 6 * MIB, "test_databases": 0, "transcripts": 0})
        );
        assert_eq!(
            two["note"],
            "3 landing(s) in the 47 h between the two readings (the integrator's and the owner's, \
             on every mission and source); not measured in both readings: live_database, session_backups"
        );
        assert_eq!(two["as_of_ms"], 48 * H);
        // Against a range, as any figure.
        let ranged = figure_json(
            &super::bytes_per_landing(&json!({"newest": newest, "older": older, "landings": 3})),
            Some(Range {
                min: None,
                max: Some(1_048_576.0),
            }),
        );
        assert_eq!(ranged["state"], "out");
        // No landing between them: not measured, its gains still by source.
        let idle = star(&json!({"newest": newest, "older": older, "landings": 0}));
        assert_eq!(
            [&idle["value"], &idle["state"]],
            [&Value::Null, &json!("not_measured_yet")]
        );
        assert!(
            idle["note"]
                .as_str()
                .is_some_and(|note| note.starts_with("not measured: no landing in the 47 h")),
            "{idle}"
        );
        assert_eq!(idle["by_source"]["scratch"], 6 * MIB);
        // No source measured in both.
        let apart = star(
            &json!({"newest": {"at_ms": 48 * H, "sources": {"scratch": {"bytes": 1}}},
                                 "older": {"at_ms": 0, "sources": {"scratch": {"not_measured": "over"}}},
                                 "landings": 4}),
        );
        assert_eq!(apart["value"], Value::Null);
        assert_eq!(
            apart["note"],
            "not measured: no source is measured in both readings; not measured in both readings: scratch"
        );
        assert!(apart.get("by_source").is_none());
    }

    /// Objective 185: S1 is the newest reading's scratch, build cache and
    /// test databases, each by source; one of them not measured leaves S1
    /// not measured, its note giving each one's why.
    #[test]
    fn rebuildable_bytes_are_scratch_build_cache_and_test_databases() {
        let s1 = |readings: &Value| figure_json(&super::rebuildable_bytes(readings), None);
        let none = s1(&Value::Null);
        assert_eq!(
            [&none["value"], &none["state"]],
            [&Value::Null, &json!("not_measured_yet")]
        );
        let read = s1(
            &json!({"newest": {"at_ms": 7, "sources": sources(&json!({"bytes": 2 * MIB}), 512, &json!({"bytes": 999}))}}),
        );
        assert_eq!(
            read["value"],
            3 * MIB,
            "transcripts and the live database are not rebuildable"
        );
        assert_eq!(
            read["by_source"],
            json!({"build_cache": MIB, "scratch": 2 * MIB, "test_databases": 0})
        );
        assert_eq!(read["as_of_ms"], 7);
        assert!(read.get("note").is_none());
        let over = s1(&json!({"newest": {"at_ms": 7, "sources": {
            "scratch": {"not_measured": "over the bound of 1200 s (9 files read)"},
            "build_cache": {"bytes": 1}}}}));
        assert_eq!(
            [&over["value"], &over["state"]],
            [&Value::Null, &json!("not_measured_yet")]
        );
        assert_eq!(
            over["note"],
            "not measured: scratch: over the bound of 1200 s (9 files read); test_databases: not in the reading"
        );
        assert!(over.get("by_source").is_none());
    }
}
