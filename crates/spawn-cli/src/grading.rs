//! `spawn grade`: the grading catalog (docs/agent-grading.md).
//!
//! A person (the owner's login) registers each graded preset a task may be
//! routed to, onboards it with the offline exam suite (`evals/onboarding/`),
//! locks or unlocks its grade, enables or disables it, and runs calibration
//! over its recent outcomes. Every change is one `grading.recorded` event
//! (`spawn_grading_record`). The rules are `spawn_core::grading`'s; routing
//! a task to a preset happens in `spawn task assign` and `spawn task
//! reject` (crate::missions).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use spawn_core::grading::{
    CalibrationDecision, DifficultyClassification, ExamCaseResult, GradedPreset, PresetState,
    REPROBE_INTERVAL_MS, RoutingDecision, SkillGrade, TaskDifficulty, calibrate_preset,
    catalog_covers, evaluate_onboarding, is_preset_id, is_quality_rejection, registered_profile,
    reprobe_plan,
};
use spawn_postgres::PostgresStore;

use crate::{Flags, open_store};

pub(crate) const USAGE: &str = "usage:
  spawn grade register <preset_id> --engine-label <label> --model-label <label> --cost-rank <n>
                       [--model-profile default|cheap] [--execution-profile <name>]
      (a new graded preset, unexamined: never routed until onboarded. preset_id:
       [a-z0-9][a-z0-9_-]{0,63}; cost rank: 0 to 1000000, lower is cheaper; model profile:
       the --model switch its worker runs on, default unless named, and in phase 1 the only
       part of a preset a worker runs on: the two labels are for people. Execution profile:
       any name of a preset id's shape (opus-max, local-qwen, ...), the named profile the
       preset stands for, run on its model profile; carried by routing, task.assigned,
       task.rerouted, spawn grade list and /api/grading. default or cheap is that model
       profile itself)
  spawn grade list [--json]
  spawn grade exam --workspaces <dir> [--suite <dir>]
      (runs every case of the offline onboarding suite (default evals/onboarding, relative
       to the working directory) against <dir>/<case_id>, the preset's work on each case,
       each check at most 30 s, and prints the results as JSON for onboard; opens no store.
       A check that exits 2, dies by a signal or runs past 30 s is a harness fault: the
       exam stops and prints nothing to onboard with)
  spawn grade onboard <preset_id> --results <json file>
      (grades the preset on its exam results: the highest grade all of whose cases, and of
       every grade below it, passed. Without l1 it is disabled. A preset a person or
       calibration disabled is enabled first)
  spawn grade lock-grade <preset_id> --grade l1|l2|l3
  spawn grade unlock-grade <preset_id>
      (a locked grade is the one routing reads, and calibration leaves the preset alone;
       unlocking the only grade of a routed preset leaves it unexamined)
  spawn grade enable <preset_id>
  spawn grade disable <preset_id>
  spawn grade calibrate [--apply] [--json]
      (each routed preset's newest 10 closed tasks first assigned to it at its calibrated
       grade, since its window started: demoted below 60% first pass (from 5 tasks;
       disabled at l1), restored one grade toward its onboarded grade from 90% (from 8
       tasks). Only a quality rejection fails a task: a cancel without one, a merge
       conflict or an incompatible head does not. Without --apply it records nothing)
  spawn grade upkeep [--apply] [--json]
      (what the fleet loop runs every SPAWN_GRADING_UPKEEP_SECS (default 21600, 0: never):
       calibrate (--apply records its decisions, as calibrate --apply), then list the presets
       due an anti-starvation re-probe: routable, not locked, and without a task first
       assigned to them at their calibrated grade for SPAWN_GRADING_REPROBE_SECS (default
       21600), counted from their window's start. The next unflagged assignment at that
       grade goes to such a preset (reprobe), at most one per preset per interval)";

/// The onboarding suite's place in a Spawn tree.
const SUITE: &str = "evals/onboarding";

/// The longest one exam check may run.
const CASE_TIMEOUT: Duration = Duration::from_secs(30);

/// `spawn grade ...`
pub(crate) fn command(rest: &[String]) -> Result<i32, String> {
    let Some((verb, rest)) = rest.split_first() else {
        return Err(USAGE.to_string());
    };
    match verb.as_str() {
        "list" => {
            let flags = Flags::parse_with_switches(rest, &["json"])?;
            only(&flags, &[])?;
            let catalog = open_store()?
                .grading_catalog()
                .map_err(|error| error.to_string())?;
            if flags.switch("json") {
                print(&json!(catalog));
            } else {
                println!("{}", table(&catalog));
            }
        }
        "calibrate" => {
            let flags = Flags::parse_with_switches(rest, &["json", "apply"])?;
            only(&flags, &[])?;
            let store = open_store()?;
            let decided = calibrate(&store, flags.switch("apply"))?;
            if flags.switch("json") {
                print(&decided);
            } else {
                for line in decided.as_array().into_iter().flatten() {
                    println!("{}", calibration_line(line));
                }
            }
            if flags.switch("apply") {
                warn_if_default_uncovered(&store);
            }
        }
        "upkeep" => upkeep_command(rest)?,
        "exam" => {
            let flags = Flags::parse(rest)?;
            only(&flags, &["workspaces", "suite"])?;
            let suite = PathBuf::from(flags.get("suite").unwrap_or_else(|| SUITE.to_string()));
            let workspaces = PathBuf::from(flags.required("workspaces")?);
            print(&exam(&suite, &workspaces, CASE_TIMEOUT)?);
        }
        verb @ ("register" | "onboard" | "lock-grade" | "unlock-grade" | "enable" | "disable") => {
            let Some((preset_id, rest)) = rest.split_first() else {
                return Err(format!("spawn grade {verb} needs a preset id\n{USAGE}"));
            };
            if !is_preset_id(preset_id) {
                return Err(format!(
                    "a preset id is [a-z0-9][a-z0-9_-]{{0,63}}, not '{preset_id}'"
                ));
            }
            let flags = Flags::parse(rest)?;
            let store = open_store()?;
            let record = if verb == "register" {
                registered(preset_id, &flags)?
            } else {
                let catalog = store.grading_catalog().map_err(|e| e.to_string())?;
                let preset = catalog
                    .iter()
                    .find(|p| p.preset_id == *preset_id)
                    .ok_or_else(|| format!("preset {preset_id} is not in the grading catalog"))?;
                match verb {
                    "onboard" => {
                        only(&flags, &["results"])?;
                        let path = flags.required("results")?;
                        let text = std::fs::read_to_string(&path)
                            .map_err(|error| format!("read {path}: {error}"))?;
                        onboarded(preset, &text)?
                    }
                    "lock-grade" => {
                        only(&flags, &["grade"])?;
                        let text = flags.required("grade")?;
                        let grade = SkillGrade::parse(&text)
                            .ok_or_else(|| format!("--grade is l1, l2 or l3, not {text}"))?;
                        grade_locked(preset, Some(grade))?
                    }
                    "unlock-grade" => {
                        only(&flags, &[])?;
                        grade_locked(preset, None)?
                    }
                    _ => {
                        only(&flags, &[])?;
                        state_change(preset, verb == "enable")?
                    }
                }
            };
            let event = store
                .grading_record(&record)
                .map_err(|error| error.to_string())?;
            let mut answer = record;
            if let Some(map) = answer.as_object_mut() {
                map.insert("event".to_string(), json!(event));
            }
            print(&answer);
            if matches!(verb, "disable" | "unlock-grade" | "onboard") {
                warn_if_default_uncovered(&store);
            }
        }
        other => return Err(format!("unknown grade command {other}\n{USAGE}")),
    }
    Ok(0)
}

/// `spawn grade upkeep [--apply] [--json]`.
fn upkeep_command(rest: &[String]) -> Result<(), String> {
    let flags = Flags::parse_with_switches(rest, &["json", "apply"])?;
    only(&flags, &[])?;
    let store = open_store()?;
    let report = upkeep(
        &store,
        flags.switch("apply"),
        now_ms(),
        reprobe_interval_ms(),
    )?;
    if flags.switch("json") {
        print(&report);
    } else {
        for line in report["calibration"].as_array().into_iter().flatten() {
            println!("{}", calibration_line(line));
        }
        println!(
            "{}",
            upkeep_line(&report).unwrap_or_else(|| "grading upkeep: nothing due".to_string())
        );
    }
    if flags.switch("apply") {
        warn_if_default_uncovered(&store);
    }
    Ok(())
}

fn print(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
    );
}

/// Refuse any flag not in `allowed`.
fn only(flags: &Flags, allowed: &[&str]) -> Result<(), String> {
    match flags.names().find(|name| !allowed.contains(name)) {
        Some(other) => Err(format!("unknown flag --{other}\n{USAGE}")),
        None => Ok(()),
    }
}

/// Say on stderr when the catalog holds presets but none routable is
/// graded for the default difficulty: unflagged assignments then fall back
/// to `--model default` (crate::missions `placement`), which an operator
/// should notice without the fleet stopping.
fn warn_if_default_uncovered(store: &PostgresStore) {
    let default = TaskDifficulty::default();
    if let Ok(catalog) = store.grading_catalog()
        && !catalog.is_empty()
        && !catalog_covers(&catalog, default)
    {
        eprintln!(
            "spawn: agent grading: no routable preset is graded {} or above: unflagged task \
             assignments fall back to --model default (routing_fallback) until one is",
            default.required_grade()
        );
    }
}

/// The `preset.registered` record `spawn grade register` asks for.
fn registered(preset_id: &str, flags: &Flags) -> Result<Value, String> {
    only(
        flags,
        &[
            "engine-label",
            "model-label",
            "cost-rank",
            "model-profile",
            "execution-profile",
        ],
    )?;
    let engine_label = flags.required("engine-label")?;
    let model_label = flags.required("model-label")?;
    let rank_text = flags.required("cost-rank")?;
    let cost_rank: u32 = rank_text
        .parse()
        .ok()
        .filter(|rank| *rank <= 1_000_000)
        .ok_or_else(|| {
            format!("--cost-rank is a whole number from 0 to 1000000, not {rank_text}")
        })?;
    let (model_profile, execution_profile) = registered_profile(
        flags.get("model-profile").as_deref(),
        flags.get("execution-profile").as_deref(),
    )?;
    let mut record = json!({"type": "preset.registered", "preset_id": preset_id,
                            "engine_label": engine_label, "model_label": model_label,
                            "model_profile": model_profile, "cost_rank": cost_rank});
    // Phase 2: named only when it is one, so a phase-1 registration's
    // record is as it was.
    if let Some(profile) = execution_profile {
        record["execution_profile"] = json!(profile);
    }
    Ok(record)
}

/// Exam results as a file holds them: `{suite, cases: [...]}` (what
/// `spawn grade exam` prints) or the bare list of cases.
#[derive(Deserialize)]
#[serde(untagged)]
enum Results {
    Suite {
        #[serde(default)]
        suite: Option<String>,
        cases: Vec<ExamCaseResult>,
    },
    Cases(Vec<ExamCaseResult>),
}

/// The `preset.onboarded` record exam results `text` decide for `preset`
/// ([`evaluate_onboarding`]). A preset disabled with a grade (by a person
/// or by calibration) is enabled first: onboarding never overrides that.
fn onboarded(preset: &GradedPreset, text: &str) -> Result<Value, String> {
    if preset.state == PresetState::Disabled
        && (preset.onboarded_grade.is_some()
            || preset.calibrated_grade.is_some()
            || preset.locked_grade.is_some())
    {
        return Err(format!(
            "preset {0} is disabled: enable it (spawn grade enable {0}) before onboarding it again",
            preset.preset_id
        ));
    }
    let (suite, cases) = match serde_json::from_str::<Results>(text) {
        Ok(Results::Suite { suite, cases }) => (suite, cases),
        Ok(Results::Cases(cases)) => (None, cases),
        Err(error) => {
            return Err(format!(
                "exam results are {{suite, cases: [{{case_id, grade, passed}}...]}} or that list: {error}"
            ));
        }
    };
    let mut seen = std::collections::BTreeSet::new();
    if let Some(case) = cases
        .iter()
        .find(|case| !seen.insert(case.case_id.as_str()))
    {
        return Err(format!("exam case {} is named twice", case.case_id));
    }
    let verdict = evaluate_onboarding(&cases);
    Ok(
        json!({"type": "preset.onboarded", "preset_id": preset.preset_id,
              "from_state": preset.state, "to_state": verdict.state,
              "to_grade": verdict.onboarded_grade,
              "passed": verdict.passed, "total": verdict.total,
              "failed_cases": verdict.failed_cases,
              "suite": suite.unwrap_or_else(|| SUITE.to_string())}),
    )
}

/// The `preset.grade_locked` record that locks `preset` at `grade`, or
/// unlocks it (none). Unlocking the only grade of a routed preset leaves
/// it unexamined.
fn grade_locked(preset: &GradedPreset, grade: Option<SkillGrade>) -> Result<Value, String> {
    if grade == preset.locked_grade {
        return Err(match grade {
            Some(grade) => format!(
                "preset {} has locked grade {grade} already",
                preset.preset_id
            ),
            None => format!("preset {} has no locked grade", preset.preset_id),
        });
    }
    let to_state =
        if grade.is_none() && preset.calibrated_grade.is_none() && preset.state.is_routable() {
            PresetState::Unexamined
        } else {
            preset.state
        };
    Ok(
        json!({"type": "preset.grade_locked", "preset_id": preset.preset_id,
              "from_grade": preset.locked_grade, "to_grade": grade,
              "from_state": preset.state, "to_state": to_state}),
    )
}

/// The `preset.state_changed` record that enables or disables `preset`.
fn state_change(preset: &GradedPreset, enable: bool) -> Result<Value, String> {
    let to = if !enable {
        PresetState::Disabled
    } else if preset.locked_grade.or(preset.calibrated_grade).is_none() {
        return Err(format!(
            "preset {} has no grade: onboard it or lock its grade before enabling it",
            preset.preset_id
        ));
    } else if preset.calibrated_grade.is_none() || preset.calibrated_grade == preset.onboarded_grade
    {
        PresetState::Active
    } else {
        PresetState::Demoted
    };
    if preset.state == to {
        return Err(format!("preset {} is {to} already", preset.preset_id));
    }
    Ok(
        json!({"type": "preset.state_changed", "preset_id": preset.preset_id,
              "from_state": preset.state, "to_state": to}),
    )
}

/// Calibrate every preset in the catalog: its decision and the window it
/// read; with `apply`, each decision that is not a hold recorded
/// (`preset.regraded`). A refusal part way names the presets already
/// recorded.
fn calibrate(store: &PostgresStore, apply: bool) -> Result<Value, String> {
    let catalog = store.grading_catalog().map_err(|error| error.to_string())?;
    let mut decided = Vec::new();
    let mut recorded: Vec<&str> = Vec::new();
    for preset in &catalog {
        let outcomes = match preset.calibrated_grade {
            Some(grade) if preset.locked_grade.is_none() && preset.state.is_routable() => store
                .grading_outcomes(
                    &preset.preset_id,
                    grade.difficulty(),
                    preset.window_start_ms,
                )
                .map_err(|error| error.to_string())?,
            _ => Vec::new(),
        };
        let decision = calibrate_preset(preset, &outcomes);
        let event = match (&decision, apply) {
            (CalibrationDecision::Hold { .. }, _) | (_, false) => None,
            (decision, true) => Some(
                store
                    .grading_record(&regraded_record(preset, decision))
                    .map_err(|error| {
                        format!(
                            "calibration stopped at preset {}: {error} (recorded before it: {})",
                            preset.preset_id,
                            if recorded.is_empty() {
                                "none".to_string()
                            } else {
                                recorded.join(", ")
                            }
                        )
                    })?,
            ),
        };
        if event.is_some() {
            recorded.push(&preset.preset_id);
        }
        let mut line = json!({"preset_id": preset.preset_id,
                              "window": outcomes.len(), "applied": event.is_some(),
                              "event": event});
        if let (Some(map), Value::Object(fields)) = (
            line.as_object_mut(),
            serde_json::to_value(&decision).unwrap_or_default(),
        ) {
            map.extend(fields);
        }
        decided.push(line);
    }
    Ok(Value::Array(decided))
}

/// The `preset.regraded` record of a calibration decision (never a hold).
fn regraded_record(preset: &GradedPreset, decision: &CalibrationDecision) -> Value {
    let (name, from_grade, first_pass, total) = match decision {
        CalibrationDecision::Demote {
            from_grade,
            first_pass,
            total,
            ..
        } => ("demote", from_grade, first_pass, total),
        CalibrationDecision::Restore {
            from_grade,
            first_pass,
            total,
            ..
        } => ("restore", from_grade, first_pass, total),
        CalibrationDecision::Disable {
            from_grade,
            first_pass,
            total,
        } => ("disable", from_grade, first_pass, total),
        CalibrationDecision::Hold { .. } => unreachable!("a hold records nothing"),
    };
    let (to_grade, to_state) = decision
        .outcome(preset)
        .unwrap_or((*from_grade, preset.state));
    json!({"type": "preset.regraded", "preset_id": preset.preset_id, "decision": name,
           "from_grade": from_grade, "to_grade": to_grade, "from_state": preset.state,
           "to_state": to_state, "first_pass": first_pass, "total": total})
}

fn calibration_line(line: &Value) -> String {
    let text = |key: &str| line[key].as_str().unwrap_or("-").to_string();
    let detail = match line["decision"].as_str() {
        Some("hold") => format!("hold ({})", text("reason")),
        Some(decision) => format!(
            "{decision} {} -> {} ({}/{} first pass){}",
            text("from_grade"),
            if decision == "disable" {
                "off".to_string()
            } else {
                text("to_grade")
            },
            line["first_pass"],
            line["total"],
            if line["applied"] == json!(true) {
                ", applied"
            } else {
                ", not applied (--apply)"
            }
        ),
        None => "-".to_string(),
    };
    format!("{}: {detail}", text("preset_id"))
}

/// This host's clock in epoch milliseconds.
pub(crate) fn now_ms() -> i64 {
    spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0
}

/// The anti-starvation re-probe's interval (`SPAWN_GRADING_REPROBE_SECS`,
/// whole seconds, default [`REPROBE_INTERVAL_MS`]; `0`: never), in ms, and
/// the warning a setting that is no whole number of seconds gets.
fn reprobe_interval_from(text: Option<&str>) -> (i64, Option<String>) {
    let Some(text) = text else {
        return (REPROBE_INTERVAL_MS, None);
    };
    match text.trim().parse::<u64>() {
        Ok(0) => (i64::MAX, None),
        Ok(secs) => (
            i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX),
            None,
        ),
        Err(_) => (
            REPROBE_INTERVAL_MS,
            Some(format!(
                "SPAWN_GRADING_REPROBE_SECS: {text:?} is not a whole number of seconds; using {}",
                REPROBE_INTERVAL_MS / 1000
            )),
        ),
    }
}

/// [`reprobe_interval_from`] the environment, saying on stderr (once a
/// process) when the setting is no whole number of seconds (the default is
/// used), as the loop says of `SPAWN_GRADING_UPKEEP_SECS`.
pub(crate) fn reprobe_interval_ms() -> i64 {
    // Once a process: `spawn serve` reads it on every `/api/grading`.
    static WARNED: std::sync::Once = std::sync::Once::new();
    let (ms, warning) =
        reprobe_interval_from(crate::config::env_value("SPAWN_GRADING_REPROBE_SECS").as_deref());
    if let Some(warning) = warning {
        WARNED.call_once(|| eprintln!("spawn: agent grading: {warning}"));
    }
    ms
}

/// What `task.assigned` records of the difficulty classifier
/// (`classification`): its decision, its rules and their evidence, the
/// line a person reads, and the difficulty the task was routed at when
/// the catalog could not take the classified one.
pub(crate) fn classification_record(
    classified: &DifficultyClassification,
    decision: &RoutingDecision,
) -> Value {
    let mut record = json!(classified);
    record["rationale"] = json!(classified.rationale());
    if decision.difficulty != classified.difficulty {
        record["routed_at"] = json!(decision.difficulty);
    }
    record
}

/// The step-up a gate's verdict on `task` gives its rework, if any: only a
/// quality rejection ([`is_quality_rejection`], `rejected`) of a task
/// routed through the catalog whose stepped-up difficulty its preset no
/// longer takes, judged in a state a gate judges (`reported`, or
/// `accepted` before its gate). The same rule as `spawn task reject`
/// (crate::missions `quality_reroute`); the integrator records it in the
/// verdict's own statement.
pub(crate) fn gate_reroute(
    store: &PostgresStore,
    task: i64,
    verdict: &str,
) -> Result<Option<crate::missions::ReworkReroute>, String> {
    if !is_quality_rejection(verdict) {
        return Ok(None);
    }
    crate::missions::quality_reroute(store, task, &["reported", "accepted"])
}

/// The fleet loop's grading upkeep (`SPAWN_GRADING_UPKEEP_SECS`): on its
/// own clock, [`upkeep`] with `--apply`. Bounded: one run a period, each a
/// read of the catalog and one window per routed preset, and at most one
/// record per preset; idempotent: a regraded preset's window starts afresh,
/// so the next run holds it, and a re-probe is a fact of the tasks, not a
/// record. A failed run is tried again a minute later; it never fails a
/// pass.
pub(crate) struct Upkeep {
    /// The period, in ms; `0`: never.
    every_ms: i64,
    /// When the next run is due.
    due_ms: i64,
}

/// The default period of the grading upkeep: 6 hours.
pub(crate) const UPKEEP_EVERY_SECS: u64 = 6 * 60 * 60;

/// The retry of a failed upkeep run.
const UPKEEP_RETRY_MS: i64 = 60_000;

impl Upkeep {
    /// The upkeep `text` (`SPAWN_GRADING_UPKEEP_SECS`) sets, first due at
    /// `now_ms`, and the warning a setting that is no whole number of
    /// seconds gets.
    pub(crate) fn from_setting(text: Option<&str>, now_ms: i64) -> (Self, Option<String>) {
        let (secs, warning) = match text.map(|text| (text, text.trim().parse::<u64>())) {
            None => (UPKEEP_EVERY_SECS, None),
            Some((_, Ok(secs))) => (secs, None),
            Some((text, Err(_))) => (
                UPKEEP_EVERY_SECS,
                Some(format!(
                    "SPAWN_GRADING_UPKEEP_SECS: {text:?} is not a whole number of seconds; \
                     using {UPKEEP_EVERY_SECS}"
                )),
            ),
        };
        (
            Self {
                every_ms: i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX),
                due_ms: now_ms,
            },
            warning,
        )
    }

    /// [`Self::from_setting`] the environment.
    pub(crate) fn from_env() -> (Self, Option<String>) {
        Self::from_setting(
            crate::config::env_value("SPAWN_GRADING_UPKEEP_SECS").as_deref(),
            now_ms(),
        )
    }

    /// Whether a run is due at `now_ms`.
    pub(crate) fn due(&self, now_ms: i64) -> bool {
        self.every_ms > 0 && now_ms >= self.due_ms
    }

    /// Run [`upkeep`] when due: its line when it changed something or
    /// found a preset due a re-probe, none otherwise.
    pub(crate) fn check(
        &mut self,
        store: &PostgresStore,
        now_ms: i64,
    ) -> Result<Option<String>, String> {
        if !self.due(now_ms) {
            return Ok(None);
        }
        match upkeep(store, true, now_ms, reprobe_interval_ms()) {
            Ok(report) => {
                self.due_ms = now_ms.saturating_add(self.every_ms);
                Ok(upkeep_line(&report))
            }
            Err(error) => {
                self.due_ms = now_ms.saturating_add(UPKEEP_RETRY_MS.min(self.every_ms));
                Err(format!("grading upkeep: {error}"))
            }
        }
    }
}

/// Calibrate the catalog (recording each decision with `apply`) and list
/// the presets due a re-probe at `now_ms` after it
/// (`spawn_core::grading::reprobe_plan`): `{calibration, reprobe_due,
/// uncovered}`, `uncovered` true when the catalog holds presets but none
/// routable is graded for the default difficulty. An empty catalog is one
/// read.
pub(crate) fn upkeep(
    store: &PostgresStore,
    apply: bool,
    now_ms: i64,
    interval_ms: i64,
) -> Result<Value, String> {
    let catalog = store.grading_catalog().map_err(|error| error.to_string())?;
    if catalog.is_empty() {
        return Ok(json!({"calibration": [], "reprobe_due": [], "uncovered": false}));
    }
    let calibration = calibrate(store, apply)?;
    let catalog = if apply {
        store.grading_catalog().map_err(|error| error.to_string())?
    } else {
        catalog
    };
    let activity = store
        .grading_activity()
        .map_err(|error| error.to_string())?;
    Ok(json!({
        "calibration": calibration,
        "reprobe_due": reprobe_plan(&catalog, &activity, now_ms, interval_ms),
        "uncovered": !catalog_covers(&catalog, TaskDifficulty::default()),
    }))
}

/// One line of an [`upkeep`] report: what calibration applied, the presets
/// due a re-probe and an uncovered default; none when it says nothing.
fn upkeep_line(report: &Value) -> Option<String> {
    let applied: Vec<String> = report["calibration"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|line| line["applied"] == json!(true))
        .map(calibration_line)
        .collect();
    let due: Vec<&str> = report["reprobe_due"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut parts = Vec::new();
    if !applied.is_empty() {
        parts.push(format!("calibrated {}", applied.join("; ")));
    }
    if !due.is_empty() {
        parts.push(format!("re-probe due: {}", due.join(", ")));
    }
    if report["uncovered"] == json!(true) {
        parts.push(format!(
            "no routable preset is graded {}: unflagged assignments fall back to --model default",
            TaskDifficulty::default().required_grade()
        ));
    }
    (!parts.is_empty()).then(|| format!("grading upkeep: {}", parts.join("; ")))
}

/// The catalog as `/api/grading` answers it: `{presets, profiles}`, each
/// preset as [`GradedPreset`] serializes (so every phase-1 field is there
/// as it was) with `routing_grade`, `profile` (its
/// [`GradedPreset::profile_name`]), `driver` and `label_only` (its
/// [`GradedPreset::driver_id`]: the engine its worker runs, and whether its
/// labels are for people only) and `reprobe_due` at `now_ms`; every
/// profile name the catalog runs as, sorted, once.
pub(crate) fn api_catalog(
    catalog: &[GradedPreset],
    activity: &[spawn_core::grading::PresetActivity],
    now_ms: i64,
    interval_ms: i64,
) -> Value {
    let presets: Vec<Value> = catalog
        .iter()
        .map(|preset| {
            let mut value = json!(preset);
            value["routing_grade"] = json!(preset.routing_grade());
            value["profile"] = json!(preset.profile_name());
            let driver = preset.driver_id();
            value["driver"] = json!(driver.driver);
            value["label_only"] = json!(driver.label_only);
            value["reprobe_due"] = json!(spawn_core::grading::reprobe_due(
                preset,
                activity,
                now_ms,
                interval_ms
            ));
            value
        })
        .collect();
    let profiles: std::collections::BTreeSet<&str> =
        catalog.iter().map(GradedPreset::profile_name).collect();
    json!({"presets": presets, "profiles": profiles})
}

/// The catalog as a table.
fn table(catalog: &[GradedPreset]) -> String {
    let grade = |grade: Option<SkillGrade>| grade.map_or("-", SkillGrade::as_str);
    let mut lines = vec![format!(
        "{:<24} {:<10} {:<7} {:<16} {:>4} {:<5} {:<5} {:<5} {:<5} {}",
        "PRESET",
        "STATE",
        "MODEL",
        "PROFILE",
        "COST",
        "ONB",
        "CAL",
        "LOCK",
        "ROUTE",
        "ENGINE/MODEL LABEL"
    )];
    for p in catalog {
        lines.push(format!(
            "{:<24} {:<10} {:<7} {:<16} {:>4} {:<5} {:<5} {:<5} {:<5} {}/{}",
            p.preset_id,
            p.state.as_str(),
            p.model_profile.as_str(),
            p.profile_name(),
            p.cost_rank,
            grade(p.onboarded_grade),
            grade(p.calibrated_grade),
            grade(p.locked_grade),
            grade(p.routing_grade()),
            p.engine_label,
            p.model_label
        ));
    }
    if catalog.is_empty() {
        lines.push("(no presets: spawn task assign keeps its --model behaviour)".to_string());
    }
    lines.join("\n")
}

/// The onboarding suite's cases: each directory of `suite` named
/// `l<n>-<name>` holding a `check.sh`, by name, with the grade it
/// examines.
fn exam_cases(suite: &Path) -> Result<Vec<(String, SkillGrade, PathBuf)>, String> {
    let entries =
        std::fs::read_dir(suite).map_err(|error| format!("read {}: {error}", suite.display()))?;
    let mut cases = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().to_string();
        let check = entry.path().join("check.sh");
        let Some(grade) = name.split('-').next().and_then(SkillGrade::parse) else {
            continue;
        };
        if check.is_file() {
            cases.push((name, grade, check));
        }
    }
    cases.sort();
    if cases.is_empty() {
        return Err(format!(
            "{} holds no exam case (l1-*/check.sh ...)",
            suite.display()
        ));
    }
    Ok(cases)
}

/// Run one check, `sh <check> <workspace>`, in a bare environment, in a
/// process group of its own, for at most `timeout`: its exit code, or why
/// it has none (a signal, the time limit). The whole group is killed when
/// it ends however it ends, so nothing a candidate's script started
/// outlives the case.
fn run_check(check: &Path, workspace: &Path, timeout: Duration) -> Result<i32, String> {
    use std::os::unix::process::CommandExt as _;
    let deadline = Instant::now() + timeout;
    let mut child = Command::new("sh")
        .arg(check)
        .arg(workspace)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("run {}: {error}", check.display()))?;
    let group = rustix::process::Pid::from_child(&child);
    let ended = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                break Err(format!("ran past {} s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => break Err(format!("wait: {error}")),
        }
    };
    let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    let _ = child.kill();
    let _ = child.wait();
    ended?.code().ok_or_else(|| "ended by a signal".to_string())
}

/// Run every case of `suite` against `workspaces/<case_id>`, each check at
/// most `timeout` ([`run_check`]): exit 0 passes, 1 fails. Anything else (2,
/// the checks' harness fault; a signal; the time limit) is a harness fault,
/// not the preset's: the exam stops with an error and grades nothing.
/// Prints `{suite, cases: [{case_id, grade, passed, exit_code}]}`.
pub(crate) fn exam(suite: &Path, workspaces: &Path, timeout: Duration) -> Result<Value, String> {
    if !workspaces.is_dir() {
        return Err(format!(
            "--workspaces {} is not a directory",
            workspaces.display()
        ));
    }
    let mut results = Vec::new();
    for (case_id, grade, check) in exam_cases(suite)? {
        let workspace = workspaces.join(&case_id);
        let fault = |why: String| {
            format!("exam case {case_id}: its check {why}: a harness fault, the exam stopped")
        };
        let code = run_check(&check, &workspace, timeout).map_err(fault)?;
        let passed = match code {
            0 => workspace.is_dir(),
            1 => false,
            other => return Err(fault(format!("exited {other}"))),
        };
        results.push(json!({"case_id": case_id, "grade": grade, "passed": passed,
                            "exit_code": code}));
    }
    Ok(json!({"suite": suite.display().to_string(), "cases": results}))
}

#[cfg(test)]
mod tests {
    use spawn_core::event::ModelProfile;

    use super::*;

    fn flags(words: &[&str]) -> Flags {
        Flags::parse(&words.iter().map(ToString::to_string).collect::<Vec<_>>()).unwrap()
    }

    fn graded_preset(state: PresetState, onboarded: Option<SkillGrade>) -> GradedPreset {
        GradedPreset {
            preset_id: "p".to_string(),
            engine_label: "claude".to_string(),
            model_label: "m".to_string(),
            model_profile: ModelProfile::Default,
            execution_profile: None,
            cost_rank: 1,
            state,
            onboarded_grade: onboarded,
            calibrated_grade: onboarded,
            locked_grade: None,
            window_start_ms: 0,
            created_at_ms: 0,
            updated_at_ms: 0,
            last_event: 0,
        }
    }

    #[test]
    fn register_builds_a_checked_record() {
        let record = registered(
            "claude-l3",
            &flags(&[
                "--engine-label",
                "claude",
                "--model-label",
                "opus",
                "--cost-rank",
                "3",
            ]),
        )
        .unwrap();
        assert_eq!(
            record,
            json!({"type": "preset.registered", "preset_id": "claude-l3",
                   "engine_label": "claude", "model_label": "opus",
                   "model_profile": "default", "cost_rank": 3})
        );
        let cheap = registered(
            "ds",
            &flags(&[
                "--engine-label",
                "claude",
                "--model-label",
                "deepseek",
                "--cost-rank",
                "0",
                "--model-profile",
                "cheap",
            ]),
        )
        .unwrap();
        assert_eq!(cheap["model_profile"], json!("cheap"));
        for bad in [
            &[
                "--engine-label",
                "e",
                "--model-label",
                "m",
                "--cost-rank",
                "-1",
            ][..],
            &[
                "--engine-label",
                "e",
                "--model-label",
                "m",
                "--cost-rank",
                "1000001",
            ],
            &["--engine-label", "e", "--model-label", "m"],
            &[
                "--engine-label",
                "e",
                "--model-label",
                "m",
                "--cost-rank",
                "1",
                "--model-profile",
                "x",
            ],
            &["--engine", "e", "--model-label", "m", "--cost-rank", "1"],
        ] {
            assert!(registered("p", &flags(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn register_names_an_execution_profile_beside_its_model_profile() {
        let base = [
            "--engine-label",
            "claude",
            "--model-label",
            "opus",
            "--cost-rank",
            "3",
        ];
        let with = |more: &[&str]| {
            let mut words = base.to_vec();
            words.extend_from_slice(more);
            registered("p", &flags(&words))
        };
        let named = with(&["--execution-profile", "opus-max"]).unwrap();
        assert_eq!(
            named,
            json!({"type": "preset.registered", "preset_id": "p", "engine_label": "claude",
                   "model_label": "opus", "model_profile": "default", "cost_rank": 3,
                   "execution_profile": "opus-max"})
        );
        let on_cheap = with(&[
            "--execution-profile",
            "local-qwen",
            "--model-profile",
            "cheap",
        ])
        .unwrap();
        assert_eq!(on_cheap["model_profile"], json!("cheap"));
        assert_eq!(on_cheap["execution_profile"], json!("local-qwen"));
        // A builtin name is the model profile, and no execution profile.
        let builtin = with(&["--execution-profile", "cheap"]).unwrap();
        assert_eq!(builtin["model_profile"], json!("cheap"));
        assert!(builtin.get("execution_profile").is_none(), "{builtin}");
        for bad in [
            &["--execution-profile", "Opus Max"][..],
            &["--execution-profile", "cheap", "--model-profile", "default"],
            &["--execution-profile", ""],
        ] {
            assert!(with(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_reprobe_interval_reads_whole_seconds_and_zero_turns_it_off() {
        assert_eq!(reprobe_interval_from(None), (REPROBE_INTERVAL_MS, None));
        assert_eq!(reprobe_interval_from(Some("60")), (60_000, None));
        assert_eq!(reprobe_interval_from(Some(" 5 ")), (5_000, None));
        assert_eq!(reprobe_interval_from(Some("0")), (i64::MAX, None));
        let (ms, warning) = reprobe_interval_from(Some("soon"));
        assert_eq!(ms, REPROBE_INTERVAL_MS);
        assert!(warning.unwrap().contains("SPAWN_GRADING_REPROBE_SECS"));
    }

    #[test]
    fn the_upkeep_runs_on_its_own_clock_and_zero_turns_it_off() {
        let (upkeep, warning) = Upkeep::from_setting(None, 1_000);
        assert!(warning.is_none());
        assert_eq!(upkeep.every_ms, 21_600_000);
        assert!(!upkeep.due(999));
        assert!(upkeep.due(1_000));
        let (off, _) = Upkeep::from_setting(Some("0"), 0);
        assert!(!off.due(i64::MAX));
        let (bad, warning) = Upkeep::from_setting(Some("hourly"), 0);
        assert_eq!(bad.every_ms, 21_600_000);
        assert!(warning.unwrap().contains("SPAWN_GRADING_UPKEEP_SECS"));
        let (short, _) = Upkeep::from_setting(Some("30"), 0);
        assert_eq!(short.every_ms, 30_000);
    }

    #[test]
    fn an_upkeep_line_says_only_what_changed_or_is_due() {
        let quiet = json!({"calibration": [
            {"preset_id": "a", "decision": "hold", "reason": "within band", "applied": false}],
            "reprobe_due": [], "uncovered": false});
        assert_eq!(upkeep_line(&quiet), None);
        let busy = json!({"calibration": [
            {"preset_id": "a", "decision": "demote", "from_grade": "l2", "to_grade": "l1",
             "first_pass": 1, "total": 5, "applied": true}],
            "reprobe_due": ["b", "c"], "uncovered": true});
        let line = upkeep_line(&busy).unwrap();
        assert!(
            line.starts_with("grading upkeep: calibrated a: demote l2 -> l1"),
            "{line}"
        );
        assert!(line.contains("re-probe due: b, c"), "{line}");
        assert!(line.contains("fall back to --model default"), "{line}");
    }

    #[test]
    fn the_api_catalog_keeps_every_phase_one_field_and_names_each_profile() {
        let mut plain = graded_preset(PresetState::Active, Some(SkillGrade::L2));
        plain.window_start_ms = 0;
        let mut named = graded_preset(PresetState::Active, Some(SkillGrade::L3));
        named.preset_id = "q".to_string();
        named.execution_profile = spawn_core::grading::ExecutionProfile::parse("opus-max");
        named.window_start_ms = 500;
        let answer = api_catalog(&[plain.clone(), named], &[], 1_000, 1_000);
        let presets = answer["presets"].as_array().unwrap();
        let first = presets[0].as_object().unwrap();
        for (key, value) in json!(plain).as_object().unwrap() {
            assert_eq!(first.get(key), Some(value), "{key}");
        }
        assert_eq!(presets[0]["profile"], json!("default"));
        assert_eq!(presets[0]["routing_grade"], json!("l2"));
        assert_eq!(presets[0]["reprobe_due"], json!(true));
        assert_eq!(presets[1]["profile"], json!("opus-max"));
        assert_eq!(presets[1]["execution_profile"], json!("opus-max"));
        assert_eq!(presets[1]["reprobe_due"], json!(false));
        assert_eq!(answer["profiles"], json!(["default", "opus-max"]));
    }

    #[test]
    fn the_api_catalog_names_each_presets_driver_and_whether_its_labels_are_only_labels() {
        let claude = graded_preset(PresetState::Active, Some(SkillGrade::L2));
        let mut agy = claude.clone();
        agy.preset_id = "agy-flash".to_string();
        agy.engine_label = "agy".to_string();
        let mut codex = claude.clone();
        codex.preset_id = "codex".to_string();
        codex.engine_label = "codex".to_string();
        let answer = api_catalog(&[claude, agy, codex], &[], 1_000, 1_000);
        let presets = answer["presets"].as_array().unwrap();
        let pairs: Vec<(Value, Value)> = presets
            .iter()
            .map(|preset| (preset["driver"].clone(), preset["label_only"].clone()))
            .collect();
        assert_eq!(
            pairs,
            [
                (json!("claude"), json!(false)),
                (json!("agy"), json!(false)),
                (json!("claude"), json!(true)),
            ]
        );
    }

    #[test]
    fn a_classification_record_carries_its_rationale_and_where_it_ran() {
        let classified = spawn_core::grading::classify_difficulty("Fix the authentication bypass");
        let catalog = vec![graded_preset(PresetState::Active, Some(SkillGrade::L2))];
        let decision =
            spawn_core::grading::route_preset(TaskDifficulty::L2, 0, None, &catalog).unwrap();
        let record = classification_record(&classified, &decision);
        assert_eq!(record["difficulty"], json!("l3"));
        assert_eq!(record["routed_at"], json!("l2"));
        assert_eq!(record["rules"], json!("brief-v2"));
        assert!(
            record["rationale"]
                .as_str()
                .unwrap()
                .starts_with("l3 (score 4")
        );
        assert_eq!(record["signals"][0]["rule"], json!("security"));
        let neutral = spawn_core::grading::classify_difficulty("a");
        let record = classification_record(&neutral, &decision);
        assert!(record.get("routed_at").is_none(), "{record}");
        assert_eq!(record["signals"], json!([]));
    }

    #[test]
    fn onboarding_reads_both_result_shapes_and_refuses_a_case_twice() {
        let fresh = graded_preset(PresetState::Unexamined, None);
        let suite = r#"{"suite": "evals/onboarding", "cases": [
            {"case_id": "l1-a", "grade": "l1", "passed": true, "exit_code": 0},
            {"case_id": "l2-b", "grade": "l2", "passed": true},
            {"case_id": "l3-c", "grade": "l3", "passed": false}]}"#;
        let record = onboarded(&fresh, suite).unwrap();
        assert_eq!(record["to_grade"], json!("l2"));
        assert_eq!(record["from_state"], json!("unexamined"));
        assert_eq!(record["to_state"], json!("active"));
        assert_eq!(record["failed_cases"], json!(["l3-c"]));
        assert_eq!(
            (record["passed"].clone(), record["total"].clone()),
            (json!(2), json!(3))
        );
        let bare = r#"[{"case_id": "l1-a", "grade": "l1", "passed": false}]"#;
        let record = onboarded(&fresh, bare).unwrap();
        assert_eq!(record["to_grade"], json!(null));
        assert_eq!(record["to_state"], json!("disabled"));
        assert_eq!(record["suite"], json!(SUITE));
        let twice = r#"[{"case_id": "a", "grade": "l1", "passed": true},
                        {"case_id": "a", "grade": "l1", "passed": true}]"#;
        assert!(
            onboarded(&fresh, twice)
                .unwrap_err()
                .contains("named twice")
        );
        assert!(
            onboarded(
                &fresh,
                r#"{"cases": [{"case_id": "a", "grade": "l9", "passed": true}]}"#
            )
            .is_err()
        );
        // A disabled preset with a grade is enabled first; one that failed
        // l1 is examined again.
        let disabled = graded_preset(PresetState::Disabled, Some(SkillGrade::L2));
        assert!(
            onboarded(&disabled, bare)
                .unwrap_err()
                .contains("enable it")
        );
        let failed = graded_preset(PresetState::Disabled, None);
        assert_eq!(
            onboarded(&failed, suite).unwrap()["from_state"],
            json!("disabled")
        );
    }

    #[test]
    fn enabling_and_disabling_say_where_the_preset_goes() {
        let active = graded_preset(PresetState::Active, Some(SkillGrade::L2));
        assert_eq!(
            state_change(&active, false).unwrap(),
            json!({"type": "preset.state_changed", "preset_id": "p",
                   "from_state": "active", "to_state": "disabled"})
        );
        assert!(
            state_change(&active, true)
                .unwrap_err()
                .contains("active already")
        );
        let mut demoted_off = graded_preset(PresetState::Disabled, Some(SkillGrade::L3));
        demoted_off.calibrated_grade = Some(SkillGrade::L1);
        assert_eq!(
            state_change(&demoted_off, true).unwrap()["to_state"],
            json!("demoted")
        );
        let failed = graded_preset(PresetState::Disabled, None);
        assert!(
            state_change(&failed, true)
                .unwrap_err()
                .contains("no grade")
        );
        let mut locked = failed.clone();
        locked.locked_grade = Some(SkillGrade::L1);
        assert_eq!(
            state_change(&locked, true).unwrap()["to_state"],
            json!("active")
        );
    }

    #[test]
    fn unlocking_the_only_grade_of_a_routed_preset_leaves_it_unexamined() {
        let mut locked = graded_preset(PresetState::Active, None);
        locked.locked_grade = Some(SkillGrade::L2);
        assert_eq!(
            grade_locked(&locked, None).unwrap(),
            json!({"type": "preset.grade_locked", "preset_id": "p", "from_grade": "l2",
                   "to_grade": null, "from_state": "active", "to_state": "unexamined"})
        );
        let mut graded = graded_preset(PresetState::Active, Some(SkillGrade::L1));
        graded.locked_grade = Some(SkillGrade::L3);
        assert_eq!(
            grade_locked(&graded, None).unwrap()["to_state"],
            json!("active")
        );
        assert!(
            grade_locked(&graded, Some(SkillGrade::L3))
                .unwrap_err()
                .contains("already")
        );
        let fresh = graded_preset(PresetState::Unexamined, None);
        assert!(
            grade_locked(&fresh, None)
                .unwrap_err()
                .contains("no locked grade")
        );
        assert_eq!(
            grade_locked(&fresh, Some(SkillGrade::L1)).unwrap()["to_state"],
            json!("unexamined")
        );
    }

    #[test]
    fn calibration_records_name_the_step_and_the_state_after_it() {
        let mut p = graded_preset(PresetState::Demoted, Some(SkillGrade::L3));
        p.calibrated_grade = Some(SkillGrade::L2);
        let restore = CalibrationDecision::Restore {
            from_grade: SkillGrade::L2,
            to_grade: SkillGrade::L3,
            first_pass: 9,
            total: 10,
        };
        assert_eq!(
            regraded_record(&p, &restore),
            json!({"type": "preset.regraded", "preset_id": "p", "decision": "restore",
                   "from_grade": "l2", "to_grade": "l3", "from_state": "demoted",
                   "to_state": "active", "first_pass": 9, "total": 10})
        );
        let disable = CalibrationDecision::Disable {
            from_grade: SkillGrade::L1,
            first_pass: 1,
            total: 5,
        };
        let record = regraded_record(&p, &disable);
        assert_eq!(
            (record["to_grade"].clone(), record["to_state"].clone()),
            (json!("l1"), json!("disabled"))
        );
    }

    /// A fresh directory under the test's temporary directory.
    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("spawn-exam-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// The suite in the tree: each case's check passes its reference
    /// solution and fails an empty workspace, and the exam runner grades a
    /// preset whose work is the references l3.
    #[test]
    fn the_onboarding_suite_passes_its_references_and_fails_an_empty_workspace() {
        let suite = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/onboarding");
        let cases = exam_cases(&suite).unwrap();
        let grades: std::collections::BTreeSet<SkillGrade> =
            cases.iter().map(|(_, grade, _)| *grade).collect();
        assert_eq!(grades.len(), 3, "a case per grade: {cases:?}");
        let root = scratch("suite");
        let references = root.join("references");
        let empty = root.join("empty");
        for (case_id, _, check) in &cases {
            let reference = check.parent().unwrap().join("reference");
            std::fs::create_dir_all(references.join(case_id)).unwrap();
            for file in std::fs::read_dir(&reference).unwrap() {
                let file = file.unwrap();
                std::fs::copy(file.path(), references.join(case_id).join(file.file_name()))
                    .unwrap();
            }
            std::fs::create_dir_all(empty.join(case_id)).unwrap();
        }
        let passed = exam(&suite, &references, CASE_TIMEOUT).unwrap();
        let failed = exam(&suite, &empty, CASE_TIMEOUT).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        for case in passed["cases"].as_array().unwrap() {
            assert_eq!(case["passed"], json!(true), "{case}");
            assert_eq!(case["exit_code"], json!(0), "{case}");
        }
        for case in failed["cases"].as_array().unwrap() {
            assert_eq!(case["passed"], json!(false), "{case}");
            assert_eq!(case["exit_code"], json!(1), "{case}");
        }
        let fresh = graded_preset(PresetState::Unexamined, None);
        let record = onboarded(&fresh, &passed.to_string()).unwrap();
        assert_eq!(record["to_grade"], json!("l3"));
        let record = onboarded(&fresh, &failed.to_string()).unwrap();
        assert_eq!(record["to_state"], json!("disabled"));
    }

    /// A harness fault is never the preset's failure: a check that exits
    /// 2, dies by a signal or runs past its time stops the exam with an
    /// error, and a slow check is killed with everything it started.
    #[test]
    fn an_exam_stops_on_a_harness_fault_and_bounds_each_check() {
        let root = scratch("faults");
        let workspaces = root.join("work");
        let case = |suite: &Path, name: &str, body: &str| {
            std::fs::create_dir_all(suite.join(name)).unwrap();
            std::fs::write(suite.join(name).join("check.sh"), body).unwrap();
            std::fs::create_dir_all(workspaces.join(name)).unwrap();
        };
        let failing = root.join("failing");
        case(&failing, "l1-fails", "exit 1\n");
        let result = exam(&failing, &workspaces, CASE_TIMEOUT).unwrap();
        assert_eq!(result["cases"][0]["passed"], json!(false));
        assert_eq!(result["cases"][0]["exit_code"], json!(1));

        let broken = root.join("broken");
        case(&broken, "l1-broken", "exit 2\n");
        let error = exam(&broken, &workspaces, CASE_TIMEOUT).unwrap_err();
        assert!(
            error.contains("l1-broken") && error.contains("exited 2"),
            "{error}"
        );
        assert!(error.contains("harness fault"), "{error}");

        let signalled = root.join("signalled");
        case(&signalled, "l1-killed", "kill -KILL $$\n");
        let error = exam(&signalled, &workspaces, CASE_TIMEOUT).unwrap_err();
        assert!(error.contains("ended by a signal"), "{error}");

        // A check that starts a child and loops: killed at the limit, with
        // the child, and said.
        let slow = root.join("slow");
        let marker = root.join("still-running");
        case(
            &slow,
            "l1-slow",
            &format!(
                "(sleep 3; : > '{}') &\nwhile :; do sleep 1; done\n",
                marker.display()
            ),
        );
        let started = Instant::now();
        let error = exam(&slow, &workspaces, Duration::from_secs(1)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(3), "{error}");
        assert!(error.contains("ran past 1 s"), "{error}");
        std::thread::sleep(Duration::from_secs(3));
        assert!(!marker.exists(), "a child of the killed check outlived it");
        std::fs::remove_dir_all(&root).unwrap();
    }
}

/// Routing, rework reroutes and calibration through the CLI's own paths,
/// against the test database (`SPAWN_TEST_DATABASE_URL`; like the
/// spawn-postgres tests, they do not skip), each in a schema of its own.
#[cfg(test)]
mod database_tests {
    use serde_json::{Value, json};
    use spawn_core::grading::{PresetState, SkillGrade};
    use spawn_postgres::PostgresStore;
    use spawn_postgres::testing::TestSchema;

    use super::calibrate;
    use crate::Flags;
    use crate::missions::{assign, reject};

    fn flags(words: &[&str]) -> Flags {
        Flags::parse_with_switches(
            &words.iter().map(ToString::to_string).collect::<Vec<_>>(),
            &["again", "pin"],
        )
        .unwrap()
    }

    fn preset(store: &PostgresStore, id: &str, model: &str, rank: u32, grade: &str) {
        store
            .grading_record(&json!({"type": "preset.registered", "preset_id": id,
                                    "engine_label": "claude", "model_label": id,
                                    "model_profile": model, "cost_rank": rank}))
            .unwrap();
        store
            .grading_record(&json!({"type": "preset.onboarded", "preset_id": id,
                                    "from_state": "unexamined", "to_state": "active",
                                    "to_grade": grade, "passed": 1, "total": 1,
                                    "failed_cases": [], "suite": "s"}))
            .unwrap();
    }

    fn disable(store: &PostgresStore, id: &str, from: &str) {
        store
            .grading_record(&json!({"type": "preset.state_changed", "preset_id": id,
                                    "from_state": from, "to_state": "disabled"}))
            .unwrap();
    }

    fn assigned(store: &PostgresStore, mission: i64, words: &[&str]) -> Result<Value, String> {
        let mission = mission.to_string();
        let mut all = vec!["--mission", mission.as_str(), "--worker", "w"];
        if !words.contains(&"--kind") {
            all.extend_from_slice(&["--kind", "ephemeral"]);
        }
        all.extend_from_slice(words);
        assign(store, &flags(&all))
    }

    fn report(store: &PostgresStore, task: i64, command: i64) {
        store.task_delivered(task, command).unwrap();
        assert!(store.task_reported(task, "done").unwrap());
    }

    fn sent_back(store: &PostgresStore, task: i64) -> Value {
        let id = task.to_string();
        reject(store, &flags(&["--id", &id, "--note", "again"])).unwrap()
    }

    #[test]
    fn without_a_catalog_assign_is_as_before_and_the_routing_flags_are_strict() {
        let schema = TestSchema::new("cli_grading_none");
        let store = schema.store();
        let mission = store.mission_add("Plain", &json!([]), "human").unwrap();
        let plain = assigned(&store, mission, &["--brief", "one"]).unwrap();
        assert_eq!(plain, json!({"task": plain["task"], "model": "default"}));
        let cheap = assigned(&store, mission, &["--brief", "two", "--model", "cheap"]).unwrap();
        assert_eq!(cheap["model"], json!("cheap"));
        for (words, says) in [
            (
                &["--brief", "3", "--model", "cheap", "--difficulty", "l1"][..],
                "--model keeps the grading catalog out",
            ),
            (
                &["--brief", "4", "--model", "default", "--preset", "p"][..],
                "--model keeps the grading catalog out",
            ),
            (
                &["--brief", "5", "--difficulty", "l2"][..],
                "no routable preset",
            ),
            (
                &["--brief", "6", "--difficulty", "l4"][..],
                "--difficulty is l1, l2 or l3",
            ),
            (
                &["--brief", "7", "--preset", "nobody"][..],
                "not in the grading catalog",
            ),
        ] {
            let error = assigned(&store, mission, words).unwrap_err();
            assert!(error.contains(says), "{words:?}: {error}");
        }
        // A plain task sent back is sent back as before.
        let task = plain["task"].as_i64().unwrap();
        report(&store, task, 1);
        assert_eq!(
            sent_back(&store, task),
            json!({"task": task, "sent_back": true})
        );
    }

    /// The partial-catalog trap: with only an `l1` preset routable, an
    /// unflagged assignment (the supervisor's) is assigned on `--model
    /// default` as before and says why, never refused; an explicit
    /// `--difficulty l2` is still refused.
    #[test]
    fn an_l1_only_catalog_lets_an_unflagged_assignment_fall_back() {
        let schema = TestSchema::new("cli_grading_l1_only");
        let store = schema.store();
        let mission = store.mission_add("Partial", &json!([]), "human").unwrap();
        preset(&store, "small", "cheap", 0, "l1");
        let plain = assigned(&store, mission, &["--brief", "a"]).unwrap();
        assert_eq!(plain["model"], json!("default"));
        assert!(plain.get("preset_id").is_none(), "{plain}");
        assert!(
            plain["routing_fallback"]
                .as_str()
                .unwrap()
                .contains("no routable preset in the grading catalog is graded l2"),
            "{plain}"
        );
        let task = plain["task"].as_i64().unwrap();
        let routed = store.routed_task(task).unwrap().unwrap();
        assert_eq!(routed.preset_id, None);
        assert_eq!(routed.model, "default");
        let error = assigned(&store, mission, &["--brief", "b", "--difficulty", "l2"]).unwrap_err();
        assert!(error.contains("no routable preset is graded l2"), "{error}");
        // An l1 task does route to it.
        let easy = assigned(&store, mission, &["--brief", "c", "--difficulty", "l1"]).unwrap();
        assert_eq!(easy["preset_id"], json!("small"));
        assert_eq!(easy["preset_grade"], json!("l1"));
    }

    #[test]
    fn a_resident_assignment_without_routing_flags_stays_on_its_model() {
        let schema = TestSchema::new("cli_grading_resident");
        let store = schema.store();
        let mission = store.mission_add("Resident", &json!([]), "human").unwrap();
        preset(&store, "mid-cheap", "cheap", 0, "l2");
        store.resident_propose("keeper", "a resident").unwrap();
        store.resident_approve("keeper").unwrap();
        let mission_text = mission.to_string();
        let resident = assign(
            &store,
            &flags(&[
                "--mission",
                &mission_text,
                "--kind",
                "resident",
                "--worker",
                "keeper",
                "--brief",
                "r",
            ]),
        )
        .unwrap();
        assert_eq!(
            resident,
            json!({"task": resident["task"], "model": "default"})
        );
        // An ephemeral one routes to the cheap l2 preset.
        let ephemeral = assigned(&store, mission, &["--brief", "e"]).unwrap();
        assert_eq!(ephemeral["preset_id"], json!("mid-cheap"));
        assert_eq!(ephemeral["model"], json!("cheap"));
    }

    #[test]
    fn the_catalog_routes_assignments_and_steps_up_rework() {
        let schema = TestSchema::new("cli_grading_route");
        let store = schema.store();
        let mission = store.mission_add("Graded", &json!([]), "human").unwrap();
        preset(&store, "small", "cheap", 0, "l1");
        preset(&store, "mid", "default", 1, "l2");
        preset(&store, "strong", "default", 2, "l3");

        // l2 unless said: the cheapest preset graded l2 or above.
        let default = assigned(&store, mission, &["--brief", "a"]).unwrap();
        assert_eq!(
            default,
            json!({"task": default["task"], "model": "default", "difficulty": "l2",
                   "preset_id": "mid", "preset_grade": "l2"})
        );
        let named = assigned(&store, mission, &["--brief", "b", "--preset", "strong"]).unwrap();
        assert_eq!(named["preset_id"], json!("strong"));
        assert_eq!(named["preset_grade"], json!("l3"));

        let easy = assigned(&store, mission, &["--brief", "d", "--difficulty", "l1"]).unwrap();
        assert_eq!(easy["preset_id"], json!("small"));
        assert_eq!(easy["model"], json!("cheap"));
        let task = easy["task"].as_i64().unwrap();
        // The first quality rejection of an l1 task needs l2: rerouted to
        // mid, on its model.
        report(&store, task, 1);
        let first = sent_back(&store, task);
        assert_eq!(
            first["rerouted"],
            json!({"type": "task.rerouted", "id": task, "preset_id": "mid",
                   "model": "default", "from_preset_id": "small", "revision": 1,
                   "routed_difficulty": "l2"})
        );
        assert!(first.get("same_model_profile").is_none(), "{first}");
        // The second still needs l2: mid keeps it.
        report(&store, task, 2);
        assert_eq!(
            sent_back(&store, task),
            json!({"task": task, "sent_back": true})
        );
        // The third needs l3: strong, on the same model profile as mid,
        // which the answer says.
        report(&store, task, 3);
        let third = sent_back(&store, task);
        assert_eq!(third["rerouted"]["preset_id"], json!("strong"));
        assert_eq!(third["rerouted"]["routed_difficulty"], json!("l3"));
        assert_eq!(third["same_model_profile"], json!(true));
        let routed = store.routed_task(task).unwrap().unwrap();
        assert_eq!(routed.preset_id.as_deref(), Some("strong"));
        assert_eq!(routed.first_preset_id.as_deref(), Some("small"));
        assert_eq!((routed.revisions, routed.quality_rejections), (3, 3));

        // With nothing strong enough left, the rework is sent back all the
        // same and the catalog's refusal said.
        disable(&store, "strong", "active");
        let hard = assigned(&store, mission, &["--brief", "e", "--preset", "mid"]).unwrap();
        let hard = hard["task"].as_i64().unwrap();
        report(&store, hard, 4);
        sent_back(&store, hard);
        report(&store, hard, 5);
        let refused = sent_back(&store, hard);
        assert_eq!(refused["sent_back"], json!(true));
        assert!(
            refused["reroute_error"]
                .as_str()
                .unwrap()
                .contains("no routable preset is graded l3"),
            "{refused}"
        );
    }

    /// A rejected task whose preset a person disabled is rerouted to a
    /// routable one even when the grade it needs has not moved.
    #[test]
    fn a_rejection_moves_rework_off_a_preset_a_person_disabled() {
        let schema = TestSchema::new("cli_grading_disabled");
        let store = schema.store();
        let mission = store.mission_add("Disabled", &json!([]), "human").unwrap();
        preset(&store, "mid-a", "default", 1, "l2");
        preset(&store, "mid-b", "cheap", 2, "l2");
        let task = assigned(&store, mission, &["--brief", "a"]).unwrap();
        assert_eq!(task["preset_id"], json!("mid-a"));
        let task = task["task"].as_i64().unwrap();
        report(&store, task, 1);
        disable(&store, "mid-a", "active");
        let answer = sent_back(&store, task);
        assert_eq!(answer["rerouted"]["preset_id"], json!("mid-b"));
        assert_eq!(answer["rerouted"]["from_preset_id"], json!("mid-a"));
        // An l2 task at its first quality rejection still needs only l2.
        assert_eq!(answer["rerouted"]["routed_difficulty"], json!("l2"));
        assert_eq!(answer["rerouted"]["model"], json!("cheap"));
        let routed = store.routed_task(task).unwrap().unwrap();
        assert_eq!(routed.model, "cheap");
        assert_eq!(routed.first_preset_id.as_deref(), Some("mid-a"));
    }

    /// The sole `l2` preset demoted by `calibrate --apply` on real quality
    /// failures: an unflagged assignment falls back to `--model default`,
    /// saying so, and the fleet keeps assigning.
    #[test]
    fn calibration_demotes_on_quality_failures_and_assignment_falls_back_after() {
        let schema = TestSchema::new("cli_grading_calibrate");
        let store = schema.store();
        let mission = store
            .mission_add("Calibrated", &json!([]), "human")
            .unwrap();
        preset(&store, "mid", "default", 1, "l2");
        let mut command = 0;
        for (n, brief) in ["a", "b", "c", "d", "e"].iter().enumerate() {
            let task = assigned(&store, mission, &["--brief", brief]).unwrap()["task"]
                .as_i64()
                .unwrap();
            command += 1;
            report(&store, task, command);
            if n == 0 {
                store.task_review(task, true, "", None).unwrap();
            } else {
                // Sent back for its quality, then given up on.
                store.task_review(task, false, "wrong", None).unwrap();
                store.task_cancel(task).unwrap();
            }
        }
        let dry = calibrate(&store, false).unwrap();
        assert_eq!(
            dry,
            json!([{"preset_id": "mid", "window": 5, "applied": false, "event": null,
                    "decision": "demote", "from_grade": "l2", "to_grade": "l1",
                    "first_pass": 1, "total": 5}])
        );
        assert_eq!(
            store.grading_catalog().unwrap()[0].calibrated_grade,
            Some(SkillGrade::L2)
        );
        let applied = calibrate(&store, true).unwrap();
        assert_eq!(applied[0]["applied"], json!(true));
        let mid = &store.grading_catalog().unwrap()[0];
        assert_eq!(mid.calibrated_grade, Some(SkillGrade::L1));
        assert_eq!(mid.state, PresetState::Demoted);
        // The window starts afresh at the new grade: nothing to decide.
        let after = calibrate(&store, true).unwrap();
        assert_eq!(after[0]["decision"], json!("hold"));
        assert_eq!(after[0]["window"], json!(0));
        // l2 is uncovered now: the next unflagged assignment falls back.
        let next = assigned(&store, mission, &["--brief", "f"]).unwrap();
        assert_eq!(next["model"], json!("default"));
        assert!(next.get("preset_id").is_none(), "{next}");
        assert!(next["routing_fallback"].is_string(), "{next}");
    }

    fn admin(schema: &TestSchema) -> postgres::Client {
        let mut client =
            postgres::Client::connect(schema.url(), postgres::NoTls).expect("the owner connects");
        client
            .batch_execute(&format!("set search_path to {}", schema.name()))
            .expect("schema");
        client
    }

    /// The `type` record of `task` in the log, the newest.
    fn logged(client: &mut postgres::Client, kind: &str, task: i64) -> Option<Value> {
        client
            .query_opt(
                "select kind -> 'detail' -> 'record' from spawn_events
                  where kind -> 'detail' -> 'record' ->> 'type' = $1
                    and (kind -> 'detail' -> 'record' ->> 'id')::bigint = $2
                  order by id desc limit 1",
                &[&kind, &task],
            )
            .expect("read the log")
            .map(|row| row.get(0))
    }

    fn named_preset(
        store: &PostgresStore,
        id: &str,
        model: &str,
        profile: &str,
        rank: u32,
        grade: &str,
    ) {
        store
            .grading_record(&json!({"type": "preset.registered", "preset_id": id,
                                    "engine_label": "claude", "model_label": id,
                                    "model_profile": model, "cost_rank": rank,
                                    "execution_profile": profile}))
            .unwrap();
        store
            .grading_record(&json!({"type": "preset.onboarded", "preset_id": id,
                                    "from_state": "unexamined", "to_state": "active",
                                    "to_grade": grade, "passed": 1, "total": 1,
                                    "failed_cases": [], "suite": "s"}))
            .unwrap();
    }

    #[test]
    fn an_unflagged_assignment_is_classified_and_its_rationale_recorded() {
        let schema = TestSchema::new("cli_grading_classify");
        let store = schema.store();
        let mut client = admin(&schema);
        let mission = store
            .mission_add("Classified", &json!([]), "human")
            .unwrap();
        preset(&store, "small", "cheap", 0, "l1");
        preset(&store, "mid", "default", 1, "l2");
        preset(&store, "strong", "default", 2, "l3");

        // A brief no rule reads: the phase-1 answer, exactly.
        let plain = assigned(&store, mission, &["--brief", "a"]).unwrap();
        assert_eq!(
            plain,
            json!({"task": plain["task"], "model": "default", "difficulty": "l2",
                   "preset_id": "mid", "preset_grade": "l2"})
        );
        // ...and its record still says the classifier ran, and why.
        let record = logged(
            &mut client,
            "task.assigned",
            plain["task"].as_i64().unwrap(),
        )
        .expect("assigned");
        assert_eq!(record["classification"]["difficulty"], json!("l2"));
        assert_eq!(record["classification"]["signals"], json!([]));

        let easy = assigned(&store, mission, &["--brief", "Fix the typo in README.md"]).unwrap();
        assert_eq!(easy["difficulty"], json!("l1"));
        assert_eq!(easy["preset_id"], json!("small"));
        assert_eq!(easy["model"], json!("cheap"));
        assert_eq!(
            easy["classification"]["signals"][0]["rule"],
            json!("low_risk")
        );

        let hard = assigned(
            &store,
            mission,
            &["--brief", "Harden authentication against SQL injection"],
        )
        .unwrap();
        assert_eq!(hard["difficulty"], json!("l3"));
        assert_eq!(hard["preset_id"], json!("strong"));
        let hard_task = hard["task"].as_i64().unwrap();
        let record = logged(&mut client, "task.assigned", hard_task).expect("assigned");
        assert_eq!(record["difficulty"], json!("l3"));
        assert_eq!(record["preset_id"], json!("strong"));
        assert_eq!(record["classification"]["difficulty"], json!("l3"));
        assert_eq!(record["classification"]["rules"], json!("brief-v2"));
        assert!(
            record["classification"]["rationale"]
                .as_str()
                .unwrap()
                .contains("security +4"),
            "{record}"
        );
        assert_eq!(
            store.routed_task(hard_task).unwrap().unwrap().difficulty,
            spawn_core::grading::TaskDifficulty::L3
        );

        // Every explicit override is as before: no classifier.
        let brief = "Rotate the leaked credential";
        let explicit =
            assigned(&store, mission, &["--brief", brief, "--difficulty", "l2"]).unwrap();
        assert_eq!(explicit["preset_id"], json!("mid"));
        assert!(explicit.get("classification").is_none(), "{explicit}");
        let record = logged(
            &mut client,
            "task.assigned",
            explicit["task"].as_i64().unwrap(),
        )
        .expect("assigned");
        assert!(record.get("classification").is_none(), "{record}");
        let named = assigned(&store, mission, &["--brief", "x1", "--preset", "small"]);
        assert!(named.is_err(), "an l2 task refuses an l1 preset, as before");
        let named = assigned(
            &store,
            mission,
            &["--brief", "Rotate x", "--preset", "mid", "--again"],
        )
        .unwrap();
        assert_eq!(named["preset_id"], json!("mid"));
        assert!(named.get("classification").is_none(), "{named}");
        let model = assigned(&store, mission, &["--brief", brief, "--model", "cheap"]).unwrap();
        assert_eq!(model, json!({"task": model["task"], "model": "cheap"}));
        // --again on a classified brief: classified the same way.
        let again = assigned(
            &store,
            mission,
            &[
                "--brief",
                "Harden authentication against SQL injection",
                "--again",
            ],
        )
        .unwrap();
        assert_eq!(again["difficulty"], json!("l3"));
    }

    #[test]
    fn a_classified_l3_with_no_l3_preset_runs_at_l2_and_says_so() {
        let schema = TestSchema::new("cli_grading_classify_capped");
        let store = schema.store();
        let mut client = admin(&schema);
        let mission = store.mission_add("Capped", &json!([]), "human").unwrap();
        preset(&store, "mid", "default", 1, "l2");
        let answer = assigned(&store, mission, &["--brief", "Write the schema migration"]).unwrap();
        assert_eq!(answer["difficulty"], json!("l2"));
        assert_eq!(answer["preset_id"], json!("mid"));
        assert_eq!(answer["classification"]["difficulty"], json!("l3"));
        assert_eq!(answer["classification"]["routed_at"], json!("l2"));
        let record = logged(
            &mut client,
            "task.assigned",
            answer["task"].as_i64().unwrap(),
        )
        .expect("assigned");
        assert_eq!(record["classification"]["routed_at"], json!("l2"));
        // A resident is never classified.
        store.resident_propose("keeper", "a resident").unwrap();
        store.resident_approve("keeper").unwrap();
        let mission_text = mission.to_string();
        let resident = assign(
            &store,
            &flags(&[
                "--mission",
                &mission_text,
                "--kind",
                "resident",
                "--worker",
                "keeper",
                "--brief",
                "Write the schema migration again",
            ]),
        )
        .unwrap();
        assert_eq!(
            resident,
            json!({"task": resident["task"], "model": "default"})
        );
    }

    #[test]
    fn a_named_execution_profile_flows_through_assignment_and_reroute() {
        let schema = TestSchema::new("cli_grading_profiles");
        let store = schema.store();
        let mut client = admin(&schema);
        let mission = store.mission_add("Profiles", &json!([]), "human").unwrap();
        named_preset(&store, "qwen", "cheap", "local-qwen", 0, "l1");
        named_preset(&store, "opus", "default", "opus-max", 1, "l2");
        named_preset(&store, "gemini", "default", "gemini-pro", 2, "l3");
        let catalog = store.grading_catalog().unwrap();
        let profiles: Vec<&str> = catalog
            .iter()
            .map(spawn_core::grading::GradedPreset::profile_name)
            .collect();
        assert_eq!(profiles, vec!["gemini-pro", "opus-max", "local-qwen"]);

        let easy = assigned(&store, mission, &["--brief", "e", "--difficulty", "l1"]).unwrap();
        assert_eq!(easy["preset_id"], json!("qwen"));
        assert_eq!(easy["model"], json!("cheap"));
        assert_eq!(easy["execution_profile"], json!("local-qwen"));
        let task = easy["task"].as_i64().unwrap();
        let record = logged(&mut client, "task.assigned", task).expect("assigned");
        assert_eq!(record["execution_profile"], json!("local-qwen"));
        assert_eq!(record["model"], json!("cheap"));
        report(&store, task, 1);
        let first = sent_back(&store, task);
        assert_eq!(
            first["rerouted"],
            json!({"type": "task.rerouted", "id": task, "preset_id": "opus",
                   "model": "default", "from_preset_id": "qwen", "revision": 1,
                   "routed_difficulty": "l2", "execution_profile": "opus-max"})
        );
        // The answer the web reads names every profile.
        let api = super::api_catalog(&catalog, &[], 0, i64::MAX);
        assert_eq!(
            api["profiles"],
            json!(["gemini-pro", "local-qwen", "opus-max"])
        );
    }

    /// The gate's verdict on `task` at `revision`, as the integrator records
    /// it ([`crate::integrate::record_gated`]).
    fn gated(store: &PostgresStore, task: i64, revision: i32, verdict: &str) -> Option<Value> {
        gated_result(store, task, revision, verdict).unwrap()
    }

    fn gated_result(
        store: &PostgresStore,
        task: i64,
        revision: i32,
        verdict: &str,
    ) -> Result<Option<Value>, String> {
        let record = if verdict == "conflict" {
            json!({"type": "task.gated", "id": task, "revision": revision,
                   "verdict": "conflict", "merge": "marked", "base": "b",
                   "reason": "main conflicts with your branch"})
        } else {
            json!({"type": "task.gated", "id": task, "revision": revision,
                   "verdict": verdict, "base": "b", "note": "a check failed"})
        };
        crate::integrate::record_gated(store, &record)
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn a_gates_quality_rejection_steps_rework_up_in_its_own_statement() {
        let schema = TestSchema::new("cli_grading_gate_reroute");
        let store = schema.store();
        let mut client = admin(&schema);
        let mission = store.mission_add("Gated", &json!([]), "human").unwrap();
        preset(&store, "small", "cheap", 0, "l1");
        preset(&store, "mid", "default", 1, "l2");
        preset(&store, "strong", "default", 2, "l3");
        let mut command = 0;
        let mut next = || {
            command += 1;
            command
        };

        // An l1 task the gate rejects: rerouted to mid with the verdict.
        let easy = assigned(&store, mission, &["--brief", "a", "--difficulty", "l1"]).unwrap();
        let easy = easy["task"].as_i64().unwrap();
        report(&store, easy, next());
        let rerouted = gated(&store, easy, 0, "rejected").expect("rerouted");
        assert_eq!(
            rerouted,
            json!({"type": "task.rerouted", "id": easy, "preset_id": "mid",
                   "model": "default", "from_preset_id": "small", "revision": 1,
                   "routed_difficulty": "l2"})
        );
        let routed = store.routed_task(easy).unwrap().unwrap();
        assert_eq!(routed.state, "revise");
        assert_eq!((routed.revisions, routed.quality_rejections), (1, 1));
        assert_eq!(routed.preset_id.as_deref(), Some("mid"));
        assert_eq!(routed.first_preset_id.as_deref(), Some("small"));
        assert_eq!(routed.model, "default");
        assert_eq!(
            logged(&mut client, "task.gated", easy).unwrap()["verdict"],
            json!("rejected")
        );

        // A conflict and an incompatible head: sent back, never stepped up.
        let fleet = assigned(&store, mission, &["--brief", "b", "--difficulty", "l1"]).unwrap();
        let fleet = fleet["task"].as_i64().unwrap();
        report(&store, fleet, next());
        assert_eq!(gated(&store, fleet, 0, "conflict"), None);
        report(&store, fleet, next());
        assert_eq!(gated(&store, fleet, 1, "incompatible"), None);
        let routed = store.routed_task(fleet).unwrap().unwrap();
        assert_eq!((routed.revisions, routed.quality_rejections), (2, 0));
        assert_eq!(routed.preset_id.as_deref(), Some("small"));
        assert!(logged(&mut client, "task.rerouted", fleet).is_none());

        // An l2 task: the first rejection keeps mid (l2 still), the second
        // steps up to strong: the review's thresholds.
        let mid = assigned(&store, mission, &["--brief", "c"]).unwrap();
        let mid = mid["task"].as_i64().unwrap();
        report(&store, mid, next());
        assert_eq!(gated(&store, mid, 0, "rejected"), None);
        assert_eq!(
            store
                .routed_task(mid)
                .unwrap()
                .unwrap()
                .preset_id
                .as_deref(),
            Some("mid")
        );
        report(&store, mid, next());
        let second = gated(&store, mid, 1, "rejected").expect("rerouted");
        assert_eq!(second["preset_id"], json!("strong"));
        assert_eq!(second["routed_difficulty"], json!("l3"));
        // A review's rejection and a gate's count together.
        let routed = store.routed_task(mid).unwrap().unwrap();
        assert_eq!((routed.revisions, routed.quality_rejections), (2, 2));

        // A verdict on another revision is refused as before; its reroute
        // with it: nothing is recorded, and the task is where it was.
        let stale = assigned(&store, mission, &["--brief", "d", "--difficulty", "l1"]).unwrap();
        let stale = stale["task"].as_i64().unwrap();
        report(&store, stale, next());
        let refused = gated_result(&store, stale, 7, "rejected").unwrap_err();
        assert!(refused.contains("not at the revision"), "{refused}");
        let routed = store.routed_task(stale).unwrap().unwrap();
        assert_eq!(routed.state, "reported");
        assert_eq!(routed.preset_id.as_deref(), Some("small"));
        assert!(logged(&mut client, "task.gated", stale).is_none());
        assert!(logged(&mut client, "task.rerouted", stale).is_none());

        // With nothing strong enough, the task is sent back on its preset.
        disable(&store, "mid", "active");
        disable(&store, "strong", "active");
        let alone = assigned(&store, mission, &["--brief", "e", "--difficulty", "l1"]).unwrap();
        let alone = alone["task"].as_i64().unwrap();
        report(&store, alone, next());
        assert_eq!(gated(&store, alone, 0, "rejected"), None);
        let routed = store.routed_task(alone).unwrap().unwrap();
        assert_eq!(routed.state, "revise");
        assert_eq!(routed.quality_rejections, 1);
        assert_eq!(routed.preset_id.as_deref(), Some("small"));

        // A task assigned without the catalog: as before.
        let plain = assigned(&store, mission, &["--brief", "f", "--model", "default"]).unwrap();
        let plain = plain["task"].as_i64().unwrap();
        report(&store, plain, next());
        assert_eq!(gated(&store, plain, 0, "rejected"), None);
        assert_eq!(store.routed_task(plain).unwrap().unwrap().state, "revise");

        // The refold keeps the gate's reroutes.
        let before: Value = client
            .query_one(
                "select jsonb_agg(to_jsonb(t) order by t.id) from mission_tasks t",
                &[],
            )
            .unwrap()
            .get(0);
        client
            .batch_execute("select spawn_control_refold()")
            .unwrap();
        let after: Value = client
            .query_one(
                "select jsonb_agg(to_jsonb(t) order by t.id) from mission_tasks t",
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(before, after);
    }

    #[test]
    fn upkeep_lists_starved_presets_and_assignment_reprobes_each_once() {
        let schema = TestSchema::new("cli_grading_upkeep");
        let store = schema.store();
        let mut client = admin(&schema);
        let mission = store.mission_add("Upkeep", &json!([]), "human").unwrap();
        // Nothing in the catalog: one read, nothing due.
        assert_eq!(
            super::upkeep(&store, true, 0, 1_000).unwrap(),
            json!({"calibration": [], "reprobe_due": [], "uncovered": false})
        );
        preset(&store, "mid", "default", 1, "l2");
        preset(&store, "mid-dear", "default", 5, "l2");
        let now = super::now_ms();
        // Just onboarded: nobody is starved yet.
        let fresh = super::upkeep(&store, false, now, 60_000).unwrap();
        assert_eq!(fresh["reprobe_due"], json!([]));
        assert_eq!(fresh["uncovered"], json!(false));
        // Both windows started long ago, and neither had a task since.
        client
            .batch_execute("update grading_presets set window_start_ms = 0")
            .unwrap();
        let due = super::upkeep(&store, true, now, 60_000).unwrap();
        assert_eq!(due["reprobe_due"], json!(["mid", "mid-dear"]));
        assert_eq!(due["calibration"][0]["decision"], json!("hold"));
        // Holds record nothing: running it again finds the same.
        assert_eq!(super::upkeep(&store, true, now, 60_000).unwrap(), due);
        // The dearer preset, starved, takes the next unflagged task.
        let probe = assigned(&store, mission, &["--brief", "a"]).unwrap();
        assert_eq!(probe["preset_id"], json!("mid-dear"));
        assert_eq!(probe["reprobe"], json!(true));
        // Once: the next goes to the minimum-sufficient preset again.
        let next = assigned(&store, mission, &["--brief", "b"]).unwrap();
        assert_eq!(next["preset_id"], json!("mid"));
        assert!(next.get("reprobe").is_none(), "{next}");
        let after = super::upkeep(&store, false, super::now_ms(), 60_000).unwrap();
        assert_eq!(after["reprobe_due"], json!([]));
        let activity = store.grading_activity().unwrap();
        assert_eq!(activity.len(), 2);
        // The fleet loop's clock: due at once, then a period later.
        let (mut upkeep, _) = super::Upkeep::from_setting(Some("3600"), now);
        assert!(upkeep.due(now));
        upkeep.check(&store, now).unwrap();
        assert!(!upkeep.due(now + 1));
        assert!(upkeep.due(now + 3_600_000));
    }

    /// The gate's step-up rule as the integrator reads it
    /// ([`super::gate_reroute`]): only `rejected` of a routed task its
    /// preset can no longer take; any other verdict, or a task assigned
    /// without the catalog, reads nothing.
    #[test]
    fn only_a_gates_rejected_reads_a_step_up() {
        let schema = TestSchema::new("cli_grading_gate_rule");
        let store = schema.store();
        let mission = store.mission_add("Rule", &json!([]), "human").unwrap();
        preset(&store, "small", "cheap", 0, "l1");
        preset(&store, "mid", "default", 1, "l2");
        let task = assigned(&store, mission, &["--brief", "a", "--difficulty", "l1"]).unwrap();
        let task = task["task"].as_i64().unwrap();
        report(&store, task, 1);
        for verdict in [
            "accepted",
            "conflict",
            "incompatible",
            "host_fault",
            "no_room",
            "aborted",
            "stale",
        ] {
            assert!(
                super::gate_reroute(&store, task, verdict)
                    .unwrap()
                    .is_none(),
                "{verdict}"
            );
        }
        let reroute = super::gate_reroute(&store, task, "rejected")
            .unwrap()
            .expect("a step-up");
        let decision = reroute.route.expect("routed");
        assert_eq!(decision.preset_id, "mid");
        assert_eq!(
            decision.routed_difficulty,
            spawn_core::grading::TaskDifficulty::L2
        );
        assert_eq!((reroute.revision, reroute.model.as_str()), (1, "cheap"));
        // Reading it recorded nothing.
        assert_eq!(store.routed_task(task).unwrap().unwrap().state, "reported");
        // Not routed: nothing to step up.
        let plain = assigned(&store, mission, &["--brief", "p", "--model", "default"]).unwrap();
        let plain = plain["task"].as_i64().unwrap();
        report(&store, plain, 2);
        assert!(
            super::gate_reroute(&store, plain, "rejected")
                .unwrap()
                .is_none()
        );
    }

    /// The sole preset demoted `l2 -> l1` by the upkeep: unflagged `l2`
    /// work falls back, but `l1` work still reaches it, so its window
    /// refills, the upkeep says it is due a re-probe when it starves, and
    /// calibration restores it: it never starves for want of tasks.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn a_demoted_sole_preset_still_takes_l1_work_and_is_restored() {
        let schema = TestSchema::new("cli_grading_demoted_sole");
        let store = schema.store();
        let mission = store.mission_add("Demoted", &json!([]), "human").unwrap();
        preset(&store, "mid", "default", 1, "l2");
        let mut command = 0;
        for (n, brief) in ["a", "b", "c", "d", "e"].iter().enumerate() {
            let task = assigned(&store, mission, &["--brief", brief]).unwrap();
            assert_eq!(task["preset_id"], json!("mid"));
            let task = task["task"].as_i64().unwrap();
            command += 1;
            report(&store, task, command);
            if n == 0 {
                store.task_review(task, true, "", None).unwrap();
            } else {
                store.task_review(task, false, "wrong", None).unwrap();
                store.task_cancel(task).unwrap();
            }
        }
        let interval = spawn_core::grading::REPROBE_INTERVAL_MS;
        let demoted = super::upkeep(&store, true, super::now_ms(), interval).unwrap();
        assert_eq!(demoted["calibration"][0]["decision"], json!("demote"));
        assert_eq!(demoted["calibration"][0]["applied"], json!(true));
        assert_eq!(demoted["uncovered"], json!(true));
        let mid = store.grading_catalog().unwrap().remove(0);
        assert_eq!(mid.calibrated_grade, Some(SkillGrade::L1));
        assert_eq!(mid.state, PresetState::Demoted);

        // Unflagged l2 work falls back, saying why.
        let neutral = assigned(&store, mission, &["--brief", "f"]).unwrap();
        assert_eq!(neutral["model"], json!("default"));
        assert!(neutral.get("preset_id").is_none(), "{neutral}");
        assert!(
            neutral["routing_fallback"]
                .as_str()
                .unwrap()
                .contains("graded l2"),
            "{neutral}"
        );
        // Starved long enough, the upkeep says it is due a re-probe.
        let later = super::upkeep(&store, false, super::now_ms() + interval, interval).unwrap();
        assert_eq!(later["reprobe_due"], json!(["mid"]));

        // Unflagged l1 work reaches it, and passes first time.
        for n in 0..8 {
            let brief = format!("Fix the typo in README.md, number {n}");
            let task = assigned(&store, mission, &["--brief", &brief]).unwrap();
            assert_eq!(task["difficulty"], json!("l1"), "{task}");
            assert_eq!(task["preset_id"], json!("mid"), "{task}");
            let task = task["task"].as_i64().unwrap();
            command += 1;
            report(&store, task, command);
            store.task_review(task, true, "", None).unwrap();
        }
        let window = store
            .grading_outcomes(
                "mid",
                spawn_core::grading::TaskDifficulty::L1,
                mid.window_start_ms,
            )
            .unwrap();
        assert_eq!(window.len(), 8);
        assert!(window.iter().all(|sample| sample.first_pass));
        // Not starved any more.
        let fed = super::upkeep(&store, false, super::now_ms(), interval).unwrap();
        assert_eq!(fed["reprobe_due"], json!([]));

        // Its refilled window restores it, and l2 work routes to it again.
        let restored = super::upkeep(&store, true, super::now_ms(), interval).unwrap();
        assert_eq!(restored["calibration"][0]["decision"], json!("restore"));
        assert_eq!(restored["uncovered"], json!(false));
        let mid = store.grading_catalog().unwrap().remove(0);
        assert_eq!(mid.calibrated_grade, Some(SkillGrade::L2));
        assert_eq!(mid.state, PresetState::Active);
        let back = assigned(&store, mission, &["--brief", "g"]).unwrap();
        assert_eq!(back["preset_id"], json!("mid"));
        assert_eq!(back["difficulty"], json!("l2"));
    }
}
