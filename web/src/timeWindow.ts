/* The visible window of a time axis: fitting, zooming around a point,
 * panning and clamping, and the clock format for a tick step. Shared by the
 * session trace and the missions timeline. Pure, tested in
 * web/test/timeWindow.test.ts. */

/** A visible window of the time axis, in epoch ms. */
export interface Domain {
  from: number;
  to: number;
}

/** What a window follows: the whole extent, the live end with a fixed
 * width, everything from a fixed start to the live end (a run still going,
 * whose start stays in view as it grows), or a fixed range. Its times are
 * on the axis the window is drawn on: folded time when idle is folded. */
export type Follow =
  | { mode: "fit" }
  | { mode: "tail"; width: number }
  | { mode: "since"; from: number }
  | { mode: "fixed"; domain: Domain };

/** `follow` moved to a new axis: `map` takes a time on the old axis to the
 * same real time on the new one (new fold of old unfold). A window set in
 * folded time would otherwise point at other real times once the fold
 * changes (the Linear toggle, a live session's new break or band width).
 * A tail keeps its width on screen, as it follows the end anyway. */
export function refollow(follow: Follow, map: (time: number) => number): Follow {
  if (follow.mode === "since") return { mode: "since", from: map(follow.from) };
  if (follow.mode === "fixed") {
    return { mode: "fixed", domain: { from: map(follow.domain.from), to: map(follow.domain.to) } };
  }
  return follow;
}

/** The missions timeline's default window (design §4.4): the last `day`,
 * from that long before now to the live end, followed as it grows. A board
 * quiet for longer shows its last `day` of activity instead of an empty
 * plot, and one younger than `day` shows whole (fit). `start` and `end`
 * are the shown bars' real span (`end` is now while one is open); the
 * window is on `fold`'s axis, so it holds when the fold changes
 * (refollow). The overview strip still shows the lifetime. */
export function todayWindow(
  fold: (time: number) => number,
  span: { start: number; end: number },
  now: number,
  day: number,
): Follow {
  const from = (span.end >= now - day ? now : span.end) - day;
  if (from <= span.start) return { mode: "fit" };
  return { mode: "since", from: fold(from) };
}

/** The narrowest window the axis zooms into. */
export const MIN_WINDOW_MS = 200;

/** A span of time with a little air on both sides: the window that fits it. */
export function fitDomain(span: { start: number; end: number }): Domain {
  const width = Math.max(span.end - span.start, 1_000);
  const pad = width * 0.02;
  return { from: span.start - pad, to: span.start + width + pad };
}

/** Keep a window inside the fitted extent and no narrower than the floor. */
export function clampDomain(domain: Domain, extent: Domain): Domain {
  const extentWidth = extent.to - extent.from;
  let width = Math.min(Math.max(domain.to - domain.from, MIN_WINDOW_MS), extentWidth);
  if (!Number.isFinite(width) || width <= 0) width = extentWidth;
  let from = domain.from;
  if (from < extent.from) from = extent.from;
  if (from + width > extent.to) from = extent.to - width;
  return { from, to: from + width };
}

/** Zoom by a factor (<1 zooms in) keeping the time under the pointer fixed. */
export function zoomDomain(
  domain: Domain,
  factor: number,
  anchor: number,
  extent: Domain,
): Domain {
  const at = Math.min(Math.max(anchor, domain.from), domain.to);
  return clampDomain(
    {
      from: at - (at - domain.from) * factor,
      to: at + (domain.to - at) * factor,
    },
    extent,
  );
}

/** Shift a window by a time delta. */
export function panDomain(domain: Domain, delta: number, extent: Domain): Domain {
  return clampDomain({ from: domain.from + delta, to: domain.to + delta }, extent);
}

/** Clock-tick format for a tick step: seconds appear once minutes are too
 * coarse, milliseconds once seconds are. */
export function tickPattern(stepMs: number): string {
  if (stepMs < 1_000) return "%H:%M:%S.%L";
  if (stepMs < 60_000) return "%H:%M:%S";
  return "%H:%M";
}
