/** Format an epoch-millisecond time as a short age. */
export function relativeTime(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms) || ms <= 0) return "—";
  const seconds = Math.floor(Math.max(0, Date.now() - ms) / 1000);
  if (seconds < 1) return "just now";
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
}

/** Format an epoch-millisecond time as a local wall clock. */
export function clockTime(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms) || ms <= 0) return "—";
  const date = new Date(ms);
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

/** Format an execution duration. */
export function formatDuration(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms)) return "—";
  if (ms < 1_000) return `${Math.round(ms)}ms`;
  const seconds = ms / 1_000;
  if (seconds < 60) return `${seconds.toFixed(1)}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ${Math.round(seconds % 60)}s`;
  return `${Math.floor(minutes / 60)}h ${minutes % 60}m`;
}

/** Format a byte count for the patch size. */
export function formatBytes(bytes: number | null | undefined): string {
  if (bytes == null || !Number.isFinite(bytes)) return "—";
  if (bytes < 1_024) return `${bytes} B`;
  const kilobytes = bytes / 1_024;
  if (kilobytes < 1_024) return `${kilobytes.toFixed(1)} KB`;
  return `${(kilobytes / 1_024).toFixed(1)} MB`;
}

/** Format the time a stage took, as "+8m 16s": seconds below ten minutes,
 * whole minutes below an hour, then hours and minutes. */
export function stageDuration(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms) || ms < 0) return "";
  const seconds = Math.round(ms / 1_000);
  if (seconds < 60) return `+${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 10) return seconds % 60 === 0 ? `+${minutes}m` : `+${minutes}m ${seconds % 60}s`;
  if (minutes < 60) return `+${minutes}m`;
  const hours = Math.floor(minutes / 60);
  return minutes % 60 === 0 ? `+${hours}h` : `+${hours}h ${minutes % 60}m`;
}

const pad2 = (value: number) => String(value).padStart(2, "0");

/** A time as the local wall clock with its date: `2026-11-06 17:37`. */
export function localDateTime(ms: number | null | undefined, seconds = false): string {
  if (ms == null || !Number.isFinite(ms) || ms <= 0) return "—";
  const date = new Date(ms);
  const day = `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())}`;
  const time = `${pad2(date.getHours())}:${pad2(date.getMinutes())}${seconds ? `:${pad2(date.getSeconds())}` : ""}`;
  return `${day} ${time}`;
}

/** The same moment in UTC: `2026-11-06 09:37:22 UTC`. */
export function utcDateTime(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms) || ms <= 0) return "—";
  return `${new Date(ms).toISOString().slice(0, 19).replace("T", " ")} UTC`;
}

/** The tooltip of a relative or local time: the local time to the second,
 * then UTC. Undefined when there is no time, so no empty tooltip shows. */
export function absoluteTitle(ms: number | null | undefined): string | undefined {
  if (ms == null || !Number.isFinite(ms) || ms <= 0) return undefined;
  return `${localDateTime(ms, true)} local · ${utcDateTime(ms)}`;
}
