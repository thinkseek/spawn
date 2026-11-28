import assert from "node:assert/strict";
import { test } from "node:test";

import {
  agentActionUrl,
  agentSnapshotUrl,
  agentsListUrl,
  buildSendKeysAction,
  buildSendTextAction,
  classifyAgentHttpError,
  defaultSendPolicyMode,
  evaluateActionCapabilities,
  fetchAgentSnapshotWithFetch,
  fetchAgentsWithFetch,
  isValidScreenGuard,
  normalizeKeyChord,
  parseAgentsListResponse,
  parseKeyChordList,
  parseStyledLine,
  parseTuiActionOutcome,
  parseTuiSnapshot,
  postAgentActionWithFetch,
  reduceAction,
  reduceDiscovery,
  reduceSnapshot,
  snapshotFromState,
  validateTerminalAddress,
  visibleAgentsForRole,
  type AgentDiscoveryState,
  type AgentEtagEntry,
  type AgentFetch,
  type AgentResponseLike,
  type AgentsListResponse,
  type DiscoveredTerminal,
  type SnapshotLoadState,
  type TuiSnapshot,
} from "../src/agentRuntime.ts";
import { classifyTokenRole, type SessionRole } from "../src/token.ts";

function fakeResponse(
  status: number,
  body: unknown,
  headersMap: Record<string, string> = {},
): AgentResponseLike {
  const normalized = new Map<string, string>(
    Object.entries(headersMap).map(([k, v]) => [k.toLowerCase(), v]),
  );
  return {
    ok: status >= 200 && status < 300,
    status,
    headers: {
      get(name: string) {
        return normalized.get(name.toLowerCase()) ?? null;
      },
    },
    async json() {
      return body;
    },
    async arrayBuffer() {
      return new ArrayBuffer(0);
    },
  };
}

function sampleDiscoveredTerminals(): DiscoveredTerminal[] {
  return [
    {
      address: "t1",
      machine: null,
      name: "spawn-worker-1",
      origin: {
        kind: "spawn_managed",
        owner_session: "sess-1",
        terminal_name: "spawn-exec_1",
        workspace: "/repo",
      },
      state: { state: "live", pid: 101 },
      cwd: "/repo",
      title: "claude",
      argv: ["claude"],
      labels: { spawn_owner: "sess-1", spawn_terminal: "spawn-exec_1" },
      cols: 200,
      rows: 50,
      agent: "claude",
      agent_status: "working",
      turn: 3,
      keep: false,
    },
    {
      address: "n252/t7",
      machine: "n252",
      name: null,
      origin: { kind: "external" },
      state: { state: "live", pid: 777 },
      cwd: "/work",
      title: "menu",
      argv: ["python3", "menu.py"],
      labels: {},
      cols: 120,
      rows: 40,
      agent: null,
      agent_status: "blocked",
      turn: 0,
      keep: true,
    },
    {
      address: "t9",
      machine: null,
      name: "done-job",
      origin: {
        kind: "spawn_managed",
        owner_session: "sess-9",
        terminal_name: "spawn-exec_9",
        workspace: null,
      },
      state: { state: "exited", cause: { kind: "code", code: 0 } },
      cwd: "/repo",
      title: "bash",
      argv: ["bash"],
      labels: { spawn_owner: "sess-9", spawn_terminal: "spawn-exec_9" },
      cols: 80,
      rows: 24,
      agent: "codex",
      agent_status: "idle",
      turn: 7,
      keep: false,
    },
  ];
}

function styledSnapshotFixture(address = "t1", guard = "0x1122334455667788"): TuiSnapshot {
  return {
    address,
    cols: 120,
    rows: 40,
    live_state: { state: "live", pid: 202 },
    title: "menu",
    fidelity: {
      fidelity: "styled_vt",
      cursor: { row: 3, col: 4 },
      alt_screen: true,
      styled_lines: [
        "Choose backend:",
        "\u001b[7m❯ 1. First option\u001b[27m",
        "  2. Second option",
      ],
    },
    plain_lines: ["Choose backend:", "❯ 1. First option", "  2. Second option"],
    agent: "claude",
    agent_status: "blocked",
    rule: "question_menu",
    blocker_visible: true,
    choices: [
      { index: 1, label: "First option", selected: true, line_index: 1 },
      { index: 2, label: "Second option", selected: false, line_index: 2 },
    ],
    guard,
  };
}

function plainTextSnapshotFixture(address = "n252/t7"): TuiSnapshot {
  return {
    address,
    cols: 100,
    rows: 30,
    live_state: { state: "live", pid: 777 },
    title: null,
    fidelity: {
      fidelity: "plain_text_only",
      read_mode: { mode: "stitched" },
      complete: false,
      why: "blocked",
    },
    plain_lines: ["Select option:", "1. One", "2. Two"],
    agent: null,
    agent_status: "unknown",
    rule: null,
    blocker_visible: false,
    choices: [],
    guard: "0xaabbccddeeff0011",
  };
}

test("classifyTokenRole identifies sv1 viewer tokens vs operator credentials", () => {
  const sig = "a".repeat(43);
  const viewerTok = `sv1.1800000000.${sig}`;
  assert.deepEqual(classifyTokenRole(viewerTok), {
    kind: "viewer",
    expiresAtMs: 1_800_000_000_000,
  });

  for (const opTok of [
    null,
    "admin-master-secret",
    "sv1.0123." + sig,
    "sv1.1800000000.short",
    "sv2.1800000000." + sig,
  ]) {
    assert.deepEqual(classifyTokenRole(opTok), { kind: "operator" });
  }
});

test("terminal address, key chord validators, and URL encoders fail closed", () => {
  assert.equal(validateTerminalAddress("t1"), true);
  assert.equal(validateTerminalAddress("n252/t7"), true);
  for (const bad of ["", "/", "/t1", "t1/", "n252//t7", "..", "n252/..", "./t1", "a/b/c", "t 1", "t1\n"]) {
    assert.equal(validateTerminalAddress(bad), false, bad);
    assert.throws(() => agentSnapshotUrl(bad));
    assert.throws(() => agentActionUrl(bad));
  }

  assert.equal(agentsListUrl(false), "/api/agents");
  assert.equal(agentsListUrl(true), "/api/agents?all=true");
  assert.equal(agentSnapshotUrl("n252/t7"), "/api/agents/n252%2Ft7/snapshot");
  assert.equal(agentActionUrl("n252/t7"), "/api/agents/n252%2Ft7/action");

  assert.equal(normalizeKeyChord("Control+Shift+Tab"), "ctrl+shift+tab");
  assert.equal(normalizeKeyChord("digit3"), "3");
  assert.equal(normalizeKeyChord("backslash"), "\\");
  assert.equal(normalizeKeyChord("bad+bad+a"), null);
  assert.equal(normalizeKeyChord("ctrl+ctrl+c"), null);

  assert.deepEqual(parseKeyChordList("down, enter"), {
    ok: true,
    keys: ["down", "enter"],
  });
  assert.equal(parseKeyChordList("   ").ok, false);
  assert.equal(parseKeyChordList("down invalid_key").ok, false);
  assert.equal(isValidScreenGuard("0x0123abcd"), true);
  assert.equal(isValidScreenGuard("not-a-guard"), false);
});

test("wire parsers decode StyledVt, PlainTextOnly, and flattened TuiActionOutcome contracts", () => {
  const listRaw = { agents: sampleDiscoveredTerminals(), count: 3 };
  const listParsed = parseAgentsListResponse(listRaw);
  assert.equal(listParsed.count, 3);
  assert.equal(listParsed.agents[0].origin.kind, "spawn_managed");
  assert.equal(listParsed.agents[1].origin.kind, "external");

  const styled = parseTuiSnapshot({
    ...styledSnapshotFixture("t1", "0xffffffffffffffff"),
    snapshot_hash: 18446744073709551615,
  });
  assert.equal(styled.fidelity.fidelity, "styled_vt");
  assert.deepEqual(styled.fidelity.cursor, { row: 3, col: 4 });
  assert.equal(styled.guard, "0xffffffffffffffff");
  assert.equal("snapshot_hash" in styled, false);

  const plain = parseTuiSnapshot(plainTextSnapshotFixture());
  assert.equal(plain.fidelity.fidelity, "plain_text_only");
  assert.equal(plain.fidelity.read_mode.mode, "stitched");
  assert.equal(plain.fidelity.why, "blocked");

  assert.throws(() => parseTuiSnapshot({ ...styledSnapshotFixture(), guard: "bad" }));
  assert.throws(() =>
    parseTuiSnapshot({
      ...styledSnapshotFixture(),
      fidelity: { fidelity: "unknown_mode" },
    }),
  );

  const interrupted = parseTuiActionOutcome({
    outcome_kind: "interrupted",
    outcome: { outcome: "stopped" },
    key_sent: "esc",
  });
  assert.equal(interrupted.outcome_kind, "interrupted");
  assert.equal(interrupted.key_sent, "esc");

  const waited = parseTuiActionOutcome({
    outcome_kind: "waited",
    terminal: "t1",
    reason: { kind: "exit", cause: { kind: "signal", signal: 15 } },
    status: null,
    turn: 9,
  });
  assert.equal(waited.outcome_kind, "waited");
  assert.equal(waited.reason.kind, "exit");
});

test("parseStyledLine extracts inverse highlights and strips non-SGR escapes safely", () => {
  const spans = parseStyledLine("\u001b[1;7mSelected\u001b[27m \u001b[32mOK\u001b[0m\u001b[?25h");
  assert.deepEqual(
    spans.map((s) => ({
      text: s.text,
      bold: s.bold,
      inverse: s.inverse,
      fg: s.fg,
    })),
    [
      { text: "Selected", bold: true, inverse: true, fg: null },
      { text: " ", bold: true, inverse: false, fg: null },
      { text: "OK", bold: true, inverse: false, fg: "success" },
    ],
  );
});

test("fetch helpers handle ?all=true, 304 ETag reuse, 401 vs 403 separation, and 409 conflicts", async () => {
  const etagCache = new Map<string, AgentEtagEntry<AgentsListResponse>>();
  const seenUrls: string[] = [];
  let seenIfNoneMatch: string | null = null;
  let calls = 0;

  const listFetcher: AgentFetch = async (url, init) => {
    seenUrls.push(url);
    const hdrs = init.headers as Record<string, string>;
    seenIfNoneMatch = hdrs["If-None-Match"] ?? null;
    calls++;
    if (calls === 1) {
      return fakeResponse(
        200,
        { agents: sampleDiscoveredTerminals(), count: 3 },
        { ETag: '"etag-1"' },
      );
    }
    return fakeResponse(304, null);
  };

  const r1 = await fetchAgentsWithFetch(listFetcher, { Authorization: "Bearer op" }, true, {
    etagCache,
  });
  assert.equal(r1.ok, true);
  const r2 = await fetchAgentsWithFetch(listFetcher, { Authorization: "Bearer op" }, true, {
    etagCache,
  });
  assert.equal(r2.ok, true);
  if (r1.ok && r2.ok) {
    assert.strictEqual(r1.data, r2.data);
  }
  assert.equal(seenUrls[0], "/api/agents?all=true");
  assert.equal(seenIfNoneMatch, '"etag-1"');

  // 403 external snapshot NEVER invokes onUnauthorized
  let unauthCalls = 0;
  const externalForbiddenFetcher: AgentFetch = async () =>
    fakeResponse(403, {
      error: "forbidden: read-only viewer cannot snapshot external terminal n252/t7",
    });
  const snap403 = await fetchAgentSnapshotWithFetch(
    externalForbiddenFetcher,
    { Authorization: "Bearer sv1.x.y" },
    "n252/t7",
    { onUnauthorized: () => unauthCalls++ },
  );
  assert.equal(snap403.ok, false);
  if (!snap403.ok) {
    assert.equal(snap403.error.kind, "forbidden_external");
  }
  assert.equal(unauthCalls, 0);

  // 403 viewer action NEVER invokes onUnauthorized
  const write403Fetcher: AgentFetch = async () =>
    fakeResponse(403, { error: "read-only viewer token cannot mutate state" });
  const write403 = await postAgentActionWithFetch(
    write403Fetcher,
    { Authorization: "Bearer sv1.x.y" },
    "t1",
    { action: "interrupt" },
    { onUnauthorized: () => unauthCalls++ },
  );
  assert.equal(write403.ok, false);
  if (!write403.ok) {
    assert.equal(write403.error.kind, "forbidden_readonly");
  }
  assert.equal(unauthCalls, 0);

  // 401 DOES invoke onUnauthorized
  const unauthFetcher: AgentFetch = async () => fakeResponse(401, { error: "unauthorized" });
  const write401 = await postAgentActionWithFetch(
    unauthFetcher,
    {},
    "t1",
    { action: "interrupt" },
    { onUnauthorized: () => unauthCalls++ },
  );
  assert.equal(write401.ok, false);
  if (!write401.ok) {
    assert.equal(write401.error.kind, "unauthorized");
  }
  assert.equal(unauthCalls, 1);

  // 409 guard_mismatch parses expected + actual guards cleanly
  const mismatch = classifyAgentHttpError(
    409,
    "guard_mismatch: terminal t1 screen changed (expected 0x1111111111111111, got 0x2222222222222222)",
    "write",
  );
  assert.deepEqual(mismatch, {
    kind: "guard_mismatch",
    message:
      "guard_mismatch: terminal t1 screen changed (expected 0x1111111111111111, got 0x2222222222222222)",
    expectedGuard: "0x1111111111111111",
    actualGuard: "0x2222222222222222",
  });
});

test("ReadOnlyViewer RBAC allows ?all=true Spawn discovery, hides External, and blocks mutations fail-closed", async () => {
  const viewerRole: SessionRole = { kind: "viewer", expiresAtMs: 1_900_000_000_000 };
  const requestedUrls: string[] = [];
  const viewerListFetcher: AgentFetch = async (url) => {
    requestedUrls.push(url);
    // Server returns both live (t1) and exited (t9) Spawn-managed terminals under ?all=true.
    const spawnOnly = sampleDiscoveredTerminals().filter(
      (agent) => agent.origin.kind === "spawn_managed",
    );
    return fakeResponse(200, { agents: spawnOnly, count: spawnOnly.length });
  };
  const listedAll = await fetchAgentsWithFetch(
    viewerListFetcher,
    { Authorization: "Bearer sv1.x.y" },
    true,
  );
  assert.equal(requestedUrls[0], "/api/agents?all=true");
  assert.equal(listedAll.ok, true);
  if (listedAll.ok) {
    const visible = visibleAgentsForRole(listedAll.data.agents, viewerRole);
    assert.deepEqual(
      visible.map((a) => a.address),
      ["t1", "t9"],
    );
  }

  // Defense-in-depth client filter still drops External even if present in payload.
  const filtered = visibleAgentsForRole(sampleDiscoveredTerminals(), viewerRole);
  assert.deepEqual(
    filtered.map((a) => a.address),
    ["t1", "t9"],
  );

  const styledState: SnapshotLoadState = {
    kind: "ready",
    address: "t1",
    snapshot: styledSnapshotFixture(),
    capturedAtMs: 1_000,
  };
  const caps = evaluateActionCapabilities(viewerRole, false, styledState, "t1");
  assert.equal(caps.canMutate, false);
  assert.equal(caps.guarded.allowed, false);
  assert.equal(
    buildSendTextAction({ text: "hello", mode: "when_unblocked", capabilities: caps }).ok,
    false,
  );
  assert.equal(
    buildSendKeysAction({ keys: ["enter"], useGuard: true, capabilities: caps }).ok,
    false,
  );
});

test("snapshotFromState and evaluateActionCapabilities reject address mismatches on frame 0", () => {
  const operatorRole: SessionRole = { kind: "operator" };
  const t1Snap = styledSnapshotFixture("t1", "0x1122334455667788");
  const readyT1: SnapshotLoadState = {
    kind: "ready",
    address: "t1",
    snapshot: t1Snap,
    capturedAtMs: 1_000,
  };

  assert.strictEqual(snapshotFromState(readyT1, "t1"), t1Snap);
  assert.equal(snapshotFromState(readyT1, "t2"), null);
  assert.equal(snapshotFromState(readyT1, null), null);

  const staleFrame0Caps = evaluateActionCapabilities(operatorRole, false, readyT1, "t2");
  assert.equal(staleFrame0Caps.canMutate, false);
  assert.equal(staleFrame0Caps.guarded.allowed, false);
  if (!staleFrame0Caps.guarded.allowed) {
    assert.equal(staleFrame0Caps.guarded.reason, "no_snapshot");
  }

  const mismatchedPayload: SnapshotLoadState = {
    kind: "ready",
    address: "t2",
    snapshot: t1Snap,
    capturedAtMs: 1_000,
  };
  assert.equal(snapshotFromState(mismatchedPayload, "t2"), null);

  const loadingWithT1Prev: SnapshotLoadState = {
    kind: "loading",
    address: "t1",
    previous: t1Snap,
  };
  assert.strictEqual(snapshotFromState(loadingWithT1Prev, "t1"), t1Snap);
  assert.equal(snapshotFromState(loadingWithT1Prev, "t2"), null);
});

test("PlainTextOnly fidelity blocks guarded SendKeys and only_if policies while allowing unguarded text", () => {
  const operatorRole: SessionRole = { kind: "operator" };
  const plain = plainTextSnapshotFixture("n252/t7");
  assert.equal(defaultSendPolicyMode(plain), "when_unblocked");

  const plainState: SnapshotLoadState = {
    kind: "ready",
    address: "n252/t7",
    snapshot: plain,
    capturedAtMs: 2_000,
  };
  const caps = evaluateActionCapabilities(operatorRole, false, plainState, "n252/t7");
  assert.equal(caps.canMutate, true);
  assert.equal(caps.selectionObservable, false);
  assert.equal(caps.guarded.allowed, false);
  if (!caps.guarded.allowed) {
    assert.equal(caps.guarded.reason, "plain_text_only");
  }

  for (const mode of ["only_if_guard", "only_if_guard_idle", "only_if_guard_working"] as const) {
    const blocked = buildSendTextAction({ text: "ls", mode, capabilities: caps });
    assert.equal(blocked.ok, false, mode);
  }
  const allowedText = buildSendTextAction({
    text: "ls",
    mode: "when_unblocked",
    capabilities: caps,
  });
  assert.deepEqual(allowedText, {
    ok: true,
    action: {
      action: "send_text",
      text: "ls",
      policy: { kind: "when_unblocked" },
    },
  });

  const blockedKeys = buildSendKeysAction({
    keys: ["down", "enter"],
    useGuard: true,
    capabilities: caps,
  });
  assert.equal(blockedKeys.ok, false);
});

test("state machine reducers handle 409 guard_mismatch auto-refresh without retry, 403, and 401", () => {
  let disc: AgentDiscoveryState = { kind: "loading", includeAll: false };
  disc = reduceDiscovery(disc, {
    type: "fetch_succeeded",
    includeAll: false,
    agents: sampleDiscoveredTerminals(),
    nowMs: 1_000,
  });
  assert.equal(disc.kind, "ready");
  disc = reduceDiscovery(disc, {
    type: "fetch_failed",
    includeAll: false,
    error: classifyAgentHttpError(401, "unauthorized", "read"),
  });
  assert.equal(disc.kind, "unauthorized");

  let snap: SnapshotLoadState = { kind: "idle" };
  snap = reduceSnapshot(snap, { type: "selected", address: "n252/t7" });
  assert.equal(snap.kind, "loading");
  snap = reduceSnapshot(snap, {
    type: "fetch_failed",
    address: "n252/t7",
    error: classifyAgentHttpError(
      403,
      "forbidden: read-only viewer cannot snapshot external terminal n252/t7",
      "read",
    ),
  });
  assert.equal(snap.kind, "forbidden_external");

  const styled = styledSnapshotFixture("t1", "0x1111111111111111");
  const caps = evaluateActionCapabilities(
    { kind: "operator" },
    false,
    { kind: "ready", address: "t1", snapshot: styled, capturedAtMs: 5_000 },
    "t1",
  );
  const built = buildSendKeysAction({
    keys: ["enter"],
    useGuard: true,
    capabilities: caps,
  });
  assert.equal(built.ok, true);
  if (!built.ok) return;

  let act = reduceAction(
    { kind: "idle" },
    { type: "submit_started", address: "t1", action: built.action },
  );
  assert.equal(act.kind, "submitting");

  act = reduceAction(act, {
    type: "submit_failed",
    address: "t1",
    action: built.action,
    error: classifyAgentHttpError(
      409,
      "guard_mismatch: terminal t1 screen changed (expected 0x1111111111111111, got 0x2222222222222222)",
      "write",
    ),
  });
  assert.equal(act.kind, "conflict_guard_mismatch");
  if (act.kind === "conflict_guard_mismatch") {
    assert.equal(act.refreshPhase, "refreshing");
    assert.equal(act.staleGuard, "0x1111111111111111");
    assert.equal(act.reportedGuard, "0x2222222222222222");
  }

  act = reduceAction(act, {
    type: "conflict_snapshot_refreshed",
    address: "t1",
    freshGuard: "0x3333333333333333",
  });
  assert.equal(act.kind, "conflict_guard_mismatch");
  if (act.kind === "conflict_guard_mismatch") {
    assert.equal(act.refreshPhase, "refreshed");
    assert.equal(act.freshGuard, "0x3333333333333333");
  }

  // Passive 403 on write maps cleanly to forbidden_readonly
  act = reduceAction(act, {
    type: "submit_failed",
    address: "t1",
    action: built.action,
    error: classifyAgentHttpError(403, "read-only viewer token cannot mutate state", "write"),
  });
  assert.equal(act.kind, "forbidden_readonly");
});

test("reduceSnapshot preserves snapshot identity across unchanged polls", () => {
  const first = styledSnapshotFixture("t1", "0xaaaa111122223333");
  const duplicate = styledSnapshotFixture("t1", "0xaaaa111122223333");
  let state = reduceSnapshot(
    { kind: "idle" },
    { type: "fetch_succeeded", address: "t1", snapshot: first, nowMs: 10_000 },
  );
  assert.equal(state.kind, "ready");
  const unchangedWithinWindow = reduceSnapshot(state, {
    type: "fetch_succeeded",
    address: "t1",
    snapshot: duplicate,
    nowMs: 12_000,
  });
  assert.equal(unchangedWithinWindow, state);

  const unchangedAfterWindow = reduceSnapshot(state, {
    type: "fetch_succeeded",
    address: "t1",
    snapshot: duplicate,
    nowMs: 30_000,
  });
  assert.equal(unchangedAfterWindow.kind, "ready");
  if (unchangedAfterWindow.kind === "ready") {
    assert.equal(unchangedAfterWindow.snapshot, first);
    assert.equal(unchangedAfterWindow.capturedAtMs, 30_000);
  }
});
