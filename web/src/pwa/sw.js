// The Spawn dashboard's service worker (issue #91 P1b): a shell that opens
// offline, nothing more. Static, no build step, no dependency; `spawn
// serve` embeds it (serve/pwa.rs) and sends it with Cache-Control:
// no-cache, so a new version is picked up on the next load and takes over
// at once.
//
// - Never touched: anything under /api/ (the fleet's state, every write and
//   the live stream), any request that carries an Authorization header, any
//   URL with a token in it, any method but GET, and other origins. Those go
//   to the network untouched (the worker does not answer them), so no
//   token and no API answer ever lands in Cache Storage.
// - The page: a navigation to "/" or "/index.html" is network first, with
//   a short timeout before the last good copy answers; that copy is stored
//   under "/" (the page routes in its hash), and only from those two paths.
//   A navigation anywhere else goes to the network and is not stored.
// - The manifest and the icon: network first, each under its own path.
// - /assets/*: cache first (the build names those by their content hash,
//   so a name never changes meaning), in a cache of their own holding at
//   most ASSETS_MAX entries, the oldest evicted first.
const VERSION = "spawn-shell-v2";
const ASSETS = "spawn-assets-v2";
const ASSETS_MAX = 60;
/** How long a navigation waits for the network before the kept page answers. */
const NAVIGATION_TIMEOUT_MS = 1500;
const PAGE_PATHS = ["/", "/index.html"];
const SHELL_PATHS = ["/manifest.webmanifest", "/icon.svg"];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(VERSION)
      .then((cache) => cache.addAll(["/", ...SHELL_PATHS]))
      .catch(() => undefined)
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((names) =>
        Promise.all(names.filter((name) => name !== VERSION && name !== ASSETS).map((name) => caches.delete(name))),
      )
      .then(() => self.clients.claim()),
  );
});

/** Whether the worker may touch `request` at all. */
function touchable(request, url) {
  if (request.method !== "GET") return false;
  if (url.origin !== self.location.origin) return false;
  if (url.pathname === "/api" || url.pathname.startsWith("/api/")) return false;
  if (url.pathname === "/sw.js") return false;
  if (request.headers.has("Authorization")) return false;
  if (url.searchParams.has("token")) return false;
  return true;
}

/** The network's answer, or a rejection after `ms`. */
function withTimeout(promise, ms) {
  if (!ms) return promise;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("the network is slow")), ms);
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (error) => {
        clearTimeout(timer);
        reject(error);
      },
    );
  });
}

/** Network first under `key`, the kept copy when the network fails or is
 * slower than `timeout` (the network's answer is still stored when it
 * comes). */
async function networkFirst(request, key, timeout) {
  const cache = await caches.open(VERSION);
  const network = fetch(request).then(async (response) => {
    if (response.ok && response.type === "basic") await cache.put(key, response.clone());
    return response;
  });
  try {
    return await withTimeout(network, timeout);
  } catch (error) {
    const kept = await cache.match(key);
    if (kept) return kept;
    return network.catch(() => {
      throw error;
    });
  }
}

/** Keep at most ASSETS_MAX assets, the oldest stored going first. */
async function trim(cache) {
  const keys = await cache.keys();
  for (const key of keys.slice(0, Math.max(0, keys.length - ASSETS_MAX))) await cache.delete(key);
}

async function cacheFirst(request) {
  const cache = await caches.open(ASSETS);
  const kept = await cache.match(request);
  if (kept) return kept;
  const response = await fetch(request);
  if (response.ok && response.type === "basic") {
    await cache.put(request, response.clone());
    await trim(cache);
  }
  return response;
}

self.addEventListener("fetch", (event) => {
  const request = event.request;
  const url = new URL(request.url);
  if (!touchable(request, url)) return;
  if (request.mode === "navigate") {
    // Only the page itself is kept, under "/"; any other navigation is the
    // network's alone.
    if (PAGE_PATHS.includes(url.pathname)) event.respondWith(networkFirst(request, "/", NAVIGATION_TIMEOUT_MS));
    return;
  }
  if (url.pathname.startsWith("/assets/")) {
    event.respondWith(cacheFirst(request));
    return;
  }
  if (SHELL_PATHS.includes(url.pathname)) {
    event.respondWith(networkFirst(request, url.pathname, 0));
  }
});
