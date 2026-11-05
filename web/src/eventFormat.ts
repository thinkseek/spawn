/* How one timeline event reads as text: shared by the event list and the
 * trace view's detail panel, so both show the same fields the same way. */

import { formatDuration } from "./format";

/** Marker color by event family; neutral unless the event means something. */
export function eventTone(name: string): string {
  if (/fail|error|lost|crash|conflict/i.test(name)) return "bg-destructive";
  if (name === "artifact.committed") return "bg-success";
  if (name.startsWith("execution.") || name.startsWith("session.")) {
    return "bg-info";
  }
  if (name.startsWith("notification.") || name.startsWith("screen.")) {
    return "bg-warning";
  }
  return "bg-faint-foreground/60";
}

export function short(value: unknown): string {
  if (typeof value === "string") {
    if (/^[0-9a-f]{40}$/.test(value)) return value.slice(0, 7);
    // Opaque ids (toolu_01Ab…) only need their tail to be told apart.
    const id = /^([a-z]+_)([A-Za-z0-9]{16,})$/.exec(value);
    if (id) return `${id[1]}…${id[2].slice(-6)}`;
    return value;
  }
  if (value === null || value === undefined) return "null";
  if (typeof value === "object") {
    try {
      return JSON.stringify(value);
    } catch {
      return String(value);
    }
  }
  return String(value);
}

/** Flatten one level of nesting into key/value pairs. */
export function pairs(detail: unknown): Array<[string, string]> {
  if (detail === null || typeof detail !== "object" || Array.isArray(detail)) {
    return detail == null ? [] : [["", short(detail)]];
  }
  const out: Array<[string, string]> = [];
  for (const [key, value] of Object.entries(detail as Record<string, unknown>)) {
    if (value === null) continue;
    if (key.endsWith("_ms") && typeof value === "number") {
      out.push([key.replace(/_ms$/, ""), formatDuration(value)]);
    } else if (value && typeof value === "object" && !Array.isArray(value)) {
      for (const [inner, innerValue] of Object.entries(
        value as Record<string, unknown>,
      )) {
        out.push([`${key}.${inner}`, short(innerValue)]);
      }
    } else {
      out.push([key, short(value)]);
    }
  }
  return out;
}

export function fullText(detail: unknown): string {
  try {
    return typeof detail === "string"
      ? detail
      : JSON.stringify(detail, null, 2);
  } catch {
    return String(detail);
  }
}
