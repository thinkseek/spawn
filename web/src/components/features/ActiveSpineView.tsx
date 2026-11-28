import {
  ArrowLeft,
  ArrowRight,
  ArrowUpRight,
  GitBranch,
  Network,
  OctagonAlert,
  RotateCcw,
} from "lucide-react";

import type { FeatureMission } from "../../features";
import { STATE_INFO, objectiveName } from "../../features";
import type {
  ActiveSpineModel,
  ActiveSpineNode,
  DependencyEdgeKind,
  SpineDependencyNode,
  SpineWorkSignal,
} from "../../featuresGraph";
import { Card, FOCUS_RING, cx } from "../ui";

export interface ActiveSpineViewProps {
  mission: FeatureMission;
  model: ActiveSpineModel;
  selectedNodeId: string | null;
  onSelectFeature: (featureId: string) => void;
  onResetFocus: () => void;
  onOpenSession: (sessionId: string) => void;
  onInspectInMindMap: (featureId: string) => void;
}

const MODE_META: Record<
  ActiveSpineModel["kind"],
  { label: string; badgeClass: string; subtitle: string }
> = {
  empty: {
    label: "Empty",
    badgeClass: "bg-muted text-muted-foreground",
    subtitle: "No capabilities defined in this mission.",
  },
  focused: {
    label: "Focused",
    badgeClass: "bg-info-tint text-info",
    subtitle: "Centered on selected capability with upstream prerequisites and downstream unlocks.",
  },
  active_work: {
    label: "Active Spine",
    badgeClass: "bg-info-tint text-info",
    subtitle: "Server-derived non-live / in-flight capabilities ordered by urgency and fan-out.",
  },
  all_live: {
    label: "All Live",
    badgeClass: "bg-ok-tint text-success",
    subtitle: "Every capability in this mission is live; click any item to trace dependencies.",
  },
};

function workSignalView(signal: SpineWorkSignal): { text: string; toneClass: string } {
  switch (signal.kind) {
    case "blocked":
      return {
        text: `blocked (${signal.blockerKind})${signal.reason ? `: ${signal.reason}` : ""}`,
        toneClass: "bg-fail-tint text-destructive",
      };
    case "active_owner":
      return {
        text: `${signal.agentId ?? "owner"}${signal.taskId !== null ? ` · task #${signal.taskId}` : ""} (${signal.taskState})`,
        toneClass: "bg-info-tint text-info",
      };
    case "active_tasks":
      return {
        text: `${signal.activeTaskCount} active task${signal.activeTaskCount === 1 ? "" : "s"}${signal.taskId !== null ? ` · #${signal.taskId}` : ""}${signal.agentId ? ` (${signal.agentId})` : ""}`,
        toneClass: "bg-info-tint text-info",
      };
    case "recent_owner":
      return {
        text: `last: ${signal.agentId ?? "agent"}${signal.taskId !== null ? ` · #${signal.taskId}` : ""} (${signal.taskState})`,
        toneClass: "bg-muted text-muted-foreground",
      };
    case "unowned":
      return {
        text: "unowned · no task in flight",
        toneClass: "bg-wait-tint text-warning",
      };
    case "idle":
      return {
        text: "idle",
        toneClass: "bg-muted text-muted-foreground",
      };
  }
}

function edgeLabel(kind: DependencyEdgeKind): string {
  switch (kind) {
    case "depends_on":
      return "depends on";
    case "blocked_by":
      return "blocks spine";
    case "blocks":
      return "blocked by spine";
    case "required_by":
      return "unlocks";
  }
}

/** Tri-column Active-Spine workspace: Upstream prerequisites (left) ->
 * Highlighted Active Spine (center) -> Downstream unlocked (right). */
export function ActiveSpineView({
  mission,
  model,
  selectedNodeId,
  onSelectFeature,
  onResetFocus,
  onOpenSession,
  onInspectInMindMap,
}: ActiveSpineViewProps) {
  const meta = MODE_META[model.kind];

  return (
    <div className="flex flex-col gap-3 xl:min-h-0 xl:flex-1">
      <Card className="shrink-0 border border-surface-border px-3.5 py-2">
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
          <div className="flex min-w-0 items-center gap-2">
            <GitBranch size={14} className="shrink-0 text-info" />
            <span
              className={cx(
                "rounded-chip px-2 py-px text-micro font-semibold",
                meta.badgeClass,
              )}
            >
              {meta.label}
            </span>
            <p className="truncate text-caption text-muted-foreground">{meta.subtitle}</p>
          </div>
          <div className="ml-auto flex flex-wrap items-center gap-2 text-micro">
            <span className="tnum rounded-chip bg-muted px-2 py-px text-muted-foreground">
              {model.summary.upstreamCount} upstream · {model.summary.spineCount} spine ·{" "}
              {model.summary.downstreamCount} downstream
            </span>
            {model.summary.cycleGuardedEdges > 0 && (
              <span
                title="Graph traversal pruned cyclic back-edges while preserving reachable dependencies"
                className="tnum inline-flex items-center gap-1 rounded-chip bg-wait-tint px-2 py-px text-warning"
              >
                <OctagonAlert size={11} />
                {model.summary.cycleGuardedEdges} cycle guarded
              </span>
            )}
            {model.kind === "focused" && (
              <button
                type="button"
                onClick={onResetFocus}
                className={cx(
                  "inline-flex items-center gap-1 rounded-chip bg-surface-selected px-2 py-0.5 text-micro font-medium text-foreground hover:bg-surface-hover",
                  FOCUS_RING,
                )}
              >
                <RotateCcw size={11} />
                Reset focus
              </button>
            )}
          </div>
        </div>
      </Card>

      <div className="grid grid-cols-1 gap-3 xl:min-h-0 xl:flex-1 xl:grid-cols-3 xl:grid-rows-[minmax(0,1fr)]">
        {/* Left Column: Upstream */}
        <ColumnShell
          ariaLabel="前依赖 (Upstream)"
          icon={<ArrowLeft size={14} className="text-info" />}
          title="前依赖 · Upstream"
          count={model.upstream.length}
          meta={`${model.summary.directUpstreamCount} direct`}
          emptyMessage="No upstream dependencies for the active spine."
          highlighted={false}
        >
          {model.upstream.map((item) => (
            <DependencyCard
              key={item.feature.id}
              missionId={mission.id}
              item={item}
              selected={selectedNodeId === item.feature.id}
              onSelect={() => onSelectFeature(item.feature.id)}
              onOpenSession={onOpenSession}
            />
          ))}
        </ColumnShell>

        {/* Center Column: Active Spine */}
        <ColumnShell
          ariaLabel="当前主心骨 (Active Spine)"
          icon={<GitBranch size={14} className="text-info" />}
          title="当前主心骨 · Active Spine"
          count={model.spine.length}
          meta={`${model.summary.liveCount}/${model.summary.totalFeatures} live`}
          emptyMessage="No active spine capabilities in this mission."
          highlighted
        >
          {model.spine.map((item) => (
            <SpineCard
              key={item.feature.id}
              missionId={mission.id}
              item={item}
              selected={selectedNodeId === item.feature.id}
              onSelect={() => onSelectFeature(item.feature.id)}
              onOpenSession={onOpenSession}
              onInspectInMindMap={onInspectInMindMap}
            />
          ))}
        </ColumnShell>

        {/* Right Column: Downstream Unlocked */}
        <ColumnShell
          ariaLabel="后依赖 (Downstream Unlocked)"
          icon={<ArrowRight size={14} className="text-success" />}
          title="后依赖 · Downstream Unlocked"
          count={model.downstream.length}
          meta={`${model.summary.directDownstreamCount} direct`}
          emptyMessage="No downstream capabilities depend on or await the active spine."
          highlighted={false}
        >
          {model.downstream.map((item) => (
            <DependencyCard
              key={item.feature.id}
              missionId={mission.id}
              item={item}
              selected={selectedNodeId === item.feature.id}
              onSelect={() => onSelectFeature(item.feature.id)}
              onOpenSession={onOpenSession}
            />
          ))}
        </ColumnShell>
      </div>
    </div>
  );
}

function ColumnShell({
  ariaLabel,
  icon,
  title,
  count,
  meta,
  emptyMessage,
  highlighted,
  children,
}: {
  ariaLabel: string;
  icon: React.ReactNode;
  title: string;
  count: number;
  meta: string;
  emptyMessage: string;
  highlighted: boolean;
  children: React.ReactNode;
}) {
  return (
    <section
      aria-label={ariaLabel}
      className={cx(
        "flex min-h-[240px] min-w-0 flex-col rounded-card bg-surface shadow-card xl:min-h-0 xl:overflow-hidden",
        highlighted ? "border-2 border-info/40" : "border border-surface-border",
      )}
    >
      <header
        className={cx(
          "flex shrink-0 items-center gap-2 border-b border-surface-border px-3.5 py-2.5",
          highlighted ? "bg-surface-selected" : "bg-surface",
        )}
      >
        {icon}
        <h2 className="min-w-0 truncate text-label font-semibold text-foreground">{title}</h2>
        <span className="tnum ml-auto shrink-0 text-micro text-muted-foreground">{meta}</span>
        <span
          className={cx(
            "tnum shrink-0 rounded-chip px-1.5 py-px text-micro font-medium",
            highlighted ? "bg-info-tint text-info" : "bg-muted text-muted-foreground",
          )}
        >
          {count}
        </span>
      </header>
      <div className="flex flex-col gap-2 p-2.5 xl:min-h-0 xl:flex-1 xl:overflow-y-auto">
        {count === 0 ? (
          <p className="rounded-control border border-dashed border-surface-border px-3 py-5 text-center text-caption text-faint-foreground">
            {emptyMessage}
          </p>
        ) : (
          children
        )}
      </div>
    </section>
  );
}

function SpineCard({
  missionId,
  item,
  selected,
  onSelect,
  onOpenSession,
  onInspectInMindMap,
}: {
  missionId: number;
  item: ActiveSpineNode;
  selected: boolean;
  onSelect: () => void;
  onOpenSession: (sessionId: string) => void;
  onInspectInMindMap: (featureId: string) => void;
}) {
  const { feature } = item;
  const stateInfo = STATE_INFO[feature.state];
  const signal = workSignalView(item.workSignal);

  return (
    <article
      className={cx(
        "flex w-full flex-col gap-1.5 rounded-control border p-3 transition-colors",
        selected
          ? "border-info bg-surface-selected"
          : "border-surface-border bg-surface",
      )}
    >
      <button
        type="button"
        onClick={onSelect}
        aria-pressed={selected}
        className={cx(
          "flex w-full flex-col gap-1.5 rounded-control text-left transition-colors hover:bg-surface-hover",
          FOCUS_RING,
        )}
      >
        <div className="flex w-full flex-wrap items-center gap-1.5">
          <span
            className={cx(
              "inline-flex items-center gap-1 rounded-chip px-1.5 py-px text-micro font-medium ring-1 ring-inset",
              stateInfo.badge,
            )}
          >
            <span className={cx("size-1.5 rounded-full", stateInfo.dot)} />
            {stateInfo.label}
          </span>
          <span className="rounded-chip bg-info-tint px-1.5 py-px text-micro font-medium text-info">
            {item.role === "focused"
              ? "focused"
              : item.role === "in_flight"
                ? "in flight"
                : item.role === "live_reference"
                  ? "live"
                  : "planned"}
          </span>
          {feature.objectives.length > 0 && (
            <span className="tnum font-mono text-micro text-faint-foreground">
              {objectiveName(missionId, feature.objectives[0])}
            </span>
          )}
          <span className="tnum ml-auto text-micro text-faint-foreground">
            ← {item.directUpstreamCount} · {item.directDownstreamCount} →
          </span>
        </div>

        <div className="w-full">
          <h3 className="text-label font-semibold text-foreground">{feature.title}</h3>
          <p className="truncate font-mono text-micro text-faint-foreground">{feature.id}</p>
        </div>

        {feature.summary && (
          <p className="line-clamp-2 w-full text-caption text-muted-foreground">{feature.summary}</p>
        )}
      </button>

      <div className="flex flex-wrap items-center gap-1.5 pt-0.5">
        <span className={cx("max-w-full truncate rounded-chip px-1.5 py-px text-micro", signal.toneClass)}>
          {signal.text}
        </span>
        <div className="ml-auto flex shrink-0 items-center gap-1.5">
          {item.sessionId && (
            <button
              type="button"
              onClick={() => {
                if (item.sessionId) onOpenSession(item.sessionId);
              }}
              className={cx(
                "inline-flex items-center gap-0.5 rounded-chip bg-surface-selected px-1.5 py-px text-micro text-info hover:underline",
                FOCUS_RING,
              )}
            >
              session <ArrowUpRight size={10} />
            </button>
          )}
          <button
            type="button"
            onClick={() => onInspectInMindMap(feature.id)}
            className={cx(
              "inline-flex items-center gap-0.5 rounded-chip px-1.5 py-px text-micro text-muted-foreground hover:bg-surface-hover hover:text-foreground",
              FOCUS_RING,
            )}
          >
            <Network size={10} /> map
          </button>
        </div>
      </div>
    </article>
  );
}

function DependencyCard({
  missionId,
  item,
  selected,
  onSelect,
  onOpenSession,
}: {
  missionId: number;
  item: SpineDependencyNode;
  selected: boolean;
  onSelect: () => void;
  onOpenSession: (sessionId: string) => void;
}) {
  const { feature } = item;
  const stateInfo = STATE_INFO[feature.state];
  const signal = workSignalView(item.workSignal);

  return (
    <article
      className={cx(
        "flex w-full flex-col gap-1.5 rounded-control border p-2.5 transition-colors",
        selected
          ? "border-info bg-surface-selected"
          : "border-surface-border bg-surface",
      )}
    >
      <button
        type="button"
        onClick={onSelect}
        aria-pressed={selected}
        className={cx(
          "flex w-full flex-col gap-1.5 rounded-control text-left transition-colors hover:bg-surface-hover",
          FOCUS_RING,
        )}
      >
        <div className="flex w-full flex-wrap items-center gap-1.5">
          <span
            className={cx(
              "inline-flex items-center gap-1 rounded-chip px-1.5 py-px text-micro font-medium ring-1 ring-inset",
              stateInfo.badge,
            )}
          >
            <span className={cx("size-1.5 rounded-full", stateInfo.dot)} />
            {stateInfo.label}
          </span>
          <span
            className={cx(
              "tnum rounded-chip px-1.5 py-px text-micro",
              item.proximity === "direct"
                ? "bg-surface-selected font-medium text-foreground"
                : "bg-muted text-muted-foreground",
            )}
          >
            {item.proximity === "direct" ? "direct" : `hop ${item.depth}`}
          </span>
          {feature.objectives.length > 0 && (
            <span className="tnum ml-auto font-mono text-micro text-faint-foreground">
              {objectiveName(missionId, feature.objectives[0])}
            </span>
          )}
        </div>

        <div className="w-full">
          <h3 className="text-label font-medium text-foreground">{feature.title}</h3>
          <p className="truncate font-mono text-micro text-faint-foreground">{feature.id}</p>
        </div>

        {item.links.length > 0 && (
          <div className="flex w-full flex-wrap gap-1">
            {item.links.slice(0, 3).map((link) => (
              <span
                key={`${link.viaFeatureId}:${link.edgeKind}`}
                title={link.label ?? undefined}
                className="truncate rounded-chip bg-muted px-1.5 py-px font-mono text-micro text-muted-foreground"
              >
                {edgeLabel(link.edgeKind)} · {link.viaFeatureId}
              </span>
            ))}
          </div>
        )}
      </button>

      <div className="flex flex-wrap items-center gap-1.5">
        <span className={cx("max-w-full truncate rounded-chip px-1.5 py-px text-micro", signal.toneClass)}>
          {signal.text}
        </span>
        {item.sessionId && (
          <button
            type="button"
            onClick={() => {
              if (item.sessionId) onOpenSession(item.sessionId);
            }}
            className={cx(
              "ml-auto inline-flex shrink-0 items-center gap-0.5 rounded-chip px-1 text-micro text-info hover:underline",
              FOCUS_RING,
            )}
          >
            session <ArrowUpRight size={10} />
          </button>
        )}
      </div>
    </article>
  );
}
