import { ListChecks, Sparkles } from "lucide-react";
import type { ReactNode } from "react";

import type { FleetState, Role } from "../../fleet";
import { ROLE_LABEL, judgeKind, percent, sessionState } from "../../fleet";
import type { AgentLivenessView } from "../../ordering";
import { StateBadge, StateDot } from "../StateDot";
import { cx } from "../ui";

/** Who decided a state. Jev (the model) is violet; the rules are neutral. */
export function JudgeBadge({
  judge,
  compact = false,
}: {
  judge: string | null | undefined;
  compact?: boolean;
}) {
  const kind = judgeKind(judge);
  const style = {
    jev: {
      className: "bg-judge-tint text-jev ring-jev/30",
      icon: <Sparkles size={11} />,
      label: "Jev",
      title: "Decided by Jev, the model judge",
    },
    rules: {
      className: "bg-muted text-muted-foreground ring-surface-border",
      icon: <ListChecks size={11} />,
      label: "Rules",
      title: "Settled by the deterministic rules",
    },
    other: {
      className: "bg-transparent text-faint-foreground ring-surface-border",
      icon: null,
      label: judge || "—",
      title: judge ? `Judge: ${judge}` : "No judgement yet",
    },
  }[kind];
  return (
    <span
      title={style.title}
      className={cx(
        "inline-flex h-5 shrink-0 items-center gap-1 rounded-md px-1.5 text-micro font-medium ring-1 ring-inset",
        style.className,
      )}
    >
      {style.icon}
      {!compact || !style.icon ? style.label : null}
    </span>
  );
}

const ROLE_STYLE: Record<Role, string> = {
  supervisor: "bg-foreground/[0.08] text-foreground ring-border-strong",
  worker: "bg-muted text-muted-foreground ring-surface-border",
  rsi: "bg-chart-line/[0.06] text-muted-foreground ring-surface-border",
};

export function RoleBadge({ role }: { role: Role }) {
  return (
    <span
      className={cx(
        "inline-flex h-[18px] shrink-0 items-center rounded px-1.5 text-micro font-medium uppercase tracking-[0.06em] ring-1 ring-inset",
        ROLE_STYLE[role],
      )}
    >
      {ROLE_LABEL[role]}
    </span>
  );
}

export function FleetStateBadge({
  state,
  size,
  pulse,
}: {
  state: FleetState;
  size?: "sm" | "md";
  pulse?: boolean;
}) {
  return <StateBadge state={sessionState(state)} size={size} pulse={pulse} />;
}

export function FleetStateDot({
  state,
  size,
  pulse,
}: {
  state: FleetState;
  size?: "sm" | "md";
  pulse?: boolean;
}) {
  return <StateDot state={sessionState(state)} size={size} pulse={pulse} />;
}

const LIVENESS_TONE: Record<AgentLivenessView["tone"], { badge: string; dot: string }> = {
  success: {
    badge: "bg-ok-tint text-success ring-success/25",
    dot: "bg-success",
  },
  info: {
    badge: "bg-info-tint text-info ring-info/25",
    dot: "bg-info",
  },
  warning: {
    badge: "bg-wait-tint text-warning ring-warning/25",
    dot: "bg-warning",
  },
  muted: {
    badge: "bg-muted text-muted-foreground ring-surface-border",
    dot: "bg-muted-foreground",
  },
};

/** Compact liveness status pill distinguishing real progress (`实时推进`)
 * from quiet work (`静默运行`) and silent stalls (`疑似停滞`). */
export function AgentLivenessPill({
  liveness,
  title,
}: {
  liveness: AgentLivenessView;
  title?: string;
}) {
  const tone = LIVENESS_TONE[liveness.tone];
  return (
    <span
      title={title}
      className={cx(
        "inline-flex h-5 shrink-0 items-center gap-1 rounded px-1.5 text-micro font-medium ring-1 ring-inset",
        tone.badge,
      )}
    >
      <span
        aria-hidden="true"
        className={cx("size-1.5 rounded-full", tone.dot, liveness.pulse && "animate-spawn-pulse")}
      />
      {liveness.label}
    </span>
  );
}

/**
 * A 0..1 risk score as a short bar. It turns amber past one half and red
 * past four fifths, because the score only matters when it is high.
 */
export function Meter({
  label,
  value,
  className,
}: {
  label: ReactNode;
  value: number | null;
  className?: string;
}) {
  const fraction = value == null ? 0 : Math.max(0, Math.min(1, value));
  const tone =
    value == null
      ? "bg-faint-foreground"
      : fraction >= 0.8
        ? "bg-destructive"
        : fraction >= 0.5
          ? "bg-warning"
          : "bg-muted-foreground";
  return (
    <div className={cx("min-w-0", className)}>
      <div className="flex items-baseline justify-between gap-2 text-caption">
        <span className="text-muted-foreground">{label}</span>
        <span className="tnum font-medium text-foreground">
          {percent(value)}
        </span>
      </div>
      <div className="mt-1 h-1 overflow-hidden rounded-full bg-muted">
        <div
          className={cx("h-full rounded-full", tone)}
          style={{ width: `${fraction * 100}%` }}
        />
      </div>
    </div>
  );
}

/** Small label/value pair for dense fact rows. */
export function Fact({
  label,
  children,
  mono = false,
  className,
}: {
  label: ReactNode;
  children: ReactNode;
  mono?: boolean;
  className?: string;
}) {
  return (
    <div className={cx("min-w-0", className)}>
      <div className="text-micro text-faint-foreground">{label}</div>
      <div
        className={cx(
          "truncate text-caption text-foreground",
          mono && "font-mono",
        )}
      >
        {children}
      </div>
    </div>
  );
}

/** Uppercase section label used inside cards. */
export function SectionLabel({
  children,
  trailing,
}: {
  children: ReactNode;
  trailing?: ReactNode;
}) {
  return (
    <div className="flex items-center gap-2 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
      {children}
      {trailing && (
        <span className="ml-auto normal-case tracking-normal">{trailing}</span>
      )}
    </div>
  );
}
