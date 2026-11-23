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
        if report.get("totals", {}).get("git_fsck") != "passed":
            raise ValueError("%s did not pass its integrity check" % path)
        reports.append(report)
    return reports


def median(reports, operation, field):
    return statistics.median(
        report["operations"][operation][field]
        for report in reports
        if operation in report["operations"]
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", nargs="+", required=True)
    parser.add_argument("--candidate", nargs="+", required=True)
    parser.add_argument("--max-regression-percent", type=float, default=10.0)
    args = parser.parse_args()
    baseline = load(args.baseline)
    candidate = load(args.candidate)
    if any(report["config"] != baseline[0]["config"] for report in baseline + candidate):
        raise ValueError("all baseline and candidate configurations must match")

    operations = sorted(
        set.intersection(*(set(report["operations"]) for report in baseline + candidate))
    )
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
