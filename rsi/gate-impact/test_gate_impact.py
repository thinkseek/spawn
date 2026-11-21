#!/usr/bin/env python3
"""Tests for the impact-based landing selector (task 490, fix for its reviews).

They are self-contained: a temporary git repository carries the path shapes
the real one has (crates/, web/src, scripts/, the scenario directory, the
serial list, the backlog), and the real policy and 243/485 timings are used
for the prediction.  Nothing here reads the live mirror, runs a check or
writes a cache.

The module is `gate_impact.py`, not `select.py`: a file named select.py on
sys.path[0] shadows the standard library's select module on hosts where
select is an extension module (macOS), and every git call then dies inside
subprocess.

Run: python3 rsi/gate-impact/test_gate_impact.py
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SELECT = os.path.join(HERE, "gate_impact.py")
POLICY = os.path.join(HERE, "policy.json")
TIMINGS = os.path.join(HERE, "timings.json")

JOURNEYS = [
    "check-e2e.sh", "check-j02-lands.sh", "check-j03-human.sh",
    "check-j04-observe.sh", "check-j05-operator.sh", "check-j06-recover-a.sh",
    "check-j07-recover-b.sh", "check-j08-stuck.sh", "check-j09-stall-cancel.sh",
    "check-j10-verdicts.sh", "check-j11-push-drain.sh", "check-j12-hold-told.sh",
]
C1 = ["journey:check-e2e.sh", "journey:check-j03-human.sh",
      "journey:check-j06-recover-a.sh", "journey:check-j07-recover-b.sh"]
SERIAL = ["check-j04-observe.sh"]


def git(repo, *args):
    return subprocess.run(["git", "-C", repo] + list(args),
                          check=True, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, text=True).stdout.strip()


def commit(repo, message):
    subprocess.run(["git", "-C", repo, "add", "-A"], check=True)
    subprocess.run(["git", "-C", repo, "-c", "user.name=t", "-c", "user.email=t@t",
                    "-c", "commit.gpgsign=false", "commit", "-qm", message],
                   check=True)
    return git(repo, "rev-parse", "HEAD")


def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)


def make_repo(root):
    write(os.path.join(root, "Cargo.toml"), '[package]\nname = "x"\n')
    write(os.path.join(root, "Cargo.lock"), "lock\n")
    write(os.path.join(root, "crates/x/src/lib.rs"), "pub fn x() {}\n")
    write(os.path.join(root, "web/index.html"), "<html></html>\n")
    write(os.path.join(root, "web/src/app.js"), "// app\n")
    write(os.path.join(root, "web/package.json"), "{}\n")
    write(os.path.join(root, "scripts/tool.sh"), "#!/bin/sh\n")
    write(os.path.join(root, "docs/readme.md"), "docs\n")
    write(os.path.join(root, "README.md"),
          "rust 2.43 or later is required for the gate\n")
    write(os.path.join(root, "rsi/scenarios/protected"),
          "rsi/\nscripts/\ndeploy/\nweb/\n!web/src/\n!web/test/\n!web/index.html\n"
          "rust-toolchain\nrust-toolchain.toml\n.cargo/\n")
    write(os.path.join(root, "rsi/scenarios/serial"),
          "# scenarios the gate runs alone\n" + "\n".join(SERIAL) + "\n")
    for name in JOURNEYS + ["007.sh", "008.sh", "241.sh", "029.sh"]:
        body = "#!/bin/sh\n# a scenario\n"
        if name == "008.sh":
            body += "grep -q '2\\.43 or later' README.md\n"
        if name == "029.sh":
            body += "# rsi: task-only\n"
        write(os.path.join(root, "rsi/scenarios", name), body)
    write(os.path.join(root, "rsi/scenarios/nightly/023.sh"), "BUSY_MAX=10500\n")
    write(os.path.join(root, "rsi/backlog/1.json"), "{}\n")
    write(os.path.join(root, ".gitignore"), "target/\nrecord.json\npolicy-*.json\n"
          "timings-*.json\nrec-*.json\n")
    git(root, "init", "-q")
    base = commit(root, "base")
    return base


def base_record(root, base, unsolved=()):
    passed = [name for name in JOURNEYS + ["007.sh", "008.sh"]]
    passed += ["check-tests.sh", "check-clippy.sh", "check-web.sh"]
    record = {
        "commit": base,
        "commit_short": base[:8],
        "gate": "fixture",
        "passed": sorted(passed),
        "ran_ci": ["rust-build", "test-listing"],
        "unsolved": sorted(unsolved),
    }
    path = os.path.join(root, "record.json")
    write(path, json.dumps(record, sort_keys=True, indent=2) + "\n")
    return path


def write_record(root, name, record):
    path = os.path.join(root, name)
    write(path, json.dumps(record, sort_keys=True, indent=2) + "\n")
    return path


def run_selector(repo, base, candidate, *extra):
    argv = [sys.executable, SELECT, "--repo", repo, "--base", base,
            "--candidate", candidate, "--policy", POLICY, "--timings", TIMINGS,
            "--format", "json"] + list(extra)
    proc = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          text=True)
    try:
        payload = json.loads(proc.stdout)
    except ValueError:
        payload = None
    return proc.returncode, payload, proc.stderr


class SelectorTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory(prefix="gate-impact-test-")
        cls.root = cls.tmp.name
        cls.base = make_repo(cls.root)
        cls.record = base_record(cls.root, cls.base, unsolved=["241.sh"])

        def case(name, edit):
            git(cls.root, "checkout", "-q", "-B", name, cls.base)
            edit()
            return commit(cls.root, name)

        cls.nightly = case("nightly", lambda: (
            write(os.path.join(cls.root, "rsi/backlog/1.json"), '{"id": 1}\n'),
            write(os.path.join(cls.root, "rsi/scenarios/nightly/023.sh"),
                  "BUSY_MAX=11000\n")))
        cls.nightly_only = case("nightly-only", lambda: write(
            os.path.join(cls.root, "rsi/scenarios/nightly/023.sh"), "BUSY_MAX=11100\n"))
        cls.nightly_other = case("nightly-other", lambda: write(
            os.path.join(cls.root, "rsi/scenarios/nightly/023.sh"), "BUSY_MAX=11200\n"))
        cls.readme = case("readme", lambda: write(
            os.path.join(cls.root, "README.md"), "rust 2.40 or later is required\n"))
        cls.rust = case("rust", lambda: write(
            os.path.join(cls.root, "crates/x/src/lib.rs"), "pub fn y() {}\n"))
        cls.web = case("web", lambda: write(
            os.path.join(cls.root, "web/src/app.js"), "// changed\n"))
        cls.shared = case("shared", lambda: write(
            os.path.join(cls.root, "scripts/tool.sh"), "#!/bin/sh\necho x\n"))
        cls.unknown = case("unknown", lambda: write(
            os.path.join(cls.root, "mystery.txt"), "?\n"))
        cls.buildrs = case("buildrs", lambda: write(
            os.path.join(cls.root, "build.rs"), "fn main() {}\n"))
        cls.nested_build = case("nested-build", lambda: write(
            os.path.join(cls.root, "crates/x/build.rs"), "fn main() {}\n"))
        cls.nested_cargo = case("nested-cargo", lambda: write(
            os.path.join(cls.root, "crates/x/.cargo/config.toml"), "[build]\n"))
        cls.proc_macro = case("proc-macro", lambda: write(
            os.path.join(cls.root, "Cargo.toml"),
            '[package]\nname = "x"\n\n[lib]\nproc-macro = true # a comment\n'))
        cls.protected_prefix = case("protected-prefix", lambda: write(
            os.path.join(cls.root, "rust-toolchain-notes"), "notes\n"))
        git(cls.root, "checkout", "-q", cls.base)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    # -- the narrow classes -------------------------------------------------

    def test_nightly_change_is_narrow_and_under_budget(self):
        code, res, err = run_selector(self.root, self.base, self.nightly_only,
                                      "--base-record", self.record,
                                      "--profile", "243",
                                      "--historical-mode", "nightly",
                                      "--unsolved-mode", "carry")
        self.assertEqual(code, 0, err)
        self.assertEqual(res["decision"], "narrow")
        self.assertEqual([job["id"] for job in res["selected"]], C1)
        self.assertTrue(res["owner_review_required"])
        skipped = {job["id"]: job["why"] for job in res["skipped"]}
        self.assertIn("cache-hit", skipped["check-tests.sh"])
        self.assertIn("cache-hit", skipped["journey:check-j02-lands.sh"])
        self.assertAlmostEqual(res["predicted"]["wall_seconds"], 131.0, places=1)
        self.assertTrue(res["predicted"]["within_budget"])

    def test_cache_only_carries_an_unsolved_objective(self):
        code, res, err = run_selector(self.root, self.base, self.nightly_only,
                                      "--base-record", self.record,
                                      "--profile", "243",
                                      "--historical-mode", "cache-only",
                                      "--unsolved-mode", "carry")
        self.assertEqual(code, 0, err)
        skipped = {job["id"]: job["why"] for job in res["skipped"]}
        self.assertIn("carried", skipped["historical:241.sh"])
        self.assertAlmostEqual(res["predicted"]["wall_seconds"], 131.0, places=1)
        self.assertTrue(res["predicted"]["within_budget"])

    def test_cache_only_without_carry_pays_the_base_and_misses_budget(self):
        code, res, err = run_selector(self.root, self.base, self.nightly_only,
                                      "--base-record", self.record,
                                      "--profile", "243",
                                      "--historical-mode", "cache-only",
                                      "--unsolved-mode", "run")
        self.assertEqual(code, 0, err)
        ids = {job["id"]: job for job in res["selected"]}
        self.assertIn("historical:241.sh", ids)
        self.assertGreater(ids["historical:241.sh"]["base_seconds"], 0)
        self.assertGreater(res["predicted"]["wall_seconds"], 180)
        self.assertFalse(res["predicted"]["within_budget"])

    def test_defaults_follow_the_policy_not_the_least_conservative_mode(self):
        # No --historical-mode and no --unsolved-mode: both come from
        # policy.json (cache-only, run), never the unapproved retirement or
        # the unratified carry.
        code, res, err = run_selector(self.root, self.base, self.nightly_only,
                                      "--base-record", self.record,
                                      "--profile", "243")
        self.assertEqual(code, 0, err)
        self.assertEqual(res["historical_mode"], "cache-only")
        self.assertEqual(res["unsolved_mode"], "run")
        ids = {job["id"] for job in res["selected"]}
        self.assertIn("historical:241.sh", ids)
        self.assertGreater(res["predicted"]["wall_seconds"], 180)

    def test_readme_change_selects_scenario_008(self):
        # 008.sh greps the tree's README.md for the git floor; a README-only
        # change must invalidate 008's key (policy scenario_inputs).
        code, res, err = run_selector(self.root, self.base, self.readme,
                                      "--base-record", self.record,
                                      "--profile", "243",
                                      "--historical-mode", "cache-only",
                                      "--unsolved-mode", "carry")
        self.assertEqual(code, 0, err)
        self.assertEqual(res["decision"], "narrow")
        ids = {job["id"]: job for job in res["selected"]}
        self.assertIn("historical:008.sh", ids)
        self.assertEqual(ids["historical:008.sh"]["why"], "input-changed")
        skipped = {job["id"]: job["why"] for job in res["skipped"]}
        self.assertIn("cache-hit", skipped["historical:007.sh"])
        self.assertGreater(res["predicted"]["wall_seconds"], 131.0)

    def test_backlog_change_selects_check_tests(self):
        # crates/spawn-cli/src/objectives.rs loads the real rsi/backlog under
        # cargo test, so check-tests.sh must run when the backlog changes.
        code, res, err = run_selector(self.root, self.base, self.nightly,
                                      "--base-record", self.record,
                                      "--profile", "243",
                                      "--historical-mode", "nightly",
                                      "--unsolved-mode", "carry")
        self.assertEqual(code, 0, err)
        self.assertEqual(res["decision"], "narrow")
        ids = {job["id"]: job for job in res["selected"]}
        self.assertIn("check-tests.sh", ids)
        self.assertEqual(ids["check-tests.sh"]["why"], "input-changed")
        self.assertGreater(res["predicted"]["wall_seconds"], 180)
        self.assertFalse(res["predicted"]["within_budget"])
        # The 243-type row is no longer 131 s: the backlog edit runs the tests.
        self.assertNotAlmostEqual(res["predicted"]["wall_seconds"], 131.0, places=1)

    def test_web_change_selects_check_tests_which_reads_web_src(self):
        # crates/spawn-cli/tests/cost_page.rs runs node --test on
        # web/src/cost.ts, so a web/src change selects check-tests.sh.
        code, res, err = run_selector(self.root, self.base, self.web,
                                      "--base-record", self.record,
                                      "--profile", "243",
                                      "--historical-mode", "cache-only",
                                      "--unsolved-mode", "run")
        self.assertEqual(code, 0, err)
        ids = {job["id"]: job for job in res["selected"]}
        self.assertIn("check-web.sh", ids)
        self.assertIn("check-tests.sh", ids)
        self.assertEqual(ids["check-tests.sh"]["why"], "input-changed")
        self.assertGreater(res["predicted"]["wall_seconds"], 180)
        self.assertFalse(res["predicted"]["within_budget"])

    def test_serial_scenario_is_charged_alone_like_the_gate(self):
        code, res, err = run_selector(self.root, self.base, self.web,
                                      "--base-record", self.record,
                                      "--profile", "243")
        self.assertEqual(code, 0, err)
        ids = {job["id"]: job for job in res["selected"]}
        self.assertIn("journey:check-j04-observe.sh", ids)
        self.assertEqual(ids["journey:check-j04-observe.sh"]["kind"], "serial")
        self.assertIn("serial: the gate runs it alone",
                      ids["journey:check-j04-observe.sh"]["why"])
        self.assertGreater(res["predicted"]["serial_seconds"], 0)
        # The same run with an empty serial list packs check-j04 into a slot.
        with open(POLICY, encoding="utf-8") as f:
            policy = json.load(f)
        policy["serial_list"] = "rsi/scenarios/no-such-serial-list"
        path = os.path.join(self.root, "policy-no-serial.json")
        write(path, json.dumps(policy, sort_keys=True))
        code2, packed, err2 = run_selector(self.root, self.base, self.web,
                                           "--base-record", self.record,
                                           "--profile", "243",
                                           "--policy", path)
        self.assertEqual(code2, 0, err2)
        self.assertGreater(res["predicted"]["wall_seconds"],
                           packed["predicted"]["wall_seconds"])

    def test_rust_change_runs_tests_and_cannot_meet_the_budget(self):
        code, res, err = run_selector(self.root, self.base, self.rust,
                                      "--base-record", self.record,
                                      "--profile", "485",
                                      "--historical-mode", "nightly",
                                      "--task-scenario", "241.sh",
                                      "--task-base-cached")
        self.assertEqual(code, 0, err)
        ids = {job["id"]: job for job in res["selected"]}
        self.assertIn("check-tests.sh", ids)
        self.assertEqual(ids["check-tests.sh"]["why"], "input-changed")
        self.assertIn("check-web.sh", {j["id"] for j in res["skipped"]})
        self.assertIn("task:241.sh", ids)
        self.assertGreater(res["predicted"]["wall_seconds"], 180)
        self.assertFalse(res["predicted"]["within_budget"])

    def test_shared_change_escalates_to_the_full_gate(self):
        code, res, err = run_selector(self.root, self.base, self.shared,
                                      "--base-record", self.record,
                                      "--profile", "243")
        self.assertEqual(code, 0, err)
        self.assertEqual(res["decision"], "full")
        self.assertAlmostEqual(res["predicted"]["wall_seconds"], 1973.79, places=1)
        self.assertFalse(res["predicted"]["within_budget"])
        self.assertTrue(any("scripts/tool.sh" in reason
                            for reason in res["decision_reasons"]))

    def test_unknown_path_fails_closed_to_the_full_gate(self):
        code, res, err = run_selector(self.root, self.base, self.unknown,
                                      "--base-record", self.record,
                                      "--profile", "243")
        self.assertEqual(code, 0, err)
        self.assertEqual(res["decision"], "full")
        self.assertTrue(res["fail_closed"])
        self.assertIn("mystery.txt", " ".join(res["decision_reasons"]))
        self.assertAlmostEqual(res["predicted"]["wall_seconds"], 1973.79, places=1)

    # -- base record binding ------------------------------------------------

    def test_base_record_without_a_commit_is_no_verdict(self):
        with open(self.record, encoding="utf-8") as f:
            record = json.load(f)
        del record["commit"]
        for name, candidate_record in (
                ("rec-nocommit.json", record),
                ("rec-empty.json", dict(record, commit="")),
                ("rec-onechar.json", dict(record, commit=self.base[:1])),
                ("rec-short.json", dict(record, commit=self.base[:7]))):
            path = write_record(self.root, name, candidate_record)
            code, res, _ = run_selector(self.root, self.base, self.nightly_only,
                                        "--base-record", path)
            self.assertEqual(code, 2, name)
            self.assertEqual(res["decision"], "no_verdict", name)
            self.assertTrue(res["fail_closed"], name)

    def test_cache_hit_needs_a_recorded_result(self):
        # No base record: every unchanged key is a cache miss, never a skip.
        # A selector that skipped on key equality alone (the mutant this test
        # kills) would skip here with no authority to do so.
        code, res, err = run_selector(self.root, self.base, self.nightly_only,
                                      "--profile", "243",
                                      "--historical-mode", "cache-only",
                                      "--unsolved-mode", "carry")
        self.assertEqual(code, 0, err)
        self.assertEqual(res["decision"], "narrow")
        self.assertTrue(res["selected"])
        self.assertFalse([job for job in res["skipped"]
                          if "cache-hit" in job["why"]])
        self.assertIn("check-tests.sh", {job["id"] for job in res["selected"]})
        self.assertGreater(res["predicted"]["wall_seconds"], 180)
        self.assertFalse(res["predicted"]["within_budget"])

    def test_missing_record_file_is_no_verdict(self):
        code, res, _ = run_selector(self.root, self.base, self.nightly_only,
                                    "--base-record", os.path.join(self.root, "nope.json"))
        self.assertEqual(code, 2)
        self.assertEqual(res["decision"], "no_verdict")

    def test_base_record_for_another_commit_is_no_verdict(self):
        code, res, _ = run_selector(self.root, self.base, self.nightly_only,
                                    "--base-record",
                                    os.path.join(HERE, "records", "base-5e416443.json"))
        self.assertEqual(code, 2)
        self.assertEqual(res["decision"], "no_verdict")

    # -- tree, ancestry and static trust ------------------------------------

    def test_subdirectory_repo_still_sees_the_scenarios(self):
        # --repo may be any path inside the worktree (`git -C` accepts one);
        # the scenario listing must not silently come back empty.
        code, res, err = run_selector(os.path.join(self.root, "crates"),
                                      self.base, self.nightly_only,
                                      "--base-record", self.record,
                                      "--profile", "243",
                                      "--historical-mode", "nightly",
                                      "--unsolved-mode", "carry")
        self.assertEqual(code, 0, err)
        self.assertEqual(res["decision"], "narrow")
        self.assertTrue(res["skipped"])
        self.assertTrue(any(job["id"].startswith("historical:")
                            for job in res["skipped"]))
        self.assertAlmostEqual(res["predicted"]["wall_seconds"], 131.0, places=1)

    def test_base_that_is_not_an_ancestor_is_no_verdict(self):
        code, res, _ = run_selector(self.root, self.nightly_only, self.base,
                                    "--base-record", self.record,
                                    "--profile", "243")
        self.assertEqual(code, 2)
        self.assertEqual(res["decision"], "no_verdict")
        self.assertIn("ancestor", res["reason"])

    def test_nested_build_time_code_is_refused(self):
        for candidate in (self.nested_build, self.nested_cargo):
            code, res, err = run_selector(self.root, self.base, candidate,
                                          "--base-record", self.record,
                                          "--profile", "243")
            self.assertEqual(code, 1, err)
            self.assertEqual(res["decision"], "reject")

    def test_proc_macro_with_a_trailing_comment_is_refused(self):
        code, res, err = run_selector(self.root, self.base, self.proc_macro,
                                      "--base-record", self.record,
                                      "--profile", "243")
        self.assertEqual(code, 1, err)
        self.assertEqual(res["decision"], "reject")

    def test_protected_matching_is_the_gates_prefix_rule(self):
        code, res, err = run_selector(self.root, self.base, self.protected_prefix,
                                      "--base-record", self.record,
                                      "--profile", "243")
        self.assertEqual(code, 0, err)
        self.assertTrue(res["owner_review_required"])
        self.assertEqual(res["protected_paths"], ["rust-toolchain-notes"])

    # -- malformed input ----------------------------------------------------

    def test_unknown_policy_version_is_no_verdict(self):
        with open(POLICY, encoding="utf-8") as f:
            policy = json.load(f)
        policy["policy_version"] = 999
        path = os.path.join(self.root, "policy-v999.json")
        write(path, json.dumps(policy))
        code, res, _ = run_selector(self.root, self.base, self.nightly_only,
                                    "--base-record", self.record,
                                    "--policy", path)
        self.assertEqual(code, 2)
        self.assertEqual(res["decision"], "no_verdict")

    def test_malformed_policy_is_no_verdict_not_a_reject(self):
        with open(POLICY, encoding="utf-8") as f:
            policy = json.load(f)
        broken = dict(policy)
        del broken["jobs"]
        path = os.path.join(self.root, "policy-nojobs.json")
        write(path, json.dumps(broken))
        code, res, _ = run_selector(self.root, self.base, self.nightly_only,
                                    "--base-record", self.record,
                                    "--policy", path)
        self.assertEqual(code, 2)
        self.assertEqual(res["decision"], "no_verdict")
        short = dict(policy)
        short["path_classes"] = []
        path = os.path.join(self.root, "policy-noclasses.json")
        write(path, json.dumps(short))
        code, res, _ = run_selector(self.root, self.base, self.nightly_only,
                                    "--base-record", self.record,
                                    "--policy", path)
        self.assertEqual(code, 2)
        self.assertEqual(res["decision"], "no_verdict")

    def test_negative_slots_are_no_verdict(self):
        code, res, _ = run_selector(self.root, self.base, self.nightly_only,
                                    "--base-record", self.record,
                                    "--profile", "243", "--slots", "-1")
        self.assertEqual(code, 2)
        self.assertEqual(res["decision"], "no_verdict")

    def test_missing_timing_is_no_verdict(self):
        with open(TIMINGS, encoding="utf-8") as f:
            timings = json.load(f)
        del timings["profiles"]["243"]["jobs"]["check-tests.sh"]
        path = os.path.join(self.root, "timings-missing.json")
        write(path, json.dumps(timings))
        code, res, _ = run_selector(self.root, self.base, self.rust,
                                    "--base-record", self.record,
                                    "--timings", path)
        self.assertEqual(code, 2)
        self.assertEqual(res["decision"], "no_verdict")
        self.assertIn("check-tests.sh", res["reason"])

    # -- approval -----------------------------------------------------------

    def test_approval_is_bound_to_the_candidate_not_the_selection(self):
        # Two distinct nightly-only candidates select exactly the same jobs;
        # the digest must still differ, because it covers the candidate tree.
        _, first, _ = run_selector(self.root, self.base, self.nightly_only,
                                   "--base-record", self.record)
        _, again, _ = run_selector(self.root, self.base, self.nightly_only,
                                   "--base-record", self.record)
        _, other, _ = run_selector(self.root, self.base, self.nightly_other,
                                   "--base-record", self.record)
        ids = lambda res: [job["id"] for job in res["selected"]]
        self.assertEqual(first["approval_digest"], again["approval_digest"])
        self.assertEqual(ids(first), ids(other))
        self.assertNotEqual(first["approval_digest"], other["approval_digest"])

    def test_no_change_is_no_verdict(self):
        code, res, _ = run_selector(self.root, self.base, self.base,
                                    "--base-record", self.record)
        self.assertEqual(code, 2)
        self.assertEqual(res["decision"], "no_verdict")


if __name__ == "__main__":
    unittest.main(verbosity=2)
