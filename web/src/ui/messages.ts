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
import { effect, signal } from '../signals.ts';
import { VirtualList } from '../virtual-list.ts';
import { fileUrl } from '../api.ts';
import { idToDate } from '../protocol.ts';
import { isEmojiOnly, renderBody } from '../richtext.ts';
import type { Attachment, Id, Message } from '../protocol.ts';
import type { Store } from '../store.ts';
import { openEmojiPicker } from './emoji-picker.ts';
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
  setSaved: (message: Id, on: boolean) => void;
  copyLink: (channel: Id, message: Id) => void;
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

  // Which anchor this list has already scrolled to, so paging older inside a
  // window does not yank the viewport back to the anchor on every page.
  let scrolledTo: Id | null = null;

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
    // Pins and saves live outside the message log, so the log's version counter
    // does not cover them; subscribe separately or a pin never repaints its
    // message.
    store.pins();
    store.savedIds();
    store.highlight();

    const rows = buildRows(store, channel, log.messages);
    const wasPinned = list.isPinnedToBottom();
    list.setItems(rows);
    // Everything above can change what an *already visible* row should show —
    // a reaction, a pin, an edit, the inline editor opening — without changing
    // which rows exist. `setItems` alone leaves those rows mounted and stale.
    list.refresh();

    // A window loaded around some older message opens *on* that message rather
    // than at the bottom, which for a historical excerpt would be an arbitrary
    // place to land.
    const anchor = log.anchor;
    if (anchor && anchor !== scrolledTo) {
      const at = rows.findIndex((r) => r.kind === 'message' && r.m.id === anchor);
      if (at >= 0) {
        scrolledTo = anchor;
        list.scrollToIndex(at, 'start');
        return;
      }
    }
    if (!anchor) scrolledTo = null;
    if (wasPinned) list.scrollToBottom();
  });

  // Re-arm the loader whenever the channel changes.
  effect(() => {
    store.currentChannel();
    loadArmed = true;
    // An editor left open in a channel you have navigated away from would keep
    // its session alive and reopen on return, over a message the author may no
    // longer even mean to change.
    cancelEdit();
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

// -- Inline editing ---------------------------------------------------------

/**
 * Which message is open in the editor.
 *
 * A signal, so opening or closing the editor repaints the row through the same
 * effect that repaints everything else. The *text* being typed deliberately is
 * not a signal — see [`session`].
 */
const editingId = signal<Id | null>(null);

/**
 * The in-progress edit, held outside the reactive graph on purpose.
 *
 * The row this lives in is recycled by the virtual list: a message arriving, a
 * reaction landing, or a scroll can rebuild it at any moment. So the text has
 * to survive outside the DOM — but it must not be a *signal*, or every
 * keystroke would repaint the row and take the caret with it. Instead the
 * textarea writes here on input, and a rebuilt textarea reads back from here,
 * caret position included.
 */
let session: { id: Id; text: string; start: number; end: number } | null = null;

export function beginEdit(id: Id, body: string): void {
  session = { id, text: body, start: body.length, end: body.length };
  editingId.set(id);
}

export function cancelEdit(): void {
  session = null;
  editingId.set(null);
}

export function isEditing(): boolean {
  return editingId.peek() !== null;
}

/**
 * The editor that replaces a message body in place.
 *
 * Keyboard contract matches the composer, because the muscle memory is the
 * same: Enter commits, Shift+Enter adds a line, Escape abandons.
 */
function messageEditor(actions: MessageActions, m: Message): HTMLElement {
  const input = el('textarea', {
    class: 'edit-input',
    rows: 1,
    'aria-label': 'Edit message',
  }) as HTMLTextAreaElement;
  input.value = session?.text ?? m.b;

  const autosize = () => {
    input.style.height = 'auto';
    input.style.height = `${Math.min(input.scrollHeight, 22 * 12)}px`;
  };

  const remember = () => {
    if (session) {
      session.text = input.value;
      session.start = input.selectionStart ?? input.value.length;
      session.end = input.selectionEnd ?? session.start;
    }
  };

  const commit = () => {
    const next = input.value.trim();
    // An edit to nothing is a delete in disguise; the server would refuse an
    // empty body anyway, so treat it as a cancel rather than an error.
    if (next && next !== m.b) actions.edit(m.id, next);
    cancelEdit();
  };

  input.addEventListener('input', () => {
    remember();
    autosize();
  });
  // Arrow keys and clicks move the caret without firing `input`, and a rebuild
  // in between would otherwise drop it back to where the last keystroke was.
  input.addEventListener('keyup', remember);
  input.addEventListener('click', remember);

  input.addEventListener('keydown', (ev: KeyboardEvent) => {
    if (ev.key === 'Enter' && !ev.shiftKey) {
      ev.preventDefault();
      commit();
    } else if (ev.key === 'Escape') {
      ev.preventDefault();
      // Stop here rather than letting it bubble: the document-level handler
      // reads Escape as "close the thread pane", and abandoning an edit should
      // not also collapse the conversation around it.
      ev.stopPropagation();
      cancelEdit();
    }
  });

  // The row is not in the document yet — the virtual list inserts it after this
  // returns — so focus has to wait for the next frame. Restoring the caret is
  // what makes a mid-typing rebuild invisible.
  requestAnimationFrame(() => {
    if (!input.isConnected) return;
    autosize();
    input.focus();
    const { start, end } = session ?? { start: input.value.length, end: input.value.length };
    input.setSelectionRange(start, end);
  });

  return el(
    'div',
    { class: 'message-edit' },
    input,
    el(
      'div',
      { class: 'edit-actions' },
      el('button', { class: 'edit-save', text: 'Save', on: { click: commit } }),
      el('button', { class: 'edit-cancel', text: 'Cancel', on: { click: cancelEdit } }),
      el('span', { class: 'edit-hint', text: 'Enter to save · Escape to cancel' }),
    ),
  );
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
      // Flashes once after a jump, so the message you asked for is obvious
      // among its neighbours.
      store.highlight() === m.id ? 'highlighted' : '',
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

  // Read unconditionally, so the enclosing effect subscribes whether or not
  // this particular row is the one being edited.
  const openEditor = editingId();

  if (m.del) {
    main.appendChild(el('div', { class: 'message-body tombstone', text: 'This message was deleted' }));
  } else if (openEditor === m.id) {
    main.appendChild(messageEditor(actions, m));
    // No hover bar and no reactions while editing: the row is a form, and
    // offering "delete" beside a half-typed correction invites a misclick.
    if (m.at?.length) main.appendChild(attachments(m.at));
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

  // The quick set covers the common case in one click; the picker is for
  // everything else, so neither has to be a compromise.
  bar.appendChild(
    el(
      'button',
      {
        class: 'action',
        title: 'Add reaction',
        on: {
          click: (ev: Event) =>
            openEmojiPicker({
              anchor: ev.currentTarget as HTMLElement,
              onPick: (emoji) => {
                const already = m.rx?.some((r) => r.e === emoji && r.me);
                actions.react(m.id, emoji, !already);
              },
            }),
        },
      },
      icon(ICONS.smile, 14),
    ),
  );

  bar.appendChild(
    el(
      'button',
      { class: 'action', title: 'Reply in thread', on: { click: () => actions.openThread(m.id) } },
      icon(ICONS.thread, 14),
    ),
  );

  bar.appendChild(
    el(
      'button',
      {
        class: 'action',
        title: 'Copy link to message',
        on: { click: () => actions.copyLink(m.ch, m.id) },
      },
      icon(ICONS.link, 14),
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

  // Saving is private, so it sits next to pinning but says nothing to anyone
  // else in the channel.
  const saved = store.isSaved(m.id);
  bar.appendChild(
    el(
      'button',
      {
        class: `action${saved ? ' active' : ''}`,
        title: saved ? 'Remove from saved' : 'Save for later',
        on: { click: () => actions.setSaved(m.id, !saved) },
      },
      icon(ICONS.bookmark, 14),
    ),
  );

  if (mine) {
    bar.appendChild(
      el(
        'button',
        { class: 'action', title: 'Edit', on: { click: () => beginEdit(m.id, m.b) } },
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
