import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  fetchDevicesWithFetch,
  fetchWhoamiWithFetch,
  flattenUiNodes,
  postDeviceCommandWithFetch,
  type DevicesFetch,
} from "../src/devices.ts";

function mockJson(status: number, body: unknown) {
  return {
    ok: status >= 200 && status < 300,
    status,
    async json() {
      return body;
    },
  };
}

test("fetchWhoamiWithFetch parses interactive and readonly_viewer responses and signals 401", async () => {
  let unauthorizedCount = 0;
  const fetcher: DevicesFetch = async () =>
    mockJson(200, {
      principal: "readonly_viewer",
      mode: "read_only",
      can_write: false,
      can_read_external_agents: false,
      token_required: true,
      expires_at_ms: 1_800_000_000_000,
      version: "0.9.0",
    });

  const ro = await fetchWhoamiWithFetch(fetcher, { Authorization: "Bearer v" }, {
    onUnauthorized: () => {
      unauthorizedCount += 1;
    },
  });
  assert.equal(ro.ok, true);
  if (ro.ok) {
    assert.equal(ro.data.principal, "readonly_viewer");
    assert.equal(ro.data.can_write, false);
    assert.equal(ro.data.can_read_external_agents, false);
  }
  assert.equal(unauthorizedCount, 0);

  const unauth = await fetchWhoamiWithFetch(async () => mockJson(401, { error: "unauthorized" }), {}, {
    onUnauthorized: () => {
      unauthorizedCount += 1;
    },
  });
  assert.equal(unauth.ok, false);
  if (!unauth.ok) assert.equal(unauth.error.kind, "unauthorized");
  assert.equal(unauthorizedCount, 1);
});

test("fetchDevicesWithFetch and postDeviceCommandWithFetch handle 200/202/403/409 accurately", async () => {
  const listRes = await fetchDevicesWithFetch(
    async () =>
      mockJson(200, {
        count: 1,
        server_time_ms: 1000,
        devices: [
          {
            device_id: "pixel-tablet-1",
            label: "Pixel Tablet",
            platform: "android",
            app_version: "0.7.1",
            active_package: "com.example.notes",
            control_enabled: true,
            accessibility_ready: true,
            notifications_ready: true,
            relay_enabled: true,
            emergency_stopped: false,
            registered_at_ms: 900,
            last_seen_ms: 1000,
            online: true,
            latest_observation: null,
            latest_observed_at_ms: null,
            node_count: 3,
            password_redacted_count: 1,
            commands: [],
          },
        ],
      }),
    {},
  );
  assert.equal(listRes.ok, true);
  if (listRes.ok) {
    assert.equal(listRes.data.devices[0]?.device_id, "pixel-tablet-1");
    assert.equal(listRes.data.devices[0]?.password_redacted_count, 1);
  }

  let capturedUrl = "";
  let capturedBody = "";
  const okCmd = await postDeviceCommandWithFetch(
    async (url, init) => {
      capturedUrl = url;
      capturedBody = String(init?.body ?? "");
      return mockJson(202, {
        command: {
          command_id: "dcmd-1",
          action: "click_node",
          params: { node_id: "0.1" },
          status: "pending",
          created_at_ms: 1001,
          dispatched_at_ms: null,
          completed_at_ms: null,
          reason: null,
          message: null,
        },
      });
    },
    { Authorization: "Bearer adm" },
    "pixel-tablet-1",
    "click_node",
    { node_id: "0.1" },
  );
  assert.equal(capturedUrl, "/api/devices/pixel-tablet-1/commands");
  assert.match(capturedBody, /click_node/);
  assert.equal(okCmd.ok, true);

  const denied = await postDeviceCommandWithFetch(
    async () => mockJson(403, { error: "read-only viewer token cannot mutate" }),
    {},
    "pixel-tablet-1",
    "screen_observe",
    {},
  );
  assert.equal(denied.ok, false);
  if (!denied.ok) assert.equal(denied.error.kind, "forbidden_readonly");

  const stopped = await postDeviceCommandWithFetch(
    async () => mockJson(409, { error: "device pixel-tablet-1 is emergency stopped" }),
    {},
    "pixel-tablet-1",
    "screen_observe",
    {},
  );
  assert.equal(stopped.ok, false);
  if (!stopped.ok) assert.equal(stopped.error.kind, "conflict");
});

test("flattenUiNodes preserves password [REDACTED] masking and pre-order depth", () => {
  const obs = {
    id: "0",
    class: "android.widget.FrameLayout",
    package: "com.example.app",
    resource_id: "",
    text: "",
    description: "",
    bounds: { left: 0, top: 0, right: 1000, bottom: 1000 },
    is_password: false,
    clickable: false,
    editable: false,
    scrollable: false,
    children: [
      {
        id: "0.0",
        class: "android.widget.EditText",
        package: "com.example.app",
        resource_id: "com.example.app:id/pin",
        text: "should-never-leak",
        description: "PIN",
        bounds: { left: 10, top: 10, right: 200, bottom: 80 },
        is_password: true,
        clickable: true,
        editable: true,
        scrollable: false,
        children: [],
      },
      {
        id: "0.1",
        class: "android.widget.Button",
        package: "com.example.app",
        resource_id: "com.example.app:id/submit",
        text: "Continue",
        description: "",
        bounds: { left: 10, top: 100, right: 200, bottom: 160 },
        is_password: false,
        clickable: true,
        editable: false,
        scrollable: false,
        children: [],
      },
    ],
  };

  const nodes = flattenUiNodes(obs);
  assert.equal(nodes.length, 3);
  assert.equal(nodes[0]?.depth, 0);
  assert.equal(nodes[1]?.id, "0.0");
  assert.equal(nodes[1]?.depth, 1);
  assert.equal(nodes[1]?.isPassword, true);
  assert.equal(nodes[1]?.text, "[REDACTED]");
  assert.equal(nodes[2]?.id, "0.1");
  assert.equal(nodes[2]?.text, "Continue");
});

test("devicesApi browser wrapper exports canonical API routes cleanly", () => {
  const src = readFileSync(new URL("../src/devicesApi.ts", import.meta.url), "utf8");
  assert.ok(src.includes('from "./devices.ts"'));
  assert.ok(src.includes("fetchWhoami"));
  assert.ok(src.includes("fetchDevices"));
  assert.ok(src.includes("postDeviceCommand"));
});
