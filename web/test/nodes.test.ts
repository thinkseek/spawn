import assert from "node:assert/strict";
import { test } from "node:test";

import { coresText, cpuLimit, gibText, normalizeNodes, reachText, viaText } from "../src/nodes.ts";

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
      transport: "ssh",
      mode: "direct",
      via: [],
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
      transport: "ssh",
      mode: "direct",
      via: [],
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

/** Issue #85: nodes reached over tcp or udp, directly or through hops, as
 * `/api/nodes` sends them: where each is reached (ssh destination, endpoint)
 * withheld by the server; one that slips through is never read. */
const REACHED = {
  nodes: [
    { name: "gw", state: "live", transport: "tcp", via: [], mode: "direct" },
    { name: "edge", state: "live", transport: "udp", via: ["gw", "relay-hk"], mode: "hole_punch" },
    { name: "inner", state: "drained", transport: "tcp", via: ["gw"] },
    { name: "jumped", state: "live", transport: "ssh", endpoint: "leak", via: ["bastion"] },
    { name: "odd", state: "live", transport: "quic", via: "gw", mode: "mesh" },
  ],
};

test("each node says how it is reached: transport, mode and hops, defaults read from the hops", () => {
  const rows = Object.fromEntries(normalizeNodes(REACHED).map((row) => [row.name, row]));
  const reach = (name: string) => {
    const { transport, mode, via } = rows[name]!;
    return { transport, mode, via };
  };
  assert.deepEqual(reach("gw"), { transport: "tcp", mode: "direct", via: [] });
  assert.deepEqual(reach("edge"), { transport: "udp", mode: "hole_punch", via: ["gw", "relay-hk"] });
  assert.deepEqual(reach("inner"), { transport: "tcp", mode: "relay", via: ["gw"] });
  assert.deepEqual(reach("jumped"), { transport: "ssh", mode: "relay", via: ["bastion"] });
  assert.ok(!("endpoint" in rows.jumped!), "no endpoint is kept");
  // Values it does not know read as an old join's.
  assert.deepEqual(reach("odd"), { transport: "ssh", mode: "direct", via: [] });
});

test("the badge and the hops read as the row says them, as the CLI says them", () => {
  assert.equal(reachText({ transport: "ssh", mode: "direct" }), "ssh");
  assert.equal(reachText({ transport: "ssh", mode: "relay" }), "ssh/relay");
  assert.equal(reachText({ transport: "tcp", mode: "direct" }), "tcp/direct");
  assert.equal(reachText({ transport: "udp", mode: "hole_punch" }), "udp/hole_punch");
  assert.equal(viaText({ via: [] }), null);
  assert.equal(viaText({ via: ["relay-hk", "edge-gw"] }), "via relay-hk → edge-gw");
});
