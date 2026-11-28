// Pure API types, wire normalizers, and UI node tree helpers for `/api/auth/whoami`
// and Android device relay (`/api/devices*`). Kept free of browser `./api` imports so
// `node --test` can unit-test every path cleanly without bundler resolution.

export type WhoamiPrincipal = "local_owner" | "admin_token" | "readonly_viewer";

export interface WhoamiInfo {
  principal: WhoamiPrincipal;
  mode: "interactive" | "read_only";
  can_write: boolean;
  can_read_external_agents: boolean;
  token_required: boolean;
  expires_at_ms: number | null;
  version: string;
}

export type DeviceCommandStatus =
  | "pending"
  | "dispatched"
  | "executed"
  | "denied"
  | "needs_confirmation"
  | "failed";

export type DeviceActionName =
  | "screen_observe"
  | "click_node"
  | "click_xy"
  | "input_text"
  | "scroll"
  | "global_action"
  | "launch_app";

export interface DeviceRelayCommand {
  command_id: string;
  action: DeviceActionName | string;
  params: Record<string, unknown>;
  status: DeviceCommandStatus;
  created_at_ms: number;
  dispatched_at_ms: number | null;
  completed_at_ms: number | null;
  reason: string | null;
  message: string | null;
}

export interface DeviceUiBounds {
  left: number;
  top: number;
  right: number;
  bottom: number;
}

export interface DeviceObservationNode {
  id: string;
  className: string;
  packageName: string;
  resourceId: string;
  text: string;
  description: string;
  bounds: DeviceUiBounds;
  isPassword: boolean;
  clickable: boolean;
  editable: boolean;
  scrollable: boolean;
  children: DeviceObservationNode[];
}

export interface FlattenedUiNode {
  id: string;
  depth: number;
  className: string;
  packageName: string;
  resourceId: string;
  text: string;
  description: string;
  bounds: DeviceUiBounds;
  isPassword: boolean;
  clickable: boolean;
  editable: boolean;
  scrollable: boolean;
}

export interface DeviceSnapshot {
  device_id: string;
  label: string;
  platform: string;
  app_version: string | null;
  active_package: string | null;
  control_enabled: boolean;
  accessibility_ready: boolean;
  notifications_ready: boolean;
  relay_enabled: boolean;
  emergency_stopped: boolean;
  registered_at_ms: number;
  last_seen_ms: number;
  online: boolean;
  latest_observation: Record<string, unknown> | null;
  latest_observed_at_ms: number | null;
  node_count: number;
  password_redacted_count: number;
  commands: DeviceRelayCommand[];
}

export interface DevicesListResponse {
  devices: DeviceSnapshot[];
  count: number;
  server_time_ms: number;
}

export type DevicesErrorKind =
  | "unauthorized"
  | "forbidden_readonly"
  | "bad_request"
  | "conflict"
  | "not_found"
  | "unavailable";

export interface DevicesHttpError {
  kind: DevicesErrorKind;
  status: number | null;
  message: string;
}

export type DevicesResult<T> =
  | { ok: true; data: T }
  | { ok: false; error: DevicesHttpError };

export interface DevicesResponseLike {
  ok: boolean;
  status: number;
  json(): Promise<unknown>;
}

export type DevicesFetch = (
  url: string,
  init?: RequestInit,
) => Promise<DevicesResponseLike>;

type JsonObj = Record<string, unknown>;

function asObj(raw: unknown): JsonObj | null {
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) {
    return null;
  }
  return raw as JsonObj;
}

function asString(raw: unknown, fallback = ""): string {
  return typeof raw === "string" ? raw : fallback;
}

function asOptString(raw: unknown): string | null {
  return typeof raw === "string" && raw.length > 0 ? raw : null;
}

function asNumber(raw: unknown, fallback = 0): number {
  return typeof raw === "number" && Number.isFinite(raw) ? raw : fallback;
}

function asOptNumber(raw: unknown): number | null {
  return typeof raw === "number" && Number.isFinite(raw) ? raw : null;
}

export function parseWhoamiInfo(raw: unknown): WhoamiInfo {
  const obj = asObj(raw);
  if (!obj) throw new Error("expected object for WhoamiInfo");
  const rawPrincipal = asString(obj.principal, "local_owner");
  const principal: WhoamiPrincipal =
    rawPrincipal === "admin_token" ||
    rawPrincipal === "readonly_viewer" ||
    rawPrincipal === "local_owner"
      ? rawPrincipal
      : "readonly_viewer";
  const mode = obj.mode === "read_only" ? "read_only" : "interactive";
  return {
    principal,
    mode,
    can_write: obj.can_write === true,
    can_read_external_agents: obj.can_read_external_agents === true,
    token_required: obj.token_required === true,
    expires_at_ms: asOptNumber(obj.expires_at_ms),
    version: asString(obj.version, ""),
  };
}

function parseRelayCommand(raw: unknown): DeviceRelayCommand | null {
  const obj = asObj(raw);
  if (!obj) return null;
  const command_id = asString(obj.command_id);
  const action = asString(obj.action);
  if (!command_id || !action) return null;
  const statusRaw = asString(obj.status, "pending");
  const status: DeviceCommandStatus =
    statusRaw === "pending" ||
    statusRaw === "dispatched" ||
    statusRaw === "executed" ||
    statusRaw === "denied" ||
    statusRaw === "needs_confirmation" ||
    statusRaw === "failed"
      ? statusRaw
      : "failed";
  return {
    command_id,
    action,
    params: asObj(obj.params) ?? {},
    status,
    created_at_ms: asNumber(obj.created_at_ms),
    dispatched_at_ms: asOptNumber(obj.dispatched_at_ms),
    completed_at_ms: asOptNumber(obj.completed_at_ms),
    reason: asOptString(obj.reason),
    message: asOptString(obj.message),
  };
}

export function parseDeviceSnapshot(raw: unknown): DeviceSnapshot {
  const obj = asObj(raw);
  if (!obj) throw new Error("expected object for DeviceSnapshot");
  const device_id = asString(obj.device_id);
  if (!device_id) throw new Error("missing device_id");
  const rawCommands = Array.isArray(obj.commands) ? obj.commands : [];
  const commands: DeviceRelayCommand[] = [];
  for (const item of rawCommands) {
    const parsed = parseRelayCommand(item);
    if (parsed) commands.push(parsed);
  }
  return {
    device_id,
    label: asString(obj.label, device_id),
    platform: asString(obj.platform, "android"),
    app_version: asOptString(obj.app_version),
    active_package: asOptString(obj.active_package),
    control_enabled: obj.control_enabled === true,
    accessibility_ready: obj.accessibility_ready === true,
    notifications_ready: obj.notifications_ready === true,
    relay_enabled: obj.relay_enabled === true,
    emergency_stopped: obj.emergency_stopped === true,
    registered_at_ms: asNumber(obj.registered_at_ms),
    last_seen_ms: asNumber(obj.last_seen_ms),
    online: obj.online === true,
    latest_observation: asObj(obj.latest_observation),
    latest_observed_at_ms: asOptNumber(obj.latest_observed_at_ms),
    node_count: asNumber(obj.node_count),
    password_redacted_count: asNumber(obj.password_redacted_count),
    commands,
  };
}

export function parseDevicesListResponse(raw: unknown): DevicesListResponse {
  const obj = asObj(raw);
  if (!obj || !Array.isArray(obj.devices)) {
    throw new Error("expected object with devices[]");
  }
  const devices = obj.devices.map((item) => parseDeviceSnapshot(item));
  return {
    devices,
    count: asNumber(obj.count, devices.length),
    server_time_ms: asNumber(obj.server_time_ms),
  };
}

export function parseUiTreeRoot(observation: unknown): DeviceObservationNode | null {
  const obj = asObj(observation);
  if (!obj) return null;
  // Support either { tree: UiNode }, { root: UiNode }, or direct UiNode object.
  const candidate = asObj(obj.tree) ?? asObj(obj.root) ?? obj;
  return normalizeUiNode(candidate);
}

function normalizeUiNode(raw: unknown): DeviceObservationNode | null {
  const obj = asObj(raw);
  if (!obj) return null;
  const id = asString(obj.id);
  const boundsObj = asObj(obj.bounds);
  const isPassword = obj.is_password === true || obj.isPassword === true;
  const rawChildren = Array.isArray(obj.children) ? obj.children : [];
  const children: DeviceObservationNode[] = [];
  for (const child of rawChildren) {
    const normalized = normalizeUiNode(child);
    if (normalized) children.push(normalized);
  }
  return {
    id,
    className: asString(obj.class ?? obj.className),
    packageName: asString(obj.package ?? obj.package_name ?? obj.packageName),
    resourceId: asString(obj.resource_id ?? obj.resourceId),
    text: isPassword ? "[REDACTED]" : asString(obj.text),
    description: asString(obj.description),
    bounds: {
      left: asNumber(boundsObj?.left),
      top: asNumber(boundsObj?.top),
      right: asNumber(boundsObj?.right),
      bottom: asNumber(boundsObj?.bottom),
    },
    isPassword,
    clickable: obj.clickable === true,
    editable: obj.editable === true,
    scrollable: obj.scrollable === true,
    children,
  };
}

/** Pre-order flatten a redacted Android `UiNode` tree up to `maxNodes` entries. */
export function flattenUiNodes(
  observationOrNode: unknown,
  maxNodes = 400,
): FlattenedUiNode[] {
  const root = parseUiTreeRoot(observationOrNode);
  if (!root) return [];
  const out: FlattenedUiNode[] = [];
  const stack: Array<{ node: DeviceObservationNode; depth: number }> = [
    { node: root, depth: 0 },
  ];
  while (stack.length > 0 && out.length < maxNodes) {
    const top = stack.pop();
    if (!top) break;
    const { node, depth } = top;
    out.push({
      id: node.id,
      depth,
      className: node.className,
      packageName: node.packageName,
      resourceId: node.resourceId,
      text: node.isPassword ? "[REDACTED]" : node.text,
      description: node.description,
      bounds: node.bounds,
      isPassword: node.isPassword,
      clickable: node.clickable,
      editable: node.editable,
      scrollable: node.scrollable,
    });
    for (let i = node.children.length - 1; i >= 0; i--) {
      stack.push({ node: node.children[i], depth: depth + 1 });
    }
  }
  return out;
}

function classifyDevicesError(status: number | null, message: string): DevicesHttpError {
  if (status === 401) {
    return { kind: "unauthorized", status, message: message || "未通过认证（HTTP 401）" };
  }
  if (status === 403) {
    return {
      kind: "forbidden_readonly",
      status,
      message: message || "当前会话为只读权限（HTTP 403）",
    };
  }
  if (status === 400) {
    return { kind: "bad_request", status, message: message || "请求参数无效（HTTP 400）" };
  }
  if (status === 404) {
    return { kind: "not_found", status, message: message || "目标设备或命令不存在（HTTP 404）" };
  }
  if (status === 409) {
    return { kind: "conflict", status, message: message || "设备处于急停或未启用中继（HTTP 409）" };
  }
  return { kind: "unavailable", status, message: message || "请求设备控制中继失败" };
}

async function readDevicesErrorMessage(response: DevicesResponseLike): Promise<string> {
  try {
    const parsed = await response.json();
    const obj = asObj(parsed);
    if (obj && typeof obj.error === "string" && obj.error.trim().length > 0) {
      return obj.error.trim();
    }
  } catch {
    // ignore invalid error JSON
  }
  return `HTTP ${response.status}`;
}

export async function fetchWhoamiWithFetch(
  fetcher: DevicesFetch,
  headers: Record<string, string>,
  options: { signal?: AbortSignal; onUnauthorized?: () => void } = {},
): Promise<DevicesResult<WhoamiInfo>> {
  let response: DevicesResponseLike;
  try {
    response = await fetcher("/api/auth/whoami", {
      method: "GET",
      signal: options.signal,
      cache: "no-store",
      headers: { Accept: "application/json", ...headers },
    });
  } catch (cause) {
    if (cause instanceof DOMException && cause.name === "AbortError") throw cause;
    return {
      ok: false,
      error: classifyDevicesError(null, cause instanceof Error ? cause.message : "网络请求失败"),
    };
  }
  if (response.status === 401) {
    const msg = await readDevicesErrorMessage(response);
    options.onUnauthorized?.();
    return { ok: false, error: classifyDevicesError(401, msg) };
  }
  if (!response.ok) {
    return {
      ok: false,
      error: classifyDevicesError(response.status, await readDevicesErrorMessage(response)),
    };
  }
  try {
    return { ok: true, data: parseWhoamiInfo(await response.json()) };
  } catch (err) {
    return {
      ok: false,
      error: classifyDevicesError(null, err instanceof Error ? err.message : "认证响应格式无效"),
    };
  }
}

export async function fetchDevicesWithFetch(
  fetcher: DevicesFetch,
  headers: Record<string, string>,
  options: { signal?: AbortSignal; onUnauthorized?: () => void } = {},
): Promise<DevicesResult<DevicesListResponse>> {
  let response: DevicesResponseLike;
  try {
    response = await fetcher("/api/devices", {
      method: "GET",
      signal: options.signal,
      cache: "no-store",
      headers: { Accept: "application/json", ...headers },
    });
  } catch (cause) {
    if (cause instanceof DOMException && cause.name === "AbortError") throw cause;
    return {
      ok: false,
      error: classifyDevicesError(null, cause instanceof Error ? cause.message : "网络请求失败"),
    };
  }
  if (response.status === 401) {
    const msg = await readDevicesErrorMessage(response);
    options.onUnauthorized?.();
    return { ok: false, error: classifyDevicesError(401, msg) };
  }
  if (!response.ok) {
    return {
      ok: false,
      error: classifyDevicesError(response.status, await readDevicesErrorMessage(response)),
    };
  }
  try {
    return { ok: true, data: parseDevicesListResponse(await response.json()) };
  } catch (err) {
    return {
      ok: false,
      error: classifyDevicesError(null, err instanceof Error ? err.message : "设备列表格式无效"),
    };
  }
}

export async function postDeviceCommandWithFetch(
  fetcher: DevicesFetch,
  headers: Record<string, string>,
  deviceId: string,
  action: DeviceActionName,
  params: Record<string, unknown>,
  options: { signal?: AbortSignal; onUnauthorized?: () => void } = {},
): Promise<DevicesResult<DeviceRelayCommand>> {
  const cleanId = deviceId.trim();
  if (!cleanId) {
    return { ok: false, error: classifyDevicesError(400, "device_id 不能为空") };
  }
  let response: DevicesResponseLike;
  try {
    response = await fetcher(`/api/devices/${encodeURIComponent(cleanId)}/commands`, {
      method: "POST",
      signal: options.signal,
      cache: "no-store",
      headers: {
        Accept: "application/json",
        ...headers,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ action, params }),
    });
  } catch (cause) {
    if (cause instanceof DOMException && cause.name === "AbortError") throw cause;
    return {
      ok: false,
      error: classifyDevicesError(null, cause instanceof Error ? cause.message : "网络请求失败"),
    };
  }
  if (response.status === 401) {
    const msg = await readDevicesErrorMessage(response);
    options.onUnauthorized?.();
    return { ok: false, error: classifyDevicesError(401, msg) };
  }
  if (!response.ok) {
    return {
      ok: false,
      error: classifyDevicesError(response.status, await readDevicesErrorMessage(response)),
    };
  }
  try {
    const raw = asObj(await response.json());
    const cmd = parseRelayCommand(raw?.command);
    if (!cmd) throw new Error("服务端未返回有效 command 对象");
    return { ok: true, data: cmd };
  } catch (err) {
    return {
      ok: false,
      error: classifyDevicesError(null, err instanceof Error ? err.message : "命令响应无效"),
    };
  }
}
