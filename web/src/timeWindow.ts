/* The visible window of a time axis: fitting, zooming around a point,
 * panning and clamping, and the clock format for a tick step. Shared by the
 * session trace and the missions timeline. Pure, tested in
 * web/test/timeWindow.test.ts. */

/** A visible window of the time axis, in epoch ms. */
export interface Domain {
  from: number;
  to: number;
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
