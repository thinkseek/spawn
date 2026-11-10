/* The visible window of a time axis as React state: what it follows (the
 * whole extent, the live end, or a fixed range), glides between windows
 * with Motion, wheel zoom and drag pan. Shared by the session trace and
 * the missions timeline, so both axes move the same way. */

import { animate } from "motion/react";
import type { AnimationPlaybackControls } from "motion/react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { RefObject } from "react";

import { clampDomain, panDomain, refollow, zoomDomain } from "../../timeWindow";
import type { Domain, Follow } from "../../timeWindow";

export type { Follow } from "../../timeWindow";

/** Pointer travel that turns a press into a drag. */
export const DRAG_PX = 3;

export interface TimeWindow {
  domain: Domain;
  extent: Domain;
  follow: Follow;
  /** Milliseconds per pixel of the plot. */
  scale: number;
  /** Time to plot x. */
  px: (time: number) => number;
  /** Set the window at once (wheel, drag): it tracks the hand. */
  setDomain: (domain: Domain) => void;
  /** Glide to a window (buttons, double-click, the overview). */
  glideTo: (domain: Domain) => void;
  zoomBy: (factor: number) => void;
  /** Glide to a range with some air around it. */
  zoomTo: (from: number, to: number) => void;
  fit: () => void;
  /** Show `from` to the live end, and keep following that end. */
  since: (from: number) => void;
  /** The axis changed under the window (a new fold): `map` takes an old
   * axis time to the new axis time of the same real moment (refollow). */
  refold: (map: (time: number) => number) => void;
  stop: () => void;
  /** Latest values, for native listeners. */
  latest: RefObject<{ domain: Domain; extent: Domain; scale: number }>;
}

export function useTimeWindow(
  extent: Domain,
  plotW: number,
  reduceMotion: boolean,
): TimeWindow {
  const [follow, setFollow] = useState<Follow>({ mode: "fit" });
  const domain = useMemo(() => {
    if (follow.mode === "fit") return extent;
    if (follow.mode === "tail") {
      return clampDomain({ from: extent.to - follow.width, to: extent.to }, extent);
    }
    if (follow.mode === "since") {
      return clampDomain({ from: Math.max(extent.from, follow.from), to: extent.to }, extent);
    }
    return clampDomain(follow.domain, extent);
  }, [follow, extent]);
  const scale = (domain.to - domain.from) / Math.max(plotW, 1);
  const px = useCallback(
    (time: number) => (time - domain.from) / scale,
    [domain.from, scale],
  );

  const latest = useRef({ domain, extent, scale });
  latest.current = { domain, extent, scale };

  const setDomain = useCallback((next: Domain) => {
    const full = latest.current.extent;
    const fullWidth = full.to - full.from;
    const width = next.to - next.from;
    if (width >= fullWidth * 0.999) setFollow({ mode: "fit" });
    else if (next.to >= full.to - width * 0.01) setFollow({ mode: "tail", width });
    else setFollow({ mode: "fixed", domain: next });
  }, []);

  const glide = useRef<AnimationPlaybackControls | null>(null);
  const stop = useCallback(() => glide.current?.stop(), []);
  const glideTo = useCallback(
    (target: Domain) => {
      glide.current?.stop();
      const from = latest.current.domain;
      if (reduceMotion) {
        setDomain(target);
        return;
      }
      glide.current = animate(0, 1, {
        duration: 0.32,
        ease: [0.2, 0.8, 0.2, 1],
        onUpdate: (t) =>
          setDomain({
            from: from.from + (target.from - from.from) * t,
            to: from.to + (target.to - from.to) * t,
          }),
      });
    },
    [reduceMotion, setDomain],
  );
  useEffect(() => () => glide.current?.stop(), []);

  const zoomBy = useCallback(
    (factor: number) => {
      const { domain: current, extent: full } = latest.current;
      glideTo(zoomDomain(current, factor, (current.from + current.to) / 2, full));
    },
    [glideTo],
  );
  const zoomTo = useCallback(
    (from: number, to: number) => {
      const pad = Math.max((to - from) * 0.15, 500);
      glideTo(clampDomain({ from: from - pad, to: to + pad }, latest.current.extent));
    },
    [glideTo],
  );
  const fit = useCallback(() => glideTo(latest.current.extent), [glideTo]);
  const since = useCallback((from: number) => {
    glide.current?.stop();
    setFollow({ mode: "since", from });
  }, []);
  const refold = useCallback((map: (time: number) => number) => {
    glide.current?.stop();
    setFollow((current) => refollow(current, map));
  }, []);

  return {
    domain,
    extent,
    follow,
    scale,
    px,
    setDomain,
    glideTo,
    zoomBy,
    zoomTo,
    fit,
    since,
    refold,
    stop,
    latest,
  };
}

/** Wheel on a plot: ⌘/Ctrl (and trackpad pinch, which arrives as
 * ctrl+wheel) zooms around the pointer; sideways scroll and Shift pan;
 * plain vertical scroll is left to the page. `left` is the plot's offset
 * inside the element. A native listener: React's is passive. */
export function useWheelZoom(
  ref: RefObject<HTMLElement | null>,
  view: TimeWindow,
  left: number,
) {
  const offset = useRef(left);
  offset.current = left;
  const { latest, setDomain, stop } = view;
  useEffect(() => {
    const element = ref.current;
    if (!element) return;
    const onWheel = (event: WheelEvent) => {
      const { domain, extent, scale } = latest.current;
      const x = event.clientX - element.getBoundingClientRect().left - offset.current;
      if (x < 0) return;
      const lines = event.deltaMode === 1 ? 16 : 1;
      const dx = event.deltaX * lines;
      const dy = event.deltaY * lines;
      if (event.ctrlKey || event.metaKey) {
        event.preventDefault();
        stop();
        const factor = Math.exp(dy * (event.ctrlKey ? 0.01 : 0.002));
        setDomain(zoomDomain(domain, factor, domain.from + x * scale, extent));
      } else if (event.shiftKey || Math.abs(dx) > Math.abs(dy)) {
        event.preventDefault();
        stop();
        const delta = event.shiftKey && dx === 0 ? dy : dx;
        setDomain(panDomain(domain, delta * scale, extent));
      }
    };
    element.addEventListener("wheel", onWheel, { passive: false });
    return () => element.removeEventListener("wheel", onWheel);
  }, [ref, latest, setDomain, stop]);
}

/** Drag to pan. `begin` on a press inside the plot; `move` returns true
 * while the press is a drag (and pans); `end` returns whether it was one,
 * so the caller can treat a press that did not move as a click. */
export function useDragPan(view: TimeWindow) {
  const drag = useRef<{ x: number; domain: Domain; moved: boolean } | null>(null);
  const { latest, setDomain, stop } = view;
  return useMemo(
    () => ({
      begin(clientX: number) {
        stop();
        drag.current = { x: clientX, domain: latest.current.domain, moved: false };
      },
      move(clientX: number): boolean {
        const active = drag.current;
        if (!active) return false;
        const dx = clientX - active.x;
        if (!active.moved && Math.abs(dx) < DRAG_PX) return false;
        active.moved = true;
        const { extent, scale } = latest.current;
        setDomain(panDomain(active.domain, -dx * scale, extent));
        return true;
      },
      end(): boolean {
        const moved = drag.current?.moved ?? false;
        drag.current = null;
        return moved;
      },
      cancel() {
        drag.current = null;
      },
      get dragging() {
        return drag.current?.moved ?? false;
      },
    }),
    [latest, setDomain, stop],
  );
}

/** A clock that ticks once a second while `live`, for spans that run to
 * now. */
export function useNowWhile(live: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!live) return;
    setNow(Date.now());
    const timer = window.setInterval(() => setNow(Date.now()), 1_000);
    return () => window.clearInterval(timer);
  }, [live]);
  return now;
}

/** The width of an element, kept current. */
export function useWidth(ref: RefObject<HTMLElement | null>): number {
  const [width, setWidth] = useState(0);
  useLayoutEffect(() => {
    const element = ref.current;
    if (!element) return;
    const observer = new ResizeObserver(() => setWidth(element.clientWidth));
    observer.observe(element);
    setWidth(element.clientWidth);
    return () => observer.disconnect();
  }, [ref]);
  return width;
}
