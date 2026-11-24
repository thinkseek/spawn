import assert from "node:assert/strict";
import { test } from "node:test";

import { coresText, cpuLimit, gibText, normalizeNodes } from "../src/nodes.ts";

const GIB = 1024 ** 3;

/** `/api/nodes` as the server answers it (serve.rs `nodes`, crate
 * spawn-postgres nodes.rs `NODES`): no clock, no ssh destination. */
const ANSWER = {
  nodes: [
    {
      name: "runner",
      state: "lost",
      lost_since_ms: 900_000,
      arch: "aarch64",
      cores: 10,
      cpu_max: "600000 100000",
      mem_total: 18 * GIB,
      mem_max: 12 * GIB,
      disk_free: 205 * GIB,
      gpu: null,
      heartbeat: { at_ms: 800_000, load1: 7.6, mem_available: 7 * GIB, disk_free: 200 * GIB },
    },
    {
      name: "desktop",
      state: "live",
      lost_since_ms: null,
      arch: "x86_64",
      cores: 16,
      cpu_max: "max 100000",
      mem_total: 31 * GIB,
      mem_max: null,
      disk_free: 74 * GIB,
      gpu: [{ name: "NVIDIA GeForce RTX 4070 Ti SUPER", memory_mb: 16_376 }],
      heartbeat: null,
    },
    { name: "odd", state: "busy" },
    { state: "live" },
  ],
};

test("the server's nodes become rows by name, each with its state and sample as said", () => {
  assert.deepEqual(normalizeNodes(ANSWER), [
    {
      name: "desktop",
      state: "live",
      arch: "x86_64",
      cores: 16,
      cpu_limit: null,
      mem_bytes: 31 * GIB,
      disk_free_bytes: 74 * GIB,
      gpu: "RTX 4070 Ti SUPER 16G",
      sampled_ms: null,
      lost_since_ms: null,
    },
    {
      name: "runner",
      state: "lost",
      arch: "aarch64",
      cores: 10,
      cpu_limit: 6,
      mem_bytes: 12 * GIB,
      disk_free_bytes: 200 * GIB,
      gpu: null,
      sampled_ms: 800_000,
      lost_since_ms: 900_000,
    },
  ]);
  assert.deepEqual(normalizeNodes({ nodes: [] }), []);
});

test("an answer that is no list of nodes is a failed read, not an empty one", () => {
  for (const raw of [null, "<html>", { error: "not found" }]) {
    assert.throws(() => normalizeNodes(raw), /not a list of nodes/);
  }
});

test("cores, limits and sizes read as the row says them", () => {
  assert.equal(cpuLimit("max 100000"), null);
  assert.equal(cpuLimit("250000 100000"), 2.5);
  assert.equal(cpuLimit(""), null);
  assert.equal(coresText({ cores: 10, cpu_limit: 6 }), "10 (6 allowed)");
  assert.equal(coresText({ cores: 16, cpu_limit: null }), "16");
  assert.equal(gibText(7.5 * GIB), "7.5G");
  assert.equal(gibText(200 * GIB), "200G");
  assert.equal(gibText(null), "—");
});
