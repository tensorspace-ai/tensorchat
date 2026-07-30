/**
 * Secondary panels: the thread pane and the search overlay.
 *
 * Both are rebuilt on change rather than diffed — a thread is tens of messages
 * and a result page is capped at fifty, so the windowing machinery the main
 * message list needs would be pure overhead here.
 */

import { ICONS, el, formatFullTime, icon, replace } from '../dom.ts';
import { effect, signal } from '../signals.ts';
import { api } from '../api.ts';
import { idToDate } from '../protocol.ts';
import { renderSnippet } from '../richtext.ts';
import type { Id, Message, SearchHit } from '../protocol.ts';
import type { Store } from '../store.ts';
import { renderMessage, type MessageActions } from './messages.ts';
import { addMembersDialog } from './modals.ts';
import { avatar } from './sidebar.ts';

// -- Thread pane ---------------------------------------------------------

export function ThreadPane(
  store: Store,
  actions: MessageActions & { close: () => void },
  composer: HTMLElement,
): HTMLElement {
  const body = el('div', { class: 'thread-body' });
  const root = el(
    'aside',
    { class: 'thread-pane', aria: { label: 'Thread' } },
    el(
      'div',
      { class: 'pane-header' },
      el('span', { class: 'pane-title', text: 'Thread' }),
      el(
        'button',
        { class: 'icon-button', title: 'Close thread', on: { click: actions.close } },
        icon(ICONS.close, 16),
      ),
    ),
    body,
    composer,
  );

  const messages = signal<Message[]>([]);
  let loadedFor: Id | null = null;

  effect(() => {
    const root_id = store.openThread();
    root.hidden = root_id === null;
    if (!root_id || root_id === loadedFor) return;
    loadedFor = root_id;
    messages.set([]);
    void api
      .thread(root_id)
      .then((list) => {
        // Guard against a slow response for a thread the user already closed.
        if (store.openThread() === root_id) messages.set(list);
      })
      .catch(() => messages.set([]));
  });

  effect(() => {
    const list = messages();
    // Live replies arrive through the normal message path; fold them in here so
    // an open thread updates without a refetch.
    const rootId = store.openThread();
    const channel = list[0]?.ch ?? null;
    if (channel) store.log(channel).version();

    const live = rootId
      ? (channel ? store.log(channel).messages : []).filter((m) => m.th === rootId)
      : [];

    const merged = new Map<Id, Message>();
    for (const m of list) merged.set(m.id, m);
    for (const m of live) merged.set(m.id, m);
    const ordered = [...merged.values()].sort((a, b) =>
      a.id.length !== b.id.length ? a.id.length - b.id.length : a.id < b.id ? -1 : 1,
    );

    replace(
      body,
      ordered.length === 0
        ? [el('div', { class: 'empty', text: 'Loading…' })]
        : ordered.map((m, i) =>
            i === 0
              ? el(
                  'div',
                  { class: 'thread-root' },
                  renderMessage(store, actions, m, false),
                  el('div', {
                    class: 'thread-count',
                    text: `${ordered.length - 1} ${ordered.length === 2 ? 'reply' : 'replies'}`,
                  }),
                )
              : renderMessage(store, actions, m, false),
          ),
    );
  });

  return root;
}

// -- Search overlay ------------------------------------------------------

export function SearchOverlay(
  store: Store,
  onOpenMessage: (channel: Id, message: Id) => void,
): HTMLElement & { open: () => void } {
  const results = el('div', { class: 'search-results' });
  const input = el('input', {
    class: 'search-input',
    type: 'search',
    placeholder: 'Search messages, or try from: in: has:',
    'aria-label': 'Search messages',
  }) as HTMLInputElement;

  /** Shown on an empty box: the operators are useless if nobody knows them. */
  const hint = el(
    'div',
    { class: 'search-hint' },
    ...(
      [
        ['from:alice', 'by one person'],
        ['in:general', 'in one channel'],
        ['has:link', 'has:file, has:image'],
        ['before:2026-01-15', 'after: too'],
      ] as const
    ).map(([op, what]) =>
      el(
        'div',
        { class: 'search-hint-row' },
        el('code', { class: 'search-hint-op', text: op }),
        el('span', { text: what }),
      ),
    ),
  );

  const root = el(
    'div',
    { class: 'search-overlay', hidden: true },
    el(
      'div',
      { class: 'search-panel' },
      el(
        'div',
        { class: 'search-box' },
        icon(ICONS.search, 16),
        input,
        el(
          'button',
          { class: 'icon-button', title: 'Close', on: { click: () => close() } },
          icon(ICONS.close, 16),
        ),
      ),
      hint,
      results,
    ),
    // `open` is attached just below; the cast names the shape callers get.
  ) as unknown as HTMLElement & { open: () => void };

  let debounce: number | undefined;
  let sequence = 0;

  function close(): void {
    root.hidden = true;
    input.value = '';
    hint.hidden = false;
    replace(results, []);
  }

  root.open = () => {
    root.hidden = false;
    hint.hidden = input.value.length > 0;
    input.focus();
    input.select();
  };

  input.addEventListener('input', () => {
    // Debounced: a query per keystroke would run a full-text search on every
    // letter, and the user is still typing anyway.
    if (debounce !== undefined) clearTimeout(debounce);
    const q = input.value.trim();
    hint.hidden = q.length > 0;
    if (q.length < 2) {
      replace(results, []);
      return;
    }
    debounce = setTimeout(() => void run(q), 180) as unknown as number;
  });

  input.addEventListener('keydown', (ev: KeyboardEvent) => {
    if (ev.key === 'Escape') close();
  });

  root.addEventListener('click', (ev) => {
    if (ev.target === root) close();
  });

  async function run(q: string): Promise<void> {
    const mine = ++sequence;
    replace(results, [el('div', { class: 'search-status', text: 'Searching…' })]);
    try {
      const hits = await api.search(q, { limit: 40 });
      // Drop a response that a newer query has already superseded.
      if (mine !== sequence) return;
      render(hits);
    } catch {
      if (mine === sequence) {
        replace(results, [el('div', { class: 'search-status', text: 'Search failed.' })]);
      }
    }
  }

  function render(hits: SearchHit[]): void {
    if (hits.length === 0) {
      replace(results, [el('div', { class: 'search-status', text: 'No matches.' })]);
      return;
    }
    replace(
      results,
      hits.map((hit) => {
        const channel = store.channels().get(hit.m.ch);
        const date = idToDate(hit.m.id);
        const body = el('div', { class: 'result-snippet' });
        // A filters-only search (`from:alice`) matched no terms, so there is no
        // snippet to highlight — show the body itself rather than a blank row.
        body.appendChild(hit.sn ? renderSnippet(hit.sn) : renderSnippet(hit.m.b));
        return el(
          'button',
          {
            class: 'search-result',
            on: {
              click: () => {
                onOpenMessage(hit.m.ch, hit.m.id);
                close();
              },
            },
          },
          avatar(hit.m.au, store.userName(hit.m.au), 28),
          el(
            'div',
            { class: 'result-main' },
            el(
              'div',
              { class: 'result-head' },
              el('span', { class: 'result-author', text: store.userName(hit.m.au) }),
              el('span', {
                class: 'result-channel',
                text: channel ? store.channelTitle(channel) : '',
              }),
              el('span', { class: 'result-time', text: formatFullTime(date) }),
            ),
            body,
          ),
        );
      }),
    );
  }

  return root;
}

// -- Pinned messages -----------------------------------------------------

/**
 * A channel's pinned messages.
 *
 * Rebuilt on change rather than diffed, like the thread pane: the list is
 * capped server-side at a hundred, so the windowing machinery would be pure
 * overhead.
 */
export function PinnedPane(store: Store, actions: MessageActions): HTMLElement {
  const body = el('div', { class: 'pinned-body' });
  const root = el(
    'aside',
    { class: 'pinned-pane', hidden: true, aria: { label: 'Pinned messages' } },
    el(
      'div',
      { class: 'pane-header' },
      el('span', { class: 'pane-title', text: 'Pinned' }),
      el('button', {
        class: 'icon-button',
        text: '×',
        title: 'Close',
        on: { click: () => (root.hidden = true) },
      }),
    ),
    body,
  );

  const messages = signal<Message[]>([]);

  const load = (channel: Id) => {
    void api
      .pins(channel)
      .then((list) => {
        if (store.currentChannel() !== channel) return;
        messages.set(list);
        // The fetch is also how the store learns this channel's pin set, which
        // is what marks pinned messages in the main scroll.
        store.setPins(channel, list.map((m) => m.id));
      })
      .catch(() => messages.set([]));
  };

  // Reload whenever the channel changes or a pin lands. `store.pins()` covers
  // both the local toggle and someone else's `pin` frame.
  effect(() => {
    const channel = store.currentChannel();
    store.pins();
    if (!channel) {
      messages.set([]);
      return;
    }
    if (!root.hidden) load(channel);
  });

  effect(() => {
    const list = messages();
    store.users();
    if (list.length === 0) {
      replace(body, [
        el('div', {
          class: 'empty',
          text: 'Nothing pinned yet. Pin a message to keep it here.',
        }),
      ]);
      return;
    }
    replace(
      body,
      list.map((m) => renderMessage(store, actions, m, false)),
    );
  });

  (root as HTMLElement & { toggle?: () => void }).toggle = () => {
    root.hidden = !root.hidden;
    const channel = store.currentChannel();
    if (!root.hidden && channel) load(channel);
  };
  return root;
}

// -- Saved messages ------------------------------------------------------

/**
 * The viewer's saved messages, across every channel.
 *
 * The one view in the product that deliberately ignores which channel a
 * message came from, so each row is labelled with its origin — without that
 * the list reads as a pile of context-free sentences.
 */
export function SavedPane(
  store: Store,
  actions: MessageActions,
  onOpenMessage: (channel: Id, message: Id) => void,
): HTMLElement {
  const body = el('div', { class: 'pinned-body' });
  const root = el(
    'aside',
    { class: 'pinned-pane', hidden: true, aria: { label: 'Saved messages' } },
    el(
      'div',
      { class: 'pane-header' },
      el('span', { class: 'pane-title', text: 'Saved' }),
      el('button', {
        class: 'icon-button',
        text: '×',
        title: 'Close',
        on: { click: () => (root.hidden = true) },
      }),
    ),
    body,
  );

  const messages = signal<Message[]>([]);

  const load = () => {
    void api
      .saved()
      .then((list) => {
        messages.set(list);
        store.setSaved(list.map((m) => m.id));
      })
      .catch(() => messages.set([]));
  };

  // Refetch when the set changes — including from another tab, which arrives
  // as a `saved` frame.
  effect(() => {
    store.savedIds();
    if (!root.hidden) load();
  });

  effect(() => {
    const list = messages();
    store.users();
    store.channels();
    if (list.length === 0) {
      replace(body, [
        el('div', {
          class: 'empty',
          text: 'Nothing saved yet. Save a message to keep it here.',
        }),
      ]);
      return;
    }
    replace(
      body,
      list.map((m) => {
        const channel = store.channels().get(m.ch);
        return el(
          'div',
          { class: 'saved-item' },
          el('button', {
            class: 'saved-origin',
            text: channel
              ? `${channel.k === 'public' ? '#' : ''}${store.channelTitle(channel)}`
              : 'a channel',
            title: 'Go to this message',
            on: { click: () => onOpenMessage(m.ch, m.id) },
          }),
          renderMessage(store, actions, m, false),
        );
      }),
    );
  });

  (root as HTMLElement & { toggle?: () => void }).toggle = () => {
    root.hidden = !root.hidden;
    if (!root.hidden) load();
  };
  return root;
}

// -- Member list ---------------------------------------------------------

export function MemberList(store: Store, onOpenDm: (user: Id) => void): HTMLElement {
  const body = el('div', { class: 'member-body' });
  const add = el('button', {
    class: 'icon-button',
    title: 'Add people',
    text: '+',
    hidden: true,
  }) as HTMLButtonElement;
  const root = el(
    'aside',
    { class: 'member-pane', hidden: true, aria: { label: 'Members' } },
    el(
      'div',
      { class: 'pane-header' },
      el('span', { class: 'pane-title', text: 'Members' }),
      add,
    ),
    body,
  );

  const members = signal<Id[]>([]);
  let loadedFor: Id | null = null;

  const load = (channel: Id) => {
    const known = store.channels().get(channel);
    if (known?.m?.length) {
      members.set(known.m);
    } else {
      void api
        .members(channel)
        .then((list) => {
          if (store.currentChannel() === channel) members.set(list);
        })
        .catch(() => members.set([]));
    }
  };

  effect(() => {
    const channel = store.currentChannel();
    if (!channel || channel === loadedFor) return;
    loadedFor = channel;
    load(channel);
  });

  // Someone was added or removed while the pane is open. Refetching is one
  // small query and keeps this list the server's answer rather than a guess
  // assembled from deltas.
  effect(() => {
    const change = store.memberChange();
    if (change && change.ch === store.currentChannel()) load(change.ch);
  });

  effect(() => {
    const ids = members();
    store.presence();
    store.users();
    const channel = store.currentChannel();
    const kind = channel ? store.channels().get(channel)?.k : undefined;
    // A direct conversation's roster is fixed — it is what identifies the
    // conversation — so the editing controls only make sense on named channels.
    const editable = channel !== null && kind !== 'dm' && kind !== 'group';
    const meId = store.me()?.id;

    add.hidden = !editable;
    add.onclick = editable
      ? () => addMembersDialog(store, channel, members(), (added) => {
          if (added.length) members.update((prev) => [...prev, ...added]);
        })
      : null;

    const sorted = [...ids].sort((a, b) => {
      const pa = store.presenceOf(a) === 'offline' ? 1 : 0;
      const pb = store.presenceOf(b) === 'offline' ? 1 : 0;
      return pa - pb || store.userName(a).localeCompare(store.userName(b));
    });
    replace(
      body,
      sorted.map((id) =>
        el(
          'div',
          { class: 'member-row-wrap' },
          el(
            'button',
            { class: 'member-row', on: { click: () => onOpenDm(id) } },
            avatar(id, store.userName(id), 26),
            el('span', { class: 'member-name', text: store.userName(id) }),
            el('span', { class: `presence presence-${store.presenceOf(id)}` }),
          ),
          // Removing yourself is a leave, which lives on the channel menu;
          // offering it twice under two names would only confuse.
          editable && id !== meId
            ? el('button', {
                class: 'member-remove',
                text: '×',
                title: `Remove ${store.userName(id)}`,
                on: {
                  click: () => {
                    if (!channel) return;
                    void api
                      .removeMember(channel, id)
                      .then(() => members.update((prev) => prev.filter((m) => m !== id)))
                      .catch(() => load(channel));
                  },
                },
              })
            : null,
        ),
      ),
    );
  });

  (root as HTMLElement & { toggle?: () => void }).toggle = () => {
    root.hidden = !root.hidden;
  };
  return root;
}
