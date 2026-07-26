/**
 * The channel sidebar.
 *
 * Rebuilt wholesale whenever the channel list or unread state changes. That is
 * the right call *here* and nowhere else in the app: a sidebar is tens of rows,
 * so a full rebuild is well under a frame, and the alternative — diffing — buys
 * nothing but bugs. The message list, which is thousands of rows, gets the
 * opposite treatment (see `virtual-list.ts`).
 */

import { ICONS, el, icon, initials, avatarHue, replace } from '../dom.ts';
import { effect } from '../signals.ts';
import type { Channel, Id } from '../protocol.ts';
import type { Store } from '../store.ts';

export type SidebarActions = {
  open: (channel: Id) => void;
  createChannel: () => void;
  browseChannels: () => void;
  newDm: () => void;
  openPreferences: () => void;
};

export function Sidebar(store: Store, actions: SidebarActions): HTMLElement {
  const root = el('nav', { class: 'sidebar', aria: { label: 'Channels' } });

  const header = el(
    'div',
    { class: 'sidebar-header' },
    el('span', { class: 'workspace-name', text: 'TensorChat' }),
    connectionDot(store),
  );

  const channelList = el('div', { class: 'channel-list' });
  const footer = el('div', { class: 'sidebar-footer' });

  root.append(header, channelList, footer);

  effect(() => {
    const channels = store.sortedChannels();
    const current = store.currentChannel();
    // Read the read-state signal so this effect re-runs when badges change.
    store.readStates();
    store.users();
    store.presence();

    const named = channels.filter((c) => c.k === 'public' || c.k === 'private');
    const direct = channels.filter((c) => c.k === 'dm' || c.k === 'group');

    replace(channelList, [
      section('Channels', ICONS.plus, actions.createChannel, [
        ...named.map((c) => channelRow(store, c, current, actions.open)),
        el('button', {
          class: 'channel-row channel-row-action',
          text: 'Browse channels',
          on: { click: actions.browseChannels },
        }),
      ]),
      section('Direct messages', ICONS.plus, actions.newDm, [
        ...direct.map((c) => channelRow(store, c, current, actions.open)),
      ]),
    ]);
  });

  effect(() => {
    const me = store.me();
    replace(footer, [
      me
        ? el(
            'button',
            {
              class: 'me-card',
              on: { click: actions.openPreferences },
              title: 'Preferences',
            },
            avatar(me.id, me.n || me.h, 28),
            el(
              'span',
              { class: 'me-text' },
              el('span', { class: 'me-name', text: me.n || me.h }),
              el('span', { class: 'me-status', text: me.st || `@${me.h}` }),
            ),
          )
        : null,
    ]);
  });

  return root;
}

function section(
  title: string,
  addIcon: string,
  onAdd: () => void,
  rows: (HTMLElement | null)[],
): HTMLElement {
  return el(
    'div',
    { class: 'channel-section' },
    el(
      'div',
      { class: 'section-header' },
      el('span', { class: 'section-title', text: title }),
      el(
        'button',
        { class: 'icon-button', title: `Add ${title.toLowerCase()}`, on: { click: onAdd } },
        icon(addIcon, 14),
      ),
    ),
    ...rows,
  );
}

function channelRow(
  store: Store,
  c: Channel,
  current: Id | null,
  open: (id: Id) => void,
): HTMLElement {
  const unread = store.unread(c.id);
  const hasUnread = (unread?.u ?? 0) > 0;
  const mentions = unread?.mn ?? 0;
  const active = c.id === current;

  const row = el('button', {
    class: [
      'channel-row',
      active ? 'active' : '',
      hasUnread && !active ? 'unread' : '',
    ]
      .filter(Boolean)
      .join(' '),
    on: { click: () => open(c.id) },
    aria: { current: active ? 'page' : 'false' },
  });

  if (c.k === 'public') {
    row.appendChild(icon(ICONS.hash, 15));
  } else if (c.k === 'private') {
    row.appendChild(icon(ICONS.lock, 15));
  } else {
    // A one-to-one DM shows the other person's presence in place of an icon.
    const others = (c.m ?? []).filter((id) => id !== store.me()?.id);
    if (others.length === 1) {
      row.appendChild(presenceDot(store.presenceOf(others[0])));
    } else {
      row.appendChild(el('span', { class: 'group-count', text: String(others.length) }));
    }
  }

  row.appendChild(el('span', { class: 'channel-name', text: store.channelTitle(c) }));

  // A mention badge is louder than plain unread, and replaces it.
  if (mentions > 0) {
    row.appendChild(
      el('span', { class: 'badge', text: mentions > 99 ? '99+' : String(mentions) }),
    );
  }
  return row;
}

function presenceDot(state: string): HTMLElement {
  return el('span', {
    class: `presence presence-${state}`,
    aria: { label: state },
  });
}

function connectionDot(store: Store): HTMLElement {
  const dot = el('span', { class: 'conn-dot' });
  effect(() => {
    const state = store.connection();
    dot.className = `conn-dot conn-${state}`;
    dot.title =
      state === 'open'
        ? 'Connected'
        : state === 'connecting'
          ? 'Connecting…'
          : 'Offline — reconnecting';
  });
  return dot;
}

/** A colored initials avatar. Derived from the id, so it needs no image fetch. */
export function avatar(id: Id, name: string, size = 36): HTMLElement {
  const hue = avatarHue(id);
  return el('span', {
    class: 'avatar',
    text: initials(name),
    style: {
      width: `${size}px`,
      height: `${size}px`,
      fontSize: `${Math.round(size * 0.4)}px`,
      background: `hsl(${hue} 55% 45%)`,
    },
    aria: { hidden: 'true' },
  });
}
