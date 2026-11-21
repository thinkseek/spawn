#!/usr/bin/env python3
"""Synthetic tests for agent_presence.py. Every source here is made up; a
made-up page service runs on a free local port inside the test. Nothing on
the host is read or changed. Run: python3 -I test_agent_presence.py"""
import fcntl
import http.server
import json
import os
import shutil
import stat
import sys
import tempfile
import threading
import time
import unittest
import urllib.parse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import agent_presence as ap  # noqa: E402

SECRET = "sk-PLANTED-0123456789abcdef"
NOW = time.time()   # the exporter reads the real clock; agent times are relative to it


def t_ms(age_s):
    return int((NOW - age_s) * 1000)


def agent(i, live=True, turn_open=False, last_age=30, turn_age=40, role="worker",
          tool=None, ended_age=None, **extra):
    a = {
        "agent_id": "agent_worker_objective_%d_%s" % (i, SECRET),
        "session_id": "session_%d_%s" % (i, SECRET),
        "execution_id": "exec_%d" % i,
        "label": "label %s" % SECRET,
        "task": "do the thing with %s at /home/example/secret/path" % SECRET,
        "mission_name": "mission %s" % SECRET,
        "task_title": "title %s" % SECRET,
        "attention": True,
        "attention_since_ms": t_ms(50),
        "judged_at_ms": t_ms(20),
        "state_since_ms": t_ms(60),
        "role_rank": 2,
        "engine": "claude",
        "role": role,
        "live": live,
        "state": "WORKING",           # the judge's word: must never be used
        "judgement": {"state": "WORKING", "reason": SECRET,
                      "evidence": {"screen": SECRET, "screen_tail": SECRET, "task": SECRET}},
        "started_at_ms": t_ms(7200),
        "ended_at_ms": t_ms(ended_age) if ended_age is not None else None,
        "activity": {
            "current_tool": tool,
            "current_tool_summary": SECRET if tool else None,
            "jobs": [{"id": SECRET, "kind": SECRET}],
            "last_event_name": "tool.started " + SECRET,
            "last_event_at_ms": t_ms(last_age) if last_age is not None else None,
            "turn_open": turn_open if live else None,
            "turn_at_ms": t_ms(turn_age) if turn_age is not None else None,
            "event_count": 5, "tool_calls": 3, "executions": 1,
        },
    }
    a.update(extra)
    return a


def ev(seq, source, execution, age_s, name="tool.started"):
    return {"session_seq": seq, "event_id": 1000 + seq, "execution_id": execution, "source": source,
            "event_name": name, "detail": {"summary": SECRET, "path": "/home/example/" + SECRET},
            "recorded_at_ms": t_ms(age_s)}


def cfg(tmp, port=1):
    c = ap.default_config()
    c.update(out_dir=os.path.join(tmp, "out"), disable_file=os.path.join(tmp, "disabled"),
             port=port, timeout_s=2.0, min_interval_s=0.0, min_free_bytes=0)
    return c


class Server:
    """A made-up page service: GET /api/fleet answers what the test sets;
    GET /api/session/<id> answers from the test's timelines."""

    def __init__(self):
        self.status, self.body, self.headers, self.paths = 200, b"{}", {}, []
        self.timelines = {}          # session_id -> list of events (ascending seq)
        outer = self

        class H(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                outer.paths.append(self.path)
                body, status = outer.body, outer.status
                if self.path.startswith("/api/session/") and status == 200:
                    u = urllib.parse.urlsplit(self.path)
                    sid = urllib.parse.unquote(u.path[len("/api/session/"):])
                    q = dict(urllib.parse.parse_qsl(u.query))
                    tl = outer.timelines.get(sid, [])
                    after, limit = int(q.get("after", 0)), int(q.get("limit", 50))
                    page = [e for e in tl if e["session_seq"] > after][:limit]
                    body = json.dumps({"session_id": sid, "state": {"observation": SECRET},
                                       "executions": [], "open_tools": [{"tool_name": SECRET}],
                                       "freshness": {"last_session_seq": tl[-1]["session_seq"] if tl else 0,
                                                     "last_event_at_ms": t_ms(1)},
                                       "timeline": page}).encode()
                self.send_response(status)
                for k, v in outer.headers.items():
                    self.send_header(k, v)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *a):
                pass

        self.httpd = http.server.HTTPServer(("127.0.0.1", 0), H)
        self.port = self.httpd.server_address[1]
        self.t = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.t.start()

    def set(self, doc):
        self.body = json.dumps(doc).encode()

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()


LIVE_AGENT_FIELDS = {   # the fields live /api/fleet had on 2026-11-16 (rev-e's list)
    "activity", "agent_id", "attention", "attention_since_ms", "ended_at_ms", "engine",
    "execution_id", "judged_at_ms", "judgement", "label", "live", "mission_name", "role",
    "role_rank", "session_id", "started_at_ms", "state", "state_since_ms", "task", "task_title"}
LIVE_ACTIVITY_FIELDS = {"current_tool", "current_tool_summary", "event_count", "executions", "jobs",
                        "last_event_at_ms", "last_event_name", "tool_calls", "turn_at_ms", "turn_open"}


class Coverage(unittest.TestCase):
    def test_the_fake_agent_has_every_live_field(self):
        a = agent(1)
        self.assertTrue(LIVE_AGENT_FIELDS <= set(a), LIVE_AGENT_FIELDS - set(a))
        self.assertTrue(LIVE_ACTIVITY_FIELDS <= set(a["activity"]), LIVE_ACTIVITY_FIELDS - set(a["activity"]))


class Classify(unittest.TestCase):
    def st(self, a, engine_age=30):
        return ap.classify(a, NOW, t_ms(engine_age) if engine_age is not None else None)

    def test_working_needs_an_open_turn_and_a_recent_engine_event(self):
        self.assertEqual(self.st(agent(1, turn_open=True), 30), "working")
        self.assertEqual(self.st(agent(1, turn_open=True), ap.WORKING_QUIET_S + 1), "unknown")
        self.assertEqual(self.st(agent(1, turn_open=True), None), "unknown")

    def test_fleet_last_event_time_is_not_used(self):
        # A89: a fresh fleet-level time (observer writes) must not keep an open turn working
        a = agent(1, turn_open=True, last_age=1)
        self.assertEqual(self.st(a, ap.WORKING_QUIET_S + 100), "unknown")

    def test_an_open_tool_allows_a_longer_quiet(self):
        self.assertEqual(self.st(agent(1, turn_open=True, tool="Bash"), 1800), "working")
        self.assertEqual(self.st(agent(1, turn_open=True, tool="Bash"), ap.OPEN_TOOL_QUIET_S + 1), "unknown")

    def test_idle_offline(self):
        self.assertEqual(self.st(agent(1, turn_open=False), None), "idle")
        self.assertEqual(self.st(agent(1, live=False, ended_age=60), None), "offline")

    def test_session_existence_alone_is_never_working(self):
        self.assertEqual(self.st(agent(1, turn_open=False, turn_age=None), 1), "idle")

    def test_judge_state_is_ignored(self):
        self.assertEqual(self.st(agent(1, turn_open=False), 1), "idle")

    def test_malformed_fields_are_unknown(self):
        for bad in ("yes", 1, None, [], {}):
            a = agent(1)
            a["live"] = bad
            self.assertEqual(self.st(a), "unknown", bad)
            b = agent(1)
            b["activity"]["turn_open"] = bad
            self.assertEqual(self.st(b), "unknown", bad)
        self.assertEqual(ap.classify("not an agent", NOW, None), "unknown")
        d = agent(1)
        d["activity"] = "x"
        self.assertEqual(self.st(d), "unknown")

    def test_future_engine_event_beyond_skew_is_unknown(self):
        self.assertEqual(self.st(agent(1, turn_open=True), -120), "unknown")
        self.assertEqual(self.st(agent(1, turn_open=True), -30), "working")

    def test_roles(self):
        self.assertEqual(ap.role_of(agent(1, role="supervisor")), "supervisor")
        self.assertEqual(ap.role_of(agent(1, role="rsi")), "rsi")
        for r in ("admin " + SECRET, None, 3, "Worker"):
            self.assertEqual(ap.role_of(agent(1, role=r)), "other")


class EngineReader(unittest.TestCase):
    """A89 end to end: the session timeline, filtered to the agent's own engine
    in its current execution."""

    def setUp(self):
        self.srv = Server()
        self.c = cfg("/nonexistent", self.srv.port)
        self.a = agent(1, turn_open=True, last_age=1)

    def tearDown(self):
        self.srv.close()

    def test_old_engine_event_with_newer_observer_records_is_unknown(self):
        sid = self.a["session_id"]
        self.srv.timelines[sid] = [ev(1, "claude", "exec_1", 3000, "message.received")] + \
            [ev(2 + i, src, ex, 600 - i * 50) for i, (src, ex) in enumerate(
                [("observer", None), ("bridge", "exec_1"), ("gateway", None), ("observer", "exec_1")] * 3)]
        e = ap.engine_last_ms(self.c, self.a)
        self.assertEqual(e, t_ms(3000))
        self.assertEqual(ap.classify(self.a, NOW, e), "unknown")

    def test_a_new_engine_event_restores_working(self):
        sid = self.a["session_id"]
        self.srv.timelines[sid] = [ev(1, "claude", "exec_1", 3000, "message.received"),
                                   ev(2, "observer", None, 500), ev(3, "claude", "exec_1", 20)]
        self.assertEqual(ap.classify(self.a, NOW, ap.engine_last_ms(self.c, self.a)), "working")

    def test_other_execution_and_other_engine_do_not_count(self):
        sid = self.a["session_id"]
        self.srv.timelines[sid] = [ev(1, "claude", "exec_OLD", 10), ev(2, "codex", "exec_1", 10),
                                   ev(3, "bridge", "exec_1", 10)]
        self.assertIsNone(ap.engine_last_ms(self.c, self.a))

    def test_only_the_newest_window_is_read(self):
        sid = self.a["session_id"]
        self.srv.timelines[sid] = [ev(1, "claude", "exec_1", 10)] + \
            [ev(i, "observer", None, 5) for i in range(2, 2 + ap.SESSION_WINDOW)]
        self.assertIsNone(ap.engine_last_ms(self.c, self.a))      # the engine event is outside: unknown
        q = urllib.parse.urlsplit(self.srv.paths[-1]).query
        self.assertEqual(dict(urllib.parse.parse_qsl(q)), {"after": "1", "limit": str(ap.SESSION_WINDOW)})

    def test_unsafe_ids_are_never_put_in_a_path(self):
        for field, bad in (("session_id", "../../api/fleet"), ("session_id", "a b"), ("session_id", "x?y=1"),
                           ("execution_id", 5), ("engine", "Claude Code"), ("session_id", "s" * 200),
                           ("session_id", ".."), ("session_id", "."), ("session_id", "....")):
            a = dict(self.a)
            a[field] = bad
            self.assertIsNone(ap.engine_last_ms(self.c, a), (field, bad))
        self.assertEqual(self.srv.paths, [])

    def test_errors_and_oversize_are_none(self):
        sid = self.a["session_id"]
        self.srv.timelines[sid] = [ev(1, "claude", "exec_1", 10)]
        self.c["max_session_bytes"] = 50
        self.assertIsNone(ap.engine_last_ms(self.c, self.a))
        self.c["max_session_bytes"] = 1 << 18
        self.srv.status = 302
        self.srv.headers = {"Location": "http://example.invalid/"}
        self.assertIsNone(ap.engine_last_ms(self.c, self.a))

    def test_session_reads_are_capped_per_run(self):
        calls = []
        agents = [agent(i, turn_open=True) for i in range(ap.MAX_SESSION_READS + 3)]
        out, _, _, _ = ap.project(self.c, {"agents": agents}, {}, 1, NOW,
                                  engine_reader=lambda c, a: calls.append(1) or t_ms(5))
        self.assertEqual(len(calls), ap.MAX_SESSION_READS)
        self.assertEqual(sum(r["state"] == "working" for r in out), ap.MAX_SESSION_READS)
        self.assertEqual(sum(r["state"] == "unknown" for r in out), 3)

    def test_no_session_read_for_idle_or_offline(self):
        calls = []
        ap.project(self.c, {"agents": [agent(1), agent(2, live=False, ended_age=5)]}, {}, 1, NOW,
                   engine_reader=lambda c, a: calls.append(1))
        self.assertEqual(calls, [])


class Project(unittest.TestCase):
    def p(self, agents, ids=None, nxt=1):
        return ap.project(cfg("/nonexistent"), {"agents": agents}, ids if ids is not None else {}, nxt, NOW,
                          engine_reader=lambda c, a: t_ms(5))

    def test_selection_live_then_recent_offline_capped(self):
        agents = [agent(i, live=False, ended_age=100 + i) for i in range(10)]
        agents += [agent(100, live=False, ended_age=ap.OFFLINE_WINDOW_S + 5)]   # too old
        agents += [agent(200 + i) for i in range(70)]
        out, _, _, _ = self.p(agents)
        self.assertEqual(len(out), ap.MAX_AGENTS)
        self.assertTrue(all(r["state"] != "offline" for r in out))
        out2, _, _, _ = self.p(agents[:15])
        self.assertEqual(sum(r["state"] == "offline" for r in out2), 10)

    def test_ids_are_stable_and_never_reused(self):
        out1, ids, nxt, ch = self.p([agent(1), agent(2)])
        self.assertTrue(ch)
        out2, ids, nxt, _ = self.p([agent(2), agent(3)], ids, nxt)
        self.assertEqual({r["id"] for r in out1}, {"a1", "a2"})
        self.assertEqual({r["id"] for r in out2}, {"a2", "a3"})
        self.assertEqual(nxt, 4)

    def test_id_map_is_bounded(self):
        ids = {("%032x" % i): i + 1 for i in range(ap.MAX_IDS)}
        ids, nxt, ch = ap.assign(["f" * 32], ids, ap.MAX_IDS + 1)
        self.assertEqual(len(ids), ap.MAX_IDS)
        self.assertEqual(ids["f" * 32], ap.MAX_IDS + 1)
        self.assertNotIn("%032x" % 0, ids)

    def test_agents_without_usable_id_are_left_out(self):
        a, b = agent(1), agent(2)
        a["agent_id"] = 5
        b["agent_id"] = "x" * 300
        out, _, _, _ = self.p([a, b, agent(3)])
        self.assertEqual(len(out), 1)

    def test_not_a_list_gives_none(self):
        self.assertIsNone(ap.project(cfg("/x"), {"agents": "x"}, {}, 1, NOW)[0])
        self.assertIsNone(ap.project(cfg("/x"), {}, {}, 1, NOW)[0])

    def test_rows_have_only_four_fields(self):
        out, _, _, _ = self.p([agent(1, turn_open=True), agent(2, live=False, ended_age=9)])
        for r in out:
            self.assertEqual(set(r), {"id", "role", "state", "evidence"})


class Valid(unittest.TestCase):
    def good(self):
        out, _, _, _ = ap.project(cfg("/x"), {"agents": [agent(1), agent(2, turn_open=True)]}, {}, 1, NOW,
                                  engine_reader=lambda c, a: t_ms(5))
        return ap.build(cfg("/x"), out, "0123456789abcdef", None, NOW)

    def test_good_document(self):
        self.assertTrue(ap.valid(self.good()))

    def test_rejections(self):
        cases = [
            lambda d: d.update(extra=1),
            lambda d: d.update(version=1),
            lambda d: d.update(source_status="fine"),
            lambda d: d.update(stale_after_s=True),
            lambda d: d.update(stale_after_s=5),
            lambda d: d.update(generated_utc="yesterday"),
            lambda d: d.update(provenance={"adapter": "x", "version": "1"}),
            lambda d: d.update(id_epoch="XYZ"),
            lambda d: d.update(id_epoch=None),                         # agents need an epoch
            lambda d: d["agents"][0].update(state="busy"),
            lambda d: d["agents"][0].update(role="admin"),
            lambda d: d["agents"][0].update(id="a01"),
            lambda d: d["agents"][0].update(id="agent_1"),
            lambda d: d["agents"][0].update(id=d["agents"][1]["id"]),
            lambda d: d["agents"][0].update(evidence="inferred"),
            lambda d: d["agents"][0].update(name=SECRET),
            lambda d: d["agents"][0].update(last_event_utc="2026-11-16T07:11:16Z"),   # no per-agent time
            lambda d: d.update(source_status="unavailable"),           # agents must be empty
            lambda d: d.update(source_status="stale"),                 # states must be unknown
            lambda d: d.update(source_last_ok_utc="2026-01-01T00:00:00Z"),   # ok but old
            lambda d: d.update(agents=d["agents"] * 40),
        ]
        for i, f in enumerate(cases):
            d = self.good()
            f(d)
            self.assertFalse(ap.valid(d), i)

    def test_size_bound(self):
        d = self.good()
        d["agents"] = [dict(d["agents"][0], id="a%d" % (i + 1)) for i in range(64)]
        self.assertTrue(ap.valid(d))
        self.assertLess(len(json.dumps(d)), ap.MAX_BYTES)


class Export(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.srv = Server()
        self.c = cfg(self.tmp, self.srv.port)

    def tearDown(self):
        self.srv.close()
        shutil.rmtree(self.tmp)

    def out(self):
        with open(os.path.join(self.c["out_dir"], "presence.json")) as f:
            return f.read()

    def doc(self):
        return json.loads(self.out())

    def ids_path(self):
        return os.path.join(self.c["out_dir"], "ids.json")

    def test_no_planted_value_reaches_the_file(self):
        a1 = agent(1, turn_open=True)
        self.srv.timelines[a1["session_id"]] = [ev(1, "claude", "exec_1", 3, "message.received")]
        self.srv.set({"agents": [a1, agent(2, role="supervisor"), agent(3, live=False, ended_age=10, tool="Bash")],
                      "health": {"secret": SECRET}})
        self.assertEqual(ap.export(self.c), "ok")
        text = self.out()
        for bad in (SECRET, "PLANTED", "agent_worker", "session_", "exec_", "/home", "WORKING",
                    "tool.started", "Bash", "mission", "label", "claude", "observer"):
            self.assertNotIn(bad, text)
        ids_text = open(self.ids_path()).read()
        self.assertNotIn(SECRET, ids_text)
        self.assertNotIn("agent_worker", ids_text)
        d = self.doc()
        self.assertEqual([r["state"] for r in d["agents"]], ["working", "idle", "offline"])
        self.assertEqual(self.srv.paths[0], "/api/fleet")
        self.assertTrue(all(p == "/api/fleet" or p.startswith("/api/session/") for p in self.srv.paths))

    def test_modes(self):
        self.srv.set({"agents": [agent(1)]})
        ap.export(self.c)
        d = self.c["out_dir"]
        self.assertEqual(stat.S_IMODE(os.stat(d).st_mode), 0o700)
        for f in ("presence.json", "ids.json", "issued.json", "lock"):
            self.assertEqual(stat.S_IMODE(os.stat(os.path.join(d, f)).st_mode), 0o600)
        self.assertEqual(sorted(os.listdir(d)), ["ids.json", "issued.json", "lock", "presence.json"])

    def test_ids_survive_a_restart(self):
        self.srv.set({"agents": [agent(1), agent(2)]})
        ap.export(self.c)
        d1 = self.doc()
        self.srv.set({"agents": [agent(2)]})
        ap.export(self.c)                                   # a new process: state is on disk only
        d2 = self.doc()
        self.assertEqual({r["id"] for r in d1["agents"]}, {"a1", "a2"})
        self.assertEqual({r["id"] for r in d2["agents"]}, {"a2"})
        self.assertEqual(d1["id_epoch"], d2["id_epoch"])

    def test_lost_or_damaged_map_starts_a_new_epoch(self):
        # A90: A -> a1 published; the map is lost or damaged; B alone must not become the same identity
        self.srv.set({"agents": [agent(1)]})
        ap.export(self.c)
        d1 = self.doc()
        for damage in ("remove", "garbage", "partial", "bad_epoch"):
            if damage == "remove":
                os.unlink(self.ids_path())
            elif damage == "garbage":
                open(self.ids_path(), "w").write("{not json")
            elif damage == "partial":
                m = json.load(open(self.ids_path()))
                m["ids"]["zz"] = "a1"
                json.dump(m, open(self.ids_path(), "w"))
            else:
                m = json.load(open(self.ids_path()))
                m["epoch"] = "nope"
                json.dump(m, open(self.ids_path(), "w"))
            self.srv.set({"agents": [agent(99)]})
            self.assertEqual(ap.export(self.c), "ok", damage)
            d2 = self.doc()
            self.assertNotEqual((d2["id_epoch"], d2["agents"][0]["id"]), (d1["id_epoch"], d1["agents"][0]["id"]), damage)
            self.assertNotEqual(d2["id_epoch"], d1["id_epoch"], damage)
            d1 = d2
            self.srv.set({"agents": [agent(1)]})
            ap.export(self.c)
            d1 = self.doc()

    def test_a_rolled_back_map_starts_a_new_epoch(self):
        # rev-e r3n1: a valid map restored from an older copy keeps its epoch but a lower next
        self.srv.set({"agents": [agent(1)]})
        ap.export(self.c)
        old_map = open(self.ids_path()).read()           # next = 2
        self.srv.set({"agents": [agent(1), agent(2), agent(3)]})
        ap.export(self.c)                                # a2, a3 published, next = 4
        d1 = self.doc()
        open(self.ids_path(), "w").write(old_map)        # rolled back
        self.srv.set({"agents": [agent(4), agent(5)]})
        self.assertEqual(ap.export(self.c), "ok")
        d2 = self.doc()
        self.assertNotEqual(d2["id_epoch"], d1["id_epoch"])
        self.srv.set({"agents": [agent(4)]})
        ap.export(self.c)
        self.assertEqual(self.doc()["id_epoch"], d2["id_epoch"])     # then stable again

    def test_rollback_after_the_higher_id_left_the_document(self):
        # cc-astra A90 (R4): A as (E,a1); keep the map (next=2); B as (E,a2); B ends and
        # leaves the selection, so the document lists only a1; restore the old map;
        # a new agent C must not become (E,a2). Once directly, once with an empty
        # unavailable document in between.
        for gap in (False, True):
            shutil.rmtree(self.c["out_dir"], ignore_errors=True)
            a, b, c = agent(1), agent(2), agent(3)
            self.srv.status = 200
            self.srv.set({"agents": [a]})
            self.assertEqual(ap.export(self.c), "ok")
            old_map = open(self.ids_path()).read()
            self.srv.set({"agents": [a, b]})
            ap.export(self.c)
            d_b = self.doc()
            e = d_b["id_epoch"]
            self.assertIn("a2", {r["id"] for r in d_b["agents"]})
            self.srv.set({"agents": [a, agent(2, live=False, ended_age=ap.OFFLINE_WINDOW_S + 10)]})
            ap.export(self.c)
            self.assertEqual({r["id"] for r in self.doc()["agents"]}, {"a1"})       # a2 has left
            if gap:
                self.srv.status = 500
                self.c["carry_s"] = 0
                time.sleep(1.1)
                self.assertEqual(ap.export(self.c), "unavailable")
                self.assertEqual(self.doc()["agents"], [])
                self.srv.status, self.c["carry_s"] = 200, 300
            open(self.ids_path(), "w").write(old_map)                                # rolled back
            self.srv.set({"agents": [a, c]})
            self.assertEqual(ap.export(self.c), "ok")
            d = self.doc()
            self.assertNotEqual(d["id_epoch"], e, gap)
            self.assertFalse(any((d["id_epoch"], r["id"]) == (e, "a2") for r in d["agents"]), gap)

    def test_write_order_is_mark_then_map_then_document(self):
        order = []
        real = ap.write_atomic
        ap.write_atomic = lambda path, data: (order.append(os.path.basename(path)), real(path, data))[1]
        try:
            self.srv.set({"agents": [agent(1)]})
            ap.export(self.c)
        finally:
            ap.write_atomic = real
        self.assertEqual(order, ["issued.json", "ids.json", "presence.json"])

    def test_a_crash_at_any_write_never_reuses_an_id(self):
        # rev-e R5 F1: a crash injected at each write in turn, then cc-astra's rollback
        # sequence. Under the wrong order (map before mark) this reuses (E, a2).
        for crash_at in ("issued.json", "ids.json", "presence.json"):
            shutil.rmtree(self.c["out_dir"], ignore_errors=True)
            a, b, c = agent(1), agent(2), agent(3)
            self.srv.set({"agents": [a]})
            self.assertEqual(ap.export(self.c), "ok")
            old_map = open(self.ids_path()).read()
            real = ap.write_atomic

            def crashing(path, data, real=real, crash_at=crash_at):
                if os.path.basename(path) == crash_at:
                    raise KeyboardInterrupt("the process dies here")
                return real(path, data)
            ap.write_atomic = crashing
            self.srv.set({"agents": [a, b]})
            try:
                ap.export(self.c)
            except KeyboardInterrupt:
                pass
            finally:
                ap.write_atomic = real
            ap.export(self.c)                                   # the next run, b still live
            d_b = self.doc()
            published = {(d_b["id_epoch"], r["id"]) for r in d_b["agents"] if r["id"] != "a1"}
            self.srv.set({"agents": [a, agent(2, live=False, ended_age=ap.OFFLINE_WINDOW_S + 10)]})
            ap.export(self.c)                                   # b leaves the document
            open(self.ids_path(), "w").write(old_map)           # the map is rolled back
            self.srv.set({"agents": [a, c]})
            ap.export(self.c)
            d = self.doc()
            reused = [r["id"] for r in d["agents"] if (d["id_epoch"], r["id"]) in published]
            self.assertEqual(reused, [], crash_at)

    def test_missing_or_damaged_high_water_mark_starts_a_new_epoch(self):
        self.srv.set({"agents": [agent(1)]})
        ap.export(self.c)
        e = self.doc()["id_epoch"]
        issued = os.path.join(self.c["out_dir"], "issued.json")
        for damage in ("remove", "garbage", "other_epoch"):
            if damage == "remove":
                os.unlink(issued)
            elif damage == "garbage":
                open(issued, "w").write("{")
            else:
                open(issued, "w").write('{"epoch": "ffffffffffffffff", "next": 2}')
            ap.export(self.c)
            self.assertNotEqual(self.doc()["id_epoch"], e, damage)
            e = self.doc()["id_epoch"]
            ap.export(self.c)
            self.assertEqual(self.doc()["id_epoch"], e, damage)        # stable again after the reset

    def test_redirect_and_errors_are_not_followed(self):
        for status in (301, 302, 307, 404, 500):
            self.srv.status = status
            self.srv.headers = {"Location": "http://example.invalid/api/fleet"}
            self.assertEqual(ap.export(self.c), "unavailable", status)
        self.assertTrue(all(p == "/api/fleet" for p in self.srv.paths))

    def test_oversize_says_so_and_garbage_is_unavailable(self):
        self.c["max_source_bytes"] = 1000
        self.srv.set({"agents": [agent(i) for i in range(20)]})
        self.assertEqual(ap.export(self.c), "source_too_large")
        self.assertEqual(self.doc()["source_status"], "unavailable")
        self.c["max_source_bytes"] = 1 << 20
        self.srv.body = b"not json"
        self.assertEqual(ap.export(self.c), "unavailable")
        self.srv.body = b"[1,2]"
        self.assertEqual(ap.export(self.c), "unavailable")

    def test_failure_carries_presence_as_unknown_then_drops_it(self):
        a1 = agent(1, turn_open=True)
        self.srv.timelines[a1["session_id"]] = [ev(1, "claude", "exec_1", 3)]
        self.srv.set({"agents": [a1]})
        self.assertEqual(ap.export(self.c), "ok")
        ep = self.doc()["id_epoch"]
        self.srv.status = 500
        self.assertEqual(ap.export(self.c), "stale")
        d = self.doc()
        self.assertEqual((d["source_status"], d["id_epoch"]), ("stale", ep))
        self.assertEqual([r["state"] for r in d["agents"]], ["unknown"])
        self.c["carry_s"] = 0
        time.sleep(1.1)
        self.assertEqual(ap.export(self.c), "unavailable")
        d = self.doc()
        self.assertEqual((d["agents"], d["id_epoch"]), ([], None))
        self.assertIsNotNone(d["source_last_ok_utc"])      # carried, never refreshed

    def test_no_source_at_all(self):
        self.c["port"] = 1
        self.assertEqual(ap.export(self.c), "unavailable")
        d = self.doc()
        self.assertEqual((d["source_status"], d["agents"], d["source_last_ok_utc"]), ("unavailable", [], None))
        self.assertFalse(os.path.exists(self.ids_path()))

    def test_disable_file(self):
        open(self.c["disable_file"], "w").close()
        self.assertEqual(ap.export(self.c), "disabled")
        self.assertFalse(os.path.exists(self.c["out_dir"]))
        self.assertEqual(self.srv.paths, [])

    def test_too_soon(self):
        self.srv.set({"agents": [agent(1)]})
        self.c["min_interval_s"] = 60
        self.assertEqual(ap.export(self.c), "ok")
        self.assertEqual(ap.export(self.c), "too_soon")
        self.assertEqual(len(self.srv.paths), 1)

    def test_out_dir_symlink_refused(self):
        real = os.path.join(self.tmp, "real")
        os.mkdir(real)
        os.symlink(real, self.c["out_dir"])
        self.assertEqual(ap.export(self.c), "out_dir_refused")

    def test_symlinked_output_is_not_followed(self):
        self.srv.set({"agents": [agent(1)]})
        ap.export(self.c)
        victim = os.path.join(self.tmp, "victim")
        open(victim, "w").write("keep")
        p = os.path.join(self.c["out_dir"], "presence.json")
        os.unlink(p)
        os.symlink(victim, p)
        ap.export(self.c)
        self.assertEqual(open(victim).read(), "keep")

    def test_busy_lock(self):
        os.makedirs(self.c["out_dir"], mode=0o700)
        fd = os.open(os.path.join(self.c["out_dir"], "lock"), os.O_WRONLY | os.O_CREAT, 0o600)
        fcntl.flock(fd, fcntl.LOCK_EX)
        try:
            self.assertEqual(ap.export(self.c), "busy")
        finally:
            os.close(fd)

    def test_disk_low_no_write(self):
        self.c["min_free_bytes"] = 1 << 62
        self.assertEqual(ap.export(self.c), "disk_low_no_write")


class Static(unittest.TestCase):
    def test_no_process_or_network_beyond_local_gets(self):
        src = open(ap.__file__).read()
        code = src.split('"""', 2)[2]          # the code after the module docstring
        for bad in ("subprocess", "os.system", "popen", "socket.socket", "urllib.request", "requests",
                    "os.environ", "getenv(", "tmux", "judgement", "screen", "exec(", "eval("):
            self.assertFalse(bad in code, bad)
        self.assertIn('"127.0.0.1"', code)
        self.assertEqual(code.count("conn.request("), 1)
        self.assertEqual(code.count("HTTPConnection("), 1)


if __name__ == "__main__":
    unittest.main(verbosity=1)
