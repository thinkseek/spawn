// Whether a scorecard's window may still change, in words: the cost reads
// the transcript archive, which a running turn's model calls reach within a
// pass of the Bridge; until every call made before the window's end is
// there, the server says the window is `settling`. Pure, and loadable with
// node alone (relative imports, `import type` only).

/** What the note reads: a card's `settling`. */
export interface SettlingCard {
  settling: boolean;
}

/** The note under a scorecard's window, or null when it has settled. */
export function settlingNote(card: SettlingCard): string | null {
  return card.settling === true
    ? "Still settling: a turn that was running at the window's end may add to its cost."
    : null;
}
