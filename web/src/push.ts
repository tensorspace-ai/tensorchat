/**
 * Web Push subscription management.
 *
 * The in-page notifier ([`notify.ts`]) covers the case where a tab is open. This
 * covers the case that matters more: nothing open at all. The two are switched
 * on by the same control, because "notify me" is one decision, not two.
 *
 * A push carries no payload — the service worker fetches the content from this
 * origin — so all this has to do is hand the browser our VAPID public key and
 * tell the server about the resulting endpoint.
 */

import { api } from './api.ts';

/** Whether this browser can receive push at all. */
export function pushSupported(): boolean {
  return (
    typeof navigator !== 'undefined' &&
    'serviceWorker' in navigator &&
    typeof PushManager !== 'undefined'
  );
}

/**
 * base64url → the `Uint8Array` that `applicationServerKey` requires.
 *
 * The API predates browsers accepting a string here, and still does not.
 */
function decodeKey(b64: string): Uint8Array<ArrayBuffer> {
  const padded = b64.replace(/-/g, '+').replace(/_/g, '/');
  const raw = atob(padded + '='.repeat((4 - (padded.length % 4)) % 4));
  // Backed by an explicit `ArrayBuffer` rather than `Uint8Array.from`, whose
  // type is the wider `ArrayBufferLike` that `BufferSource` will not accept.
  const out = new Uint8Array(new ArrayBuffer(raw.length));
  for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
  return out;
}

async function registration(): Promise<ServiceWorkerRegistration | null> {
  if (!pushSupported()) return null;
  try {
    // `ready` rather than `getRegistration`: the worker may still be
    // installing on a first visit, and subscribing before it is active fails.
    return await navigator.serviceWorker.ready;
  } catch {
    return null;
  }
}

/** Whether this browser currently holds a push subscription. */
export async function isSubscribed(): Promise<boolean> {
  const reg = await registration();
  if (!reg) return false;
  return (await reg.pushManager.getSubscription()) !== null;
}

/**
 * Subscribe this browser and register the endpoint with the server.
 *
 * Resolves false when push is unavailable, the server has it switched off, or
 * the user declined — all of which are ordinary outcomes rather than errors.
 */
export async function subscribe(): Promise<boolean> {
  const reg = await registration();
  if (!reg) return false;

  const { key } = await api.pushKey();
  // No key means the server has push disabled; there is nothing to subscribe to.
  if (!key) return false;

  try {
    // Reuse an existing subscription rather than replacing it: the browser
    // returns the same endpoint anyway, and re-subscribing with a different
    // key would throw.
    const existing = await reg.pushManager.getSubscription();
    const sub =
      existing ??
      (await reg.pushManager.subscribe({
        // Required by Chrome: a push must always result in something the user
        // can see. This client always shows a notification, so it is honest.
        userVisibleOnly: true,
        applicationServerKey: decodeKey(key),
      }));
    await api.pushSubscribe(sub.endpoint);
    return true;
  } catch {
    // Permission denied, or the push service is unreachable.
    return false;
  }
}

/** Unsubscribe this browser and tell the server to forget the endpoint. */
export async function unsubscribe(): Promise<void> {
  const reg = await registration();
  const sub = await reg?.pushManager.getSubscription();
  if (!sub) return;
  // Tell the server first: if the browser-side unsubscribe succeeded and this
  // did not, the server would keep pushing to an endpoint nobody is listening
  // on until the push service returned 410 and pruned it.
  await api.pushUnsubscribe(sub.endpoint).catch(() => {
    /* the endpoint is pruned on the first 410 regardless */
  });
  await sub.unsubscribe().catch(() => {
    /* already gone */
  });
}
