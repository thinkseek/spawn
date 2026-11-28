/** The serve token as a credential: what `spawn serve` accepts as
 * `SPAWN_SERVE_TOKEN` (visible ASCII, 1 to 512 characters), so it always
 * fits an `Authorization` header. Anything else is no token: a `?token=`
 * with a newline, a NUL or a non-Latin-1 character would make every
 * `fetch` throw before a request is sent. */
export const TOKEN_PATTERN = /^[!-~]{1,512}$/;

/** `text` when it is a usable token, else null. */
export function serveCredential(text: string | null | undefined): string | null {
  return text != null && TOKEN_PATTERN.test(text) ? text : null;
}

/** What the page does with the token it was given and the one it saved. */
export interface TokenChoice {
  /** The token every API read carries, if any. */
  token: string | null;
  /** Save this one in place of the saved value. */
  save: string | null;
  /** Remove the saved value: it was not a token. */
  clear: boolean;
  /** Why a given or saved value was not used, for the page to show. */
  problem: string | null;
}

/** Decide from the page URL's `?token=` (`fromUrl`) and the saved value
 * (`saved`). A usable `?token=` wins and is saved; an unusable one is
 * neither saved nor sent. An unusable saved value is removed. */
export function chooseToken(fromUrl: string | null, saved: string | null): TokenChoice {
  const given = serveCredential(fromUrl);
  const kept = serveCredential(saved);
  const problems: string[] = [];
  if (fromUrl !== null && given === null) {
    problems.push("the ?token= in the address is not a token (visible ASCII, at most 512 characters) and was not saved");
  }
  if (saved !== null && kept === null) {
    problems.push("the saved token was not a token and was removed");
  }
  return {
    token: given ?? kept,
    save: given,
    clear: saved !== null && kept === null && given === null,
    problem: problems.length > 0 ? `${problems.join("; ")}.` : null,
  };
}

/** LocalStorage key holding the persisted serve token across tabs and reloads. */
export const TOKEN_KEY = "spawn.serve.token";

/** Closed reason why the page currently asks for the serve token. */
export type AuthPromptReason = "missing_token" | "rejected_token" | "invalid_token";

/** Closed outcome of probing `GET /api/overview`. */
export type AuthProbeResult =
  | { kind: "authenticated" }
  | { kind: "needs_token" }
  | { kind: "forbidden"; message: string }
  | { kind: "unreachable"; message: string };

/** Closed authentication gate state for the web console. */
export type AuthGateState =
  | { kind: "checking" }
  | { kind: "authenticated"; hasStoredToken: boolean }
  | { kind: "needs_token"; reason: AuthPromptReason; problem: string | null; hasStoredToken: boolean }
  | { kind: "forbidden"; message: string; hasStoredToken: boolean }
  | { kind: "unreachable"; message: string; hasStoredToken: boolean };

/** Closed result of validating user-entered token text before probing the server. */
export type TokenSubmitValidation =
  | { ok: true; token: string }
  | { ok: false; message: string };

/** Closed result of submitting a candidate token through the login gate. */
export type TokenVerifyResult =
  | { ok: true }
  | { ok: false; message: string };

/** Canonical auth synchronization events emitted at HTTP boundaries or storage events. */
export type AuthSyncEvent =
  | { kind: "unauthorized" }
  | { kind: "signed_out" }
  | { kind: "token_updated"; token: string };

export const FORBIDDEN_HOST_OR_ORIGIN_MESSAGE =
  "访问被服务端拒绝（HTTP 403）：当前 Host 或 Origin 未获服务端允许，请确认访问域名或代理 Host/Origin 配置。";

export function isAbortError(cause: unknown): boolean {
  return (
    (cause instanceof DOMException || cause instanceof Error) &&
    cause.name === "AbortError"
  );
}

/** Structural response interface for pure fetch-based auth probing and unit tests. */
export interface AuthFetchResponse {
  ok: boolean;
  status: number;
  json(): Promise<unknown>;
  arrayBuffer(): Promise<ArrayBuffer>;
}

export type AuthFetch = (url: string, init: RequestInit) => Promise<AuthFetchResponse>;

/** Validate and trim an interactive single-token login submission. */
export function validateSubmittedToken(raw: string): TokenSubmitValidation {
  const trimmed = raw.trim();
  if (trimmed.length === 0) {
    return { ok: false, message: "请输入 Token" };
  }
  const credential = serveCredential(trimmed);
  if (credential === null) {
    return {
      ok: false,
      message: "Token 格式无效（需为 1–512 位可见 ASCII 字符，不含空格或换行）",
    };
  }
  return { ok: true, token: credential };
}

/** Map a 401 failure into whether the browser had no token, a bad format, or a rejected one. */
export function unauthorizedReason(hasToken: boolean, problem: string | null = null): AuthPromptReason {
  if (hasToken) return "rejected_token";
  return problem ? "invalid_token" : "missing_token";
}

function isPlainJsonObject(value: unknown): boolean {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/** Probe `/api/overview` with the supplied headers and classify the closed auth outcome. */
export async function probeAuthWithFetch(
  fetcher: AuthFetch,
  headers: Record<string, string>,
  signal?: AbortSignal,
): Promise<AuthProbeResult> {
  try {
    const response = await fetcher("/api/overview", {
      signal,
      cache: "no-store",
      headers,
    });
    if (response.status === 401) {
      await response.arrayBuffer().catch(() => undefined);
      return { kind: "needs_token" };
    }
    if (response.status === 403) {
      await response.arrayBuffer().catch(() => undefined);
      return {
        kind: "forbidden",
        message: FORBIDDEN_HOST_OR_ORIGIN_MESSAGE,
      };
    }
    if (response.status === 304) {
      await response.arrayBuffer().catch(() => undefined);
      return { kind: "authenticated" };
    }
    if (!response.ok) {
      await response.arrayBuffer().catch(() => undefined);
      return {
        kind: "unreachable",
        message: `服务端返回异常状态（HTTP ${response.status}），请稍后重试。`,
      };
    }
    try {
      const payload = await response.json();
      if (!isPlainJsonObject(payload)) {
        return {
          kind: "unreachable",
          message: "服务端返回了非 JSON 概览响应，请检查网络或代理后重试。",
        };
      }
      return { kind: "authenticated" };
    } catch {
      return {
        kind: "unreachable",
        message: "服务端返回了非 JSON 响应（可能是代理页或网关错误），请检查后重试。",
      };
    }
  } catch (cause) {
    if (isAbortError(cause)) throw cause;
    return {
      kind: "unreachable",
      message: cause instanceof Error ? cause.message : "无法连接到服务端",
    };
  }
}

/** Validate syntax, probe `/api/overview` with Bearer auth, and invoke `onVerified` on 200/304 JSON. */
export async function verifyTokenWithFetch(
  raw: string,
  fetcher: AuthFetch,
  onVerified: (token: string) => void,
  signal?: AbortSignal,
): Promise<TokenVerifyResult> {
  const checked = validateSubmittedToken(raw);
  if (!checked.ok) {
    return { ok: false, message: checked.message };
  }
  try {
    const response = await fetcher("/api/overview", {
      signal,
      cache: "no-store",
      headers: {
        Accept: "application/json",
        Authorization: `Bearer ${checked.token}`,
      },
    });
    if (response.status === 401) {
      await response.arrayBuffer().catch(() => undefined);
      return {
        ok: false,
        message: "Token 无效或已过期，请核对 SPAWN_SERVE_TOKEN 后重试。",
      };
    }
    if (response.status === 403) {
      await response.arrayBuffer().catch(() => undefined);
      return {
        ok: false,
        message: FORBIDDEN_HOST_OR_ORIGIN_MESSAGE,
      };
    }
    if (response.status === 304) {
      await response.arrayBuffer().catch(() => undefined);
      onVerified(checked.token);
      return { ok: true };
    }
    if (!response.ok) {
      await response.arrayBuffer().catch(() => undefined);
      return {
        ok: false,
        message: `服务端返回异常状态（HTTP ${response.status}），请稍后重试。`,
      };
    }
    try {
      const payload = await response.json();
      if (!isPlainJsonObject(payload)) {
        return {
          ok: false,
          message: "服务端返回了非 JSON 响应，请确认地址后重试。",
        };
      }
      onVerified(checked.token);
      return { ok: true };
    } catch {
      return {
        ok: false,
        message: "服务端返回了非 JSON 响应（可能是代理页或网关错误），请确认后重试。",
      };
    }
  } catch (cause) {
    if (isAbortError(cause)) throw cause;
    return {
      ok: false,
      message: cause instanceof Error ? `无法连接服务端：${cause.message}` : "无法连接服务端",
    };
  }
}

/** Map a probe result into the closed UI gate state. */
export function authStateFromProbe(
  result: AuthProbeResult,
  hasStoredToken: boolean,
  problem: string | null,
): AuthGateState {
  switch (result.kind) {
    case "authenticated":
      return { kind: "authenticated", hasStoredToken };
    case "needs_token":
      return {
        kind: "needs_token",
        reason: unauthorizedReason(hasStoredToken, problem),
        problem,
        hasStoredToken,
      };
    case "forbidden":
      return {
        kind: "forbidden",
        message: result.message,
        hasStoredToken,
      };
    case "unreachable":
      return {
        kind: "unreachable",
        message: result.message,
        hasStoredToken,
      };
  }
}

/** Transition to the token login gate after an HTTP 401 on an API or SSE response. */
export function authStateOnUnauthorized(
  hasStoredToken: boolean,
  problem: string | null,
): AuthGateState {
  return {
    kind: "needs_token",
    reason: unauthorizedReason(hasStoredToken, problem),
    problem,
    hasStoredToken,
  };
}

/** Classify a cross-tab `storage` event into sign-out, token update, or irrelevant no-op. */
export function classifyStorageTokenEvent(
  key: string | null,
  newValue: string | null,
  currentToken: string | null,
): AuthSyncEvent | null {
  if (key !== null && key !== TOKEN_KEY) return null;
  const next = serveCredential(newValue);
  if (next === null) {
    return currentToken !== null ? { kind: "signed_out" } : null;
  }
  return next !== currentToken ? { kind: "token_updated", token: next } : null;
}
