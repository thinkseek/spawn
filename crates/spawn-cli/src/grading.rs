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
use spawn_core::event::ModelProfile;
use spawn_core::grading::{
    CalibrationDecision, ExamCaseResult, GradedPreset, PresetState, SkillGrade, TaskDifficulty,
    calibrate_preset, catalog_covers, evaluate_onboarding, is_preset_id,
};
use spawn_postgres::PostgresStore;

use crate::{Flags, open_store};

pub(crate) const USAGE: &str = "usage:
  spawn grade register <preset_id> --engine-label <label> --model-label <label> --cost-rank <n>
                       [--model-profile default|cheap]
      (a new graded preset, unexamined: never routed until onboarded. preset_id:
       [a-z0-9][a-z0-9_-]{0,63}; cost rank: 0 to 1000000, lower is cheaper; model profile:
       the --model switch its worker runs on, default unless named, and in phase 1 the only
       part of a preset a worker runs on: the two labels are for people)
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
       conflict or an incompatible head does not. Without --apply it records nothing)";

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
        &["engine-label", "model-label", "cost-rank", "model-profile"],
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
    let model_profile = match flags.get("model-profile") {
        None => ModelProfile::Default,
        Some(text) => ModelProfile::parse(&text)
            .ok_or_else(|| format!("--model-profile is default or cheap, not {text}"))?,
    };
    Ok(json!({"type": "preset.registered", "preset_id": preset_id,
              "engine_label": engine_label, "model_label": model_label,
              "model_profile": model_profile, "cost_rank": cost_rank}))
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

/// The catalog as a table.
fn table(catalog: &[GradedPreset]) -> String {
    let grade = |grade: Option<SkillGrade>| grade.map_or("-", SkillGrade::as_str);
    let mut lines = vec![format!(
        "{:<24} {:<10} {:<7} {:>4} {:<5} {:<5} {:<5} {:<5} {}",
        "PRESET", "STATE", "MODEL", "COST", "ONB", "CAL", "LOCK", "ROUTE", "ENGINE/MODEL LABEL"
    )];
    for p in catalog {
        lines.push(format!(
            "{:<24} {:<10} {:<7} {:>4} {:<5} {:<5} {:<5} {:<5} {}/{}",
            p.preset_id,
            p.state.as_str(),
            p.model_profile.as_str(),
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
}
