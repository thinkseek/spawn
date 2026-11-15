#!/usr/bin/env python3
"""Synthetic, offline tests of shadow_observe. Fake commands only; nothing on
the host is probed, stopped or changed. Run: python3 -I test_shadow_observe.py"""
import ast
import re
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import shadow_observe as so  # noqa: E402

SECRET = "postgres://made-up-user:MADE-UP-PASSWORD-9f3@127.0.0.1/db"


def fake(path, body):
    with open(path, "w") as fh:
        fh.write("#!/bin/sh\n" + body)
    os.chmod(path, 0o755)


class T(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="shadow-test-")
        b = os.path.join(self.tmp, "bin")
        os.mkdir(b)
        self.calls = os.path.join(self.tmp, "calls")
        # each fake records only that it ran, with its arguments
        fake(os.path.join(b, "systemctl"), 'echo "systemctl $*" >> %s\n'
             'echo ActiveState=active; echo SubState=running; echo NRestarts=2; '
             'echo ActiveEnterTimestampMonotonic=12345\n' % self.calls)
        fake(os.path.join(b, "git"), 'echo "git $*" >> %s\necho %d\n' % (self.calls, int(time.time()) - 7200 - 600))
        fake(os.path.join(b, "tmux"), 'echo "tmux $*" >> %s\necho "0.0 0 %d"; echo "1.0 0 %d"\n'
             % (self.calls, int(time.time()) - 5, int(time.time()) - 40))
        for name in ("queue", "loop_log", "version"):
            open(os.path.join(self.tmp, name), "w").close()
        os.mkdir(os.path.join(self.tmp, "gate"))
        open(os.path.join(self.tmp, "gate", "g1"), "w").close()
        with open(os.path.join(self.tmp, "boot"), "w") as fh:
            fh.write("11111111-2222-3333-4444-555555555555\n")
        self.cfg = so.default_config()
        self.cfg.update(
            state_dir=os.path.join(self.tmp, "state"),
            disable_file=os.path.join(self.tmp, "disabled"),
            exclude_file=os.path.join(self.tmp, "exclude"),
            tools={t: os.path.join(b, t) for t in ("systemctl", "git", "tmux")},
            store=os.path.join(self.tmp, "store.git"),
            paths={"queue": os.path.join(self.tmp, "queue"), "loop_log": os.path.join(self.tmp, "loop_log"),
                   "gate_logs": os.path.join(self.tmp, "gate"),
                   "keep_test_version": os.path.join(self.tmp, "version")},
            page=("127.0.0.1", 1),            # nothing listens: the page reads "not_answering"
            boot_id_file=os.path.join(self.tmp, "boot"),
            probe_timeout_s=1.0, run_deadline_s=6.0, min_free_bytes=0)

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def samples(self):
        d = self.cfg["state_dir"]
        out = []
        for f in sorted(os.listdir(d)) if os.path.isdir(d) else []:
            if f.startswith("samples-"):
                with open(os.path.join(d, f)) as fh:
                    out += [json.loads(l) for l in fh]
        return out

    def raw(self):
        d = self.cfg["state_dir"]
        return "".join(open(os.path.join(d, f)).read() for f in os.listdir(d) if f != "lock")

    # ---- the basic shape ----
    def test_one_sample_observed_and_bounded(self):
        self.assertEqual(so.observe(self.cfg), "ok")
        (s,) = self.samples()
        self.assertEqual(s["seq"], 1)
        u = s["components"]["unit:spawn-loop.service"]
        self.assertEqual((u["basis"], u["process"], u["restarts"]), ("observed", "up", 2))
        self.assertEqual(s["components"]["page"]["read_path"], "not_answering")
        self.assertLess(len(json.dumps(s)), self.cfg["max_line_bytes"])
        self.assertEqual(stat.S_IMODE(os.stat(self.cfg["state_dir"]).st_mode), 0o700)

    # ---- false activity: panes busy, main not moving, is NOT progress ----
    def test_false_activity_is_not_main_moving(self):
        so.observe(self.cfg)
        p = self.samples()[0]["movement"]
        self.assertEqual(p["label"], "activity_only")
        self.assertEqual(p["basis"], "inferred")
        self.assertGreater(p["main_tip_commit_age_s"], self.cfg["main_moved_recent_s"])
        self.assertLess(p["activity_age_s"], 60)

    def test_no_sample_ever_claims_progress_or_validation(self):
        # A fresh main tip with a fresh gate folder is still only "main moved":
        # the observer has no evidence linking a gate to that commit.
        fake(self.cfg["tools"]["git"], 'echo %d\n' % (int(time.time()) - 30))
        so.observe(self.cfg)
        s = self.samples()[0]
        self.assertEqual(s["movement"]["label"], "main_moved_recent")
        raw = self.raw().lower()
        for claim in ("milestone", "progress", "validated", "verified", "healthy", "useful"):
            self.assertNotIn(claim, raw)
        with open(os.path.join(HERE, "shadow_observe.py")) as fh:
            labels = set(re.findall(r'out\["label"\] = "([a-z_]+)"', fh.read()))
        self.assertEqual(labels, {"unknown", "main_moved_recent", "activity_only", "quiet"})

    def test_unit_up_is_process_only_never_healthy(self):
        so.observe(self.cfg)
        self.assertNotIn("healthy", json.dumps(self.samples()[0]))

    # ---- boots, clocks ----
    def test_old_boot_voids_the_gap(self):
        so.observe(self.cfg)
        with open(self.cfg["boot_id_file"], "w") as fh:
            fh.write("99999999-2222-3333-4444-555555555555\n")
        so.observe(self.cfg)
        a, b = self.samples()
        self.assertEqual(b["seq"], 2)
        self.assertTrue(b["boot_changed"])
        self.assertIsNone(b["gap_s"])
        self.assertTrue(b["discontinuity"])
        self.assertFalse(a["boot_changed"])

    def test_long_gap_in_one_boot_is_a_discontinuity(self):
        so.observe(self.cfg)
        st = os.path.join(self.cfg["state_dir"], "state.json")
        s = json.load(open(st))
        s["mono_s"] -= 500
        json.dump(s, open(st, "w"))
        so.observe(self.cfg)
        b = self.samples()[1]
        self.assertGreater(b["gap_s"], 400)
        self.assertTrue(b["discontinuity"])

    def test_corrupt_state_is_said_not_hidden(self):
        so.observe(self.cfg)
        with open(os.path.join(self.cfg["state_dir"], "state.json"), "w") as fh:
            fh.write("{not json")
        so.observe(self.cfg)
        b = self.samples()[1]
        self.assertEqual(b["state"], "corrupt")
        self.assertTrue(b["discontinuity"])

    # ---- malformed evidence and redaction ----
    def test_malformed_evidence_is_unknown_and_never_copied(self):
        fake(self.cfg["tools"]["systemctl"],
             'echo "ActiveState=%s"; echo SubState=running; echo NRestarts=x; echo "Environment=%s"\n' % (SECRET, SECRET))
        fake(self.cfg["tools"]["git"], 'echo "%s"\n' % SECRET)
        fake(self.cfg["tools"]["tmux"], 'echo "%s"\n' % SECRET)
        self.assertEqual(so.observe(self.cfg), "ok")
        s = self.samples()[0]
        self.assertEqual(s["components"]["unit:spawn-serve.service"], {"probe": "malformed", "basis": "unknown", "process": "unknown"})
        self.assertEqual(s["components"]["store_main"]["probe"], "malformed")
        self.assertEqual(s["components"]["session:supervisor"]["probe"], "malformed")
        self.assertEqual(s["movement"]["label"], "unknown")
        raw = self.raw()
        for piece in (SECRET, "MADE-UP-PASSWORD", "made-up-user", "postgres"):
            self.assertNotIn(piece, raw)

    # ---- rev-g F1: each free-text check, alone ----
    def test_free_text_unit_state_with_valid_restarts_is_malformed(self):
        for field in ("ActiveState", "SubState"):
            other = "SubState=running" if field == "ActiveState" else "ActiveState=active"
            fake(self.cfg["tools"]["systemctl"], 'echo "%s=%s"; echo %s; echo NRestarts=0\n' % (field, SECRET, other))
            self.assertEqual(so.observe(self.cfg), "ok")
            s = self.samples()[-1]
            self.assertEqual(s["components"]["unit:spawn-loop.service"],
                             {"probe": "malformed", "basis": "unknown", "process": "unknown"}, field)
            for piece in (SECRET, "MADE-UP", "postgres"):
                self.assertNotIn(piece, self.raw())
        fake(self.cfg["tools"]["systemctl"], 'echo "ActiveState=Free Text"; echo SubState=running; echo NRestarts=0\n')
        so.observe(self.cfg)
        self.assertEqual(self.samples()[-1]["components"]["unit:spawn-loop.service"]["probe"], "malformed")
        self.assertNotIn("Free Text", self.raw())

    def test_free_text_boot_id_is_unknown_and_never_copied(self):
        with open(self.cfg["boot_id_file"], "w") as fh:
            fh.write("boot %s\n" % SECRET)
        self.assertEqual(so.observe(self.cfg), "ok")
        self.assertEqual(self.samples()[0]["boot_id"], "unknown")
        self.assertTrue(self.samples()[0]["discontinuity"])
        for piece in (SECRET, "MADE-UP", "postgres"):
            self.assertNotIn(piece, self.raw())

    def test_pane_lines_with_extra_text_or_word_keys_are_dropped(self):
        now = int(time.time())
        fake(self.cfg["tools"]["tmux"],
             'echo "0.0 0 %d trailing-TEXT-here"; echo "x.y 0 %d"; echo "wordkey 0 %d"; echo "1.0 0 %d"\n'
             % (now, now, now, now - 30))
        self.assertEqual(so.observe(self.cfg), "ok")
        c = self.samples()[0]["components"]["session:supervisor"]
        self.assertEqual(sorted(c["panes"]), ["1.0"])
        self.assertEqual(c["malformed_lines"], 3)
        for piece in ("trailing-TEXT-here", "wordkey", "x.y"):
            self.assertNotIn(piece, self.raw())

    def test_commit_time_out_of_bounds_is_malformed(self):
        for body in ("echo 0\n", "echo -5\n", "echo %d\n" % (int(time.time()) + 2 * 86400), "echo 12x\n"):
            fake(self.cfg["tools"]["git"], body)
            so.observe(self.cfg)
            s = self.samples()[-1]
            self.assertEqual(s["components"]["store_main"], {"probe": "malformed", "basis": "unknown"}, body)
            self.assertEqual(s["movement"]["label"], "unknown")

    def test_todays_file_alone_at_the_cap_is_kept_and_nothing_is_written(self):
        os.makedirs(self.cfg["state_dir"], mode=0o700)
        today = os.path.join(self.cfg["state_dir"], "samples-%s.jsonl" % time.strftime("%Y%m%d", time.gmtime()))
        with open(today, "w") as fh:
            fh.write("y" * 6000)
        self.cfg["max_total_bytes"] = 5000
        self.assertEqual(so.observe(self.cfg), "cap_reached_no_write")
        self.assertEqual(os.path.getsize(today), 6000, "kept, not deleted, not grown")
        self.assertFalse(os.path.exists(os.path.join(self.cfg["state_dir"], "state.json")))

    def test_children_get_a_scrubbed_environment(self):
        envf = os.path.join(self.tmp, "envseen")
        fake(self.cfg["tools"]["git"], 'env | cut -d= -f1 | sort > %s\necho %d\n' % (envf, int(time.time()) - 60))
        os.environ["SHADOW_TEST_CANARY"] = "CANARY-VALUE-77"
        os.environ["SPAWN_DATABASE_URL"] = SECRET
        try:
            so.observe(self.cfg)
        finally:
            del os.environ["SHADOW_TEST_CANARY"], os.environ["SPAWN_DATABASE_URL"]
        seen = set(open(envf).read().split())
        self.assertFalse(seen & {"SHADOW_TEST_CANARY", "SPAWN_DATABASE_URL"})
        self.assertLessEqual(seen - {"PWD", "SHLVL", "_", "OLDPWD"},
                             {"PATH", "LC_ALL", "GIT_OPTIONAL_LOCKS", "HOME", "XDG_RUNTIME_DIR",
                              "DBUS_SESSION_BUS_ADDRESS", "TMUX_TMPDIR"})

    def test_symlinked_state_dir_is_refused(self):
        real = os.path.join(self.tmp, "elsewhere")
        os.mkdir(real)
        os.symlink(real, self.cfg["state_dir"])
        self.assertEqual(so.observe(self.cfg), "state_dir_refused")
        self.assertEqual(os.listdir(real), [])

    def test_symlinked_sample_file_is_not_followed(self):
        os.makedirs(self.cfg["state_dir"], mode=0o700)
        target = os.path.join(self.tmp, "victim")
        open(target, "w").close()
        os.symlink(target, os.path.join(self.cfg["state_dir"],
                                        "samples-%s.jsonl" % time.strftime("%Y%m%d", time.gmtime())))
        self.assertEqual(so.observe(self.cfg), "write_error")
        self.assertEqual(os.path.getsize(target), 0)

    def test_no_environment_or_argv_in_telemetry(self):
        os.environ["SHADOW_TEST_CANARY"] = "CANARY-VALUE-77"
        try:
            so.observe(self.cfg)
        finally:
            del os.environ["SHADOW_TEST_CANARY"]
        self.assertNotIn("CANARY", self.raw())

    # ---- hung probe ----
    def test_hung_probe_times_out_is_unknown_and_its_child_is_gone(self):
        pidf = os.path.join(self.tmp, "pid")
        fake(self.cfg["tools"]["git"], 'echo $$ > %s\nsleep 60\n' % pidf)
        t0 = time.monotonic()
        self.assertEqual(so.observe(self.cfg), "ok")
        self.assertLess(time.monotonic() - t0, 5)
        s = self.samples()[0]
        self.assertEqual(s["components"]["store_main"], {"probe": "timeout", "basis": "unknown"})
        self.assertEqual(s["movement"]["label"], "unknown")
        pid = int(open(pidf).read())
        time.sleep(0.2)
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    def test_run_deadline_skips_the_rest(self):
        fake(self.cfg["tools"]["systemctl"], "sleep 60\n")
        self.cfg.update(probe_timeout_s=0.5, run_deadline_s=1.2)
        t0 = time.monotonic()
        so.observe(self.cfg)
        self.assertLess(time.monotonic() - t0, 4)
        c = self.samples()[0]["components"]
        self.assertEqual(c["unit:spawn-loop.service"]["probe"], "timeout")
        self.assertEqual(c["session:cc-astra"]["probe"], "skipped_deadline")

    # ---- exclusions, disable ----
    def test_excluded_component_is_not_probed(self):
        with open(self.cfg["exclude_file"], "w") as fh:
            fh.write("# owner stop\nunit:spawn-loop.service\nsession:cc-astra\n")
        so.observe(self.cfg)
        c = self.samples()[0]["components"]
        self.assertEqual(c["unit:spawn-loop.service"], {"probe": "excluded", "basis": "excluded"})
        calls = open(self.calls).read()
        self.assertNotIn("spawn-loop.service", calls)
        self.assertNotIn("cc-astra", calls)
        self.assertIn("spawn-serve.service", calls)

    def test_unreadable_exclude_file_excludes_everything(self):
        os.mkdir(self.cfg["exclude_file"])        # a folder: open() fails
        so.observe(self.cfg)
        s = self.samples()[0]
        self.assertTrue(s["exclude_unreadable"])
        self.assertTrue(all(c["probe"] == "excluded" for c in s["components"].values()))
        self.assertFalse(os.path.exists(self.calls))

    def test_disable_switch_probes_and_writes_nothing(self):
        open(self.cfg["disable_file"], "w").close()
        open(self.cfg["exclude_file"], "w").write("unit:spawn-loop.service\n")
        before = sorted(os.listdir(self.tmp))
        self.assertEqual(so.observe(self.cfg), "disabled")
        self.assertEqual(sorted(os.listdir(self.tmp)), before, "no state folder, no sample, no call")
        self.assertEqual(open(self.cfg["exclude_file"]).read(), "unit:spawn-loop.service\n",
                         "disabling never clears the owner's exclusions")

    def test_only_listed_components_ever_appear(self):
        so.observe(self.cfg)
        names = set(self.samples()[0]["components"])
        want = {"unit:" + u for u in so.UNITS} | {"session:" + s for s in so.SESSIONS} | {
            "page", "store_main", "queue", "loop_log", "gate_logs", "keep_test_version"}
        self.assertEqual(names, want)
        self.assertNotIn("superinfer", open(os.path.join(HERE, "shadow_observe.py")).read().lower())

    # ---- non-overlap ----
    def test_second_run_while_one_holds_the_lock_is_busy(self):
        import fcntl
        os.makedirs(self.cfg["state_dir"], mode=0o700)
        fd = os.open(os.path.join(self.cfg["state_dir"], "lock"), os.O_WRONLY | os.O_CREAT, 0o600)
        fcntl.flock(fd, fcntl.LOCK_EX)
        try:
            self.assertEqual(so.observe(self.cfg), "busy")
            self.assertEqual(self.samples(), [])
            self.assertFalse(os.path.exists(self.calls))
        finally:
            os.close(fd)

    # ---- telemetry limits, disk ----
    def test_total_cap_removes_only_own_oldest_samples(self):
        os.makedirs(self.cfg["state_dir"], mode=0o700)
        for day in ("20200101", "20200102"):
            with open(os.path.join(self.cfg["state_dir"], "samples-%s.jsonl" % day), "w") as fh:
                fh.write("x" * 4000)
        keep = os.path.join(self.cfg["state_dir"], "not-a-sample.txt")
        open(keep, "w").write("x" * 9000)
        self.cfg["max_total_bytes"] = 5000
        so.observe(self.cfg)
        left = sorted(os.listdir(self.cfg["state_dir"]))
        self.assertNotIn("samples-20200101.jsonl", left)
        self.assertIn("samples-20200102.jsonl", left)
        self.assertTrue(os.path.exists(keep))

    def test_oversize_sample_is_truncated_not_written_whole(self):
        self.cfg["max_line_bytes"] = 1500
        self.assertIn(so.observe(self.cfg), ("ok", "sample_too_large"))
        for line in self.raw().splitlines():
            self.assertLessEqual(len(line) + 1, 1500) if line.startswith('{"boot') else None

    def test_low_disk_writes_nothing_and_says_so(self):
        self.cfg["min_free_bytes"] = 2 ** 60
        self.assertEqual(so.observe(self.cfg), "disk_low_no_write")
        self.assertEqual(self.samples(), [])
        self.assertFalse(os.path.exists(os.path.join(self.cfg["state_dir"], "state.json")))

    # ---- no action path: proven from the source, not from behaviour ----
    def test_no_action_path_in_the_source(self):
        src = open(os.path.join(HERE, "shadow_observe.py")).read()
        tree = ast.parse(src)
        self.assertEqual(so.ALLOWED, (
            ("systemctl", "--user", "show", None, "-p", "ActiveState,SubState,NRestarts,ActiveEnterTimestampMonotonic"),
            ("git", "--git-dir", None, "log", "-1", "--format=%ct", "refs/heads/main"),
            ("tmux", "list-panes", "-s", "-t", None, "-F", "#{window_index}.#{pane_index} #{pane_dead} #{window_activity}")))
        imports = set()
        for n in ast.walk(tree):
            if isinstance(n, ast.Import):
                imports |= {a.name.split(".")[0] for a in n.names}
            if isinstance(n, ast.ImportFrom):
                imports.add(n.module.split(".")[0])
        self.assertEqual(imports, {"errno", "fcntl", "json", "os", "re", "signal", "stat", "subprocess", "sys", "time", "http"})
        # every process start and every kill is inside run_probe
        owners = {}
        for fn in [n for n in ast.walk(tree) if isinstance(n, ast.FunctionDef)]:
            for n in ast.walk(fn):
                if isinstance(n, ast.Attribute) and isinstance(n.value, ast.Name):
                    owners.setdefault("%s.%s" % (n.value.id, n.attr), set()).add(fn.name)
        self.assertEqual(owners.get("subprocess.Popen"), {"run_probe"})
        self.assertEqual(owners.get("os.killpg"), {"run_probe"})
        for banned in ("subprocess.run", "subprocess.call", "subprocess.check_output", "os.system", "os.kill",
                       "os.popen", "os.execv", "os.execvp", "os.fork", "os.rename", "os.rmdir", "os.truncate"):
            self.assertNotIn(banned, owners, banned)
        self.assertEqual(owners.get("os.unlink"), {"enforce_total"})
        # words of action never appear as command words
        strings = {n.value for n in ast.walk(tree) if isinstance(n, ast.Constant) and isinstance(n.value, str)
                   and len(n.value) < 40}
        for banned in ("restart", "start", "stop", "kill", "send-keys", "reload", "enable", "disable", "push",
                       "commit", "deploy", "psql", "curl", "ssh", "sudo", "claude", "POST", "PUT", "DELETE"):
            self.assertNotIn(banned, strings, banned)

    def test_run_probe_refuses_anything_off_the_allowlist(self):
        P, F, st = so.SHOW_PROPS, so.PANE_FORMAT, self.cfg["store"]
        for argv in (["systemctl", "--user", "restart", "spawn-loop.service"], ["systemctl", "show", "x"],
                     ["systemctl", "--user", "show", "other.service", "-p", P],
                     ["systemctl", "--user", "show", "spawn-loop.service", "-p", "Environment"],
                     ["systemctl", "--user", "show", "spawn-loop.service", "-p", P, "--all"],
                     ["tmux", "send-keys"], ["tmux", "list-panes", "-s", "-t", "other", "-F", F],
                     ["tmux", "list-panes", "-a", "-t", "supervisor", "-F", F],
                     ["git", "push"], ["git", "--git-dir", st, "push"],
                     ["git", "--git-dir", st, "log", "-1", "--format=%B", "refs/heads/main"],
                     ["git", "--git-dir", "/elsewhere", "log", "-1", "--format=%ct", "refs/heads/main"],
                     ["sh", "-c", "true"], []):
            with self.assertRaises(RuntimeError):
                so.run_probe(self.cfg, argv, time.monotonic() + 5)

    def test_page_probe_refuses_any_other_host(self):
        self.cfg["page"] = ("example.com", 80)
        self.assertEqual(so.probe_page(self.cfg, time.monotonic() + 5)["probe"], "refused")

    def test_fakes_received_only_read_commands(self):
        so.observe(self.cfg)
        for line in open(self.calls).read().splitlines():
            self.assertRegex(line, r"^(systemctl --user show spawn-[a-z-]+\.service -p [A-Za-z,]+"
                                   r"|git --git-dir \S+ log -1 --format=%ct refs/heads/main"
                                   r"|tmux list-panes -s -t (supervisor|cc-astra) -F .*)$")


if __name__ == "__main__":
    unittest.main()
