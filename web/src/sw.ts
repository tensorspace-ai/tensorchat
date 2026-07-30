/**
 * Service worker: offline shell and push notifications.
 *
 * Built as its own bundle and served from the site root, because a worker's
 * scope cannot be broader than its own URL — one at `/assets/sw.js` could not
 * control `/`.
 *
 * ## Caching
 *
 * Two rules, matching how the assets are actually served:
 *
 * * `/assets/*` is content-hashed and served `immutable`, so it is **cache
 *   first**. A hit never touches the network; a miss is fetched and kept.
 * * Everything else — navigations especially — is **network first**, falling
 *   back to the cached shell only when the network fails. A deploy must be
 *   picked up on the next load, and `index.html` is the only file that names
 *   the hashed bundles; serving a stale one from cache would pin a returning
 *   user to the previous deployment.
 *
 * `/api` and `/ws` are never cached. Chat state is live by definition, and a
 * stale reply would be worse than an error.
 *
 * ## Push
 *
 * Pushes carry **no payload**. The server sends an empty VAPID-signed message
 * and this worker fetches the details from `/api/me/notifications` on the same
 * origin, with the session cookie. Message bodies therefore never pass through
 * Google's or Mozilla's push infrastructure — the only thing that leaves the
 * server is "something happened for this subscription".
 */

/// <reference lib="webworker" />

// Makes this a module, so the `self` below shadows the `WorkerGlobalScope` one
// the lib declares globally instead of colliding with it. A service worker's
// global really is a `ServiceWorkerGlobalScope`; TypeScript has no way to know
// that from the file alone.
export {};

declare const self: ServiceWorkerGlobalScope;

/**
 * Cache name, rebuilt into the bundle at build time from the asset hashes.
 * Changing it is what retires the previous deployment's cache in `activate`.
 */
declare const __CACHE_NAME__: string;
/** The shell to precache: `index.html` plus this build's hashed assets. */
declare const __PRECACHE__: string[];

const CACHE = __CACHE_NAME__;

self.addEventListener('install', (event) => {
  event.waitUntil(
    (async () => {
      const cache = await caches.open(CACHE);
      // `reload` so a precache never picks up a stale HTTP-cached copy of the
      // very files it exists to serve offline.
      await cache.addAll(__PRECACHE__.map((u) => new Request(u, { cache: 'reload' })));
      // Take over at once rather than waiting for every tab to close. The
      // alternative leaves a deploy half-applied for as long as one tab lives.
      await self.skipWaiting();
    })(),
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    (async () => {
      for (const key of await caches.keys()) {
        if (key !== CACHE) await caches.delete(key);
      }
      await self.clients.claim();
    })(),
  );
});

/** Whether a request is app data rather than a static asset. */
function isLive(url: URL): boolean {
  return url.pathname.startsWith('/api/') || url.pathname === '/ws' || url.pathname === '/healthz';
}

self.addEventListener('fetch', (event) => {
  const req = event.request;
  // Only GET is cacheable, and only our own origin is ours to cache.
  if (req.method !== 'GET') return;
  const url = new URL(req.url);
  if (url.origin !== self.location.origin || isLive(url)) return;

  // Content-hashed and immutable: a cache hit is always correct.
  if (url.pathname.startsWith('/assets/')) {
    event.respondWith(
      (async () => {
        const hit = await caches.match(req);
        if (hit) return hit;
        const res = await fetch(req);
        if (res.ok) void (await caches.open(CACHE)).put(req, res.clone());
        return res;
      })(),
    );
    return;
  }

  // Everything else: network first, cache as the offline fallback.
  event.respondWith(
    (async () => {
      try {
        const res = await fetch(req);
        if (res.ok && res.type === 'basic') {
          void (await caches.open(CACHE)).put(req, res.clone());
        }
        return res;
      } catch {
        const hit = await caches.match(req);
        if (hit) return hit;
        // A navigation with nothing cached for this exact URL still gets the
        // shell: the app is a single page and routes on the fragment.
        if (req.mode === 'navigate') {
          const shell = await caches.match('/index.html');
          if (shell) return shell;
        }
        throw new Error('offline');
      }
    })(),
  );
});

type PushItem = {
  /** Channel id, for the permalink. */
  ch: string;
  /** Message id. */
  id: string;
  title: string;
  body: string;
};

self.addEventListener('push', (event) => {
  event.waitUntil(
    (async () => {
      // If a window is already focused the user is looking at the app, and the
      // in-page notifier has it covered. A push notification on top would be a
      // duplicate.
      const clients = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
      if (clients.some((c) => c.visibilityState === 'visible' && c.focused)) return;

      let items: PushItem[] = [];
      try {
        const res = await fetch('/api/me/notifications', { credentials: 'same-origin' });
        if (res.ok) items = (await res.json()) as PushItem[];
      } catch {
        // Offline, or the session has expired. Fall through to the generic
        // notification below rather than showing nothing at all — the push
        // already told us something happened.
      }

      if (items.length === 0) {
        await self.registration.showNotification('TensorChat', {
          body: 'You have a new message.',
          icon: '/icons/icon-192.png',
          badge: '/icons/icon-192-maskable.png',
          tag: 'tc-generic',
        });
        return;
      }

      // Collapse a burst into one notification per conversation, so returning
      // after an hour away does not produce a wall of them. `renotify` lets a
      // later message in the same conversation still alert.
      const seen = new Set<string>();
      for (const item of items) {
        if (seen.has(item.ch)) continue;
        seen.add(item.ch);
        await self.registration.showNotification(item.title, {
          body: item.body,
          icon: '/icons/icon-192.png',
          badge: '/icons/icon-192-maskable.png',
          tag: `tc-${item.ch}`,
          renotify: true,
          data: { url: `/#/c/${item.ch}/${item.id}` },
        } as NotificationOptions);
      }
    })(),
  );
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const target = (event.notification.data as { url?: string } | undefined)?.url ?? '/';
  event.waitUntil(
    (async () => {
      const clients = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
      // Reuse an open tab rather than piling up windows. Navigating it to the
      // permalink is what puts the reader on the actual message.
      for (const client of clients) {
        if (new URL(client.url).origin === self.location.origin) {
          await client.focus();
          if ('navigate' in client) await client.navigate(target);
          return;
        }
      }
      await self.clients.openWindow(target);
    })(),
  );
});
