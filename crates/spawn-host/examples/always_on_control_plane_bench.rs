//! Control-plane PG16 concurrency, sharded Bridge claim, and Context/Work-Hours
//! attribution completeness benchmark (`always_on_control_plane_bench`).
//!
//! Reads ONLY `SPAWN_BENCH_DATABASE_URL` (never `SPAWN_DATABASE_URL`), creates a
//! disposable schema, drives concurrent `EventLog::append` + live `read_global`
//! (`xid8` watermark) readers, multi-shard `CommandQueue::claim_for_shard`
//! workers, and end-to-end context/work-hours (`agent_id`, `execution_id`,
//! `task_id`, `model_label`, `transcript_ref`, `duration_ms`, tokens/USD), then
//! prints one deterministic JSON report.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use spawn_cas::sha256_of;
use spawn_core::bridge::{Bridge, BridgeConfig};
use spawn_core::command::{CommandAction, Outcome, Submission};
use spawn_core::event::{ENGINE, EventKind, GlobalPosition, ModelProfile, NewEvent, Source};
use spawn_core::id::{
    AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId, TerminalName,
};
use spawn_core::ingest;
use spawn_core::memory::{MemoryStore, MemoryWatch};
use spawn_core::port::{BridgeShard, CommandQueue, EventLog, ShardPartition, TranscriptArchive};
use spawn_core::time::SystemClock;
use spawn_host::LocalTranscriptArchive;
use spawn_postgres::{
    PostgresStore, ProvAttributionSource, ProvBoundary, ProvChangeInput, ProvChangeKind,
    ProvEntryClass, ProvEntryInput,
};

const USAGE: &str = "spawn always-on control-plane & attribution benchmark\n\
\n\
Usage: cargo run --release -p spawn-host --example always_on_control_plane_bench -- [options]\n\
\n\
  --sessions <n>              Simulated concurrent agent sessions (default: 16)\n\
  --events-per-session <n>    Events written per session (default: 64)\n\
  --shards <n>                Bridge partitions to run concurrently (default: 4)\n\
  --commands-per-session <n>  Commands submitted/claimed per session (default: 16)\n\
  --writer-pool <n>           Concurrent PG writer connections (default: 8)\n\
  --attribution-tasks <n>     Bridge-driven tasks to check for full lineage (default: 16)\n\
  --archive-root <dir>        Transcript archive root (default: unique temp dir)\n\
  --help                      Show this help\n\
\n\
Requires SPAWN_BENCH_DATABASE_URL pointing to an isolated benchmark database.\n";

#[derive(Debug)]
struct Config {
    sessions: usize,
    events_per_session: usize,
    shards: u16,
    commands_per_session: usize,
    writer_pool: usize,
    attribution_tasks: usize,
    archive_root: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sessions: 16,
            events_per_session: 64,
            shards: 4,
            commands_per_session: 16,
            writer_pool: 8,
            attribution_tasks: 16,
            archive_root: None,
        }
    }
}

fn parse_usize(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<usize, String> {
    let raw = args
        .next()
        .ok_or_else(|| format!("{flag} requires a positive integer"))?;
    let val = raw
        .parse::<usize>()
        .map_err(|_| format!("{flag} requires a positive integer, got {raw:?}"))?;
    if val == 0 {
        return Err(format!("{flag} must be >= 1"));
    }
    Ok(val)
}

fn parse_config() -> Result<Option<Config>, String> {
    let mut cfg = Config::default();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sessions" => cfg.sessions = parse_usize(&mut args, "--sessions")?,
            "--events-per-session" => {
                cfg.events_per_session = parse_usize(&mut args, "--events-per-session")?;
            }
            "--shards" => {
                let raw = parse_usize(&mut args, "--shards")?;
                cfg.shards =
                    u16::try_from(raw).map_err(|_| "--shards must fit in u16".to_string())?;
            }
            "--commands-per-session" => {
                cfg.commands_per_session = parse_usize(&mut args, "--commands-per-session")?;
            }
            "--writer-pool" => cfg.writer_pool = parse_usize(&mut args, "--writer-pool")?,
            "--attribution-tasks" => {
                cfg.attribution_tasks = parse_usize(&mut args, "--attribution-tasks")?;
            }
            "--archive-root" => {
                cfg.archive_root = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| "--archive-root needs a path".to_string())?,
                ));
            }
            "--help" | "-h" => return Ok(None),
            _ => return Err(format!("unknown option {arg:?}\n\n{USAGE}")),
        }
    }
    Ok(Some(cfg))
}

struct ScratchArchive(PathBuf, bool);

impl ScratchArchive {
    fn new(configured: Option<PathBuf>) -> Result<Self, String> {
        let keep = configured.is_some();
        let path = configured.unwrap_or_else(|| {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            env::temp_dir().join(format!(
                "spawn-control-plane-bench-{}-{nanos:x}",
                std::process::id()
            ))
        });
        fs::create_dir_all(&path)
            .map_err(|e| format!("create archive dir {}: {e}", path.display()))?;
        Ok(Self(path, keep))
    }
}

impl Drop for ScratchArchive {
    fn drop(&mut self) {
        if !self.1 {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

fn u64_to_f64(value: u64) -> f64 {
    let hi = u32::try_from(value >> 32).unwrap_or(u32::MAX);
    let lo = u32::try_from(value & 0xffff_ffff).unwrap_or(u32::MAX);
    f64::from(hi) * 4_294_967_296.0 + f64::from(lo)
}

fn percentile(sorted: &[f64], pct: u32) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let len = u128::try_from(sorted.len()).unwrap_or(1);
    let rank = (len * u128::from(pct)).div_ceil(100);
    let idx = usize::try_from(rank.saturating_sub(1).min(len - 1)).unwrap_or(0);
    sorted[idx]
}

fn latency_summary(values: &[f64]) -> Value {
    if values.is_empty() {
        return json!({"n": 0, "min_ms": 0.0, "p50_ms": 0.0, "p95_ms": 0.0, "p99_ms": 0.0, "max_ms": 0.0, "mean_ms": 0.0});
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let sum: f64 = sorted.iter().sum();
    let count = u64_to_f64(u64::try_from(sorted.len()).unwrap_or(u64::MAX));
    json!({
        "n": sorted.len(),
        "min_ms": sorted[0],
        "p50_ms": percentile(&sorted, 50),
        "p95_ms": percentile(&sorted, 95),
        "p99_ms": percentile(&sorted, 99),
        "max_ms": sorted[sorted.len() - 1],
        "mean_ms": sum / count,
    })
}

fn is_deadlock_err(err: &impl std::fmt::Display) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("deadlock") || msg.contains("40p01")
}

fn drain_initial_events(store: &PostgresStore) -> Result<GlobalPosition, String> {
    let mut cursor = GlobalPosition(0);
    loop {
        let batch = store
            .read_global(cursor, 1000)
            .map_err(|e| format!("initial read_global: {e}"))?;
        if let Some(last) = batch.last() {
            cursor = last.position;
        } else {
            return Ok(cursor);
        }
    }
}

struct EventCounters {
    next_idx: AtomicUsize,
    writers_done: AtomicBool,
    write_errors: AtomicUsize,
    read_errors: AtomicUsize,
    deadlocks: AtomicUsize,
}

struct ReadSample {
    position: GlobalPosition,
    session_id: SessionId,
    session_seq: spawn_core::event::SessionSeq,
}

fn run_event_reader_loop(
    reader: &PostgresStore,
    initial_cursor: GlobalPosition,
    counters: &EventCounters,
) -> (Vec<f64>, Vec<ReadSample>) {
    let mut cursor = initial_cursor;
    let mut latencies = Vec::new();
    let mut samples = Vec::new();
    loop {
        let done = counters.writers_done.load(Ordering::Acquire);
        let t0 = Instant::now();
        match reader.read_global(cursor, 64) {
            Ok(batch) => {
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                if !batch.is_empty() {
                    latencies.push(ms);
                    for rec in batch {
                        cursor = rec.position;
                        samples.push(ReadSample {
                            position: rec.position,
                            session_id: rec.event.session_id,
                            session_seq: rec.session_seq,
                        });
                    }
                } else if done {
                    while let Ok(final_batch) = reader.read_global(cursor, 256) {
                        if final_batch.is_empty() {
                            break;
                        }
                        for rec in final_batch {
                            cursor = rec.position;
                            samples.push(ReadSample {
                                position: rec.position,
                                session_id: rec.event.session_id,
                                session_seq: rec.session_seq,
                            });
                        }
                    }
                    break;
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            Err(err) => {
                if is_deadlock_err(&err) {
                    counters.deadlocks.fetch_add(1, Ordering::Relaxed);
                }
                counters.read_errors.fetch_add(1, Ordering::Relaxed);
                if done {
                    break;
                }
            }
        }
    }
    (latencies, samples)
}

fn run_event_writer_loop(
    wstore: &PostgresStore,
    sessions: usize,
    total_events: usize,
    counters: &EventCounters,
) -> (Vec<f64>, BTreeSet<GlobalPosition>) {
    let mut lats = Vec::new();
    let mut pos_set = BTreeSet::new();
    loop {
        let idx = counters.next_idx.fetch_add(1, Ordering::Relaxed);
        if idx >= total_events {
            break;
        }
        let sess_idx = idx % sessions;
        let seq_idx = idx / sessions;
        let agent = AgentId::new(format!("agent_ev_{sess_idx:03}")).expect("valid agent id");
        let session =
            SessionId::new(format!("session_ev_{sess_idx:03}")).expect("valid session id");
        let kind = if seq_idx.is_multiple_of(2) {
            EventKind::MessageReceived
        } else {
            EventKind::ToolCompleted {
                tool_use_id: format!("tool_{idx}"),
                duration_ms: Some(12),
            }
        };
        let t0 = Instant::now();
        match wstore.append(NewEvent {
            agent_id: agent,
            session_id: session,
            execution_id: None,
            source: Source::Bridge,
            source_event_id: Some(format!("bench-ev:{idx}")),
            kind,
        }) {
            Ok(appended) => {
                lats.push(t0.elapsed().as_secs_f64() * 1000.0);
                pos_set.insert(appended.record.position);
            }
            Err(err) => {
                if is_deadlock_err(&err) {
                    counters.deadlocks.fetch_add(1, Ordering::Relaxed);
                }
                counters.write_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    (lats, pos_set)
}

struct TempFilesGuard(Vec<PathBuf>);

impl Drop for TempFilesGuard {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = fs::remove_file(p);
        }
    }
}

fn spawn_xid8_t1_psql(pg_envs: &[(String, String)]) -> Result<std::process::Child, String> {
    let mut cmd = Command::new("psql");
    cmd.args(["-X", "-A", "-t", "-q", "-v", "ON_ERROR_STOP=1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (k, v) in pg_envs {
        cmd.env(k, v);
    }
    cmd.spawn()
        .map_err(|e| format!("spawn psql for xid8 inversion: {e}"))
}

fn wait_for_t1_reserved_id(
    child: &mut std::process::Child,
    id_file: &Path,
    ready_file: &Path,
) -> Result<i64, String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            let mut err_buf = String::new();
            if let Some(mut se) = child.stderr.take() {
                let _ = std::io::Read::read_to_string(&mut se, &mut err_buf);
            }
            return Err(format!(
                "psql xid8 T1 exited unexpectedly ({status}): {}",
                err_buf.trim()
            ));
        }
        if fs::read_to_string(ready_file).is_ok_and(|s| !s.trim().is_empty()) {
            let raw_id = fs::read_to_string(id_file)
                .map_err(|e| format!("read xid8 id file {}: {e}", id_file.display()))?;
            return raw_id
                .trim()
                .parse::<i64>()
                .map_err(|e| format!("parse reserved xid8 id {raw_id:?}: {e}"));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("timed out waiting for psql T1 nextval output".to_string());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn finish_t1_xid8_commit(
    mut t1: std::process::Child,
    mut t1_in: std::process::ChildStdin,
    id_t1: i64,
    round: usize,
) -> Result<(), String> {
    let phase2 = format!(
        "SELECT pg_advisory_xact_lock(1399957401, hashtext('session_xid8_a_{round}'));\n\
         INSERT INTO spawn_agent_sessions (agent_id, session_id) \
         VALUES ('agent_xid8_a_{round}', 'session_xid8_a_{round}') ON CONFLICT DO NOTHING;\n\
         INSERT INTO spawn_events (id, agent_id, session_id, execution_id, source, source_event_id, \
         kind, session_seq, recorded_at_ms, commit_xid) \
         VALUES ({id_t1}, 'agent_xid8_a_{round}', 'session_xid8_a_{round}', NULL, 'observer', \
         'xid8-a:{round}', '{{\"name\":\"message.received\"}}'::jsonb, 1, spawn_now_ms(), pg_current_xact_id());\n\
         SELECT spawn_session_fold('session_xid8_a_{round}');\n\
         COMMIT;\n\
         \\q\n"
    );
    if let Err(err) = t1_in
        .write_all(phase2.as_bytes())
        .and_then(|()| t1_in.flush())
    {
        let _ = t1.kill();
        let _ = t1.wait();
        return Err(format!("write xid8 T1 phase2 SQL: {err}"));
    }
    drop(t1_in);

    let out = t1
        .wait_with_output()
        .map_err(|e| format!("wait xid8 T1 psql: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "xid8 T1 commit failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn run_single_xid8_inversion(
    pg_envs: &[(String, String)],
    schema: &str,
    store: &PostgresStore,
    cursor: &mut GlobalPosition,
    round: usize,
) -> Result<(), String> {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let base = env::temp_dir();
    let id_file = base.join(format!("spawn_xid8_id_{pid}_{round}_{nanos:x}.txt"));
    let ready_file = base.join(format!("spawn_xid8_ready_{pid}_{round}_{nanos:x}.txt"));
    let _cleanup = TempFilesGuard(vec![id_file.clone(), ready_file.clone()]);

    let mut t1 = spawn_xid8_t1_psql(pg_envs)?;
    let mut t1_in = t1
        .stdin
        .take()
        .ok_or_else(|| "missing stdin pipe for xid8 T1 psql".to_string())?;
    let phase1 = format!(
        "SET search_path TO {schema};\n\
         BEGIN;\n\
         SELECT nextval(pg_get_serial_sequence('spawn_events', 'id')) \\g {}\n\
         SELECT 1 \\g {}\n",
        id_file.display(),
        ready_file.display(),
    );
    t1_in
        .write_all(phase1.as_bytes())
        .and_then(|()| t1_in.flush())
        .map_err(|e| format!("write xid8 T1 phase1 SQL: {e}"))?;

    let id_t1 = wait_for_t1_reserved_id(&mut t1, &id_file, &ready_file)?;
    let pos_t1 = GlobalPosition(
        u64::try_from(id_t1).map_err(|_| format!("invalid negative id_t1={id_t1}"))?,
    );

    let t2 = store
        .append(NewEvent {
            agent_id: AgentId::new(format!("agent_xid8_b_{round}")).map_err(|e| e.to_string())?,
            session_id: SessionId::new(format!("session_xid8_b_{round}"))
                .map_err(|e| e.to_string())?,
            execution_id: None,
            source: Source::Observer,
            source_event_id: Some(format!("xid8-b:{round}")),
            kind: EventKind::MessageReceived,
        })
        .map_err(|e| format!("append xid8 T2 event: {e}"))?;
    let pos_t2 = t2.record.position;
    if pos_t2 <= pos_t1 {
        return Err(format!("expected T2 id {pos_t2:?} > T1 id {pos_t1:?}"));
    }

    let g1 = store
        .read_global(*cursor, 100)
        .map_err(|e| format!("read_global after T2 commit: {e}"))?;
    let g1_pos: Vec<GlobalPosition> = g1.iter().map(|r| r.position).collect();
    if g1_pos != [pos_t2] {
        return Err(format!(
            "xid8 inversion round {round}: expected [{pos_t2:?}] after T2, got {g1_pos:?}"
        ));
    }
    *cursor = pos_t2;

    finish_t1_xid8_commit(t1, t1_in, id_t1, round)?;

    let g2 = store
        .read_global(*cursor, 100)
        .map_err(|e| format!("read_global after T1 commit: {e}"))?;
    let g2_pos: Vec<GlobalPosition> = g2.iter().map(|r| r.position).collect();
    if g2_pos != [pos_t1] {
        return Err(format!(
            "xid8 inversion round {round}: failed to recover backward id {pos_t1:?} after cursor {pos_t2:?}, got {g2_pos:?}"
        ));
    }
    *cursor = pos_t1;

    let g3 = store
        .read_global(*cursor, 100)
        .map_err(|e| format!("read_global post-inversion drain: {e}"))?;
    if !g3.is_empty() {
        return Err(format!(
            "xid8 inversion round {round}: expected 0 duplicate trailing rows, got {}",
            g3.len()
        ));
    }
    Ok(())
}

fn verify_deterministic_xid8_inversions(
    bench_url: &str,
    schema: &str,
    store: &PostgresStore,
    start_cursor: GlobalPosition,
    rounds: usize,
) -> Result<usize, String> {
    if schema.is_empty()
        || !schema
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(format!("invalid benchmark schema {schema:?}"));
    }
    let pg_envs = parse_pg_env(bench_url)?;
    let mut cursor = start_cursor;
    for round in 0..rounds {
        run_single_xid8_inversion(&pg_envs, schema, store, &mut cursor, round)?;
    }
    Ok(rounds)
}

struct EventReadTally {
    out_of_order_reads: usize,
    commit_id_inversions: usize,
    duplicate_reads: usize,
    missing_reads: usize,
    unexpected_reads: usize,
}

fn tally_event_log_reads(
    read_positions: &[ReadSample],
    written_positions: &BTreeSet<GlobalPosition>,
) -> EventReadTally {
    let mut out_of_order_reads = 0_usize;
    let mut commit_id_inversions = 0_usize;
    let mut duplicate_reads = 0_usize;
    let mut seen_read = BTreeSet::new();
    let mut prev_pos: Option<GlobalPosition> = None;
    let mut per_session_last: BTreeMap<SessionId, (GlobalPosition, spawn_core::event::SessionSeq)> =
        BTreeMap::new();
    for sample in read_positions {
        if let Some(p) = prev_pos
            && sample.position <= p
        {
            commit_id_inversions += 1;
        }
        prev_pos = Some(sample.position);
        if let Some((last_pos, last_seq)) = per_session_last.get(&sample.session_id)
            && (sample.position <= *last_pos
                || sample.session_seq <= *last_seq
                || sample.session_seq.0 != last_seq.0 + 1)
        {
            out_of_order_reads += 1;
        }
        let _ = per_session_last.insert(
            sample.session_id.clone(),
            (sample.position, sample.session_seq),
        );
        if !seen_read.insert(sample.position) {
            duplicate_reads += 1;
        }
    }
    EventReadTally {
        out_of_order_reads,
        commit_id_inversions,
        duplicate_reads,
        missing_reads: written_positions.difference(&seen_read).count(),
        unexpected_reads: seen_read.difference(written_positions).count(),
    }
}

fn run_event_log_stress(
    base_store: &PostgresStore,
    bench_url: &str,
    schema: &str,
    cfg: &Config,
) -> Result<Value, String> {
    let initial_cursor = drain_initial_events(base_store)?;
    let total_events = cfg.sessions * cfg.events_per_session;
    let counters = EventCounters {
        next_idx: AtomicUsize::new(0),
        writers_done: AtomicBool::new(false),
        write_errors: AtomicUsize::new(0),
        read_errors: AtomicUsize::new(0),
        deadlocks: AtomicUsize::new(0),
    };

    let mut writers = Vec::with_capacity(cfg.writer_pool);
    for _ in 0..cfg.writer_pool {
        writers.push(
            base_store
                .new_connection()
                .map_err(|e| format!("writer new_connection: {e}"))?,
        );
    }
    let reader = base_store
        .new_connection()
        .map_err(|e| format!("reader new_connection: {e}"))?;

    let wall_t0 = Instant::now();
    let (write_lat_all, written_positions, read_lat_all, read_positions) =
        std::thread::scope(|scope| {
            let counters_ref = &counters;
            let reader_handle =
                scope.spawn(|| run_event_reader_loop(&reader, initial_cursor, counters_ref));

            let mut writer_handles = Vec::with_capacity(writers.len());
            for wstore in &writers {
                writer_handles.push(scope.spawn(move || {
                    run_event_writer_loop(wstore, cfg.sessions, total_events, counters_ref)
                }));
            }

            let mut all_write_lats = Vec::with_capacity(total_events);
            let mut all_written = BTreeSet::new();
            for h in writer_handles {
                let (lats, pos_set) = h.join().expect("writer thread");
                all_write_lats.extend(lats);
                all_written.extend(pos_set);
            }
            counters.writers_done.store(true, Ordering::Release);
            let (read_lats, read_pos) = reader_handle.join().expect("reader thread");
            (all_write_lats, all_written, read_lats, read_pos)
        });
    let wall_ms = wall_t0.elapsed().as_secs_f64() * 1000.0;
    let tally = tally_event_log_reads(&read_positions, &written_positions);
    let end_cursor = read_positions.last().map_or(initial_cursor, |s| s.position);
    let deterministic_inversions =
        verify_deterministic_xid8_inversions(bench_url, schema, base_store, end_cursor, 4)?;

    Ok(json!({
        "writer_pool": cfg.writer_pool,
        "sessions": cfg.sessions,
        "events_target": total_events,
        "events_written": written_positions.len(),
        "events_read": read_positions.len(),
        "out_of_order_reads": tally.out_of_order_reads,
        "concurrent_commit_id_inversions": tally.commit_id_inversions,
        "deterministic_commit_id_inversions": deterministic_inversions,
        "commit_id_inversions": tally.commit_id_inversions + deterministic_inversions,
        "duplicate_reads": tally.duplicate_reads,
        "missing_reads": tally.missing_reads,
        "unexpected_reads": tally.unexpected_reads,
        "deadlocks": counters.deadlocks.load(Ordering::Relaxed),
        "write_errors": counters.write_errors.load(Ordering::Relaxed),
        "read_errors": counters.read_errors.load(Ordering::Relaxed),
        "wall_ms": wall_ms,
        "events_per_sec": u64_to_f64(u64::try_from(written_positions.len()).unwrap_or(0)) / (wall_ms / 1000.0).max(1e-6),
        "append_latency": latency_summary(&write_lat_all),
        "read_global_latency": latency_summary(&read_lat_all),
    }))
}

struct ShardCounters {
    submit_idx: AtomicUsize,
    submissions_done: AtomicBool,
    finished_total: AtomicUsize,
    submit_errors: AtomicUsize,
    claim_errors: AtomicUsize,
    finish_errors: AtomicUsize,
    wrong_shard_claims: AtomicUsize,
    deadlocks: AtomicUsize,
}

fn run_command_submitter_loop(
    sub_store: &PostgresStore,
    sessions: usize,
    total_cmds: usize,
    principal: &Principal,
    counters: &ShardCounters,
) {
    loop {
        let idx = counters.submit_idx.fetch_add(1, Ordering::Relaxed);
        if idx >= total_cmds {
            break;
        }
        let sess_idx = idx % sessions;
        let submission = Submission {
            agent_id: AgentId::new(format!("agent_cmd_{sess_idx:03}")).expect("valid agent"),
            session_id: SessionId::new(format!("session_cmd_{sess_idx:03}"))
                .expect("valid session"),
            idempotency_key: IdempotencyKey::new(format!("bench-cmd-key:{idx}"))
                .expect("valid key"),
            action: CommandAction::Input {
                text: format!("bench command payload {idx}"),
                tags: Vec::new(),
            },
        };
        if let Err(err) = sub_store.submit(principal, &submission) {
            if is_deadlock_err(&err) {
                counters.deadlocks.fetch_add(1, Ordering::Relaxed);
            }
            counters.submit_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn run_shard_claimer_loop(
    shard_idx: u16,
    partition: ShardPartition,
    worker_name: &str,
    cstore: &PostgresStore,
    total_cmds: usize,
    deadline: Instant,
    counters: &ShardCounters,
) -> (u16, Vec<CommandId>, Vec<f64>, Vec<f64>) {
    let shard = BridgeShard::Partition(partition);
    let mut claim_lats = Vec::new();
    let mut fin_lats = Vec::new();
    let mut ids = Vec::new();
    let mut empty_after_done = 0_usize;
    loop {
        if counters.finished_total.load(Ordering::Acquire) >= total_cmds
            || Instant::now() >= deadline
        {
            break;
        }
        let t0 = Instant::now();
        match cstore.claim_for_shard(worker_name, Duration::from_secs(30), shard) {
            Ok(Some(claimed)) => {
                claim_lats.push(t0.elapsed().as_secs_f64() * 1000.0);
                empty_after_done = 0;
                if !shard.owns_session(&claimed.session_id) {
                    counters.wrong_shard_claims.fetch_add(1, Ordering::Relaxed);
                }
                ids.push(claimed.command_id);
                let tf = Instant::now();
                match cstore.finish(&claimed, &Outcome::Succeeded) {
                    Ok(()) => {
                        fin_lats.push(tf.elapsed().as_secs_f64() * 1000.0);
                        counters.finished_total.fetch_add(1, Ordering::AcqRel);
                    }
                    Err(err) => {
                        if is_deadlock_err(&err) {
                            counters.deadlocks.fetch_add(1, Ordering::Relaxed);
                        }
                        counters.finish_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Ok(None) => {
                if counters.submissions_done.load(Ordering::Acquire) {
                    empty_after_done += 1;
                    if empty_after_done >= 5 {
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(err) => {
                if is_deadlock_err(&err) {
                    counters.deadlocks.fetch_add(1, Ordering::Relaxed);
                }
                counters.claim_errors.fetch_add(1, Ordering::Relaxed);
                if Instant::now() >= deadline {
                    break;
                }
            }
        }
    }
    (shard_idx, ids, claim_lats, fin_lats)
}

struct ShardWorker {
    idx: u16,
    partition: ShardPartition,
    name: String,
    store: PostgresStore,
}

fn prepare_shard_connections(
    base_store: &PostgresStore,
    cfg: &Config,
) -> Result<(Vec<PostgresStore>, Vec<ShardWorker>), String> {
    let submit_threads = cfg.writer_pool.clamp(1, 4);
    let mut submitters = Vec::with_capacity(submit_threads);
    for _ in 0..submit_threads {
        submitters.push(
            base_store
                .new_connection()
                .map_err(|e| format!("submitter new_connection: {e}"))?,
        );
    }

    let mut shard_workers = Vec::with_capacity(usize::from(cfg.shards));
    for idx in 0..cfg.shards {
        let partition =
            ShardPartition::new(idx, cfg.shards).map_err(|e| format!("shard init: {e}"))?;
        let store = base_store
            .new_connection()
            .map_err(|e| format!("shard new_connection: {e}"))?;
        let name = format!("always-on-shard-{idx}");
        let _ownership = store
            .recover_worker(&name)
            .map_err(|e| format!("recover shard {name}: {e}"))?
            .0;
        shard_workers.push(ShardWorker {
            idx,
            partition,
            name,
            store,
        });
    }
    Ok((submitters, shard_workers))
}

fn run_shard_claim_stress(base_store: &PostgresStore, cfg: &Config) -> Result<Value, String> {
    let total_cmds = cfg.sessions * cfg.commands_per_session;
    let counters = ShardCounters {
        submit_idx: AtomicUsize::new(0),
        submissions_done: AtomicBool::new(false),
        finished_total: AtomicUsize::new(0),
        submit_errors: AtomicUsize::new(0),
        claim_errors: AtomicUsize::new(0),
        finish_errors: AtomicUsize::new(0),
        wrong_shard_claims: AtomicUsize::new(0),
        deadlocks: AtomicUsize::new(0),
    };

    let (submitters, shard_workers) = prepare_shard_connections(base_store, cfg)?;
    let principal = Principal::new("always-on-bench").map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_mins(1);
    let wall_t0 = Instant::now();

    let (claimed_all, claim_lats, finish_lats, per_shard_counts) = std::thread::scope(|scope| {
        let counters_ref = &counters;
        let principal_ref = &principal;
        let mut sub_handles = Vec::with_capacity(submitters.len());
        for sub_store in &submitters {
            sub_handles.push(scope.spawn(move || {
                run_command_submitter_loop(
                    sub_store,
                    cfg.sessions,
                    total_cmds,
                    principal_ref,
                    counters_ref,
                );
            }));
        }

        let mut claim_handles = Vec::with_capacity(shard_workers.len());
        for worker in &shard_workers {
            let idx_val = worker.idx;
            let part_val = worker.partition;
            claim_handles.push(scope.spawn(move || {
                run_shard_claimer_loop(
                    idx_val,
                    part_val,
                    &worker.name,
                    &worker.store,
                    total_cmds,
                    deadline,
                    counters_ref,
                )
            }));
        }

        for h in sub_handles {
            h.join().expect("submitter thread");
        }
        counters.submissions_done.store(true, Ordering::Release);

        let mut all_ids = Vec::with_capacity(total_cmds);
        let mut all_claim_lats = Vec::with_capacity(total_cmds);
        let mut all_fin_lats = Vec::with_capacity(total_cmds);
        let mut by_shard: BTreeMap<String, usize> = BTreeMap::new();
        for h in claim_handles {
            let (shard_idx, ids, clats, flats) = h.join().expect("claimer thread");
            by_shard.insert(format!("shard_{shard_idx}"), ids.len());
            all_ids.extend(ids);
            all_claim_lats.extend(clats);
            all_fin_lats.extend(flats);
        }
        (all_ids, all_claim_lats, all_fin_lats, by_shard)
    });
    let wall_ms = wall_t0.elapsed().as_secs_f64() * 1000.0;

    let mut unique_claimed = BTreeSet::new();
    let mut duplicate_claims = 0_usize;
    for id in &claimed_all {
        if !unique_claimed.insert(*id) {
            duplicate_claims += 1;
        }
    }
    let unclaimed_commands = total_cmds.saturating_sub(unique_claimed.len());

    Ok(json!({
        "shards": cfg.shards,
        "sessions": cfg.sessions,
        "commands_target": total_cmds,
        "commands_claimed_unique": unique_claimed.len(),
        "duplicate_claims": duplicate_claims,
        "unclaimed_commands": unclaimed_commands,
        "wrong_shard_claims": counters.wrong_shard_claims.load(Ordering::Relaxed),
        "deadlocks": counters.deadlocks.load(Ordering::Relaxed),
        "submit_errors": counters.submit_errors.load(Ordering::Relaxed),
        "claim_errors": counters.claim_errors.load(Ordering::Relaxed),
        "finish_errors": counters.finish_errors.load(Ordering::Relaxed),
        "per_shard_counts": per_shard_counts,
        "wall_ms": wall_ms,
        "claims_per_sec": u64_to_f64(u64::try_from(unique_claimed.len()).unwrap_or(0)) / (wall_ms / 1000.0).max(1e-6),
        "claim_latency": latency_summary(&claim_lats),
        "finish_latency": latency_summary(&finish_lats),
    }))
}

#[derive(Default)]
struct AttributionGaps {
    missing_agent_id: usize,
    missing_execution_id: usize,
    missing_task_id: usize,
    missing_model_label: usize,
    missing_transcript_ref: usize,
    missing_duration_ms: usize,
    missing_cost_or_tokens: usize,
    missing_landing_record: usize,
    tasks_fully_attributed: usize,
}

impl AttributionGaps {
    fn total_field_gaps(&self) -> usize {
        self.missing_agent_id
            + self.missing_execution_id
            + self.missing_task_id
            + self.missing_model_label
            + self.missing_transcript_ref
            + self.missing_duration_ms
            + self.missing_cost_or_tokens
            + self.missing_landing_record
    }
}

/// Built-in first-party model pricing table matching `crates/spawn-cli/src/cost.rs`.
fn built_in_model_prices() -> Value {
    json!({
        "claude-fable-5-2": {"input": 8.0, "output": 40.0, "cache_write": 10.0, "cache_read": 0.2},
        "claude-opus-5-5": {"input": 4.0, "output": 20.0, "cache_write": 5.0, "cache_read": 0.2},
        "claude-sonnet-5-5": {"input": 2.0, "output": 10.0, "cache_write": 2.5, "cache_read": 0.1},
        "claude-opus-4-8": {"input": 5.0, "output": 25.0, "cache_write": 6.25, "cache_read": 0.5},
        "claude-opus-4-7": {"input": 5.0, "output": 25.0, "cache_write": 6.25, "cache_read": 0.5},
        "claude-opus-4-6": {"input": 5.0, "output": 25.0, "cache_write": 6.25, "cache_read": 0.5},
        "claude-sonnet-4-6": {"input": 3.0, "output": 15.0, "cache_write": 3.75, "cache_read": 0.3},
        "claude-sonnet-4-5": {"input": 3.0, "output": 15.0, "cache_write": 3.75, "cache_read": 0.3},
        "claude-haiku-4-5": {"input": 1.0, "output": 5.0, "cache_write": 1.25, "cache_read": 0.1},
    })
}

/// Convert Unix epoch milliseconds to RFC3339 / ISO-8601 UTC (`YYYY-MM-DDTHH:MM:SS.mmmZ`).
fn format_utc_iso_ms(epoch_ms: i64) -> String {
    let clamped = epoch_ms.max(0);
    let secs = clamped / 1_000;
    let millis = clamped % 1_000;
    let days = secs / 86_400;
    let rem_secs = secs % 86_400;
    let hour = rem_secs / 3_600;
    let minute = (rem_secs % 3_600) / 60;
    let second = rem_secs % 60;

    // Howard Hinnant civil-from-days algorithm.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

struct BridgeStartedExecution {
    execution_id: ExecutionId,
    native_sid: String,
    transcript_path: String,
    terminal: TerminalName,
    exit_file: String,
}

struct StartedEventScan {
    started: bool,
    native: Option<(String, String)>,
}

fn find_started_execution(
    store: &PostgresStore,
    cursor: &mut GlobalPosition,
    session_id: &SessionId,
    execution_id: &ExecutionId,
) -> Result<StartedEventScan, String> {
    let mut started = false;
    let mut native = None;
    loop {
        let batch = store
            .read_global(*cursor, 200)
            .map_err(|e| format!("read_global bridge start: {e}"))?;
        if batch.is_empty() {
            break;
        }
        for rec in &batch {
            *cursor = rec.position;
            if &rec.event.session_id != session_id
                || rec.event.execution_id.as_ref() != Some(execution_id)
            {
                continue;
            }
            match &rec.event.kind {
                EventKind::ExecutionStarted { .. } => started = true,
                EventKind::NativeSessionStarted {
                    native_session_id,
                    transcript_path: Some(path),
                    ..
                } => native = Some((native_session_id.clone(), path.clone())),
                _ => {}
            }
        }
        if batch.len() < 200 {
            break;
        }
    }
    Ok(StartedEventScan { started, native })
}

fn drive_bridge_until_started(
    bridge: &mut Bridge,
    store: &PostgresStore,
    mem_host: &MemoryStore,
    session_id: &SessionId,
    execution_id: &ExecutionId,
    scan_cursor: &mut GlobalPosition,
) -> Result<BridgeStartedExecution, String> {
    let mut saw_started = false;
    let mut native_pair: Option<(String, String)> = None;
    for _ in 0..500 {
        let _ = bridge
            .work()
            .map_err(|e| format!("bridge.work start: {e}"))?;
        let _ = bridge
            .poll()
            .map_err(|e| format!("bridge.poll start: {e}"))?;
        let scan = find_started_execution(store, scan_cursor, session_id, execution_id)?;
        if scan.started {
            saw_started = true;
        }
        if scan.native.is_some() {
            native_pair = scan.native;
        }
        if saw_started
            && let Some((native_sid, transcript_path)) = &native_pair
            && let Some(launch) = mem_host.launches().into_iter().rev().find(|l| {
                l.environment
                    .iter()
                    .any(|(k, v)| k == "SPAWN_EXECUTION_ID" && v == execution_id.as_str())
            })
            && let Some(exit_file) = launch
                .environment
                .iter()
                .find(|(k, _)| k == "SPAWN_EXIT_FILE")
                .map(|(_, v)| v.clone())
        {
            return Ok(BridgeStartedExecution {
                execution_id: execution_id.clone(),
                native_sid: native_sid.clone(),
                transcript_path: transcript_path.clone(),
                terminal: launch.terminal,
                exit_file,
            });
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Err(format!(
        "bridge did not start execution {execution_id} for session {session_id}"
    ))
}

struct AttrTaskSpec<'a> {
    task_id: i64,
    idx: usize,
    model_profile: ModelProfile,
    model_name: &'a str,
}

fn drive_bridge_execution(
    bridge: &mut Bridge,
    store: &PostgresStore,
    mem_host: &MemoryStore,
    archive: &LocalTranscriptArchive,
    spec: &AttrTaskSpec<'_>,
) -> Result<SessionId, String> {
    let task_id = spec.task_id;
    let idx = spec.idx;
    let model_name = spec.model_name;
    let worker = format!("attr{idx:03}");
    let agent_id =
        AgentId::new(format!("agent_worker_{worker}_{task_id}")).map_err(|e| e.to_string())?;
    let session_id =
        SessionId::new(format!("session_worker_{worker}_{task_id}")).map_err(|e| e.to_string())?;
    let principal = Principal::new("always-on-attr").map_err(|e| e.to_string())?;
    let mut scan_cursor = drain_initial_events(store)?;

    let cmd_id = store
        .submit(
            &principal,
            &Submission {
                agent_id: agent_id.clone(),
                session_id: session_id.clone(),
                idempotency_key: IdempotencyKey::new(format!("attr-start:{task_id}:{idx}"))
                    .map_err(|e| e.to_string())?,
                action: CommandAction::Start {
                    engine: ENGINE.to_string(),
                    prompt: format!("Work task {task_id} on {model_name}"),
                    parent_session_id: None,
                    from: None,
                    model: Some(spec.model_profile),
                    charter: None,
                    model_label: None,
                },
            },
        )
        .map_err(|e| format!("submit attribution start: {e}"))?;
    let exec_id = ExecutionId::for_command(cmd_id);
    mem_host.set_pane(&TerminalName::for_execution(&exec_id), "❯ ");

    let started = drive_bridge_until_started(
        bridge,
        store,
        mem_host,
        &session_id,
        &exec_id,
        &mut scan_cursor,
    )?;

    let pg_now_ms = store
        .database_now_ms()
        .map_err(|e| format!("database_now_ms for transcript: {e}"))?;
    let ts_iso = format_utc_iso_ms(pg_now_ms);
    let transcript_lines = format!(
        "{{\"type\":\"assistant\",\"uuid\":\"u-tool-{idx}\",\"timestamp\":\"{ts_iso}\",\"message\":{{\"id\":\"msg-t-{idx}-{task_id}\",\"model\":\"{model_name}\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"tu_{idx}_{task_id}\",\"name\":\"Edit\",\"input\":{{\"file_path\":\"src/task_{idx}.rs\"}}}}]}}}}\n\
         {{\"type\":\"assistant\",\"uuid\":\"u-msg-{idx}\",\"timestamp\":\"{ts_iso}\",\"message\":{{\"id\":\"msg-u-{idx}-{task_id}\",\"model\":\"{model_name}\",\"content\":[{{\"type\":\"text\",\"text\":\"implemented task {task_id}\"}}],\"usage\":{{\"input_tokens\":4000,\"cache_creation_input_tokens\":2000,\"cache_read_input_tokens\":8000,\"output_tokens\":1200}}}}}}\n\
         {{\"type\":\"system\",\"subtype\":\"turn_duration\",\"uuid\":\"u-turn-{idx}\"}}\n"
    );
    mem_host.append_file(&started.transcript_path, &transcript_lines);
    let _ = bridge
        .work()
        .map_err(|e| format!("bridge.work transcript: {e}"))?;
    let _ = bridge
        .poll()
        .map_err(|e| format!("bridge.poll transcript: {e}"))?;

    let copies = archive
        .copies(&agent_id, &started.native_sid)
        .map_err(|e| format!("archive.copies: {e}"))?;
    if copies.is_empty() {
        return Err(format!(
            "Bridge did not archive transcript for {} ({})",
            agent_id, started.native_sid
        ));
    }

    mem_host.exit_on_next_check(&started.terminal, &started.exit_file, "0\n");
    for _ in 0..50 {
        let _ = bridge.work().map_err(|e| format!("bridge.work end: {e}"))?;
        let _ = bridge.poll().map_err(|e| format!("bridge.poll end: {e}"))?;
        let execs = store
            .executions(&session_id)
            .map_err(|e| format!("executions poll: {e}"))?;
        if execs
            .iter()
            .any(|e| e.execution_id == started.execution_id && e.ended_at.is_some())
        {
            return Ok(session_id);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Err(format!(
        "execution {} in session {session_id} did not end cleanly",
        started.execution_id
    ))
}

fn find_native_sid_for_execution(
    store: &PostgresStore,
    execution_id: &str,
) -> Result<Option<String>, String> {
    let mut cursor = GlobalPosition(0);
    let mut found = None;
    loop {
        let batch = store
            .read_global(cursor, 500)
            .map_err(|e| format!("read_global native sid: {e}"))?;
        if batch.is_empty() {
            break;
        }
        for rec in &batch {
            cursor = rec.position;
            if rec.event.execution_id.as_ref().map(ExecutionId::as_str) == Some(execution_id)
                && let EventKind::NativeSessionStarted {
                    native_session_id, ..
                } = &rec.event.kind
            {
                found = Some(native_session_id.clone());
            }
        }
        if batch.len() < 500 {
            break;
        }
    }
    Ok(found)
}

fn read_archived_transcript_bytes(
    archive: &LocalTranscriptArchive,
    agent_id: &str,
    native_sid: &str,
) -> Result<Vec<u8>, String> {
    let agent = AgentId::new(agent_id).map_err(|e| e.to_string())?;
    let copies = archive
        .copies(&agent, native_sid)
        .map_err(|e| format!("archive.copies({agent_id}, {native_sid}): {e}"))?;
    let mut full = Vec::new();
    for (copy_idx, &len) in copies.iter().enumerate() {
        let chunk = archive
            .read(
                &agent,
                native_sid,
                copy_idx,
                0,
                usize::try_from(len).unwrap_or(usize::MAX),
            )
            .map_err(|e| format!("archive.read({agent_id}, {native_sid}, {copy_idx}): {e}"))?;
        full.extend_from_slice(&chunk);
    }
    Ok(full)
}

fn record_landed_provenance_from_system(
    store: &PostgresStore,
    archive: &LocalTranscriptArchive,
    task_id: i64,
    session_id: &SessionId,
    cost_execs_by_id: &BTreeMap<String, &Value>,
    idx: usize,
) -> Result<(), String> {
    let task_facts = store
        .prov_task_facts(&[task_id])
        .map_err(|e| format!("prov_task_facts: {e}"))?;
    let agent_id = task_facts
        .get(&task_id)
        .map(|f| f.agent_id.clone())
        .ok_or_else(|| format!("task {task_id} missing from mission_tasks"))?;
    let execs = store
        .executions(session_id)
        .map_err(|e| format!("executions({session_id}): {e}"))?;
    let exec_row = execs
        .iter()
        .find(|e| e.ended_at.is_some())
        .ok_or_else(|| format!("session {session_id} missing ended execution"))?;
    let execution_id = exec_row.execution_id.as_str().to_string();
    let native_sid = find_native_sid_for_execution(store, &execution_id)?
        .ok_or_else(|| format!("execution {execution_id} missing NativeSessionStarted in PG"))?;
    let archived_bytes = read_archived_transcript_bytes(archive, &agent_id, &native_sid)?;
    let archive_sha = sha256_of(&archived_bytes);
    let transcript_ref = format!(
        "archive/{agent_id}/{native_sid}.jsonl#sha256={}",
        archive_sha.hex()
    );
    let model_label = cost_execs_by_id
        .get(&execution_id)
        .and_then(|row| row["models"].as_array())
        .and_then(|models| models.first())
        .and_then(|m| m["model"].as_str())
        .map(str::to_string);

    store
        .task_delivered(task_id, i64::try_from(idx + 1).expect("fits i64"))
        .map_err(|e| format!("task_delivered: {e}"))?;
    let commit_hex = format!("{:040x}", idx + 1);
    let base_hex = format!("{:040x}", idx + 10_000);
    let _ = store
        .task_reported_with(task_id, "done", Some(&commit_hex))
        .map_err(|e| format!("task_reported_with: {e}"))?;
    let _ = store
        .integrator_record(&json!({
            "type": "task.gated",
            "id": task_id,
            "revision": 0,
            "verdict": "accepted",
            "base": base_hex,
            "candidate": commit_hex,
        }))
        .map_err(|e| format!("integrator_record gated: {e}"))?;
    let _ = store
        .integrator_record(&json!({
            "type": "task.landed",
            "id": task_id,
            "revision": 0,
            "from": base_hex,
            "to": commit_hex,
        }))
        .map_err(|e| format!("integrator_record landed: {e}"))?;
    let at_ms = store
        .database_now_ms()
        .map_err(|e| format!("database_now_ms provenance: {e}"))?;
    let _ = store
        .record_change(
            &ProvChangeInput {
                change_id: format!("chg-attr-{idx:03}"),
                rev: 1,
                kind: ProvChangeKind::Land,
                domain: "d6:bench".to_string(),
                agent_id,
                execution_id: Some(execution_id),
                task_id: Some(task_id),
                preset_id: None,
                model_label,
                transcript_ref: Some(transcript_ref),
                attributed: ProvAttributionSource::Host,
                base: base_hex,
                result: commit_hex,
                boundary: ProvBoundary::Land,
                at_ms,
            },
            &[ProvEntryInput {
                path: format!("src/task_{idx}.rs"),
                before: None,
                after: Some(archive_sha.to_string()),
                size: Some(i64::try_from(archived_bytes.len()).expect("fits i64")),
                class: ProvEntryClass::Content,
                origin: None,
            }],
            &[],
        )
        .map_err(|e| format!("record_change: {e}"))?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MatchState {
    Matched,
    Mismatched,
}

impl MatchState {
    fn from_bool(ok: bool) -> Self {
        if ok { Self::Matched } else { Self::Mismatched }
    }

    fn is_matched(self) -> bool {
        self == Self::Matched
    }
}

#[derive(Clone)]
struct ObservedLineage {
    task_id: Option<i64>,
    prov_task_id: Option<i64>,
    agent_matches_task: MatchState,
    agent_matches_execution: MatchState,
    has_execution_id: MatchState,
    model_matches_cost: MatchState,
    transcript_ref_valid: MatchState,
    duration_ms: Option<i64>,
    cost_usd: Option<f64>,
    unpriced_tokens: i64,
    has_landing_record: MatchState,
}

fn evaluate_observed_lineage(obs: &ObservedLineage, gaps: &mut AttributionGaps) -> Option<i64> {
    let mut ok = true;
    if !obs.agent_matches_task.is_matched() || !obs.agent_matches_execution.is_matched() {
        gaps.missing_agent_id += 1;
        ok = false;
    }
    if !obs.has_execution_id.is_matched() {
        gaps.missing_execution_id += 1;
        ok = false;
    }
    if obs.task_id.is_none() || obs.prov_task_id != obs.task_id {
        gaps.missing_task_id += 1;
        ok = false;
    }
    if !obs.model_matches_cost.is_matched() {
        gaps.missing_model_label += 1;
        ok = false;
    }
    if !obs.transcript_ref_valid.is_matched() {
        gaps.missing_transcript_ref += 1;
        ok = false;
    }
    let dur_ms = match obs.duration_ms {
        Some(ms) if ms > 0 => Some(ms),
        _ => {
            gaps.missing_duration_ms += 1;
            ok = false;
            None
        }
    };
    let cost_ok = obs.cost_usd.is_some_and(|u| u > 0.0) && obs.unpriced_tokens == 0;
    if !cost_ok {
        gaps.missing_cost_or_tokens += 1;
        ok = false;
    }
    if !obs.has_landing_record.is_matched() {
        gaps.missing_landing_record += 1;
        ok = false;
    }
    if ok {
        gaps.tasks_fully_attributed += 1;
    }
    dur_ms
}

fn verify_transcript_ref_against_disk(
    archive: &LocalTranscriptArchive,
    agent_id: &str,
    native_sid: &str,
    recorded_ref: Option<&str>,
) -> bool {
    let Some(tref) = recorded_ref else {
        return false;
    };
    let expected_prefix = format!("archive/{agent_id}/{native_sid}.jsonl#sha256=");
    let Some(hex_digest) = tref.strip_prefix(&expected_prefix) else {
        return false;
    };
    let Ok(bytes) = read_archived_transcript_bytes(archive, agent_id, native_sid) else {
        return false;
    };
    if bytes.is_empty() || sha256_of(&bytes).hex() != hex_digest {
        return false;
    }
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .any(|line| ingest::claude_usage(line).is_some())
}

fn read_observed_task_lineage(
    store: &PostgresStore,
    archive: &LocalTranscriptArchive,
    task_id: i64,
    session_id: &SessionId,
    landed_tasks: &BTreeSet<i64>,
    cost_tasks: &BTreeMap<i64, &Value>,
    cost_execs_by_id: &BTreeMap<String, &Value>,
) -> Result<ObservedLineage, String> {
    let provs = store
        .prov_changes_for_task(task_id)
        .map_err(|e| format!("prov_changes_for_task: {e}"))?;
    let prov = provs.first();

    let task_facts = store
        .prov_task_facts(&[task_id])
        .map_err(|e| format!("prov_task_facts: {e}"))?;
    let task_agent = task_facts.get(&task_id).map(|f| f.agent_id.as_str());

    let execs = store
        .executions(session_id)
        .map_err(|e| format!("executions: {e}"))?;
    let ended_exec = execs.iter().find(|e| e.ended_at.is_some());
    let exec_id_str = ended_exec.map(|e| e.execution_id.as_str().to_string());
    let exec_agents = match &exec_id_str {
        Some(id) => store
            .prov_execution_agents(std::slice::from_ref(id))
            .map_err(|e| format!("prov_execution_agents: {e}"))?,
        None => std::collections::HashMap::new(),
    };
    let event_agent = exec_id_str
        .as_ref()
        .and_then(|id| exec_agents.get(id))
        .map(String::as_str);

    let cost_row = cost_tasks.get(&task_id).copied();
    let cost_exec_row = exec_id_str
        .as_ref()
        .and_then(|id| cost_execs_by_id.get(id))
        .copied();
    let cost_model = cost_exec_row
        .and_then(|r| r["models"].as_array())
        .and_then(|m| m.first())
        .and_then(|m| m["model"].as_str());

    let agent_matches_task = MatchState::from_bool(
        prov.zip(task_agent)
            .is_some_and(|(p, t_ag)| !p.agent_id.is_empty() && p.agent_id == t_ag),
    );
    let agent_matches_execution = MatchState::from_bool(
        prov.zip(event_agent)
            .is_some_and(|(p, e_ag)| p.agent_id == e_ag),
    );
    let has_execution_id = MatchState::from_bool(
        prov.zip(exec_id_str.as_deref())
            .is_some_and(|(p, eid)| p.execution_id.as_deref() == Some(eid)),
    );
    let model_matches_cost = MatchState::from_bool(
        prov.zip(cost_model)
            .is_some_and(|(p, m)| p.model_label.as_deref() == Some(m) && !m.is_empty()),
    );

    let native_sid = match &exec_id_str {
        Some(eid) => find_native_sid_for_execution(store, eid)?,
        None => None,
    };
    let transcript_ref_valid =
        MatchState::from_bool(prov.zip(native_sid.as_deref()).is_some_and(|(p, nsid)| {
            verify_transcript_ref_against_disk(
                archive,
                &p.agent_id,
                nsid,
                p.transcript_ref.as_deref(),
            )
        }));

    Ok(ObservedLineage {
        task_id: Some(task_id),
        prov_task_id: prov.and_then(|p| p.task_id),
        agent_matches_task,
        agent_matches_execution,
        has_execution_id,
        model_matches_cost,
        transcript_ref_valid,
        duration_ms: ended_exec.and_then(|e| e.duration_ms),
        cost_usd: cost_row.and_then(|r| r["usd"].as_f64()),
        unpriced_tokens: cost_row
            .and_then(|r| r["unpriced_tokens"].as_i64())
            .unwrap_or(1),
        has_landing_record: MatchState::from_bool(landed_tasks.contains(&task_id)),
    })
}

type LineageMutation = (&'static str, fn(&mut ObservedLineage));

fn run_negative_attribution_falsification(valid: &ObservedLineage) -> Result<usize, String> {
    let mutations: [LineageMutation; 8] = [
        ("missing_agent_id", |o| {
            o.agent_matches_task = MatchState::Mismatched;
        }),
        ("missing_execution_id", |o| {
            o.has_execution_id = MatchState::Mismatched;
        }),
        ("missing_task_id", |o| o.prov_task_id = None),
        ("missing_model_label", |o| {
            o.model_matches_cost = MatchState::Mismatched;
        }),
        ("missing_transcript_ref", |o| {
            o.transcript_ref_valid = MatchState::Mismatched;
        }),
        ("missing_duration_ms", |o| o.duration_ms = Some(0)),
        ("missing_cost_or_tokens", |o| o.unpriced_tokens = 1),
        ("missing_landing_record", |o| {
            o.has_landing_record = MatchState::Mismatched;
        }),
    ];
    for (name, mutate) in mutations {
        let mut mutated = valid.clone();
        mutate(&mut mutated);
        let mut gaps = AttributionGaps::default();
        let _ = evaluate_observed_lineage(&mutated, &mut gaps);
        if gaps.tasks_fully_attributed != 0 || gaps.total_field_gaps() == 0 {
            return Err(format!(
                "negative attribution falsification failed: mutating {name} did not cause ATTRIBUTION_COMPLETENESS to fail"
            ));
        }
    }
    Ok(mutations.len())
}

fn run_attribution_bridge_tasks(
    store: &PostgresStore,
    archive: &LocalTranscriptArchive,
    cfg: &Config,
) -> Result<Vec<(i64, SessionId)>, String> {
    let mission = store
        .mission_add(
            "Always-on Bridge attribution completeness benchmark",
            &json!(["/tmp/always-on-bench-source.git"]),
            "human",
        )
        .map_err(|e| format!("mission_add: {e}"))?;
    store
        .mission_configure(mission, Some("/bench/gate"), "gate", None, None)
        .map_err(|e| format!("mission_configure: {e}"))?;

    let pg_bridge = Arc::new(
        store
            .new_connection()
            .map_err(|e| format!("bridge pg new_connection: {e}"))?,
    );
    let mem_host = Arc::new(MemoryStore::new("always-on-bench", Arc::new(SystemClock)));
    let mut bcfg = BridgeConfig::new("always-on-attr-bridge");
    bcfg.claude_projects = "/projects".to_string();
    bcfg.watch = Some(Arc::new(MemoryWatch::refusing()));
    bcfg.recover_worker_leases = false;
    let mut bridge = Bridge::new(
        pg_bridge.clone(),
        pg_bridge,
        mem_host.clone(),
        mem_host.clone(),
        bcfg,
    )
    .with_transcript_archive(Arc::new(archive.clone()));
    bridge
        .recover()
        .map_err(|e| format!("bridge.recover: {e}"))?;

    let models = [
        (ModelProfile::Default, "claude-opus-5-5"),
        (ModelProfile::Default, "claude-sonnet-4-6"),
    ];
    let mut tasks = Vec::with_capacity(cfg.attribution_tasks);
    for idx in 0..cfg.attribution_tasks {
        let (profile, model_name) = models[idx % models.len()];
        let worker = format!("attr{idx:03}");
        let task_id = store
            .task_assign_direct(
                mission,
                "ephemeral",
                &worker,
                &format!("Bridge attribution task {idx}"),
                "default",
                false,
                false,
                Some("claude"),
                None,
            )
            .map_err(|e| format!("task_assign_direct: {e}"))?;
        let session_id = drive_bridge_execution(
            &mut bridge,
            store,
            &mem_host,
            archive,
            &AttrTaskSpec {
                task_id,
                idx,
                model_profile: profile,
                model_name,
            },
        )?;
        tasks.push((task_id, session_id));
    }
    Ok(tasks)
}

fn collect_attribution_gaps(
    store: &PostgresStore,
    archive: &LocalTranscriptArchive,
    tasks: &[(i64, SessionId)],
    landed_tasks: &BTreeSet<i64>,
    cost_tasks: &BTreeMap<i64, &Value>,
    cost_execs_by_id: &BTreeMap<String, &Value>,
) -> Result<(AttributionGaps, i64, usize), String> {
    let mut gaps = AttributionGaps::default();
    let mut total_work_duration_ms = 0_i64;
    let mut sample_observed = None;
    for (task_id, session_id) in tasks {
        let obs = read_observed_task_lineage(
            store,
            archive,
            *task_id,
            session_id,
            landed_tasks,
            cost_tasks,
            cost_execs_by_id,
        )?;
        if sample_observed.is_none() {
            sample_observed = Some(obs.clone());
        }
        if let Some(ms) = evaluate_observed_lineage(&obs, &mut gaps) {
            total_work_duration_ms = total_work_duration_ms.saturating_add(ms);
        }
    }
    let neg_checked = match sample_observed.as_ref() {
        Some(obs) => run_negative_attribution_falsification(obs)?,
        None => {
            return Err("no observed attribution task to run negative falsification".to_string());
        }
    };
    Ok((gaps, total_work_duration_ms, neg_checked))
}

fn verify_attribution_completeness(
    store: &PostgresStore,
    archive: &LocalTranscriptArchive,
    cfg: &Config,
) -> Result<Value, String> {
    let window_start = store
        .database_now_ms()
        .map_err(|e| format!("database_now_ms window start: {e}"))?
        .saturating_sub(60_000);
    let tasks = run_attribution_bridge_tasks(store, archive, cfg)?;

    let recorded_msgs = store
        .cost_record(&built_in_model_prices())
        .map_err(|e| format!("cost_record: {e}"))?;
    let window_end = store
        .database_now_ms()
        .map_err(|e| format!("database_now_ms window_end: {e}"))?
        .saturating_add(60_000);
    let cost = store
        .cost_window(window_start, window_end)
        .map_err(|e| format!("cost_window: {e}"))?;
    let cost_execs_by_id: BTreeMap<String, &Value> = cost["executions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| row["execution"].as_str().map(|id| (id.to_string(), row)))
        .collect();

    for (idx, (task_id, session_id)) in tasks.iter().enumerate() {
        record_landed_provenance_from_system(
            store,
            archive,
            *task_id,
            session_id,
            &cost_execs_by_id,
            idx,
        )?;
    }

    let facts = store
        .scorecard_facts(window_end)
        .map_err(|e| format!("scorecard_facts: {e}"))?;
    let landed_tasks: BTreeSet<i64> = facts["control"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|rec| rec["type"] == "task.landed")
        .filter_map(|rec| rec["id"].as_i64())
        .collect();
    let cost_tasks: BTreeMap<i64, &Value> = cost["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| row["task"].as_i64().map(|id| (id, row)))
        .collect();

    let (gaps, total_work_duration_ms, neg_checked) = collect_attribution_gaps(
        store,
        archive,
        &tasks,
        &landed_tasks,
        &cost_tasks,
        &cost_execs_by_id,
    )?;

    let total_tasks = tasks.len();
    let total_field_gaps = gaps.total_field_gaps();
    let completeness_pct = if total_tasks == 0 {
        0.0
    } else {
        (u64_to_f64(u64::try_from(gaps.tasks_fully_attributed).unwrap_or(0)) * 100.0)
            / u64_to_f64(u64::try_from(total_tasks).unwrap_or(1))
    };
    let tokens = &cost["total"]["tokens"];
    let total_tokens = tokens["input"].as_i64().unwrap_or(0)
        + tokens["output"].as_i64().unwrap_or(0)
        + tokens["cache_write"].as_i64().unwrap_or(0)
        + tokens["cache_read"].as_i64().unwrap_or(0);

    Ok(json!({
        "bridge_driven": true,
        "tasks_total": total_tasks,
        "tasks_fully_attributed": gaps.tasks_fully_attributed,
        "attribution_gaps": total_field_gaps,
        "completeness_pct": completeness_pct,
        "recorded_usage_messages": recorded_msgs,
        "total_work_duration_ms": total_work_duration_ms,
        "total_tokens": total_tokens,
        "total_cost_usd": cost["total"]["usd"].as_f64().unwrap_or(0.0),
        "unpriced_tokens": cost["total"]["unpriced_tokens"].as_i64().unwrap_or(0),
        "negative_falsification_passed": true,
        "negative_cases_checked": neg_checked,
        "field_gaps": {
            "agent_id": gaps.missing_agent_id,
            "execution_id": gaps.missing_execution_id,
            "task_id": gaps.missing_task_id,
            "model_label": gaps.missing_model_label,
            "transcript_ref": gaps.missing_transcript_ref,
            "duration_ms": gaps.missing_duration_ms,
            "cost_or_tokens": gaps.missing_cost_or_tokens,
            "landing_record": gaps.missing_landing_record,
        }
    }))
}

fn parse_pg_env(url: &str) -> Result<Vec<(String, String)>, String> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or_else(|| "database URL must start with postgres:// or postgresql://".to_string())?;
    let (before_query, query_opt) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, db_opt) = before_query.split_once('/').unwrap_or((before_query, ""));
    let mut envs = BTreeMap::new();
    let hostport = if let Some((userinfo, hp)) = authority.rsplit_once('@') {
        if let Some((user, pass)) = userinfo.split_once(':') {
            if !user.is_empty() {
                envs.insert("PGUSER".to_string(), user.to_string());
            }
            if !pass.is_empty() {
                envs.insert("PGPASSWORD".to_string(), pass.to_string());
            }
        } else if !userinfo.is_empty() {
            envs.insert("PGUSER".to_string(), userinfo.to_string());
        }
        hp
    } else {
        authority
    };
    if let Some((host, port)) = hostport.rsplit_once(':')
        && !hostport.ends_with(']')
    {
        if !host.is_empty() {
            envs.insert(
                "PGHOST".to_string(),
                host.trim_matches(['[', ']']).to_string(),
            );
        }
        if !port.is_empty() {
            envs.insert("PGPORT".to_string(), port.to_string());
        }
    } else if !hostport.is_empty() {
        envs.insert(
            "PGHOST".to_string(),
            hostport.trim_matches(['[', ']']).to_string(),
        );
    }
    if !db_opt.is_empty() {
        envs.insert("PGDATABASE".to_string(), db_opt.to_string());
    }
    for pair in query_opt.split('&').filter(|p| !p.is_empty()) {
        if let Some((k, v)) = pair.split_once('=')
            && !v.is_empty()
        {
            match k {
                "host" => {
                    envs.insert("PGHOST".to_string(), v.to_string());
                }
                "port" => {
                    envs.insert("PGPORT".to_string(), v.to_string());
                }
                "dbname" => {
                    envs.insert("PGDATABASE".to_string(), v.to_string());
                }
                "user" => {
                    envs.insert("PGUSER".to_string(), v.to_string());
                }
                "password" => {
                    envs.insert("PGPASSWORD".to_string(), v.to_string());
                }
                "sslmode" => {
                    envs.insert("PGSSLMODE".to_string(), v.to_string());
                }
                _ => {}
            }
        }
    }
    envs.insert("PGCONNECT_TIMEOUT".to_string(), "5".to_string());
    Ok(envs.into_iter().collect())
}

struct BenchSchemaGuard {
    url: String,
    schema: String,
}

impl BenchSchemaGuard {
    fn new(url: String, schema: String) -> Result<Self, String> {
        if schema.is_empty()
            || !schema
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(format!("invalid disposable schema name: {schema:?}"));
        }
        Ok(Self { url, schema })
    }
}

impl Drop for BenchSchemaGuard {
    fn drop(&mut self) {
        let Ok(pg_envs) = parse_pg_env(&self.url) else {
            return;
        };
        let sql = format!(
            "SET lock_timeout = '5s'; DROP SCHEMA IF EXISTS {} CASCADE;",
            self.schema
        );
        let mut cmd = Command::new("psql");
        cmd.args(["-X", "-q", "-v", "ON_ERROR_STOP=1", "-c", &sql])
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (k, v) in pg_envs {
            cmd.env(k, v);
        }
        match cmd.output() {
            Ok(out) if !out.status.success() => {
                eprintln!(
                    "always_on_control_plane_bench: schema cleanup warning for {}: {}",
                    self.schema,
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Err(err) => {
                eprintln!(
                    "always_on_control_plane_bench: schema cleanup skipped for {}: {err}",
                    self.schema
                );
            }
            Ok(_) => {}
        }
    }
}

fn run(cfg: &Config) -> Result<Value, String> {
    let bench_url = env::var("SPAWN_BENCH_DATABASE_URL").map_err(|_| {
        "SPAWN_BENCH_DATABASE_URL must be set to a disposable benchmark database URL".to_string()
    })?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let schema = format!("spawn_always_on_{}_{nanos:x}", std::process::id());
    let _schema_guard = BenchSchemaGuard::new(bench_url.clone(), schema.clone())?;
    let base_store = PostgresStore::connect_in_schema(&bench_url, &schema)
        .map_err(|e| format!("connect benchmark schema {schema}: {e}"))?;
    let scratch_archive = ScratchArchive::new(cfg.archive_root.clone())?;
    let archive = LocalTranscriptArchive::new(scratch_archive.0.clone());

    let event_log = run_event_log_stress(&base_store, &bench_url, &schema, cfg)?;
    let shard_claims = run_shard_claim_stress(&base_store, cfg)?;
    let attribution = verify_attribution_completeness(&base_store, &archive, cfg)?;
    drop(base_store);

    let created_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));

    Ok(json!({
        "schema_version": 1,
        "benchmark": "always-on-control-plane",
        "created_at_ms": created_at_ms,
        "config": {
            "sessions": cfg.sessions,
            "events_per_session": cfg.events_per_session,
            "shards": cfg.shards,
            "commands_per_session": cfg.commands_per_session,
            "writer_pool": cfg.writer_pool,
            "attribution_tasks": cfg.attribution_tasks,
        },
        "dimension_b_pg16": {
            "event_log_xid8": event_log,
            "sharded_bridge_claim": shard_claims,
        },
        "dimension_c_attribution": attribution,
    }))
}

fn main() {
    match parse_config() {
        Ok(None) => print!("{USAGE}"),
        Ok(Some(cfg)) => match run(&cfg) {
            Ok(report) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).expect("JSON report")
                );
            }
            Err(err) => {
                eprintln!("always_on_control_plane_bench: {err}");
                std::process::exit(1);
            }
        },
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_iso_roundtrips_with_ingest_parser() {
        for ts in [0_i64, 1_700_000_000_123, 1_764_300_000_987] {
            let iso = format_utc_iso_ms(ts);
            assert_eq!(ingest::utc_ms(&iso), Some(ts), "roundtrip failed for {iso}");
        }
    }

    #[test]
    fn pg_env_parser_handles_unix_socket_override() {
        let envs: BTreeMap<String, String> = parse_pg_env(
            "postgres://spawn@127.0.0.1:65511/bench_db?host=/tmp/pgsock&sslmode=disable",
        )
        .expect("parse url")
        .into_iter()
        .collect();
        assert_eq!(envs.get("PGUSER").map(String::as_str), Some("spawn"));
        assert_eq!(envs.get("PGHOST").map(String::as_str), Some("/tmp/pgsock"));
        assert_eq!(envs.get("PGPORT").map(String::as_str), Some("65511"));
        assert_eq!(envs.get("PGDATABASE").map(String::as_str), Some("bench_db"));
        assert_eq!(envs.get("PGSSLMODE").map(String::as_str), Some("disable"));
    }

    #[test]
    fn attribution_negative_falsification_catches_all_eight_gaps() {
        let valid = ObservedLineage {
            task_id: Some(42),
            prov_task_id: Some(42),
            agent_matches_task: MatchState::Matched,
            agent_matches_execution: MatchState::Matched,
            has_execution_id: MatchState::Matched,
            model_matches_cost: MatchState::Matched,
            transcript_ref_valid: MatchState::Matched,
            duration_ms: Some(12),
            cost_usd: Some(0.05),
            unpriced_tokens: 0,
            has_landing_record: MatchState::Matched,
        };
        let mut baseline_gaps = AttributionGaps::default();
        assert_eq!(
            evaluate_observed_lineage(&valid, &mut baseline_gaps),
            Some(12)
        );
        assert_eq!(baseline_gaps.tasks_fully_attributed, 1);
        assert_eq!(baseline_gaps.total_field_gaps(), 0);
        assert_eq!(run_negative_attribution_falsification(&valid), Ok(8));

        let mut zero_usd = valid;
        zero_usd.cost_usd = Some(0.0);
        let mut zero_gaps = AttributionGaps::default();
        let _ = evaluate_observed_lineage(&zero_usd, &mut zero_gaps);
        assert_eq!(zero_gaps.tasks_fully_attributed, 0);
        assert_eq!(zero_gaps.missing_cost_or_tokens, 1);
    }
}
