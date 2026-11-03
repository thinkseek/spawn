import { FileCode } from "lucide-react";

import { cx } from "./ui";

type Row =
  | { kind: "file"; path: string; added: number; removed: number }
  | { kind: "hunk"; text: string }
  | { kind: "meta"; text: string }
  | {
      kind: "add" | "del" | "ctx";
      old: number | null;
      new: number | null;
      text: string;
    };

/** Parse a unified diff into display rows with old/new line numbers. */
export function parseDiff(patch: string): Row[] {
  const rows: Row[] = [];
  let file: Extract<Row, { kind: "file" }> | null = null;
  let inHunk = false;
  let oldLine = 0;
  let newLine = 0;

  for (const line of patch.replace(/\n$/, "").split("\n")) {
    if (line.startsWith("diff --git ")) {
      const match = / b\/(.+)$/.exec(line);
      file = {
        kind: "file",
        path: match ? match[1] : line.slice(11),
        added: 0,
        removed: 0,
      };
      rows.push(file);
      inHunk = false;
      continue;
    }
    const hunk = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@(.*)$/.exec(line);
    if (hunk) {
      oldLine = Number(hunk[1]);
      newLine = Number(hunk[2]);
      inHunk = true;
      rows.push({ kind: "hunk", text: line });
      continue;
    }
    if (!inHunk) {
      if (
        line.startsWith("index ") ||
        line.startsWith("--- ") ||
        line.startsWith("+++ ")
      ) {
        if (!file && line.startsWith("+++ ")) {
          file = {
            kind: "file",
            path: line.slice(4).replace(/^b\//, ""),
            added: 0,
            removed: 0,
          };
          rows.push(file);
        }
        continue;
      }
      if (line.trim()) rows.push({ kind: "meta", text: line });
      continue;
    }
    if (line.startsWith("+")) {
      rows.push({ kind: "add", old: null, new: newLine++, text: line.slice(1) });
      if (file) file.added += 1;
    } else if (line.startsWith("-")) {
      rows.push({ kind: "del", old: oldLine++, new: null, text: line.slice(1) });
      if (file) file.removed += 1;
    } else if (line.startsWith("\\")) {
      rows.push({ kind: "meta", text: line });
    } else {
      rows.push({
        kind: "ctx",
        old: oldLine++,
        new: newLine++,
        text: line.slice(1),
      });
    }
  }
  return rows;
}

export function diffStats(patch: string) {
  let files = 0;
  let added = 0;
  let removed = 0;
  for (const row of parseDiff(patch)) {
    if (row.kind === "file") {
      files += 1;
      added += row.added;
      removed += row.removed;
    }
  }
  return { files, added, removed };
}

const SIGN = { add: "+", del: "−", ctx: " " } as const;

const ROW_TONE = {
  add: "bg-success/[0.09]",
  del: "bg-destructive/[0.09]",
  ctx: "",
} as const;

const GUTTER_TONE = {
  add: "bg-success/[0.14] text-success/70",
  del: "bg-destructive/[0.14] text-destructive/70",
  ctx: "text-faint-foreground",
} as const;

/** Unified diff with old/new gutters, per-file headers and +/- tinting. */
export function DiffView({ patch }: { patch: string }) {
  const rows = parseDiff(patch);
  return (
    <div className="font-mono text-[12px] leading-5">
      {rows.map((row, index) => {
        if (row.kind === "file") {
          return (
            <div
              key={index}
              className="sticky top-0 z-[1] flex h-9 items-center gap-2 border-y border-surface-border bg-surface px-4 font-sans text-label first:border-t-0"
            >
              <FileCode size={14} className="shrink-0 text-faint-foreground" />
              <span className="min-w-0 truncate font-mono text-[12px] text-foreground">
                {row.path}
              </span>
              <span className="tnum ml-auto shrink-0 text-caption">
                <span className="text-success">+{row.added}</span>{" "}
                <span className="text-destructive">−{row.removed}</span>
              </span>
            </div>
          );
        }
        if (row.kind === "hunk") {
          return (
            <div
              key={index}
              className="flex bg-info/[0.07] text-info/80"
            >
              <span className="w-[6.5rem] shrink-0 border-r border-surface-border" />
              <span className="whitespace-pre px-3">{row.text}</span>
            </div>
          );
        }
        if (row.kind === "meta") {
          return (
            <div key={index} className="flex text-faint-foreground">
              <span className="w-[6.5rem] shrink-0 border-r border-surface-border" />
              <span className="whitespace-pre px-3 italic">{row.text}</span>
            </div>
          );
        }
        return (
          <div key={index} className={cx("flex w-max min-w-full", ROW_TONE[row.kind])}>
            <span
              className={cx(
                "tnum sticky left-0 flex w-[6.5rem] shrink-0 select-none border-r border-surface-border",
                GUTTER_TONE[row.kind],
                row.kind === "ctx" && "bg-page-canvas",
              )}
            >
              <span className="w-12 pr-2 text-right">{row.old ?? ""}</span>
              <span className="w-12 pr-2 text-right">{row.new ?? ""}</span>
            </span>
            <span
              className={cx(
                "w-5 shrink-0 select-none text-center",
                row.kind === "add"
                  ? "text-success"
                  : row.kind === "del"
                    ? "text-destructive"
                    : "text-faint-foreground",
              )}
            >
              {SIGN[row.kind]}
            </span>
            <span
              className={cx(
                "whitespace-pre pr-4",
                row.kind === "ctx" ? "text-muted-foreground" : "text-foreground",
              )}
            >
              {row.text || " "}
            </span>
          </div>
        );
      })}
    </div>
  );
}
