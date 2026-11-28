// Browser HTTP client wrapper for global Agent Runtime endpoints (Wave-AR AR-3).
// Connects pure fetch helpers in `./agentRuntime.ts` with the dashboard's
// shared Bearer token headers, ETag cache, and 401 auth gate signaling.

import { apiHeaders, reportUnauthorizedStatus, subscribeAuthSync } from "./api";
import {
  agentEventsUrl,
  fetchAgentsWithFetch,
  fetchAgentEventsWithFetch,
  fetchAgentSnapshotWithFetch,
  postAgentActionWithFetch,
  validateTerminalAddress,
  type AgentEtagEntry,
  type AgentFetch,
  type AgentResult,
  type AgentsListResponse,
  type FusedEventsReport,
  type TuiAction,
  type TuiActionOutcome,
  type TuiSnapshot,
} from "./agentRuntime.ts";

const listEtagCache = new Map<string, AgentEtagEntry<AgentsListResponse>>();
const snapshotEtagCache = new Map<string, AgentEtagEntry<TuiSnapshot>>();
const eventsEtagCache = new Map<string, AgentEtagEntry<FusedEventsReport>>();
const MAX_EVENTS_ETAG_ENTRIES = 32;

subscribeAuthSync(() => {
  listEtagCache.clear();
  snapshotEtagCache.clear();
  eventsEtagCache.clear();
});

const browserFetch: AgentFetch = (url, init) => fetch(url, init);
const onUnauthorized = () => reportUnauthorizedStatus(401);

export function clearAgentRuntimeCaches(): void {
  listEtagCache.clear();
  snapshotEtagCache.clear();
  eventsEtagCache.clear();
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

export function fetchGlobalAgentEvents(
  address: string,
  options: {
    limit?: number;
    afterKey?: string | null;
    bypassEtag?: boolean;
    signal: AbortSignal;
  },
): Promise<AgentResult<FusedEventsReport>> {
  if (validateTerminalAddress(address)) {
    try {
      const targetUrl = agentEventsUrl(address, {
        limit: options.limit,
        afterKey: options.afterKey,
      });
      const prefix = `/api/agents/${encodeURIComponent(address)}/events`;
      for (const cachedKey of Array.from(eventsEtagCache.keys())) {
        if (
          (cachedKey === prefix || cachedKey.startsWith(`${prefix}?`)) &&
          cachedKey !== targetUrl
        ) {
          eventsEtagCache.delete(cachedKey);
        }
      }
      while (eventsEtagCache.size >= MAX_EVENTS_ETAG_ENTRIES) {
        const oldest = eventsEtagCache.keys().next().value;
        if (oldest === undefined) break;
        eventsEtagCache.delete(oldest);
      }
    } catch {
      // Let fetchAgentEventsWithFetch return a structured bad_request error.
    }
  }
  return fetchAgentEventsWithFetch(browserFetch, apiHeaders(), address, {
    limit: options.limit,
    afterKey: options.afterKey,
    bypassEtag: options.bypassEtag,
    signal: options.signal,
    etagCache: eventsEtagCache,
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
