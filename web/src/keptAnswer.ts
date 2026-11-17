/* Which answer a polled page shows while its query changes (GitHub #61): a
 * new window or scope must not blank the page, but a failed read of the new
 * query must not show the previous query's answer as if it were the new
 * one's. Pure, tested in web/test/keptAnswer.test.ts. */

/** `data` and `error` are the poll's for the current key (data null while
 * the new key loads or after its read failed); `last` is the latest answer
 * of any key. Shown: the current answer; else, while the new key loads with
 * no error, the last one, marked `refreshing`; after an error with no
 * answer for this key, nothing (the page says it could not load). */
export function keptAnswer<T>(
  data: T | null,
  error: string | null,
  last: T | null,
): { shown: T | null; refreshing: boolean } {
  if (data !== null) return { shown: data, refreshing: false };
  if (error !== null) return { shown: null, refreshing: false };
  return { shown: last, refreshing: last !== null };
}
