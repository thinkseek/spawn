/** Diagonal wave stagger delays (ms) for the 3x3 Beautiful UI pixel-grid loader. */
export const PIXEL_GRID_DELAYS_MS: readonly number[] = [
  0, 120, 240,
  120, 240, 360,
  240, 360, 480,
];

/** Returns the stagger delay (ms) for cell [0..8]. */
export function pixelCellDelayMs(cellIndex: number): number {
  const safe = ((cellIndex % PIXEL_GRID_DELAYS_MS.length) + PIXEL_GRID_DELAYS_MS.length) % PIXEL_GRID_DELAYS_MS.length;
  return PIXEL_GRID_DELAYS_MS[safe];
}
