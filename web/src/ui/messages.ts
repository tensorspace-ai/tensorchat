/**
 * The message list.
 *
 * This is the only part of the UI with a real performance budget: a channel can
 * hold a hundred thousand messages and must still scroll at 60fps and jump to
 * "oldest unread" instantly. Three things make that work:
 *
 * 1. **Windowing.** `VirtualList` mounts only the visible slice plus a little
 *    overscan, so DOM size is bounded by the viewport, not by history.
 * 2. **Recycling.** Rows that leave the window are reused rather than rebuilt.
 * 3. **Anchored prepends.** Loading older history inserts above the viewport;
 *    the scroll position is restored against a stable anchor so the content the
 *    reader is looking at does not move.
 *
 * The row *contents* are still built with plain DOM calls — see `richtext.ts`
 * for why nothing here goes through `innerHTML`.
 */

import { ICONS, el, formatDayLabel, formatFullTime, formatSize, formatTime, icon, replace, sameDay } from '../dom.ts';
import { effect } from '../signals.ts';
import { VirtualList } from '../virtual-list.ts';
import { fileUrl } from '../api.ts';
import { idToDate } from '../protocol.ts';
import { isEmojiOnly, renderBody } from '../richtext.ts';
import type { Attachment, Id, Message } from '../protocol.ts';
import type { Store } from '../store.ts';
import { avatar } from './sidebar.ts';

/** Consecutive messages from one author within this window share a header. */
const GROUP_WINDOW_MS = 5 * 60 * 1000;

/** Rows are a mix of messages, day separators, and pending echoes. */
type Row =
  | { kind: 'day'; key: string; date: Date }
  | { kind: 'message'; key: string; m: Message; grouped: boolean }
  | { kind: 'pending'; key: string; body: string; failed: boolean };

export type MessageActions = {
  react: (message: Id, emoji: string, on: boolean) => void;
  setPin: (message: Id, on: boolean) => void;
  openThread: (root: Id) => void;
  edit: (message: Id, body: string) => void;
  remove: (message: Id) => void;
  loadOlder: () => void;
  openMention: (user: Id) => void;
  openChannel: (channel: Id) => void;
};

export function MessageList(store: Store, actions: MessageActions): HTMLElement {
  const content = el('div', { class: 'message-content' });
  const viewport = el('div', { class: 'message-viewport', role: 'log' }, content);

  const list = new VirtualList<Row>({
    viewport,
    content,
    // Tuned to a typical one-line grouped message; wrong guesses are corrected
    // by measurement, this only affects the initial scrollbar length.
    estimateHeight: 44,
    overscan: 6,
    key: (row) => row.key,
    renderRow: (row) => renderRow(store, actions, row),
    // Recycled rows are rebuilt in place rather than replaced, so the element
    // (and its scroll-anchoring identity) survives.
    updateRow: (element, row) => {
      const fresh = renderRow(store, actions, row);
      element.className = fresh.className;
      replace(element, [...fresh.childNodes]);
    },
  });

  // Load older history when the reader approaches the top.
  let loadArmed = true;
  viewport.addEventListener('scroll', () => {
    if (viewport.scrollTop < 400) {
      if (loadArmed) {
        loadArmed = false;
        actions.loadOlder();
      }
    } else {
      loadArmed = true;
    }
  });

  effect(() => {
    const channel = store.currentChannel();
    if (!channel) {
      list.setItems([]);
      return;
    }
    const log = store.log(channel);
    // Subscribe to this channel's log version; any mutation re-runs the effect.
    log.version();
    store.users();
    store.pending();
    // Pins live outside the message log, so the log's version counter does not
    // cover them; subscribe separately or a pin never repaints its message.
    store.pins();

    const rows = buildRows(store, channel, log.messages);
    const wasPinned = list.isPinnedToBottom();
    list.setItems(rows);
    if (wasPinned) list.scrollToBottom();
  });

  // Re-arm the loader whenever the channel changes.
  effect(() => {
    store.currentChannel();
    loadArmed = true;
    list.scrollToBottom();
  });

  return viewport;
}

/** Flatten a message log into renderable rows, inserting day separators. */
function buildRows(store: Store, channel: Id, messages: Message[]): Row[] {
  const rows: Row[] = [];
  let previous: Message | undefined;
  let previousDate: Date | undefined;

  for (const m of messages) {
    // Thread replies live in the thread pane, not the channel scroll.
    if (m.th) continue;
    const date = idToDate(m.id);

    if (!previousDate || !sameDay(previousDate, date)) {
      rows.push({ kind: 'day', key: `day-${date.toDateString()}`, date });
      previous = undefined;
    }

    const grouped =
      !!previous &&
      previous.au === m.au &&
      !previous.del &&
      !m.del &&
      date.getTime() - idToDate(previous.id).getTime() < GROUP_WINDOW_MS;

    rows.push({ kind: 'message', key: m.id, m, grouped });
    previous = m;
    previousDate = date;
  }

  // Optimistic echoes sit at the bottom until the server confirms them.
  for (const p of store.pending().values()) {
    if (p.channel !== channel || p.threadRoot) continue;
    rows.push({ kind: 'pending', key: `pending-${p.nonce}`, body: p.body, failed: p.failed });
  }
  return rows;
}

function renderRow(store: Store, actions: MessageActions, row: Row): HTMLElement {
  if (row.kind === 'day') {
    return el(
      'div',
      { class: 'day-sep' },
      el('span', { class: 'day-label', text: formatDayLabel(row.date) }),
    );
  }
  if (row.kind === 'pending') {
    return el(
      'div',
      { class: `message pending${row.failed ? ' failed' : ''}` },
      el('div', { class: 'message-gutter' }),
      el(
        'div',
        { class: 'message-main' },
        el('div', { class: 'message-body', text: row.body }),
        row.failed ? el('span', { class: 'send-failed', text: 'Not delivered' }) : null,
      ),
    );
  }
  return renderMessage(store, actions, row.m, row.grouped);
}

export function renderMessage(
  store: Store,
  actions: MessageActions,
  m: Message,
  grouped: boolean,
): HTMLElement {
  const author = store.user(m.au);
  const name = author ? author.n || author.h : 'unknown';
  const date = idToDate(m.id);
  const meId = store.me()?.id;
  const mentionsMe = !!(meId && m.mn?.includes(meId));

  const pinned = store.isPinned(m.ch, m.id);

  const root = el('div', {
    class: [
      'message',
      grouped ? 'grouped' : '',
      m.del ? 'deleted' : '',
      mentionsMe ? 'mentions-me' : '',
      pinned ? 'pinned' : '',
    ]
      .filter(Boolean)
      .join(' '),
    data: { id: m.id },
  });

  // The gutter holds either the avatar (first of a group) or a hover timestamp.
  const gutter = el('div', { class: 'message-gutter' });
  if (grouped) {
    gutter.appendChild(el('span', { class: 'hover-time', text: formatTime(date) }));
  } else {
    gutter.appendChild(avatar(m.au, name));
  }

  const main = el('div', { class: 'message-main' });
  // Inside `main`, not on the row: `.message` is a flex row, so a sibling of
  // the gutter would land beside the avatar instead of above the text.
  if (pinned) {
    main.appendChild(
      el('div', { class: 'pinned-flag' }, icon(ICONS.pin, 11), el('span', { text: 'Pinned' })),
    );
  }
  if (!grouped) {
    main.appendChild(
      el(
        'div',
        { class: 'message-head' },
        el('span', { class: 'author', text: name }),
        author?.bot ? el('span', { class: 'bot-tag', text: 'APP' }) : null,
        el('time', { class: 'timestamp', text: formatTime(date), title: formatFullTime(date) }),
      ),
    );
  }

  if (m.del) {
    main.appendChild(el('div', { class: 'message-body tombstone', text: 'This message was deleted' }));
  } else {
    const body = el('div', {
      class: `message-body${isEmojiOnly(m.b) ? ' jumbo' : ''}`,
    });
    body.appendChild(
      renderBody(m.b, {
        userByHandle: (h) => [...store.users().values()].find((u) => u.h === h),
        channelByName: (n) => [...store.channels().values()].find((c) => c.n === n)?.id,
        meId,
        onMention: actions.openMention,
        onChannel: actions.openChannel,
      }),
    );
    if (m.ed) body.appendChild(el('span', { class: 'edited', text: '(edited)' }));
    main.appendChild(body);

    if (m.at?.length) main.appendChild(attachments(m.at));
    if (m.rx?.length) main.appendChild(reactions(m, actions));
    if (m.rc) {
      main.appendChild(
        el(
          'button',
          { class: 'thread-link', on: { click: () => actions.openThread(m.id) } },
          icon(ICONS.thread, 13),
          el('span', { text: `${m.rc} ${m.rc === 1 ? 'reply' : 'replies'}` }),
        ),
      );
    }
    root.appendChild(hoverActions(store, actions, m));
  }

  root.append(gutter, main);
  return root;
}

function attachments(list: Attachment[]): HTMLElement {
  const wrap = el('div', { class: 'attachments' });
  for (const a of list) {
    if (a.mt.startsWith('image/')) {
      const img = el('img', {
        class: 'attachment-image',
        src: fileUrl(a.id),
        alt: a.n,
        loading: 'lazy',
        decoding: 'async',
      });
      // Reserve the box before the bytes arrive, so loading an image does not
      // shove the conversation around. The server reads these from the file
      // header at upload time precisely for this.
      if (a.w && a.hh) {
        img.width = a.w;
        img.height = a.hh;
        img.style.aspectRatio = `${a.w} / ${a.hh}`;
      }
      wrap.appendChild(el('a', { href: fileUrl(a.id), target: '_blank', rel: 'noopener' }, img));
    } else {
      wrap.appendChild(
        el(
          'a',
          { class: 'attachment-file', href: fileUrl(a.id), target: '_blank', rel: 'noopener' },
          icon(ICONS.paperclip, 15),
          el(
            'span',
            { class: 'attachment-meta' },
            el('span', { class: 'attachment-name', text: a.n }),
            el('span', { class: 'attachment-size', text: formatSize(a.sz) }),
          ),
        ),
      );
    }
  }
  return wrap;
}

function reactions(m: Message, actions: MessageActions): HTMLElement {
  const wrap = el('div', { class: 'reactions' });
  for (const r of m.rx ?? []) {
    wrap.appendChild(
      el(
        'button',
        {
          class: `reaction${r.me ? ' mine' : ''}`,
          title: r.e,
          on: { click: () => actions.react(m.id, r.e, !r.me) },
        },
        el('span', { class: 'reaction-emoji', text: r.e }),
        el('span', { class: 'reaction-count', text: String(r.c) }),
      ),
    );
  }
  return wrap;
}

/** Quick reactions offered on hover. */
const QUICK = ['👍', '🎉', '👀', '❤️'];

function hoverActions(store: Store, actions: MessageActions, m: Message): HTMLElement {
  const mine = store.me()?.id === m.au;
  const bar = el('div', { class: 'message-actions' });

  for (const emoji of QUICK) {
    bar.appendChild(
      el('button', {
        class: 'action',
        text: emoji,
        title: `React ${emoji}`,
        on: {
          click: () => {
            const already = m.rx?.some((r) => r.e === emoji && r.me);
            actions.react(m.id, emoji, !already);
          },
        },
      }),
    );
  }

  bar.appendChild(
    el(
      'button',
      { class: 'action', title: 'Reply in thread', on: { click: () => actions.openThread(m.id) } },
      icon(ICONS.thread, 14),
    ),
  );

  // A pin belongs to the channel, not to the author, so anyone in the channel
  // gets this — unlike edit and delete below.
  const pinned = store.isPinned(m.ch, m.id);
  bar.appendChild(
    el(
      'button',
      {
        class: `action${pinned ? ' active' : ''}`,
        title: pinned ? 'Unpin' : 'Pin to channel',
        on: { click: () => actions.setPin(m.id, !pinned) },
      },
      icon(ICONS.pin, 14),
    ),
  );

  if (mine) {
    bar.appendChild(
      el(
        'button',
        { class: 'action', title: 'Edit', on: { click: () => actions.edit(m.id, m.b) } },
        icon(ICONS.edit, 14),
      ),
    );
    bar.appendChild(
      el(
        'button',
        {
          class: 'action danger',
          title: 'Delete',
          on: {
            click: () => {
              if (confirm('Delete this message?')) actions.remove(m.id);
            },
          },
        },
        icon(ICONS.trash, 14),
      ),
    );
  }
  return bar;
}
