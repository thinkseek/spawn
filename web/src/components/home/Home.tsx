import { useState } from "react";
import type { ReactNode } from "react";
import { ArrowRight, Bot, CircleAlert, CircleCheck, Copy, Flag, History, Info, MessageSquareText, Target } from "lucide-react";

import { relativeTime } from "../../format";
import type { AgentsNow, Digest, MissionsToday, NeedItem, NotesShown } from "../../home";
import { needsSummary } from "../../home";
import { Card, CardHeader, FOCUS_RING, FOCUS_RING_INSET, cx, sinceText } from "../ui";

// The home page's sections (design-ui-information.md §3, layout A): what
// needs you, missions today, agents working now, the supervisor's latest
// lines, and today as a digest. Each reads a model from home.ts.

/** A command to copy: the page has no write path of its own. */
function CopyCommand({ command }: { command: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <span className="inline-flex min-w-0 items-center gap-1.5">
      <code className="min-w-0 truncate rounded-chip bg-inset px-1.5 py-0.5 font-mono text-micro text-foreground select-all">
        {command}
      </code>
      <button
        type="button"
        onClick={() => {
          void navigator.clipboard?.writeText(command).then(
            () => {
              setCopied(true);
              window.setTimeout(() => setCopied(false), 1_500);
            },
            () => setCopied(false),
          );
        }}
        title="Copy the command"
        className={cx(
          "inline-flex h-6 shrink-0 items-center gap-1 rounded-chip px-1.5 text-micro text-muted-foreground hover:bg-surface-hover hover:text-foreground",
          FOCUS_RING,
        )}
      >
        <Copy size={11} aria-hidden="true" />
        {copied ? "Copied" : "Copy"}
      </button>
    </span>
  );
}

function NeedDetail({ item }: { item: NeedItem }) {
  const [open, setOpen] = useState(false);
  if (!item.detail) return null;
  return (
    <p className={cx("mt-0.5 text-caption text-muted-foreground", !open && "flex min-w-0 items-baseline")}>
      <span className={cx(!open && "min-w-0 truncate")}>{open && item.full ? item.full : item.detail}</span>
      {item.full && (
        <button
          type="button"
          onClick={() => setOpen((value) => !value)}
          aria-expanded={open}
          className={cx("ml-1.5 shrink-0 rounded-chip text-faint-foreground hover:text-foreground hover:underline", FOCUS_RING)}
        >
          {open ? "less" : "more"}
        </button>
      )}
    </p>
  );
}

export function NeedsYou({ items, onOpenAgent }: { items: NeedItem[]; onOpenAgent: (session: string) => void }) {
  const act = items.filter((item) => item.tone === "act").length;
  if (items.length === 0) {
    return (
      <Card className="flex items-center gap-2 px-4 py-3">
        <CircleCheck size={14} className="shrink-0 text-success" aria-hidden="true" />
        <h2 className="text-label font-medium text-foreground">Nothing needs you.</h2>
      </Card>
    );
  }
  return (
    <Card>
      <CardHeader
        icon={<CircleAlert size={14} className={act ? "text-destructive" : undefined} />}
        title="Needs you"
        meta={<span className="tnum">{needsSummary(items)}</span>}
      />
      <ul className="divide-y divide-surface-border">
        {items.map((item) => (
          <li key={item.key} className="flex min-w-0 items-start gap-3 px-4 py-2.5">
            {item.tone === "act" ? (
              <Flag size={13} className="mt-0.5 shrink-0 text-destructive" aria-label="to act on" />
            ) : (
              <Info size={13} className="mt-0.5 shrink-0 text-faint-foreground" aria-label="to know" />
            )}
            <div className="min-w-0 flex-1">
              <div className="flex min-w-0 items-baseline gap-2">
                <span className="min-w-0 truncate text-label font-medium text-foreground">{item.title}</span>
                {item.since_ms !== null && (
                  <span className="tnum shrink-0 text-caption text-faint-foreground">{sinceText(item.since_ms)}</span>
                )}
              </div>
              <NeedDetail item={item} />
            </div>
            <div className="flex shrink-0 items-center gap-2 self-center">
              {item.command && <CopyCommand command={item.command} />}
              {item.session && (
                <button
                  type="button"
                  onClick={() => onOpenAgent(item.session ?? "")}
                  className={cx(
                    "inline-flex h-7 items-center gap-1 rounded-control px-2 text-caption font-medium text-foreground shadow-hairline hover:bg-surface-hover",
                    FOCUS_RING,
                  )}
                >
                  Open
                  <ArrowRight size={12} aria-hidden="true" />
                </button>
              )}
            </div>
          </li>
        ))}
      </ul>
    </Card>
  );
}

export function MissionsTodayCard({ model, onOpen }: { model: MissionsToday; onOpen: () => void }) {
  return (
    <Card>
      <CardHeader icon={<Target size={14} />} title="Missions" meta={<span>today</span>} />
      {model.rows.length === 0 ? (
        <p className="px-4 py-3 text-caption text-muted-foreground">No mission has work open.</p>
      ) : (
        <ul className="divide-y divide-surface-border">
          {model.rows.map((row) => (
            <li key={row.id ?? row.name}>
              <button
                type="button"
                onClick={onOpen}
                className={cx("block w-full px-4 py-2.5 text-left hover:bg-surface-hover", FOCUS_RING_INSET)}
              >
                <div className="flex min-w-0 items-baseline gap-2">
                  <span className="tnum shrink-0 text-caption text-faint-foreground">#{row.id}</span>
                  <span className="shrink-0 text-label font-semibold text-foreground">{row.name}</span>
                  {row.halted && <span className="shrink-0 text-caption font-medium text-destructive">halted</span>}
                  <span className="tnum ml-auto shrink-0 text-caption text-muted-foreground">
                    {row.landedToday} landed today
                  </span>
                </div>
                <ul className="mt-1 space-y-0.5">
                  {row.inFlight.map((task) => (
                    <li key={task.task_id ?? task.title} className="flex min-w-0 items-baseline gap-2 text-caption">
                      <span className="tnum w-10 shrink-0 font-mono text-micro text-faint-foreground">T{task.task_id}</span>
                      <span className="min-w-0 flex-1 truncate text-foreground">{task.title ?? "untitled"}</span>
                      <span className="tnum shrink-0 text-muted-foreground">
                        {task.state}
                        {task.since_ms !== null && ` ${sinceText(task.since_ms)}`}
                      </span>
                    </li>
                  ))}
                </ul>
                {row.lastRejection && (
                  <p className="mt-1 truncate text-caption text-muted-foreground">
                    last sent back: T{row.lastRejection.task_id} {row.lastRejection.verdict}{" "}
                    {relativeTime(row.lastRejection.at_ms)}
                    {row.lastRejection.reason && <> · {row.lastRejection.reason}</>}
                  </p>
                )}
              </button>
            </li>
          ))}
        </ul>
      )}
      {(model.quiet.length > 0 || model.closed > 0) && (
        <p className="border-t border-surface-border px-4 py-2 text-caption text-faint-foreground">
          {model.quiet.length > 0 && (
            <>
              Nothing open: {model.quiet.map((mission) => `#${mission.id} ${mission.name}`).join(", ")}
            </>
          )}
          {model.quiet.length > 0 && model.closed > 0 && " · "}
          {model.closed > 0 && `${model.closed} closed`}
        </p>
      )}
    </Card>
  );
}

export function AgentsNowCard({ model, onOpenAgent }: { model: AgentsNow; onOpenAgent: (session: string) => void }) {
  return (
    <Card>
      <CardHeader icon={<Bot size={14} />} title="Working now" meta={<span className="tnum">{model.working.length}</span>} />
      {model.working.length === 0 ? (
        <p className="px-4 py-3 text-caption text-muted-foreground">No agent is working.</p>
      ) : (
        <ul className="divide-y divide-surface-border">
          {model.working.map((agent) => (
            <li key={agent.session_id}>
              <button
                type="button"
                onClick={() => onOpenAgent(agent.session_id)}
                className={cx(
                  "grid w-full grid-cols-[minmax(0,14rem)_minmax(0,1fr)_minmax(0,16rem)] items-baseline gap-3 px-4 py-2 text-left hover:bg-surface-hover",
                  FOCUS_RING_INSET,
                )}
              >
                <span className="truncate text-label font-medium text-foreground">{agent.name}</span>
                <span className="truncate text-caption text-muted-foreground" title={agent.task ?? undefined}>
                  {agent.task ?? "no task"}
                </span>
                <span className="tnum truncate text-right text-caption text-foreground">
                  {agent.doing}
                  {agent.since_ms !== null && (
                    <span className="text-faint-foreground"> · {sinceText(agent.since_ms)}</span>
                  )}
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
      {model.idle.length > 0 && (
        <p className="border-t border-surface-border px-4 py-2 text-caption text-faint-foreground">
          Idle:{" "}
          {model.idle.map((agent, index) => (
            <span key={agent.session_id}>
              {index > 0 && ", "}
              <button
                type="button"
                onClick={() => onOpenAgent(agent.session_id)}
                className={cx("rounded-chip text-muted-foreground hover:text-foreground hover:underline", FOCUS_RING)}
              >
                {agent.name}
              </button>
              {agent.since_ms !== null && ` ${sinceText(agent.since_ms)}`}
            </span>
          ))}
        </p>
      )}
    </Card>
  );
}

export function SupervisorNotes({
  model,
  all,
  talk,
}: {
  model: NotesShown;
  all: Array<{ text: string; at_ms: number | null }>;
  /** The box that types to the supervisor. */
  talk: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const shown = open ? all : model.shown;
  return (
    <Card>
      <CardHeader
        icon={<MessageSquareText size={14} />}
        title="From the supervisor"
        meta={
          model.more > 0 && (
            <button
              type="button"
              onClick={() => setOpen((value) => !value)}
              aria-expanded={open}
              className={cx("rounded-chip px-1 text-caption text-muted-foreground hover:text-foreground", FOCUS_RING)}
            >
              {open ? "Show the latest" : `${model.more} more`}
            </button>
          )
        }
      />
      {shown.length === 0 ? (
        <p className="px-4 py-2.5 text-caption text-muted-foreground">Nothing new in the last 6 hours.</p>
      ) : (
        <ul className="divide-y divide-surface-border">
          {shown.map((note, index) => (
            <li key={`${note.at_ms}-${index}`} className="flex min-w-0 items-baseline gap-3 px-4 py-2">
              <p className={cx("min-w-0 flex-1 text-caption text-foreground", !open && "truncate")} title={note.text}>
                {note.text}
              </p>
              <span className="tnum shrink-0 text-caption text-faint-foreground">{relativeTime(note.at_ms)}</span>
            </li>
          ))}
        </ul>
      )}
      <div className="border-t border-surface-border px-4 py-2.5">{talk}</div>
    </Card>
  );
}

const DIGEST_TONE: Record<string, string> = {
  landed: "text-success",
  "sent back": "text-warning",
  alert: "text-destructive",
};

export function DigestCard({ digest }: { digest: Digest }) {
  const [open, setOpen] = useState(false);
  return (
    <Card>
      <button
        type="button"
        onClick={() => setOpen((value) => !value)}
        aria-expanded={open}
        className={cx("flex h-10 w-full items-center gap-2 px-4 text-left hover:bg-surface-hover", FOCUS_RING_INSET, open && "border-b border-surface-border")}
      >
        <History size={14} className="shrink-0 text-faint-foreground" aria-hidden="true" />
        <span className="text-label font-medium text-foreground">Today</span>
        <span className="tnum text-caption text-muted-foreground">
          {digest.landed} landed · {digest.sentBack} sent back · {digest.alerts} alert{digest.alerts === 1 ? "" : "s"}
        </span>
        <span className="ml-auto text-caption text-muted-foreground">{open ? "Hide" : "Show"}</span>
      </button>
      {open && (
        <ul className="max-h-80 divide-y divide-surface-border overflow-y-auto">
          {digest.entries.map((entry, index) => (
            <li key={`${entry.at_ms}-${index}`} className="flex min-w-0 items-baseline gap-3 px-4 py-1.5 text-caption">
              <span className="tnum w-16 shrink-0 text-faint-foreground">
                {new Date(entry.at_ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}
              </span>
              <span className={cx("w-20 shrink-0 font-medium", DIGEST_TONE[entry.kind])}>{entry.kind}</span>
              <span className="min-w-0 flex-1 truncate text-foreground" title={entry.text}>
                {entry.text}
              </span>
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}
