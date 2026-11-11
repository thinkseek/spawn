// Whether a scorecard's tokens reach back to its windows' starts, in words:
// the cost reads the transcript archive, and a window that starts before
// the archive's oldest model call has no tokens from before it. The server
// says so (`tokens_short`, `tokens_from_ms`) rather than count a short cost
// as the whole. Pure, and loadable with node alone (relative imports).

import { localDateTime } from "./format.ts";

/** What the note reads: a card's `tokens_short` and `tokens_from_ms`. */
export interface TokenReachCard {
  tokens_short: "window" | "previous" | null;
  tokens_from_ms: number | null;
}

/** A server card's fields as the note reads them: anything but `"window"`
 * or `"previous"` reads as whole, and a time that is not a finite number as
 * none. */
export function tokenReachOf(raw: Record<string, unknown>): TokenReachCard {
  const short = raw.tokens_short;
  const from = raw.tokens_from_ms;
  return {
    tokens_short: short === "window" || short === "previous" ? short : null,
    tokens_from_ms: typeof from === "number" && Number.isFinite(from) ? from : null,
  };
}

/** The note under a scorecard's window, or null when the archive reaches
 * back past both windows' starts. */
export function tokenReachNote(card: TokenReachCard): string | null {
  const from = card.tokens_from_ms;
  if (card.tokens_short === "window") {
    return from === null
      ? "Tokens not counted: the transcript archive holds no model call."
      : `Tokens counted from ${localDateTime(from)} only: the transcript archive begins there, after this window's start.`;
  }
  if (card.tokens_short === "previous" && from !== null) {
    return `Previous window's tokens counted from ${localDateTime(from)} only: the transcript archive begins there, after that window's start.`;
  }
  return null;
}
