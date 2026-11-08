//! Running the gate (`scripts/rsi/gate.sh`, docs/fleet-architecture.md
//! §10.2): one black-box judge for every landing, with a wall clock.
//!
//! The verdict is the file the gate writes at `SPAWN_GATE_VERDICT`, in a
//! private directory this process creates and the gate's sandbox hides, and
//! it counts only when the gate exits 0. Stdout is the record, not the
//! verdict: candidate code once wrote an accepted line to the gate's stdout
//! through /proc and killed the gate. A gate that exits non-zero, or dies
//! of a signal, is an abort whatever it printed, with one exception: exit
//! [`NO_ROOM_EXIT`] *and* a no-room fact in the verdict file
//! (`{"no_room":true,"free_kb":n,"min_gb":n,"tmpdir":"…"}`) means TMPDIR had
//! no room to judge ([`GateFailure::NoRoom`]). The gate writes that file
//! before any candidate code runs (scripts/disk.sh `gate_room`).
//!
//! A TMPDIR with no room at all may leave no fact: the gate cannot write
//! its verdict file, or this process cannot create its private directory.
//! Then this process measures the free space itself ([`judge`]'s `free_kb`,
//! `df` in production): of the directory it could not create in, or of the
//! gate's TMPDIR after an exit [`NO_ROOM_EXIT`] with no readable fact. It
//! records [`GateFailure::NoRoom`] with its own measurement only when that is
//! under the floor (SPAWN_GATE_MIN_FREE_GB, default 10). Otherwise the run is
//! an abort, as is a fact with any other exit status.
//!
//! **A host fault is no verdict either.** Exit [`HOST_FAULT_EXIT`] *and* a
//! host-fault fact in the verdict file (`{"host_fault":true,"scenario",
//! "on","runs","reason"}`) means the gate found a scenario failing where the
//! candidate is not to blame ([`GateFailure::HostFault`]): a scenario's
//! harness error on the base, before any candidate code ran (`on` `base`),
//! or a check that failed twice on the candidate and once on the base (`on`
//! `candidate and base`). The integrator records `host_fault`, retried after
//! a backoff and counted toward no halt. The fact counts only from the
//! verdict file the sandbox never sees, and only with that exit; exit 6
//! without it is an abort.
//!
//! **The host's room is measured before a gate, not learned from its
//! crash.** Before it starts a gate, the integrator checks the host
//! ([`Room::check`]), in this order, stopping at the first that falls short:
//! free disk in the gate's TMPDIR against SPAWN_GATE_MIN_FREE_GB (the
//! floor `scripts/disk.sh` reads too), `MemAvailable` against
//! SPAWN_GATE_MIN_FREE_MEMORY_MB (default 2048 MiB), and, when
//! SPAWN_TEST_DATABASE_URL is set (and the mission's gate gets it: not one
//! marked `no-test-database`), the test database: a connection, then a
//! forced parallel query, each within 10 s, so a server without the shared
//! memory a parallel query needs (/dev/shm in a container) is found before
//! the workspace tests are. A check that falls short ([`Shortfall`]) means
//! the gate does not run; the integrator records `no_room` with the check,
//! its measure and its threshold, which is no verdict on the work. A gate
//! that ran 25 minutes into a crashed test database is never needed to
//! learn the host had no room. A threshold of 0 disables its check. The
//! gate's own exit 5 above stays: a TMPDIR that fills between the check and
//! the gate's start is still found.
//!
//! **A base that disagrees with itself is the host's, not a verdict.** The
//! gate judges every objective scenario on the base before any candidate
//! code runs, and an unsolved result needs two runs that agree. A first run
//! that exits 1 and a second that exits 0 is no verdict on the candidate: no
//! candidate code has run yet, and the base is code that already landed. The
//! gate says so itself, and exits [`BASE_DISAGREES_EXIT`] with its own pair
//! of lines at the end of stdout (`| <name> exited 1, then 0 on the base: a
//! harness error, not a verdict; the gate aborts`, then `| stage gate aborted
//! <ms>ms base: <name> again (unsolved needs two runs that agree)`), its own
//! trailing lines last when it printed them (its `| base runs:` phase line
//! and its `| gate time:` line) and nothing else. That pair, with exit
//! 4, is [`GateFailure::BaseDisagrees`]: the integrator records it
//! `host_fault` (objective 020's verdict) with the evidence `base_disagrees`,
//! never a count toward a halt. Every other harness error on the base (a
//! first run that exits 2, `1, then 2`, `1, then 3`, a missing line, the
//! pair with any other exit) stays an abort.
//!
//! **The gate runs in a resource envelope of its own** (objective 029): a
//! transient systemd user scope `<slice stem>-gate-<task id>-<n>.scope`
//! directly inside Spawn's slice, with SPAWN_GATE_MEMORY_HIGH, _MAX and
//! _SWAP_MAX and OOMPolicy stop, so everything the gate starts (bwrap, the
//! builds, the scenarios and their engines) is in that scope's cgroup. The
//! gate's process group and wall-clock kill work as before (`systemd-run`
//! execs the gate in place, so the group is the gate's). A gate its own
//! envelope kills — its scope's `memory.events` `oom_kill` above 0, or the
//! unit's `Result` `oom-kill`, read at its end, never its exit status alone
//! — is [`GateFailure::MemoryLimit`]: the host's fault, not a verdict. The
//! unit is then reset, so no unit is left behind; a scope that ended another
//! way needs nothing (a transient scope that stops is collected). The
//! integrator admits a gate whose envelope fits under the slice and on the
//! host ([`crate::integrate`]).
//!
//! The gate runs with a cleared environment: [`GATE_ENV`] and nothing else.
//! The integrator's own database URL never reaches candidate code.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// The integrator's variables the gate receives (scripts/rsi/gate.sh lists
/// what it reads). Everything else, the owner's SPAWN_DATABASE_URL first,
/// stays out.
pub(crate) const GATE_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "TMPDIR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "SPAWN_TEST_DATABASE_URL",
    // The free space the gate wants in TMPDIR before it starts
    // (scripts/disk.sh); read outside the sandbox only. The integrator
    // checks the same floor before it starts the gate ([`Room`]); its
    // memory floor, SPAWN_GATE_MIN_FREE_MEMORY_MB, is its own and stays out.
    "SPAWN_GATE_MIN_FREE_GB",
    // How many scenarios the gate runs at once, and the test server's
    // programs it runs a server of its own from (GitHub issue #6): a docker
    // image, or the host's directory of them; all read outside the sandbox
    // only. A gate that gets no test database
    // ([`crate::integrate::NO_TEST_DATABASE`]) gets no server either.
    "SPAWN_GATE_JOBS",
    "SPAWN_GATE_PG_IMAGE",
    "SPAWN_GATE_PG_BIN",
];

/// The gate's exit status when TMPDIR has no room to judge now
/// (scripts/rsi/gate.sh). It counts only with the no-room fact in the
/// verdict file.
pub(crate) const NO_ROOM_EXIT: i32 = 5;

/// The gate's exit status when it found a host or harness fault instead of
/// a verdict (scripts/rsi/gate.sh `host_fault`). It counts only with the
/// host-fault fact in the verdict file.
pub(crate) const HOST_FAULT_EXIT: i32 = 6;

/// The gate's exit status when a scenario's two base runs disagreed
/// (scripts/rsi/gate.sh: the first exited 1, the second did not exit 1). It
/// counts only with the gate's own pair of lines at the end of its stdout
/// ([`base_disagrees`]); with any other output it is an abort.
pub(crate) const BASE_DISAGREES_EXIT: i32 = 4;

/// The gate's `| gate time:` line, one of the two it may end a
/// base-disagreement with.
const GATE_TIME_LINE: &str = "| gate time:";

/// The gate's `| base runs:` line (scripts/rsi/gate.sh's `phase_time`), the
/// other one: a gate run at 2026-11-08 05:02 printed it after the pair and
/// before the timing line, where the 2026-11-06 14:43 one printed none, so
/// both shapes are the gate's own.
const BASE_RUNS_LINE: &str = "| base runs:";

/// The tail of a base run's `exited` line
/// (`| <name> exited 1, then 0 on the base: …`).
const BASE_EXITED_TAIL: &str = " exited 1, then 0 on the base";

/// The rest of the gate's `exited` line, after the scenario's name.
const BASE_EXITED_SAID: &str = ": a harness error, not a verdict; the gate aborts";

/// The head of the stage line the gate closes a disagreeing base with.
const BASE_STAGE_HEAD: &str = "| stage gate aborted ";

/// The rest of that stage line, after its milliseconds and `base: <name>`.
const BASE_STAGE_SAID: &str = " again (unsolved needs two runs that agree)";

/// The gate's floor when SPAWN_GATE_MIN_FREE_GB is unset (scripts/disk.sh).
const DEFAULT_MIN_FREE_GB: u64 = 10;

/// The integrator's floor on available memory when
/// SPAWN_GATE_MIN_FREE_MEMORY_MB is unset, in MiB.
const DEFAULT_MIN_FREE_MEMORY_MB: u64 = 2048;

/// How long the test database has to connect, and then to run the parallel
/// query ([`Room`]).
const DATABASE_PROBE_LIMIT: Duration = Duration::from_secs(10);

/// The most scenarios a gate runs at once by default, as gate.sh caps its
/// own default.
const MAX_DEFAULT_JOBS: usize = 8;

/// The host's cores and how the integrator shares them among its gates
/// (gate-cpu-room). Each gate gets SPAWN_GATE_JOBS from the integrator
/// ([`Cpu::jobs_for`]), never its own default: the owner's
/// SPAWN_GATE_JOBS when set; 1 for a gate on the shared test server
/// (whose connections every agent's tests share, as gate.sh says); else
/// the cores less those kept for the workers, divided among the gates that
/// can run at once, at most 8. A gate starts only when its jobs, the
/// running gates' jobs and the cores kept for the workers fit within the
/// cores, or when no gate runs ([`Cpu::fits`]); otherwise it waits for the
/// next pass, as it waits for a gate slot: no record, no count. (2026-11-08:
/// with SPAWN_GATE_PG_BIN each gate took half of the 9 cores, two or three
/// gates and the workers made a load of 18-20, and a gate failed on its
/// base.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cpu {
    /// The host's cores (`available_parallelism`).
    pub(crate) cores: usize,
    /// Cores kept for the workers and the host: SPAWN_GATE_WORKER_CORES,
    /// a whole number from 0 to `cores` - 1, by default a quarter of the
    /// cores (at least 1).
    pub(crate) reserved: usize,
    /// SPAWN_GATE_JOBS as the owner set it: every gate runs that many.
    pub(crate) jobs: Option<usize>,
}

impl Cpu {
    /// The settings as the integrator's environment (`lookup`) sets them,
    /// for a host of `cores` cores. A setting that does not parse is an
    /// error: the integrator does not start, as for `--gates`.
    pub(crate) fn from_env(
        lookup: impl Fn(&str) -> Option<String>,
        cores: usize,
    ) -> Result<Self, String> {
        let cores = cores.max(1);
        let set = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
        let jobs = match set("SPAWN_GATE_JOBS") {
            None => None,
            Some(text) => Some(whole(&text).filter(|jobs| *jobs >= 1).ok_or_else(|| {
                format!("SPAWN_GATE_JOBS is a whole number from 1, not '{text}'")
            })?),
        };
        let reserved = match set("SPAWN_GATE_WORKER_CORES") {
            None => (cores / 4).max(1).min(cores - 1),
            Some(text) => whole(&text).filter(|kept| *kept < cores).ok_or_else(|| {
                format!(
                    "SPAWN_GATE_WORKER_CORES is a whole number from 0 to {}, not '{text}'",
                    cores - 1
                )
            })?,
        };
        Ok(Self {
            cores,
            reserved,
            jobs,
        })
    }

    /// The jobs of a gate when `at_once` gates can run at once; `shared`:
    /// the gate runs on the shared test server (it gets
    /// SPAWN_TEST_DATABASE_URL and no server of its own).
    pub(crate) fn jobs_for(&self, shared: bool, at_once: usize) -> usize {
        if let Some(jobs) = self.jobs {
            return jobs;
        }
        if shared {
            return 1;
        }
        ((self.cores - self.reserved) / at_once.max(1)).clamp(1, MAX_DEFAULT_JOBS)
    }

    /// Whether a gate of `jobs` may start beside running gates of
    /// `running` jobs in all: they and the cores kept for the workers fit
    /// within the cores, or no gate runs (one gate always may: a host whose
    /// cores cannot hold even one must still judge).
    pub(crate) fn fits(&self, running: usize, jobs: usize) -> bool {
        running == 0 || running + jobs + self.reserved <= self.cores
    }
}

/// A whole number as gate.sh reads one: digits only, no sign, no leading
/// zero (0 itself is a whole number).
fn whole(text: &str) -> Option<usize> {
    let text = text.trim();
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if text.len() > 1 && text.starts_with('0') {
        return None;
    }
    text.parse().ok()
}

/// Whether a gate whose environment is `env` runs on the shared test
/// server: it gets SPAWN_TEST_DATABASE_URL and neither SPAWN_GATE_PG_IMAGE
/// nor SPAWN_GATE_PG_BIN (gate.sh then defaults to one job).
pub(crate) fn on_shared_server(env: &[(String, String)]) -> bool {
    let set = |name: &str| env_value(env, name).is_some_and(|value| !value.is_empty());
    set("SPAWN_TEST_DATABASE_URL") && !set("SPAWN_GATE_PG_IMAGE") && !set("SPAWN_GATE_PG_BIN")
}

/// Free KiB of the filesystem holding a path, as this host reports it now.
fn measured_free_kb(path: &Path) -> Option<u64> {
    crate::disk::free_bytes(path).ok().map(|bytes| bytes / 1024)
}

/// MiB of memory this host has available now: `MemAvailable` of
/// /proc/meminfo.
fn measured_available_mb() -> Option<u64> {
    available_mb(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

/// The `MemAvailable` line of a /proc/meminfo text, in whole MiB.
fn available_mb(meminfo: &str) -> Option<u64> {
    let line = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))?;
    let mut words = line.split_whitespace();
    let kib: u64 = words.next()?.parse().ok()?;
    matches!(words.next(), None | Some("kB")).then_some(kib / 1024)
}

/// The test database's probe, as `spawn integrate` runs it.
fn probed_database(url: &str) -> Result<(), spawn_postgres::ProbeFailure> {
    spawn_postgres::parallel_query_probe(url, DATABASE_PROBE_LIMIT)
}

/// Free KiB of the filesystem holding a path ([`Room`]'s disk measure).
pub(crate) type FreeKb = dyn Fn(&Path) -> Option<u64> + Send + Sync;
/// Available MiB of memory ([`Room`]'s memory measure).
pub(crate) type AvailableMb = dyn Fn() -> Option<u64> + Send + Sync;
/// The test database's probe by URL ([`Room`]'s database measure).
pub(crate) type DatabaseProbe =
    dyn Fn(&str) -> Result<(), spawn_postgres::ProbeFailure> + Send + Sync;

/// What the integrator measures before it starts a gate, and against what
/// (objective 018). At the start of each judging, after the candidate is
/// built and before the gate starts, [`Room::check`] measures, in this
/// order, stopping at the first that falls short:
///
/// 1. disk: the free KiB (`df`) of the gate's TMPDIR (the `TMPDIR` of the
///    gate's environment, `/tmp` when unset) against
///    SPAWN_GATE_MIN_FREE_GB (default 10), the floor `scripts/disk.sh`
///    reads too: one floor, one meaning;
/// 2. memory: `MemAvailable` in MiB against SPAWN_GATE_MIN_FREE_MEMORY_MB
///    (default 2048), read by the integrator only;
/// 3. database: when SPAWN_TEST_DATABASE_URL is set in the integrator's
///    environment (whatever [`withhold_owners_cluster`] later decides for
///    the gate), a connection within 10 s, then a forced parallel query
///    within 10 s ([`spawn_postgres::parallel_query_probe`]). Any error
///    fails it. Not for a gate whose mission's owner said it needs no test
///    database (`no-test-database`, [`Room::without_test_database`]): that
///    gate never gets the URL, and a test server that is down must not hold
///    back work that never reaches it.
///
/// A threshold of 0 disables its check; one that does not parse is an
/// abort ([`RoomFailure::Bad`]), as a bad floor is to the gate (exit 3). A
/// measure that cannot be read decides nothing: the gate runs, and checks
/// its own floor. The three measures are seams for tests.
#[derive(Clone)]
pub(crate) struct Room {
    /// SPAWN_GATE_MIN_FREE_MEMORY_MB as the integrator's environment sets it.
    min_free_memory_mb: Option<String>,
    /// SPAWN_TEST_DATABASE_URL as the integrator's environment sets it.
    test_database_url: Option<String>,
    /// The gate gets no test database (its mission's `no-test-database`),
    /// so there is none to check.
    no_test_database: bool,
    free_kb: std::sync::Arc<FreeKb>,
    available_mb: std::sync::Arc<AvailableMb>,
    probe: std::sync::Arc<DatabaseProbe>,
}

/// What fell short before a gate: the check, what it measured, and the
/// threshold it measured against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Shortfall {
    /// Free KiB in the gate's TMPDIR under SPAWN_GATE_MIN_FREE_GB (GiB).
    Disk {
        free_kb: u64,
        min_gb: u64,
        tmpdir: String,
    },
    /// Available MiB under SPAWN_GATE_MIN_FREE_MEMORY_MB.
    Memory { available_mb: u64, min_mb: u64 },
    /// The test database failed a probe: the server (host:port/database or
    /// the socket directory, never the password), the probe and its error.
    Database {
        server: String,
        probe: String,
        error: String,
    },
}

impl Shortfall {
    /// The check's name in the record: `disk`, `memory` or `database`.
    pub(crate) fn check(&self) -> &'static str {
        match self {
            Self::Disk { .. } => "disk",
            Self::Memory { .. } => "memory",
            Self::Database { .. } => "database",
        }
    }

    /// What the check measured: free KiB, available MiB, or the error.
    pub(crate) fn measured(&self) -> serde_json::Value {
        match self {
            Self::Disk { free_kb, .. } => (*free_kb).into(),
            Self::Memory { available_mb, .. } => (*available_mb).into(),
            Self::Database { error, .. } => error.as_str().into(),
        }
    }

    /// What it measured against: the floor in KiB, the floor in MiB, or
    /// the probe it failed.
    pub(crate) fn threshold(&self) -> serde_json::Value {
        match self {
            Self::Disk { min_gb, .. } => min_gb.saturating_mul(1_048_576).into(),
            Self::Memory { min_mb, .. } => (*min_mb).into(),
            Self::Database { probe, .. } => probe.as_str().into(),
        }
    }

    /// The measure against its threshold, in one line for the log.
    fn line(&self) -> String {
        match self {
            Self::Disk {
                free_kb,
                min_gb,
                tmpdir,
            } => format!(
                "disk: {free_kb} KiB free in {tmpdir} (TMPDIR), under SPAWN_GATE_MIN_FREE_GB={min_gb} \
                 ({} KiB)",
                min_gb.saturating_mul(1_048_576)
            ),
            Self::Memory {
                available_mb,
                min_mb,
            } => format!(
                "memory: {available_mb} MiB available (MemAvailable), under \
                 SPAWN_GATE_MIN_FREE_MEMORY_MB={min_mb}"
            ),
            Self::Database {
                server,
                probe,
                error,
            } => format!("database: the test database ({server}) failed \"{probe}\": {error}"),
        }
    }
}

/// Why [`Room::check`] says the gate may not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RoomFailure {
    /// A check fell short: no room to judge now, not a verdict. `log` lists
    /// every check made, with its measure and threshold.
    Short { short: Shortfall, log: String },
    /// A threshold that does not parse: an abort, as the gate's exit 3 for a
    /// bad SPAWN_GATE_MIN_FREE_GB.
    Bad(String),
}

impl Room {
    /// The checks as the integrator's environment (`lookup`) sets them, on
    /// this host's measures.
    pub(crate) fn from_env(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let set = |name: &str| lookup(name).filter(|value| !value.is_empty());
        Self {
            min_free_memory_mb: set("SPAWN_GATE_MIN_FREE_MEMORY_MB"),
            test_database_url: set("SPAWN_TEST_DATABASE_URL"),
            no_test_database: false,
            free_kb: std::sync::Arc::new(measured_free_kb),
            available_mb: std::sync::Arc::new(measured_available_mb),
            probe: std::sync::Arc::new(probed_database),
        }
    }

    /// These checks for a gate that gets no test database (its mission's
    /// scenarios hold `no-test-database`): disk and memory only.
    pub(crate) fn without_test_database(&self) -> Self {
        Self {
            test_database_url: None,
            no_test_database: true,
            ..self.clone()
        }
    }

    /// These checks on other measures (tests).
    #[cfg(test)]
    pub(crate) fn measured_by(
        self,
        free_kb: impl Fn(&Path) -> Option<u64> + Send + Sync + 'static,
        available_mb: impl Fn() -> Option<u64> + Send + Sync + 'static,
        probe: impl Fn(&str) -> Result<(), spawn_postgres::ProbeFailure> + Send + Sync + 'static,
    ) -> Self {
        Self {
            free_kb: std::sync::Arc::new(free_kb),
            available_mb: std::sync::Arc::new(available_mb),
            probe: std::sync::Arc::new(probe),
            ..self
        }
    }

    /// Measure the host before a gate whose environment is `env`
    /// ([`gate_env`]): its TMPDIR and SPAWN_GATE_MIN_FREE_GB come from
    /// there. `Ok` with the log of the checks made when the gate may run.
    pub(crate) fn check(&self, env: &[(String, String)]) -> Result<String, RoomFailure> {
        let disk_floor_gb = threshold(
            "SPAWN_GATE_MIN_FREE_GB",
            env_value(env, "SPAWN_GATE_MIN_FREE_GB"),
            DEFAULT_MIN_FREE_GB,
            "GiB",
        )?;
        let memory_floor_mb = threshold(
            "SPAWN_GATE_MIN_FREE_MEMORY_MB",
            self.min_free_memory_mb.as_deref(),
            DEFAULT_MIN_FREE_MEMORY_MB,
            "MiB",
        )?;
        let mut log = String::new();
        let short = |short: Shortfall, mut log: String| {
            let _ = writeln!(log, "{}: the gate did not run", short.line());
            Err(RoomFailure::Short { short, log })
        };
        if disk_floor_gb == 0 {
            log.push_str("disk: SPAWN_GATE_MIN_FREE_GB=0, not checked\n");
        } else {
            let tmpdir = env_value(env, "TMPDIR").unwrap_or("/tmp");
            match (self.free_kb)(Path::new(tmpdir)) {
                None => {
                    let _ = writeln!(
                        log,
                        "disk: the free space of {tmpdir} (TMPDIR) cannot be read; the gate \
                         checks it itself"
                    );
                }
                Some(free_kb) if free_kb < disk_floor_gb.saturating_mul(1_048_576) => {
                    let disk = Shortfall::Disk {
                        free_kb,
                        min_gb: disk_floor_gb,
                        tmpdir: tmpdir.to_string(),
                    };
                    return short(disk, log);
                }
                Some(free_kb) => {
                    let _ = writeln!(
                        log,
                        "disk: {free_kb} KiB free in {tmpdir} (TMPDIR), SPAWN_GATE_MIN_FREE_GB={disk_floor_gb}: ok"
                    );
                }
            }
        }
        if memory_floor_mb == 0 {
            log.push_str("memory: SPAWN_GATE_MIN_FREE_MEMORY_MB=0, not checked\n");
        } else {
            match (self.available_mb)() {
                None => log.push_str("memory: MemAvailable cannot be read; not checked\n"),
                Some(available_mb) if available_mb < memory_floor_mb => {
                    let memory = Shortfall::Memory {
                        available_mb,
                        min_mb: memory_floor_mb,
                    };
                    return short(memory, log);
                }
                Some(available_mb) => {
                    let _ = writeln!(
                        log,
                        "memory: {available_mb} MiB available, SPAWN_GATE_MIN_FREE_MEMORY_MB={memory_floor_mb}: ok"
                    );
                }
            }
        }
        let Some(url) = self.test_database_url.as_deref() else {
            log.push_str(if self.no_test_database {
                "database: this gate gets no test database (no-test-database), nothing to check\n"
            } else {
                "database: SPAWN_TEST_DATABASE_URL is unset, nothing to check\n"
            });
            return Ok(log);
        };
        let server = without_password(&server_of(url), url);
        match (self.probe)(url) {
            Ok(()) => {
                let _ = writeln!(
                    log,
                    "database: the test database ({server}) connected and ran a parallel query: ok"
                );
                Ok(log)
            }
            Err(failure) => {
                let database = Shortfall::Database {
                    server,
                    probe: without_password(&failure.probe, url),
                    error: without_password(&failure.error, url),
                };
                short(database, log)
            }
        }
    }
}

/// A threshold from its text (`None`: unset, the default): whole units, as
/// `scripts/disk.sh` reads SPAWN_GATE_MIN_FREE_GB.
fn threshold(name: &str, text: Option<&str>, default: u64, unit: &str) -> Result<u64, RoomFailure> {
    let Some(text) = text else {
        return Ok(default);
    };
    text.bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| text.parse::<u64>().ok())
        .flatten()
        .ok_or_else(|| {
            RoomFailure::Bad(format!(
                "{name} is a whole number of {unit}, not '{text}'; the gate did not run"
            ))
        })
}

/// The server a postgres URL names, without its login: `host:port/database`
/// (the host a decoded socket directory when it is one; port 5432 when
/// absent).
pub(crate) fn server_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let parameter = |name: &str| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
            .map(percent_decoded)
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit())
                && (!host.starts_with('[') || host.ends_with(']')) =>
        {
            (percent_decoded(host), port.to_string())
        }
        _ => (percent_decoded(authority), String::new()),
    };
    let host = if host.is_empty() {
        parameter("host").unwrap_or_default()
    } else {
        host
    };
    let port = if port.is_empty() {
        parameter("port").unwrap_or_else(|| "5432".to_string())
    } else {
        port
    };
    let database = if path.is_empty() {
        parameter("dbname").unwrap_or_default()
    } else {
        percent_decoded(path)
    };
    format!("{host}:{port}/{database}")
}

/// `text` with the password of `url` (as written, and decoded) masked: what
/// a probe says about the URL reaches the record without it.
pub(crate) fn without_password(text: &str, url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let authority = rest.split('/').next().unwrap_or(rest);
    let mut secrets: Vec<String> = Vec::new();
    if let Some((login, _)) = authority.rsplit_once('@')
        && let Some((_, password)) = login.split_once(':')
    {
        secrets.push(password.to_string());
    }
    for pair in query.split('&') {
        if let Some(password) = pair.strip_prefix("password=") {
            secrets.push(password.to_string());
        }
    }
    let decoded: Vec<String> = secrets
        .iter()
        .map(|secret| percent_decoded(secret))
        .collect();
    secrets.extend(decoded);
    // The longest first, so a password that contains another is masked whole.
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    let mut text = text.to_string();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        text = text.replace(secret.as_str(), "***");
    }
    text
}

/// `%XX` escapes decoded; anything else as it is.
fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = |at: usize| bytes.get(at).and_then(|byte| (*byte as char).to_digit(16));
        match (bytes[index], hex(index + 1), hex(index + 2)) {
            (b'%', Some(high), Some(low)) => {
                // Two hex digits make one byte.
                out.push(u8::try_from(high * 16 + low).unwrap_or(b'?'));
                index += 3;
            }
            (byte, _, _) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A gate's resource envelope (objective 029): its own scope unit inside
/// Spawn's slice, and the slice, manager and settings it runs under.
#[derive(Debug, Clone)]
pub(crate) struct Scope {
    /// The scope unit, `<slice stem>-gate-<task id>-<n>.scope`.
    pub(crate) unit: String,
    /// The slice it runs in and the limits it carries.
    pub(crate) envelope: spawn_core::envelope::Envelope,
}

impl Scope {
    /// The scope's cgroup directory.
    fn cgroup(&self) -> std::path::PathBuf {
        self.envelope.scope_cgroup(&self.unit)
    }
}

/// One gate verdict.
#[derive(Debug, Clone)]
pub(crate) struct Verdict {
    pub(crate) accepted: bool,
    pub(crate) reason: String,
    /// The candidate's task metric, from the verdict only.
    pub(crate) metric: Option<String>,
    /// The checks that failed once and passed again on the candidate (the
    /// verdict's `flakes`: a flake, not a regression), in the log's order;
    /// only names of checks (`check-*.sh`) are kept.
    pub(crate) flakes: Vec<String>,
    /// Everything the gate printed, stderr marked `! `, the verdict last.
    pub(crate) output: String,
}

/// Why the gate produced no verdict.
#[derive(Debug)]
pub(crate) enum GateFailure {
    /// The gate ran past its wall clock. Infrastructure, not the candidate.
    Timeout,
    /// The gate could not start.
    Failed(String),
    /// The gate ran but gave no verdict to believe: a non-zero or signalled
    /// exit, or no verdict file. `log` is what it printed.
    Aborted { reason: String, log: String },
    /// TMPDIR had less than SPAWN_GATE_MIN_FREE_GB free, so the gate did not
    /// start. A fact about the host, not the work. It names what the gate
    /// saw: free KiB, the floor in GiB, and the directory.
    NoRoom {
        free_kb: u64,
        min_gb: u64,
        tmpdir: String,
        log: String,
    },
    /// The gate found a scenario failing where the candidate is not to
    /// blame (exit [`HOST_FAULT_EXIT`] with its fact): `scenario`, where it
    /// failed (`on`: `base`, or `candidate and base`), its runs' exits as
    /// the gate says them, and the gate's reason.
    HostFault {
        scenario: String,
        on: String,
        runs: String,
        reason: String,
        log: String,
    },
    /// The base disagreed with itself: one of its scenarios exited 1 and then
    /// 0, before any candidate code ran (exit [`BASE_DISAGREES_EXIT`] and the
    /// gate's own pair of lines). A fact about the host, not the work.
    BaseDisagrees {
        /// The scenario's file name, as the gate named it.
        scenario: String,
        /// The whole seconds of the gate's `setup <n> s`, when it printed
        /// its `| gate time:` line.
        setup_s: Option<u64>,
        log: String,
    },
    /// The gate's own resource envelope killed it (objective 029): its
    /// scope's `memory.events` `oom_kill` or the unit's `Result` `oom-kill`,
    /// read at its end. The host's fault, never a verdict on the work.
    MemoryLimit {
        /// The scope unit the gate ran in.
        scope: String,
        /// Its MemoryHigh and MemoryMax, in bytes.
        memory_high: u64,
        memory_max: u64,
        /// Processes the kernel killed in it (above 0).
        oom_kills: u64,
        /// Its `memory.peak` when it could be read before the cgroup went.
        memory_peak: Option<u64>,
        /// What the gate printed.
        log: String,
    },
}

/// A base that disagreed with itself, as the gate's own pair of lines says
/// it: the scenario and, when the gate printed its `| gate time:` line, the
/// setup's seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BaseDisagrees {
    pub(crate) scenario: String,
    pub(crate) setup_s: Option<u64>,
}

/// Read the gate's own account of a base that disagreed with itself from
/// its stdout: the last two lines before the gate's own trailing ones are
/// its pair (`| <name> exited 1, then 0 on the base: …`, then `| stage gate
/// aborted <ms>ms base: <name> again (…)`), with the same `<name>` in both,
/// a scenario's file name (no space, no `|`), optionally followed by its
/// `| base runs:` line and its `| gate time:` line (the gate's own trailing
/// lines, the timing one read for the setup) and nothing else. Anywhere else
/// in the log, with another prefix (`| | …`: a nested gate's line, prefixed
/// again), with other exits, or with anything after the pair it says
/// nothing.
fn base_disagrees(stdout: &str) -> Option<BaseDisagrees> {
    let mut lines: Vec<&str> = stdout.lines().collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let mut setup_s = None;
    while let Some(line) = lines.last() {
        if line.starts_with(BASE_RUNS_LINE) {
            lines.pop();
        } else if let Some(times) = line.strip_prefix(GATE_TIME_LINE) {
            setup_s = setup_seconds(times);
            lines.pop();
        } else {
            break;
        }
    }
    let stage = lines.pop()?;
    let exited = lines.pop()?;
    let name = exited
        .strip_prefix("| ")?
        .strip_suffix(BASE_EXITED_SAID)?
        .strip_suffix(BASE_EXITED_TAIL)?;
    let scenario = name
        .chars()
        .all(|char| !char.is_whitespace() && char != '|')
        .then_some(name)?;
    if scenario.is_empty() {
        return None;
    }
    let (millis, rest) = stage.strip_prefix(BASE_STAGE_HEAD)?.split_once("ms ")?;
    if millis.is_empty() || !millis.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let staged = rest.strip_prefix("base: ")?.strip_suffix(BASE_STAGE_SAID)?;
    (staged == scenario).then(|| BaseDisagrees {
        scenario: scenario.to_string(),
        setup_s,
    })
}

/// The whole seconds of `setup <n> s` in the gate's `| gate time:` line.
fn setup_seconds(times: &str) -> Option<u64> {
    let (_, after) = times.split_once("setup ")?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The gate's environment, read through `lookup`: the [`GATE_ENV`]
/// variables that are set. A SPAWN_TEST_DATABASE_URL whose text names the
/// owner's cluster (same host and port, or same socket directory) is
/// withheld here, whatever database it names: the test login is a
/// superuser, and a superuser on the owner's cluster reaches the owner's
/// database. Text is only the cheap first pass; [`withhold_owners_cluster`]
/// decides by the servers' identity. (Not an error: the e2e runs an
/// integrator whose own database is the test database.)
pub(crate) fn gate_env(lookup: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let owner = lookup("SPAWN_DATABASE_URL");
    GATE_ENV
        .iter()
        .filter_map(|name| {
            lookup(name)
                .filter(|value| !value.is_empty())
                .map(|value| ((*name).to_string(), value))
        })
        .filter(|(name, value)| {
            name != "SPAWN_TEST_DATABASE_URL"
                || owner
                    .as_deref()
                    .is_none_or(|owner| cluster_of(owner) != cluster_of(value))
        })
        .collect()
}

/// Where the integrator writes its gate environment for the deploy
/// (`deploy/deploy.sh`, warm_base_cache): set in spawn-integrate's unit to
/// `%h/.config/spawn/gate-env`. Unset, nothing is written.
pub(crate) const GATE_ENV_FILE: &str = "SPAWN_GATE_ENV_FILE";

/// Writes `env` (the integrator's [`gate_env`]) to `path` for the deploy's
/// base-only gate, which must compute the same cache key as the
/// integrator's gates. The deploy read it from `/proc/<pid>/environ`
/// before the integrator was made not dumpable; this file is what it reads
/// now, and it holds the gates' test URL, so: mode 0600 from its creation,
/// under `~/.config/spawn` (denied to engines), written whole (a file
/// beside it, renamed over it).
///
/// The format is `/proc/<pid>/environ`'s, `NAME=value` records each ended
/// by a NUL, with a first record `SPAWN_INTEGRATE_PID=<pid>` so the deploy
/// knows the file is the running integrator's and not a previous one's.
pub(crate) fn write_gate_env(
    path: &Path,
    pid: u32,
    env: &[(String, String)],
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut records = format!("SPAWN_INTEGRATE_PID={pid}\0").into_bytes();
    for (name, value) in env {
        records.extend_from_slice(format!("{name}={value}\0").as_bytes());
    }
    let staged = path.with_file_name(format!(
        ".{}.{pid}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("gate-env")
    ));
    let _ = std::fs::remove_file(&staged);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staged)?;
    let written = file.write_all(&records).and_then(|()| file.sync_all());
    drop(file);
    match written.and_then(|()| std::fs::rename(&staged, path)) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&staged);
            Err(error)
        }
    }
}

/// The decisive pass over [`gate_env`]'s result: SPAWN_TEST_DATABASE_URL
/// stays only when the owner's server and the test server both answer with
/// their identity (`identity`, `spawn_postgres::cluster_identity` in the
/// integrator) and the two differ. One server spelled two ways (its socket,
/// a hostname, 127.0.0.2, a LAN address) has one identity. Fails closed: no
/// owner URL, or a connection or query that fails, withholds the test URL
/// too. Returns the environment and, when the URL was withheld, why.
pub(crate) fn withhold_owners_cluster(
    env: &[(String, String)],
    owner: Option<&str>,
    identity: impl Fn(&str) -> Result<String, String>,
) -> (Vec<(String, String)>, Option<String>) {
    let Some((_, test)) = env
        .iter()
        .find(|(name, _)| name == "SPAWN_TEST_DATABASE_URL")
    else {
        return (env.to_vec(), None);
    };
    let reason = match owner {
        None => Some(
            "SPAWN_DATABASE_URL is unset, so the test server cannot be told apart from the owner's"
                .to_string(),
        ),
        Some(owner) => match (identity(owner), identity(test)) {
            (Ok(owner), Ok(test)) if owner != test => None,
            (Ok(_), Ok(_)) => {
                Some("SPAWN_TEST_DATABASE_URL reaches the owner's server".to_string())
            }
            (Err(error), _) => Some(format!(
                "the owner's server has no readable identity: {error}"
            )),
            (_, Err(error)) => Some(format!("the test server has no readable identity: {error}")),
        },
    };
    match reason {
        None => (env.to_vec(), None),
        Some(reason) => (
            env.iter()
                .filter(|(name, _)| name != "SPAWN_TEST_DATABASE_URL")
                .cloned()
                .collect(),
            Some(format!(
                "{reason}; the gate does not get SPAWN_TEST_DATABASE_URL"
            )),
        ),
    }
}

/// The cluster a postgres URL names: its host, with a percent-encoded
/// socket directory decoded and `localhost`/`::1` read as 127.0.0.1, and
/// its port (5432 when absent).
fn cluster_of(url: &str) -> (String, String) {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit())
                && (!host.starts_with('[') || host.ends_with(']')) =>
        {
            (host, port)
        }
        _ => (authority, "5432"),
    };
    let host = host.replace("%2F", "/").replace("%2f", "/");
    let host = match host.trim_start_matches('[').trim_end_matches(']') {
        "localhost" | "::1" | "127.0.0.1" => "127.0.0.1".to_string(),
        _ => host.trim_end_matches('/').to_string(),
    };
    (host, port.to_string())
}

/// Judge `candidate` against `base` in `repo` with the scenarios in
/// `scenarios`, and the task scenario when there is one. `env` is the
/// gate's whole environment ([`gate_env`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn judge(
    gate: &Path,
    repo: &Path,
    base: &str,
    candidate: &str,
    scenarios: &Path,
    scenario: Option<&Path>,
    env: &[(String, String)],
    limit: Duration,
    scope: Option<&Scope>,
) -> Result<Verdict, GateFailure> {
    judge_in(
        &std::env::temp_dir(),
        &measured_free_kb,
        gate,
        repo,
        base,
        candidate,
        scenarios,
        scenario,
        env,
        limit,
        scope,
    )
}

/// [`judge`], with the private directory made under `parent` and free space
/// measured by `free_kb` (a seam for tests: a disk with no room at all).
#[allow(clippy::too_many_arguments)]
fn judge_in(
    parent: &Path,
    free_kb: &dyn Fn(&Path) -> Option<u64>,
    gate: &Path,
    repo: &Path,
    base: &str,
    candidate: &str,
    scenarios: &Path,
    scenario: Option<&Path>,
    env: &[(String, String)],
    limit: Duration,
    scope: Option<&Scope>,
) -> Result<Verdict, GateFailure> {
    let floor = env_value(env, "SPAWN_GATE_MIN_FREE_GB")
        .map_or(Some(DEFAULT_MIN_FREE_GB), |text| text.parse::<u64>().ok());
    // The gate's own TMPDIR, as gate.sh reads it.
    let tmpdir = PathBuf::from(env_value(env, "TMPDIR").unwrap_or("/tmp"));
    let short = |path: &Path, log: &str, why: &str| -> Option<GateFailure> {
        let min_gb = floor?;
        let free = free_kb(path)?;
        (free < min_gb.saturating_mul(1_048_576)).then(|| GateFailure::NoRoom {
            free_kb: free,
            min_gb,
            tmpdir: path.display().to_string(),
            log: format!(
                "{log}! integrate: {why}; measured {free} KiB free in {}, under \
                 SPAWN_GATE_MIN_FREE_GB={min_gb}\n",
                path.display()
            ),
        })
    };
    let directory = match PrivateDirectory::create_in(parent) {
        Ok(directory) => directory,
        Err(error) => {
            return Err(short(
                parent,
                "",
                &format!("cannot create the gate's private directory: {error}"),
            )
            .unwrap_or_else(|| failed(&error)));
        }
    };
    let verdict_path = directory.path().join("verdict");
    let mut words = vec![
        gate.display().to_string(),
        repo.display().to_string(),
        base.to_string(),
        candidate.to_string(),
        scenarios.display().to_string(),
    ];
    if let Some(scenario) = scenario {
        words.push(scenario.display().to_string());
    }
    // The gate runs in its own scope, inside Spawn's slice, when the host
    // has an envelope (objective 029): everything it starts is in it.
    let (program, arguments) = match scope {
        Some(scope) => scope.envelope.wrap(
            &scope.unit,
            scope.envelope.settings.gate,
            "sh",
            &words,
        ),
        None => ("sh".to_string(), words),
    };
    let mut command = Command::new(program);
    command.args(&arguments);
    command
        .env_clear()
        .envs(env.iter().map(|(name, value)| (name, value)))
        .env("SPAWN_GATE_VERDICT", &verdict_path);
    if let Some(scope) = scope {
        // The manager's bus, for systemd-run only: candidate code runs in a
        // sandbox whose /run is an empty tmpfs and reaches no manager.
        command.env("XDG_RUNTIME_DIR", &scope.envelope.manager.runtime_dir);
    }
    let cgroup = scope.map(Scope::cgroup);
    let run = match run_bounded_in(command, limit, directory.path(), cgroup.as_deref()) {
        Ok(run) => run,
        Err(GateFailure::Failed(error)) => {
            return Err(short(
                parent,
                "",
                &format!("cannot run the gate in its private directory: {error}"),
            )
            .unwrap_or(GateFailure::Failed(error)));
        }
        Err(other) => return Err(other),
    };
    // The scope's own facts, read before its unit is reset: a unit left
    // behind is one per gate, and a collected one no longer says why it
    // ended.
    let facts = scope.map(|scope| scope.envelope.scope_facts(&scope.unit));
    if let Some(scope) = scope {
        scope.envelope.reset_failed(&scope.unit);
    }
    let verdict = std::fs::read_to_string(&verdict_path).ok();
    if let (Some(scope), Some(facts)) = (scope, &facts)
        && let Some(failure) = memory_limit(scope, facts, run.memory_peak, &run)
    {
        return Err(failure);
    }
    match decide(&run, verdict.as_deref()) {
        // Exit 5 with no readable fact: the gate said "no room" but could
        // not write it down. Measure; believe it only under the floor.
        Err(GateFailure::Aborted { reason, log }) if run.status.code() == Some(NO_ROOM_EXIT) => {
            Err(short(
                &tmpdir,
                &log,
                "the gate exited 5 with no readable no-room fact",
            )
            .unwrap_or(GateFailure::Aborted { reason, log }))
        }
        other => other,
    }
}

/// The value of `name` in the gate's environment, when set.
fn env_value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// The failure a gate's envelope makes of its end (objective 029), when it
/// killed it: the scope's own `oom_kill` counter or its unit's `Result`
/// `oom-kill`, never the exit status (137 is any SIGKILL). `None` when the
/// envelope did not kill it.
fn memory_limit(
    scope: &Scope,
    facts: &spawn_core::envelope::ScopeFacts,
    peak: Option<u64>,
    run: &Run,
) -> Option<GateFailure> {
    if !facts.oom_killed() {
        return None;
    }
    let mut output = run.stdout.clone();
    for line in run.stderr.lines() {
        let _ = writeln!(output, "! {line}");
    }
    Some(GateFailure::MemoryLimit {
        scope: scope.unit.clone(),
        memory_high: scope.envelope.settings.gate.high,
        memory_max: scope.envelope.settings.gate.max,
        oom_kills: facts
            .oom_kills
            .max(u64::from(facts.result.as_deref() == Some("oom-kill"))),
        memory_peak: peak.or(facts.memory_peak),
        log: output,
    })
}

/// The verdict of a finished gate run: the verdict file's, only after a
/// clean exit.
///
/// Two failures are facts about the host rather than aborts, each an exit
/// status together with a fact the gate states: exit [`NO_ROOM_EXIT`] with
/// its no-room fact in the verdict file ([`no_room`]), and exit
/// [`BASE_DISAGREES_EXIT`] with its own pair of lines at the end of stdout
/// ([`base_disagrees`]: a scenario that exited 1 and then 0 on the base,
/// before any candidate code ran). Every other non-zero or signalled exit is
/// an abort with the whole record, the gate's stdout then its stderr marked
/// `! `: the pair with any other exit, a base run that exited 2 or `1, then
/// 2`, and a nested gate's line (`| | …`) in a scenario's log.
fn decide(run: &Run, verdict: Option<&str>) -> Result<Verdict, GateFailure> {
    // The record: stdout, then stderr marked `! ` so no line of it can pass
    // for a verdict or a section header. An abort keeps all of it.
    let record = |stdout: &str| {
        let mut output = stdout.to_string();
        for text in run.stderr.lines() {
            let _ = writeln!(output, "! {text}");
        }
        output
    };
    let aborted = |reason: String| GateFailure::Aborted {
        reason,
        log: record(&run.stdout),
    };
    if run.status.code() == Some(NO_ROOM_EXIT)
        && let Some((free_kb, min_gb, tmpdir)) = verdict.and_then(no_room)
    {
        return Err(GateFailure::NoRoom {
            free_kb,
            min_gb,
            tmpdir,
            log: record(&run.stdout),
        });
    }
    if run.status.code() == Some(BASE_DISAGREES_EXIT)
        && let Some(fact) = base_disagrees(&run.stdout)
    {
        return Err(GateFailure::BaseDisagrees {
            scenario: fact.scenario,
            setup_s: fact.setup_s,
            log: record(&run.stdout),
        });
    }
    if run.status.code() == Some(HOST_FAULT_EXIT)
        && let Some(GateFailure::HostFault {
            scenario,
            on,
            runs,
            reason,
            ..
        }) = verdict.and_then(host_fault)
    {
        return Err(GateFailure::HostFault {
            scenario,
            on,
            runs,
            reason,
            log: record(&run.stdout),
        });
    }
    if !run.status.success() {
        return Err(aborted(format!("the gate ended with {}", run.status)));
    }
    let Some(line) = verdict
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.contains('\n'))
    else {
        return Err(aborted("the gate wrote no verdict".to_string()));
    };
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|error| aborted(format!("gate verdict: {error}")))?;
    let text = |name: &str| value.get(name).and_then(|v| v.as_str()).map(str::to_string);
    let accepted = match text("verdict").as_deref() {
        Some("accepted") => true,
        Some("rejected") => false,
        _ => return Err(aborted(format!("gate verdict: {line}"))),
    };
    // A believed verdict is recorded last, and once: stdout's copy goes.
    let stdout = run
        .stdout
        .trim_end()
        .strip_suffix(line)
        .filter(|head| head.is_empty() || head.ends_with('\n'))
        .unwrap_or(&run.stdout);
    let mut output = record(stdout);
    let _ = writeln!(output, "{line}");
    Ok(Verdict {
        accepted,
        reason: text("reason").unwrap_or_else(|| "gate rejected the candidate".to_string()),
        metric: text("metric"),
        flakes: flakes(&value),
        output,
    })
}

/// The verdict's `flakes`: the names of checks (`check-<name>.sh`, no path)
/// that failed once and passed again; anything else in the list is dropped.
fn flakes(value: &serde_json::Value) -> Vec<String> {
    value
        .get("flakes")
        .and_then(serde_json::Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(serde_json::Value::as_str)
                .filter(|name| {
                    name.strip_prefix("check-")
                        .and_then(|rest| rest.strip_suffix(".sh"))
                        .is_some_and(|stem| !stem.is_empty())
                        && name.len() <= 100
                        && !name.contains('/')
                })
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The no-room fact of a verdict file, `(free_kb, min_gb, tmpdir)`: one
/// JSON line with `no_room` true, whole numbers and a directory.
fn no_room(file: &str) -> Option<(u64, u64, String)> {
    let line = file.trim();
    if line.contains('\n') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("no_room")?.as_bool() != Some(true) || value.get("verdict").is_some() {
        return None;
    }
    let free_kb = value.get("free_kb")?.as_u64()?;
    let min_gb = value.get("min_gb")?.as_u64()?;
    let tmpdir = value.get("tmpdir")?.as_str()?;
    (!tmpdir.is_empty()).then(|| (free_kb, min_gb, tmpdir.to_string()))
}

/// The host-fault fact of a verdict file: one JSON line with `host_fault`
/// true, a scenario, where it failed (`base` or `candidate and base`), its
/// runs and a reason, and no verdict. Its `log` is left empty.
fn host_fault(file: &str) -> Option<GateFailure> {
    let line = file.trim();
    if line.contains('\n') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("host_fault")?.as_bool() != Some(true) || value.get("verdict").is_some() {
        return None;
    }
    let text = |name: &str| value.get(name)?.as_str().map(str::to_string);
    let scenario = text("scenario").filter(|scenario| !scenario.is_empty())?;
    let on = text("on").filter(|on| on == "base" || on == "candidate and base")?;
    Some(GateFailure::HostFault {
        scenario,
        on,
        runs: text("runs").unwrap_or_default(),
        reason: text("reason").unwrap_or_default(),
        log: String::new(),
    })
}

fn failed(error: &std::io::Error) -> GateFailure {
    GateFailure::Failed(error.to_string())
}

/// A new directory, mode 0700, under the temporary directory; removed on
/// drop. The gate's stdout, stderr and verdict file live in it.
struct PrivateDirectory(PathBuf);

impl PrivateDirectory {
    #[cfg(test)]
    fn create() -> std::io::Result<Self> {
        Self::create_in(&std::env::temp_dir())
    }

    fn create_in(parent: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt as _;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path = parent.join(format!("spawn-gate-{}-{nonce:x}", std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A finished command.
#[derive(Debug)]
pub(crate) struct Run {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    /// The highest `memory.peak` the cgroup showed while it ran (objective
    /// 029): the scope is gone once the gate ends, so this is what its
    /// record can say.
    pub(crate) memory_peak: Option<u64>,
}

/// Run a command with a wall clock and no stdin, its stdout and stderr in
/// files in `directory`.
///
/// The child writes to files, not to pipes: a pipe that fills up would block
/// the child while this loop waits for it to exit. The child leads its own
/// process group, and the whole group is killed when the gate ends or times
/// out, so nothing the candidate started outlives the verdict (the gate's
/// sandbox also dies with its parent).
#[cfg(test)]
pub(crate) fn run_bounded(
    command: Command,
    limit: Duration,
    directory: &Path,
) -> Result<Run, GateFailure> {
    run_bounded_in(command, limit, directory, None)
}

/// [`run_bounded`] with a cgroup to sample while the command runs (the
/// gate's scope, whose `memory.peak` goes with the cgroup).
pub(crate) fn run_bounded_in(
    mut command: Command,
    limit: Duration,
    directory: &Path,
    cgroup: Option<&Path>,
) -> Result<Run, GateFailure> {
    use std::os::unix::process::CommandExt as _;

    let out_path = directory.join("stdout");
    let err_path = directory.join("stderr");
    let stdout = std::fs::File::create(&out_path).map_err(|error| failed(&error))?;
    let stderr = std::fs::File::create(&err_path).map_err(|error| failed(&error))?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map_err(|error| failed(&error))?;
    let group = child.id();
    let kill_group = || {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{group}")])
            .stderr(Stdio::null())
            .status();
    };
    let deadline = Instant::now() + limit;
    let mut memory_peak: Option<u64> = None;
    let status = loop {
        if let Some(cgroup) = cgroup
            && let Ok(text) = std::fs::read_to_string(cgroup.join("memory.peak"))
            && let Ok(peak) = text.trim().parse::<u64>()
        {
            memory_peak = Some(memory_peak.map_or(peak, |seen: u64| seen.max(peak)));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                kill_group();
                let _ = child.kill();
                let _ = child.wait();
                return Err(GateFailure::Timeout);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(error) => {
                kill_group();
                return Err(failed(&error));
            }
        }
    };
    kill_group();
    Ok(Run {
        status,
        stdout: std::fs::read_to_string(&out_path).unwrap_or_default(),
        stderr: std::fs::read_to_string(&err_path).unwrap_or_default(),
        memory_peak,
    })
}

/// Scenario files in the controller's checkout.
pub(crate) fn count_scenarios(directory: &Path) -> Result<u32, String> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(format!("read {}: {error}", directory.display())),
    };
    let mut count = 0;
    for entry in entries {
        let path = entry
            .map_err(|error| format!("read {}: {error}", directory.display()))?
            .path();
        if path.extension().and_then(|text| text.to_str()) == Some("sh") {
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::{
<<<<<<< f0da0711f669349bf3aaf42d7cf54d31a7d76a82
        BaseDisagrees, Cpu, GateFailure, PrivateDirectory, gate_env, judge, judge_in, on_shared_server, run_bounded,
        withhold_owners_cluster, write_gate_env,
=======
        BaseDisagrees, GateFailure, PrivateDirectory, Run, Scope,
        gate_env, judge, judge_in, memory_limit, run_bounded, withhold_owners_cluster,
>>>>>>> 1810cddf170ceaec780257528360e58ffa3e304b
    };
    use spawn_core::envelope::{Envelope, Limits, Manager, ScopeFacts, Settings};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

<<<<<<< f0da0711f669349bf3aaf42d7cf54d31a7d76a82
    fn cpu(pairs: &[(&str, &str)], cores: usize) -> Result<Cpu, String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        Cpu::from_env(
            |name| {
                pairs
                    .iter()
                    .find(|(known, _)| known == name)
                    .map(|(_, value)| value.clone())
            },
            cores,
        )
    }

    /// SPAWN_GATE_JOBS and SPAWN_GATE_WORKER_CORES parse as gate.sh reads a
    /// whole number (digits, no leading zero); unset or empty is the
    /// default; anything else refuses to start, naming the variable.
    #[test]
    fn the_cpu_settings_parse_like_the_others() {
        let plain = cpu(&[], 9).expect("defaults");
        assert_eq!(
            plain,
            Cpu {
                cores: 9,
                reserved: 2,
                jobs: None
            }
        );
        assert_eq!(cpu(&[("SPAWN_GATE_JOBS", "")], 9), Ok(plain.clone()));
        assert_eq!(
            cpu(&[("SPAWN_GATE_JOBS", " 2 ")], 9).map(|c| c.jobs),
            Ok(Some(2))
        );
        for bad in ["0", "-1", "two", "01", "2.5", "+3"] {
            let error = cpu(&[("SPAWN_GATE_JOBS", bad)], 9).expect_err(bad);
            assert!(
                error.contains("SPAWN_GATE_JOBS is a whole number from 1"),
                "{error}"
            );
        }
        assert_eq!(
            cpu(&[("SPAWN_GATE_WORKER_CORES", "0")], 9).map(|c| c.reserved),
            Ok(0)
        );
        assert_eq!(
            cpu(&[("SPAWN_GATE_WORKER_CORES", "8")], 9).map(|c| c.reserved),
            Ok(8)
        );
        for bad in ["9", "12", "x", "-1", "03"] {
            let error = cpu(&[("SPAWN_GATE_WORKER_CORES", bad)], 9).expect_err(bad);
            assert!(
                error.contains("SPAWN_GATE_WORKER_CORES is a whole number from 0 to 8"),
                "{error}"
            );
        }
        // Small hosts: a quarter of the cores, at least 1, never all of them.
        assert_eq!(cpu(&[], 2).map(|c| c.reserved), Ok(1));
        assert_eq!(cpu(&[], 1).map(|c| c.reserved), Ok(0));
        assert_eq!(cpu(&[], 16).map(|c| c.reserved), Ok(4));
    }

    /// The room decision at its boundary, on the 9-core host of 2026-11-08
    /// (2 cores kept for the workers).
    #[test]
    fn a_gate_starts_only_within_the_cores() {
        let host = cpu(&[], 9).expect("defaults");
        // The default divides the 7 cores among the gates that can run at once.
        assert_eq!(host.jobs_for(false, 1), 7);
        assert_eq!(host.jobs_for(false, 2), 3);
        assert_eq!(host.jobs_for(false, 3), 2);
        assert_eq!(host.jobs_for(false, 9), 1, "at least one");
        assert_eq!(
            cpu(&[], 64).expect("big").jobs_for(false, 1),
            8,
            "at most 8"
        );
        assert_eq!(host.jobs_for(true, 1), 1, "the shared test server");
        // Exactly the cores: starts; one more: waits.
        assert!(host.fits(3, 4), "3 + 4 + 2 = 9");
        assert!(!host.fits(3, 5), "3 + 5 + 2 = 10");
        assert!(!host.fits(4, 4), "SPAWN_GATE_JOBS=4: a second gate waits");
        // The defaults for two gates fit together; a third does not.
        let two = host.jobs_for(false, 2);
        assert!(host.fits(two, two));
        assert!(!host.fits(two + two, two));
        // No gate running: one always starts, however many jobs.
        assert!(host.fits(0, 20));
        // The owner's SPAWN_GATE_JOBS is every gate's.
        let owner = cpu(&[("SPAWN_GATE_JOBS", "2")], 9).expect("set");
        assert_eq!(owner.jobs_for(false, 3), 2);
        assert_eq!(owner.jobs_for(true, 1), 2);
        assert!(owner.fits(4, 2) && !owner.fits(6, 2));
    }

    #[test]
    fn a_gate_is_on_the_shared_server_without_one_of_its_own() {
        let env = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                .collect()
        };
        assert!(on_shared_server(&env(&[(
            "SPAWN_TEST_DATABASE_URL",
            "postgres://x"
        )])));
        assert!(!on_shared_server(&env(&[
            ("SPAWN_TEST_DATABASE_URL", "postgres://x"),
            ("SPAWN_GATE_PG_BIN", "/usr/lib/postgresql/16/bin")
        ])));
        assert!(!on_shared_server(&env(&[
            ("SPAWN_TEST_DATABASE_URL", "postgres://x"),
            ("SPAWN_GATE_PG_IMAGE", "postgres:16-alpine")
        ])));
        assert!(!on_shared_server(&env(&[])));
    }

    /// The file the integrator writes is read by the deploy (through
    /// deploy/gate-env.sh, which deploy.sh sources) as exactly the
    /// environment the integrator's gates get: the deploy's base-only gate
    /// computes the same cache key from it.
    #[test]
    fn the_deploy_reads_the_integrators_gate_environment_from_its_file() {
        use std::os::unix::fs::PermissionsExt;
        let host = [
            ("PATH", "/home/o/.cargo/bin:/usr/bin:/bin"),
            ("HOME", "/home/o"),
            ("LANG", "C.UTF-8"),
            ("TMPDIR", "/var/tmp/spawn"),
            ("CARGO_HOME", "/home/o/.cargo"),
            ("RUSTUP_HOME", "/home/o/.rustup"),
            ("SPAWN_GATE_MIN_FREE_GB", "10"),
            ("SPAWN_GATE_JOBS", ""),
            ("SPAWN_GATE_PG_BIN", "/usr/lib/postgresql/16/bin"),
            (
                "SPAWN_TEST_DATABASE_URL",
                "postgres://gate:p%40ss@%2Fvar%2Frun%2Fpostgresql:5433/spawn_test",
            ),
            (
                "SPAWN_DATABASE_URL",
                "postgres://spawn:pw@127.0.0.1:15433/spawn",
            ),
            ("SPAWN_SERVE_TOKEN", "never-in-the-file"),
        ];
        let env = gate_env(|name| {
            host.iter()
                .find(|(known, _)| *known == name)
                .map(|(_, value)| (*value).to_string())
        });
        let directory = PrivateDirectory::create().expect("directory");
        let file = directory.path().join("gate-env");
        write_gate_env(&file, 4242, &env).expect("written");
        let mode = std::fs::metadata(&file)
            .expect("the file")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let bytes = std::fs::read(&file).expect("read");
        assert!(bytes.starts_with(b"SPAWN_INTEGRATE_PID=4242\0"));
        let text = String::from_utf8(bytes).expect("utf-8");
        assert!(!text.contains("never-in-the-file") && !text.contains("15433"));
        // Written whole: nothing staged is left beside it.
        let names: Vec<_> = std::fs::read_dir(directory.path())
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["gate-env"]);

        let helper = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/gate-env.sh");
        let output = Command::new("bash")
            .arg("-c")
            .arg(
                r#"set -eu; . "$1"; keep=(); own=""; test_url=""; home=""
                gate_env_read "$2"
                printf '%s\0' "${keep[@]}" "own=$own" "test_url=$test_url" "home=$home""#,
            )
            .arg("bash")
            .arg(&helper)
            .arg(&file)
            .output()
            .expect("bash runs");
        assert!(output.status.success(), "{output:?}");
        let read: Vec<String> = String::from_utf8(output.stdout)
            .expect("utf-8")
            .split_terminator('\0')
            .map(str::to_string)
            .collect();
        let mut expected: Vec<String> = env
            .iter()
            .filter(|(name, _)| name != "SPAWN_TEST_DATABASE_URL")
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        expected.push("own=/usr/lib/postgresql/16/bin".to_string());
        expected.push(format!(
            "test_url={}",
            "postgres://gate:p%40ss@%2Fvar%2Frun%2Fpostgresql:5433/spawn_test"
        ));
        expected.push("home=/home/o".to_string());
        assert_eq!(read, expected);
        // An empty value is not the gate's (gate_env drops it), nor the
        // deploy's any more.
        assert!(
            !read
                .iter()
                .any(|assignment| assignment == "SPAWN_GATE_JOBS=")
        );

        // Written again by the next integrator: replaced whole.
        write_gate_env(&file, 4343, &env[..1]).expect("rewritten");
        let bytes = std::fs::read(&file).expect("read");
        assert_eq!(
            bytes,
            format!("SPAWN_INTEGRATE_PID=4343\0{}={}\0", env[0].0, env[0].1).into_bytes()
        );
=======
    /// A gate's envelope: 30 MiB high, 32 MiB max, 8 MiB swap.
    fn scope() -> Scope {
        Scope {
            unit: "spawn-gate-9-1.scope".to_string(),
            envelope: Envelope {
                settings: Settings {
                    mode: spawn_core::envelope::Mode::Systemd,
                    slice: "spawn.slice".to_string(),
                    slice_high: 16 << 30,
                    slice_max: 20 << 30,
                    cpu_weight: 50,
                    execution: Limits {
                        high: 10 << 30,
                        max: 12 << 30,
                        swap_max: 4 << 30,
                    },
                    gate: Limits {
                        high: 30 << 20,
                        max: 32 << 20,
                        swap_max: 8 << 20,
                    },
                },
                manager: Manager {
                    control_group: "/user.slice/user-1000.slice/user@1000.service".to_string(),
                    runtime_dir: "/run/user/1000".to_string(),
                },
            },
        }
    }

    #[test]
    fn only_the_scopes_own_kill_is_a_memory_limit_not_an_exit_status() {
        use std::os::unix::process::ExitStatusExt as _;
        // A gate killed by a signal, exit status 137: any SIGKILL looks like
        // that, so the status alone decides nothing.
        let killed = Run {
            status: std::process::ExitStatus::from_raw(137 << 8),
            stdout: "gate: out".to_string(),
            stderr: "! err".to_string(),
            memory_peak: Some(33 << 20),
        };
        let quiet = ScopeFacts {
            scope: "spawn-gate-9-1.scope".to_string(),
            result: Some("success".to_string()),
            load_state: Some("not-found".to_string()),
            ..ScopeFacts::default()
        };
        assert!(memory_limit(&scope(), &quiet, killed.memory_peak, &killed).is_none());
        // The scope's own facts say the memory limit killed it.
        let killed_by_envelope = ScopeFacts {
            scope: "spawn-gate-9-1.scope".to_string(),
            result: Some("oom-kill".to_string()),
            load_state: Some("failed".to_string()),
            memory_peak: Some(33 << 20),
            ..ScopeFacts::default()
        };
        let failure = memory_limit(&scope(), &killed_by_envelope, killed.memory_peak, &killed)
            .expect("the envelope killed it");
        match failure {
            GateFailure::MemoryLimit {
                scope,
                memory_high,
                memory_max,
                oom_kills,
                memory_peak,
                log,
            } => {
                assert_eq!(scope, "spawn-gate-9-1.scope");
                assert_eq!(memory_high, 30 << 20);
                assert_eq!(memory_max, 32 << 20);
                assert!(oom_kills > 0);
                assert_eq!(memory_peak, Some(33 << 20));
                assert!(log.contains("gate: out") && log.contains("! err"));
            }
            other => panic!("not a memory limit: {other:?}"),
        }
        // The cgroup's own counter counts too, without the unit's Result.
        let counted = ScopeFacts {
            scope: "spawn-gate-9-1.scope".to_string(),
            oom_kills: 2,
            ..ScopeFacts::default()
        };
        assert!(memory_limit(&scope(), &counted, None, &killed).is_some());
>>>>>>> 1810cddf170ceaec780257528360e58ffa3e304b
    }

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    fn bounded(script: &str, limit: Duration) -> Result<super::Run, GateFailure> {
        let directory = PrivateDirectory::create().expect("directory");
        run_bounded(shell(script), limit, directory.path())
    }

    #[test]
    fn a_command_past_its_deadline_is_killed() {
        let started = Instant::now();
        let killed = bounded("exec sleep 30", Duration::from_millis(300));
        assert!(matches!(killed, Err(GateFailure::Timeout)));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_command_inside_its_deadline_returns_its_output() {
        let run = bounded(
            "echo accepted; echo forged >&2; exit 3",
            Duration::from_secs(10),
        )
        .expect("output");
        assert!(run.stdout.contains("accepted"));
        assert!(!run.stdout.contains("forged"), "stderr stays out of stdout");
        assert!(run.stderr.contains("forged"));
        assert_eq!(run.status.code(), Some(3));
    }

    #[test]
    fn a_timeout_kills_the_whole_group() {
        let marker = std::env::temp_dir().join(format!("spawn-group-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let script = format!("(sleep 2; touch {}) & sleep 30", marker.display());
        let killed = bounded(&script, Duration::from_millis(300));
        assert!(matches!(killed, Err(GateFailure::Timeout)));
        std::thread::sleep(Duration::from_secs(3));
        assert!(!marker.exists(), "the background child died with the gate");
    }

    /// A stand-in gate: `body` runs as the gate script, with the usual
    /// arguments.
    struct FakeGate {
        directory: PrivateDirectory,
    }

    impl FakeGate {
        fn new(body: &str) -> Self {
            let directory = PrivateDirectory::create().expect("directory");
            std::fs::write(directory.path().join("gate.sh"), body).expect("gate");
            Self { directory }
        }

        fn judge(&self, env: &[(String, String)]) -> Result<super::Verdict, GateFailure> {
            let path = |name: &str| -> PathBuf { self.directory.path().join(name) };
            judge(
                &path("gate.sh"),
                Path::new("/nonexistent/repo.git"),
                "base",
                "candidate",
                &path("scenarios"),
                None,
                env,
                Duration::from_secs(20),
                None,
            )
        }
    }

    impl FakeGate {
        /// [`FakeGate::judge`] with the private directory under `parent` and
        /// free space measured as `free_kb` says, wherever.
        fn judge_room(
            &self,
            env: &[(String, String)],
            parent: &Path,
            free_kb: Option<u64>,
        ) -> Result<super::Verdict, GateFailure> {
            let path = |name: &str| -> PathBuf { self.directory.path().join(name) };
            judge_in(
                parent,
                &|_| free_kb,
                &path("gate.sh"),
                Path::new("/nonexistent/repo.git"),
                "base",
                "candidate",
                &path("scenarios"),
                None,
                env,
                Duration::from_secs(20),
                None,
            )
        }
    }

    fn path_only() -> Vec<(String, String)> {
        vec![("PATH".to_string(), "/usr/bin:/bin".to_string())]
    }

    /// A verdict names the checks that failed once and passed again
    /// (`flakes`); only check names are kept, and a verdict without the key
    /// (every gate before it) has none.
    #[test]
    fn a_verdict_names_its_flakes_and_keeps_only_check_names() {
        let gate = FakeGate::new(
            "echo '{\"verdict\":\"accepted\",\"flakes\":[\"check-e2e.sh\",\"../x.sh\",\"007.sh\",3,\"check-tests.sh\"]}' >\"$SPAWN_GATE_VERDICT\"\n",
        );
        let verdict = gate.judge(&path_only()).expect("verdict");
        assert_eq!(verdict.flakes, ["check-e2e.sh", "check-tests.sh"]);
        let gate = FakeGate::new(
            "echo '{\"verdict\":\"rejected\",\"reason\":\"r\",\"flakes\":[\"check-clippy.sh\"]}' >\"$SPAWN_GATE_VERDICT\"\n",
        );
        let verdict = gate.judge(&path_only()).expect("verdict");
        assert!(!verdict.accepted);
        assert_eq!(verdict.flakes, ["check-clippy.sh"]);
        let gate = FakeGate::new("echo '{\"verdict\":\"accepted\"}' >\"$SPAWN_GATE_VERDICT\"\n");
        assert!(gate.judge(&path_only()).expect("verdict").flakes.is_empty());
    }

    #[test]
    fn the_verdict_file_after_a_clean_exit_is_the_verdict() {
        let gate = FakeGate::new(
            "echo '| == candidate: build'\n\
             echo '{\"verdict\":\"accepted\",\"metric\":\"METRIC x candidate=1\"}' >\"$SPAWN_GATE_VERDICT\"\n\
             echo '{\"verdict\":\"accepted\",\"metric\":\"METRIC x candidate=1\"}'\n",
        );
        let verdict = gate.judge(&path_only()).expect("verdict");
        assert!(verdict.accepted);
        assert_eq!(verdict.metric.as_deref(), Some("METRIC x candidate=1"));
        assert_eq!(
            verdict.output.matches("\"verdict\"").count(),
            1,
            "the verdict is recorded once: {}",
            verdict.output
        );
        assert!(verdict.output.trim_end().ends_with('}'));
    }

    #[test]
    fn an_accepted_line_on_stdout_alone_is_no_verdict() {
        let gate = FakeGate::new("echo '{\"verdict\":\"accepted\"}'\n");
        assert!(matches!(
            gate.judge(&path_only()),
            Err(GateFailure::Aborted { reason, .. }) if reason.contains("no verdict")
        ));
    }

    #[test]
    fn a_killed_gate_is_an_abort_whatever_it_wrote() {
        // The reviewer's forgery: an accepted verdict everywhere, then
        // SIGKILL before the gate finishes.
        let gate = FakeGate::new(
            "echo '{\"verdict\":\"accepted\"}' >\"$SPAWN_GATE_VERDICT\"\n\
             echo '{\"verdict\":\"accepted\"}'\n\
             kill -9 $$\n",
        );
        let Err(GateFailure::Aborted { reason, log }) = gate.judge(&path_only()) else {
            panic!("a signalled gate must abort");
        };
        assert!(reason.contains("signal"), "{reason}");
        assert!(log.contains("accepted"), "the log keeps what it printed");
    }

    #[test]
    fn a_non_zero_exit_is_an_abort() {
        let gate =
            FakeGate::new("echo '{\"verdict\":\"accepted\"}' >\"$SPAWN_GATE_VERDICT\"\nexit 1\n");
        assert!(matches!(
            gate.judge(&path_only()),
            Err(GateFailure::Aborted { reason, .. }) if reason.contains("exit status: 1")
        ));
    }

    const NO_ROOM_FACT: &str =
        r#"{"no_room":true,"free_kb":2097152,"min_gb":10,"tmpdir":"/data/tmp"}"#;

    #[test]
    fn exit_5_with_the_no_room_fact_is_no_room() {
        let gate = FakeGate::new(&format!(
            "echo 'gate: 2.0 GiB free' >&2\necho '{NO_ROOM_FACT}' >\"$SPAWN_GATE_VERDICT\"\nexit 5\n"
        ));
        let Err(GateFailure::NoRoom {
            free_kb,
            min_gb,
            tmpdir,
            log,
        }) = gate.judge(&path_only())
        else {
            panic!("no room is its own outcome");
        };
        assert_eq!(
            (free_kb, min_gb, tmpdir.as_str()),
            (2_097_152, 10, "/data/tmp")
        );
        assert!(log.contains("! gate: 2.0 GiB free"), "{log}");
    }

    #[test]
    fn exit_5_without_the_fact_is_an_abort() {
        // A bare status, a status with a verdict instead, or a fact that
        // names no directory: nothing recorded says "no room".
        for body in [
            "exit 5\n".to_string(),
            "echo '{\"verdict\":\"accepted\"}' >\"$SPAWN_GATE_VERDICT\"\nexit 5\n".to_string(),
            "echo '{\"no_room\":true,\"free_kb\":1,\"min_gb\":10}' >\"$SPAWN_GATE_VERDICT\"\nexit 5\n"
                .to_string(),
            "echo '{\"no_room\":true,\"free_kb\":\"lots\",\"min_gb\":10,\"tmpdir\":\"/t\"}' \
             >\"$SPAWN_GATE_VERDICT\"\nexit 5\n"
                .to_string(),
        ] {
            let gate = FakeGate::new(&body);
            assert!(
                matches!(
                    // Room to spare, whatever this host has.
                    gate.judge_room(&path_only(), &std::env::temp_dir(), Some(u64::MAX)),
                    Err(GateFailure::Aborted { ref reason, .. }) if reason.contains("exit status: 5")
                ),
                "{body}"
            );
        }
    }

    /// A TMPDIR at 0 bytes free: this process cannot even make its private
    /// directory. It measures the directory itself, and records no room only
    /// when the measure is under the floor.
    #[test]
    fn no_private_directory_on_a_full_disk_is_no_room_by_measure() {
        let gate = FakeGate::new("exit 0\n");
        let full = Path::new("/nonexistent/full-tmp");
        let Err(GateFailure::NoRoom {
            free_kb,
            min_gb,
            tmpdir,
            log,
        }) = gate.judge_room(&path_only(), full, Some(0))
        else {
            panic!("a full disk is no room");
        };
        assert_eq!((free_kb, min_gb), (0, 10), "the default floor");
        assert_eq!(tmpdir, "/nonexistent/full-tmp");
        assert!(
            log.contains("cannot create the gate's private directory"),
            "{log}"
        );
        // The same failure with room to spare, or unmeasurable: an abort.
        for measured in [Some(11 * 1_048_576), None] {
            assert!(
                matches!(
                    gate.judge_room(&path_only(), full, measured),
                    Err(GateFailure::Failed(_))
                ),
                "{measured:?}"
            );
        }
    }

    /// The gate exits 5 but could not write its fact (a full disk): the
    /// integrator measures the gate's TMPDIR against the gate's floor.
    #[test]
    fn exit_5_without_a_fact_is_no_room_only_by_measure() {
        let gate = FakeGate::new("exit 5\n");
        let parent = std::env::temp_dir();
        let env = |floor: &str| {
            let mut env = path_only();
            env.push(("TMPDIR".to_string(), "/data/full".to_string()));
            env.push(("SPAWN_GATE_MIN_FREE_GB".to_string(), floor.to_string()));
            env
        };
        let Err(GateFailure::NoRoom {
            free_kb,
            min_gb,
            tmpdir,
            log,
        }) = gate.judge_room(&env("4"), &parent, Some(0))
        else {
            panic!("exit 5 on a full TMPDIR is no room");
        };
        assert_eq!((free_kb, min_gb, tmpdir.as_str()), (0, 4, "/data/full"));
        assert!(log.contains("no readable no-room fact"), "{log}");
        for (floor, measured) in [("4", Some(4 * 1_048_576)), ("4", None), ("lots", Some(0))] {
            assert!(
                matches!(
                    gate.judge_room(&env(floor), &parent, measured),
                    Err(GateFailure::Aborted { .. })
                ),
                "floor {floor}, measured {measured:?}: an abort"
            );
        }
        // Any other exit, on the same full disk, still counts.
        let other = FakeGate::new("exit 3\n");
        assert!(matches!(
            other.judge_room(&env("4"), &parent, Some(0)),
            Err(GateFailure::Aborted { .. })
        ));
    }

    #[test]
    fn the_no_room_fact_with_another_status_is_an_abort() {
        // Exit 3 is every other refusal to start (a missing bind directory,
        // a bad floor): it counts, whatever the file says.
        for status in [0, 3] {
            let gate = FakeGate::new(&format!(
                "echo '{NO_ROOM_FACT}' >\"$SPAWN_GATE_VERDICT\"\nexit {status}\n"
            ));
            assert!(
                matches!(gate.judge(&path_only()), Err(GateFailure::Aborted { .. })),
                "exit {status}"
            );
        }
    }

    // --- objective 033: a base that disagrees with itself ------------------

    /// The gate's own pair for a scenario that exited 1 and then 0 on the
    /// base, as scripts/rsi/gate.sh prints it, inside a log.
    const DISAGREES_PAIR: &str = "| == base: 011.sh\n\
         | 011: covered within 10 s, delivered within 30 s, no churn\n\
         | 011.sh exited 1, then 0 on the base: a harness error, not a verdict; the gate aborts\n\
         | stage gate aborted 66871ms base: 011.sh again (unsolved needs two runs that agree)\n";

    /// The gate's timing line, after the pair.
    const GATE_TIME_LINE: &str = "| gate time: base 141 s (2 scenario runs, 19 results from the cache), candidate 0 s, \
         setup 244 s (exports, socket scan, static checks), total 385 s\n";

    /// A gate that prints `output` on stdout, then runs `status`.
    fn printing_gate(output: &str, status: &str) -> FakeGate {
        FakeGate::new(&format!("cat <<'GATE'\n{output}GATE\n{status}\n"))
    }

    #[test]
    fn the_pair_at_the_end_of_stdout_is_a_base_that_disagrees() {
        let gate = printing_gate(DISAGREES_PAIR, "exit 4");
        let Err(GateFailure::BaseDisagrees {
            scenario,
            setup_s,
            log,
        }) = gate.judge(&path_only())
        else {
            panic!("the pair with exit 4 is a base that disagrees");
        };
        assert_eq!(scenario, "011.sh");
        assert_eq!(setup_s, None, "no timing line: no setup");
        assert!(log.contains("| 011.sh exited 1, then 0"), "{log}");

        // Its `| gate time:` line may follow the pair, and nothing else.
        let gate = printing_gate(&format!("{DISAGREES_PAIR}{GATE_TIME_LINE}"), "exit 4");
        let Err(GateFailure::BaseDisagrees {
            scenario, setup_s, ..
        }) = gate.judge(&path_only())
        else {
            panic!("the pair with its timing line");
        };
        assert_eq!((scenario.as_str(), setup_s), ("011.sh", Some(244)));

        // The gate's own `| base runs:` phase line may sit between the pair
        // and its timing line: the gate printed one on 2026-11-08 05:02 and
        // none on 2026-11-06 14:43, and both are its own.
        let runs = "| base runs: 17 in 139 s, 5 at a time (their own time: 576 s)\n";
        for trailing in [
            runs.to_string(),
            format!("{runs}{GATE_TIME_LINE}"),
            format!("{GATE_TIME_LINE}{runs}"),
        ] {
            let gate = printing_gate(&format!("{DISAGREES_PAIR}{trailing}"), "exit 4");
            let Err(GateFailure::BaseDisagrees {
                scenario, setup_s, ..
            }) = gate.judge(&path_only())
            else {
                panic!("{trailing:?} is the gate's own trailing line");
            };
            assert_eq!(scenario, "011.sh");
            assert_eq!(
                setup_s,
                if trailing.contains("gate time") {
                    Some(244)
                } else {
                    None
                },
                "{trailing:?}"
            );
        }

        // Stderr lines after it say nothing about the pair.
        let gate = printing_gate(
            &format!("{DISAGREES_PAIR}{GATE_TIME_LINE}"),
            "echo '! 011: a line on stderr' >&2; exit 4",
        );
        let Err(GateFailure::BaseDisagrees {
            scenario, setup_s, ..
        }) = gate.judge(&path_only())
        else {
            panic!("stderr after the pair is no part of it");
        };
        assert_eq!((scenario.as_str(), setup_s), ("011.sh", Some(244)));
    }

    #[test]
    fn the_setup_is_read_from_the_timing_line_or_left_out() {
        let pair = |times: &str| super::base_disagrees(&format!("{DISAGREES_PAIR}{times}"));
        assert_eq!(
            pair(""),
            Some(BaseDisagrees {
                scenario: "011.sh".to_string(),
                setup_s: None,
            })
        );
        for (seconds, want) in [("0", 0), ("29", 29), ("244", 244), ("1417", 1417)] {
            let line =
                format!("| gate time: base 1 s, candidate 0 s, setup {seconds} s (x), total 2 s\n");
            assert_eq!(
                pair(&line).map(|found| found.setup_s),
                Some(Some(want)),
                "setup {seconds} s"
            );
        }
        // A timing line without a setup says nothing: no field.
        assert_eq!(
            pair("| gate time: base 1 s, candidate 0 s, total 2 s\n").map(|found| found.setup_s),
            Some(None)
        );
    }

    #[test]
    fn a_nested_gates_copy_of_the_line_is_not_it() {
        // A scenario of the gate ran a gate of its own, whose output the
        // outer gate prefixes again: `| | …`. It names the inner scenario,
        // and is not the outer gate's own pair.
        let nested = "| | 102.sh exited 1, then 0 on the base: a harness error, not a verdict; \
                      the gate aborts\n\
                      | | stage gate aborted 5ms base: 102.sh again (unsolved needs two runs that \
                      agree)\n";
        let mut output = String::from(DISAGREES_PAIR);
        output.push_str(nested);
        // The nested copy first, the gate's own pair last: the outer one.
        let mut reordered = String::from(nested);
        reordered.push_str(DISAGREES_PAIR);
        assert_eq!(
            super::base_disagrees(&reordered).map(|found| found.scenario),
            Some("011.sh".to_string())
        );
        // The nested copy alone, at the end, is nobody's pair.
        let mut alone = String::from("| == base: 011.sh\n");
        alone.push_str(nested);
        assert_eq!(super::base_disagrees(&alone), None);
        let gate = printing_gate(&alone, "exit 4");
        assert!(matches!(
            gate.judge(&path_only()),
            Err(GateFailure::Aborted { .. })
        ));
    }

    #[test]
    fn anything_after_the_pair_is_not_it() {
        // A line after the pair, even a harmless one: the gate's stdout does
        // not end with its own pair.
        for tail in [
            "| == candidate: build\n",
            "| gate time: base 1 s, candidate 0 s, setup 2 s (x), total 3 s\n| == more\n",
            "| stage gate ok 5ms candidate: build\n",
        ] {
            let output = format!("{DISAGREES_PAIR}{tail}");
            assert_eq!(super::base_disagrees(&output), None, "{tail:?}");
            let gate = printing_gate(&output, "exit 4");
            assert!(
                matches!(gate.judge(&path_only()), Err(GateFailure::Aborted { .. })),
                "{tail:?}"
            );
        }
        // The pair is two lines: the first alone, or with another name in
        // the second, is no pair.
        assert_eq!(
            super::base_disagrees(
                "| 011.sh exited 1, then 0 on the base: a harness error, not a verdict; the gate \
                 aborts\n"
            ),
            None
        );
        assert_eq!(
            super::base_disagrees(
                "| 011.sh exited 1, then 0 on the base: a harness error, not a verdict; the gate \
                 aborts\n\
                 | stage gate aborted 5ms base: 012.sh again (unsolved needs two runs that agree)\n"
            ),
            None
        );
    }

    #[test]
    fn another_harness_error_on_the_base_stays_an_abort() {
        // `exited 2`, `1, then 2`, `1, then 3`: real harness errors.
        for said in [
            "011.sh exited 2 on the base: a harness error, not a verdict; the gate aborts",
            "011.sh exited 1, then 2 on the base: a harness error, not a verdict; the gate aborts",
            "011.sh exited 1, then 3 on the base: a harness error, not a verdict; the gate aborts",
            "011.sh exited 1 on the base: a harness error, not a verdict; the gate aborts",
        ] {
            let output = format!(
                "| == base: 011.sh\n| {said}\n\
                 | stage gate aborted 5ms base: 011.sh again (unsolved needs two runs that agree)\n"
            );
            assert_eq!(super::base_disagrees(&output), None, "{said}");
            let gate = printing_gate(&output, "exit 4");
            let Err(GateFailure::Aborted { log, .. }) = gate.judge(&path_only()) else {
                panic!("{said} is an abort");
            };
            assert!(log.contains("011.sh"), "{log}");
        }
    }

    #[test]
    fn the_pair_with_another_exit_status_is_no_host_fault() {
        let parent = std::env::temp_dir();
        // 0 (a verdict, or none), 1, 3 and a signal: each an abort.
        for status in ["exit 0", "exit 1", "exit 3", "kill -9 $$"] {
            let gate = printing_gate(&format!("{DISAGREES_PAIR}{GATE_TIME_LINE}"), status);
            assert!(
                matches!(
                    gate.judge_room(&path_only(), &parent, Some(u64::MAX)),
                    Err(GateFailure::Aborted { .. })
                ),
                "{status}"
            );
        }
        // 5 is the no-room exit: with no room measured, it is an abort; the
        // pair says nothing.
        let gate = printing_gate(&format!("{DISAGREES_PAIR}{GATE_TIME_LINE}"), "exit 5");
        assert!(matches!(
            gate.judge_room(&path_only(), &parent, Some(u64::MAX)),
            Err(GateFailure::Aborted { .. })
        ));
    }

    #[test]
    fn the_pair_with_a_readable_no_room_fact_is_no_room() {
        // Exit 5 with its fact is the gate's no-room fact, whatever it
        // printed before: the first of the two host facts wins.
        let gate = FakeGate::new(&format!(
            "cat <<'GATE'\n{DISAGREES_PAIR}GATE\n\
             echo '{{\"no_room\":true,\"free_kb\":2097152,\"min_gb\":10,\"tmpdir\":\"/data/tmp\"}}' \
             >\"$SPAWN_GATE_VERDICT\"\n\
             exit 5\n"
        ));
        assert!(matches!(
            gate.judge(&path_only()),
            Err(GateFailure::NoRoom { .. })
        ));
    }

    /// The gate's fact of a host fault (scripts/rsi/gate.sh `host_fault`).
    const HOST_FAULT_FACT: &str = "{\"host_fault\":true,\"scenario\":\"012.sh\",\"on\":\"base\",\
         \"runs\":\"1, then 0\",\"reason\":\"012.sh exited 1, then 0 on the base\"}";

    #[test]
    fn exit_6_with_the_host_fault_fact_is_a_host_fault() {
        let gate = FakeGate::new(&format!(
            "echo '| == base: 012.sh'\necho '{HOST_FAULT_FACT}' >\"$SPAWN_GATE_VERDICT\"\nexit 6\n"
        ));
        let Err(GateFailure::HostFault {
            scenario,
            on,
            runs,
            reason,
            log,
        }) = gate.judge(&path_only())
        else {
            panic!("exit 6 with its fact is a host fault");
        };
        assert_eq!(
            (scenario.as_str(), on.as_str(), runs.as_str()),
            ("012.sh", "base", "1, then 0")
        );
        assert!(reason.contains("on the base"), "{reason}");
        assert!(
            log.contains("| == base: 012.sh"),
            "the gate's output: {log}"
        );
        let both = HOST_FAULT_FACT
            .replace("\"on\":\"base\"", "\"on\":\"candidate and base\"")
            .replace("012.sh", "check-e2e.sh");
        let gate = FakeGate::new(&format!("echo '{both}' >\"$SPAWN_GATE_VERDICT\"\nexit 6\n"));
        assert!(matches!(
            gate.judge(&path_only()),
            Err(GateFailure::HostFault { on, .. }) if on == "candidate and base"
        ));
    }

    #[test]
    fn the_host_fault_fact_counts_only_whole_and_with_exit_6() {
        // Another status, the fact on stdout only, or a fact that names no
        // scenario, an unknown place, or a verdict too: an abort, which is
        // what a gate that dies is.
        let cases = [
            format!("echo '{HOST_FAULT_FACT}' >\"$SPAWN_GATE_VERDICT\"\nexit 4\n"),
            format!("echo '{HOST_FAULT_FACT}' >\"$SPAWN_GATE_VERDICT\"\nexit 0\n"),
            format!("echo '{HOST_FAULT_FACT}'\nexit 6\n"),
            "exit 6\n".to_string(),
            format!(
                "echo '{}' >\"$SPAWN_GATE_VERDICT\"\nexit 6\n",
                HOST_FAULT_FACT.replace("\"scenario\":\"012.sh\"", "\"scenario\":\"\"")
            ),
            format!(
                "echo '{}' >\"$SPAWN_GATE_VERDICT\"\nexit 6\n",
                HOST_FAULT_FACT.replace("\"on\":\"base\"", "\"on\":\"candidate\"")
            ),
            format!(
                "echo '{}' >\"$SPAWN_GATE_VERDICT\"\nexit 6\n",
                HOST_FAULT_FACT.replace('{', "{\"verdict\":\"accepted\",")
            ),
        ];
        for script in cases {
            assert!(
                matches!(
                    FakeGate::new(&script).judge(&path_only()),
                    Err(GateFailure::Aborted { .. })
                ),
                "{script}"
            );
        }
    }

    #[test]
    fn the_file_outranks_stdout() {
        let gate = FakeGate::new(
            "echo '{\"verdict\":\"rejected\",\"reason\":\"a \\\"quoted\\\" reason\"}' >\"$SPAWN_GATE_VERDICT\"\n\
             echo '{\"verdict\":\"accepted\"}'\n",
        );
        let verdict = gate.judge(&path_only()).expect("verdict");
        assert!(!verdict.accepted);
        assert_eq!(verdict.reason, "a \"quoted\" reason");
    }

    #[test]
    fn the_gate_sees_only_the_environment_it_is_given() {
        // Cargo sets CARGO_PKG_NAME for this test process; the gate must not
        // inherit it, nor anything else.
        assert!(std::env::var_os("CARGO_PKG_NAME").is_some());
        let gate = FakeGate::new(
            "if [ -n \"${CARGO_PKG_NAME-}\" ] || [ -n \"${SPAWN_DATABASE_URL-}\" ] \
                 || [ \"$SPAWN_TEST_DATABASE_URL\" != postgres://t@127.0.0.1:15434/spawn_test ]; then\n\
                 echo '{\"verdict\":\"rejected\",\"reason\":\"leak\"}' >\"$SPAWN_GATE_VERDICT\"\n\
             else\n\
                 echo '{\"verdict\":\"accepted\"}' >\"$SPAWN_GATE_VERDICT\"\n\
             fi\n",
        );
        let lookup = |name: &str| match name {
            "PATH" => Some("/usr/bin:/bin".to_string()),
            "SPAWN_TEST_DATABASE_URL" => {
                Some("postgres://t@127.0.0.1:15434/spawn_test".to_string())
            }
            "SPAWN_DATABASE_URL" => Some("postgres://o@127.0.0.1:15433/spawn".to_string()),
            _ => None,
        };
        let env = gate_env(lookup);
        let verdict = gate.judge(&env).expect("verdict");
        assert!(verdict.accepted, "{}", verdict.reason);
    }

    #[test]
    fn the_allowlist_passes_what_gate_sh_reads_and_nothing_else() {
        let lookup = |name: &str| match name {
            "PATH" | "HOME" | "LANG" | "TMPDIR" | "CARGO_HOME" | "RUSTUP_HOME" => {
                Some(format!("{name}-value"))
            }
            "SPAWN_TEST_DATABASE_URL" => Some("postgres://t:p@127.0.0.1:15434/spawn_test".into()),
            "SPAWN_GATE_MIN_FREE_GB" => Some("10".into()),
            "SPAWN_GATE_JOBS" => Some("4".into()),
            "SPAWN_GATE_PG_IMAGE" => Some("postgres:16-alpine".into()),
            "SPAWN_GATE_PG_BIN" => Some("/usr/lib/postgresql/16/bin".into()),
            "SPAWN_DATABASE_URL" => Some("postgres://o:p@127.0.0.1:15433/spawn".into()),
            "SPAWN_JUDGE_URL" | "http_proxy" | "SSH_AUTH_SOCK" => Some("secret".into()),
            _ => None,
        };
        let env = gate_env(lookup);
        let names: Vec<&str> = env.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "PATH",
                "HOME",
                "LANG",
                "TMPDIR",
                "CARGO_HOME",
                "RUSTUP_HOME",
                "SPAWN_TEST_DATABASE_URL",
                "SPAWN_GATE_MIN_FREE_GB",
                "SPAWN_GATE_JOBS",
                "SPAWN_GATE_PG_IMAGE",
                "SPAWN_GATE_PG_BIN"
            ]
        );
        let unset = |name: &str| (name == "PATH").then(|| "/usr/bin".to_string());
        assert_eq!(gate_env(unset).len(), 1, "unset names are left out");
    }

    #[test]
    fn a_test_database_that_is_the_owners_is_withheld() {
        let lookup = |name: &str| match name {
            "SPAWN_TEST_DATABASE_URL" => Some("postgres://other:pw@127.0.0.1:15433/spawn".into()),
            "SPAWN_DATABASE_URL" => {
                Some("postgres://owner:pw@127.0.0.1:15433/spawn?sslmode=disable".into())
            }
            _ => None,
        };
        let env = gate_env(lookup);
        assert!(
            env.iter()
                .all(|(name, _)| name != "SPAWN_TEST_DATABASE_URL")
        );
    }

    /// Whether the test URL reaches the gate, next to this owner URL.
    fn passed(owner: &str, test: &str) -> bool {
        let lookup = |name: &str| match name {
            "SPAWN_TEST_DATABASE_URL" => Some(test.to_string()),
            "SPAWN_DATABASE_URL" => Some(owner.to_string()),
            _ => None,
        };
        gate_env(lookup)
            .iter()
            .any(|(name, _)| name == "SPAWN_TEST_DATABASE_URL")
    }

    #[test]
    fn a_test_database_on_the_owners_cluster_is_withheld_whatever_its_name() {
        let owner = "postgres://owner:pw@127.0.0.1:15433/spawn";
        // The reviewer's case: another database on the same cluster.
        assert!(!passed(
            owner,
            "postgres://postgres:pw@127.0.0.1:15433/spawn_test"
        ));
        assert!(!passed(
            owner,
            "postgres://postgres:pw@localhost:15433/other"
        ));
        assert!(!passed(
            "postgres://owner:pw@[::1]:15433/spawn",
            "postgres://t:pw@127.0.0.1:15433/x"
        ));
        assert!(passed(
            owner,
            "postgres://postgres:pw@127.0.0.1:15434/spawn_test"
        ));
        assert!(passed(
            owner,
            "postgres://postgres:pw@%2Fhome%2Fu%2Fsock/spawn_test"
        ));
        // Default ports compare equal.
        assert!(!passed(
            "postgres://o:pw@db.local/spawn",
            "postgres://t:pw@db.local:5432/spawn_test"
        ));
    }

    #[test]
    fn a_test_database_on_the_owners_socket_is_withheld() {
        let owner = "postgres://owner:pw@%2Fhome%2Fu%2Fsock/spawn";
        assert!(!passed(
            owner,
            "postgres://postgres:pw@%2Fhome%2Fu%2Fsock/spawn_test"
        ));
        assert!(!passed(
            owner,
            "postgres://postgres:pw@%2fhome%2fu%2fsock%2F/x"
        ));
        assert!(passed(
            owner,
            "postgres://postgres:pw@%2Fhome%2Fu%2Fother/spawn_test"
        ));
    }

    fn with_test_url(test: &str) -> Vec<(String, String)> {
        vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("SPAWN_TEST_DATABASE_URL".to_string(), test.to_string()),
        ]
    }

    /// Identities by URL, as two servers would answer: the live server
    /// under four spellings, the test server, and one that is down.
    fn servers(url: &str) -> Result<String, String> {
        match url {
            "postgres://o:pw@127.0.0.1:15433/spawn"
            | "postgres://t:pw@%2Fvar%2Frun%2Flive/spawn_test"
            | "postgres://t:pw@127.0.0.2:15433/spawn_test"
            | "postgres://t:pw@myhost:15433/spawn_test" => Ok("7703".to_string()),
            "postgres://t:pw@%2Fhome%2Fu%2Ftest-sock/spawn_test" => Ok("7704".to_string()),
            _ => Err("connection refused".to_string()),
        }
    }

    fn kept(owner: Option<&str>, test: &str) -> (bool, Option<String>) {
        let (env, warning) = withhold_owners_cluster(&with_test_url(test), owner, servers);
        assert!(env.iter().any(|(name, _)| name == "PATH"), "the rest stays");
        (
            env.iter()
                .any(|(name, _)| name == "SPAWN_TEST_DATABASE_URL"),
            warning,
        )
    }

    #[test]
    fn the_test_url_stays_only_for_another_server() {
        let owner = Some("postgres://o:pw@127.0.0.1:15433/spawn");
        assert_eq!(
            kept(owner, "postgres://t:pw@%2Fhome%2Fu%2Ftest-sock/spawn_test"),
            (true, None)
        );
        // The live server over its socket, 127.0.0.2, a hostname: one
        // identity, so withheld, though the text names another cluster.
        for spelling in [
            "postgres://t:pw@%2Fvar%2Frun%2Flive/spawn_test",
            "postgres://t:pw@127.0.0.2:15433/spawn_test",
            "postgres://t:pw@myhost:15433/spawn_test",
        ] {
            let (passed, warning) = kept(owner, spelling);
            assert!(!passed, "{spelling} reaches the owner's server");
            assert!(warning.is_some_and(|w| w.contains("owner's server")));
        }
    }

    #[test]
    fn an_unreadable_identity_withholds_the_test_url() {
        let owner = Some("postgres://o:pw@127.0.0.1:15433/spawn");
        let (passed, warning) = kept(owner, "postgres://t:pw@%2Fdown/spawn_test");
        assert!(!passed);
        assert!(warning.is_some_and(|w| w.contains("test server")));
        let (passed, warning) = kept(
            Some("postgres://o:pw@%2Fdown/spawn"),
            "postgres://t:pw@%2Fhome%2Fu%2Ftest-sock/spawn_test",
        );
        assert!(!passed);
        assert!(warning.is_some_and(|w| w.contains("owner's server")));
        let (passed, _) = kept(None, "postgres://t:pw@%2Fhome%2Fu%2Ftest-sock/spawn_test");
        assert!(!passed, "no owner URL to compare with");
    }

    #[test]
    fn no_test_url_asks_no_server() {
        let env = vec![("PATH".to_string(), "/usr/bin".to_string())];
        let (kept, warning) = withhold_owners_cluster(&env, None, |_| panic!("no connection"));
        assert_eq!(kept, env);
        assert!(warning.is_none());
    }
}

/// Objective 018: the host's room, measured before a gate starts, on fake
/// measures ([`Room::measured_by`]).
#[cfg(test)]
mod room_tests {
    use super::{Room, RoomFailure, Shortfall, available_mb, server_of, without_password};
    use spawn_postgres::ProbeFailure;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    const URL: &str = "postgres://gate:hunter2@%2Fvar%2Frun%2Ftest-pg:5433/spawn_test";

    /// The integrator's environment: these names set, nothing else.
    fn integrator_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    /// The gate's environment: PATH, TMPDIR, and these.
    fn gate_env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut env = vec![
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("TMPDIR".to_string(), "/data/tmp".to_string()),
        ];
        env.extend(
            pairs
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string())),
        );
        env
    }

    /// A room whose measures say `free_kb`, `available_mb` and `probe`,
    /// noting each measure taken in `taken`.
    fn room(
        integrator: &[(&str, &str)],
        free_kb: Option<u64>,
        available_mb: Option<u64>,
        probe: Result<(), ProbeFailure>,
        taken: &Arc<Mutex<Vec<String>>>,
    ) -> Room {
        let (disk, memory, database) = (Arc::clone(taken), Arc::clone(taken), Arc::clone(taken));
        Room::from_env(integrator_env(integrator)).measured_by(
            move |path: &Path| {
                disk.lock()
                    .expect("taken")
                    .push(format!("disk {}", path.display()));
                free_kb
            },
            move || {
                memory.lock().expect("taken").push("memory".to_string());
                available_mb
            },
            move |url: &str| {
                database
                    .lock()
                    .expect("taken")
                    .push(format!("database {url}"));
                probe.clone()
            },
        )
    }

    fn taken() -> Arc<Mutex<Vec<String>>> {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn short(result: Result<String, RoomFailure>) -> (Shortfall, String) {
        match result {
            Err(RoomFailure::Short { short, log }) => (short, log),
            other => panic!("a check falls short: {other:?}"),
        }
    }

    const GIB_KB: u64 = 1_048_576;
    const PLENTY: Option<u64> = Some(u64::MAX / 2);

    fn shared_memory() -> ProbeFailure {
        ProbeFailure {
            probe: "a parallel query within 10 s".to_string(),
            error: "ERROR: could not resize shared memory segment \"/PostgreSQL.2\" to 8388608 \
                    bytes: No space left on device (53100)"
                .to_string(),
        }
    }

    #[test]
    fn each_check_short_names_its_measure_and_threshold() {
        // Disk: the gate's TMPDIR under the default floor of 10 GiB.
        let seen = taken();
        let (disk, log) =
            short(room(&[], Some(3 * GIB_KB), PLENTY, Ok(()), &seen).check(&gate_env(&[])));
        assert_eq!(
            disk,
            Shortfall::Disk {
                free_kb: 3 * GIB_KB,
                min_gb: 10,
                tmpdir: "/data/tmp".to_string()
            }
        );
        assert_eq!(disk.check(), "disk");
        assert_eq!(disk.measured(), 3 * GIB_KB);
        assert_eq!(disk.threshold(), 10 * GIB_KB, "the floor in KiB");
        assert!(log.contains("SPAWN_GATE_MIN_FREE_GB=10"), "{log}");
        assert_eq!(*seen.lock().expect("taken"), ["disk /data/tmp"]);
        // The floor the gate reads, from the gate's environment.
        let (disk, _) = short(
            room(&[], Some(3 * GIB_KB), PLENTY, Ok(()), &taken())
                .check(&gate_env(&[("SPAWN_GATE_MIN_FREE_GB", "4")])),
        );
        assert_eq!(disk.threshold(), 4 * GIB_KB);

        // Memory: MemAvailable under the default floor of 2048 MiB.
        let (memory, log) =
            short(room(&[], PLENTY, Some(1500), Ok(()), &taken()).check(&gate_env(&[])));
        assert_eq!(
            memory,
            Shortfall::Memory {
                available_mb: 1500,
                min_mb: 2048
            }
        );
        assert_eq!(
            (memory.check(), memory.measured(), memory.threshold()),
            ("memory", 1500.into(), 2048.into())
        );
        assert!(log.contains("disk: ") && log.contains(": ok"), "{log}");
        let (memory, _) = short(
            room(
                &[("SPAWN_GATE_MIN_FREE_MEMORY_MB", "100000000")],
                PLENTY,
                Some(30_000),
                Ok(()),
                &taken(),
            )
            .check(&gate_env(&[])),
        );
        assert_eq!(memory.threshold(), 100_000_000);

        // Database: the probe's error and the probe it failed.
        let failure = ProbeFailure {
            probe: "connect within 10 s".to_string(),
            error: "error connecting to server: No such file or directory (os error 2)".to_string(),
        };
        let (database, _) = short(
            room(
                &[("SPAWN_TEST_DATABASE_URL", URL)],
                PLENTY,
                PLENTY,
                Err(failure),
                &taken(),
            )
            .check(&gate_env(&[])),
        );
        assert_eq!(database.check(), "database");
        assert_eq!(
            database.measured(),
            "error connecting to server: No such file or directory (os error 2)"
        );
        assert_eq!(database.threshold(), "connect within 10 s");
        assert!(matches!(
            database,
            Shortfall::Database { ref server, .. } if server == "/var/run/test-pg:5433/spawn_test"
        ));
    }

    #[test]
    fn a_threshold_of_zero_disables_its_check() {
        let seen = taken();
        let checks = room(
            &[("SPAWN_GATE_MIN_FREE_MEMORY_MB", "0")],
            Some(0),
            Some(0),
            Ok(()),
            &seen,
        )
        .check(&gate_env(&[("SPAWN_GATE_MIN_FREE_GB", "0")]))
        .expect("nothing is checked, so nothing falls short");
        assert!(seen.lock().expect("taken").is_empty(), "no measure taken");
        assert!(
            checks.contains("SPAWN_GATE_MIN_FREE_GB=0, not checked"),
            "{checks}"
        );
        assert!(
            checks.contains("SPAWN_GATE_MIN_FREE_MEMORY_MB=0, not checked"),
            "{checks}"
        );
        // One disabled, the other still checked.
        let (memory, _) = short(
            room(&[], Some(0), Some(10), Ok(()), &taken())
                .check(&gate_env(&[("SPAWN_GATE_MIN_FREE_GB", "0")])),
        );
        assert_eq!(memory.check(), "memory");
    }

    #[test]
    fn the_checks_run_disk_memory_database_and_stop_at_the_first_short() {
        let database = [("SPAWN_TEST_DATABASE_URL", URL)];
        let order = |free_kb, available_mb, probe| {
            let seen = taken();
            let result = room(&database, free_kb, available_mb, probe, &seen).check(&gate_env(&[]));
            let seen = seen.lock().expect("taken").clone();
            (result, seen)
        };
        let (result, seen) = order(PLENTY, PLENTY, Ok(()));
        let checks = result.expect("every check met");
        assert_eq!(
            seen,
            ["disk /data/tmp", "memory", &format!("database {URL}")],
            "every check, in order"
        );
        assert_eq!(checks.lines().count(), 3, "{checks}");
        let (result, seen) = order(Some(0), Some(0), Err(shared_memory()));
        assert_eq!(short(result).0.check(), "disk");
        assert_eq!(
            seen,
            ["disk /data/tmp"],
            "disk short: nothing else measured"
        );
        let (result, seen) = order(PLENTY, Some(0), Err(shared_memory()));
        assert_eq!(short(result).0.check(), "memory");
        assert_eq!(seen, ["disk /data/tmp", "memory"], "memory short: no probe");
        let (result, _) = order(PLENTY, PLENTY, Err(shared_memory()));
        assert_eq!(short(result).0.check(), "database");
    }

    #[test]
    fn a_parallel_query_short_of_shared_memory_is_the_database_check() {
        let (database, log) = short(
            room(
                &[("SPAWN_TEST_DATABASE_URL", URL)],
                PLENTY,
                PLENTY,
                Err(shared_memory()),
                &taken(),
            )
            .check(&gate_env(&[])),
        );
        assert_eq!(database.check(), "database");
        assert!(
            database
                .measured()
                .as_str()
                .is_some_and(|error| error.contains("could not resize shared memory segment")),
            "{database:?}"
        );
        assert_eq!(database.threshold(), "a parallel query within 10 s");
        assert!(
            log.contains("could not resize shared memory segment"),
            "{log}"
        );
        assert!(log.contains("the gate did not run"), "{log}");
    }

    #[test]
    fn the_password_never_leaves_the_check() {
        // The password as written (percent-encoded) and decoded, in the
        // probe's words: neither survives, and the server is named alone.
        let url = "postgres://gate:hun%40ter2@db.internal:5434/spawn_test?sslmode=disable";
        let failure = ProbeFailure {
            probe: "connect within 10 s".to_string(),
            error: format!("password authentication failed for gate:hun@ter2 ({url})"),
        };
        let (database, log) = short(
            room(
                &[("SPAWN_TEST_DATABASE_URL", url)],
                PLENTY,
                PLENTY,
                Err(failure),
                &taken(),
            )
            .check(&gate_env(&[])),
        );
        let text = format!("{database:?} {log}");
        assert!(
            !text.contains("hun@ter2") && !text.contains("hun%40ter2"),
            "{text}"
        );
        assert!(matches!(
            database,
            Shortfall::Database { ref server, .. } if server == "db.internal:5434/spawn_test"
        ));
        assert_eq!(
            without_password(
                "x postgres://u:pw@h/d?password=other y other",
                "postgres://u:pw@h/d?password=other"
            ),
            "x postgres://u:***@h/d?password=*** y ***"
        );
        assert_eq!(
            without_password("no secret", "postgres://u@h/d"),
            "no secret"
        );
    }

    #[test]
    fn a_threshold_that_does_not_parse_is_an_abort() {
        for (gate, integrator) in [
            (vec![("SPAWN_GATE_MIN_FREE_GB", "lots")], vec![]),
            (vec![("SPAWN_GATE_MIN_FREE_GB", "+5")], vec![]),
            (vec![], vec![("SPAWN_GATE_MIN_FREE_MEMORY_MB", "2G")]),
            (vec![], vec![("SPAWN_GATE_MIN_FREE_MEMORY_MB", "-1")]),
            (
                vec![],
                vec![("SPAWN_GATE_MIN_FREE_MEMORY_MB", "99999999999999999999999")],
            ),
        ] {
            let seen = taken();
            let result = room(&integrator, Some(0), Some(0), Ok(()), &seen).check(&gate_env(&gate));
            assert!(
                matches!(&result, Err(RoomFailure::Bad(reason))
                         if reason.contains("SPAWN_GATE_MIN_FREE_") && reason.contains("whole number")),
                "{gate:?} {integrator:?}: {result:?}"
            );
            assert!(seen.lock().expect("taken").is_empty(), "nothing measured");
        }
    }

    #[test]
    fn no_test_database_url_probes_nothing_and_an_unreadable_measure_decides_nothing() {
        let seen = taken();
        let checks = room(&[], None, None, Err(shared_memory()), &seen)
            .check(&gate_env(&[]))
            .expect("the gate runs and checks its own floor");
        assert_eq!(*seen.lock().expect("taken"), ["disk /data/tmp", "memory"]);
        assert!(
            checks.contains("SPAWN_TEST_DATABASE_URL is unset"),
            "{checks}"
        );
        assert!(checks.contains("cannot be read"), "{checks}");
        // TMPDIR unset in the gate's environment: /tmp, as gate.sh reads it.
        let seen = taken();
        let env = vec![("PATH".to_string(), "/bin".to_string())];
        assert!(room(&[], PLENTY, PLENTY, Ok(()), &seen).check(&env).is_ok());
        assert_eq!(seen.lock().expect("taken")[0], "disk /tmp");
    }

    /// A mission whose gate needs no test database (`no-test-database`)
    /// is not held back by the test server: no probe, the other checks as
    /// before.
    #[test]
    fn a_gate_without_a_test_database_is_not_probed() {
        let seen = taken();
        let room = room(
            &[("SPAWN_TEST_DATABASE_URL", URL)],
            PLENTY,
            PLENTY,
            Err(shared_memory()),
            &seen,
        );
        let checks = room
            .without_test_database()
            .check(&gate_env(&[]))
            .expect("no database to check");
        assert_eq!(*seen.lock().expect("taken"), ["disk /data/tmp", "memory"]);
        assert!(checks.contains("no-test-database"), "{checks}");
        let (memory, _) = short(
            room.measured_by(|_| PLENTY, || Some(1), |_| panic!("no probe"))
                .without_test_database()
                .check(&gate_env(&[])),
        );
        assert_eq!(memory.check(), "memory", "disk and memory still count");
        // The same room with the database: the probe decides.
        let (database, _) = short(room_with_url_failing(&taken()).check(&gate_env(&[])));
        assert_eq!(database.check(), "database");
    }

    fn room_with_url_failing(seen: &Arc<Mutex<Vec<String>>>) -> Room {
        room(
            &[("SPAWN_TEST_DATABASE_URL", URL)],
            PLENTY,
            PLENTY,
            Err(shared_memory()),
            seen,
        )
    }

    #[test]
    fn mem_available_is_read_in_mib() {
        let meminfo = "MemTotal:       32800860 kB\nMemFree:         1000000 kB\n\
                       MemAvailable:   20293592 kB\nBuffers:          100 kB\n";
        assert_eq!(available_mb(meminfo), Some(19_817));
        assert_eq!(
            available_mb("MemTotal: 1 kB\n"),
            None,
            "no MemAvailable line"
        );
        assert_eq!(available_mb("MemAvailable: lots kB\n"), None);
    }

    #[test]
    fn the_server_is_named_without_its_login() {
        assert_eq!(
            server_of("postgres://u:p@%2Fhome%2Fu%2Fsock/spawn_test"),
            "/home/u/sock:5432/spawn_test"
        );
        assert_eq!(
            server_of("postgresql://u:p@127.0.0.1:15434/x?sslmode=disable"),
            "127.0.0.1:15434/x"
        );
        assert_eq!(server_of("postgres://u:p@[::1]:5433/x"), "[::1]:5433/x");
        assert_eq!(
            server_of("postgres:///spawn_test?host=/run/pg&port=5433&password=p"),
            "/run/pg:5433/spawn_test"
        );
        assert_eq!(server_of("postgres://u:p@db/"), "db:5432/");
    }
}
