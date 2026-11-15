#!/usr/bin/env python3
"""scripts/storage-extents.py's ordering rule (cc-astra A82), synthetic and
offline: real files in a scratch folder, but their extents are made up (the
script's extents() is replaced) and so is the clock, so no file system with
reflinks is needed and the bound falls exactly between two files.

    python3 -I -B scripts/test-storage-extents.py

Removes only its own mkdtemp folder."""
import importlib.util
import io
import json
import os
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SPEC = importlib.util.spec_from_file_location("storage_extents", os.path.join(HERE, "storage-extents.py"))
se = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(se)

E1, E2, OWN = (4096, 1000, 0), (8192, 2000, 0), (16384, 300, 0)


class Clock:
    """Each call is one second later than the one before."""
    def __init__(self):
        self.t = 0.0

    def monotonic(self):
        self.t += 1.0
        return self.t


class T(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="storage-extents-test-")
        self.map = {}
        for name, found in (("cache/e1", [E1]), ("cache/e2", [E2]),
                            ("scratch/e1", [E1]), ("scratch/e2", [E2]), ("scratch/own", [OWN])):
            path = os.path.join(self.tmp, name)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w") as fh:
                fh.write("x")
            self.map[os.path.realpath(path)] = found
        self.real_extents, self.real_time = se.extents, se.time
        se.extents = lambda fd: list(self.map[os.path.realpath("/proc/self/fd/%d" % fd)])
        se.time = Clock()

    def tearDown(self):
        se.extents, se.time = self.real_extents, self.real_time
        shutil.rmtree(self.tmp, ignore_errors=True)

    def run_main(self, bound, names=("cache", "scratch")):
        config = {"bound_secs": bound,
                  "sources": [{"name": n, "roots": [os.path.join(self.tmp, n)], "exclude": []} for n in names]}
        stdin, stdout = sys.stdin, sys.stdout
        sys.stdin, sys.stdout = io.StringIO(json.dumps(config)), io.StringIO()
        try:
            se.main()
            out = sys.stdout.getvalue()
        finally:
            sys.stdin, sys.stdout = stdin, stdout
        self.assertNotIn(self.tmp, out, "no path is ever printed")
        return {r["name"]: r for r in json.loads(out)["sources"]}

    def test_a_source_after_one_over_its_bound_is_not_measured(self):
        # The clock reads 1 at the start, 2 at the first file (in bound), 3 at
        # the second (over 1.5): cache stops after one of its two shared extents.
        got = self.run_main(1.5)
        self.assertTrue(got["cache"]["over_bound"])
        self.assertEqual(got["cache"]["files"], 1)
        self.assertNotIn("bytes", got["scratch"])
        self.assertIn("cache, before it, was over its bound", got["scratch"]["not_measured"])

    def test_with_no_source_over_its_bound_each_extent_counts_in_the_first_that_holds_it(self):
        got = self.run_main(1000)
        self.assertEqual((got["cache"]["bytes"], got["cache"]["files"]), (3000, 2))
        self.assertEqual((got["scratch"]["bytes"], got["scratch"]["files"]), (300, 3))

    def test_a_source_before_the_one_over_its_bound_keeps_its_count(self):
        # cache's two files are at 1 and 2 s from its start, in a 2.5 s bound;
        # scratch's third file is at 3 s from its start, over it; late is after.
        os.makedirs(os.path.join(self.tmp, "late"))
        path = os.path.join(self.tmp, "late", "own")
        with open(path, "w") as fh:
            fh.write("x")
        self.map[os.path.realpath(path)] = [(32768, 50, 0)]
        got = self.run_main(2.5, names=("cache", "scratch", "late"))
        self.assertEqual((got["cache"]["bytes"], got["cache"]["files"]), (3000, 2))
        self.assertTrue(got["scratch"]["over_bound"])
        self.assertNotIn("bytes", got["scratch"])
        self.assertNotIn("bytes", got["late"])
        self.assertIn("scratch, before it, was over its bound", got["late"]["not_measured"])

    def test_the_order_of_the_sources_decides_which_holds_a_shared_extent(self):
        # scratch first, in bound: it holds E1 and E2; cache then adds nothing.
        got = self.run_main(3.5, names=("scratch", "cache"))
        self.assertEqual(got["scratch"]["bytes"], 3300)
        self.assertEqual((got["cache"]["bytes"], got["cache"]["files"]), (0, 2))


if __name__ == "__main__":
    unittest.main()
