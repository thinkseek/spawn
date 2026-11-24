#!/usr/bin/env python3
"""Large, real-agent storage benchmark for a running Spawn fleet.

Generate a deterministic golden repository, then run one ephemeral task per
agent. Each agent performs iterations in separate tool calls, so Spawn records
real tool and checkpoint timings. The runner validates results from the agent
branches, samples host pressure, stops the executions, and writes a JSON report.

The live store is used deliberately for `run`: this is a fleet E2E stress test.
Use a new local golden repository and an output directory that does not exist.
No network access is needed by the workload itself.
"""

import argparse
import collections
import hashlib
import json
import os
import pathlib
import re
import shutil
import statistics
import subprocess
import sys
import threading
import time

GOLDEN_VERSION = 1
DEFAULT_FILE_KIB = 256
DEFAULT_SHARDS = 32
DEFAULT_FILES_PER_SHARD = 64
OUTPUT_BYTES = 1024 * 1024

WORKLOAD_SOURCE = r'''#!/usr/bin/env python3
"""Deterministic golden-shard reader used inside every agent workspace."""
import argparse
import hashlib
import json
import os
import pathlib
import time

OUTPUT_BYTES = 1024 * 1024


def stream(label, size):
    out = bytearray()
    counter = 0
    encoded = label.encode("utf-8")
    while len(out) < size:
        out.extend(hashlib.sha256(encoded + b"|" + str(counter).encode("ascii")).digest())
        counter += 1
    return bytes(out[:size])


def file_hash(path):
    digest = hashlib.sha256()
    with open(str(path), "rb") as source:
        while True:
            chunk = source.read(1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


def durable_write(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".partial")
    with open(str(temporary), "wb") as target:
        target.write(data)
        target.flush()
        os.fsync(target.fileno())
    os.replace(str(temporary), str(path))
    directory = os.open(str(path.parent), os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--worker", type=int, required=True)
    parser.add_argument("--iteration", type=int, required=True)
    args = parser.parse_args()
    began = time.monotonic()
    root = pathlib.Path(__file__).resolve().parent.parent
    manifest = json.loads((root / "golden/manifest.json").read_text())
    shard = manifest["shards"][args.worker]
    digest = hashlib.sha256()
    total = 0
    for entry in shard["files"]:
        path = root / "golden" / entry["path"]
        actual = file_hash(path)
        if actual != entry["sha256"]:
            raise SystemExit("golden digest mismatch: %s" % entry["path"])
        total += path.stat().st_size
        digest.update(entry["path"].encode("utf-8") + b"\0")
        digest.update(bytes.fromhex(actual))
    shard_hash = digest.hexdigest()
    if total != shard["bytes"] or shard_hash != shard["sha256"]:
        raise SystemExit("golden shard summary mismatch")

    label = "spawn-cluster-output-v1|worker=%d|iteration=%d|%s" % (
        args.worker, args.iteration, shard_hash)
    artifact = stream(label, OUTPUT_BYTES)
    artifact_hash = hashlib.sha256(artifact).hexdigest()
    directory = root / "results" / ("worker-%03d" % args.worker)
    durable_write(directory / ("iter-%03d.bin" % args.iteration), artifact)
    result = {
        "schema_version": 1,
        "worker": args.worker,
        "iteration": args.iteration,
        "shard": args.worker,
        "files": len(shard["files"]),
        "bytes": total,
        "sha256": shard_hash,
        "artifact_bytes": OUTPUT_BYTES,
        "artifact_sha256": artifact_hash,
        "workload_ms": round((time.monotonic() - began) * 1000.0, 3),
        "status": "ok",
    }
    durable_write(
        directory / ("iter-%03d.json" % args.iteration),
        (json.dumps(result, sort_keys=True, indent=2) + "\n").encode("utf-8"),
    )
    print("GOLDEN_OK worker=%d iteration=%d bytes=%d elapsed_ms=%.3f" % (
        args.worker, args.iteration, total, (time.monotonic() - began) * 1000.0))


if __name__ == "__main__":
    main()
'''


def stream(label, size):
    out = bytearray()
    counter = 0
    encoded = label.encode("utf-8")
    while len(out) < size:
        out.extend(hashlib.sha256(encoded + b"|" + str(counter).encode("ascii")).digest())
        counter += 1
    return bytes(out[:size])


def run_command(arguments, binary=False, timeout=None):
    result = subprocess.run(
        arguments,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
    )
    if result.returncode != 0:
        stderr = result.stderr.decode("utf-8", "replace").strip()
        raise RuntimeError(
            "command failed (%s): %s: %s"
            % (result.returncode, " ".join(arguments[:3]), stderr[-2000:])
        )
    return result.stdout if binary else result.stdout.decode("utf-8")


def git(source, arguments, binary=False):
    return run_command(["git", "--git-dir", str(source)] + arguments, binary=binary)


def new_directory(path):
    path = path.absolute()
    path.mkdir(mode=0o700)
    return path


def generate(args):
    root = new_directory(args.root)
    seed = root / "seed"
    source = root / "golden.git"
    (seed / "tools").mkdir(parents=True)
    (seed / "tools/golden_workload.py").write_text(WORKLOAD_SOURCE)
    os.chmod(str(seed / "tools/golden_workload.py"), 0o755)
    manifest = {
        "schema_version": GOLDEN_VERSION,
        "shards": [],
        "file_bytes": args.file_kib * 1024,
    }
    dataset = hashlib.sha256()
    total = 0
    began = time.monotonic()
    for shard_index in range(args.shards):
        shard = {"shard": shard_index, "bytes": 0, "files": []}
        shard_digest = hashlib.sha256()
        for file_index in range(args.files_per_shard):
            relative = "shards/%03d/%04d.bin" % (shard_index, file_index)
            path = seed / "golden" / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            label = "spawn-golden-v1|shard=%d|file=%d" % (shard_index, file_index)
            data = stream(label, args.file_kib * 1024)
            path.write_bytes(data)
            actual = hashlib.sha256(data).hexdigest()
            entry = {"path": relative, "bytes": len(data), "sha256": actual}
            shard["files"].append(entry)
            shard["bytes"] += len(data)
            shard_digest.update(relative.encode("utf-8") + b"\0")
            shard_digest.update(bytes.fromhex(actual))
        shard["sha256"] = shard_digest.hexdigest()
        manifest["shards"].append(shard)
        dataset.update(bytes.fromhex(shard["sha256"]))
        total += shard["bytes"]
        print("generate: shard %d/%d" % (shard_index + 1, args.shards), file=sys.stderr)
    manifest["bytes"] = total
    manifest["sha256"] = dataset.hexdigest()
    (seed / "golden/manifest.json").write_text(json.dumps(manifest, sort_keys=True, indent=2) + "\n")
    (seed / "README.md").write_text(
        "# Spawn storage cluster golden dataset\n\n"
        "Deterministic incompressible files with per-file and per-shard SHA-256.\n"
        "Agents run `tools/golden_workload.py` and write only under `results/`.\n"
    )
    run_command(["git", "-C", str(seed), "init", "--quiet"])
    run_command(["git", "-C", str(seed), "config", "user.name", "spawn-storage-benchmark"])
    run_command(["git", "-C", str(seed), "config", "user.email", "spawn-storage@localhost"])
    run_command(["git", "-C", str(seed), "add", "-A"])
    run_command(["git", "-C", str(seed), "commit", "--quiet", "-m", "Golden storage cluster dataset"])
    run_command(["git", "-C", str(seed), "branch", "-M", "main"])
    run_command(["git", "clone", "--bare", "--quiet", str(seed), str(source)])
    config = {
        "schema_version": 1,
        "source": str(source),
        "bytes": total,
        "sha256": manifest["sha256"],
        "shards": args.shards,
        "files_per_shard": args.files_per_shard,
        "file_kib": args.file_kib,
        "generation_seconds": time.monotonic() - began,
    }
    (root / "golden-config.json").write_text(json.dumps(config, indent=2) + "\n")
    print(json.dumps(config, indent=2))


def spawn_binary():
    configured = os.environ.get("SPAWN_BIN")
    if configured:
        return configured
    return shutil.which("spawn") or str(pathlib.Path.home() / ".local/share/spawn-rsi/bin/spawn")


def spawn_json(arguments):
    return json.loads(run_command([spawn_binary()] + arguments))


def brief(worker, iterations):
    return (
        "You are storage load worker %d. Work only in this repository. Do not use network, "
        "subagents, or paid model tools. Do not modify golden/ or tools/. "
        "For iteration numbers 0 through %d, run exactly this command with the iteration "
        "number substituted, using a separate Bash tool call for every iteration and never "
        "combining iterations into one shell loop: "
        "python3 -I tools/golden_workload.py --worker %d --iteration N. "
        "Wait for each command to succeed before starting the next. After all iterations, "
        "write .spawn/report.md with an Outcome of done, list the completed iterations and "
        "the bytes verified. Then reply STORAGE_CLUSTER_WORKER_%03d_DONE."
    ) % (worker, iterations - 1, worker, worker)


def iteration_brief(worker, iteration):
    return (
        "Storage worker %d: perform only iteration %d now. Use one Bash tool call to run "
        "python3 -I tools/golden_workload.py --worker %d --iteration %d. "
        "Do not run any other iteration, use a shell loop, modify golden/ or tools/, use "
        "network, or start subagents. Do not write .spawn/report.md. After the command "
        "succeeds, reply STORAGE_CLUSTER_ITERATION_%d_DONE and wait for the next input."
    ) % (worker, iteration, worker, iteration, iteration)


def finalize_brief(worker, iterations):
    return (
        "Storage worker %d: all %d workload iterations are complete. Do not run the workload "
        "again. Write .spawn/report.md now with Outcome done and the completed iteration and "
        "byte counts, then reply STORAGE_CLUSTER_WORKER_%03d_DONE."
    ) % (worker, iterations, worker)


class HostMonitor(threading.Thread):
    def __init__(self, path, root):
        super().__init__(daemon=True)
        self.path = path
        self.root = root
        self.stop_event = threading.Event()
        self.previous_cpu = None
        self.previous_disk = None

    def stop(self):
        self.stop_event.set()

    def run(self):
        with open(str(self.path), "a") as output:
            while not self.stop_event.is_set():
                sample = {"at_ms": int(time.time() * 1000)}
                try:
                    with open("/proc/loadavg") as source:
                        fields = source.read().split()
                    sample["load1"] = float(fields[0])
                    with open("/proc/meminfo") as source:
                        memory = dict(
                            (line.split(":", 1)[0], int(line.split()[1]) * 1024)
                            for line in source if ":" in line and line.split()[1].isdigit()
                        )
                    sample["memory_available_bytes"] = memory.get("MemAvailable")
                    stat = os.statvfs(str(self.root))
                    sample["free_bytes"] = stat.f_bavail * stat.f_frsize
                    with open("/proc/stat") as source:
                        cpu = [int(value) for value in source.readline().split()[1:]]
                    if self.previous_cpu:
                        total = sum(cpu) - sum(self.previous_cpu)
                        idle = (cpu[3] + cpu[4]) - (self.previous_cpu[3] + self.previous_cpu[4])
                        sample["cpu_busy_percent"] = max(0.0, 100.0 * (total - idle) / total) if total else 0.0
                    self.previous_cpu = cpu
                    read_sectors = write_sectors = io_ms = 0
                    with open("/proc/diskstats") as source:
                        for line in source:
                            fields = line.split()
                            if (len(fields) >= 14 and fields[2].startswith(("vda", "nvme", "sda"))
                                    and not os.path.exists("/sys/class/block/%s/partition" % fields[2])):
                                read_sectors += int(fields[5])
                                write_sectors += int(fields[9])
                                io_ms += int(fields[12])
                    disk = (read_sectors, write_sectors, io_ms)
                    if self.previous_disk:
                        sample["disk_read_bytes"] = max(0, disk[0] - self.previous_disk[0]) * 512
                        sample["disk_write_bytes"] = max(0, disk[1] - self.previous_disk[1]) * 512
                        sample["disk_io_ms"] = max(0, disk[2] - self.previous_disk[2])
                    self.previous_disk = disk
                except (OSError, ValueError, IndexError) as error:
                    sample["error"] = str(error)
                output.write(json.dumps(sample) + "\n")
                output.flush()
                self.stop_event.wait(1.0)


def percentile(values, fraction):
    ordered = sorted(values)
    rank = max(1, int((len(ordered) * fraction) + 0.999999))
    return ordered[min(len(ordered), rank) - 1]


def stats(values):
    if not values:
        return None
    return {
        "n": len(values),
        "min_ms": min(values),
        "p50_ms": percentile(values, 0.50),
        "p95_ms": percentile(values, 0.95),
        "p99_ms": percentile(values, 0.99),
        "max_ms": max(values),
        "mean_ms": statistics.mean(values),
    }


def plain_stats(values):
    if not values:
        return None
    return {
        "n": len(values),
        "min": min(values),
        "p50": percentile(values, 0.50),
        "p95": percentile(values, 0.95),
        "p99": percentile(values, 0.99),
        "max": max(values),
        "mean": statistics.mean(values),
    }


def directory_size(root):
    total = files = 0
    for directory, _, names in os.walk(str(root)):
        for name in names:
            path = os.path.join(directory, name)
            try:
                if not os.path.islink(path):
                    total += os.path.getsize(path)
                    files += 1
            except OSError:
                pass
    return {"bytes": total, "files": files}


def find_task(board, mission_id, task_id):
    for mission in board.get("missions", []):
        if mission.get("id") == mission_id:
            for task in mission.get("tasks", []):
                if task.get("id") == task_id:
                    return task
    return None


def stop_all(tasks):
    for task in tasks.values():
        agent = task.get("agent_id")
        session = task.get("session_id")
        if not agent or not session:
            continue
        try:
            run_command([
                spawn_binary(), "submit", "--agent", agent, "--session", session,
                "--key", "storage-cluster-stop-%d" % task["id"], "--action", '{"type":"stop"}',
            ], timeout=20)
        except Exception as error:
            print("stop task %s: %s" % (task["id"], error), file=sys.stderr)


def verify_worker(source, agent, worker, iterations, manifest):
    reference = "refs/heads/spawn/%s" % agent
    shard = manifest["shards"][worker]
    rows = []
    for iteration in range(iterations):
        prefix = "results/worker-%03d/iter-%03d" % (worker, iteration)
        raw = git(source, ["show", "%s:%s.json" % (reference, prefix)], binary=True)
        result = json.loads(raw.decode("utf-8"))
        artifact = git(source, ["show", "%s:%s.bin" % (reference, prefix)], binary=True)
        if result.get("status") != "ok" or result.get("sha256") != shard["sha256"]:
            raise RuntimeError("worker %d iteration %d has a bad golden result" % (worker, iteration))
        if result.get("bytes") != shard["bytes"] or result.get("files") != len(shard["files"]):
            raise RuntimeError("worker %d iteration %d has bad counts" % (worker, iteration))
        label = "spawn-cluster-output-v1|worker=%d|iteration=%d|%s" % (
            worker, iteration, shard["sha256"])
        expected_artifact = hashlib.sha256(stream(label, OUTPUT_BYTES)).hexdigest()
        if result.get("artifact_sha256") != expected_artifact:
            raise RuntimeError("worker %d iteration %d has a non-golden artifact hash" % (worker, iteration))
        if len(artifact) != OUTPUT_BYTES or hashlib.sha256(artifact).hexdigest() != expected_artifact:
            raise RuntimeError("worker %d iteration %d has a bad artifact" % (worker, iteration))
        rows.append(result)
    changed = git(source, ["diff", "--name-only", "main", reference, "--", "golden", "tools"])
    if changed.strip():
        raise RuntimeError("worker %d modified golden inputs or tools" % worker)
    return rows


def verify_worker_when_published(source, agent, worker, iterations, manifest):
    deadline = time.monotonic() + 60
    while True:
        try:
            return verify_worker(source, agent, worker, iterations, manifest)
        except RuntimeError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(2)


def commit_iteration(source, commit, worker):
    paths = git(source, ["diff-tree", "--no-commit-id", "--name-only", "-r", commit])
    match = re.search(
        r"(?:^|\n)results/worker-%03d/iter-(\d+)\.json(?:\n|$)" % worker,
        paths,
    )
    return int(match.group(1)) if match else None


def event_iterations(events, worker, source, strict_turns=False):
    starts = {}
    pending = []
    completed = []
    rows = {}
    artifact_counts = collections.Counter()
    turn = 0
    for event in events:
        name = event.get("event_name")
        detail = event.get("detail") or {}
        at = event.get("recorded_at_ms")
        if name == "tool.started" and detail.get("tool_name") == "Bash":
            starts[detail.get("tool_use_id")] = (at, turn)
        elif name in ("tool.completed", "tool.failed"):
            tool = detail.get("tool_use_id")
            started = starts.pop(tool, None)
            if started is not None and name == "tool.completed":
                started_at, started_turn = started
                item = {
                    "tool_use_id": tool,
                    "tool_started_ms": started_at,
                    "tool_completed_ms": at,
                    "reported_tool_ms": detail.get("duration_ms"),
                    "turn": started_turn,
                }
                pending.append(item)
                completed.append(item)
        elif name == "artifact.committed" and (completed if strict_turns else pending):
            commit = (detail.get("artifact") or {}).get("commit")
            if not commit:
                continue
            iteration = commit_iteration(source, commit, worker)
            if iteration is None:
                continue
            if strict_turns:
                candidates = [item for item in completed if item["turn"] == iteration]
                if len(candidates) != 1:
                    raise RuntimeError(
                        "worker %d iteration %d has %d completed Bash calls in its measured turn; expected one"
                        % (worker, iteration, len(candidates)))
                tool = candidates[0]
                artifact_counts[iteration] += 1
            else:
                # Autonomous mode is a throughput/coalescing case, not a strict
                # per-turn timing claim.
                tool = pending.pop()
            row = {
                "worker": worker,
                "iteration": iteration,
                "tool_started_ms": tool["tool_started_ms"],
                "tool_completed_ms": tool["tool_completed_ms"],
                "reported_tool_ms": tool["reported_tool_ms"],
                "checkpoint_ms": at,
                "observed_tool_ms": tool["tool_completed_ms"] - tool["tool_started_ms"],
                "checkpoint_delay_ms": at - tool["tool_completed_ms"],
                "iteration_ms": at - tool["tool_started_ms"],
                "commit": commit,
            }
            rows[iteration] = row
        elif name == "turn.stopped":
            turn += 1
    if strict_turns:
        for iteration in rows:
            calls = sum(item["turn"] == iteration for item in completed)
            if calls != 1 or artifact_counts[iteration] != 1:
                raise RuntimeError(
                    "worker %d iteration %d has %d Bash calls and %d result checkpoints; expected one of each"
                    % (worker, iteration, calls, artifact_counts[iteration]))
    return rows


def analyze_host(path):
    samples = [json.loads(line) for line in open(str(path)) if line.strip()]
    valid = [sample for sample in samples if "error" not in sample]
    if not valid:
        return {"samples": len(samples)}
    return {
        "samples": len(samples),
        "duration_ms": valid[-1]["at_ms"] - valid[0]["at_ms"],
        "max_load1": max(sample["load1"] for sample in valid),
        "min_memory_available_bytes": min(
            sample["memory_available_bytes"] for sample in valid if sample.get("memory_available_bytes") is not None),
        "min_free_bytes": min(sample["free_bytes"] for sample in valid),
        "cpu_busy_percent": plain_stats([sample["cpu_busy_percent"] for sample in valid if "cpu_busy_percent" in sample]),
        "disk_read_bytes": sum(sample.get("disk_read_bytes", 0) for sample in valid),
        "disk_write_bytes": sum(sample.get("disk_write_bytes", 0) for sample in valid),
        "disk_io_ms": sum(sample.get("disk_io_ms", 0) for sample in valid),
    }


def run_cluster(args):
    source = args.source.absolute()
    manifest = json.loads(git(source, ["show", "main:golden/manifest.json"]))
    if args.agents < 1 or args.agents > len(manifest["shards"]):
        raise ValueError("agents must be between 1 and the golden shard count")
    if args.iterations < 1:
        raise ValueError("iterations must be positive")
    output = new_directory(args.output)
    workspace_setting = os.environ.get("SPAWN_WORKSPACE_ROOT")
    if not workspace_setting:
        raise RuntimeError("SPAWN_WORKSPACE_ROOT must name the live workspace filesystem")
    workspace_root = pathlib.Path(workspace_setting).absolute()
    workspace_stat = os.statvfs(str(workspace_root))
    free = workspace_stat.f_bavail * workspace_stat.f_frsize
    required = int(args.agents * manifest["bytes"] * 1.15 + 20 * 1024 ** 3)
    if free < required:
        raise RuntimeError("not enough workspace free space at %s: need at least %.1f GiB" % (
            workspace_root, required / 1024 ** 3))
    source_stat = os.statvfs(str(source))
    source_required = int(args.agents * args.iterations * OUTPUT_BYTES * 1.15 + 1024 ** 3)
    if source_stat.f_bavail * source_stat.f_frsize < source_required:
        raise RuntimeError("not enough source free space for published artifacts: need at least %.1f GiB" % (
            source_required / 1024 ** 3))
    config = {
        "schema_version": 1,
        "source": str(source),
        "agents": args.agents,
        "iterations": args.iterations,
        "golden_bytes": manifest["bytes"],
        "golden_sha256": manifest["sha256"],
        "workspace_root": str(workspace_root),
        "mode": "autonomous-single-turn" if args.autonomous_single_turn else "synchronized-turns",
        "started_at_ms": int(time.time() * 1000),
    }
    (output / "config.json").write_text(json.dumps(config, indent=2) + "\n")
    monitor = HostMonitor(output / "host.jsonl", workspace_root)
    monitor.start()
    tasks = {}
    mission_id = None
    began = time.monotonic()
    try:
        added = spawn_json([
            "mission", "add", "--text", "Storage cluster golden benchmark %d" % config["started_at_ms"],
            "--source", str(source),
        ])
        mission_id = added["mission"]
        config["mission"] = mission_id
        for worker in range(args.agents):
            initial = brief(worker, args.iterations) if args.autonomous_single_turn else iteration_brief(worker, 0)
            task_id = spawn_json([
                "task", "assign", "--mission", str(mission_id), "--kind", "ephemeral",
                "--worker", "storage%03d" % worker,
                "--brief", initial, "--model", "default",
            ])["task"]
            tasks[task_id] = {"id": task_id}
            print("assigned task %s worker %d" % (task_id, worker), file=sys.stderr)
        (output / "tasks.json").write_text(json.dumps(tasks, indent=2) + "\n")

        deadline = began + args.timeout
        terminal = {"reported", "accepted", "landed"}
        merged_events = {}
        if not args.autonomous_single_turn:
            # Wait for every task to have a live agent/session before driving
            # the synchronized turns. The initial prompt is iteration zero.
            while True:
                board = spawn_json(["mission", "list", "--limit", "200"])
                for task_id in tasks:
                    task = find_task(board, mission_id, task_id)
                    if task is not None:
                        tasks[task_id].update(task)
                ready = sum(
                    bool(task.get("agent_id") and task.get("session_id")) and task.get("state") in terminal | {"delivered"}
                    for task in tasks.values()
                )
                print("delivery ready %d/%d elapsed=%.1fs" % (
                    ready, len(tasks), time.monotonic() - began), file=sys.stderr, flush=True)
                if ready == len(tasks):
                    break
                if time.monotonic() > deadline:
                    raise TimeoutError("agents were not delivered before the timeout")
                time.sleep(args.poll)
            (output / "tasks.json").write_text(json.dumps(tasks, indent=2) + "\n")

            # One extra turn writes .spawn/report.md. Keeping finalization out
            # of the workload turns makes every measured iteration identical.
            for iteration in range(args.iterations + 1):
                if iteration > 0:
                    for task_id, task in sorted(tasks.items()):
                        worker = int(re.search(r"storage(\d+)(?:_|$)", task["agent_id"]).group(1))
                        if iteration < args.iterations:
                            text = iteration_brief(worker, iteration)
                        else:
                            text = finalize_brief(worker, args.iterations)
                        run_command([
                            spawn_binary(), "submit", "--agent", task["agent_id"],
                            "--session", task["session_id"],
                            "--key", "storage-cluster-input-%d-%d" % (task_id, iteration),
                            "--action", json.dumps({"type": "input", "text": text}),
                        ])
                pending = set(tasks)
                while pending:
                    for task_id in list(pending):
                        task = tasks[task_id]
                        tail = spawn_json([
                            "tail", "--session", task["session_id"], "--events", "200"])
                        events = tail.get("events", [])
                        worker = int(re.search(r"storage(\d+)(?:_|$)", task["agent_id"]).group(1))
                        merged = merged_events.setdefault(task_id, {})
                        for event in events:
                            merged[event["session_seq"]] = event
                        turns = sum(event.get("event_name") == "turn.stopped" for event in merged.values())
                        if turns >= iteration + 1:
                            (output / ("tail-%03d-turn-%03d.json" % (worker, iteration))).write_text(
                                json.dumps(tail, indent=2) + "\n")
                            pending.remove(task_id)
                    print("iteration %d turns complete %d/%d elapsed=%.1fs" % (
                        iteration, len(tasks) - len(pending), len(tasks), time.monotonic() - began),
                        file=sys.stderr, flush=True)
                    if pending and time.monotonic() > deadline:
                        raise TimeoutError("iteration %d did not finish: tasks %s" % (iteration, sorted(pending)))
                    if pending:
                        time.sleep(args.poll)
        else:
            while True:
                board = spawn_json(["mission", "list", "--limit", "200"])
                states = {}
                for task_id in tasks:
                    task = find_task(board, mission_id, task_id)
                    if task is not None:
                        tasks[task_id].update(task)
                        states[task_id] = task.get("state")
                counts = collections.Counter(states.values())
                print("state %s elapsed=%.1fs" % (dict(counts), time.monotonic() - began), file=sys.stderr)
                if len(states) == len(tasks) and all(state in terminal for state in states.values()):
                    break
                failures = {state for state in states.values() if state in ("cancelled", "failed", "rejected")}
                if failures:
                    raise RuntimeError("tasks entered failure states: %s" % sorted(failures))
                if time.monotonic() > deadline:
                    (output / "last-board.json").write_text(json.dumps(board, indent=2) + "\n")
                    raise TimeoutError("cluster did not finish within %d seconds" % args.timeout)
                time.sleep(args.poll)
        (output / "tasks.json").write_text(json.dumps(tasks, indent=2) + "\n")

        time.sleep(6)  # Let the background branch pusher catch up.
        all_iterations = []
        agent_reports = {}
        token_totals = collections.Counter()
        archive_failures = 0
        for task_id, task in sorted(tasks.items()):
            match = re.search(r"storage(\d+)(?:_|$)", task["agent_id"])
            if not match:
                raise RuntimeError("cannot identify the worker from %s" % task["agent_id"])
            worker = int(match.group(1))
            if args.autonomous_single_turn:
                tail = spawn_json(["tail", "--session", task["session_id"], "--events", "200"])
            else:
                tail = {"events": [merged_events[task_id][seq] for seq in sorted(merged_events[task_id])]}
            (output / ("tail-%03d.json" % worker)).write_text(json.dumps(tail, indent=2) + "\n")
            archive_failures += sum(event.get("event_name") == "archive.failed" for event in tail.get("events", []))
            messages = {}
            for event in tail.get("events", []):
                if event.get("event_name") == "model.usage":
                    detail = event.get("detail") or {}
                    message = detail.get("message_id") or event.get("session_seq")
                    previous = messages.setdefault(message, {})
                    for key in ("input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens"):
                        previous[key] = max(previous.get(key, 0), detail.get(key) or 0)
            for detail in messages.values():
                for key, value in detail.items():
                    token_totals[key] += value
            verified = verify_worker_when_published(source, task["agent_id"], worker, args.iterations, manifest)
            timed = event_iterations(
                tail.get("events", []), worker, source,
                strict_turns=not args.autonomous_single_turn)
            missing = sorted(set(range(args.iterations)) - set(timed))
            if missing:
                raise RuntimeError("worker %d lacks timing events for iterations %s" % (worker, missing))
            for iteration, row in timed.items():
                row["verified_bytes"] = verified[iteration]["bytes"]
                row["verified_artifact_sha256"] = verified[iteration]["artifact_sha256"]
                row["workload_ms"] = verified[iteration].get("workload_ms")
                all_iterations.append(row)
            agent_reports[str(task_id)] = {"worker": worker, "verified_iterations": len(verified)}

        cluster_iterations = []
        for iteration in range(args.iterations):
            rows = [row for row in all_iterations if row["iteration"] == iteration]
            cluster_iterations.append({
                "iteration": iteration,
                "agents": len(rows),
                "first_tool_started_ms": min(row["tool_started_ms"] for row in rows),
                "last_checkpoint_ms": max(row["checkpoint_ms"] for row in rows),
                "cluster_wall_ms": max(row["checkpoint_ms"] for row in rows) - min(row["tool_started_ms"] for row in rows),
                "iteration_latency": stats([row["iteration_ms"] for row in rows]),
                "checkpoint_delay": stats([row["checkpoint_delay_ms"] for row in rows]),
                "observed_tool": stats([row["observed_tool_ms"] for row in rows]),
                "workload": stats([row["workload_ms"] for row in rows if row.get("workload_ms") is not None]),
            })
        monitor.stop()
        monitor.join(3)
        report = {
            "schema_version": 1,
            "config": config,
            "result": "passed",
            "wall_ms": int((time.monotonic() - began) * 1000),
            "agents_completed": len(agent_reports),
            "iterations_verified": len(all_iterations),
            "golden_bytes_read": sum(row["verified_bytes"] for row in all_iterations),
            "new_artifact_bytes": len(all_iterations) * OUTPUT_BYTES,
            "latency": {
                "iteration": stats([row["iteration_ms"] for row in all_iterations]),
                "observed_tool": stats([row["observed_tool_ms"] for row in all_iterations]),
                "workload": stats([row["workload_ms"] for row in all_iterations if row.get("workload_ms") is not None]),
                "checkpoint_delay": stats([row["checkpoint_delay_ms"] for row in all_iterations]),
            },
            "cluster_iterations": cluster_iterations,
            "model_tokens": dict(token_totals),
            "archive_failed_events": archive_failures,
            "host": analyze_host(output / "host.jsonl"),
            "source_disk": directory_size(source),
            "agents": agent_reports,
        }
        (output / "cluster-report.json").write_text(json.dumps(report, indent=2) + "\n")
        (output / "iterations.json").write_text(json.dumps(all_iterations, indent=2) + "\n")
        print(json.dumps(report, indent=2))
    finally:
        monitor.stop()
        monitor.join(3)
        stop_all(tasks)
        config["ended_at_ms"] = int(time.time() * 1000)
        (output / "config.json").write_text(json.dumps(config, indent=2) + "\n")
        if mission_id is not None:
            try:
                spawn_json(["mission", "close", "--id", str(mission_id), "--state", "dropped",
                            "--summary", "Storage cluster benchmark finished; see %s" % output])
            except Exception as error:
                print("close mission: %s" % error, file=sys.stderr)


def main():
    if hasattr(sys.stderr, "reconfigure"):
        sys.stderr.reconfigure(line_buffering=True)
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="command", required=True)
    make = subcommands.add_parser("generate")
    make.add_argument("--root", type=pathlib.Path, required=True)
    make.add_argument("--file-kib", type=int, default=DEFAULT_FILE_KIB)
    make.add_argument("--shards", type=int, default=DEFAULT_SHARDS)
    make.add_argument("--files-per-shard", type=int, default=DEFAULT_FILES_PER_SHARD)
    make.set_defaults(function=generate)
    run = subcommands.add_parser("run")
    run.add_argument("--source", type=pathlib.Path, required=True)
    run.add_argument("--output", type=pathlib.Path, required=True)
    run.add_argument("--agents", type=int, default=DEFAULT_SHARDS)
    run.add_argument("--iterations", type=int, default=8)
    run.add_argument("--timeout", type=int, default=1800)
    run.add_argument("--poll", type=float, default=5.0)
    run.add_argument(
        "--autonomous-single-turn",
        action="store_true",
        help="let each agent issue all tool calls in one turn; useful for measuring checkpoint coalescing",
    )
    run.set_defaults(function=run_cluster)
    args = parser.parse_args()
    if args.command == "generate" and min(args.file_kib, args.shards, args.files_per_shard) < 1:
        parser.error("generation sizes must be positive")
    args.function(args)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, TimeoutError, subprocess.SubprocessError) as error:
        print("storage cluster benchmark: %s" % error, file=sys.stderr)
        sys.exit(1)
