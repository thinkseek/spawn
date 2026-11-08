import type { ReactNode } from "react";

import { cx } from "../ui";

// A small markdown-ish renderer for task briefs and reports. It understands
// headings, bullet and numbered lists, fenced code, blockquotes, horizontal
// rules and paragraphs, plus inline `code`, **bold** and *italic*. Everything
// becomes React elements; no raw HTML is ever injected.

type Block =
  | { kind: "heading"; level: number; text: string }
  | { kind: "code"; lang: string; text: string }
  | { kind: "list"; ordered: boolean; items: string[] }
  | { kind: "quote"; text: string }
  | { kind: "rule" }
  | { kind: "paragraph"; text: string };

const HEADING = /^(#{1,6})\s+(.*)$/;
const FENCE = /^\s*(```|~~~)\s*([\w+-]*)\s*$/;
const BULLET = /^\s*[-*+]\s+(.*)$/;
const NUMBERED = /^\s*\d+[.)]\s+(.*)$/;
const QUOTE = /^\s*>\s?(.*)$/;
const RULE = /^\s*([-*_])(\s*\1){2,}\s*$/;

function parse(source: string): Block[] {
  const lines = source.replace(/\r\n?/g, "\n").split("\n");
  const blocks: Block[] = [];
  let index = 0;

  while (index < lines.length) {
    const line = lines[index];

    if (line.trim() === "") {
      index += 1;
      continue;
    }

    const fence = FENCE.exec(line);
    if (fence) {
      const marker = fence[1];
      const body: string[] = [];
      index += 1;
      while (index < lines.length && !lines[index].trim().startsWith(marker)) {
        body.push(lines[index]);
        index += 1;
      }
      index += 1; // closing fence (or end of input)
      blocks.push({ kind: "code", lang: fence[2], text: body.join("\n") });
      continue;
    }

    const heading = HEADING.exec(line);
    if (heading) {
      blocks.push({
        kind: "heading",
        level: heading[1].length,
        text: heading[2].replace(/\s+#+\s*$/, ""),
      });
      index += 1;
      continue;
    }

    if (RULE.test(line)) {
      blocks.push({ kind: "rule" });
      index += 1;
      continue;
    }

    if (BULLET.test(line) || NUMBERED.test(line)) {
      const ordered = !BULLET.test(line);
      const pattern = ordered ? NUMBERED : BULLET;
      const items: string[] = [];
      while (index < lines.length) {
        const current = lines[index];
        const item = pattern.exec(current);
        if (item) {
          items.push(item[1]);
        } else if (
          current.trim() !== "" &&
          /^\s{2,}/.test(current) &&
          items.length > 0
        ) {
          // Indented continuation of the previous item.
          items[items.length - 1] += ` ${current.trim()}`;
        } else {
          break;
        }
        index += 1;
      }
      blocks.push({ kind: "list", ordered, items });
      continue;
    }

    if (QUOTE.test(line)) {
      const body: string[] = [];
      while (index < lines.length) {
        const quoted = QUOTE.exec(lines[index]);
        if (!quoted) break;
        body.push(quoted[1]);
        index += 1;
      }
      blocks.push({ kind: "quote", text: body.join(" ") });
      continue;
    }

    const body: string[] = [];
    while (index < lines.length) {
      const current = lines[index];
      if (
        current.trim() === "" ||
        FENCE.test(current) ||
        HEADING.test(current) ||
        BULLET.test(current) ||
        NUMBERED.test(current) ||
        QUOTE.test(current)
      ) {
        break;
      }
      body.push(current);
      index += 1;
    }
    blocks.push({ kind: "paragraph", text: body.join("\n") });
  }

  return blocks;
}

const INLINE = /(`[^`\n]+`)|(\*\*[^*\n]+\*\*)|(__[^_\n]+__)|(\*[^*\n]+\*)/g;

function inline(text: string): ReactNode[] {
  const out: ReactNode[] = [];
  let last = 0;
  let key = 0;
  for (const match of text.matchAll(INLINE)) {
    const start = match.index ?? 0;
    if (start > last) out.push(text.slice(last, start));
    const token = match[0];
    if (match[1]) {
      out.push(
        <code
          key={key++}
          className="rounded bg-muted px-1 py-px font-mono text-[0.92em] text-foreground"
        >
          {token.slice(1, -1)}
        </code>,
      );
    } else if (match[2] || match[3]) {
      out.push(
        <strong key={key++} className="font-semibold text-foreground">
          {token.slice(2, -2)}
        </strong>,
      );
    } else {
      out.push(<em key={key++}>{token.slice(1, -1)}</em>);
    }
    last = start + token.length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

const HEADING_CLASS: Record<number, string> = {
  1: "text-body font-semibold",
  2: "text-label font-semibold",
  3: "text-label font-medium",
};

export function Markdown({
  source,
  className,
}: {
  source: string | null | undefined;
  className?: string;
}) {
  if (!source) return null;
  const blocks = parse(source);
  return (
    <div
      className={cx(
        "min-w-0 space-y-2 break-words text-caption text-foreground",
        className,
      )}
    >
      {blocks.map((block, index) => {
        switch (block.kind) {
          case "heading":
            return (
              <div
                key={index}
                role="heading"
                aria-level={block.level}
                className={cx(
                  "pt-1 text-foreground",
                  HEADING_CLASS[block.level] ?? "text-caption font-medium",
                )}
              >
                {inline(block.text)}
              </div>
            );
          case "code":
            return (
              <pre
                key={index}
                className="overflow-x-auto rounded-md border border-surface-border bg-muted/60 px-3 py-2 font-mono text-micro leading-relaxed text-foreground"
              >
                <code>{block.text}</code>
              </pre>
            );
          case "list": {
            const ListTag = block.ordered ? "ol" : "ul";
            return (
              <ListTag
                key={index}
                className={cx(
                  "space-y-0.5 pl-5",
                  block.ordered ? "list-decimal" : "list-disc",
                  "marker:text-faint-foreground",
                )}
              >
                {block.items.map((item, itemIndex) => (
                  <li key={itemIndex}>{inline(item)}</li>
                ))}
              </ListTag>
            );
          }
          case "quote":
            return (
              <blockquote
                key={index}
                className="border-l-2 border-border-strong pl-3 text-muted-foreground"
              >
                {inline(block.text)}
              </blockquote>
            );
          case "rule":
            return <hr key={index} className="border-surface-border" />;
          case "paragraph":
            return (
              <p key={index} className="whitespace-pre-wrap">
                {inline(block.text)}
              </p>
            );
        }
      })}
    </div>
  );
}
