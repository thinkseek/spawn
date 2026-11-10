import { ArrowDown, SquareTerminal } from "lucide-react";
import { useLayoutEffect, useRef, useState } from "react";
import type { KeyboardEvent, ReactNode } from "react";

import { keyName } from "../keys";
import type { KeyName } from "../keys";
import { cx } from "./ui";

/** Light emphasis for Claude Code's screen-reader transcript. */
function lineClass(line: string): string {
  if (/^you: /.test(line)) return "text-foreground font-semibold";
  if (/^tool: /.test(line)) return "text-info";
  if (/^\s*(Error|error)[:\s]/.test(line)) return "text-destructive";
  if (/\(ctrl\+o to expand\)\s*$/.test(line) || /^… \+\d+ lines/.test(line)) {
    return "text-faint-foreground";
  }
  if (/esc to interrupt|shift\+tab to cycle/.test(line)) {
    return "text-faint-foreground";
  }
  return "";
}

function TerminalLines({ text }: { text: string }) {
  const lines = text.replace(/\n+$/, "").split("\n");
  return (
    <pre className="whitespace-pre-wrap break-words font-[inherit]">
      {lines.map((line, index) => (
        // Lines repeat, so the index is part of the key.
        <div key={index} className={lineClass(line)}>
          {line || " "}
        </div>
      ))}
    </pre>
  );
}

export function LiveBadge({ live, known = true }: { live: boolean; known?: boolean }) {
  return (
    <span
      className={cx(
        "inline-flex h-5 items-center gap-1.5 rounded-md px-1.5 text-caption font-medium ring-1 ring-inset",
        live
          ? "bg-ok-tint text-success ring-success/25"
          : "bg-white/[0.04] text-muted-foreground ring-white/10",
      )}
    >
      <span
        aria-hidden="true"
        className={cx(
          "inline-block size-1.5 rounded-full",
          live ? "animate-spawn-pulse bg-success" : "bg-muted-foreground",
        )}
      />
      {known ? (live ? "Live" : "Ended") : "…"}
    </span>
  );
}

interface TerminalPanelProps {
  title?: ReactNode;
  /** Beside the title: a switch between views of one screen. */
  tabs?: ReactNode;
  /** With `tabs`: the body is their tabpanel (its id, and the selected tab's id). */
  panel?: { id: string; labelledBy: string };
  /** Short identifier shown beside the title, e.g. the tmux session. */
  tag?: string | null;
  /** Right side of the header: freshness, live badge. */
  status?: ReactNode;
  text: string;
  /** Shown instead of the body while `text` is empty. */
  placeholder: string;
  className?: string;
  /** Named keys for the agent (backlog P1): while the body itself has
   * focus, Esc, the arrows, Enter, Y and N go to `onKey`, and `hint` (and
   * `feedback`, the last key's fate) shows under it. Never from a text box,
   * with a modifier, or on auto-repeat (keys.ts). */
  keys?: { hint: string; onKey: (name: KeyName) => void; feedback?: ReactNode };
}

/**
 * A live terminal: dark, mono, comfortable leading, and it follows the
 * bottom unless the reader scrolls up, in which case a "Follow output"
 * button brings it back.
 */
export function TerminalPanel({
  title = "Terminal",
  tabs,
  panel,
  tag,
  status,
  text,
  placeholder,
  className,
  keys,
}: TerminalPanelProps) {
  const scroller = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);
  const [following, setFollowing] = useState(true);
  const [focused, setFocused] = useState(false);
  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (!keys) return;
    const name = keyName({
      key: event.key,
      ctrlKey: event.ctrlKey,
      altKey: event.altKey,
      metaKey: event.metaKey,
      repeat: event.repeat,
      isComposing: event.nativeEvent.isComposing,
      onPanel: event.target === event.currentTarget,
    });
    if (!name) return;
    // The arrows would scroll the panel, Enter could submit a form, and Esc
    // would close the drawer around it (its listener is on window): the key
    // is the agent's.
    event.preventDefault();
    event.stopPropagation();
    keys.onKey(name);
  };

  useLayoutEffect(() => {
    const element = scroller.current;
    if (element && pinned.current) element.scrollTop = element.scrollHeight;
  }, [text]);

  return (
    // A terminal is dark in both themes, so it carries the dark theme: its
    // title, tag, status and buttons take the dark tokens (contrast.mjs
    // checks those), not the light page's dark ink on a dark ground.
    <section
      data-theme="dark"
      className={cx(
        "relative flex min-h-0 flex-col overflow-hidden rounded-lg border border-surface-border bg-terminal",
        className,
      )}
    >
      <header className="flex h-10 shrink-0 items-center gap-2 border-b border-white/[0.06] bg-white/[0.02] px-4">
        <SquareTerminal size={14} className="shrink-0 text-faint-foreground" />
        <h2 className="text-label font-medium text-foreground">{title}</h2>
        {tabs}
        {tag && (
          <span className="hidden truncate rounded bg-white/[0.05] px-1.5 py-px font-mono text-micro text-muted-foreground sm:inline">
            {tag}
          </span>
        )}
        {status && (
          <span className="ml-auto flex shrink-0 items-center gap-3">
            {status}
          </span>
        )}
      </header>

      <div
        ref={scroller}
        id={panel?.id}
        role={panel ? "tabpanel" : undefined}
        aria-labelledby={panel?.labelledBy}
        tabIndex={panel || keys ? 0 : undefined}
        aria-keyshortcuts={keys ? "Escape ArrowUp ArrowDown ArrowLeft ArrowRight Enter Y N" : undefined}
        aria-description={keys?.hint}
        onKeyDown={keys ? onKeyDown : undefined}
        onFocus={keys ? (event) => event.target === event.currentTarget && setFocused(true) : undefined}
        onBlur={keys ? () => setFocused(false) : undefined}
        onScroll={(event) => {
          const element = event.currentTarget;
          const atBottom =
            element.scrollHeight - element.scrollTop - element.clientHeight <
            24;
          pinned.current = atBottom;
          if (atBottom !== following) setFollowing(atBottom);
        }}
        className="min-h-0 flex-1 overflow-auto px-4 py-3 font-mono text-[12px] leading-5 text-terminal-foreground"
      >
        {text ? (
          <TerminalLines text={text} />
        ) : (
          <div className="grid h-full place-items-center text-caption text-faint-foreground">
            {placeholder}
          </div>
        )}
      </div>

      {keys && (focused || keys.feedback) ? (
        <div
          aria-live="polite"
          className="flex h-7 shrink-0 items-center gap-3 border-t border-white/[0.06] bg-white/[0.02] px-4 text-caption"
        >
          {focused ? <span className="truncate text-muted-foreground">{keys.hint}</span> : null}
          {keys.feedback ? <span className="ml-auto shrink-0 truncate">{keys.feedback}</span> : null}
        </div>
      ) : null}

      {!following && text && (
        <button
          type="button"
          onClick={() => {
            const element = scroller.current;
            if (!element) return;
            pinned.current = true;
            setFollowing(true);
            element.scrollTo({ top: element.scrollHeight, behavior: "smooth" });
          }}
          className="absolute right-4 bottom-3 inline-flex h-7 items-center gap-1.5 rounded-md border border-border-strong bg-surface-raised px-2.5 text-caption font-medium text-foreground shadow-[var(--floating-shadow)] transition-colors hover:bg-surface-selected"
        >
          <ArrowDown size={12} />
          Follow output
        </button>
      )}
    </section>
  );
}

/** A static terminal excerpt, e.g. the screen tail a judge saw. */
export function TerminalBlock({
  text,
  maxHeight = "16rem",
  className,
  tail = false,
}: {
  text: string;
  maxHeight?: string;
  className?: string;
  /** Open scrolled to the bottom, for screen tails. */
  tail?: boolean;
}) {
  const scroller = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const element = scroller.current;
    if (tail && element) element.scrollTop = element.scrollHeight;
  }, [tail, text]);
  return (
    <div
      ref={scroller}
      data-theme="dark"
      className={cx(
        "overflow-auto rounded-md border border-surface-border bg-terminal px-3 py-2.5 font-mono text-[12px] leading-5 text-terminal-foreground",
        className,
      )}
      style={{ maxHeight }}
    >
      <TerminalLines text={text} />
    </div>
  );
}
