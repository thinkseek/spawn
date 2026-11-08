//! Resource envelopes (objective 029): every execution and every gate runs in
//! a systemd user scope of its own, directly inside one slice of Spawn's, so
//! that one build, test or tool that runs away is bounded by its own limits
//! instead of taking the host's memory, swap and CPU.
//!
//! A systemd user manager makes a slice carry limits without root
//! (`systemctl --user set-property --runtime`, or a drop-in), and a transient
//! scope started with `systemd-run --user --scope --slice=<slice>` runs a
//! command under its own MemoryHigh, MemoryMax and MemorySwapMax inside it.
//! This module is the arithmetic and the cgroup reads: the settings (which
//! the loop and the integrator parse at start and refuse when they do not
//! fit), the names (`<slice stem>-<execution id>.scope`, and
//! `<slice stem>-gate-<task id>-<n>.scope`), the admission decision (an
//! envelope fits under the slice and on the host, or it waits), and what a
//! scope's cgroup and unit say when it ends.
//!
//! The slice caps no swap: this host's swap is zram, RAM itself, and an
//! envelope with unbounded swap bounds nothing. Every envelope caps its
//! swap (SPAWN_*_MEMORY_SWAP_MAX, 4G by default). Admission measures what a
//! slice cannot give back (`anon` + `shmem` of its `memory.stat`: page cache
//! is reclaimed under the limit, anonymous and tmpfs memory go to swap,
//! which is RAM here) against the slice's MemoryMax minus the envelope's,
//! and `MemAvailable` against the envelope's MemoryMax.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::id::ExecutionId;
use crate::port::{HostRecords, StoreError};

/// Bytes in a kibibyte, mebibyte, gibibyte and tebibyte: the powers of 1024
/// systemd reads a size in.
pub const KIB: u64 = 1024;
/// [`KIB`] squared.
pub const MIB: u64 = 1024 * KIB;
/// [`KIB`] cubed.
pub const GIB: u64 = 1024 * MIB;
/// [`KIB`] to the fourth.
pub const TIB: u64 = 1024 * GIB;

/// Spawn's slice when SPAWN_SLICE says nothing.
pub const DEFAULT_SLICE: &str = "spawn.slice";
/// The slice's soft limit when SPAWN_SLICE_MEMORY_HIGH says nothing.
pub const DEFAULT_SLICE_MEMORY_HIGH: u64 = 16 * GIB;
/// The slice's hard limit when SPAWN_SLICE_MEMORY_MAX says nothing.
pub const DEFAULT_SLICE_MEMORY_MAX: u64 = 20 * GIB;
/// The slice's CPU weight when SPAWN_SLICE_CPU_WEIGHT says nothing: half of
/// systemd's default 100, so Spawn yields to the host's other jobs under
/// contention and uses an idle CPU otherwise.
pub const DEFAULT_SLICE_CPU_WEIGHT: u64 = 50;
/// An execution's soft limit when SPAWN_EXECUTION_MEMORY_HIGH says nothing.
pub const DEFAULT_EXECUTION_MEMORY_HIGH: u64 = 10 * GIB;
/// An execution's hard limit when SPAWN_EXECUTION_MEMORY_MAX says nothing.
pub const DEFAULT_EXECUTION_MEMORY_MAX: u64 = 12 * GIB;
/// A gate's soft limit when SPAWN_GATE_MEMORY_HIGH says nothing.
pub const DEFAULT_GATE_MEMORY_HIGH: u64 = 10 * GIB;
/// A gate's hard limit when SPAWN_GATE_MEMORY_MAX says nothing.
pub const DEFAULT_GATE_MEMORY_MAX: u64 = 12 * GIB;
/// Every envelope's swap cap when its SPAWN_*_MEMORY_SWAP_MAX says nothing.
pub const DEFAULT_MEMORY_SWAP_MAX: u64 = 4 * GIB;

/// How long one `systemctl` call may take before it is killed: a manager
/// that stopped answering must not hold a loop's tick.
pub const SYSTEMCTL_LIMIT: Duration = Duration::from_secs(10);

/// Whether executions run in envelopes at all (SPAWN_ENVELOPE).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A scope per execution and per gate, inside Spawn's slice.
    Systemd,
    /// As before objective 029: the terminal's own cgroup, no limits.
    Off,
}

/// One envelope: its soft limit, its hard limit and its swap cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// MemoryHigh: above this the cgroup is throttled (its processes crawl)
    /// before anything is killed.
    pub high: u64,
    /// MemoryMax: this kills the process that reaches it, and OOMPolicy stop
    /// ends the rest of the cgroup with it.
    pub max: u64,
    /// MemorySwapMax: what the envelope may put into swap, which on a host
    /// whose swap is zram is RAM itself. Never unbounded.
    pub swap_max: u64,
}

/// The envelope settings, as the loop and the integrator read them from the
/// environment at start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// SPAWN_ENVELOPE: `systemd` (default) or `off`.
    pub mode: Mode,
    /// SPAWN_SLICE (default `spawn.slice`).
    pub slice: String,
    /// SPAWN_SLICE_MEMORY_HIGH.
    pub slice_high: u64,
    /// SPAWN_SLICE_MEMORY_MAX.
    pub slice_max: u64,
    /// SPAWN_SLICE_CPU_WEIGHT.
    pub cpu_weight: u64,
    /// SPAWN_EXECUTION_MEMORY_HIGH, _MAX and _SWAP_MAX.
    pub execution: Limits,
    /// SPAWN_GATE_MEMORY_HIGH, _MAX and _SWAP_MAX.
    pub gate: Limits,
}

/// A size as a setting writes it: bytes, or a number with K, M, G or T
/// (powers of 1024, as systemd reads them). `None` when it does not parse.
#[must_use]
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let (digits, multiplier) = match text.as_bytes().last()? {
        b'k' | b'K' => (&text[..text.len() - 1], KIB),
        b'm' | b'M' => (&text[..text.len() - 1], MIB),
        b'g' | b'G' => (&text[..text.len() - 1], GIB),
        b't' | b'T' => (&text[..text.len() - 1], TIB),
        _ => (text, 1),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()?.checked_mul(multiplier)
}

impl Settings {
    /// The settings `lookup` names (the environment), with their defaults:
    /// a size that does not parse, a slice MemoryHigh above its MemoryMax,
    /// an envelope's MemoryHigh above its own MemoryMax, or an envelope's
    /// MemoryMax above the slice's is refused with the setting said.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mode = match lookup("SPAWN_ENVELOPE")
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
            .as_deref()
        {
            None | Some("systemd") => Mode::Systemd,
            Some("off") => Mode::Off,
            Some(other) => {
                return Err(format!(
                    "SPAWN_ENVELOPE is `systemd` or `off`, not '{other}'"
                ));
            }
        };
        let size = |name: &str, default: u64| -> Result<u64, String> {
            let Some(text) = lookup(name) else {
                return Ok(default);
            };
            let text = text.trim();
            if text.is_empty() {
                return Ok(default);
            }
            parse_size(text).ok_or_else(|| {
                format!(
                    "{name} is bytes or a number with K, M, G or T (powers of 1024), not '{text}'"
                )
            })
        };
        let weight = |name: &str, default: u64| -> Result<u64, String> {
            let Some(text) = lookup(name) else {
                return Ok(default);
            };
            let text = text.trim();
            if text.is_empty() {
                return Ok(default);
            }
            match text.parse::<u64>() {
                Ok(weight) if (1..=10_000).contains(&weight) => Ok(weight),
                _ => Err(format!(
                    "{name} is a CPU weight from 1 to 10000, not '{text}'"
                )),
            }
        };
        let slice = match lookup("SPAWN_SLICE").map(|text| text.trim().to_string()) {
            Some(name) if !name.is_empty() => {
                if name.len() > 200
                    || name.contains('/')
                    || name.chars().any(char::is_whitespace)
                {
                    return Err(format!("SPAWN_SLICE is a unit name, not '{name}'"));
                }
                match name.strip_suffix(".slice") {
                    Some(stem) => format!("{stem}.slice"),
                    None => format!("{name}.slice"),
                }
            }
            _ => DEFAULT_SLICE.to_string(),
        };
        let settings = Self {
            mode,
            slice,
            slice_high: size("SPAWN_SLICE_MEMORY_HIGH", DEFAULT_SLICE_MEMORY_HIGH)?,
            slice_max: size("SPAWN_SLICE_MEMORY_MAX", DEFAULT_SLICE_MEMORY_MAX)?,
            cpu_weight: weight("SPAWN_SLICE_CPU_WEIGHT", DEFAULT_SLICE_CPU_WEIGHT)?,
            execution: Limits {
                high: size("SPAWN_EXECUTION_MEMORY_HIGH", DEFAULT_EXECUTION_MEMORY_HIGH)?,
                max: size("SPAWN_EXECUTION_MEMORY_MAX", DEFAULT_EXECUTION_MEMORY_MAX)?,
                swap_max: size("SPAWN_EXECUTION_MEMORY_SWAP_MAX", DEFAULT_MEMORY_SWAP_MAX)?,
            },
            gate: Limits {
                high: size("SPAWN_GATE_MEMORY_HIGH", DEFAULT_GATE_MEMORY_HIGH)?,
                max: size("SPAWN_GATE_MEMORY_MAX", DEFAULT_GATE_MEMORY_MAX)?,
                swap_max: size("SPAWN_GATE_MEMORY_SWAP_MAX", DEFAULT_MEMORY_SWAP_MAX)?,
            },
        };
        settings.validate()?;
        Ok(settings)
    }

    /// The settings' own consistency: every soft limit at or below its hard
    /// one, and every envelope's hard limit under the slice's.
    pub fn validate(&self) -> Result<(), String> {
        if self.slice_high > self.slice_max {
            return Err(format!(
                "SPAWN_SLICE_MEMORY_HIGH ({}) is above SPAWN_SLICE_MEMORY_MAX ({})",
                self.slice_high, self.slice_max
            ));
        }
        for (name, limits) in [
            ("SPAWN_EXECUTION", self.execution),
            ("SPAWN_GATE", self.gate),
        ] {
            if limits.high > limits.max {
                return Err(format!(
                    "{name}_MEMORY_HIGH ({}) is above {name}_MEMORY_MAX ({})",
                    limits.high, limits.max
                ));
            }
            if limits.max > self.slice_max {
                return Err(format!(
                    "{name}_MEMORY_MAX ({}) is above SPAWN_SLICE_MEMORY_MAX ({})",
                    limits.max, self.slice_max
                ));
            }
        }
        if self.cpu_weight == 0 {
            return Err("SPAWN_SLICE_CPU_WEIGHT is 0".to_string());
        }
        Ok(())
    }

    /// The slice's name without `.slice`: what a scope of this slice is
    /// named after.
    #[must_use]
    pub fn slice_stem(&self) -> &str {
        self.slice.strip_suffix(".slice").unwrap_or(&self.slice)
    }
}

/// The user manager this host's session talks to, as probed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manager {
    /// The manager's own cgroup, `/user.slice/user-1000.slice/user@1000.service`.
    pub control_group: String,
    /// The runtime directory whose `bus` is the manager's, handed to a pane
    /// and to a gate so their `systemd-run` finds the manager whatever their
    /// own environment (a cleared one, a tmux server's) says.
    pub runtime_dir: String,
}

/// The slice and the manager a set of settings runs under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// What the environment said.
    pub settings: Settings,
    /// The user manager that makes the slice and the scopes.
    pub manager: Manager,
}

/// What a scope's cgroup and unit say, read at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeFacts {
    /// The unit name (`spawn-exec_12.scope`).
    pub scope: String,
    /// The unit's `Result` (`oom-kill` after a memory-limit kill), when the
    /// unit still exists.
    pub result: Option<String>,
    /// The unit's `LoadState`: `not-found` once it is gone, as a collected
    /// scope is.
    pub load_state: Option<String>,
    /// `memory.current`.
    pub memory_current: Option<u64>,
    /// `memory.peak`.
    pub memory_peak: Option<u64>,
    /// `memory.high`.
    pub memory_high: Option<u64>,
    /// `memory.max`.
    pub memory_max: Option<u64>,
    /// `memory.swap.current`.
    pub memory_swap_current: Option<u64>,
    /// `memory.swap.max`.
    pub memory_swap_max: Option<u64>,
    /// `memory.events`' `high`: how often the cgroup was throttled.
    pub throttled: Option<u64>,
    /// `memory.events`' `oom_kill`: processes the kernel killed in it.
    pub oom_kills: u64,
}

impl ScopeFacts {
    /// Whether the scope's memory limit killed something in it: its own
    /// `memory.events` counter, or the unit's `Result` when the cgroup is
    /// already gone (a scope may not be read after it stopped).
    #[must_use]
    pub fn oom_killed(&self) -> bool {
        self.oom_kills > 0 || self.result.as_deref() == Some("oom-kill")
    }

    /// The facts as a record's evidence: the scope, its limits and peak in
    /// bytes (the peak only when it could be read), and the kill count.
    #[must_use]
    pub fn evidence(&self, memory_max: u64) -> Value {
        let mut evidence = json!({
            "scope": self.scope,
            "memory_max": memory_max,
            "oom_kills": self.oom_kills.max(u64::from(self.result.as_deref() == Some("oom-kill"))),
        });
        if let Some(peak) = self.memory_peak {
            evidence["memory_peak"] = json!(peak);
        }
        evidence
    }

    /// What the fleet view shows of a live execution's scope.
    #[must_use]
    pub fn reading(&self) -> Value {
        json!({
            "scope": self.scope,
            "memory_current": self.memory_current,
            "memory_peak": self.memory_peak,
            "memory_high": self.memory_high,
            "memory_max": self.memory_max,
            "memory_swap_current": self.memory_swap_current,
            "memory_swap_max": self.memory_swap_max,
            "throttled": self.throttled,
            "oom_kills": self.oom_kills,
        })
    }
}

/// What an admission decision measured, and what it measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortfall {
    /// `slice` or `host`: the first measure that fell short.
    pub measure: &'static str,
    /// The bytes it measured.
    pub measured: u64,
    /// The bytes it needed.
    pub threshold: u64,
}

impl Shortfall {
    /// The record an execution or a gate that does not fit leaves, once.
    #[must_use]
    pub fn record(&self, kind: &str, fields: &Value, envelope_bytes: u64) -> Value {
        let mut record = json!({
            "type": "admission.waiting",
            "kind": kind,
            "envelope_bytes": envelope_bytes,
            "measure": self.measure,
            "measured_bytes": self.measured,
            "threshold_bytes": self.threshold,
        });
        if let (Some(target), Some(fields)) = (record.as_object_mut(), fields.as_object()) {
            target.extend(fields.clone());
        }
        record
    }
}

/// One start's admission: what it is, and where its wait begins.
#[derive(Debug, Clone)]
pub struct Admission<'a> {
    /// `execution` (a command's start or resume) or `gate` (a task's).
    pub kind: &'a str,
    /// The record's own fields: `agent_id`, `session_id` and `command_id`
    /// for an execution; `task` for a gate.
    pub fields: Value,
    /// The envelope's MemoryMax, in bytes: what admission measured.
    pub envelope_bytes: u64,
    /// The event its wait begins after: 0 for an execution (a command id is
    /// unique), the task's latest judging for a gate.
    pub after: i64,
}

impl Admission<'_> {
    /// The field that names the start in the log.
    fn field(&self) -> &'static str {
        if self.kind == "gate" { "task" } else { "command_id" }
    }

    /// Its value there.
    fn value(&self) -> Option<String> {
        let value = self.fields.get(self.field())?;
        Some(value.as_u64().map_or_else(|| value.to_string(), |number| number.to_string()))
    }
}

/// Whether one start may run now (objective 029): while its envelope does
/// not fit (`short`, the first measure that falls short), the wait is
/// recorded once — `admission.waiting`, with the measure, what it measured
/// and its threshold — and the start does not run; the command or the task
/// keeps its place, and the decision is made again at the next pass from
/// that pass's measures. The record's time is what a later
/// [`admitted`] measures the wait from, so a restarted loop neither records
/// the wait again nor forgets when it began.
pub fn admit(
    records: &dyn HostRecords,
    start: &Admission<'_>,
    short: Option<Shortfall>,
) -> Result<bool, StoreError> {
    let Some(short) = short else {
        return Ok(true);
    };
    let Some(value) = start.value() else {
        return Ok(true);
    };
    let waiting = records.admission_record(
        "admission.waiting",
        start.kind,
        start.field(),
        &value,
        start.after,
    )?;
    if waiting.is_none() {
        records.record(&short.record(start.kind, &start.fields, start.envelope_bytes))?;
    }
    Ok(false)
}

/// The `admission.admitted` record of a start that waited, written once when
/// it runs: `waited_ms` is from the wait's first decision to now, by the
/// database's clock. A start that fits at once wrote no wait and writes
/// nothing here.
pub fn admitted(records: &dyn HostRecords, start: &Admission<'_>) -> Result<(), StoreError> {
    let Some(value) = start.value() else {
        return Ok(());
    };
    if records
        .admission_record(
            "admission.admitted",
            start.kind,
            start.field(),
            &value,
            start.after,
        )?
        .is_some()
    {
        return Ok(());
    }
    let Some(wait) = records.admission_record(
        "admission.waiting",
        start.kind,
        start.field(),
        &value,
        start.after,
    )?
    else {
        return Ok(());
    };
    let now = records.now_ms()?;
    let mut record = json!({
        "type": "admission.admitted",
        "kind": start.kind,
        "envelope_bytes": start.envelope_bytes,
        "waited_ms": now.saturating_sub(wait),
    });
    if let (Some(target), Some(fields)) = (record.as_object_mut(), start.fields.as_object()) {
        target.extend(fields.clone());
    }
    records.record(&record)?;
    Ok(())
}

/// Probe the user manager: one is reachable when systemd's user bus answers
/// (`systemctl --user`), and it delegates the memory controller to its
/// units. `Err` says what is missing, for the loop's `host.envelope` record
/// (`unavailable`) and its one line of log.
pub fn probe() -> Result<Manager, String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_dir());
    let Some(runtime) = runtime else {
        return Err(
            "XDG_RUNTIME_DIR is unset or no directory: no systemd user manager is reachable"
                .to_string(),
        );
    };
    if !runtime.join("bus").exists() {
        return Err(format!(
            "no user bus at {}: no systemd user manager is reachable",
            runtime.join("bus").display()
        ));
    }
    let version = systemctl(&["show", "-p", "Version", "--value"])?;
    if version.trim().is_empty() {
        return Err("systemctl --user names no manager version".to_string());
    }
    let control_group = systemctl(&["show", "-p", "ControlGroup", "--value"])?;
    let control_group = control_group.trim().to_string();
    if control_group.is_empty() {
        return Err("the user manager has no cgroup".to_string());
    }
    let path = cgroup_root().join(control_group.trim_start_matches('/'));
    if !path.is_dir() {
        return Err(format!(
            "the user manager's cgroup {control_group} is not in /sys/fs/cgroup"
        ));
    }
    let subtree = std::fs::read_to_string(path.join("cgroup.subtree_control")).unwrap_or_default();
    if !subtree.split_whitespace().any(|controller| controller == "memory") {
        return Err(format!(
            "the user manager does not delegate the memory controller (subtree_control: {})",
            subtree.trim()
        ));
    }
    Ok(Manager {
        control_group,
        runtime_dir: runtime.display().to_string(),
    })
}

/// `/sys/fs/cgroup`, the cgroup v2 mount this host's units live in.
fn cgroup_root() -> PathBuf {
    PathBuf::from("/sys/fs/cgroup")
}

/// One `systemctl --user` call, stdout only, killed after
/// [`SYSTEMCTL_LIMIT`].
fn systemctl(arguments: &[&str]) -> Result<String, String> {
    let mut command = Command::new("systemctl");
    command
        .arg("--user")
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_bounded(command)
}

/// Run `command`, returning its stdout when it exits 0 within
/// [`SYSTEMCTL_LIMIT`].
fn run_bounded(mut command: Command) -> Result<String, String> {
    let mut child = command
        .spawn()
        .map_err(|error| format!("systemctl: {error}"))?;
    let deadline = Instant::now() + SYSTEMCTL_LIMIT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|error| format!("systemctl: {error}"))?;
                if status.success() {
                    return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
                }
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!(
                    "systemctl --user {} failed: {}",
                    command
                        .get_args()
                        .map(|arg| arg.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join(" "),
                    stderr.trim()
                ));
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("systemctl --user did not answer within 10 s".to_string());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(format!("systemctl: {error}")),
        }
    }
}

/// `MemAvailable` of /proc/meminfo, in bytes.
#[must_use]
pub fn host_available() -> Option<u64> {
    available_bytes(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

/// The `MemAvailable` line of a /proc/meminfo text, in bytes.
#[must_use]
pub fn available_bytes(meminfo: &str) -> Option<u64> {
    let line = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))?;
    let mut words = line.split_whitespace();
    let kib: u64 = words.next()?.parse().ok()?;
    match words.next() {
        None | Some("kB") => Some(kib.saturating_mul(KIB)),
        Some(_) => None,
    }
}

impl Envelope {
    /// The scope an execution runs in: `<slice stem>-<execution id>.scope`,
    /// `spawn-exec_12.scope` in `spawn.slice`.
    #[must_use]
    pub fn execution_scope(&self, execution: &ExecutionId) -> String {
        format!("{}-{execution}.scope", self.settings.slice_stem())
    }

    /// The scope a gate runs in: `<slice stem>-gate-<task id>-<n>.scope`,
    /// `n` that task's judging from 1.
    #[must_use]
    pub fn gate_scope(&self, task: i64, run: u32) -> String {
        format!("{}-gate-{task}-{run}.scope", self.settings.slice_stem())
    }

    /// The slice's cgroup directory.
    #[must_use]
    pub fn slice_cgroup(&self) -> PathBuf {
        cgroup_root()
            .join(self.manager.control_group.trim_start_matches('/'))
            .join(&self.settings.slice)
    }

    /// A unit's cgroup directory, inside the slice.
    #[must_use]
    pub fn scope_cgroup(&self, scope: &str) -> PathBuf {
        self.slice_cgroup().join(scope)
    }

    /// Give the slice its limits, before any execution or gate starts:
    /// `systemctl --user set-property --runtime` writes a drop-in under
    /// `$XDG_RUNTIME_DIR/systemd/user.control`, so the slice needs no unit
    /// file.
    pub fn apply_slice(&self) -> Result<(), String> {
        let high = format!("MemoryHigh={}", self.settings.slice_high);
        let max = format!("MemoryMax={}", self.settings.slice_max);
        let weight = format!("CPUWeight={}", self.settings.cpu_weight);
        run_bounded({
            let mut command = Command::new("systemctl");
            command
                .arg("--user")
                .arg("set-property")
                .arg("--runtime")
                .arg(&self.settings.slice)
                .arg(high)
                .arg(max)
                .arg(weight)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            command
        })?;
        Ok(())
    }

    /// The first measure that does not fit an envelope of `envelope_max`
    /// bytes, the slice first: the slice's anonymous and shared memory above
    /// its MemoryMax minus the envelope's, or `MemAvailable` under the
    /// envelope's MemoryMax. `None`: it fits (a measure that cannot be read
    /// decides nothing).
    #[must_use]
    pub fn shortfall(&self, envelope_max: u64) -> Option<Shortfall> {
        shortfall_of(
            self.settings.slice_max,
            envelope_max,
            self.slice_measure(),
            host_available(),
        )
    }

    /// The slice's memory that cannot be given back: `anon` + `shmem` of its
    /// `memory.stat`. Page cache is reclaimed under the limit and is not
    /// counted.
    #[must_use]
    pub fn slice_measure(&self) -> Option<u64> {
        let stat = std::fs::read_to_string(self.slice_cgroup().join("memory.stat")).ok()?;
        let mut sum: Option<u64> = None;
        for line in stat.lines() {
            let mut words = line.split_whitespace();
            let Some(key) = words.next() else { continue };
            if key != "anon" && key != "shmem" {
                continue;
            }
            let value: u64 = words.next()?.parse().ok()?;
            sum = Some(sum.unwrap_or(0).saturating_add(value));
        }
        sum
    }

    /// Read a scope's cgroup and unit now.
    #[must_use]
    pub fn scope_facts(&self, scope: &str) -> ScopeFacts {
        let mut facts = ScopeFacts {
            scope: scope.to_string(),
            ..ScopeFacts::default()
        };
        let directory = self.scope_cgroup(scope);
        facts.memory_current = read_u64(&directory.join("memory.current"));
        facts.memory_peak = read_u64(&directory.join("memory.peak"));
        facts.memory_high = read_u64(&directory.join("memory.high"));
        facts.memory_max = read_u64(&directory.join("memory.max"));
        facts.memory_swap_current = read_u64(&directory.join("memory.swap.current"));
        facts.memory_swap_max = read_u64(&directory.join("memory.swap.max"));
        if let Ok(events) = std::fs::read_to_string(directory.join("memory.events")) {
            for line in events.lines() {
                let mut words = line.split_whitespace();
                let Some(key) = words.next() else { continue };
                let value = words.next().and_then(|text| text.parse::<u64>().ok());
                match key {
                    "high" => facts.throttled = value,
                    "oom_kill" => facts.oom_kills = value.unwrap_or(0),
                    _ => {}
                }
            }
        }
        if let Ok(text) = systemctl(&["show", scope, "-p", "Result", "-p", "LoadState"]) {
            for line in text.lines() {
                if let Some(result) = line.strip_prefix("Result=") {
                    facts.result = Some(result.trim().to_string());
                } else if let Some(state) = line.strip_prefix("LoadState=") {
                    facts.load_state = Some(state.trim().to_string());
                }
            }
        }
        if facts.load_state.as_deref() == Some("not-found") {
            facts.result = None;
        }
        facts
    }

    /// Forget a scope whose end is recorded: `reset-failed` removes a failed
    /// transient unit (a collected one no longer says why it ended, so the
    /// unit's Result is read first and the unit then reset). A live or
    /// successfully finished scope needs nothing.
    pub fn reset_failed(&self, scope: &str) {
        let _ = run_bounded({
            let mut command = Command::new("systemctl");
            command
                .arg("--user")
                .arg("reset-failed")
                .arg(scope)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            command
        });
    }

    /// The program and arguments that run `program` and `arguments` in
    /// `unit` inside the slice, through `systemd-run --user --scope ... --
    /// <program> <arguments>`. `OOMPolicy=stop` ends the rest of the scope
    /// when its memory limit kills a process; no `--collect`, so a failed
    /// scope's Result says why it ended until it is read and reset.
    ///
    /// `systemd-run` is started by a shell in the background (`"$@" <&3 &
    /// wait $!`, with the pane's standard input kept), not exec'd in the
    /// pane's place: a terminal server that moves a new pane into a scope of
    /// its own (tmux does) may do so after the pane's process was moved into
    /// this one, and it names the pane's own pid, not a child of it. The
    /// shell waits, so the pane lives exactly as long as the execution does.
    #[must_use]
    pub fn wrap(
        &self,
        unit: &str,
        limits: Limits,
        program: &str,
        arguments: &[String],
    ) -> (String, Vec<String>) {
        let mut words: Vec<String> = [
            "--user",
            // The pane is the engine's screen: systemd-run says nothing on
            // it (the unit is in the execution's record).
            "--quiet",
            "--scope",
            "--unit",
            unit,
            "--slice",
            &self.settings.slice,
            "-p",
        ]
        .iter()
        .map(|word| (*word).to_string())
        .collect();
        words.push(format!("MemoryHigh={}", limits.high));
        words.push("-p".to_string());
        words.push(format!("MemoryMax={}", limits.max));
        words.push("-p".to_string());
        words.push(format!("MemorySwapMax={}", limits.swap_max));
        words.push("-p".to_string());
        words.push("OOMPolicy=stop".to_string());
        words.push("--".to_string());
        words.push(program.to_string());
        words.extend(arguments.iter().cloned());
        let mut shell = vec![
            "-c".to_string(),
            // A background command's standard input would be /dev/null in a
            // non-interactive shell: the pane's terminal is kept for it.
            r#"exec 3<&0; "$@" <&3 3<&- & wait $!"#.to_string(),
            "sh".to_string(),
            "systemd-run".to_string(),
        ];
        shell.extend(words);
        ("/bin/sh".to_string(), shell)
    }

    /// The `host.envelope` record of a host that runs executions in
    /// envelopes: the slice and every setting, in bytes.
    #[must_use]
    pub fn host_record(&self) -> Value {
        json!({
            "type": "host.envelope",
            "state": "systemd",
            "slice": self.settings.slice,
            "memory_high": self.settings.slice_high,
            "memory_max": self.settings.slice_max,
            "cpu_weight": self.settings.cpu_weight,
            "execution_memory_high": self.settings.execution.high,
            "execution_memory_max": self.settings.execution.max,
            "execution_memory_swap_max": self.settings.execution.swap_max,
            "gate_memory_high": self.settings.gate.high,
            "gate_memory_max": self.settings.gate.max,
            "gate_memory_swap_max": self.settings.gate.swap_max,
        })
    }

    /// The `host.envelope` record of a host with no reachable user manager
    /// (or one that does not delegate memory): executions run as before.
    #[must_use]
    pub fn unavailable_record(reason: &str) -> Value {
        json!({"type": "host.envelope", "state": "unavailable", "error": reason})
    }

    /// The `host.envelope` record of a host told SPAWN_ENVELOPE=off.
    #[must_use]
    pub fn off_record() -> Value {
        json!({"type": "host.envelope", "state": "off"})
    }
}

/// The admission decision (objective 029): an envelope of `envelope_max`
/// bytes fits when the slice's memory that cannot be given back (`slice`,
/// absent = 0: nothing runs in it yet) is at most the slice's MemoryMax minus
/// the envelope's, and the host's `MemAvailable` is at least the envelope's.
/// The slice is checked first; `None` means it fits. A host measure that
/// cannot be read decides nothing.
#[must_use]
pub fn shortfall_of(
    slice_max: u64,
    envelope_max: u64,
    slice: Option<u64>,
    host: Option<u64>,
) -> Option<Shortfall> {
    let threshold = slice_max.saturating_sub(envelope_max);
    let measured = slice.unwrap_or(0);
    if measured > threshold {
        return Some(Shortfall {
            measure: "slice",
            measured,
            threshold,
        });
    }
    let threshold = envelope_max;
    let measured = host?;
    (measured < threshold).then_some(Shortfall {
        measure: "host",
        measured,
        threshold,
    })
}

/// A cgroup file's number, `None` when absent or not a number (`max`).
fn read_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::{
        Admission, admit, admitted, shortfall_of,
        DEFAULT_EXECUTION_MEMORY_HIGH, DEFAULT_EXECUTION_MEMORY_MAX, DEFAULT_GATE_MEMORY_HIGH,
        DEFAULT_GATE_MEMORY_MAX, DEFAULT_MEMORY_SWAP_MAX, DEFAULT_SLICE, DEFAULT_SLICE_CPU_WEIGHT,
        DEFAULT_SLICE_MEMORY_HIGH, DEFAULT_SLICE_MEMORY_MAX, Envelope, GIB, Limits, MIB, Manager,
        Mode, Settings, available_bytes, parse_size,
    };

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

    fn envelope(slice: &str) -> Envelope {
        Envelope {
            settings: Settings {
                mode: Mode::Systemd,
                slice: slice.to_string(),
                slice_high: 128 * MIB,
                slice_max: 144 * MIB,
                cpu_weight: 50,
                execution: Limits {
                    high: 92 * MIB,
                    max: 96 * MIB,
                    swap_max: 16 * MIB,
                },
                gate: Limits {
                    high: 30 * MIB,
                    max: 32 * MIB,
                    swap_max: 8 * MIB,
                },
            },
            manager: Manager {
                control_group: "/user.slice/user-1000.slice/user@1000.service".to_string(),
                runtime_dir: "/run/user/1000".to_string(),
            },
        }
    }

    #[test]
    fn sizes_are_bytes_or_powers_of_1024() {
        assert_eq!(parse_size("16G"), Some(16 * GIB));
        assert_eq!(parse_size("128M"), Some(128 * MIB));
        assert_eq!(parse_size("4g"), Some(4 * GIB));
        assert_eq!(parse_size("512K"), Some(512 * 1024));
        assert_eq!(parse_size("2T"), Some(2 * 1024 * GIB));
        assert_eq!(parse_size("100663296"), Some(100_663_296));
        assert_eq!(parse_size(" 92M "), Some(92 * MIB));
        assert_eq!(parse_size("G"), None);
        assert_eq!(parse_size("12.5M"), None);
        assert_eq!(parse_size("-1M"), None);
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("max"), None);
    }

    #[test]
    fn the_defaults_are_the_owners_ruling() {
        let settings = Settings::from_lookup(environment(&[])).expect("defaults");
        assert_eq!(settings.mode, Mode::Systemd);
        assert_eq!(settings.slice, DEFAULT_SLICE);
        assert_eq!(settings.slice_high, DEFAULT_SLICE_MEMORY_HIGH);
        assert_eq!(settings.slice_max, DEFAULT_SLICE_MEMORY_MAX);
        assert_eq!(settings.cpu_weight, DEFAULT_SLICE_CPU_WEIGHT);
        assert_eq!(settings.execution.high, DEFAULT_EXECUTION_MEMORY_HIGH);
        assert_eq!(settings.execution.max, DEFAULT_EXECUTION_MEMORY_MAX);
        assert_eq!(settings.execution.swap_max, DEFAULT_MEMORY_SWAP_MAX);
        assert_eq!(settings.gate.high, DEFAULT_GATE_MEMORY_HIGH);
        assert_eq!(settings.gate.max, DEFAULT_GATE_MEMORY_MAX);
        assert_eq!(settings.gate.swap_max, DEFAULT_MEMORY_SWAP_MAX);
        assert_eq!(settings.slice_stem(), "spawn");
    }

    #[test]
    fn a_setting_that_does_not_parse_is_refused_with_its_name() {
        for (name, value) in [
            ("SPAWN_SLICE_MEMORY_HIGH", "16X"),
            ("SPAWN_SLICE_MEMORY_MAX", "twenty"),
            ("SPAWN_EXECUTION_MEMORY_HIGH", "10GB"),
            ("SPAWN_EXECUTION_MEMORY_MAX", "12.5G"),
            ("SPAWN_EXECUTION_MEMORY_SWAP_MAX", "lots"),
            ("SPAWN_GATE_MEMORY_HIGH", "-1G"),
        ] {
            let error = Settings::from_lookup(environment(&[(name, value)]))
                .expect_err("refused");
            if !value.is_empty() {
                assert!(error.contains(name), "{name} in {error}");
            }
        }
        let error = Settings::from_lookup(environment(&[("SPAWN_SLICE_CPU_WEIGHT", "0")]))
            .expect_err("refused");
        assert!(error.contains("SPAWN_SLICE_CPU_WEIGHT"), "{error}");
        let error = Settings::from_lookup(environment(&[("SPAWN_ENVELOPE", "maybe")]))
            .expect_err("refused");
        assert!(error.contains("SPAWN_ENVELOPE"), "{error}");
    }

    #[test]
    fn envelopes_must_nest() {
        let error = Settings::from_lookup(environment(&[
            ("SPAWN_SLICE_MEMORY_HIGH", "20G"),
            ("SPAWN_SLICE_MEMORY_MAX", "16G"),
        ]))
        .expect_err("a slice high above its max");
        assert!(error.contains("SPAWN_SLICE_MEMORY_HIGH"), "{error}");
        let error = Settings::from_lookup(environment(&[
            ("SPAWN_EXECUTION_MEMORY_HIGH", "12G"),
            ("SPAWN_EXECUTION_MEMORY_MAX", "10G"),
        ]))
        .expect_err("an envelope high above its max");
        assert!(error.contains("SPAWN_EXECUTION_MEMORY_HIGH"), "{error}");
        let error = Settings::from_lookup(environment(&[
            ("SPAWN_SLICE_MEMORY_HIGH", "7G"),
            ("SPAWN_SLICE_MEMORY_MAX", "8G"),
            ("SPAWN_EXECUTION_MEMORY_MAX", "12G"),
        ]))
        .expect_err("an envelope above its slice");
        assert!(error.contains("SPAWN_EXECUTION_MEMORY_MAX"), "{error}");
        let error = Settings::from_lookup(environment(&[
            ("SPAWN_SLICE_MEMORY_HIGH", "7G"),
            ("SPAWN_SLICE_MEMORY_MAX", "8G"),
            ("SPAWN_EXECUTION_MEMORY_HIGH", "5G"),
            ("SPAWN_EXECUTION_MEMORY_MAX", "6G"),
            ("SPAWN_GATE_MEMORY_MAX", "12G"),
        ]))
        .expect_err("a gate above its slice");
        assert!(error.contains("SPAWN_GATE_MEMORY_MAX"), "{error}");
    }

    #[test]
    fn a_named_slice_keeps_its_name_and_gets_its_suffix() {
        let settings =
            Settings::from_lookup(environment(&[("SPAWN_SLICE", "bb029_1_2")])).expect("named");
        assert_eq!(settings.slice, "bb029_1_2.slice");
        assert_eq!(settings.slice_stem(), "bb029_1_2");
        let settings =
            Settings::from_lookup(environment(&[("SPAWN_SLICE", "mine.slice")])).expect("named");
        assert_eq!(settings.slice, "mine.slice");
        assert_eq!(settings.slice_stem(), "mine");
    }

    #[test]
    fn off_runs_as_before() {
        let settings =
            Settings::from_lookup(environment(&[("SPAWN_ENVELOPE", "off")])).expect("off");
        assert_eq!(settings.mode, Mode::Off);
    }

    #[test]
    fn scopes_are_named_after_the_slice_and_the_execution() {
        let envelope = envelope("bb029_1_2.slice");
        let execution = crate::id::ExecutionId::new("exec_1").expect("an id");
        assert_eq!(envelope.execution_scope(&execution), "bb029_1_2-exec_1.scope");
        assert_eq!(envelope.gate_scope(7, 1), "bb029_1_2-gate-7-1.scope");
        assert_eq!(envelope.gate_scope(7, 2), "bb029_1_2-gate-7-2.scope");
        assert_eq!(
            envelope.scope_cgroup("bb029_1_2-exec_1.scope"),
            std::path::PathBuf::from(
                "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/bb029_1_2.slice/\
                 bb029_1_2-exec_1.scope"
            )
        );
        let plain = self::envelope("spawn.slice");
        assert_eq!(plain.execution_scope(&execution), "spawn-exec_1.scope");
    }

    #[test]
    fn the_wrapper_carries_every_limit() {
        let envelope = envelope("bb029_1_2.slice");
        let (program, words) = envelope.wrap(
            "bb029_1_2-exec_1.scope",
            envelope.settings.execution,
            "/usr/bin/spawn",
            &["run".to_string(), "--".to_string()],
        );
        assert_eq!(program, "/bin/sh");
        assert_eq!(
            words.join(" "),
            "-c exec 3<&0; \"$@\" <&3 3<&- & wait $! sh systemd-run --user --quiet --scope --unit \
             bb029_1_2-exec_1.scope --slice bb029_1_2.slice -p MemoryHigh=96468992 \
             -p MemoryMax=100663296 -p MemorySwapMax=16777216 -p OOMPolicy=stop -- \
             /usr/bin/spawn run --"
        );
    }

    #[test]
    fn the_host_record_names_the_slice_and_every_setting_in_bytes() {
        let record = envelope("bb029_1_2.slice").host_record();
        assert_eq!(record["type"], "host.envelope");
        assert_eq!(record["state"], "systemd");
        assert_eq!(record["slice"], "bb029_1_2.slice");
        assert_eq!(record["memory_high"], 128 * MIB);
        assert_eq!(record["execution_memory_max"], 96 * MIB);
        assert_eq!(record["execution_memory_swap_max"], 16 * MIB);
        assert_eq!(record["gate_memory_max"], 32 * MIB);
        assert_eq!(record["cpu_weight"], 50);
        let unavailable = Envelope::unavailable_record("no user bus");
        assert_eq!(unavailable["state"], "unavailable");
        assert_eq!(unavailable["error"], "no user bus");
        assert_eq!(Envelope::off_record()["state"], "off");
    }

    #[test]
    fn admission_is_the_slice_first_then_the_host() {
        let gib = GIB;
        // Fits: 2 GiB held under a 20 GiB slice leaves 12 GiB for a 12 GiB
        // envelope; 20 GiB available on the host.
        assert_eq!(super::shortfall_of(20 * gib, 12 * gib, Some(2 * gib), Some(20 * gib)), None);
        // An empty slice measures nothing.
        assert_eq!(super::shortfall_of(20 * gib, 12 * gib, None, Some(20 * gib)), None);
        // The slice short: 9 GiB > 20 - 12.
        let short = super::shortfall_of(20 * gib, 12 * gib, Some(9 * gib), Some(20 * gib))
            .expect("the slice is short");
        assert_eq!(short.measure, "slice");
        assert_eq!(short.measured, 9 * gib);
        assert_eq!(short.threshold, 8 * gib);
        // The host short: 11 GiB < a 12 GiB envelope.
        let short = super::shortfall_of(20 * gib, 12 * gib, Some(2 * gib), Some(11 * gib))
            .expect("the host is short");
        assert_eq!(short.measure, "host");
        assert_eq!(short.threshold, 12 * gib);
        // Both short: the slice is the one named.
        let short = super::shortfall_of(20 * gib, 12 * gib, Some(9 * gib), Some(gib)).expect("short");
        assert_eq!(short.measure, "slice");
        // A host measure that cannot be read decides nothing.
        assert_eq!(super::shortfall_of(20 * gib, 12 * gib, Some(2 * gib), None), None);
        // The scenario's own numbers: 144 MiB slice, 96 MiB envelope, 49 MiB
        // held means the threshold 48 MiB is passed.
        let mib = MIB;
        let short = super::shortfall_of(144 * mib, 96 * mib, Some(49 * mib), Some(1024 * mib))
            .expect("the slice is short");
        assert_eq!(short.threshold, 48 * mib);
        assert_eq!(short.measure, "slice");
        assert_eq!(super::shortfall_of(144 * mib, 96 * mib, Some(48 * mib), Some(96 * mib)), None);
    }

    #[test]
    fn an_exit_status_alone_is_no_memory_limit() {
        // A scope that ended by a signal leaves no kill of its own: the
        // exit code 137 says nothing (any SIGKILL makes it).
        let facts = super::ScopeFacts {
            scope: "spawn-exec_1.scope".to_string(),
            result: Some("success".to_string()),
            load_state: Some("not-found".to_string()),
            ..super::ScopeFacts::default()
        };
        assert!(!facts.oom_killed());
        assert_eq!(facts.evidence(100).get("memory_peak"), None);
    }

    /// A control stream in memory: the records `admit` and `admitted` write,
    /// read back as the store's queries would find them, and a clock the
    /// test moves.
    #[derive(Debug, Default)]
    struct Log {
        records: std::sync::Mutex<Vec<(serde_json::Value, i64)>>,
        now: std::sync::Mutex<i64>,
    }

    impl Log {
        fn records(&self) -> Vec<serde_json::Value> {
            self.records
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .map(|(record, _)| record.clone())
                .collect()
        }

        fn at(&self, now: i64) {
            *self.now.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = now;
        }
    }

    impl crate::port::HostRecords for Log {
        fn record(
            &self,
            record: &serde_json::Value,
        ) -> Result<crate::port::Recorded, crate::port::StoreError> {
            let mut records = self
                .records
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let at = *self.now.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            records.push((record.clone(), at));
            Ok(crate::port::Recorded {
                id: i64::try_from(records.len()).unwrap_or(i64::MAX),
                at_ms: at,
            })
        }

        fn admission_record(
            &self,
            record_type: &str,
            kind: &str,
            field: &str,
            value: &str,
            _after: i64,
        ) -> Result<Option<i64>, crate::port::StoreError> {
            let records = self
                .records
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Ok(records
                .iter()
                .find(|(record, _)| {
                    record["type"] == record_type
                        && record["kind"] == kind
                        && record[field].to_string().trim_matches('"') == value
                })
                .map(|(_, at)| *at))
        }

        fn interrupted(&self, _execution: &str) -> Result<bool, crate::port::StoreError> {
            Ok(false)
        }

        fn now_ms(&self) -> Result<i64, crate::port::StoreError> {
            Ok(*self.now.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
        }
    }

    /// An execution's admission, as the Bridge makes it.
    fn execution() -> Admission<'static> {
        Admission {
            kind: "execution",
            fields: serde_json::json!({"agent_id": "agent_resident_late",
                                       "session_id": "session_resident_late",
                                       "command_id": 7}),
            envelope_bytes: 96 * MIB,
            after: 0,
        }
    }

    #[test]
    fn a_start_that_fits_writes_nothing() {
        let log = Log::default();
        assert!(admit(&log, &execution(), None).expect("it fits"));
        admitted(&log, &execution()).expect("nothing to admit");
        assert!(log.records().is_empty(), "{:?}", log.records());
    }

    #[test]
    fn a_wait_is_recorded_once_and_admitted_with_its_waited_ms() {
        let log = Log::default();
        log.at(1_000);
        let short = shortfall_of(144 * MIB, 96 * MIB, Some(60 * MIB), Some(20 * GIB))
            .expect("the slice is short");
        assert!(!admit(&log, &execution(), Some(short)).expect("held"));
        // Decided again at the next pass: the same wait, not a second one.
        log.at(1_400);
        assert!(!admit(&log, &execution(), Some(short)).expect("still held"));
        let waiting = log.records();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0]["type"], "admission.waiting");
        assert_eq!(waiting[0]["kind"], "execution");
        assert_eq!(waiting[0]["measure"], "slice");
        assert_eq!(waiting[0]["envelope_bytes"], 96 * MIB);
        assert_eq!(waiting[0]["threshold_bytes"], 48 * MIB);
        assert!(waiting[0]["measured_bytes"].as_u64() > Some(48 * MIB));
        assert_eq!(waiting[0]["command_id"], 7);
        // It fits now: admitted once, from the wait's first decision.
        log.at(5_000);
        assert!(admit(&log, &execution(), None).expect("it fits"));
        admitted(&log, &execution()).expect("admitted");
        admitted(&log, &execution()).expect("not twice");
        let records = log.records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1]["type"], "admission.admitted");
        assert_eq!(records[1]["waited_ms"], 4_000);
        assert_eq!(records[1]["envelope_bytes"], 96 * MIB);
        assert_eq!(records[1]["agent_id"], "agent_resident_late");
    }

    #[test]
    fn a_gates_wait_names_its_task_and_no_agent() {
        let log = Log::default();
        let gate = Admission {
            kind: "gate",
            fields: serde_json::json!({"task": 9}),
            envelope_bytes: 32 * MIB,
            after: 41,
        };
        let short = shortfall_of(144 * MIB, 32 * MIB, Some(2 * MIB), Some(8 * MIB))
            .expect("the host is short");
        assert!(!admit(&log, &gate, Some(short)).expect("held"));
        let waiting = log.records();
        assert_eq!(waiting[0]["kind"], "gate");
        assert_eq!(waiting[0]["task"], 9);
        assert_eq!(waiting[0]["measure"], "host");
        assert_eq!(waiting[0]["threshold_bytes"], 32 * MIB);
        assert!(waiting[0].get("agent_id").is_none());
    }

    #[test]
    fn mem_available_is_read_in_bytes() {
        let meminfo = "MemTotal:       32800000 kB\nMemAvailable:   19999999 kB\nSwapTotal: 0 kB\n";
        assert_eq!(available_bytes(meminfo), Some(19_999_999 * 1024));
        assert_eq!(available_bytes("MemTotal: 1 kB\n"), None);
        assert_eq!(available_bytes("MemAvailable: x kB\n"), None);
    }

    #[test]
    fn an_oom_kill_is_read_from_the_counter_or_the_units_result() {
        let mut facts = super::ScopeFacts::default();
        assert!(!facts.oom_killed());
        facts.oom_kills = 1;
        assert!(facts.oom_killed());
        let mut facts = super::ScopeFacts {
            result: Some("oom-kill".to_string()),
            ..super::ScopeFacts::default()
        };
        assert!(facts.oom_killed());
        let evidence = facts.evidence(100_663_296);
        assert_eq!(evidence["memory_max"], 100_663_296);
        assert_eq!(evidence["oom_kills"], 1);
        facts.memory_peak = Some(100_000_000);
        assert_eq!(facts.evidence(1)["memory_peak"], 100_000_000);
    }
}
