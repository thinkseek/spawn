#!/usr/bin/env python3
"""Compare independent Spawn storage benchmark reports.

Usage:
  python3 -I scripts/storage-benchmark-compare.py \
    --baseline baseline-1.json baseline-2.json \
    --candidate candidate-1.json candidate-2.json

The configurations must match. The comparison uses the median across runs of
each operation's p50 and p95, not one hand-picked run. Exit 1 when an operation's
median p50 regresses by more than --max-regression-percent (default 10).
"""

import argparse
import json
import statistics
import sys


def load(paths):
    reports = []
    for path in paths:
        with open(path, "r") as source:
            report = json.load(source)
        if report.get("schema_version") != 1 or report.get("benchmark") != "spawn-storage":
            raise ValueError("%s is not a Spawn storage benchmark report" % path)
        totals = report.get("totals", {})
        if totals.get("git_fsck") != "passed" and totals.get("integrity") != "passed":
            raise ValueError("%s did not pass its integrity check" % path)
        reports.append(report)
    return reports


def median(reports, operation, field):
    return statistics.median(
        report["operations"][operation][field]
        for report in reports
        if operation in report["operations"]
    )


def comparable_config(report, cross_backend):
    cfg = dict(report["config"])
    if cross_backend:
        for key in ("backend", "tmpfs_budget_mb", "database"):
            cfg.pop(key, None)
        cfg.setdefault("concurrency", 1)
    return cfg


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", nargs="+", required=True)
    parser.add_argument("--candidate", nargs="+", required=True)
    parser.add_argument("--max-regression-percent", type=float, default=10.0)
    parser.add_argument("--allow-cross-backend", action="store_true")
    args = parser.parse_args()
    baseline = load(args.baseline)
    candidate = load(args.candidate)
    base_cfg = comparable_config(baseline[0], args.allow_cross_backend)
    if any(
        comparable_config(report, args.allow_cross_backend) != base_cfg
        for report in baseline + candidate
    ):
        raise ValueError("all baseline and candidate configurations must match")

    reports = baseline + candidate
    operation_sets = [set(report["operations"]) for report in reports]
    if args.allow_cross_backend:
        expected_operations = set.intersection(*operation_sets)
        if not expected_operations:
            raise ValueError("reports share no common operations")
    else:
        expected_operations = operation_sets[0]
        for path, operations in zip(args.baseline + args.candidate, operation_sets):
            if operations != expected_operations:
                missing = sorted(expected_operations - operations)
                extra = sorted(operations - expected_operations)
                raise ValueError(
                    "%s has different operations (missing=%s extra=%s)" % (path, missing, extra)
                )
    if not args.allow_cross_backend:
        git_versions = [report.get("environment", {}).get("git_version") for report in reports]
        if any(not version for version in git_versions) or len(set(git_versions)) != 1:
            raise ValueError("all reports must record the same environment.git_version")
    for group, paths in ((baseline, args.baseline), (candidate, args.candidate)):
        revisions = {report.get("environment", {}).get("spawn_source_revision") for report in group}
        binaries = {report.get("environment", {}).get("benchmark_binary_sha256") for report in group}
        if not args.allow_cross_backend and (
            len(revisions) != 1 or None in revisions or len(binaries) != 1 or None in binaries
        ):
            raise ValueError(
                "each group must record one Spawn source revision and binary SHA-256: %s" % paths
            )
    identity = lambda report: (
        report["environment"].get("spawn_source_revision"),
        report["environment"].get("benchmark_binary_sha256"),
        report["environment"].get("backend", report["config"].get("backend", "git")),
    )
    if not args.allow_cross_backend and identity(baseline[0]) == identity(candidate[0]):
        raise ValueError("baseline and candidate reports name the same source and benchmark binary")
    operations = sorted(expected_operations)
    print("%-24s %12s %12s %10s" % ("operation", "baseline", "candidate", "change"))
    failures = []
    for operation in operations:
        old = median(baseline, operation, "p50_ms")
        new = median(candidate, operation, "p50_ms")
        change = (new - old) / old * 100.0 if old else 0.0
        old_p95 = median(baseline, operation, "p95_ms")
        new_p95 = median(candidate, operation, "p95_ms")
        print(
            "%-24s %9.3f ms %9.3f ms %+9.1f%%  (p95 %.3f -> %.3f ms)"
            % (operation, old, new, change, old_p95, new_p95)
        )
        if change > args.max_regression_percent:
            failures.append(operation)

    old_speed = statistics.median(
        report["totals"]["measured_cycles_per_second"] for report in baseline
    )
    new_speed = statistics.median(
        report["totals"]["measured_cycles_per_second"] for report in candidate
    )
    speed_change = (new_speed - old_speed) / old_speed * 100.0
    print(
        "%-24s %9.3f/s  %9.3f/s %+9.1f%%"
        % ("measured cycle throughput", old_speed, new_speed, speed_change)
    )
    if speed_change < -args.max_regression_percent:
        failures.append("measured cycle throughput")

    if args.allow_cross_backend:
        def report_disk_bytes(report):
            disk = report.get("disk", {})
            if "total_bytes" in disk:
                return disk["total_bytes"]
            return sum(
                section.get("bytes", 0)
                for section in disk.values()
                if isinstance(section, dict)
            )

        old_disk = statistics.median(report_disk_bytes(r) for r in baseline)
        new_disk = statistics.median(report_disk_bytes(r) for r in candidate)
        disk_change = (new_disk - old_disk) / old_disk * 100.0 if old_disk else 0.0
        print(
            "%-24s %9.0f B   %9.0f B  %+9.1f%%"
            % ("total disk bytes", old_disk, new_disk, disk_change)
        )

    if failures:
        print("regression over %.1f%%: %s" % (
            args.max_regression_percent, ", ".join(failures)), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print("storage benchmark compare: %s" % error, file=sys.stderr)
        sys.exit(2)
