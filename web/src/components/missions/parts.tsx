/* Small pieces the missions cards and the missions timeline both show. */

import { ArrowUpRight, ChevronRight } from "lucide-react";
import type { ReactNode } from "react";

import type { TaskGradingView } from "../../missions";
import { cx } from "../ui";
import { Badge } from "../ui/badge";

/** The task's agent, linked to its session in the fleet view. */
export function AgentLink({
  sessionId,
  agentId,
}: {
  sessionId: string | null;
  agentId: string | null;
}) {
  const label = agentId ?? sessionId;
  if (!label) return null;
  if (!sessionId) {
    return (
      <span className="truncate font-mono text-caption text-muted-foreground">
        {label}
      </span>
    );
  }
  return (
    <a
      href={`#/fleet/${encodeURIComponent(sessionId)}`}
      title={`Open ${sessionId} in the fleet view`}
      className="inline-flex min-w-0 items-center gap-0.5 truncate font-mono text-caption text-info hover:underline"
    >
      {label}
      <ArrowUpRight size={12} className="shrink-0" />
    </a>
  );
}

/** Badges shown on Missions card task rows: difficulty, preset routing, and rework step-up. */
export function TaskGradingBadges({ grading }: { grading: TaskGradingView }) {
  return (
    <>
      {grading.difficultyBadge && (
        <Badge tone={grading.difficultyTone} title={grading.difficultyTitle}>
          {grading.difficultyBadge}
          {grading.tierRole ? ` · ${grading.tierRole}` : ""}
        </Badge>
      )}
      {grading.presetBadge && (
        <Badge
          tone={grading.rerouted ? "warning" : "neutral"}
          title={grading.presetTitle}
        >
          <span className="font-mono">{grading.presetBadge}</span>
        </Badge>
      )}
      {grading.reworkBadge && (
        <Badge tone="warning" title={grading.reworkTitle}>
          {grading.reworkBadge}
        </Badge>
      )}
    </>
  );
}

/** Compact grading pills for Missions timeline rows and nested earlier attempts. */
export function TaskGradingCompact({
  grading,
  showPreset = false,
}: {
  grading: TaskGradingView;
  showPreset?: boolean;
}) {
  const showReworkMarker =
    grading.compactReworkBadge !== null &&
    (!grading.steppedUp || grading.qualityRejections > 0);
  return (
    <span className="inline-flex shrink-0 items-center gap-1">
      {grading.compactDifficultyBadge && (
        <Badge
          tone={grading.difficultyTone}
          size="sm"
          title={grading.tooltipLine || grading.difficultyTitle}
        >
          {grading.compactDifficultyBadge}
        </Badge>
      )}
      {showPreset && grading.presetId && (
        <Badge
          tone={grading.rerouted ? "warning" : "neutral"}
          size="sm"
          title={grading.presetTitle}
        >
          <span className="max-w-[7rem] truncate font-mono">
            {grading.presetId}
          </span>
        </Badge>
      )}
      {showReworkMarker && (
        <Badge tone="warning" size="sm" title={grading.reworkTitle}>
          {grading.compactReworkBadge}
        </Badge>
      )}
    </span>
  );
}

/** Full task grading, preset, model, and rework trajectory fields inside TaskPanel `<dl>`. */
export function TaskGradingDetails({ grading }: { grading: TaskGradingView }) {
  const initialDifficulty = grading.difficultyLabel ?? grading.routedDifficultyLabel;
  return (
    <>
      {(initialDifficulty || grading.routedDifficultyLabel) && (
        <>
          <dt className="text-muted-foreground">Difficulty</dt>
          <dd className="tnum min-w-0 text-foreground">
            <span className="inline-flex flex-wrap items-center gap-1.5">
              {initialDifficulty && (
                <Badge tone={grading.initialDifficultyTone}>{initialDifficulty}</Badge>
              )}
              {grading.steppedUp && grading.routedDifficultyLabel ? (
                <>
                  <span className="text-faint-foreground">→</span>
                  <Badge tone="warning">{grading.routedDifficultyLabel}</Badge>
                  <span className="text-caption text-warning">stepped up</span>
                </>
              ) : (
                grading.routedDifficultyLabel && (
                  <span className="text-caption text-muted-foreground">
                    routed {grading.routedDifficultyLabel}
                  </span>
                )
              )}
            </span>
          </dd>
        </>
      )}
      {grading.tierRole && (
        <>
          <dt className="text-muted-foreground">Tier role</dt>
          <dd className="tnum min-w-0 text-foreground">
            <span className="font-mono text-caption">{grading.tierRole}</span>
          </dd>
        </>
      )}
      {(grading.presetId || grading.engineModelLabel || grading.executionProfile) && (
        <>
          <dt className="text-muted-foreground">
            {grading.presetId ? "Preset" : "Model"}
          </dt>
          <dd className="tnum min-w-0 text-foreground">
            {grading.presetId && (
              <span className="font-mono text-caption">{grading.presetId}</span>
            )}
            {grading.executionProfile && (
              <span
                className={cx(
                  "text-caption text-muted-foreground",
                  grading.presetId && "ml-1.5",
                )}
              >
                ({grading.executionProfile})
              </span>
            )}
            {grading.engineModelLabel && (
              <span
                className={cx(
                  "font-mono text-caption text-muted-foreground",
                  (grading.presetId || grading.executionProfile) && "ml-1.5",
                )}
              >
                {grading.engineModelLabel}
              </span>
            )}
          </dd>
        </>
      )}
      {grading.presetTrajectory && (
        <>
          <dt className="text-muted-foreground">Reroute</dt>
          <dd className="tnum min-w-0 text-foreground">
            <Badge tone="warning" title={grading.presetTitle}>
              <span className="font-mono">{grading.presetTrajectory}</span>
            </Badge>
          </dd>
        </>
      )}
      <dt className="text-muted-foreground">Quality rej</dt>
      <dd className="tnum min-w-0 text-foreground">
        <span className="inline-flex flex-wrap items-center gap-1.5">
          {grading.qualityRejections > 0 ? (
            <Badge tone="warning" title={grading.reworkTitle}>
              {grading.qualityRejections}
            </Badge>
          ) : (
            <span className="font-mono text-caption text-muted-foreground">0</span>
          )}
          {grading.reworkNote && (
            <span
              className={cx(
                "text-caption",
                grading.steppedUp || grading.rerouted
                  ? "text-warning"
                  : "text-muted-foreground",
              )}
            >
              {grading.reworkNote}
            </span>
          )}
        </span>
      </dd>
    </>
  );
}

export function Collapsible({
  label,
  children,
  defaultOpen = false,
  onToggle,
}: {
  label: string;
  children: ReactNode;
  defaultOpen?: boolean;
  /** Told when the section opens or closes. */
  onToggle?: (open: boolean) => void;
}) {
  return (
    <details
      open={defaultOpen}
      className="group min-w-0"
      onToggle={onToggle ? (event) => onToggle(event.currentTarget.open) : undefined}
    >
      <summary className="flex cursor-pointer list-none items-center gap-1 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground select-none hover:text-muted-foreground [&::-webkit-details-marker]:hidden">
        <ChevronRight
          size={12}
          className="transition-transform group-open:rotate-90"
        />
        {label}
      </summary>
      <div className="mt-1.5 rounded-md border border-surface-border bg-page-canvas/60 px-3 py-2">
        {children}
      </div>
    </details>
  );
}
