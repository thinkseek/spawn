#!/usr/bin/env bash
# End-to-end Always-On production readiness benchmark orchestrator & Go/No-Go Scorecard.
#
# Drives:
#   Dimension A: D6 (L1 tmpfs + FastCDC + CAS Merkle) at 8 & 16 concurrent branches
#   Dimension B: Isolated 1 CPU / 1 GiB RAM PG16 (max_connections=30) concurrency & sharding
#   Dimension C: Context & work-hours lineage completeness verification
#   Dimension D: Deterministic Always-On Go/No-Go Scorecard (JSON + Markdown)
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/bench-always-on.sh [options]

  --out-dir <dir>         Output directory for scorecard.{json,md} & sub-reports
  --pg-mode <mode>        auto (default), docker, systemd, or local
  --d6-iterations <n>     Measured D6 checkpoint cycles per branch (default: 12)
  --d6-warmup <n>         Warm-up D6 cycles per branch (default: 2)
  --d6-files <n>          Workspace files per branch (default: 128)
  --d6-file-bytes <n>     Bytes per workspace file (default: 4096)
  --fastcdc-bytes <n>     Oversized FastCDC file bytes > 4 MiB (default: 5242880)
  --pg-pool-size <n>      Shared PG pool size for 16-branch D6 run (default: 12)
  --skip-build            Reuse already compiled release benchmark binaries
  --self-test             Run scorer negative-falsification self-tests and exit
  --help, -h              Show this help
EOF
}

OUT_DIR=""
PG_MODE="auto"
D6_ITERATIONS=12
D6_WARMUP=2
D6_FILES=128
D6_FILE_BYTES=4096
FASTCDC_BYTES=5242880
PG_POOL_SIZE=12
SKIP_BUILD=0
SELF_TEST=0

while [ "$#" -gt 0 ]; do
    case "$1" in
        --out-dir) OUT_DIR="$2"; shift 2 ;;
        --pg-mode) PG_MODE="$2"; shift 2 ;;
        --d6-iterations) D6_ITERATIONS="$2"; shift 2 ;;
        --d6-warmup) D6_WARMUP="$2"; shift 2 ;;
        --d6-files) D6_FILES="$2"; shift 2 ;;
        --d6-file-bytes) D6_FILE_BYTES="$2"; shift 2 ;;
        --fastcdc-bytes) FASTCDC_BYTES="$2"; shift 2 ;;
        --pg-pool-size) PG_POOL_SIZE="$2"; shift 2 ;;
        --skip-build) SKIP_BUILD=1; shift ;;
        --self-test) SELF_TEST=1; shift ;;
        --help|-h) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

if [ -f /data00/spawn-toolchain/env ]; then
    # shellcheck disable=SC1091
    source /data00/spawn-toolchain/env
fi
if [ -d /data00/spawn-toolchain/bin-m2-pg ]; then
    export PATH="/data00/spawn-toolchain/bin-m2-pg:$PATH"
fi

if ! command -v python3 >/dev/null 2>&1; then
    echo "bench-always-on: python3 is required" >&2
    exit 2
fi

run_scorer_python() {
    python3 - "$@" <<'PY'
import copy
import datetime
import json
import pathlib
import sys


def evaluate_scorecard(
    d6_8,
    d6_16,
    pg_restart_verify,
    cp,
    samples,
    *,
    pg_mode,
    cgroup_enforced,
    cgroup_details,
    pg_ver,
    pg_ver_num,
    pg_max_conn,
    source_rev,
    arch,
    cpu_cores,
):
    valid_samples = [
        s
        for s in samples
        if s.get("ok") is True
        and isinstance(s.get("total_conn"), int)
        and isinstance(s.get("active_conn"), int)
        and isinstance(s.get("deadlocks_total"), int)
        and isinstance(s.get("rss_kb"), int)
    ]
    probe_errors = sum(1 for s in samples if s.get("ok") is not True)
    peak_total_conn = max((s["total_conn"] for s in valid_samples), default=0)
    peak_active_conn = max((s["active_conn"] for s in valid_samples), default=0)
    peak_rss_kb = max((s["rss_kb"] for s in valid_samples), default=0)
    peak_rss_mib = round(peak_rss_kb / 1024.0, 2)
    pg_deadlocks_delta = (
        valid_samples[-1]["deadlocks_total"] - valid_samples[0]["deadlocks_total"]
        if len(valid_samples) >= 2
        else (valid_samples[0]["deadlocks_total"] if valid_samples else 0)
    )

    ao_8 = d6_8["always_on_d6"]
    ao_16 = d6_16["always_on_d6"]
    cd_8 = ao_8["crash_restart_drill"]
    cd_16 = ao_16["crash_restart_drill"]
    ev = cp["dimension_b_pg16"]["event_log_xid8"]
    sh = cp["dimension_b_pg16"]["sharded_bridge_claim"]
    attr = cp["dimension_c_attribution"]

    isolated_1c1g = bool(pg_mode in ("docker", "systemd") and cgroup_enforced)
    pg_envelope_label = (
        f"1 CPU / 1 GiB RAM ({cgroup_details})"
        if isolated_1c1g
        else "UNCONSTRAINED (no 1C/1G cgroup limit enforced)"
    )

    def d6_land_ok(ao, expected_n):
        return (
            0 < ao["max_bytes_per_landing"] < 52_428_800
            and ao.get("expected_landings") == expected_n
            and ao.get("landings_completed") == expected_n
            and ao.get("merged_files_verified") == expected_n
        )

    def d6_dedup_ok(ao, expected_n):
        return bool(
            ao.get("slo_cdc_dedup_passed") is True
            and ao.get("cdc_shifted_files") == expected_n
            and ao.get("cdc_shifted_logical_bytes", 0) > 0
            and ao.get("cdc_unique_stored_bytes", 0) > 0
            and ao.get("cdc_dedup_ratio", 0.0) >= 2.0
            and 0.0 < ao.get("cdc_fixed_block_dedup_ratio", 0.0) < 1.2
            and ao.get("logical_written_bytes", 0) >= ao.get("cdc_shifted_logical_bytes", 0)
        )

    def crash_post_torn_ok(cd):
        before_entries = cd.get("journal_entries_before_commit", 0)
        after_entries = cd.get("journal_entries_after_commit", 0)
        pre_seq = cd.get("reconcile_pre_last_journal_seq", 0)
        post_seq = cd.get("reconcile_post_last_journal_seq", 0)
        return bool(
            cd.get("verified") is True
            and cd.get("lost_refs") == 0
            and cd.get("lost_commits") == 0
            and cd.get("corrupt_objects") == 0
            and cd.get("torn_tail_present_before_commit") is True
            and cd.get("torn_tail_healed_on_disk") is True
            and cd.get("torn_tail_healed_and_committed") is True
            and cd.get("post_torn_commit_reconciled") is True
            and after_entries == before_entries + 1
            and cd.get("reconcile_post_entries_considered", 0) >= 1
            and cd.get("reconcile_converged_entries_considered") == 0
            and post_seq > pre_seq > 0
            and cd.get("verifier_pid", 0) > 0
        )

    def crash_drill_ok(cd):
        return bool(
            crash_post_torn_ok(cd)
            and cd.get("sigkill_executed") is True
            and cd.get("exit_signal") == 9
            and cd.get("committed_before_kill", 0) >= 6
        )

    pg_restart_ok = bool(
        pg_restart_verify.get("inject_torn_tail") is True
        and crash_post_torn_ok(pg_restart_verify)
    )

    envelope_reasons = []
    if len(valid_samples) < 3:
        envelope_reasons.append(f"samples={len(valid_samples)}<3")
    if probe_errors > 0:
        envelope_reasons.append(f"probe_errors={probe_errors}")
    if peak_rss_kb <= 0:
        envelope_reasons.append("rss_kb=0")
    if peak_total_conn <= 0:
        envelope_reasons.append("peak_conn=0")
    if not isolated_1c1g:
        envelope_reasons.append("unconstrained_cgroup")
    if pg_ver_num < 160000:
        envelope_reasons.append(f"pg_version={pg_ver}<16")
    if pg_max_conn != 30:
        envelope_reasons.append(f"max_connections={pg_max_conn}!=30")
    envelope_note = f" [{', '.join(envelope_reasons)}]" if envelope_reasons else ""

    gates = [
        {
            "id": "D6_8_LAND_DELTA",
            "dimension": "A: D6 8-Branch",
            "metric": "max_bytes_per_landing",
            "measured": (
                f"{ao_8['max_bytes_per_landing']:,} B "
                f"({ao_8['max_bytes_per_landing'] / (1024*1024):.3f} MiB), "
                f"landed={ao_8.get('landings_completed', 0)}/{ao_8.get('expected_landings', 8)}, "
                f"verified={ao_8.get('merged_files_verified', 0)}"
            ),
            "threshold": "< 52,428,800 B (50.0 MiB) & 8/8 verified",
            "passed": d6_land_ok(ao_8, 8),
        },
        {
            "id": "D6_16_LAND_DELTA",
            "dimension": "A: D6 16-Branch",
            "metric": "max_bytes_per_landing",
            "measured": (
                f"{ao_16['max_bytes_per_landing']:,} B "
                f"({ao_16['max_bytes_per_landing'] / (1024*1024):.3f} MiB), "
                f"landed={ao_16.get('landings_completed', 0)}/{ao_16.get('expected_landings', 16)}, "
                f"verified={ao_16.get('merged_files_verified', 0)}"
            ),
            "threshold": "< 52,428,800 B (50.0 MiB) & 16/16 verified",
            "passed": d6_land_ok(ao_16, 16),
        },
        {
            "id": "D6_8_DEDUP_RATIO",
            "dimension": "A: D6 8-Branch",
            "metric": "cdc_dedup_ratio vs fixed_block",
            "measured": (
                f"cdc={ao_8.get('cdc_dedup_ratio', 0.0):.2f}x "
                f"vs fixed={ao_8.get('cdc_fixed_block_dedup_ratio', 0.0):.2f}x "
                f"(files={ao_8.get('cdc_shifted_files', 0)}/8, total={ao_8['dedup_ratio']:.2f}x)"
            ),
            "threshold": "cdc >= 2.00x & fixed < 1.20x & 8/8 shifted",
            "passed": d6_dedup_ok(ao_8, 8),
        },
        {
            "id": "D6_16_DEDUP_RATIO",
            "dimension": "A: D6 16-Branch",
            "metric": "cdc_dedup_ratio vs fixed_block",
            "measured": (
                f"cdc={ao_16.get('cdc_dedup_ratio', 0.0):.2f}x "
                f"vs fixed={ao_16.get('cdc_fixed_block_dedup_ratio', 0.0):.2f}x "
                f"(files={ao_16.get('cdc_shifted_files', 0)}/16, total={ao_16['dedup_ratio']:.2f}x)"
            ),
            "threshold": "cdc >= 2.00x & fixed < 1.20x & 16/16 shifted",
            "passed": d6_dedup_ok(ao_16, 16),
        },
        {
            "id": "D6_8_CHECKPOINT_P95",
            "dimension": "A: D6 8-Branch",
            "metric": "checkpoint_changed p95_ms",
            "measured": f"{d6_8['operations']['checkpoint_changed']['p95_ms']:.2f} ms",
            "threshold": "<= 500.00 ms",
            "passed": d6_8["operations"]["checkpoint_changed"]["p95_ms"] <= 500.0,
        },
        {
            "id": "D6_16_CHECKPOINT_P95",
            "dimension": "A: D6 16-Branch",
            "metric": "checkpoint_changed p95_ms",
            "measured": f"{d6_16['operations']['checkpoint_changed']['p95_ms']:.2f} ms",
            "threshold": "<= 1000.00 ms",
            "passed": d6_16["operations"]["checkpoint_changed"]["p95_ms"] <= 1000.0,
        },
        {
            "id": "D6_16_LAND_P95",
            "dimension": "A: D6 16-Branch",
            "metric": "land_main p95_ms",
            "measured": (
                f"{d6_16['operations']['land_main']['p95_ms']:.2f} ms "
                f"(n={d6_16['operations']['land_main']['n']}, retries={ao_16.get('land_cas_retries', 0)})"
            ),
            "threshold": "<= 500.00 ms & n=16",
            "passed": (
                d6_16["operations"]["land_main"]["p95_ms"] <= 500.0
                and d6_16["operations"]["land_main"]["n"] == 16
            ),
        },
        {
            "id": "D6_CRASH_RESTART_ZERO_LOSS",
            "dimension": "A: Crash-Restart",
            "metric": "SIGKILL + torn_heal + post_torn_commit + pg_restart",
            "measured": (
                f"8b=({cd_8['lost_refs']}/{cd_8['lost_commits']}/{cd_8['corrupt_objects']}), "
                f"16b=({cd_16['lost_refs']}/{cd_16['lost_commits']}/{cd_16['corrupt_objects']}), "
                f"post_torn_heal_16b={cd_16.get('torn_tail_healed_and_committed')}/"
                f"{cd_16.get('post_torn_commit_reconciled')}, "
                f"pg_restart={pg_restart_ok} (snaps={pg_restart_verify.get('snapshots_verified', 0)})"
            ),
            "threshold": "0/0/0, SIGKILL=true, torn_heal_commit=true, post_torn_reconcile=true, pg_restart=true",
            "passed": crash_drill_ok(cd_8) and crash_drill_ok(cd_16) and pg_restart_ok,
        },
        {
            "id": "PG16_XID8_WATERMARK_INTEGRITY",
            "dimension": "B: 1C/1G PG16",
            "metric": "session OOO / dup / miss / deadlock",
            "measured": (
                f"{ev['out_of_order_reads']} / {ev['duplicate_reads']} / "
                f"{ev['missing_reads']} / {ev['deadlocks']} "
                f"({ev['events_written']}/{ev['events_target']} events, "
                f"xid8_reorders={ev.get('commit_id_inversions', 0)} "
                f"[det={ev.get('deterministic_commit_id_inversions', 0)}], "
                f"xmin_holdback={ev.get('xmin_holdbacks_verified', 0)}/{ev.get('xmin_holdbacks_attempted', 0)}, "
                f"xmin_leaks={ev.get('xmin_premature_leaks', 0)})"
            ),
            "threshold": "0 / 0 / 0 / 0, write_errors=0, xid8_reorders>=4 (det>=4), xmin_holdback>=2/2, xmin_leaks=0",
            "passed": (
                ev["events_written"] == ev["events_target"]
                and ev["events_target"] >= 1024
                and ev["out_of_order_reads"] == 0
                and ev["duplicate_reads"] == 0
                and ev["missing_reads"] == 0
                and ev.get("unexpected_reads", 0) == 0
                and ev["deadlocks"] == 0
                and ev["write_errors"] == 0
                and ev["read_errors"] == 0
                and ev.get("deterministic_commit_id_inversions", 0) >= 4
                and ev.get("commit_id_inversions", 0) >= 4
                and ev.get("xmin_holdbacks_attempted", 0) >= 2
                and ev.get("xmin_holdbacks_verified", 0) == ev.get("xmin_holdbacks_attempted", 0)
                and ev.get("xmin_premature_leaks", 1) == 0
            ),
        },
        {
            "id": "PG16_SHARD_CLAIM_INTEGRITY",
            "dimension": "B: 1C/1G PG16",
            "metric": "dup / unclaimed / wrong_shard / deadlock",
            "measured": (
                f"{sh['duplicate_claims']} / {sh['unclaimed_commands']} / "
                f"{sh['wrong_shard_claims']} / {sh['deadlocks']} "
                f"({sh['commands_claimed_unique']}/{sh['commands_target']})"
            ),
            "threshold": "0 / 0 / 0 / 0 and full claim",
            "passed": (
                sh["commands_claimed_unique"] == sh["commands_target"]
                and sh["duplicate_claims"] == 0
                and sh["unclaimed_commands"] == 0
                and sh["wrong_shard_claims"] == 0
                and sh["deadlocks"] == 0
                and sh["submit_errors"] == 0
                and sh["claim_errors"] == 0
                and sh["finish_errors"] == 0
            ),
        },
        {
            "id": "PG16_RESOURCE_ENVELOPE",
            "dimension": "B: 1C/1G PG16",
            "metric": "samples / cgroup / pg16 / rss / conn / deadlocks",
            "measured": (
                f"{peak_total_conn}/{pg_max_conn} conns, {peak_rss_mib:.2f} MiB RSS, "
                f"{pg_deadlocks_delta} deadlocks, samples={len(valid_samples)}{envelope_note}"
            ),
            "threshold": "samples>=3, probe_err=0, PG>=16, max_conn=30, 1C/1G cgroup, conn<=27, RSS<900MiB",
            "passed": (
                len(valid_samples) >= 3
                and probe_errors == 0
                and isolated_1c1g
                and pg_ver_num >= 160000
                and pg_max_conn == 30
                and 0 < peak_total_conn <= 27
                and 0 < peak_rss_kb
                and peak_rss_mib < 900.0
                and pg_deadlocks_delta == 0
            ),
        },
        {
            "id": "ATTRIBUTION_COMPLETENESS",
            "dimension": "C: Attribution",
            "metric": "completeness_pct & field_gaps",
            "measured": (
                f"{attr['completeness_pct']:.2f}% ({attr['tasks_fully_attributed']}/{attr['tasks_total']}), "
                f"gaps={attr['attribution_gaps']}, bridge={attr.get('bridge_driven')}, "
                f"neg={attr.get('negative_cases_caught', 0)}/{attr.get('negative_cases_total', 0)}, "
                f"dur_ms={attr['total_work_duration_ms']}, usd={attr['total_cost_usd']:.4f}"
            ),
            "threshold": "100.00%, gaps=0, unpriced=0, bridge=true, neg=12/12",
            "passed": (
                attr.get("bridge_driven") is True
                and attr.get("negative_cases_total", 0) >= 12
                and attr.get("negative_cases_caught", 0) == attr.get("negative_cases_total", 0)
                and attr["tasks_total"] > 0
                and attr["tasks_fully_attributed"] == attr["tasks_total"]
                and attr["attribution_gaps"] == 0
                and attr["unpriced_tokens"] == 0
                and attr["total_cost_usd"] > 0.0
                and attr["total_work_duration_ms"] > 0
            ),
        },
    ]

    all_passed = all(g["passed"] for g in gates)
    verdict = "GO" if all_passed else "NO-GO"

    scorecard = {
        "schema_version": 1,
        "generated_at_utc": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "verdict": verdict,
        "gates_passed": sum(1 for g in gates if g["passed"]),
        "gates_total": len(gates),
        "environment": {
            "source_revision": source_rev,
            "arch": arch,
            "host_cpu_cores": int(cpu_cores),
            "pg_mode": pg_mode,
            "cgroup_enforced": bool(isolated_1c1g),
            "cgroup_details": cgroup_details,
            "pg_version": pg_ver,
            "pg_version_num": int(pg_ver_num),
            "pg_max_connections": int(pg_max_conn),
            "pg_envelope": pg_envelope_label,
        },
        "pg_monitor": {
            "samples": len(valid_samples),
            "probe_errors": probe_errors,
            "peak_total_connections": peak_total_conn,
            "peak_active_connections": peak_active_conn,
            "peak_pg_rss_mib": peak_rss_mib,
            "pg_stat_deadlocks_delta": pg_deadlocks_delta,
        },
        "pg_restart_verify": pg_restart_verify,
        "gates": gates,
        "dimensions": {
            "dimension_a_d6_8_branches": d6_8,
            "dimension_a_d6_16_branches": d6_16,
            "dimension_b_and_c_control_plane": cp,
        },
    }
    return scorecard


def run_self_tests():
    def mk_d6(n):
        return {
            "operations": {
                "checkpoint_changed": {"p50_ms": 20.0, "p95_ms": 80.0, "p99_ms": 95.0, "n": n * 12},
                "merge_base": {"p50_ms": 40.0, "p95_ms": 90.0, "p99_ms": 98.0, "n": n * 2},
                "land_main": {"p50_ms": 6.0, "p95_ms": 11.0, "p99_ms": 13.0, "n": n},
            },
            "always_on_d6": {
                "max_bytes_per_landing": 1800,
                "bytes_per_landing": 1300.0,
                "expected_landings": n,
                "landings_completed": n,
                "merged_files_verified": n,
                "land_cas_retries": max(0, n - 1),
                "logical_written_bytes": n * 5_500_000,
                "cas_stored_bytes": n * 1_600_000,
                "dedup_ratio": 3.4,
                "cdc_shifted_files": n,
                "cdc_shifted_logical_bytes": n * 5_243_500,
                "cdc_unique_stored_bytes": n * 1_300_000 + 3_900_000,
                "cdc_fixed_block_unique_bytes": n * 5_243_500,
                "cdc_dedup_ratio": 3.1,
                "cdc_fixed_block_dedup_ratio": 1.0,
                "slo_cdc_dedup_passed": True,
                "crash_restart_drill": {
                    "verified": True,
                    "lost_refs": 0,
                    "lost_commits": 0,
                    "corrupt_objects": 0,
                    "sigkill_executed": True,
                    "exit_signal": 9,
                    "committed_before_kill": 6,
                    "journal_entries_before_commit": 10,
                    "journal_entries_after_commit": 11,
                    "torn_tail_present_before_commit": True,
                    "torn_tail_healed_on_disk": True,
                    "torn_tail_healed_and_committed": True,
                    "post_torn_commit_reconciled": True,
                    "reconcile_pre_last_journal_seq": 10,
                    "reconcile_post_entries_considered": 1,
                    "reconcile_post_last_journal_seq": 11,
                    "reconcile_converged_entries_considered": 0,
                    "verifier_pid": 99999,
                    "recovery_ms": 120.0,
                    "snapshots_verified": n + 1,
                    "blobs_verified": n * 4,
                },
            },
        }

    base_d6_8 = mk_d6(8)
    base_d6_16 = mk_d6(16)
    base_pg_restart = {
        "verified": True,
        "inject_torn_tail": True,
        "lost_refs": 0,
        "lost_commits": 0,
        "corrupt_objects": 0,
        "journal_entries_before_commit": 11,
        "journal_entries_after_commit": 12,
        "torn_tail_present_before_commit": True,
        "torn_tail_healed_on_disk": True,
        "torn_tail_healed_and_committed": True,
        "post_torn_commit_reconciled": True,
        "reconcile_pre_last_journal_seq": 11,
        "reconcile_post_entries_considered": 1,
        "reconcile_post_last_journal_seq": 12,
        "reconcile_converged_entries_considered": 0,
        "snapshots_verified": 17,
        "blobs_verified": 64,
        "verifier_pid": 88888,
    }
    base_cp = {
        "dimension_b_pg16": {
            "event_log_xid8": {
                "sessions": 16,
                "writer_pool": 8,
                "events_written": 1024,
                "events_target": 1024,
                "events_per_sec": 300.0,
                "out_of_order_reads": 0,
                "duplicate_reads": 0,
                "missing_reads": 0,
                "unexpected_reads": 0,
                "deadlocks": 0,
                "write_errors": 0,
                "read_errors": 0,
                "deterministic_commit_id_inversions": 4,
                "commit_id_inversions": 5,
                "xmin_holdbacks_attempted": 2,
                "xmin_holdbacks_verified": 2,
                "xmin_premature_leaks": 0,
                "append_latency": {"p50_ms": 4.0, "p95_ms": 80.0, "p99_ms": 95.0, "n": 1024},
                "read_global_latency": {"p50_ms": 5.0, "p95_ms": 70.0, "p99_ms": 90.0, "n": 40},
            },
            "sharded_bridge_claim": {
                "commands_claimed_unique": 256,
                "commands_target": 256,
                "claims_per_sec": 120.0,
                "duplicate_claims": 0,
                "unclaimed_commands": 0,
                "wrong_shard_claims": 0,
                "deadlocks": 0,
                "submit_errors": 0,
                "claim_errors": 0,
                "finish_errors": 0,
                "claim_latency": {"p50_ms": 3.0, "p95_ms": 40.0, "p99_ms": 50.0, "n": 256},
                "finish_latency": {"p50_ms": 2.0, "p95_ms": 30.0, "p99_ms": 40.0, "n": 256},
                "per_shard_counts": {"shard_0": 64, "shard_1": 64, "shard_2": 64, "shard_3": 64},
            },
        },
        "dimension_c_attribution": {
            "bridge_driven": True,
            "negative_cases_total": 12,
            "negative_cases_caught": 12,
            "tasks_total": 16,
            "tasks_fully_attributed": 16,
            "attribution_gaps": 0,
            "completeness_pct": 100.0,
            "total_work_duration_ms": 18,
            "total_tokens": 243200,
            "unpriced_tokens": 0,
            "total_cost_usd": 0.5,
        },
    }
    base_samples = [
        {"ok": True, "total_conn": 8, "active_conn": 4, "deadlocks_total": 0, "rss_kb": 150000},
        {"ok": True, "total_conn": 14, "active_conn": 10, "deadlocks_total": 0, "rss_kb": 420000},
        {"ok": True, "total_conn": 6, "active_conn": 1, "deadlocks_total": 0, "rss_kb": 200000},
    ]
    base_kw = dict(
        pg_mode="systemd",
        cgroup_enforced=True,
        cgroup_details="CPUQuota=100%, MemoryMax=1G, MemorySwapMax=0",
        pg_ver="16.16",
        pg_ver_num=160016,
        pg_max_conn=30,
        source_rev="deadbeef",
        arch="x86_64",
        cpu_cores=16,
    )

    ok_sc = evaluate_scorecard(
        base_d6_8, base_d6_16, base_pg_restart, base_cp, base_samples, **base_kw
    )
    assert ok_sc["verdict"] == "GO" and ok_sc["gates_passed"] == 12, ok_sc

    def expect_nogo(name, fn):
        d8 = copy.deepcopy(base_d6_8)
        d16 = copy.deepcopy(base_d6_16)
        pr = copy.deepcopy(base_pg_restart)
        c = copy.deepcopy(base_cp)
        sm = copy.deepcopy(base_samples)
        kw = copy.deepcopy(base_kw)
        expected_gate = fn(d8, d16, pr, c, sm, kw)
        sc = evaluate_scorecard(d8, d16, pr, c, sm, **kw)
        assert sc["verdict"] == "NO-GO", f"self-test {name} unexpectedly returned GO"
        gate_map = {g["id"]: g["passed"] for g in sc["gates"]}
        assert gate_map[expected_gate] is False, f"self-test {name} did not fail {expected_gate}"
        return sc

    expect_nogo("empty_samples", lambda d8, d16, pr, c, sm, kw: (sm.clear(), "PG16_RESOURCE_ENVELOPE")[1])
    expect_nogo("lt3_samples", lambda d8, d16, pr, c, sm, kw: (sm.pop(), "PG16_RESOURCE_ENVELOPE")[1])
    expect_nogo("probe_error", lambda d8, d16, pr, c, sm, kw: (sm.append({"ok": False, "error": "psql_failed"}), "PG16_RESOURCE_ENVELOPE")[1])
    expect_nogo("zero_rss", lambda d8, d16, pr, c, sm, kw: (sm.__setitem__(0, {"ok": True, "total_conn": 8, "active_conn": 1, "deadlocks_total": 0, "rss_kb": 0}), sm.__setitem__(1, {"ok": True, "total_conn": 8, "active_conn": 1, "deadlocks_total": 0, "rss_kb": 0}), sm.__setitem__(2, {"ok": True, "total_conn": 8, "active_conn": 1, "deadlocks_total": 0, "rss_kb": 0}), "PG16_RESOURCE_ENVELOPE")[3])
    sc_local = expect_nogo("pg_mode_local", lambda d8, d16, pr, c, sm, kw: (kw.__setitem__("pg_mode", "local"), "PG16_RESOURCE_ENVELOPE")[1])
    assert "UNCONSTRAINED" in sc_local["environment"]["pg_envelope"]
    sc_no_cg = expect_nogo("cgroup_unenforced", lambda d8, d16, pr, c, sm, kw: (kw.__setitem__("cgroup_enforced", False), "PG16_RESOURCE_ENVELOPE")[1])
    assert "UNCONSTRAINED" in sc_no_cg["environment"]["pg_envelope"]
    expect_nogo("pg_14_9", lambda d8, d16, pr, c, sm, kw: (kw.update({"pg_ver": "14.9", "pg_ver_num": 140009}), "PG16_RESOURCE_ENVELOPE")[1])
    expect_nogo("max_conn_100", lambda d8, d16, pr, c, sm, kw: (kw.__setitem__("pg_max_conn", 100), "PG16_RESOURCE_ENVELOPE")[1])
    expect_nogo("land_delta_gt_50mib", lambda d8, d16, pr, c, sm, kw: (d16["always_on_d6"].__setitem__("max_bytes_per_landing", 60_000_000), "D6_16_LAND_DELTA")[1])
    expect_nogo("partial_landings", lambda d8, d16, pr, c, sm, kw: (d16["always_on_d6"].__setitem__("landings_completed", 15), "D6_16_LAND_DELTA")[1])
    expect_nogo("low_cdc_dedup", lambda d8, d16, pr, c, sm, kw: (d16["always_on_d6"].__setitem__("cdc_dedup_ratio", 1.5), "D6_16_DEDUP_RATIO")[1])
    expect_nogo("fixed_block_cheating_corpus", lambda d8, d16, pr, c, sm, kw: (d8["always_on_d6"].__setitem__("cdc_fixed_block_dedup_ratio", 1.5), "D6_8_DEDUP_RATIO")[1])
    expect_nogo("missing_shifted_files", lambda d8, d16, pr, c, sm, kw: (d16["always_on_d6"].__setitem__("cdc_shifted_files", 15), "D6_16_DEDUP_RATIO")[1])
    expect_nogo("missing_sigkill", lambda d8, d16, pr, c, sm, kw: (d16["always_on_d6"]["crash_restart_drill"].__setitem__("sigkill_executed", False), "D6_CRASH_RESTART_ZERO_LOSS")[1])
    expect_nogo("failed_pg_restart", lambda d8, d16, pr, c, sm, kw: (pr.__setitem__("verified", False), "D6_CRASH_RESTART_ZERO_LOSS")[1])
    expect_nogo("missing_torn_tail_pg_restart", lambda d8, d16, pr, c, sm, kw: (pr.__setitem__("torn_tail_present_before_commit", False), "D6_CRASH_RESTART_ZERO_LOSS")[1])
    expect_nogo("missing_post_torn_heal", lambda d8, d16, pr, c, sm, kw: (d16["always_on_d6"]["crash_restart_drill"].__setitem__("torn_tail_healed_on_disk", False), "D6_CRASH_RESTART_ZERO_LOSS")[1])
    expect_nogo("missing_post_torn_reconcile", lambda d8, d16, pr, c, sm, kw: (d16["always_on_d6"]["crash_restart_drill"].__setitem__("post_torn_commit_reconciled", False), "D6_CRASH_RESTART_ZERO_LOSS")[1])
    expect_nogo("missing_inject_torn_pg_restart", lambda d8, d16, pr, c, sm, kw: (pr.__setitem__("inject_torn_tail", False), "D6_CRASH_RESTART_ZERO_LOSS")[1])
    expect_nogo("zero_xid8_inversions", lambda d8, d16, pr, c, sm, kw: (c["dimension_b_pg16"]["event_log_xid8"].__setitem__("commit_id_inversions", 0), "PG16_XID8_WATERMARK_INTEGRITY")[1])
    expect_nogo("zero_det_xid8_inversions", lambda d8, d16, pr, c, sm, kw: (c["dimension_b_pg16"]["event_log_xid8"].__setitem__("deterministic_commit_id_inversions", 0), "PG16_XID8_WATERMARK_INTEGRITY")[1])
    expect_nogo("zero_xmin_holdback", lambda d8, d16, pr, c, sm, kw: (c["dimension_b_pg16"]["event_log_xid8"].__setitem__("xmin_holdbacks_verified", 1), "PG16_XID8_WATERMARK_INTEGRITY")[1])
    expect_nogo("xmin_premature_leak", lambda d8, d16, pr, c, sm, kw: (c["dimension_b_pg16"]["event_log_xid8"].__setitem__("xmin_premature_leaks", 1), "PG16_XID8_WATERMARK_INTEGRITY")[1])
    expect_nogo("attribution_gap", lambda d8, d16, pr, c, sm, kw: (c["dimension_c_attribution"].__setitem__("attribution_gaps", 1), "ATTRIBUTION_COMPLETENESS")[1])
    expect_nogo("failed_attr_neg_check", lambda d8, d16, pr, c, sm, kw: (c["dimension_c_attribution"].__setitem__("negative_cases_caught", 11), "ATTRIBUTION_COMPLETENESS")[1])
    expect_nogo("insufficient_attr_neg_cases", lambda d8, d16, pr, c, sm, kw: (c["dimension_c_attribution"].update({"negative_cases_total": 8, "negative_cases_caught": 8}), "ATTRIBUTION_COMPLETENESS")[1])
    expect_nogo("non_bridge_attr", lambda d8, d16, pr, c, sm, kw: (c["dimension_c_attribution"].__setitem__("bridge_driven", False), "ATTRIBUTION_COMPLETENESS")[1])
    print("bench-always-on.sh self-test: 27/27 negative-falsification cases passed")


def render_markdown(scorecard):
    ev = scorecard["dimensions"]["dimension_b_and_c_control_plane"]["dimension_b_pg16"]["event_log_xid8"]
    sh = scorecard["dimensions"]["dimension_b_and_c_control_plane"]["dimension_b_pg16"]["sharded_bridge_claim"]
    attr = scorecard["dimensions"]["dimension_b_and_c_control_plane"]["dimension_c_attribution"]
    d6_8 = scorecard["dimensions"]["dimension_a_d6_8_branches"]
    d6_16 = scorecard["dimensions"]["dimension_a_d6_16_branches"]
    ao_8 = d6_8["always_on_d6"]
    ao_16 = d6_16["always_on_d6"]
    env = scorecard["environment"]
    mon = scorecard["pg_monitor"]
    pr = scorecard["pg_restart_verify"]
    verdict = scorecard["verdict"]

    def fmt_op(ops, key):
        op = ops.get(key)
        if not op:
            return "n/a"
        return f"p50={op['p50_ms']:.2f}ms / p95={op['p95_ms']:.2f}ms / p99={op['p99_ms']:.2f}ms (n={op['n']})"

    lines = [
        f"# Always-On Go/No-Go Scorecard — Verdict: **{verdict}** ({scorecard['gates_passed']}/{scorecard['gates_total']} gates passed)",
        "",
        f"- **Generated (UTC)**: `{scorecard['generated_at_utc']}`",
        f"- **Source Revision**: `{env['source_revision']}`",
        f"- **Host / PG16 Envelope**: `{env['arch']}` ({env['host_cpu_cores']} cores host), PG `{env['pg_version']}` in `{env['pg_mode']}` mode — `{env['pg_envelope']}` (`max_connections={env['pg_max_connections']}`)",
        f"- **PG16 Monitor (`{mon['samples']}` samples, `{mon['probe_errors']}` probe errors)**: peak `{mon['peak_total_connections']}/{env['pg_max_connections']}` connections (active peak `{mon['peak_active_connections']}`), RSS peak `{mon['peak_pg_rss_mib']:.2f} MiB`, PG deadlocks delta `{mon['pg_stat_deadlocks_delta']}`",
        "",
        "## Gate Matrix (Red-Line SLOs vs Measured)",
        "",
        "| Status | Gate | Dimension | Metric | Red-Line SLO | Measured |",
        "| :--- | :--- | :--- | :--- | :--- | :--- |",
    ]
    for g in scorecard["gates"]:
        status = "PASS" if g["passed"] else "FAIL"
        lines.append(
            f"| **{status}** | `{g['id']}` | {g['dimension']} | `{g['metric']}` | {g['threshold']} | {g['measured']} |"
        )

    lines.extend([
        "",
        "## Dimension Details",
        "",
        "### Dimension A · D6 (L1 tmpfs + FastCDC + CAS Merkle)",
        f"- **8 concurrent branches**: checkpoint={fmt_op(d6_8['operations'], 'checkpoint_changed')}; merge={fmt_op(d6_8['operations'], 'merge_base')}; land={fmt_op(d6_8['operations'], 'land_main')}; cas_retries=`{ao_8['land_cas_retries']}`; shift_cdc=`{ao_8.get('cdc_dedup_ratio', 0.0):.2f}x` vs fixed=`{ao_8.get('cdc_fixed_block_dedup_ratio', 0.0):.2f}x` (`{ao_8.get('cdc_shifted_files', 0)}` shifted files); total_dedup=`{ao_8['dedup_ratio']:.2f}x`; mean/max land delta=`{ao_8['bytes_per_landing']:.0f}`/`{ao_8['max_bytes_per_landing']}` B; crash-restart recovery=`{ao_8['crash_restart_drill']['recovery_ms']:.2f} ms` (SIGKILL=`{ao_8['crash_restart_drill']['sigkill_executed']}`, torn_before=`{ao_8['crash_restart_drill'].get('torn_tail_present_before_commit')}`, healed_disk=`{ao_8['crash_restart_drill'].get('torn_tail_healed_on_disk')}`, post_reconcile=`{ao_8['crash_restart_drill'].get('post_torn_commit_reconciled')}`, seq=`{ao_8['crash_restart_drill'].get('reconcile_pre_last_journal_seq', 0)}->{ao_8['crash_restart_drill'].get('reconcile_post_last_journal_seq', 0)}`, 0 loss across `{ao_8['crash_restart_drill']['snapshots_verified']}` snapshots / `{ao_8['crash_restart_drill']['blobs_verified']}` blobs)",
        f"- **16 concurrent branches**: checkpoint={fmt_op(d6_16['operations'], 'checkpoint_changed')}; merge={fmt_op(d6_16['operations'], 'merge_base')}; land={fmt_op(d6_16['operations'], 'land_main')}; cas_retries=`{ao_16['land_cas_retries']}`; shift_cdc=`{ao_16.get('cdc_dedup_ratio', 0.0):.2f}x` vs fixed=`{ao_16.get('cdc_fixed_block_dedup_ratio', 0.0):.2f}x` (`{ao_16.get('cdc_shifted_files', 0)}` shifted files); total_dedup=`{ao_16['dedup_ratio']:.2f}x`; mean/max land delta=`{ao_16['bytes_per_landing']:.0f}`/`{ao_16['max_bytes_per_landing']}` B; crash-restart recovery=`{ao_16['crash_restart_drill']['recovery_ms']:.2f} ms` (`seq={ao_16['crash_restart_drill'].get('reconcile_pre_last_journal_seq', 0)}->{ao_16['crash_restart_drill'].get('reconcile_post_last_journal_seq', 0)}`) + post-PG16-restart verified=`{pr['verified']}` (`inject_torn={pr.get('inject_torn_tail')}`, healed_disk=`{pr.get('torn_tail_healed_on_disk')}`, post_reconcile=`{pr.get('post_torn_commit_reconciled')}`, seq=`{pr.get('reconcile_pre_last_journal_seq', 0)}->{pr.get('reconcile_post_last_journal_seq', 0)}`, `{pr['snapshots_verified']}` snapshots / `{pr['blobs_verified']}` blobs, `{pr['lost_refs']}/{pr['lost_commits']}/{pr['corrupt_objects']}` loss)",
        "",
        "### Dimension B · Control Plane 1C/1G PG16 Concurrency & Sharding",
        f"- **`xid8` Watermark Stress**: `{ev['events_written']}/{ev['events_target']}` events across `{ev['sessions']}` sessions / `{ev['writer_pool']}` writers at `{ev['events_per_sec']:.1f} events/s`; append={fmt_op({'a': ev['append_latency']}, 'a')}; read_global={fmt_op({'r': ev['read_global_latency']}, 'r')}; xid8_reorders=`{ev.get('commit_id_inversions', 0)}`; xmin_holdbacks=`{ev.get('xmin_holdbacks_verified', 0)}/{ev.get('xmin_holdbacks_attempted', 0)}` (leaks=`{ev.get('xmin_premature_leaks', 0)}`)",
        f"- **4-Shard Bridge Claim**: `{sh['commands_claimed_unique']}/{sh['commands_target']}` commands claimed at `{sh['claims_per_sec']:.1f} claims/s`; claim={fmt_op({'c': sh['claim_latency']}, 'c')}; finish={fmt_op({'f': sh['finish_latency']}, 'f')}; per-shard=`{json.dumps(sh['per_shard_counts'], sort_keys=True)}`",
        "",
        "### Dimension C · Context & Work-Hours Attribution Completeness",
        f"- **Bridge-Driven Attribution**: `{attr['tasks_fully_attributed']}/{attr['tasks_total']}` tasks (`{attr['completeness_pct']:.2f}%`), bridge_driven=`{attr['bridge_driven']}`, negative_cases=`{attr.get('negative_cases_caught', 0)}/{attr.get('negative_cases_total', 0)}`, field gaps=`{attr['attribution_gaps']}`, work duration total=`{attr['total_work_duration_ms']} ms`, tokens=`{attr['total_tokens']}` (unpriced=`{attr['unpriced_tokens']}`), cost=`${attr['total_cost_usd']:.4f}`",
        "",
    ])
    return "\n".join(lines)


if sys.argv[1] == "--self-test":
    run_self_tests()
    sys.exit(0)

out_dir = pathlib.Path(sys.argv[1])
samples_path = pathlib.Path(sys.argv[2])
(
    pg_mode,
    cgroup_enforced_str,
    cgroup_details,
    pg_ver,
    pg_ver_num_str,
    pg_max_conn_str,
    source_rev,
    arch,
    cpu_cores,
) = sys.argv[3:12]

d6_8 = json.loads((out_dir / "d6_8.json").read_text())
d6_16 = json.loads((out_dir / "d6_16.json").read_text())
pg_restart_verify = json.loads((out_dir / "pg_restart_verify.json").read_text())
cp = json.loads((out_dir / "control_plane.json").read_text())

samples = []
if samples_path.exists():
    for line in samples_path.read_text().splitlines():
        line = line.strip()
        if line:
            samples.append(json.loads(line))

scorecard = evaluate_scorecard(
    d6_8,
    d6_16,
    pg_restart_verify,
    cp,
    samples,
    pg_mode=pg_mode,
    cgroup_enforced=(cgroup_enforced_str == "true"),
    cgroup_details=cgroup_details,
    pg_ver=pg_ver,
    pg_ver_num=int(pg_ver_num_str),
    pg_max_conn=int(pg_max_conn_str),
    source_rev=source_rev,
    arch=arch,
    cpu_cores=int(cpu_cores),
)
(out_dir / "scorecard.json").write_text(json.dumps(scorecard, indent=2, sort_keys=True) + "\n")
md = render_markdown(scorecard)
(out_dir / "scorecard.md").write_text(md)
print(md)
sys.exit(0 if scorecard["verdict"] == "GO" else 1)
PY
}

validate_docker_cgroup_limits() {
    local nano_cpus="$1"
    local mem_bytes="$2"
    local mem_swap="$3"
    if ! [[ "$nano_cpus" =~ ^[0-9]+$ ]] || [ "$nano_cpus" -le 0 ] || [ "$nano_cpus" -gt 1000000000 ]; then
        echo "bench-always-on: docker NanoCpus limit missing/exceeded ($nano_cpus)" >&2
        return 1
    fi
    if ! [[ "$mem_bytes" =~ ^[0-9]+$ ]] || [ "$mem_bytes" -le 0 ] || [ "$mem_bytes" -gt 1073741824 ]; then
        echo "bench-always-on: docker Memory limit missing/exceeded ($mem_bytes)" >&2
        return 1
    fi
    if ! [[ "$mem_swap" =~ ^-?[0-9]+$ ]] || [ "$mem_swap" -ne "$mem_bytes" ]; then
        echo "bench-always-on: docker MemorySwap limit missing/exceeded ($mem_swap != $mem_bytes)" >&2
        return 1
    fi
}

if [ "$SELF_TEST" -eq 1 ]; then
    validate_docker_cgroup_limits 1000000000 1073741824 1073741824
    if validate_docker_cgroup_limits 1000000000 1073741824 -1 2>/dev/null; then
        echo "bench-always-on self-test failed: accepted unlimited docker MemorySwap (-1)" >&2
        exit 1
    fi
    if validate_docker_cgroup_limits 1000000000 1073741824 0 2>/dev/null; then
        echo "bench-always-on self-test failed: accepted unset docker MemorySwap (0)" >&2
        exit 1
    fi
    if validate_docker_cgroup_limits 1000000000 1073741824 2147483648 2>/dev/null; then
        echo "bench-always-on self-test failed: accepted docker swap enabled (2G != 1G)" >&2
        exit 1
    fi
    if validate_docker_cgroup_limits 1000000000 1073741824 "" 2>/dev/null; then
        echo "bench-always-on self-test failed: accepted empty docker MemorySwap" >&2
        exit 1
    fi
    if validate_docker_cgroup_limits "" 1073741824 1073741824 2>/dev/null; then
        echo "bench-always-on self-test failed: accepted empty docker NanoCpus" >&2
        exit 1
    fi
    if validate_docker_cgroup_limits 1000000000 "<no value>" 1073741824 2>/dev/null; then
        echo "bench-always-on self-test failed: accepted <no value> docker Memory" >&2
        exit 1
    fi
    echo "bench-always-on.sh docker cgroup swap self-test: 7/7 cases passed"
    run_scorer_python --self-test
    exit 0
fi

if [ "$PG_MODE" = "auto" ]; then
    if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
        PG_MODE="docker"
    elif command -v systemd-run >/dev/null 2>&1 && systemd-run --user --scope true >/dev/null 2>&1; then
        PG_MODE="systemd"
    else
        PG_MODE="local"
    fi
fi

if [ "$PG_MODE" != "docker" ] && [ "$PG_MODE" != "systemd" ] && [ "$PG_MODE" != "local" ]; then
    echo "Invalid --pg-mode: $PG_MODE" >&2
    exit 2
fi

if [ "$PG_MODE" != "docker" ]; then
    for tool in initdb postgres pg_ctl pg_isready createdb psql; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            echo "bench-always-on: required PostgreSQL tool '$tool' not found in PATH" >&2
            exit 2
        fi
    done
fi

if [ -z "$OUT_DIR" ]; then
    OUT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/spawn-always-on-scorecard.XXXXXX")"
else
    mkdir -p "$OUT_DIR"
fi

PG_TMP_BASE="${TMPDIR:-/tmp}"
# Keep Unix domain socket path well below Linux 107-byte SUN_LEN limit.
if [ "${#PG_TMP_BASE}" -gt 50 ]; then
    PG_TMP_BASE="/tmp"
fi
WORK_TMP="$(mktemp -d "$PG_TMP_BASE/spawn-always-on-pg.XXXXXX")"
TMPFS_BASE="/tmp"
if [ -d /dev/shm ] && [ -w /dev/shm ]; then
    TMPFS_BASE="/dev/shm"
fi
TMPFS_8="$TMPFS_BASE/spawn-bench-l1-8-$$"
TMPFS_16="$TMPFS_BASE/spawn-bench-l1-16-$$"
PG_DOCKER_NAME="spawn-always-on-pg-$$"
PG_PID=""
MON_PID=""
PG_SCOPE_UNIT=""
PG_RESTART_SEQ=0
BENCH_URL=""
PSQL_TARGET=""
CGROUP_ENFORCED="false"
CGROUP_DETAILS="none"
KEPT_D6_SCRATCH=""
MON_LOG="$WORK_TMP/pg_samples.jsonl"

stop_monitor() {
    if [ -n "$MON_PID" ]; then
        kill "$MON_PID" 2>/dev/null || true
        wait "$MON_PID" 2>/dev/null || true
        MON_PID=""
    fi
}

cleanup() {
    set +e
    stop_monitor
    if [ "$PG_MODE" != "docker" ] && [ -f "$WORK_TMP/pgdata/postmaster.pid" ] && command -v pg_ctl >/dev/null 2>&1; then
        pg_ctl -D "$WORK_TMP/pgdata" -m fast stop >/dev/null 2>&1 \
            || pg_ctl -D "$WORK_TMP/pgdata" -m immediate stop >/dev/null 2>&1 \
            || true
    fi
    if [ -n "$PG_PID" ]; then
        kill -TERM "$PG_PID" 2>/dev/null || true
        for _ in $(seq 1 30); do
            kill -0 "$PG_PID" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL "$PG_PID" 2>/dev/null || true
        wait "$PG_PID" 2>/dev/null || true
    fi
    if [ "$PG_MODE" = "docker" ]; then
        docker rm -f -v "$PG_DOCKER_NAME" >/dev/null 2>&1 || true
    fi
    if [ -n "$PG_SCOPE_UNIT" ] && command -v systemctl >/dev/null 2>&1; then
        systemctl --user stop "$PG_SCOPE_UNIT" >/dev/null 2>&1 || true
    fi
    if [ -n "$KEPT_D6_SCRATCH" ]; then
        rm -rf "$KEPT_D6_SCRATCH" 2>/dev/null || true
    fi
    rm -rf "$WORK_TMP" "$TMPFS_8" "$TMPFS_16" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

if [ "$SKIP_BUILD" -eq 0 ]; then
    cargo build --offline --release -p spawn-host \
        --example storage_benchmark \
        --example always_on_control_plane_bench
fi

STORAGE_BIN="$REPO_ROOT/target/release/examples/storage_benchmark"
CP_BIN="$REPO_ROOT/target/release/examples/always_on_control_plane_bench"
if [ ! -x "$STORAGE_BIN" ] || [ ! -x "$CP_BIN" ]; then
    echo "bench-always-on: missing release benchmark binaries under target/release/examples" >&2
    exit 1
fi

PG_FLAGS=(
    -c "max_connections=30"
    -c "shared_buffers=128MB"
    -c "work_mem=4MB"
    -c "maintenance_work_mem=32MB"
    -c "effective_cache_size=512MB"
    -c "fsync=on"
    -c "synchronous_commit=on"
    -c "log_lock_waits=on"
)

verify_cgroup_limits() {
    case "$PG_MODE" in
        docker)
            local info
            info="$(docker inspect --format '{{.HostConfig.NanoCpus}},{{.HostConfig.Memory}},{{.HostConfig.MemorySwap}}' "$PG_DOCKER_NAME")"
            IFS=',' read -r nano_cpus mem_bytes mem_swap <<<"$info"
            validate_docker_cgroup_limits "$nano_cpus" "$mem_bytes" "$mem_swap" || exit 1
            CGROUP_ENFORCED="true"
            CGROUP_DETAILS="docker NanoCpus=$nano_cpus Memory=$mem_bytes MemorySwap=$mem_swap"
            ;;
        systemd)
            local pm_pid_file="$WORK_TMP/pgdata/postmaster.pid"
            if [ ! -f "$pm_pid_file" ]; then
                echo "bench-always-on: missing postmaster.pid at $pm_pid_file" >&2
                exit 1
            fi
            local pm_pid
            pm_pid="$(head -n 1 "$pm_pid_file" | tr -d '[:space:]')"
            local unit_props
            unit_props="$(systemctl --user show "$PG_SCOPE_UNIT" -p CPUQuotaPerSecUSec -p MemoryMax -p MemorySwapMax)"
            local details
            details="$(python3 - "$pm_pid" "$unit_props" <<'PY'
import pathlib, sys

pid = int(sys.argv[1])
props = {}
for line in sys.argv[2].splitlines():
    if "=" in line:
        k, v = line.split("=", 1)
        props[k.strip()] = v.strip()

mem_prop = props.get("MemoryMax", "")
swap_prop = props.get("MemorySwapMax", "")
cpu_prop = props.get("CPUQuotaPerSecUSec", "")
if not mem_prop.isdigit() or int(mem_prop) > 1073741824:
    raise SystemExit(f"systemd MemoryMax not enforced <= 1G: {props}")
if swap_prop != "0":
    raise SystemExit(f"systemd MemorySwapMax not 0: {props}")
if cpu_prop not in ("1s", "1000ms", "1000000us"):
    raise SystemExit(f"systemd CPUQuotaPerSecUSec not 1s: {props}")

cg_lines = pathlib.Path(f"/proc/{pid}/cgroup").read_text().splitlines()
v2_path = None
mem_v1_path = None
cpu_v1_path = None
for line in cg_lines:
    parts = line.split(":", 2)
    if len(parts) != 3:
        continue
    _, ctrls, rel = parts
    ctrl_list = ctrls.split(",") if ctrls else []
    if not ctrls:
        v2_path = rel
    if "memory" in ctrl_list:
        mem_v1_path = rel
    if "cpu" in ctrl_list:
        cpu_v1_path = rel

def read_first_existing(paths):
    for p in paths:
        if p.exists():
            return p.read_text().strip()
    return None

if v2_path is not None and (mem_v1_path is None and cpu_v1_path is None):
    cg_dir = pathlib.Path("/sys/fs/cgroup") / v2_path.lstrip("/")
    mem_max = read_first_existing([cg_dir / "memory.max"])
    cpu_max = read_first_existing([cg_dir / "cpu.max"])
    if not mem_max or mem_max == "max" or int(mem_max) > 1073741824:
        raise SystemExit(f"cgroup v2 memory.max invalid: {mem_max}")
    q_str, p_str = (cpu_max or "max 100000").split()[:2]
    if q_str == "max" or int(q_str) <= 0 or int(q_str) > int(p_str):
        raise SystemExit(f"cgroup v2 cpu.max invalid: {cpu_max}")
    print(f"systemd-v2 CPUQuota=100% ({cpu_max}), MemoryMax={mem_max}B, MemorySwapMax=0")
else:
    if mem_v1_path is None or cpu_v1_path is None:
        raise SystemExit(f"missing cgroup v1 memory/cpu controller paths in {cg_lines}")
    mem_file = pathlib.Path("/sys/fs/cgroup/memory") / mem_v1_path.lstrip("/") / "memory.limit_in_bytes"
    cpu_dir = pathlib.Path("/sys/fs/cgroup/cpu") / cpu_v1_path.lstrip("/")
    mem_lim = int(mem_file.read_text().strip())
    quota = int((cpu_dir / "cpu.cfs_quota_us").read_text().strip())
    period = int((cpu_dir / "cpu.cfs_period_us").read_text().strip())
    if mem_lim > 1073741824:
        raise SystemExit(f"cgroup v1 memory.limit_in_bytes={mem_lim} > 1G")
    if quota <= 0 or period <= 0 or quota > period:
        raise SystemExit(f"cgroup v1 cpu quota/period invalid: {quota}/{period}")
    print(f"systemd-v1 cfs={quota}/{period}us, memory.limit_in_bytes={mem_lim}B, MemorySwapMax=0")
PY
)"
            CGROUP_ENFORCED="true"
            CGROUP_DETAILS="$details"
            ;;
        local)
            CGROUP_ENFORCED="false"
            CGROUP_DETAILS="local unconstrained (taskset -c 0 only)"
            ;;
    esac
}

wait_postgres_ready() {
    local pgsock="$1"
    for _ in $(seq 1 60); do
        if [ "$PG_MODE" = "docker" ]; then
            if docker exec "$PG_DOCKER_NAME" pg_isready -U postgres -d spawn_bench >/dev/null 2>&1; then
                return 0
            fi
        else
            if pg_isready -h "$pgsock" -U postgres >/dev/null 2>&1 && [ -f "$WORK_TMP/pgdata/postmaster.pid" ]; then
                return 0
            fi
        fi
        sleep 0.2
    done
    echo "bench-always-on: PostgreSQL failed to become ready in mode=$PG_MODE" >&2
    if [ -f "$WORK_TMP/pg.log" ]; then
        tail -n 30 "$WORK_TMP/pg.log" >&2 || true
    fi
    exit 1
}

launch_postgres_process() {
    local pgdata="$WORK_TMP/pgdata"
    local pgsock="$WORK_TMP/pgsock"
    local pglog="$WORK_TMP/pg.log"
    if [ "$PG_MODE" = "systemd" ]; then
        PG_RESTART_SEQ=$((PG_RESTART_SEQ + 1))
        PG_SCOPE_UNIT="spawn-bench-pg16-$$-${PG_RESTART_SEQ}.scope"
        systemd-run --user --scope --quiet --unit="$PG_SCOPE_UNIT" \
            -p "CPUQuota=100%" \
            -p "MemoryMax=1G" \
            -p "MemorySwapMax=0" \
            postgres -D "$pgdata" -h "" -k "$pgsock" "${PG_FLAGS[@]}" \
            >>"$pglog" 2>&1 &
        PG_PID=$!
    else
        taskset -c 0 postgres -D "$pgdata" -h "" -k "$pgsock" "${PG_FLAGS[@]}" \
            >>"$pglog" 2>&1 &
        PG_PID=$!
    fi
}

start_postgres() {
    case "$PG_MODE" in
        docker)
            local port
            port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
            docker run -d \
                --name "$PG_DOCKER_NAME" \
                --cpus=1 \
                --memory=1g \
                --memory-swap=1g \
                -p "127.0.0.1:${port}:5432" \
                -e POSTGRES_HOST_AUTH_METHOD=trust \
                -e POSTGRES_USER=postgres \
                -e POSTGRES_DB=spawn_bench \
                postgres:16 \
                "${PG_FLAGS[@]}" >/dev/null
            BENCH_URL="postgres://postgres@127.0.0.1:${port}/spawn_bench"
            wait_postgres_ready ""
            PSQL_TARGET="$BENCH_URL"
            verify_cgroup_limits
            ;;
        systemd|local)
            local pgdata="$WORK_TMP/pgdata"
            local pgsock="$WORK_TMP/pgsock"
            mkdir -p "$pgsock"
            chmod 700 "$pgsock"
            initdb -D "$pgdata" -U postgres -A trust --encoding=UTF8 --locale=C >/dev/null
            launch_postgres_process
            wait_postgres_ready "$pgsock"
            createdb -h "$pgsock" -U postgres spawn_bench
            BENCH_URL="postgres://postgres@localhost/spawn_bench?host=${pgsock}"
            PSQL_TARGET="-h $pgsock -U postgres -d spawn_bench"
            verify_cgroup_limits
            ;;
    esac
}

stop_postgres_preserve_pgdata() {
    if [ "$PG_MODE" = "docker" ]; then
        docker stop "$PG_DOCKER_NAME" >/dev/null
    else
        pg_ctl -D "$WORK_TMP/pgdata" -m fast stop >/dev/null
        if [ -n "$PG_PID" ]; then
            wait "$PG_PID" 2>/dev/null || true
            PG_PID=""
        fi
        if [ -n "$PG_SCOPE_UNIT" ] && command -v systemctl >/dev/null 2>&1; then
            systemctl --user stop "$PG_SCOPE_UNIT" >/dev/null 2>&1 || true
            PG_SCOPE_UNIT=""
        fi
    fi
}

restart_postgres_preserved() {
    if [ "$PG_MODE" = "docker" ]; then
        docker start "$PG_DOCKER_NAME" >/dev/null
        wait_postgres_ready ""
    else
        launch_postgres_process
        wait_postgres_ready "$WORK_TMP/pgsock"
    fi
    verify_cgroup_limits
}

run_psql_scalar() {
    local sql="$1"
    if [ "$PG_MODE" = "docker" ]; then
        docker exec "$PG_DOCKER_NAME" psql -X -U postgres -d spawn_bench -v ON_ERROR_STOP=1 -Atqc "$sql"
    else
        # shellcheck disable=SC2086
        psql -X $PSQL_TARGET -v ON_ERROR_STOP=1 -Atqc "$sql"
    fi
}

sample_pg_once() {
    local err_file="$WORK_TMP/psql_probe.err"
    local stats=""
    local rc=0
    local probe_sql="select coalesce(count(*),0)::text || ',' || coalesce(count(*) filter (where state = 'active'),0)::text || ',' || coalesce((select sum(deadlocks) from pg_stat_database where datname='spawn_bench'),0)::text || ',' || coalesce((select pid from pg_stat_activity where pid = pg_backend_pid() limit 1),0)::text from pg_stat_activity where datname='spawn_bench';"
    set +e
    stats="$(run_psql_scalar "$probe_sql" 2>"$err_file")"
    rc=$?
    set -e
    if [ "$rc" -ne 0 ] || ! [[ "$stats" =~ ^[0-9]+,[0-9]+,[0-9]+,[0-9]+$ ]]; then
        local err_msg
        err_msg="$(tr '\n' ' ' <"$err_file" | head -c 240)"
        python3 -c 'import json, sys; print(json.dumps({"ok": False, "rc": int(sys.argv[1]), "error": sys.argv[2]}))' "$rc" "$err_msg" >>"$MON_LOG"
        return 0
    fi

    local rss_kb="0"
    if [ "$PG_MODE" = "docker" ]; then
        rss_kb="$(docker stats --no-stream --format '{{.MemUsage}}' "$PG_DOCKER_NAME" 2>/dev/null | python3 -c '
import re, sys
s = sys.stdin.read().strip().split("/")[0].strip()
m = re.match(r"([0-9.]+)\s*([KMGT]?i?B)", s)
if not m:
    print(0)
else:
    v, u = float(m.group(1)), m.group(2)
    mult = {"B": 1/1024, "KiB": 1, "KB": 1000/1024, "MiB": 1024, "MB": 1000, "GiB": 1024*1024, "GB": 1000*1000}.get(u, 0)
    print(int(v * mult))
')"
    elif [ -d "$WORK_TMP/pgdata" ]; then
        rss_kb="$(python3 - "$WORK_TMP/pgdata" <<'PY'
import glob, os, sys
pgdata = os.path.realpath(sys.argv[1])
pids = set()
pm_pid = os.path.join(pgdata, "postmaster.pid")
try:
    with open(pm_pid) as f:
        root_pid = int(f.readline().strip())
    pids.add(root_pid)
except Exception:
    pass
if pids:
    children = {}
    for st in glob.glob("/proc/[0-9]*/stat"):
        try:
            parts = open(st).read().split()
            pid = int(parts[0])
            ppid = int(parts[3])
            children.setdefault(ppid, []).append(pid)
        except Exception:
            pass
    stack = list(pids)
    while stack:
        cur = stack.pop()
        for ch in children.get(cur, []):
            if ch not in pids:
                pids.add(ch)
                stack.append(ch)
rss = 0
for pid in pids:
    try:
        for line in open(f"/proc/{pid}/status"):
            if line.startswith("VmRSS:"):
                rss += int(line.split()[1])
                break
    except Exception:
        pass
print(rss)
PY
)"
    fi
    IFS=',' read -r total_conn active_conn deadlocks_total _unused <<<"$stats"
    printf '{"ok":true,"total_conn":%s,"active_conn":%s,"deadlocks_total":%s,"rss_kb":%s}\n' \
        "$total_conn" "$active_conn" "$deadlocks_total" "${rss_kb:-0}" >>"$MON_LOG"
}

monitor_pg_loop() {
    while true; do
        sample_pg_once
        sleep 0.5
    done
}

start_monitor() {
    sample_pg_once
    monitor_pg_loop &
    MON_PID=$!
}

start_postgres
: >"$MON_LOG"
start_monitor

SOURCE_REV="$(git rev-parse HEAD)"

echo "[1/4] Running Dimension A (8-branch D6 L1+FastCDC+CrashDrill)..." >&2
SPAWN_BENCH_DATABASE_URL="$BENCH_URL" "$STORAGE_BIN" \
    --backend d6 \
    --concurrency 8 \
    --pg-pool-size 8 \
    --tmpfs-root "$TMPFS_8" \
    --tmpfs-budget-mb 2048 \
    --iterations "$D6_ITERATIONS" \
    --warmup "$D6_WARMUP" \
    --files "$D6_FILES" \
    --file-bytes "$D6_FILE_BYTES" \
    --edits 4 \
    --oversized-file-bytes "$FASTCDC_BYTES" \
    --land-branches \
    --crash-restart-drill \
    --source-revision "$SOURCE_REV" \
    >"$OUT_DIR/d6_8.json"

echo "[2/4] Running Dimension A (16-branch D6 L1+FastCDC+CrashDrill + Kept State)..." >&2
SPAWN_BENCH_DATABASE_URL="$BENCH_URL" "$STORAGE_BIN" \
    --backend d6 \
    --concurrency 16 \
    --pg-pool-size "$PG_POOL_SIZE" \
    --tmpfs-root "$TMPFS_16" \
    --tmpfs-budget-mb 2048 \
    --iterations "$D6_ITERATIONS" \
    --warmup "$D6_WARMUP" \
    --files "$D6_FILES" \
    --file-bytes "$D6_FILE_BYTES" \
    --edits 4 \
    --oversized-file-bytes "$FASTCDC_BYTES" \
    --land-branches \
    --crash-restart-drill \
    --keep \
    --save-crash-manifest "$WORK_TMP/d6_16_crash_manifest.json" \
    --source-revision "$SOURCE_REV" \
    >"$OUT_DIR/d6_16.json"
KEPT_D6_SCRATCH="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["environment"]["scratch_root"])' "$OUT_DIR/d6_16.json")"

echo "[3/4] Restarting PG16 & Re-Verifying D6 Crash State in Fresh Process..." >&2
stop_monitor
stop_postgres_preserve_pgdata
restart_postgres_preserved
start_monitor
SPAWN_BENCH_DATABASE_URL="$BENCH_URL" "$STORAGE_BIN" \
    --internal-crash-verify --inject-torn-tail "$WORK_TMP/d6_16_crash_manifest.json" \
    >"$OUT_DIR/pg_restart_verify.json"
if [ -n "$KEPT_D6_SCRATCH" ]; then
    rm -rf "$KEPT_D6_SCRATCH"
    KEPT_D6_SCRATCH=""
fi

echo "[4/4] Running Dimensions B & C (1C/1G PG16 + Sharded Claim + Bridge Attribution)..." >&2
SPAWN_BENCH_DATABASE_URL="$BENCH_URL" "$CP_BIN" \
    --sessions 16 \
    --events-per-session 64 \
    --shards 4 \
    --commands-per-session 16 \
    --writer-pool 8 \
    --attribution-tasks 16 \
    >"$OUT_DIR/control_plane.json"

stop_monitor
sample_pg_once

PG_VER="$(run_psql_scalar "show server_version;")"
PG_VER_NUM="$(run_psql_scalar "show server_version_num;")"
PG_MAX_CONN="$(run_psql_scalar "show max_connections;")"
if [ -z "$PG_VER" ] || ! [[ "$PG_VER_NUM" =~ ^[0-9]+$ ]] || ! [[ "$PG_MAX_CONN" =~ ^[0-9]+$ ]]; then
    echo "bench-always-on: failed to read PostgreSQL version/max_connections metadata" >&2
    exit 1
fi
UNAME_ARCH="$(uname -m)"
CPU_CORES="$(nproc 2>/dev/null || echo 1)"

run_scorer_python \
    "$OUT_DIR" \
    "$MON_LOG" \
    "$PG_MODE" \
    "$CGROUP_ENFORCED" \
    "$CGROUP_DETAILS" \
    "$PG_VER" \
    "$PG_VER_NUM" \
    "$PG_MAX_CONN" \
    "$SOURCE_REV" \
    "$UNAME_ARCH" \
    "$CPU_CORES"
