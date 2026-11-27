// observer-frog service worker.
// Invariants (F10, F16): network-first shell, network-ONLY /api/*, every caches.match awaited,
// versioned cache name with old caches deleted on activate.
// Paths are relative so the app works at / and behind a /status/ prefix alike.
const CACHE_NAME = 'observer-frog-shell-v55';
const SHELL = ['./', './index.html'];

self.addEventListener('install', (event) => {
  self.skipWaiting();
  event.waitUntil(
    caches.open(CACHE_NAME).then(async (cache) => {
      for (const url of SHELL) {
        try {
          const response = await fetch(url, { cache: 'no-cache' });
          if (response && response.status === 200) await cache.put(url, response);
        } catch (_) {}
      }
    })
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE_NAME).map((k) => caches.delete(k))))
      .then(() => self.clients.claim())
  );
});

self.addEventListener('fetch', (event) => {
  if (event.request.method !== 'GET') return;
  const url = new URL(event.request.url);

  // Network-only for telemetry: a cached reading is a wrong reading.
  if (url.pathname.includes('/api/')) return;

  event.respondWith(
    fetch(event.request)
      .then((networkResponse) => {
        if (networkResponse && networkResponse.status === 200) {
          const clone = networkResponse.clone();
          caches.open(CACHE_NAME).then((cache) => cache.put(event.request, clone));
        }
        return networkResponse;
      })
      .catch(async () => {
        // Must be awaited: an un-awaited caches.match() returns a truthy Promise and the page
        // renders blank instead of falling back.
        const cached = await caches.match(event.request, { ignoreSearch: true });
        if (cached) return cached;
        if (event.request.mode === 'navigate') {
          const shell = (await caches.match('./index.html')) || (await caches.match('./'));
          if (shell) return shell;
        }
        return new Response('Offline', { status: 503, headers: { 'Content-Type': 'text/plain' } });
      })
  );
});
