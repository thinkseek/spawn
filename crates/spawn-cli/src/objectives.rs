//! Objective missions (docs/fleet-architecture.md §10.4): the controller's
//! backlog (`rsi/backlog/*.json`, in its checkout) turned into tasks.
//!
//! The integrator gives the mission's resident the lowest open objective,
//! one task at a time. A one-shot objective is solved once a task for it
//! lands; a recurring one stays open until the metric of its latest landing
//! meets its target. The metric is the gate's verdict line, never scenario
//! output, which candidate code controls.

use std::fmt::Write as _;
use std::path::Path;

use serde::Deserialize;
use spawn_postgres::PostgresStore;

use crate::integrate::Mission;

/// Gate verdicts quoted in an objective task's brief.
const HISTORY: i64 = 6;

/// One backlog task.
///
/// A recurring task is an objective: landing once does not solve it. It stays
/// open until the metric its scenario prints reaches `target`.
#[derive(Debug, Clone)]
pub(crate) struct Task {
    pub(crate) id: u32,
    pub(crate) title: String,
    pub(crate) prompt: String,
    pub(crate) recurring: bool,
    pub(crate) metric: Option<String>,
    pub(crate) unit: String,
    pub(crate) lower_is_better: bool,
    pub(crate) target: Option<f64>,
}

impl Task {
    /// Whether `value` meets the target.
    pub(crate) fn meets(&self, value: f64) -> bool {
        self.target.is_some_and(|target| {
            if self.lower_is_better {
                value <= target
            } else {
                value >= target
            }
        })
    }
}

/// One `METRIC <name> baseline=<n> candidate=<n>` line of a gate log.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Metric {
    pub(crate) name: String,
    pub(crate) baseline: Option<f64>,
    pub(crate) candidate: Option<f64>,
}

/// The `METRIC <name> baseline=<n> candidate=<n>` lines of a text (a
/// verdict's metric line), the last line of each name winning.
pub(crate) fn metric_lines(text: &str) -> Vec<Metric> {
    let mut metrics: Vec<Metric> = Vec::new();
    for line in text.lines() {
        let line = line.trim_start_matches('|').trim();
        let Some(rest) = line.strip_prefix("METRIC ") else {
            continue;
        };
        let mut words = rest.split_whitespace();
        let Some(name) = words.next() else {
            continue;
        };
        let mut metric = Metric {
            name: name.to_string(),
            baseline: None,
            candidate: None,
        };
        for word in words {
            match word.split_once('=') {
                Some(("baseline", value)) => metric.baseline = value.parse().ok(),
                Some(("candidate", value)) => metric.candidate = value.parse().ok(),
                _ => {}
            }
        }
        metrics.retain(|known| known.name != metric.name);
        metrics.push(metric);
    }
    metrics
}

/// The candidate value of metric `name` in one verdict metric line.
pub(crate) fn candidate_value(line: &str, name: &str) -> Option<f64> {
    metric_lines(line)
        .into_iter()
        .find(|metric| metric.name == name)?
        .candidate
}

/// Every task in the controller's backlog directory, by id.
pub(crate) fn load_backlog(directory: &Path) -> Result<Vec<Task>, String> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("read {}: {error}", directory.display())),
    };
    let mut tasks = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|error| format!("read {}: {error}", directory.display()))?
            .path();
        if path.extension().and_then(|text| text.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| format!("parse {}: {error}", path.display()))?;
        let text_field = |name: &str| value.get(name).and_then(|v| v.as_str()).map(str::to_string);
        let id = value
            .get("id")
            .and_then(serde_json::Value::as_u64)
            .and_then(|id| u32::try_from(id).ok())
            .ok_or_else(|| format!("{} has no id", path.display()))?;
        let prompt =
            text_field("prompt").ok_or_else(|| format!("{} has no prompt", path.display()))?;
        let task = Task {
            id,
            title: text_field("title").unwrap_or_else(|| format!("Task {id}")),
            prompt,
            recurring: value.get("recurring").and_then(serde_json::Value::as_bool) == Some(true),
            metric: text_field("metric"),
            unit: text_field("unit").unwrap_or_default(),
            lower_is_better: text_field("direction").as_deref() != Some("higher"),
            target: value.get("target").and_then(serde_json::Value::as_f64),
        };
        // Without both, a recurring objective could never be met.
        if task.recurring && (task.metric.is_none() || task.target.is_none()) {
            return Err(format!(
                "{} is recurring but has no metric and target",
                path.display()
            ));
        }
        tasks.push(task);
    }
    tasks.sort_by_key(|task| task.id);
    Ok(tasks)
}

/// One landed objective task, newest first in a list.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Landing {
    pub(crate) objective: u32,
    #[serde(default)]
    pub(crate) metric: Option<String>,
}

/// One gate verdict on an objective's task.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GateRecord {
    pub(crate) task: i64,
    pub(crate) verdict: String,
    #[serde(default)]
    pub(crate) reason: Option<String>,
    #[serde(default)]
    pub(crate) metric: Option<String>,
}

/// The latest landed value of a recurring objective's metric.
pub(crate) fn landed_value(task: &Task, landings: &[Landing]) -> Option<f64> {
    let name = task.metric.as_deref()?;
    landings
        .iter()
        .filter(|landing| landing.objective == task.id)
        .find_map(|landing| candidate_value(landing.metric.as_deref()?, name))
}

/// The lowest open objective: a one-shot one with no landing, or a
/// recurring one whose latest landed metric misses its target.
pub(crate) fn next_open<'a>(backlog: &'a [Task], landings: &[Landing]) -> Option<&'a Task> {
    backlog.iter().find(|task| {
        if task.recurring {
            !landed_value(task, landings).is_some_and(|value| task.meets(value))
        } else {
            !landings.iter().any(|landing| landing.objective == task.id)
        }
    })
}

/// The brief of an objective task: the objective, the working rules, and
/// what the gate said about earlier attempts.
pub(crate) fn brief(task: &Task, current: Option<f64>, history: &[GateRecord]) -> String {
    let mut brief = format!("Objective {}: {}\n\n{}\n", task.id, task.title, task.prompt);
    brief.push_str(
        "\nWork only inside this workspace. Do not run git commands and do not create \
         commits: the host commits your workspace. Do not edit `rsi/`, `scripts/rsi/`, \
         `scripts/blackbox/` or `scripts/mvp-e2e.sh`: the gate refuses any change there. \
         The gate builds your tree, runs this objective's scenario (it must fail on the \
         current main and pass on yours) and every other check (clippy, the tests, the \
         e2e, the black-box scenarios, the web build). If it rejects your work, you get \
         the task back with its output.\n",
    );
    if task.recurring
        && let Some(name) = &task.metric
    {
        let _ = write!(
            brief,
            "\nObjective metric: `{name}` ({} is better).",
            if task.lower_is_better {
                "lower"
            } else {
                "higher"
            }
        );
        if let Some(value) = current {
            let _ = write!(brief, " Current landed value: {value} {}.", task.unit);
        }
        if let Some(target) = task.target {
            let _ = write!(brief, " Target: {target} {}.", task.unit);
        }
        brief.push('\n');
    }
    if !history.is_empty() {
        brief.push_str("\nEarlier gate verdicts on this objective, newest first:\n");
        for verdict in history {
            let _ = writeln!(
                brief,
                "- task {}: {}{}{}",
                verdict.task,
                verdict.verdict,
                verdict
                    .reason
                    .as_deref()
                    .map_or(String::new(), |reason| format!("; {reason}")),
                verdict
                    .metric
                    .as_deref()
                    .map_or(String::new(), |metric| format!("; {metric}")),
            );
        }
    }
    brief
}

/// Give an objective mission's resident its next task, when the mission
/// has none open. Returns the task id.
pub(crate) fn assign_next(store: &PostgresStore, mission: &Mission) -> Result<Option<i64>, String> {
    let Some(config) = &mission.objectives else {
        return Ok(None);
    };
    if !mission.tasks.is_empty() {
        // One task at a time: anything listed is still open.
        return Ok(None);
    }
    let backlog = config
        .get("backlog")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "objectives have no backlog".to_string())?;
    let objectives = load_backlog(Path::new(backlog))?;
    let landings: Vec<Landing> = serde_json::from_value(
        store
            .objective_landings(mission.id)
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("objective landings: {error}"))?;
    let Some(task) = next_open(&objectives, &landings) else {
        return Ok(None);
    };
    let id = i32::try_from(task.id).map_err(|_| format!("objective {} is too large", task.id))?;
    let history: Vec<GateRecord> = serde_json::from_value(
        store
            .objective_history(mission.id, id, HISTORY)
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("objective history: {error}"))?;
    let scenario = Path::new(&mission.gate).join(format!("{:03}.sh", task.id));
    let scenario = scenario.is_file().then(|| scenario.display().to_string());
    let text = brief(task, landed_value(task, &landings), &history);
    let assigned = store
        .objective_assign(mission.id, id, scenario.as_deref(), &text)
        .map_err(|error| error.to_string())?;
    println!(
        "integrate: mission {} objective {} -> task {assigned}",
        mission.id, task.id
    );
    Ok(Some(assigned))
}

#[cfg(test)]
mod tests {
    use super::{Landing, Task, load_backlog, metric_lines, next_open};

    fn objective(id: u32, recurring: bool) -> Task {
        Task {
            id,
            title: format!("objective {id}"),
            prompt: "do it".to_string(),
            recurring,
            metric: recurring.then(|| "ingest_total_ns".to_string()),
            unit: "ns".to_string(),
            lower_is_better: true,
            target: recurring.then_some(1100.0),
        }
    }

    fn landing(objective: u32, metric: Option<&str>) -> Landing {
        Landing {
            objective,
            metric: metric.map(str::to_string),
        }
    }

    #[test]
    fn the_lowest_open_objective_is_next() {
        let backlog = [objective(1, false), objective(2, true), objective(3, false)];
        assert_eq!(next_open(&backlog, &[]).map(|task| task.id), Some(1));
        let solved_one = [landing(1, None)];
        assert_eq!(
            next_open(&backlog, &solved_one).map(|task| task.id),
            Some(2)
        );
        let missed = [
            landing(
                2,
                Some("METRIC ingest_total_ns baseline=1500 candidate=1200"),
            ),
            landing(1, None),
        ];
        assert_eq!(next_open(&backlog, &missed).map(|task| task.id), Some(2));
        // Newest first: the latest landing decides.
        let met = [
            landing(
                2,
                Some("METRIC ingest_total_ns baseline=1200 candidate=1021"),
            ),
            landing(
                2,
                Some("METRIC ingest_total_ns baseline=1500 candidate=1200"),
            ),
            landing(1, None),
        ];
        assert_eq!(next_open(&backlog, &met).map(|task| task.id), Some(3));
        let all = [landing(3, None), met[0].clone(), landing(1, None)];
        assert!(next_open(&backlog, &all).is_none());
    }

    #[test]
    fn a_metric_line_is_read() {
        let metrics = metric_lines("METRIC ingest_total_ns baseline=1573 candidate=1021");
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].name, "ingest_total_ns");
        assert_eq!(metrics[0].baseline, Some(1573.0));
        assert_eq!(metrics[0].candidate, Some(1021.0));
    }

    #[test]
    fn a_recurring_objective_needs_a_metric_and_a_target() {
        let dir = std::env::temp_dir().join(format!("spawn-backlog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let write = |body: &str| std::fs::write(dir.join("001.json"), body).expect("write");
        write(
            r#"{"id": 1, "prompt": "go faster", "recurring": true, "metric": "ns", "target": 900}"#,
        );
        assert_eq!(load_backlog(&dir).expect("valid").len(), 1);
        write(r#"{"id": 1, "prompt": "go faster", "recurring": true, "metric": "ns"}"#);
        assert!(load_backlog(&dir).is_err(), "never met");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
