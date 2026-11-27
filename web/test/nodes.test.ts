import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  buildShaBadge,
  coresText,
  cpuLimit,
  gibText,
  mergeNodeRows,
  nodeVersionText,
  normalizeNodeList,
  normalizeNodes,
  powerOf,
  powerText,
  powerTone,
  reachText,
  shortBuildSha,
  viaText,
} from "../src/nodes.ts";

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
      power: null,
      low_battery_drain: false,
      effective_drained: false,
      schedulable: null,
      build_sha: null,
      node_version: null,
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
      power: null,
      low_battery_drain: false,
      effective_drained: false,
      schedulable: null,
      build_sha: null,
      node_version: null,
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

/** Issue #91: each node's `power` and the low-battery soft drain, as
 * `/api/nodes` sends them (serve.rs `nodes`, crate nodes/power.rs
 * `with_soft_drain`). */
const POWERED = {
  nodes: [
    {
      name: "laptop",
      state: "live",
      power: { on_ac: false, battery_pct: 14, charging: false, remaining_mins: 52 },
      low_battery_drain: true,
      effective_drained: true,
      schedulable: false,
    },
    {
      name: "mac",
      state: "live",
      power: { on_ac: true, battery_pct: 88, charging: true, remaining_mins: 75 },
      low_battery_drain: false,
      effective_drained: false,
      schedulable: true,
    },
    {
      name: "desk",
      state: "drained",
      power: null,
      low_battery_drain: false,
      effective_drained: true,
      schedulable: false,
    },
    { name: "old", state: "drained", power: { battery_pct: 50 } },
  ],
};

test("a node's power and soft drain are read as the server says them", () => {
  const rows = Object.fromEntries(normalizeNodes(POWERED).map((row) => [row.name, row]));
  assert.deepEqual(rows.laptop!.power, { on_ac: false, battery_pct: 14, charging: false, remaining_mins: 52 });
  assert.equal(rows.laptop!.low_battery_drain, true);
  assert.equal(rows.laptop!.effective_drained, true);
  assert.equal(rows.laptop!.schedulable, false);
  assert.equal(rows.mac!.schedulable, true);
  assert.equal(rows.desk!.power, null);
  assert.equal(rows.desk!.effective_drained, true);
  // A power without `on_ac` is none; an older server's drained node is
  // still effectively drained, its eligibility unsaid.
  assert.equal(rows.old!.power, null);
  assert.equal(rows.old!.effective_drained, true);
  assert.equal(rows.old!.schedulable, null);
});

test("the power reads as the CLI says it, with its tone", () => {
  assert.equal(powerText({ power: null }), null);
  assert.equal(
    powerText({ power: { on_ac: false, battery_pct: 57, charging: false, remaining_mins: 192 } }),
    "battery 57% discharging 3h12m",
  );
  assert.equal(powerText({ power: { on_ac: true, battery_pct: 100, charging: false, remaining_mins: null } }), "AC 100%");
  assert.equal(powerText({ power: { on_ac: true, battery_pct: null, charging: false, remaining_mins: null } }), "AC");
  assert.equal(powerText({ power: { on_ac: true, battery_pct: 40, charging: true, remaining_mins: 5 } }), "AC 40% charging 0h05m");
  const on = (on_ac: boolean, charging = false) => ({ on_ac, battery_pct: 50, charging, remaining_mins: null });
  assert.equal(powerTone({ power: null, low_battery_drain: false }), null);
  assert.equal(powerTone({ power: on(false), low_battery_drain: true }), "low");
  assert.equal(powerTone({ power: on(false), low_battery_drain: false }), "draining");
  assert.equal(powerTone({ power: on(true), low_battery_drain: false }), "ok");
  assert.equal(powerTone({ power: on(false, true), low_battery_drain: false }), "ok");
  // Out of range values are clamped, not trusted.
  assert.deepEqual(powerOf({ on_ac: false, battery_pct: 140, remaining_mins: -3 }), {
    on_ac: false,
    battery_pct: 100,
    charging: false,
    remaining_mins: null,
  });
});

test("node build_sha and node_version are normalized from top-level or heartbeat, with null fallbacks", () => {
  const fullSha = "e3f8ca5a66214f0a47f80878c2ceb97a76301dbb";
  const rows = Object.fromEntries(
    normalizeNodes({
      nodes: [
        { name: "v1-full", state: "live", build_sha: `  ${fullSha}  `, node_version: " 0.1.0 " },
        {
          name: "v1-beat",
          state: "live",
          build_sha: null,
          node_version: "",
          heartbeat: { at_ms: 1000, build_sha: "abcdef123456", node_version: "v0.2.1" },
        },
        { name: "v1-empty", state: "live", build_sha: "   ", node_version: 123 },
        { name: "legacy", state: "drained" },
      ],
    }).map((row) => [row.name, row]),
  );

  assert.equal(rows["v1-full"]?.build_sha, fullSha);
  assert.equal(rows["v1-full"]?.node_version, "0.1.0");
  assert.equal(rows["v1-beat"]?.build_sha, "abcdef123456");
  assert.equal(rows["v1-beat"]?.node_version, "v0.2.1");
  assert.equal(rows["v1-empty"]?.build_sha, null);
  assert.equal(rows["v1-empty"]?.node_version, null);
  assert.equal(rows.legacy?.build_sha, null);
  assert.equal(rows.legacy?.node_version, null);

  assert.deepEqual(normalizeNodeList(undefined), []);
  assert.deepEqual(normalizeNodeList(null), []);
  assert.equal(normalizeNodeList([{ name: "fleet-only", state: "live", build_sha: fullSha }]).length, 1);
});

test("shortBuildSha, buildShaBadge, and nodeVersionText format telemetry and handle missing/null fields", () => {
  const fullSha = "e3f8ca5a66214f0a47f80878c2ceb97a76301dbb";
  assert.equal(shortBuildSha(fullSha), "e3f8ca5a");
  assert.equal(shortBuildSha(fullSha, 12), "e3f8ca5a6621");
  assert.equal(shortBuildSha("abc1234"), "abc1234");
  assert.equal(shortBuildSha("  9f8e7d6c5b4a  "), "9f8e7d6c");
  assert.equal(shortBuildSha(null), null);
  assert.equal(shortBuildSha(undefined), null);
  assert.equal(shortBuildSha(""), null);
  assert.equal(shortBuildSha("   "), null);

  assert.deepEqual(buildShaBadge({ build_sha: fullSha }), {
    short: "e3f8ca5a",
    full: fullSha,
  });
  assert.deepEqual(buildShaBadge({ build_sha: "abc1234" }), {
    short: "abc1234",
    full: "abc1234",
  });
  assert.equal(buildShaBadge({ build_sha: null }), null);
  assert.equal(buildShaBadge({ build_sha: "" }), null);
  assert.equal(buildShaBadge({ build_sha: "  " }), null);

  assert.equal(nodeVersionText({ node_version: "0.1.0" }), "v0.1.0");
  assert.equal(nodeVersionText({ node_version: " 1.4.2-rc1 " }), "v1.4.2-rc1");
  assert.equal(nodeVersionText({ node_version: "v0.3.0" }), "v0.3.0");
  assert.equal(nodeVersionText({ node_version: "dev-build" }), "dev-build");
  assert.equal(nodeVersionText({ node_version: null }), null);
  assert.equal(nodeVersionText({ node_version: "" }), null);
  assert.equal(nodeVersionText({ node_version: "   " }), null);
});

test("mergeNodeRows combines /api/nodes and /api/fleet node payloads without losing telemetry", () => {
  const apiNodes = normalizeNodeList([
    { name: "alpha", state: "live", build_sha: null, node_version: null },
    { name: "beta", state: "live", build_sha: "1111222233334444", node_version: "0.2.0" },
  ]);
  const fleetNodes = normalizeNodeList([
    { name: "alpha", state: "live", build_sha: "aaaa1111bbbb2222", node_version: "0.1.0" },
    { name: "beta", state: "live", build_sha: "9999888877776666", node_version: "0.1.0" },
    { name: "gamma", state: "drained", build_sha: "cccc3333dddd4444", node_version: "0.1.5" },
  ]);

  const merged = mergeNodeRows(apiNodes, fleetNodes);
  assert.deepEqual(
    merged.map((row) => [row.name, row.build_sha, row.node_version]),
    [
      ["alpha", "aaaa1111bbbb2222", "0.1.0"],
      ["beta", "1111222233334444", "0.2.0"],
      ["gamma", "cccc3333dddd4444", "0.1.5"],
    ],
  );
  assert.deepEqual(mergeNodeRows(null, fleetNodes).map((row) => row.name), ["alpha", "beta", "gamma"]);
  assert.deepEqual(mergeNodeRows(apiNodes, undefined).map((row) => row.name), ["alpha", "beta"]);
  assert.deepEqual(mergeNodeRows(null, null), []);
});

test("NodeRows and FleetView wire build_sha monospace badge tooltip and node_version", () => {
  const nodeRows = readFileSync(new URL("../src/components/fleet/NodeRows.tsx", import.meta.url), "utf8");
  const fleetView = readFileSync(new URL("../src/components/fleet/FleetView.tsx", import.meta.url), "utf8");
  for (const needle of [
    "mergeNodeRows(data, fleetNodes)",
    "buildShaBadge(node)",
    "nodeVersionText(node)",
    "title={sha.full}",
    "{sha.short}",
    "font-mono",
  ]) {
    assert.ok(nodeRows.includes(needle), `NodeRows.tsx includes ${needle}`);
  }
  assert.ok(fleetView.includes("<NodeRows fleetNodes={data.nodes} />"), "FleetView passes /api/fleet nodes to NodeRows");
});

test("enrolled v1 nodes from PR #203 /api/nodes normalize state, last_beat_ms, drained, and telemetry", () => {
  const sha = "0123456789abcdef0123456789abcdef01234567";
  const rows = Object.fromEntries(
    normalizeNodes({
      nodes: [
        {
          name: "n252",
          proto: "node-v1",
          state: "active",
          drained: false,
          degraded: false,
          lost: false,
          last_beat_ms: 1_000_000,
          build_sha: sha,
          node_version: "0.1.0",
          effective_drained: false,
          schedulable: true,
        },
        {
          name: "n-drained",
          proto: "node-v1",
          state: "active,drained",
          drained: true,
          last_beat_ms: 1_000_100,
          effective_drained: true,
          schedulable: false,
        },
        {
          name: "n-degraded",
          proto: "node-v1",
          state: "active,degraded",
          drained: false,
          degraded: true,
          last_beat_ms: 1_000_200,
          effective_drained: false,
          schedulable: false,
        },
        {
          name: "n-lost",
          proto: "node-v1",
          state: "lost",
          drained: false,
          lost: true,
          last_beat_ms: 800_000,
          schedulable: false,
        },
        {
          name: "n-lost-drained",
          proto: "node-v1",
          state: "lost,drained",
          drained: true,
          lost: true,
          last_beat_ms: 700_000,
          effective_drained: true,
          schedulable: false,
        },
        {
          name: "n-revoked",
          proto: "node-v1",
          state: "revoked",
        },
      ],
    }).map((row) => [row.name, row]),
  );

  assert.equal(rows.n252?.state, "live");
  assert.equal(rows.n252?.sampled_ms, 1_000_000);
  assert.equal(rows.n252?.build_sha, sha);
  assert.equal(rows.n252?.node_version, "0.1.0");
  assert.equal(rows.n252?.effective_drained, false);
  assert.equal(rows.n252?.schedulable, true);

  assert.equal(rows["n-drained"]?.state, "drained");
  assert.equal(rows["n-drained"]?.effective_drained, true);
  assert.equal(rows["n-drained"]?.sampled_ms, 1_000_100);

  assert.equal(rows["n-degraded"]?.state, "live");
  assert.equal(rows["n-degraded"]?.schedulable, false);

  assert.equal(rows["n-lost"]?.state, "lost");
  assert.equal(rows["n-lost"]?.sampled_ms, 800_000);

  assert.equal(rows["n-lost-drained"]?.state, "lost");
  assert.equal(rows["n-lost-drained"]?.effective_drained, true);

  assert.equal(rows["n-revoked"], undefined);
});
