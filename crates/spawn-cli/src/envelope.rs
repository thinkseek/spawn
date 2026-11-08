//! The host's resource envelope, as `spawn loop` and `spawn integrate` run
//! under it (objective 029).
//!
//! The settings come from the environment ([`spawn_core::envelope::Settings`]):
//! the slice and its MemoryHigh, MemoryMax and CPU weight, and an execution's
//! and a gate's MemoryHigh, MemoryMax and MemorySwapMax. A size that does not
//! parse, and envelopes that do not nest under each other and the slice, are
//! refused at start with the setting said: the loop and the integrator do not
//! run on a silent default.
//!
//! With SPAWN_ENVELOPE=systemd (the default) and a systemd user manager that
//! delegates the memory controller, the slice is given its limits
//! (`systemctl --user set-property --runtime`, a drop-in under
//! `$XDG_RUNTIME_DIR/systemd/user.control`, so no unit file is needed) before
//! anything starts in it, and the loop records one `host.envelope` record
//! with the slice and every setting in bytes — again only when one of them
//! changes (the latest read back from the log on start, as `host.disk` is).
//! With no reachable user manager (the gate's sandbox empties /run), or one
//! that does not delegate memory, executions and gates run as before and the
//! loop records `host.envelope` state `unavailable` once, with its error, and
//! says it once in its log. SPAWN_ENVELOPE=off records state `off`.
//!
//! One slice per Spawn instance: the live fleet's is `spawn.slice`; a test or
//! a second instance on the host names its own.

use std::path::Path;

use serde_json::{Value, json};
use spawn_core::envelope::{Envelope, Mode, ScopeFacts, Settings, host_available, probe};
use spawn_postgres::PostgresStore;

use crate::config::env_value;

/// What this process knows of the host's envelopes: the settings, the user
/// manager when one is reachable, and the latest `host.envelope` record once
/// read back from the log.
pub(crate) struct Runtime {
    state: State,
    /// The latest `host.envelope` record, `None` until read from the log.
    recorded: Option<Value>,
    /// The unreachable-manager reason was already said in this process's log.
    said: bool,
}

/// What the host can do.
enum State {
    /// A user manager that delegates memory: scopes run in the slice.
    Systemd(Envelope),
    /// No user manager (or none that delegates memory): as before.
    Unavailable(String),
    /// SPAWN_ENVELOPE=off: as before, on purpose.
    Off,
}

impl Runtime {
    /// Parse the settings and probe the user manager. A setting that does
    /// not parse, or does not nest, is an error: the caller refuses to start.
    pub(crate) fn from_env() -> Result<Self, String> {
        Self::with(env_value, probe)
    }

    /// [`Self::from_env`] with the environment and the probe as parameters.
    pub(crate) fn with(
        lookup: impl Fn(&str) -> Option<String>,
        probe: impl FnOnce() -> Result<spawn_core::envelope::Manager, String>,
    ) -> Result<Self, String> {
        let settings = Settings::from_lookup(lookup)?;
        let state = match settings.mode {
            Mode::Off => State::Off,
            Mode::Systemd => match probe() {
                Ok(manager) => State::Systemd(Envelope {
                    settings: settings.clone(),
                    manager,
                }),
                Err(error) => State::Unavailable(error),
            },
        };
        Ok(Self {
            state,
            recorded: None,
            said: false,
        })
    }

    /// The envelope executions and gates run in, when the host has one.
    pub(crate) fn envelope(&self) -> Option<&Envelope> {
        match &self.state {
            State::Systemd(envelope) => Some(envelope),
            State::Unavailable(_) | State::Off => None,
        }
    }

    /// Make the slice carry its limits before anything starts in it, and say
    /// what went wrong when it cannot (executions still get their own
    /// scopes: an execution's envelope does not depend on the slice's).
    pub(crate) fn prepare(&self) -> Vec<String> {
        match &self.state {
            State::Systemd(envelope) => match envelope.apply_slice() {
                Ok(()) => Vec::new(),
                Err(error) => vec![format!(
                    "envelope: the slice {} has no limits ({error}); every execution and gate \
                     still runs in its own scope",
                    envelope.settings.slice
                )],
            },
            State::Unavailable(_) | State::Off => Vec::new(),
        }
    }

    /// The `host.envelope` record this host makes now.
    pub(crate) fn record_value(&self) -> Value {
        match &self.state {
            State::Systemd(envelope) => envelope.host_record(),
            State::Unavailable(error) => Envelope::unavailable_record(error),
            State::Off => Envelope::off_record(),
        }
    }

    /// Record `host.envelope` once — and again only when one of its settings
    /// changed, read back from the log on the first call so a restarted loop
    /// records nothing twice. Returns the line for the loop's log, if it
    /// recorded or has something to say once.
    pub(crate) fn record(&mut self, store: &PostgresStore) -> Result<Vec<String>, String> {
        if self.recorded.is_none() {
            let latest = store.host_envelope().map_err(|error| error.to_string())?;
            self.recorded = Some(latest);
        }
        let record = self.record_value();
        if same_envelope(self.recorded.as_ref(), &record) {
            return Ok(Vec::new());
        }
        let mut lines = Vec::new();
        if let State::Unavailable(error) = &self.state
            && !self.said
        {
            self.said = true;
            lines.push(format!(
                "envelope: no systemd user manager here ({error}); executions and gates run \
                 without an envelope"
            ));
        }
        store
            .host_record(&record)
            .map_err(|error| format!("host.envelope: {error}"))?;
        self.recorded = Some(record.clone());
        lines.push(match record["state"].as_str().unwrap_or_default() {
            "systemd" => format!(
                "envelope: {} ({} MiB high, {} MiB max, CPU weight {}); executions {} MiB high, \
                 {} MiB max, {} MiB swap; gates {} MiB high, {} MiB max, {} MiB swap",
                record["slice"].as_str().unwrap_or_default(),
                mib(record["memory_high"].as_u64()),
                mib(record["memory_max"].as_u64()),
                record["cpu_weight"].as_u64().unwrap_or_default(),
                mib(record["execution_memory_high"].as_u64()),
                mib(record["execution_memory_max"].as_u64()),
                mib(record["execution_memory_swap_max"].as_u64()),
                mib(record["gate_memory_high"].as_u64()),
                mib(record["gate_memory_max"].as_u64()),
                mib(record["gate_memory_swap_max"].as_u64()),
            ),
            other => format!("envelope: {other}"),
        });
        Ok(lines)
    }
}

/// Whole MiB of `bytes`, for a log line.
fn mib(bytes: Option<u64>) -> u64 {
    bytes.unwrap_or_default() / (1024 * 1024)
}

/// Whether the latest record and `record` say the same thing: the same
/// state, slice and every setting — `event_id` and `at_ms` are the store's,
/// not a setting.
fn same_envelope(recorded: Option<&Value>, record: &Value) -> bool {
    let Some(recorded) = recorded else {
        return false;
    };
    let fields = [
        "state",
        "slice",
        "memory_high",
        "memory_max",
        "cpu_weight",
        "execution_memory_high",
        "execution_memory_max",
        "execution_memory_swap_max",
        "gate_memory_high",
        "gate_memory_max",
        "gate_memory_swap_max",
    ];
    fields
        .iter()
        .all(|field| recorded.get(*field) == record.get(*field))
        && recorded["state"] == record["state"]
}

/// The envelope facts the fleet view shows: for each live execution its
/// scope at this request (read from the cgroup the start recorded), and the
/// slice's own numbers.
pub(crate) fn fleet(envelope: Option<&Envelope>, agents: &mut [Value]) {
    for agent in agents {
        let scope = agent["scope"].as_str().unwrap_or_default().to_string();
        let cgroup = agent["cgroup"].as_str().unwrap_or_default().to_string();
        if agent["live"] != true || scope.is_empty() || cgroup.is_empty() {
            agent["envelope"] = Value::Null;
            continue;
        }
        let directory = std::path::PathBuf::from(&cgroup);
        let facts = read_cgroup(&directory, &scope);
        let mut reading = facts.reading();
        reading["slice"] = slice_reading(agent, envelope, &directory);
        agent["envelope"] = reading;
    }
}

/// Name on each agent row the admission wait it is in, when it waits: its
/// measure and its threshold, so a person sees an agent that has not started
/// for lack of room rather than one that failed.
pub(crate) fn admissions(agents: &mut [Value], waits: &Value) {
    let Some(waits) = waits.as_array() else {
        return;
    };
    for agent in agents {
        let Some(agent_id) = agent["agent_id"].as_str() else {
            continue;
        };
        let wait = waits.iter().find(|wait| {
            wait["kind"] == "execution" && wait["agent_id"].as_str() == Some(agent_id)
        });
        if let Some(wait) = wait {
            agent["admission"] = json!({
                "measure": wait["measure"],
                "envelope_bytes": wait["envelope_bytes"],
                "measured_bytes": wait["measured_bytes"],
                "threshold_bytes": wait["threshold_bytes"],
                "command_id": wait["command_id"],
                "at_ms": wait["at_ms"],
            });
        }
    }
}

/// Read a scope's cgroup at its recorded path.
fn read_cgroup(directory: &Path, scope: &str) -> ScopeFacts {
    let mut facts = ScopeFacts {
        scope: scope.to_string(),
        ..ScopeFacts::default()
    };
    let read = |name: &str| {
        std::fs::read_to_string(directory.join(name))
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
    };
    facts.memory_current = read("memory.current");
    facts.memory_peak = read("memory.peak");
    facts.memory_high = read("memory.high");
    facts.memory_max = read("memory.max");
    facts.memory_swap_current = read("memory.swap.current");
    facts.memory_swap_max = read("memory.swap.max");
    if let Ok(events) = std::fs::read_to_string(directory.join("memory.events")) {
        for line in events.lines() {
            let mut words = line.split_whitespace();
            match (words.next(), words.next()) {
                (Some("high"), Some(value)) => facts.throttled = value.parse().ok(),
                (Some("oom_kill"), Some(value)) => facts.oom_kills = value.parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    facts
}

/// The slice a live execution's scope sits in, and its numbers now: its
/// configured limits from the environment when this process has them, and
/// what its cgroup holds either way.
fn slice_reading(agent: &Value, envelope: Option<&Envelope>, scope: &Path) -> Value {
    let directory = scope
        .parent()
        .map_or_else(std::path::PathBuf::new, Path::to_path_buf);
    let read = |name: &str| {
        std::fs::read_to_string(directory.join(name))
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
    };
    let mut stat = json!({});
    let mut held: u64 = 0;
    if let Ok(text) = std::fs::read_to_string(directory.join("memory.stat")) {
        for line in text.lines() {
            let mut words = line.split_whitespace();
            let Some(key) = words.next() else { continue };
            let Some(value) = words.next().and_then(|text| text.parse::<u64>().ok()) else {
                continue;
            };
            stat[key] = json!(value);
            if key == "anon" || key == "shmem" {
                held = held.saturating_add(value);
            }
        }
    }
    let mut oom = 0;
    if let Ok(events) = std::fs::read_to_string(directory.join("memory.events")) {
        for line in events.lines() {
            if let Some(value) = line
                .strip_prefix("oom_kill ")
                .and_then(|value| value.trim().parse::<u64>().ok())
            {
                oom = value;
            }
        }
    }
    let settings = envelope.map(|envelope| &envelope.settings);
    json!({
        "name": agent["slice"].as_str()
            .map(str::to_string)
            .or_else(|| settings.map(|settings| settings.slice.clone())),
        "memory_high": settings.map_or_else(|| read("memory.high"), |settings| Some(settings.slice_high)),
        "memory_max": settings.map_or_else(|| read("memory.max"), |settings| Some(settings.slice_max)),
        "cpu_weight": settings.map(|settings| settings.cpu_weight),
        "execution_memory_max": settings.map(|settings| settings.execution.max),
        "gate_memory_max": settings.map(|settings| settings.gate.max),
        "memory_current": read("memory.current"),
        "memory_peak": read("memory.peak"),
        "measured_bytes": held,
        "oom_kills": oom,
        "stat": stat,
    })
}

/// The host's available memory now, as the health strip shows it beside the
/// disk's free space.
#[must_use]
pub(crate) fn host_measure() -> Value {
    json!({"mem_available_bytes": host_available()})
}

#[cfg(test)]
mod tests {
    use super::{Runtime, State, same_envelope};
    use serde_json::json;
    use spawn_core::envelope::{Manager, Settings};

    fn environment(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, value)| value.clone())
        }
    }

    /// A manager, in the shape the probe returns it: a test's stand-in.
    #[allow(clippy::unnecessary_wraps)]
    fn manager() -> Result<Manager, String> {
        Ok(Manager {
            control_group: "/user.slice/user-1000.slice/user@1000.service".to_string(),
            runtime_dir: "/run/user/1000".to_string(),
        })
    }

    #[test]
    fn off_is_off_and_a_missing_manager_is_unavailable_not_an_error() {
        let off = Runtime::with(environment(&[("SPAWN_ENVELOPE", "off")]), manager).expect("off");
        assert!(matches!(off.state, State::Off));
        assert!(off.envelope().is_none());
        let unavailable =
            Runtime::with(environment(&[]), || Err("no bus".to_string())).expect("unavailable");
        assert!(matches!(unavailable.state, State::Unavailable(_)));
        assert!(unavailable.envelope().is_none());
        assert_eq!(unavailable.record_value()["state"], "unavailable");
        let systemd = Runtime::with(environment(&[]), manager).expect("systemd");
        assert!(systemd.envelope().is_some());
        assert_eq!(systemd.record_value()["state"], "systemd");
    }

    #[test]
    fn a_setting_that_does_not_parse_refuses_to_start() {
        let refused = Runtime::with(
            environment(&[("SPAWN_EXECUTION_MEMORY_MAX", "12 GiB")]),
            manager,
        );
        assert!(refused.is_err());
    }

    #[test]
    fn the_slice_record_is_written_once_and_again_when_a_setting_changes() {
        let record = json!({"type": "host.envelope", "state": "systemd", "slice": "spawn.slice",
                            "memory_high": 17_179_869_184_u64, "memory_max": 21_474_836_480_u64,
                            "cpu_weight": 50, "event_id": 3, "at_ms": 10});
        assert!(!same_envelope(None, &record));
        assert!(same_envelope(Some(&record.clone()), &record));
        assert!(!same_envelope(
            Some(&record),
            &json!({"state": "systemd",
                                                     "slice": "spawn.slice"})
        ));
        let changed = json!({"type": "host.envelope", "state": "systemd", "slice": "spawn.slice",
                             "memory_high": 17_179_869_184_u64, "memory_max": 21_474_836_480_u64,
                             "cpu_weight": 30});
        assert!(!same_envelope(Some(&record), &changed));
        let _ = Settings::from_lookup(environment(&[])).expect("settings");
    }
}
