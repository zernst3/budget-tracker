/**
 * Service Worker for Budget Tracker PWA
 *
 * Implements app-shell caching ONLY (thin service worker per SPIRIT-ROBUSTNESS-1).
 * NOT offline-first: the budget app is server-backed and requires the database.
 * The service worker's only job is to cache the immutable app shell (HTML, CSS, JS)
 * so cold starts are faster on repeat visits. Dynamic content (API responses) are
 * never cached; the network is the source of truth.
 *
 * Cache strategy: cache-first for immutable assets (versioned JS bundles, fonts);
 * network-first for the HTML shell (always fetch fresh, but use stale cache if offline).
 */

const CACHE_NAME = 'budget-tracker-v1-shell';
const SHELL_ASSETS = [
  '/',
  '/favicon.ico',
  '/manifest.json',
];

/**
 * Install: pre-cache the app shell (HTML, manifest, essential assets).
 * Immutable versioned bundles are cached by the browser automatically.
 */
self.addEventListener('install', (event) => {
  event.waitUntil(
    caches.open(CACHE_NAME).then((cache) => {
      // Pre-cache the shell assets (HTML + manifest).
      // The dioxus-generated JS bundle is automatically versioned and cached
      // by the browser cache headers; we cache the entry point here.
      return cache.addAll(SHELL_ASSETS).catch((err) => {
        // Offline install is a non-fatal condition; skip pre-caching if the
        // network is unavailable. The user can still visit the site online,
        // and the next successful visit will cache the shell.
        console.warn('Service worker install: offline, skipping pre-cache', err);
      });
    })
  );
  // Force the new worker to activate immediately (no wait for clients).
  self.skipWaiting();
});

/**
 * Activate: clean up old cache versions. This runs once when the new SW is activated.
 */
self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches.keys().then((cacheNames) => {
      return Promise.all(
        cacheNames.map((cacheName) => {
          if (cacheName !== CACHE_NAME) {
            console.log('Service worker: deleting old cache', cacheName);
            return caches.delete(cacheName);
          }
        })
      );
    })
  );
  self.clients.claim();
});

/**
 * Fetch: network-first for HTML shell; cache-first for versioned assets.
 *
 * Requests for the HTML shell (/) go network-first: fetch from the server,
 * but use the stale cache as a fallback if offline. This ensures the user
 * sees fresh HTML on each visit (including any new authentication state),
 * but can still view the shell offline if they've visited before.
 *
 * Versioned assets (JS bundles with hashes in the name) are cache-first:
 * the browser's default cache headers already handle these as immutable,
 * so the service worker just lets them through.
 *
 * API responses (data fetches) are NEVER cached. They always go to the network.
 */
self.addEventListener('fetch', (event) => {
  const { request } = event;

  // Only intercept GET requests (no POSTs, PUTs, DELETEs).
  if (request.method !== 'GET') {
    return;
  }

  // Skip external requests (cross-origin resources, third-party APIs).
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) {
    return;
  }

  // API routes (starting with /api, /_server, etc.) always go to the network.
  // These are dynamic and must never be cached.
  if (
    url.pathname.startsWith('/api') ||
    url.pathname.startsWith('/_') ||
    url.pathname.startsWith('/auth') ||
    url.pathname.startsWith('/login') ||
    url.pathname.startsWith('/logout')
  ) {
    return event.respondWith(fetch(request));
  }

  // HTML shell: network-first (fetch fresh, fall back to cache if offline).
  if (url.pathname === '/' || url.pathname.endsWith('.html')) {
    return event.respondWith(
      fetch(request)
        .then((response) => {
          // Cache a successful response (even if stale later).
          if (response.ok) {
            const cache = caches.open(CACHE_NAME);
            cache.then((c) => c.put(request, response.clone()));
          }
          return response;
        })
        .catch(() => {
          // Network failed; try the cache.
          return caches.match(request);
        })
    );
  }

  // Versioned static assets (JS, CSS, fonts, images): cache-first.
  // The browser's default cache headers already mark these as immutable.
  event.respondWith(
    caches.match(request).then((cached) => {
      if (cached) {
        return cached;
      }
      return fetch(request).then((response) => {
        // Cache successful responses for future offline access.
        if (response.ok) {
          const cache = caches.open(CACHE_NAME);
          cache.then((c) => c.put(request, response.clone()));
        }
        return response;
      });
    })
  );
});
