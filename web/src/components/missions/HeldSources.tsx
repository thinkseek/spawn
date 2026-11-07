import { CirclePause } from "lucide-react";

import type { SourceHold } from "../../attention";
import { absoluteTitle, formatDuration, relativeTime } from "../../format";

/**
 * The owner's holds on mission sources (`spawn mission hold`): while one
 * lasts no task lands on its source; the source's gates still run and its
 * accepted tasks wait behind it. Each ends by a record: the owner's release,
 * or its expiry at `expires_ms`, which the integrator records (and the
 * supervisor is told of). Shown on the Missions page (every hold) and in
 * the attention view (those with a task waiting). Not an escalation.
 */
export function HeldSources({
  holds,
  title,
  hint,
}: {
  holds: SourceHold[];
  title: string;
  hint: string;
}) {
  if (holds.length === 0) return null;
  return (
    <section
      aria-label={title}
      className="shrink-0 rounded-lg border border-border px-4 py-2"
    >
      <header className="flex flex-wrap items-center gap-x-2 gap-y-1">
        <CirclePause size={14} className="shrink-0 text-muted-foreground" />
        <h2 className="text-label font-medium text-foreground">{title}</h2>
        <span className="tnum text-micro text-muted-foreground">{holds.length}</span>
        <span className="text-micro text-faint-foreground">{hint}</span>
      </header>
      <ul className="mt-1 divide-y divide-border">
        {holds.map((hold) => (
          <HoldRow key={`${hold.source}-${hold.hold}`} hold={hold} />
        ))}
      </ul>
    </section>
  );
}

function HoldRow({ hold }: { hold: SourceHold }) {
  const left =
    hold.expires_ms === null ? null : Math.max(0, hold.expires_ms - Date.now());
  return (
    <li className="flex flex-wrap items-baseline gap-x-3 gap-y-0.5 py-1.5 text-caption">
      <span className="font-mono text-foreground" title={hold.source}>
        {hold.source}
      </span>
      <span className="tnum text-muted-foreground">hold #{hold.hold}</span>
      {hold.reason && <span className="text-foreground/85">{hold.reason}</span>}
      {hold.main && (
        <span className="text-muted-foreground">
          at main{" "}
          <span className="font-mono" title={hold.main}>
            {hold.main.slice(0, 10)}
          </span>
        </span>
      )}
      <span className="text-muted-foreground" title={absoluteTitle(hold.held_at_ms)}>
        taken {relativeTime(hold.held_at_ms)}
        {hold.by ? ` by ${hold.by}` : ""}
      </span>
      {left !== null && (
        <span className="tnum text-muted-foreground" title={absoluteTitle(hold.expires_ms)}>
          {left > 0
            ? `expires in ${formatDuration(left)}`
            : "past its bound: the integrator ends it at its next pass"}
        </span>
      )}
      <span className="tnum text-muted-foreground">
        {hold.waiting.length === 0
          ? "no task waiting"
          : `waiting: ${hold.waiting.map((task) => `#${task}`).join(", ")}`}
      </span>
    </li>
  );
}
