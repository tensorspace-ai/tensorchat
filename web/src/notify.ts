/**
 * Desktop notifications.
 *
 * Entirely client-side. Real Web Push — a service worker, VAPID keys, a
 * subscription table, a server that can wake a device that has no page open —
 * is a different and much larger feature. This one covers the case that
 * actually bites: the tab is open behind something else and you miss a message
 * addressed to you.
 *
 * The bar for interrupting someone is deliberately high. A notification fires
 * only for a message you could not have already seen, that was addressed to you
 * (a mention or a direct message), in a channel you have not muted — with the
 * one exception that a mention pierces a mute, matching how badges behave.
 */

import type { Id, ServerFrame } from './protocol.ts';
import type { Store } from './store.ts';

const PREF_KEY = 'tc_notifications';

/** Collapse a burst in one channel into a single notification. */
const TAG_PREFIX = 'tc-channel-';

export type Notifier = {
  /** Whether the browser supports notifications at all. */
  supported: boolean;
  /** Whether the user has switched them on *and* granted permission. */
  enabled: () => boolean;
  /**
   * Turn them on or off. Enabling prompts for permission, which browsers only
   * allow from a user gesture — so this must be called from a click.
   *
   * Resolves with whether they ended up enabled.
   */
  setEnabled: (on: boolean) => Promise<boolean>;
  /** Offer a frame. Non-qualifying frames are ignored. */
  consider: (frame: ServerFrame) => void;
};

export function createNotifier(store: Store, openChannel: (channel: Id) => void): Notifier {
  const supported = typeof Notification !== 'undefined';
  let wanted = localStorage.getItem(PREF_KEY) === 'on';

  const enabled = () => supported && wanted && Notification.permission === 'granted';

  const setEnabled = async (on: boolean): Promise<boolean> => {
    if (!supported || !on) {
      wanted = false;
      localStorage.setItem(PREF_KEY, 'off');
      return false;
    }
    // `requestPermission` resolves immediately with the existing answer if the
    // user has already decided, so this is safe to call unconditionally.
    const permission =
      Notification.permission === 'default'
        ? await Notification.requestPermission()
        : Notification.permission;
    wanted = permission === 'granted';
    localStorage.setItem(PREF_KEY, wanted ? 'on' : 'off');
    return wanted;
  };

  const consider = (frame: ServerFrame): void => {
    if (!enabled() || !('msg' in frame)) return;
    const m = frame.msg.m;

    const meId = store.me()?.id;
    if (!meId || m.au === meId) return;

    // Already on screen: the message is in the channel you are looking at, in a
    // focused window. Interrupting there is just noise.
    const looking = !document.hidden && store.currentChannel() === m.ch;
    if (looking) return;

    const channel = store.channels().get(m.ch);
    const direct = channel?.k === 'dm' || channel?.k === 'group';
    const mentioned = !!m.mn?.includes(meId);
    if (!direct && !mentioned) return;

    // A mention pierces a mute; ambient DM traffic in a muted conversation does
    // not. Same rule the unread badges follow.
    if (store.isMuted(m.ch) && !mentioned) return;

    const title = channel
      ? channel.k === 'public'
        ? `#${store.channelTitle(channel)}`
        : store.channelTitle(channel)
      : 'TensorChat';

    try {
      const note = new Notification(title, {
        body: `${store.userName(m.au)}: ${preview(m.b)}`,
        tag: TAG_PREFIX + m.ch,
      });
      note.onclick = () => {
        window.focus();
        openChannel(m.ch);
        note.close();
      };
    } catch {
      // Some browsers throw rather than reject when notifications are blocked
      // by policy. A failed notification must never take the socket with it.
    }
  };

  return { supported, enabled, setEnabled, consider };
}

/** One line of body text, short enough for a notification bubble. */
function preview(body: string): string {
  const flat = body.replace(/\s+/g, ' ').trim();
  return flat.length > 140 ? `${flat.slice(0, 139)}…` : flat;
}
