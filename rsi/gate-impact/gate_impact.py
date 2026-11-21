#!/usr/bin/env python3
"""The impact-based landing selector: a bounded dry-run prototype for task 490.

It answers one question about a (base, candidate) pair, from git alone:

    Which of the landing path's jobs must run for this candidate, which
    may be skipped because their inputs are byte-identical to the base and
    the base's accepting gate recorded a pass, and how long does the
    policy's landing slice take on the timings measured by the 243 gate?

It prints the policy version and digest, the input keys, every selected and
skipped job with its reason, and the predicted wall clock against the 180 s
budget.  It never runs a check, never writes a cache entry and never touches
gate.sh: this is the dry-run half of the policy, and a live install is a
separate, reviewed change.

Policy (rsi/gate-impact/policy.json, version 1):
- The mandatory core is always on: the static checks (protected paths, no
  build-time code, the test count from the base's cached list).
- A job runs when the content hash of what it reads changed, when its key is
  unchanged but no base result is recorded (a cache miss), or when it is a
  C1 data-loss/control journey (always re-run on the landing path).
- Shared or cross-cutting input (the judge, scripts, deploy, the UI build
  config, the toolchain) escalates to today's full gate.  A path no class
  claims escalates too.  Neither is ever a narrow accept.
- Real-time holds (192, 209) and the historical objectives are nightly in
  the proposed policy; `--historical-mode cache-only` models today's gate
  plus content-addressed results instead, so the two costs can be compared.
  The default is the policy's own (`historical.default_mode`, cache-only):
  an operator who omits the flag does not install the unapproved retirement.
- A file under rsi/scenarios/nightly/ is not in any landing key: the gate
  never runs it, so it must not invalidate a landing result.  But the
  backlog is not inert: `cargo test` (check-tests.sh) reads rsi/backlog/
  through crates/spawn-cli/src/objectives.rs, and scenario 008 reads the
  tree's README.md, so both are key material for the jobs that read them.
- A scenario listed in rsi/scenarios/serial runs alone, as the gate runs it;
  its seconds are charged serially, not into the four-slot makespan.
- A base record binds to its exact base commit: a missing, short or
  different commit is no verdict (exit 2), never a cache hit.

Exit codes:
  0  a selection was produced (decision narrow, full or reject)
  1  static trust refused the candidate (build-time code)
  2  no verdict: the policy, timings, record or tree could not be read, a
     selected job has no measured time, or the base record is for another
     commit.  The caller keeps today's full gate; nothing is narrowed.
  3  --require-narrow was given and the decision is not narrow.

Not done here (deliberately): executing any job, writing a result, deciding
that a scenario may be retired, or authorising a skip from model text.  The
only authority is the paths and blobs git holds.
"""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys

SUPPORTED_POLICY_VERSION = 1
SUPPORTED_TIMINGS_VERSION = 1

EXIT_OK = 0
EXIT_REJECT = 1
EXIT_NO_VERDICT = 2
EXIT_NOT_NARROW = 3

HERE = os.path.dirname(os.path.abspath(__file__))


class NoVerdict(Exception):
    """The selector cannot answer; the caller must keep the full gate."""


class Reject(Exception):
    """Static trust refused the candidate."""


# ---------------------------------------------------------------------------
# git


def git(repo, args, text=False):
    proc = subprocess.run(["git", "-C", repo] + args,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if proc.returncode != 0:
        raise NoVerdict("git %s failed: %s" % (
            " ".join(args),
            proc.stderr.decode("utf-8", "replace").strip()))
    return proc.stdout.decode("utf-8", "replace") if text else proc.stdout


def commit_id(repo, rev):
    return git(repo, ["rev-parse", "--verify", rev + "^{commit}"], text=True).strip()


def is_ancestor(repo, base, candidate):
    """True when `base` is an ancestor of (or equal to) `candidate`.

    The controller checks this today, but a selector that claims fail-closed
    must not answer for a candidate that does not contain its base: the diff
    would then describe a reversal, not a landing.
    """
    proc = subprocess.run(["git", "-C", repo, "merge-base", "--is-ancestor",
                           base, candidate],
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if proc.returncode == 0:
        return True
    if proc.returncode == 1:
        return False
    raise NoVerdict("git merge-base --is-ancestor %s %s failed: %s" % (
        short(base), short(candidate),
        proc.stderr.decode("utf-8", "replace").strip()))


def tree_id(repo, commit):
    return git(repo, ["rev-parse", commit + "^{tree}"], text=True).strip()


def changed_paths(repo, base, candidate):
    raw = git(repo, ["diff", "--no-renames", "--name-only", "-z", base, candidate])
    return sorted(p for p in raw.decode("utf-8", "surrogateescape").split("\0") if p)


def blob(repo, commit, path):
    try:
        return git(repo, ["rev-parse", "%s:%s" % (commit, path)], text=True).strip()
    except NoVerdict:
        return None


def file_text(repo, commit, path):
    try:
        return git(repo, ["cat-file", "-p", "%s:%s" % (commit, path)],
                   text=True)
    except NoVerdict:
        return None


def tree_key(repo, commit, specs, policy_version):
    """A content hash of the files under `specs` in `commit`.

    The spec list is hashed too, so two empty selections do not collide.
    `ls-tree -r` prints one entry per file (mode, type, blob, path), already
    sorted; anything that changes those files changes the hash.  This is
    the C14 shape: the key covers what the job can read, not a label.
    """
    raw = git(repo, ["ls-tree", "-r", "-z", "--full-tree", commit, "--"] + list(specs))
    h = hashlib.sha256()
    h.update(("policy=%d\n" % policy_version).encode())
    for spec in specs:
        h.update(("spec=%s\n" % spec).encode())
    h.update(raw)
    return h.hexdigest()


def top_level_scenarios(repo, commit, scenarios_dir):
    raw = git(repo, ["ls-tree", "-r", "--full-tree", "--name-only", commit,
                     "--", scenarios_dir], text=True)
    prefix = scenarios_dir.rstrip("/") + "/"
    out = []
    for line in raw.splitlines():
        if not line.startswith(prefix):
            continue
        rest = line[len(prefix):]
        if "/" in rest or not rest.endswith(".sh"):
            continue
        out.append(rest)
    return sorted(out)


# ---------------------------------------------------------------------------
# policy


def canon(obj):
    return json.dumps(obj, sort_keys=True, separators=(",", ":"))


def sha_text(text):
    return hashlib.sha256(text.encode()).hexdigest()


def validate_policy(policy, path):
    """Every field the selector reads, checked before it is read.

    A malformed policy used to raise KeyError or ValueError out of `select`
    and end the process with exit 1 (the static-reject code).  It is a
    missing authority, not a refused candidate: exit 2, no verdict.
    """
    def need(cond, what):
        if not cond:
            raise NoVerdict("policy %s is malformed: %s" % (path, what))

    need(isinstance(policy, dict), "not a JSON object")
    need(isinstance(policy.get("path_classes"), list) and policy["path_classes"],
         "path_classes must be a non-empty list")
    for cls in policy["path_classes"]:
        need(isinstance(cls, dict), "a path class is not an object")
        need(isinstance(cls.get("id"), str) and cls["id"], "a path class has no id")
        need(cls.get("impact") in ("none", "web", "rust", "full"),
             "path class %r has no known impact" % cls.get("id"))
        need(isinstance(cls.get("paths"), list), "path class %r has no paths" % cls["id"])
    need(isinstance(policy.get("jobs"), list) and policy["jobs"],
         "jobs must be a non-empty list")
    for job in policy["jobs"]:
        need(isinstance(job, dict), "a job is not an object")
        need(isinstance(job.get("id"), str) and job["id"], "a job has no id")
        need(isinstance(job.get("kind"), str), "job %r has no kind" % job.get("id"))
        need(isinstance(job.get("timing"), str), "job %r has no timing" % job.get("id"))
        need(isinstance(job.get("key", []), list), "job %r has a bad key" % job["id"])
    need(isinstance(policy.get("core"), dict) and isinstance(policy["core"].get("id"), str),
         "core must be an object with an id")
    need(isinstance(policy.get("historical"), dict), "historical must be an object")
    need(isinstance(policy["historical"].get("match"), str),
         "historical.match must be a string")
    need(isinstance(policy["historical"].get("default_mode"), str),
         "historical.default_mode must be a string")
    need(isinstance(policy["historical"].get("unsolved_default"), str),
         "historical.unsolved_default must be a string")
    need(isinstance(policy.get("journey_dir"), str) and policy["journey_dir"].endswith("/"),
         "journey_dir must be a directory path ending in /")
    need(isinstance(policy.get("protected_list"), str), "protected_list must be a string")
    need(isinstance(policy.get("serial_list"), str), "serial_list must be a string")
    need(isinstance(policy.get("scenario_inputs", {}), dict),
         "scenario_inputs must be an object")
    need(isinstance(policy.get("slots"), int) and policy["slots"] >= 1,
         "slots must be a positive integer")
    need(isinstance(policy.get("budget_seconds"), (int, float))
         and policy["budget_seconds"] > 0, "budget_seconds must be positive")


def load_policy(path):
    try:
        with open(path, "r", encoding="utf-8") as f:
            raw = f.read()
        policy = json.loads(raw)
    except (OSError, ValueError) as exc:
        raise NoVerdict("policy %s is unreadable: %s" % (path, exc))
    version = policy.get("policy_version") if isinstance(policy, dict) else None
    if version != SUPPORTED_POLICY_VERSION:
        raise NoVerdict("policy version %r is not supported (this selector knows %d)"
                        % (version, SUPPORTED_POLICY_VERSION))
    validate_policy(policy, path)
    return policy, sha_text(canon(policy))


def load_timings(path, profile):
    try:
        with open(path, "r", encoding="utf-8") as f:
            timings = json.load(f)
    except (OSError, ValueError) as exc:
        raise NoVerdict("timings %s are unreadable: %s" % (path, exc))
    if timings.get("version") != SUPPORTED_TIMINGS_VERSION:
        raise NoVerdict("timings version %r is not supported (this selector knows %d)"
                        % (timings.get("version"), SUPPORTED_TIMINGS_VERSION))
    profiles = timings.get("profiles") or {}
    if profile not in profiles:
        raise NoVerdict("timings profile %r is missing (have: %s)"
                        % (profile, ", ".join(sorted(profiles))))
    return profiles[profile]


def load_base_record(path, base):
    if not path:
        return None
    try:
        with open(path, "r", encoding="utf-8") as f:
            record = json.load(f)
    except (OSError, ValueError) as exc:
        raise NoVerdict("base record %s is unreadable: %s" % (path, exc))
    if not isinstance(record, dict):
        raise NoVerdict("base record %s is not a JSON object" % path)
    recorded = record.get("commit")
    if not isinstance(recorded, str):
        raise NoVerdict("base record %s has no commit field; it must name the "
                        "exact 40-character base commit" % path)
    if not re.fullmatch(r"[0-9a-f]{40}", recorded):
        raise NoVerdict("base record %s names commit %r, which is not a full "
                        "40-character commit; a short or empty commit binds to "
                        "no base" % (path, recorded))
    if recorded != base:
        raise NoVerdict("base record is for %s, not base %s" % (recorded, base))
    return record


# ---------------------------------------------------------------------------
# path classification


def path_matches(path, spec):
    if spec.endswith("/"):
        return path == spec[:-1] or path.startswith(spec)
    return path == spec


def classify(path, policy):
    for cls in policy["path_classes"]:
        for spec in cls["paths"]:
            if path_matches(path, spec):
                return cls
    return None


def parse_protected(text):
    deny, allow = [], []
    for line in (text or "").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("!"):
            allow.append(line[1:])
        else:
            deny.append(line)

    def protected(path):
        # The gate matches these as string prefixes (gate.sh:2772-2784), not
        # as exact paths or directories: `rust-toolchain` protects
        # `rust-toolchain.toml` and any `rust-toolchain-*` the gate would
        # refuse.  Mirror it, so the selector's flag cannot disagree.
        if any(path.startswith(spec) for spec in allow):
            return False
        return any(path.startswith(spec) for spec in deny)
    return protected


BUILD_SCRIPT = re.compile(r"(?m)^\s*build\s*=")
PROC_MACRO = re.compile(r"(?m)^\s*proc-macro\s*=\s*true\b")


def static_build_time(repo, candidate, paths):
    """The subset of gate.sh's no-build-time-code rule a diff can decide."""
    bad = []
    for path in paths:
        if path == "build.rs" or path.endswith("/build.rs"):
            bad.append((path, "names a build script (build.rs)"))
        elif path == ".cargo" or path.startswith(".cargo/") or "/.cargo/" in path:
            bad.append((path, "touches .cargo/"))
        elif path == "Cargo.toml" or path.endswith("/Cargo.toml"):
            text = file_text(repo, candidate, path) or ""
            if BUILD_SCRIPT.search(text):
                bad.append((path, "a Cargo.toml sets build ="))
            if PROC_MACRO.search(text):
                bad.append((path, "a Cargo.toml declares a proc-macro target"))
    return bad


# ---------------------------------------------------------------------------
# the selection


def short(commit):
    return commit[:8]


def key_for(policy_version, env, parts):
    return sha_text(canon({"policy": policy_version, "env": env, "parts": parts}))


def schedule(jobs, slots):
    """Longest first, onto the slot that frees first: the gate's own order."""
    loads = [0.0] * slots
    for _, seconds in sorted(jobs, key=lambda item: (-item[1], item[0])):
        slot = min(range(slots), key=lambda i: (loads[i], i))
        loads[slot] += seconds
    return max(loads) if loads else 0.0


class Selection:
    def __init__(self):
        self.selected = []      # dicts: id, why, key, seconds, kind
        self.skipped = []       # dicts: id, why, key
        self.unknown = []
        self.classes = {}
        self.static_failures = []
        self.full_reasons = []
        self.assumptions = []
        self.base_seconds = 0.0


def select(opts):
    repo = opts.repo
    if not os.path.isdir(repo):
        raise NoVerdict("repository %s does not exist" % repo)
    base = commit_id(repo, opts.base)
    candidate = commit_id(repo, opts.candidate)
    policy, policy_digest = load_policy(opts.policy)
    profile = load_timings(opts.timings, opts.profile)
    record = load_base_record(opts.base_record, base)

    version = policy["policy_version"]
    env = profile.get("env") or {}

    # The defaults are the policy's, not this file's: an operator who omits
    # the flags must not install a retirement the owner has not approved.
    modes = policy["historical"].get("modes") or ["cache-only", "nightly"]
    if opts.historical_mode is None:
        opts.historical_mode = policy["historical"]["default_mode"]
    if opts.historical_mode not in modes:
        raise NoVerdict("historical mode %r is not one of the policy's %s"
                        % (opts.historical_mode, modes))
    if opts.unsolved_mode is None:
        opts.unsolved_mode = policy["historical"]["unsolved_default"]
    if opts.unsolved_mode not in ("carry", "run"):
        raise NoVerdict("unsolved mode %r is not one of carry, run"
                        % opts.unsolved_mode)

    if opts.slots is not None and opts.slots < 1:
        raise NoVerdict("--slots must be a positive integer, not %d" % opts.slots)
    slots = opts.slots or policy["slots"]
    if opts.budget is not None and opts.budget <= 0:
        raise NoVerdict("--budget must be positive, not %r" % opts.budget)
    budget = opts.budget or policy["budget_seconds"]

    paths = changed_paths(repo, base, candidate)
    if not paths:
        raise NoVerdict("no paths changed between %s and %s: nothing to land"
                        % (short(base), short(candidate)))
    if not is_ancestor(repo, base, candidate):
        raise NoVerdict("base %s is not an ancestor of candidate %s: the diff "
                        "would not describe a landing" % (short(base), short(candidate)))

    result = Selection()
    protected_text = file_text(repo, base, policy["protected_list"])
    if protected_text is None:
        raise NoVerdict("the controller's %s is missing at the base"
                        % policy["protected_list"])
    protected = parse_protected(protected_text)
    protected_paths = [p for p in paths if protected(p)]

    for path in paths:
        cls = classify(path, policy)
        if cls is None:
            result.unknown.append(path)
        else:
            result.classes.setdefault(cls["id"], []).append(path)
            if cls["impact"] == "full":
                result.full_reasons.append(
                    "shared/cross-cutting input changed: %s (%s)" % (path, cls["id"]))

    result.static_failures = static_build_time(repo, candidate, paths)
    if result.static_failures:
        decision = "reject"
    elif result.unknown or result.full_reasons:
        decision = "full"
    else:
        decision = "narrow"

    # Content keys, computed on both trees.  The specs are the policy's own
    # reading of each class; a job whose key does not name everything it can
    # read is the C14 failure this prototype cannot yet rule out, so the
    # live install needs the proof, not this file's word.
    rust_specs = ["crates/", "Cargo.toml", "Cargo.lock"]
    web_specs = ["web/src/", "web/test/", "web/index.html",
                 "web/package.json", "web/package-lock.json", "web/pnpm-lock.yaml"]
    scripts_specs = ["scripts/"]
    # The backlog is not inert data: crates/spawn-cli/src/objectives.rs loads
    # the tree's real rsi/backlog under `cargo test`, so check-tests.sh (and
    # only check-tests.sh) reads it.  The README floor sentence is read by
    # scenario 008.  Neither may sit in the "no job reads it" docs class.
    backlog_specs = ["rsi/backlog/"]
    readme_specs = ["README.md"]
    keys = {
        "static": key_for(version, env, {
            "protected": sha_text(protected_text),
            "core": policy["core"]["id"],
        }),
        "rust": tree_key(repo, candidate, rust_specs, version),
        "web": tree_key(repo, candidate, web_specs, version),
        "scripts": tree_key(repo, candidate, scripts_specs, version),
        "backlog": tree_key(repo, candidate, backlog_specs, version),
        "readme": tree_key(repo, candidate, readme_specs, version),
    }
    base_keys = {
        "static": key_for(version, env, {
            "protected": sha_text(file_text(repo, base, policy["protected_list"]) or ""),
            "core": policy["core"]["id"],
        }),
        "rust": tree_key(repo, base, rust_specs, version),
        "web": tree_key(repo, base, web_specs, version),
        "scripts": tree_key(repo, base, scripts_specs, version),
        "backlog": tree_key(repo, base, backlog_specs, version),
        "readme": tree_key(repo, base, readme_specs, version),
    }
    scenarios_dir = policy["journey_dir"]
    scenario_names = top_level_scenarios(repo, candidate, scenarios_dir)
    scenario_keys = {}
    base_scenario_keys = {}
    for name in scenario_names:
        rel = scenarios_dir + name
        scenario_keys[name] = blob(repo, candidate, rel)
        base_scenario_keys[name] = blob(repo, base, rel)

    passed = set((record or {}).get("passed") or [])
    ran_ci = set((record or {}).get("ran_ci") or [])

    def job_key(kind, name, tree_keys, scenario_blob):
        parts = {"kind": kind, "name": name}
        for key_name in tree_keys:
            parts[key_name] = tree_keys[key_name]
        if scenario_blob is not None:
            parts["blob"] = scenario_blob
        return key_for(version, env, parts)

    base_tree_keys = {name: base_keys[name] for name in
                      ("rust", "web", "scripts", "backlog", "readme")}
    cand_tree_keys = {name: keys[name] for name in
                      ("rust", "web", "scripts", "backlog", "readme")}
    scenario_inputs = policy.get("scenario_inputs") or {}

    def decide(job_id, kind, tree_key_names, name, always=False):
        cand_parts = {k: cand_tree_keys[k] for k in tree_key_names}
        base_parts = {k: base_tree_keys[k] for k in tree_key_names}
        ckey = job_key(kind, name, cand_parts, scenario_keys.get(name))
        bkey = job_key(kind, name, base_parts, base_scenario_keys.get(name))
        if always:
            return "selected", "always-on (C1 data-loss/control)", ckey
        if bkey == ckey and (name in passed or job_id in ran_ci):
            return "skipped", "cache-hit on base %s" % short(base), ckey
        if bkey == ckey:
            return "selected", "cache-miss (no base result for an unchanged key)", ckey
        return "selected", "input-changed", ckey

    def timing(job_id):
        if job_id in profile.get("serial", {}):
            return profile["serial"][job_id]
        if job_id in profile.get("jobs", {}):
            return profile["jobs"][job_id]
        return None

    def require_timing(job_id):
        seconds = timing(job_id)
        if seconds is None:
            raise NoVerdict(
                "selected job %r has no measured time in profile %r; "
                "the selector predicts nothing it has not measured" % (job_id, opts.profile))
        return seconds

    # Explicit jobs.
    for job in policy["jobs"]:
        job_id = job["id"]
        name = job.get("journey", job_id)
        kind = "journey" if job.get("journey") else "ci"
        key_names = job.get("key", [])
        if job.get("journey"):
            key_names = ["rust", "web", "scripts"]
        state, why, ckey = decide(job_id, kind, key_names, name, job.get("always", False))
        if state == "selected" and decision == "narrow":
            seconds = require_timing(job["timing"])
            result.selected.append({"id": job_id, "kind": job["kind"], "why": why,
                                    "key": ckey, "seconds": seconds,
                                    "timing": job["timing"]})
        elif state == "skipped" and decision == "narrow":
            result.skipped.append({"id": job_id, "why": why, "key": ckey})
        elif decision == "full":
            seconds = timing(job["timing"])
            if seconds is None:
                seconds = 0.0
            result.selected.append({"id": job_id, "kind": job["kind"],
                                    "why": "full gate", "key": ckey,
                                    "seconds": seconds, "timing": job["timing"]})

    explicit_journeys = {job["journey"] for job in policy["jobs"] if job.get("journey")}
    explicit_names = explicit_journeys | {job["id"] for job in policy["jobs"]}
    historical_mode = opts.historical_mode
    nightly_holds = set(policy.get("nightly_holds", {}).get("scenarios") or [])
    historical_pattern = re.compile(policy["historical"]["match"])
    task_only = re.compile(r"(?m)^#\s*rsi:\s*task-only\b")
    record_unsolved = set((record or {}).get("unsolved") or [])
    base_jobs = profile.get("base_jobs") or {}

    def scenario_key_names(name):
        """What a scenario reads: the three trees, plus the policy's extras.

        A scenario is keyed on the trees it may read; the policy adds the
        one path a scenario reads directly (`008.sh` reads README.md).
        """
        extra = scenario_inputs.get(name) or []
        unknown_extra = [key for key in extra if key not in cand_tree_keys]
        if unknown_extra:
            raise NoVerdict("policy scenario_inputs names unknown key(s) %s for %s"
                            % (unknown_extra, name))
        return ["rust", "web", "scripts"] + list(extra)

    def base_cost(name, trust_record=True):
        """What the base phase costs for a historical objective.

        A result already recorded for the base commit (solved or unsolved)
        costs nothing: the base cache holds it, and the candidate's key has
        no bearing on a fact about the base tree.  The one case that runs is
        an unsolved objective whose key did not change and whose result is
        not carried (--unsolved-mode run): then the selector charges the
        base's two agreeing runs, because nothing recorded decides it.
        """
        if trust_record and (name in passed or name in record_unsolved):
            return 0.0
        if name in base_jobs:
            return base_jobs[name]
        secs = timing(name) or 0.0
        return secs * 2

    for name in scenario_names:
        if name in explicit_names:
            continue
        if opts.task_scenario and name == opts.task_scenario:
            continue
        if task_only.search(file_text(repo, candidate, scenarios_dir + name) or ""):
            result.skipped.append({
                "id": ("historical:" if historical_pattern.match(name) else "check:") + name,
                "why": "task-only (the gate runs it only as its own task's scenario)",
                "key": job_key("scenario", name,
                               {k: cand_tree_keys[k] for k in scenario_key_names(name)},
                               scenario_keys.get(name))})
            continue
        is_historical = bool(historical_pattern.match(name))
        is_nightly_hold = name in nightly_holds
        job_id = ("historical:" if is_historical else "check:") + name
        ckey = job_key("scenario", name,
                       {k: cand_tree_keys[k] for k in scenario_key_names(name)},
                       scenario_keys.get(name))
        bkey = job_key("scenario", name,
                       {k: base_tree_keys[k] for k in scenario_key_names(name)},
                       base_scenario_keys.get(name))
        if decision == "narrow":
            if (historical_mode == "nightly") and (is_historical or is_nightly_hold):
                result.skipped.append({
                    "id": job_id,
                    "why": "nightly-scope (design-gate-v2 Decision 1; install pending)",
                    "key": ckey})
                continue
            if is_historical and bkey == ckey and name in record_unsolved:
                if opts.unsolved_mode == "carry":
                    result.skipped.append({
                        "id": job_id,
                        "why": "carried: unsolved on base, inputs unchanged",
                        "key": ckey})
                    continue
                cost = base_cost(name, trust_record=False)
                result.base_seconds += cost
                result.selected.append({
                    "id": job_id, "kind": "parallel", "scenario": name,
                    "why": "cache-miss: unsolved base result is not carried (--unsolved-mode run)",
                    "key": ckey, "seconds": require_timing(name),
                    "timing": name, "base_seconds": round(cost, 3)})
                continue
            state, why, _ = decide(job_id, "scenario", scenario_key_names(name), name)
            if state == "selected":
                cost = base_cost(name) if is_historical else 0.0
                result.base_seconds += cost
                result.selected.append({"id": job_id, "kind": "parallel",
                                        "scenario": name,
                                        "why": why, "key": ckey,
                                        "seconds": require_timing(name),
                                        "timing": name,
                                        "base_seconds": round(cost, 3)})
            else:
                result.skipped.append({"id": job_id, "why": why, "key": ckey})
        else:
            seconds = timing(name) or 0.0
            result.selected.append({"id": job_id, "kind": "parallel",
                                    "scenario": name, "why": "full gate", "key": ckey,
                                    "seconds": seconds, "timing": name})

    # The task's own scenario: always on the landing path when the task names
    # one, and never a cache hit (it is the acceptance test of this task).
    task_seconds = None
    if opts.task_scenario:
        name = opts.task_scenario
        if name not in scenario_names:
            raise NoVerdict("task scenario %r is not in %s at the candidate"
                            % (name, scenarios_dir))
        ckey = job_key("task", name, cand_tree_keys, scenario_keys.get(name))
        task_times = (profile.get("task") or {}).get(name)
        if not task_times:
            raise NoVerdict("task scenario %r has no measured time in profile %r"
                            % (name, opts.profile))
        representative = sum(task_times) / len(task_times)
        runs = [representative] * max(1, opts.task_runs)
        if not opts.task_base_cached:
            runs.insert(0, representative)
            result.assumptions.append(
                "task scenario base run counted at one candidate-run time: "
                "no cached base result was declared (--task-base-cached)")
        task_seconds = sum(runs)
        result.selected.append({
            "id": "task:" + name,
            "kind": "parallel",
            "why": "task acceptance test: fail on base, pass on candidate",
            "key": ckey,
            "seconds": task_seconds,
            "timing": name,
            "runs": len(runs)})

    # Prediction.
    if decision == "full":
        wall = float(profile["full_gate"])
        method = "measured today's full gate (profile %s)" % opts.profile
        own = None
        serial_seconds = 0.0
        parallel = 0.0
        base_seconds = float(profile.get("base_phase") or 0.0)
        setup = float(profile.get("setup") or 0.0)
        measured_candidate = float(profile.get("candidate_phase") or 0.0)
    else:
        setup = float(profile.get("setup") or 0.0)
        base_seconds = result.base_seconds
        measured_candidate = 0.0
        serial_seconds = 0.0
        if decision == "narrow":
            for job in result.selected:
                if job["kind"] == "serial":
                    serial_seconds += job["seconds"]
            parallel_jobs = [(job["id"], job["seconds"]) for job in result.selected
                             if job["kind"] == "parallel"]
            parallel = schedule(parallel_jobs, slots)
            own = sum(job["seconds"] for job in result.selected)
            wall = setup + base_seconds + serial_seconds + parallel
            method = ("setup + base + serial + longest-first on %d slots (profile %s)"
                      % (slots, opts.profile))
        else:  # reject
            wall = 0.0
            own = 0.0
            parallel = 0.0
            method = "not run: static trust refused the candidate"

    within = wall <= budget
    decision_reasons = []
    if result.static_failures:
        decision_reasons = ["static trust: %s: %s" % pair for pair in result.static_failures]
    elif result.unknown:
        decision_reasons = ["unknown path class (fail-closed to the full gate): %s" % p
                            for p in result.unknown]
    elif result.full_reasons:
        decision_reasons = result.full_reasons

    approval = sha_text(canon({
        "policy": policy_digest,
        "base": base,
        "candidate": candidate,
        "candidate_tree": tree_id(repo, candidate),
        "decision": decision,
        "selected": sorted(job["id"] for job in result.selected),
        "keys": {job["id"]: job["key"] for job in result.selected},
        "task": opts.task_scenario,
        "timings_profile": opts.profile,
        "historical_mode": opts.historical_mode,
        "unsolved_mode": opts.unsolved_mode,
    }))

    return {
        "model": "impact-based landing slice (prototype, dry-run only)",
        "policy_version": version,
        "policy_digest": policy_digest,
        "policy_name": policy.get("name"),
        "base": base,
        "candidate": candidate,
        "candidate_tree": tree_id(repo, candidate),
        "approval_digest": approval,
        "decision": decision,
        "decision_reasons": decision_reasons,
        "fail_closed": bool(result.unknown) or decision == "reject",
        "changed": [{"path": p,
                     "class": (classify(p, policy) or {"id": "unknown", "impact": "full"})["id"],
                     "protected": protected(p)} for p in paths],
        "unknown_paths": result.unknown,
        "protected_paths": protected_paths,
        "owner_review_required": bool(protected_paths),
        "classes": result.classes,
        "keys": keys,
        "base_keys": base_keys,
        "selected": result.selected,
        "skipped": result.skipped,
        "predicted": {
            "wall_seconds": round(wall, 3),
            "budget_seconds": budget,
            "within_budget": within,
            "method": method,
            "setup_seconds": setup,
            "base_seconds": round(base_seconds, 3),
            "candidate_phase_seconds": round(measured_candidate if decision == "full" else parallel, 3),
            "serial_seconds": round(serial_seconds, 3),
            "parallel_seconds": round(parallel, 3),
            "jobs_own_seconds": None if own is None else round(own, 3),
            "slots": slots,
            "profile": opts.profile,
            "note": ("the full gate and any escalated path exceed the 180 s target by design; "
                     "the target applies only to the slice the policy requires for a narrow change"),
        },
        "historical_mode": historical_mode,
        "unsolved_mode": opts.unsolved_mode,
        "task_scenario": opts.task_scenario,
        "assumptions": result.assumptions + [
            "unsolved objectives: %s (carry: an unchanged input carries the base's "
            "recorded unsolved result to the new main; run: the base's two agreeing "
            "runs are charged instead)" % opts.unsolved_mode,
        ] + ([
            "the base record %s is the accepting gate's candidate passes for %s; "
            "candidate runs of a later tree are never read" % (
                os.path.basename(opts.base_record), short(base))
        ] if opts.base_record else [
            "no base record: every job is a cache miss (fail-closed direction)"
        ]),
        "cache": {
            "base_record": opts.base_record,
            "base_record_commit": (record or {}).get("commit"),
            "seed": "the accepting gate's own candidate runs (design-gate-v2 C16); "
                    "this selector reads results, never writes them",
        },
    }


# ---------------------------------------------------------------------------
# output


def text_report(res, out):
    p = res["predicted"]
    out.write("gate-impact selector: %s\n" % res["model"])
    out.write("policy        v%s  sha256:%s\n" % (res["policy_version"], res["policy_digest"]))
    out.write("base          %s\n" % res["base"])
    out.write("candidate     %s\n" % res["candidate"])
    out.write("candidate tree %s\n" % res["candidate_tree"])
    out.write("approval      sha256:%s\n" % res["approval_digest"])
    out.write("              bind any approval to this digest; a new candidate changes it\n")
    out.write("decision      %s\n" % res["decision"])
    for reason in res["decision_reasons"]:
        out.write("              - %s\n" % reason)
    out.write("changed       %d path(s)\n" % len(res["changed"]))
    for item in res["changed"]:
        out.write("  %-42s %-14s%s\n" % (
            item["path"], item["class"],
            "  protected" if item["protected"] else ""))
    out.write("owner review  %s\n" % ("required (protected paths changed)"
                                      if res["owner_review_required"] else "not required by path class"))
    out.write("keys\n")
    for name in ("static", "rust", "web", "scripts"):
        out.write("  %-8s sha256:%s  base sha256:%s%s\n" % (
            name, res["keys"][name], res["base_keys"][name],
            "  unchanged" if res["keys"][name] == res["base_keys"][name] else "  CHANGED"))
    out.write("selected      %d job(s)\n" % len(res["selected"]))
    for job in res["selected"]:
        out.write("  %-44s %-18s %8.3fs  key %s\n" % (
            job["id"], job["why"], job["seconds"], job["key"][:16]))
    out.write("skipped       %d job(s)\n" % len(res["skipped"]))
    for job in res["skipped"]:
        out.write("  %-44s %-18s key %s\n" % (
            job["id"], job["why"], job["key"][:16]))
    if res["decision"] == "full":
        out.write("predicted     %.1f s (measured total; setup %.1f, base %.1f, candidate %.1f)  [%s]\n" % (
            p["wall_seconds"], p["setup_seconds"], p["base_seconds"],
            p["candidate_phase_seconds"], p["method"]))
    else:
        out.write("predicted     %.1f s = setup %.1f + base %.1f + serial %.1f + parallel %.1f  [%s]\n" % (
            p["wall_seconds"], p["setup_seconds"], p["base_seconds"], p["serial_seconds"],
            p["parallel_seconds"], p["method"]))
    out.write("budget        %.0f s -> %s\n" % (
        p["budget_seconds"], "WITHIN" if p["within_budget"] else "OVER"))
    out.write("assumptions\n")
    for item in res["assumptions"]:
        out.write("  - %s\n" % item)


def no_verdict_report(reason, policy_version, out):
    out.write(json.dumps({
        "decision": "no_verdict",
        "fail_closed": True,
        "reason": reason,
        "policy_version": policy_version,
    }, sort_keys=True) + "\n")


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Impact-based landing selector (dry run; not wired into gate.sh).")
    parser.add_argument("--repo", required=True, help="git repository or bare repository")
    parser.add_argument("--base", required=True)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--policy", default=os.path.join(HERE, "policy.json"))
    parser.add_argument("--timings", default=os.path.join(HERE, "timings.json"))
    parser.add_argument("--profile", default="243",
                        help="timings profile from timings.json (243 or 485)")
    parser.add_argument("--base-record", default=None,
                        help="JSON record of the base's accepting-gate passes")
    parser.add_argument("--historical-mode", choices=["cache-only", "nightly"],
                        default="nightly",
                        help="cache-only: today's checks minus cache hits; "
                             "nightly: the proposed design-gate-v2 Decision 1 retirement")
    parser.add_argument("--unsolved-mode", choices=["carry", "run"],
                        default="carry",
                        help="carry: an unchanged input carries the base's recorded "
                             "unsolved objective result to the new main; "
                             "run: re-judge it on the base (fail-closed strict cache)")
    parser.add_argument("--task-scenario", default=None,
                        help="the task's own scenario file name, e.g. 241.sh")
    parser.add_argument("--task-runs", type=int, default=1,
                        help="candidate runs of the task scenario (live P25 uses 3)")
    parser.add_argument("--task-base-cached", action="store_true",
                        help="a cached base 'unsolved' result exists for the task scenario")
    parser.add_argument("--slots", type=int, default=None,
                        help="host scenario slots (default: policy)")
    parser.add_argument("--budget", type=float, default=None,
                        help="budget seconds (default: policy)")
    parser.add_argument("--format", choices=["text", "json"], default="text")
    parser.add_argument("--require-narrow", action="store_true",
                        help="exit 3 unless the decision is narrow")
    opts = parser.parse_args(argv)

    try:
        res = select(opts)
    except Reject as exc:
        no_verdict_report(str(exc), None, sys.stdout)
        return EXIT_REJECT
    except NoVerdict as exc:
        if opts.format == "json":
            no_verdict_report(str(exc), None, sys.stdout)
        else:
            sys.stderr.write("gate-impact: no verdict: %s\n" % exc)
        return EXIT_NO_VERDICT

    if opts.format == "json":
        sys.stdout.write(json.dumps(res, sort_keys=True, indent=2) + "\n")
    else:
        text_report(res, sys.stdout)

    if res["decision"] == "reject":
        return EXIT_REJECT
    if opts.require_narrow and res["decision"] != "narrow":
        return EXIT_NOT_NARROW
    return EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
