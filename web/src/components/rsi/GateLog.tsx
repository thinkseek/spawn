import { cx } from "../ui";
import { formatNumber } from "./Phase";
import { formatPercent, percentChange } from "./metric";

export interface Verdict {
  accepted: boolean;
  reason: string | null;
}

function verdictOf(line: string): Verdict | null {
  const trimmed = line.trim();
  if (!trimmed.startsWith("{")) return null;
  try {
    const value: unknown = JSON.parse(trimmed);
    if (value === null || typeof value !== "object") return null;
    const raw = value as Record<string, unknown>;
    if (typeof raw.verdict !== "string") return null;
    return {
      accepted: raw.verdict === "accepted",
      reason: typeof raw.reason === "string" ? raw.reason : null,
    };
  } catch {
    return null;
  }
}

/** The gate's final JSON verdict line, if it printed one. */
export function parseVerdict(log: string): Verdict | null {
  const lines = log.split("\n").filter((line) => line.trim());
  const last = lines[lines.length - 1];
  return last ? verdictOf(last) : null;
}

function metricOf(text: string) {
  const match = /^METRIC (\S+)\s*(.*)$/.exec(text);
  if (!match) return null;
  const fields: Record<string, number> = {};
  for (const pair of match[2].matchAll(/(\w+)=(-?[\d.]+)/g)) {
    fields[pair[1]] = Number(pair[2]);
  }
  return {
    name: match[1],
    baseline: fields.baseline ?? null,
    candidate: fields.candidate ?? null,
  };
}

/**
 * Gate output: scenario lines ("| ") get a dim rail, METRIC lines a
 * highlighted row with the change, and the JSON verdict a colored footer.
 */
export function GateLog({ log }: { log: string }) {
  const lines = log.replace(/\n+$/, "").split("\n");
  return (
    <div className="py-2 font-mono text-[12px] leading-5">
      {lines.map((line, index) => {
        const scenario = line.startsWith("| ") || line === "|";
        const text = scenario ? line.slice(2) : line;
        const verdict = !scenario ? verdictOf(line) : null;
        const metric = metricOf(text);

        if (verdict) {
          return (
            <div
              key={index}
              className={cx(
                "mx-4 my-2 flex items-center gap-3 rounded-md px-3 py-2 ring-1 ring-inset",
                verdict.accepted
                  ? "bg-ok-tint text-success ring-success/25"
                  : "bg-destructive/10 text-destructive ring-destructive/25",
              )}
            >
              <span className="font-sans text-label font-semibold">
                Verdict: {verdict.accepted ? "accepted" : "rejected"}
              </span>
              {verdict.reason && (
                <span className="min-w-0 break-words text-foreground/80">
                  {verdict.reason}
                </span>
              )}
            </div>
          );
        }

        let body;
        if (metric) {
          const change = percentChange(metric.baseline, metric.candidate);
          body = (
            <span className="flex flex-wrap items-baseline gap-x-3 rounded bg-info/[0.09] px-2 py-0.5 ring-1 ring-info/20 ring-inset">
              <span className="font-semibold text-info">METRIC</span>
              <span className="text-foreground">{metric.name}</span>
              <span className="tnum text-muted-foreground">
                baseline{" "}
                <span className="text-foreground">
                  {formatNumber(metric.baseline)}
                </span>
              </span>
              <span className="tnum text-muted-foreground">
                candidate{" "}
                <span className="text-foreground">
                  {formatNumber(metric.candidate)}
                </span>
              </span>
              <span className="tnum text-muted-foreground">
                {formatPercent(change)}
              </span>
            </span>
          );
        } else if (/^==\s/.test(text)) {
          body = (
            <span className="font-semibold text-foreground">
              {text.replace(/^==\s*/, "")}
            </span>
          );
        } else if (/\b(FAIL|FAILED|error|panicked)\b/.test(text)) {
          body = <span className="text-destructive">{text || " "}</span>;
        } else {
          body = (
            <span
              className={scenario ? "text-foreground/85" : "text-muted-foreground"}
            >
              {text || " "}
            </span>
          );
        }

        return (
          <div key={index} className="flex px-4">
            <span
              aria-hidden="true"
              className={cx(
                "w-4 shrink-0 select-none",
                scenario ? "text-faint-foreground/60" : "text-transparent",
              )}
            >
              │
            </span>
            <span className="min-w-0 flex-1 whitespace-pre-wrap break-words">
              {body}
            </span>
          </div>
        );
      })}
    </div>
  );
}
