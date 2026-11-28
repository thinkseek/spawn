// Browser HTTP client wrapper for global Agent Runtime endpoints (Wave-AR AR-3).
// Connects pure fetch helpers in `./agentRuntime.ts` with the dashboard's
// shared Bearer token headers, ETag cache, and 401 auth gate signaling.

import { apiHeaders, reportUnauthorizedStatus, subscribeAuthSync } from "./api";
import {
  fetchAgentsWithFetch,
  fetchAgentSnapshotWithFetch,
  postAgentActionWithFetch,
  postSpawnTerminalWithFetch,
  type AgentEtagEntry,
  type AgentFetch,
  type AgentResult,
  type AgentsListResponse,
  type SpawnTerminalRequest,
  type SpawnTerminalResponse,
  type TuiAction,
  type TuiActionOutcome,
  type TuiSnapshot,
} from "./agentRuntime.ts";

const listEtagCache = new Map<string, AgentEtagEntry<AgentsListResponse>>();
const snapshotEtagCache = new Map<string, AgentEtagEntry<TuiSnapshot>>();

subscribeAuthSync(() => {
  listEtagCache.clear();
  snapshotEtagCache.clear();
});

const browserFetch: AgentFetch = (url, init) => fetch(url, init);
const onUnauthorized = () => reportUnauthorizedStatus(401);

export function clearAgentRuntimeCaches(): void {
  listEtagCache.clear();
  snapshotEtagCache.clear();
}

export function fetchGlobalAgents(
  includeAll: boolean,
  signal: AbortSignal,
): Promise<AgentResult<AgentsListResponse>> {
  return fetchAgentsWithFetch(browserFetch, apiHeaders(), includeAll, {
    signal,
    etagCache: listEtagCache,
    onUnauthorized,
  });
}

export function fetchGlobalAgentSnapshot(
  address: string,
  signal: AbortSignal,
  bypassEtag = false,
): Promise<AgentResult<TuiSnapshot>> {
  return fetchAgentSnapshotWithFetch(browserFetch, apiHeaders(), address, {
    signal,
    etagCache: snapshotEtagCache,
    bypassEtag,
    onUnauthorized,
  });
}

export function postGlobalAgentAction(
  address: string,
  action: TuiAction,
  signal?: AbortSignal,
): Promise<AgentResult<TuiActionOutcome>> {
  return postAgentActionWithFetch(browserFetch, apiHeaders(), address, action, {
    signal,
    onUnauthorized,
  });
}

export function postGlobalAgentSpawn(
  req: SpawnTerminalRequest,
  signal?: AbortSignal,
): Promise<AgentResult<SpawnTerminalResponse>> {
  return postSpawnTerminalWithFetch(browserFetch, apiHeaders(), req, {
    signal,
    onUnauthorized,
  });
}
