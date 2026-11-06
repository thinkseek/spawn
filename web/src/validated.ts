// Reads the server may answer "not modified": the page keeps what it last
// read at each URL with its tag (`ETag`) and sends the tag back on the next
// read (`If-None-Match`); a 304 gives the same value again, the very same
// object, so React has nothing to render. Every read also says it accepts
// an answer up to a second old (`Cache-Control: max-age=1`): the server
// then computes a board once per second for every open page instead of
// once per page (crates/spawn-cli/src/serve/answers.rs). The page polls
// every two seconds or more, so it never reads an answer older than its
// poll. Pure apart from `fetch`, so node's test runner checks it.

/** What a page polls with: the request headers of every read. */
export const SHARED_FOR = "max-age=1";

/** URLs kept at most; past it the oldest read goes. */
const KEPT_URLS = 128;

interface Kept {
  etag: string;
  raw: unknown;
  normalized: unknown;
  normalize: (raw: unknown) => unknown;
}

export type FetchLike = (url: string, init: RequestInit) => Promise<Response>;

export interface ReadOptions {
  signal: AbortSignal;
  /** Headers of every API read (the token). */
  headers: Record<string, string>;
  /** The error of a failed read, from its status and the server's reason. */
  fail: (status: number, detail: string) => Error;
}

/** The last answer read at each URL, with its tag. */
export class Validated {
  private readonly kept = new Map<string, Kept>();
  private readonly fetcher: FetchLike;

  constructor(fetcher: FetchLike = (url, init) => fetch(url, init)) {
    this.fetcher = fetcher;
  }

  /** GET `url` and normalize it, or, answered 304, the value read last
   * time (the same object). A failure throws `fail`'s error, with the
   * server's `error` when it sent one. */
  async read<T>(url: string, normalize: (raw: unknown) => T, options: ReadOptions): Promise<T> {
    const kept = this.kept.get(url);
    const headers: Record<string, string> = {
      ...options.headers,
      "Cache-Control": SHARED_FOR,
    };
    if (kept) headers["If-None-Match"] = kept.etag;
    const response = await this.fetcher(url, {
      signal: options.signal,
      cache: "no-store",
      headers,
    });
    if (response.status === 304 && kept) {
      // Read again by another caller with another reading of the same
      // answer: normalize it its own way.
      if (kept.normalize !== normalize) return normalize(kept.raw);
      this.touch(url, kept);
      return kept.normalized as T;
    }
    if (!response.ok) {
      let detail = "";
      try {
        const body: unknown = await response.json();
        const error =
          body !== null && typeof body === "object" ? (body as Record<string, unknown>).error : null;
        if (typeof error === "string") detail = error;
      } catch {
        // Not JSON: the status says enough.
      }
      throw options.fail(response.status, detail);
    }
    const raw: unknown = await response.json();
    const normalized = normalize(raw);
    const etag = response.headers.get("ETag");
    if (etag) {
      this.touch(url, { etag, raw, normalized, normalize });
    } else {
      this.kept.delete(url);
    }
    return normalized;
  }

  /** How many URLs are kept (for tests). */
  get size(): number {
    return this.kept.size;
  }

  private touch(url: string, kept: Kept) {
    this.kept.delete(url);
    this.kept.set(url, kept);
    while (this.kept.size > KEPT_URLS) {
      const oldest = this.kept.keys().next().value;
      if (oldest === undefined) break;
      this.kept.delete(oldest);
    }
  }
}

/** Bytes of `text` in UTF-8, as the server counts a text's length. */
export function utf8Bytes(text: string): number {
  return new TextEncoder().encode(text).length;
}

/** Whether a text the server cut to its first lines (`excerpt`) is shorter
 * than the whole (`bytes`, its `<field>_bytes`). */
export function isCut(excerpt: string | null, bytes: number | null): boolean {
  if (bytes === null) return false;
  return utf8Bytes(excerpt ?? "") < bytes;
}

/** A length in bytes as a person reads it: `812 B`, `4.2 KB`, `1.3 MB`. */
export function formatBytes(bytes: number): string {
  if (bytes < 1000) return `${bytes} B`;
  if (bytes < 1_000_000) return `${(bytes / 1000).toFixed(bytes < 10_000 ? 1 : 0)} KB`;
  return `${(bytes / 1_000_000).toFixed(1)} MB`;
}

/** Whether the page is hidden (another tab, a locked phone): its polls
 * wait until it shows again. */
export function pageHidden(doc: { visibilityState?: string } | undefined): boolean {
  return doc?.visibilityState === "hidden";
}
