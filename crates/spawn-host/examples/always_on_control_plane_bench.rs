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
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use spawn_cas::sha256_of;
use spawn_core::command::{CommandAction, Outcome, Submission};
use spawn_core::event::{
    ENGINE, EndCause, EventKind, GlobalPosition, LaunchSummary, ModelProfile, NewEvent, Source,
};
use spawn_core::id::{
    AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId, WorkspaceId,
};
use spawn_core::port::{BridgeShard, CommandQueue, EventLog, ShardPartition, TranscriptArchive};
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
  --events-per-session <n>    Events written per session (default: 40)\n\
  --shards <n>                Bridge partitions to run concurrently (default: 4)\n\
  --commands-per-session <n>  Commands submitted/claimed per session (default: 16)\n\
  --writer-pool <n>           Concurrent PG writer connections (default: 8)\n\
  --attribution-tasks <n>     Synthetic tasks to check for full lineage (default: 16)\n\
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
            events_per_session: 40,
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

fn run_event_log_stress(base_store: &PostgresStore, cfg: &Config) -> Result<Value, String> {
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

    let mut out_of_order_reads = 0_usize;
    let mut commit_id_inversions = 0_usize;
    let mut duplicate_reads = 0_usize;
    let mut seen_read = BTreeSet::new();
    let mut prev_pos: Option<GlobalPosition> = None;
    let mut per_session_last: BTreeMap<SessionId, (GlobalPosition, spawn_core::event::SessionSeq)> =
        BTreeMap::new();
    for sample in &read_positions {
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
    let missing_reads = written_positions.difference(&seen_read).count();
    let unexpected_reads = seen_read.difference(&written_positions).count();

    Ok(json!({
        "writer_pool": cfg.writer_pool,
        "sessions": cfg.sessions,
        "events_target": total_events,
        "events_written": written_positions.len(),
        "events_read": read_positions.len(),
        "out_of_order_reads": out_of_order_reads,
        "commit_id_inversions": commit_id_inversions,
        "duplicate_reads": duplicate_reads,
        "missing_reads": missing_reads,
        "unexpected_reads": unexpected_reads,
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

struct TaskFixture {
    task_id: i64,
    agent: AgentId,
    session: SessionId,
    execution: ExecutionId,
    native_sid: String,
    model_label: &'static str,
    transcript_ref: String,
    payload: Vec<u8>,
}

fn build_attribution_events(
    idx: usize,
    command_id: CommandId,
    now_ms: i64,
    fix: &TaskFixture,
) -> Result<[(String, Source, EventKind); 6], String> {
    let workspace = WorkspaceId::new("0:/workspaces/always-on-bench").map_err(|e| e.to_string())?;
    Ok([
        (
            format!("native:{idx}"),
            Source::Bridge,
            EventKind::NativeSessionStarted {
                engine: ENGINE.to_string(),
                native_session_id: fix.native_sid.clone(),
                transcript_path: Some(fix.transcript_ref.clone()),
                engine_version: Some("2.0.0-bench".to_string()),
            },
        ),
        (
            format!("start:{idx}"),
            Source::Bridge,
            EventKind::ExecutionStarted {
                engine: ENGINE.to_string(),
                workspace,
                command_id,
                fork: None,
                model: ModelProfile::Default,
                transcript_from: Some(0),
                launch: Some(LaunchSummary {
                    relay_url: None,
                    model: Some(fix.model_label.to_string()),
                    small_model: None,
                    config_dir: None,
                }),
            },
        ),
        (
            format!("tool_s:{idx}"),
            Source::Claude,
            EventKind::ToolStarted {
                tool_name: "edit_file".to_string(),
                tool_use_id: format!("tu_{idx}"),
                summary: Some("crates/demo/src/lib.rs".to_string()),
                summary_whole: true,
            },
        ),
        (
            format!("tool_c:{idx}"),
            Source::Claude,
            EventKind::ToolCompleted {
                tool_use_id: format!("tu_{idx}"),
                duration_ms: Some(120),
            },
        ),
        (
            format!("usage:{idx}"),
            Source::Claude,
            EventKind::ModelUsage {
                message_id: format!("msg_attr_{idx}"),
                model: fix.model_label.to_string(),
                at_ms: now_ms,
                input_tokens: 4_000,
                output_tokens: 1_200,
                cache_write_tokens: 2_000,
                cache_read_tokens: 8_000,
            },
        ),
        (
            format!("turn:{idx}"),
            Source::Claude,
            EventKind::TurnStopped {
                input_tokens: Some(4_000),
                output_tokens: Some(1_200),
            },
        ),
    ])
}

fn append_attribution_execution_events(
    store: &PostgresStore,
    idx: usize,
    command_id: CommandId,
    fix: &TaskFixture,
) -> Result<i64, String> {
    let now_ms = store
        .database_now_ms()
        .map_err(|e| format!("database_now_ms: {e}"))?;
    let events = build_attribution_events(idx, command_id, now_ms, fix)?;

    for (ev_key, ev_src, ev_kind) in events {
        let _ = store
            .append(NewEvent {
                agent_id: fix.agent.clone(),
                session_id: fix.session.clone(),
                execution_id: Some(fix.execution.clone()),
                source: ev_src,
                source_event_id: Some(ev_key),
                kind: ev_kind,
            })
            .map_err(|e| format!("append attr event: {e}"))?;
    }

    // Real elapsed duration between execution.started and execution.ended (> 0 ms in PG).
    std::thread::sleep(Duration::from_millis(6));
    let _ = store
        .append(NewEvent {
            agent_id: fix.agent.clone(),
            session_id: fix.session.clone(),
            execution_id: Some(fix.execution.clone()),
            source: Source::Bridge,
            source_event_id: Some(format!("end:{idx}")),
            kind: EventKind::ExecutionEnded {
                cause: EndCause::Exited,
                exit_code: Some(0),
                signal: None,
                evidence: None,
            },
        })
        .map_err(|e| format!("append execution.ended: {e}"))?;

    store
        .database_now_ms()
        .map_err(|e| format!("database_now_ms: {e}"))
}

fn record_landing_and_provenance(
    store: &PostgresStore,
    idx: usize,
    ended_ms: i64,
    fix: &TaskFixture,
) -> Result<(), String> {
    store
        .task_delivered(fix.task_id, i64::try_from(idx + 1).expect("fits i64"))
        .map_err(|e| format!("task_delivered: {e}"))?;
    let commit_hex = format!("{:040x}", idx + 1);
    let base_hex = format!("{:040x}", idx + 10_000);
    let _ = store
        .task_reported_with(fix.task_id, "done", Some(&commit_hex))
        .map_err(|e| format!("task_reported_with: {e}"))?;
    let _ = store
        .integrator_record(&json!({
            "type": "task.gated",
            "id": fix.task_id,
            "revision": 0,
            "verdict": "accepted",
            "base": base_hex,
            "candidate": commit_hex,
        }))
        .map_err(|e| format!("integrator_record gated: {e}"))?;
    let _ = store
        .integrator_record(&json!({
            "type": "task.landed",
            "id": fix.task_id,
            "revision": 0,
            "from": base_hex,
            "to": commit_hex,
        }))
        .map_err(|e| format!("integrator_record landed: {e}"))?;
    let _ = store
        .record_change(
            &ProvChangeInput {
                change_id: format!("chg-attr-{idx:03}"),
                rev: 1,
                kind: ProvChangeKind::Land,
                domain: "d6:bench".to_string(),
                agent_id: fix.agent.as_str().to_string(),
                execution_id: Some(fix.execution.as_str().to_string()),
                task_id: Some(fix.task_id),
                preset_id: Some("always-on-default".to_string()),
                model_label: Some(fix.model_label.to_string()),
                transcript_ref: Some(fix.transcript_ref.clone()),
                attributed: ProvAttributionSource::Host,
                base: base_hex,
                result: commit_hex,
                boundary: ProvBoundary::Land,
                at_ms: ended_ms,
            },
            &[ProvEntryInput {
                path: format!("src/task_{idx}.rs"),
                before: None,
                after: Some(sha256_of(&fix.payload).to_string()),
                size: Some(i64::try_from(fix.payload.len()).expect("fits i64")),
                class: ProvEntryClass::Content,
                origin: None,
            }],
            &[],
        )
        .map_err(|e| format!("record_change: {e}"))?;
    Ok(())
}

fn emit_one_attribution_task(
    store: &PostgresStore,
    archive: &LocalTranscriptArchive,
    mission: i64,
    idx: usize,
    model_label: &'static str,
) -> Result<TaskFixture, String> {
    let worker = format!("attr{idx:03}");
    let (engine_name, task_model_label) = if idx.is_multiple_of(2) {
        ("claude", None)
    } else {
        ("codex", Some(model_label))
    };
    let task_id = store
        .task_assign_direct(
            mission,
            "ephemeral",
            &worker,
            &format!("Synthetic attribution task {idx}"),
            "default",
            false,
            false,
            Some(engine_name),
            task_model_label,
        )
        .map_err(|e| format!("task_assign_direct: {e}"))?;

    let agent =
        AgentId::new(format!("agent_worker_{worker}_{task_id}")).map_err(|e| e.to_string())?;
    let session =
        SessionId::new(format!("session_worker_{worker}_{task_id}")).map_err(|e| e.to_string())?;
    let cmd_num = u64::try_from(9_000_000 + idx).expect("fits u64");
    let command_id = CommandId(cmd_num);
    let execution = ExecutionId::for_command(command_id);
    let native_sid = format!("native-attr-{idx:03}");
    let payload = format!(
        "{{\"task_id\":{task_id},\"model\":{model_label:?},\"role\":\"assistant\",\"msg\":\"work_{idx}\"}}\n"
    )
    .into_bytes();

    archive
        .append(&agent, &native_sid, 0, 0, &payload)
        .map_err(|e| format!("archive.append: {e}"))?;
    let transcript_ref = format!(
        "archive/{}/{native_sid}.jsonl#sha256={}",
        agent.as_str(),
        sha256_of(&payload).hex()
    );

    let fix = TaskFixture {
        task_id,
        agent,
        session,
        execution,
        native_sid,
        model_label,
        transcript_ref,
        payload,
    };
    let ended_ms = append_attribution_execution_events(store, idx, command_id, &fix)?;
    record_landing_and_provenance(store, idx, ended_ms, &fix)?;

    Ok(fix)
}

fn emit_synthetic_attribution_tasks(
    store: &PostgresStore,
    archive: &LocalTranscriptArchive,
    count: usize,
) -> Result<(Vec<TaskFixture>, i64), String> {
    let mission = store
        .mission_add(
            "Always-on attribution completeness benchmark",
            &json!(["/tmp/always-on-bench-source.git"]),
            "human",
        )
        .map_err(|e| format!("mission_add: {e}"))?;
    store
        .mission_configure(mission, Some("/bench/gate"), "gate", None, None)
        .map_err(|e| format!("mission_configure: {e}"))?;

    let mut fixtures = Vec::with_capacity(count);
    let models = ["claude-sonnet-4-6", "gpt-5.4"];
    let base_now = store
        .database_now_ms()
        .map_err(|e| format!("database_now_ms: {e}"))?;
    let window_start = base_now.saturating_sub(60_000);

    for idx in 0..count {
        let model_label = models[idx % models.len()];
        fixtures.push(emit_one_attribution_task(
            store,
            archive,
            mission,
            idx,
            model_label,
        )?);
    }

    Ok((fixtures, window_start))
}

fn verify_task_lineage(
    store: &PostgresStore,
    archive: &LocalTranscriptArchive,
    fix: &TaskFixture,
    landed_tasks: &BTreeSet<i64>,
    cost_tasks: &BTreeMap<i64, &Value>,
    gaps: &mut AttributionGaps,
) -> Result<Option<i64>, String> {
    let mut ok = true;
    let provs = store
        .prov_changes_for_execution(fix.execution.as_str())
        .map_err(|e| format!("prov_changes_for_execution: {e}"))?;
    let prov = provs.first();
    if prov.is_none_or(|p| p.agent_id != fix.agent.as_str()) {
        gaps.missing_agent_id += 1;
        ok = false;
    }
    if prov.is_none_or(|p| p.execution_id.as_deref() != Some(fix.execution.as_str())) {
        gaps.missing_execution_id += 1;
        ok = false;
    }
    if prov.is_none_or(|p| p.task_id != Some(fix.task_id)) {
        gaps.missing_task_id += 1;
        ok = false;
    }
    if prov.is_none_or(|p| p.model_label.as_deref() != Some(fix.model_label)) {
        gaps.missing_model_label += 1;
        ok = false;
    }
    let archive_ok = archive
        .read(&fix.agent, &fix.native_sid, 0, 0, fix.payload.len())
        .is_ok_and(|bytes| bytes == fix.payload);
    if !archive_ok
        || prov.is_none_or(|p| p.transcript_ref.as_deref() != Some(fix.transcript_ref.as_str()))
    {
        gaps.missing_transcript_ref += 1;
        ok = false;
    }

    let execs = store
        .executions(&fix.session)
        .map_err(|e| format!("executions: {e}"))?;
    let exec_dur = execs
        .iter()
        .find(|e| e.execution_id == fix.execution && e.ended_at.is_some())
        .and_then(|e| e.duration_ms);
    let dur_ms = match exec_dur {
        Some(ms) if ms > 0 => Some(ms),
        _ => {
            gaps.missing_duration_ms += 1;
            ok = false;
            None
        }
    };

    let cost_ok = cost_tasks.get(&fix.task_id).is_some_and(|row| {
        row["usd"].as_f64().is_some_and(|u| u > 0.0)
            && row["unpriced_tokens"].as_i64().unwrap_or(1) == 0
    });
    if !cost_ok {
        gaps.missing_cost_or_tokens += 1;
        ok = false;
    }
    if !landed_tasks.contains(&fix.task_id) {
        gaps.missing_landing_record += 1;
        ok = false;
    }
    if ok {
        gaps.tasks_fully_attributed += 1;
    }
    Ok(dur_ms)
}

fn verify_attribution_completeness(
    store: &PostgresStore,
    archive: &LocalTranscriptArchive,
    cfg: &Config,
) -> Result<Value, String> {
    let (fixtures, window_start) =
        emit_synthetic_attribution_tasks(store, archive, cfg.attribution_tasks)?;

    let prices = json!({
        "claude-sonnet-4-6": {"input": 3.0, "output": 15.0, "cache_write": 3.75, "cache_read": 0.30},
        "gpt-5.4": {"input": 2.5, "output": 10.0, "cache_write": 2.50, "cache_read": 0.25},
    });
    let recorded_msgs = store
        .cost_record(&prices)
        .map_err(|e| format!("cost_record: {e}"))?;
    let window_end = store
        .database_now_ms()
        .map_err(|e| format!("database_now_ms: {e}"))?
        .saturating_add(60_000);
    let cost = store
        .cost_window(window_start, window_end)
        .map_err(|e| format!("cost_window: {e}"))?;
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

    let mut gaps = AttributionGaps::default();
    let mut total_work_duration_ms = 0_i64;
    for fix in &fixtures {
        if let Some(ms) =
            verify_task_lineage(store, archive, fix, &landed_tasks, &cost_tasks, &mut gaps)?
        {
            total_work_duration_ms = total_work_duration_ms.saturating_add(ms);
        }
    }

    let total_tasks = fixtures.len();
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
        "tasks_total": total_tasks,
        "tasks_fully_attributed": gaps.tasks_fully_attributed,
        "attribution_gaps": total_field_gaps,
        "completeness_pct": completeness_pct,
        "recorded_usage_messages": recorded_msgs,
        "total_work_duration_ms": total_work_duration_ms,
        "total_tokens": total_tokens,
        "total_cost_usd": cost["total"]["usd"].as_f64().unwrap_or(0.0),
        "unpriced_tokens": cost["total"]["unpriced_tokens"].as_i64().unwrap_or(0),
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

fn run(cfg: &Config) -> Result<Value, String> {
    let bench_url = env::var("SPAWN_BENCH_DATABASE_URL").map_err(|_| {
        "SPAWN_BENCH_DATABASE_URL must be set to a disposable benchmark database URL".to_string()
    })?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let schema = format!("spawn_always_on_{}_{nanos:x}", std::process::id());
    let base_store = PostgresStore::connect_in_schema(&bench_url, &schema)
        .map_err(|e| format!("connect benchmark schema {schema}: {e}"))?;
    let scratch_archive = ScratchArchive::new(cfg.archive_root.clone())?;
    let archive = LocalTranscriptArchive::new(scratch_archive.0.clone());

    let event_log = run_event_log_stress(&base_store, cfg)?;
    let shard_claims = run_shard_claim_stress(&base_store, cfg)?;
    let attribution = verify_attribution_completeness(&base_store, &archive, cfg)?;

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
