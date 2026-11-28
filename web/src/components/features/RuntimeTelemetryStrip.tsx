import { Activity, Coins, Cpu } from "lucide-react";
import { useEffect, useState } from "react";

import { readOptionalTelemetrySliceByKey } from "../../featuresApi";
import { compactCount } from "../../format";
import {
  INITIAL_RUNTIME_TELEMETRY,
  TELEMETRY_POLL_MS,
  TELEMETRY_SLICE_KEYS,
  applyTelemetrySliceUpdate,
  formatCostAmountText,
  type RuntimeTelemetryState,
  type TelemetryAgentsSummary,
  type TelemetryCostWindow,
  type TelemetryModelsSummary,
  type TelemetrySlice,
  type TelemetrySliceKey,
} from "../../runtimeTelemetry";
import { Card, cx } from "../ui";

/** Non-blocking runtime status strip for `#/features`, reading existing
 * read-only endpoints (`/api/fleet`, `/api/grading`, `/api/cost?window=24h|7d`).
 * Each slice settles and updates independently with per-request timeout so one
 * slow endpoint cannot starve siblings or abort healthy reads. */
export function RuntimeTelemetryStrip() {
  const [telemetry, setTelemetry] = useState<RuntimeTelemetryState>(INITIAL_RUNTIME_TELEMETRY);

  useEffect(() => {
    const unmountController = new AbortController();
    const inFlight = new Set<TelemetrySliceKey>();

    const refresh = () => {
      if (typeof document !== "undefined" && document.visibilityState === "hidden") return;
      for (const key of TELEMETRY_SLICE_KEYS) {
        if (inFlight.has(key)) continue;
        inFlight.add(key);
        void readOptionalTelemetrySliceByKey(key, unmountController.signal)
          .then((update) => {
            if (!unmountController.signal.aborted) {
              setTelemetry((prev) => applyTelemetrySliceUpdate(prev, update));
            }
          })
          .catch(() => {
            // Unmount abort is expected; HTTP/timeout failures resolve as "unavailable".
          })
          .finally(() => {
            inFlight.delete(key);
          });
      }
    };

    refresh();
    const timer = window.setInterval(refresh, TELEMETRY_POLL_MS);
    const onVisibility = () => {
      if (document.visibilityState === "visible") refresh();
    };
    document.addEventListener("visibilitychange", onVisibility);

    return () => {
      unmountController.abort();
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, []);

  if (
    telemetry.fleet.status === "unavailable" &&
    telemetry.grading.status === "unavailable" &&
    telemetry.cost24h.status === "unavailable" &&
    telemetry.cost7d.status === "unavailable"
  ) {
    return null;
  }

  return (
    <Card className="shrink-0 border border-surface-border px-3 py-2">
      <div
        aria-label="Runtime telemetry"
        className="flex flex-wrap items-center gap-x-4 gap-y-1.5 text-caption"
      >
        <AgentsCell slice={telemetry.fleet} />
        <Divider />
        <ModelsCell slice={telemetry.grading} />
        <Divider />
        <CostCell label="24h" slice={telemetry.cost24h} />
        <CostCell label="7d" slice={telemetry.cost7d} />
      </div>
    </Card>
  );
}

function Divider() {
  return <span aria-hidden="true" className="hidden h-3.5 w-px bg-surface-border sm:inline-block" />;
}

function AgentsCell({ slice }: { slice: TelemetrySlice<TelemetryAgentsSummary> }) {
  return (
    <div className="flex flex-wrap items-center gap-1.5">
      <span className="inline-flex items-center gap-1 text-faint-foreground">
        <Activity size={12} />
        <span>Agents</span>
      </span>
      {slice.status === "loading" ? (
        <span className="text-muted-foreground">…</span>
      ) : slice.status === "unavailable" ? (
        <span className="text-muted-foreground">—</span>
      ) : (
        <>
          <span
            className={cx(
              "tnum rounded-chip px-1.5 py-px text-micro font-medium",
              slice.data.live > 0 ? "bg-ok-tint text-success" : "bg-muted text-muted-foreground",
            )}
            title={`${slice.data.live} live of ${slice.data.total} tracked agent sessions (${slice.data.working} working)`}
          >
            {slice.data.live} live
          </span>
          <span
            className={cx(
              "tnum rounded-chip px-1.5 py-px text-micro font-medium",
              slice.data.waiting > 0
                ? "bg-wait-tint text-warning"
                : "bg-muted text-muted-foreground",
            )}
          >
            {slice.data.waiting} waiting
          </span>
          {slice.data.blocked > 0 && (
            <span className="tnum rounded-chip bg-fail-tint px-1.5 py-px text-micro font-medium text-destructive">
              {slice.data.blocked} blocked
            </span>
          )}
        </>
      )}
    </div>
  );
}

function ModelsCell({ slice }: { slice: TelemetrySlice<TelemetryModelsSummary> }) {
  return (
    <div className="flex flex-wrap items-center gap-1.5">
      <span className="inline-flex items-center gap-1 text-faint-foreground">
        <Cpu size={12} />
        <span>Routing</span>
      </span>
      {slice.status === "loading" ? (
        <span className="text-muted-foreground">…</span>
      ) : slice.status === "unavailable" ? (
        <span className="text-muted-foreground">—</span>
      ) : (
        <div className="flex flex-wrap items-center gap-1">
          {slice.data.tiers.map((tier) => {
            const title =
              tier.activeModels.length > 0
                ? `${tier.label}: ${tier.activeCount}/${tier.totalCount} active (${tier.activeModels.join(", ")})`
                : `${tier.label}: ${tier.activeCount}/${tier.totalCount} active`;
            return (
              <span
                key={tier.label}
                title={title}
                className={cx(
                  "tnum inline-flex items-center gap-1 rounded-chip px-1.5 py-px text-micro",
                  tier.activeCount > 0
                    ? "bg-surface-selected font-medium text-foreground"
                    : "bg-muted text-muted-foreground",
                )}
              >
                <span>
                  {tier.label} {tier.activeCount}
                </span>
                {tier.activeModels.length === 1 && (
                  <span className="hidden max-w-[110px] truncate text-muted-foreground sm:inline">
                    {tier.activeModels[0]}
                  </span>
                )}
              </span>
            );
          })}
        </div>
      )}
    </div>
  );
}

function costTitle(win: TelemetryCostWindow): string {
  const header = `${win.window}: ${win.totalTokens.toLocaleString("en-US")} tokens · ${formatCostAmountText(win.amount)}`;
  if (win.models.length === 0) return header;
  const perModel = win.models
    .slice(0, 6)
    .map((m) => `${m.model}: ${compactCount(m.totalTokens)} tok · ${formatCostAmountText(m.amount)}`);
  if (win.models.length > 6) perModel.push(`+${win.models.length - 6} more`);
  return `${header}\n${perModel.join("\n")}`;
}

function CostCell({
  label,
  slice,
}: {
  label: "24h" | "7d";
  slice: TelemetrySlice<TelemetryCostWindow>;
}) {
  return (
    <div className="flex flex-wrap items-center gap-1.5">
      <span className="inline-flex items-center gap-1 text-faint-foreground">
        {label === "24h" && <Coins size={12} />}
        <span>{label}</span>
      </span>
      {slice.status === "loading" ? (
        <span className="text-muted-foreground">…</span>
      ) : slice.status === "unavailable" ? (
        <span className="text-muted-foreground">—</span>
      ) : (
        <span className="tnum inline-flex items-center gap-1" title={costTitle(slice.data)}>
          <span className="text-muted-foreground">{compactCount(slice.data.totalTokens)} tok</span>
          <span className="text-faint-foreground">·</span>
          {slice.data.amount.kind === "unpriced" ? (
            <span className="rounded-chip bg-wait-tint px-1.5 py-px text-micro font-medium text-warning">
              unpriced
            </span>
          ) : slice.data.amount.kind === "mixed" ? (
            <span className="inline-flex items-center gap-1">
              <span className="font-medium text-foreground">{slice.data.amount.text}</span>
              <span className="rounded-chip bg-wait-tint px-1.5 py-px text-micro text-warning">
                {slice.data.amount.unpricedShort}
              </span>
            </span>
          ) : (
            <span className="font-medium text-foreground">{slice.data.amount.text}</span>
          )}
        </span>
      )}
    </div>
  );
}
