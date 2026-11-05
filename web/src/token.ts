/** The serve token as a credential: what `spawn serve` accepts as
 * `SPAWN_SERVE_TOKEN` (visible ASCII, 1 to 512 characters), so it always
 * fits an `Authorization` header. Anything else is no token: a `?token=`
 * with a newline, a NUL or a non-Latin-1 character would make every
 * `fetch` throw before a request is sent. */
export const TOKEN_PATTERN = /^[!-~]{1,512}$/;

/** `text` when it is a usable token, else null. */
export function serveCredential(text: string | null | undefined): string | null {
  return text != null && TOKEN_PATTERN.test(text) ? text : null;
}

/** What the page does with the token it was given and the one it saved. */
export interface TokenChoice {
  /** The token every API read carries, if any. */
  token: string | null;
  /** Save this one in place of the saved value. */
  save: string | null;
  /** Remove the saved value: it was not a token. */
  clear: boolean;
  /** Why a given or saved value was not used, for the page to show. */
  problem: string | null;
}

/** Decide from the page URL's `?token=` (`fromUrl`) and the saved value
 * (`saved`). A usable `?token=` wins and is saved; an unusable one is
 * neither saved nor sent. An unusable saved value is removed. */
export function chooseToken(fromUrl: string | null, saved: string | null): TokenChoice {
  const given = serveCredential(fromUrl);
  const kept = serveCredential(saved);
  const problems: string[] = [];
  if (fromUrl !== null && given === null) {
    problems.push("the ?token= in the address is not a token (visible ASCII, at most 512 characters) and was not saved");
  }
  if (saved !== null && kept === null) {
    problems.push("the saved token was not a token and was removed");
  }
  return {
    token: given ?? kept,
    save: given,
    clear: saved !== null && kept === null && given === null,
    problem: problems.length > 0 ? `${problems.join("; ")}.` : null,
  };
}
