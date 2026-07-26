/**
 * The application shell: layout, wiring, and the handful of behaviors that do
 * not belong to any single component (keyboard shortcuts, the title badge,
 * marking a channel read when you look at it).
 */

import { ICONS, el, icon, replace } from './dom.ts';
import { effect } from './signals.ts';
import { api, setToken } from './api.ts';
import { Connection } from './ws.ts';
import { store } from './store.ts';
import { idCompare } from './protocol.ts';
import type { Channel, Id } from './protocol.ts';
import { Composer } from './ui/composer.ts';
import { LoginScreen } from './ui/login.ts';
import { MessageList, type MessageActions } from './ui/messages.ts';
import { MemberList, SearchOverlay, ThreadPane } from './ui/panels.ts';
import { Sidebar } from './ui/sidebar.ts';
import {
  browseChannelsDialog,
  createChannelDialog,
  newDmDialog,
  preferencesDialog,
} from './ui/modals.ts';

/** Messages fetched per history page. */
const PAGE_SIZE = 50;

export function mount(root: HTMLElement): void {
  const token = api === undefined ? null : localStorage.getItem('tc_token');

  const showLogin = () => {
    replace(root, [LoginScreen(() => start(root))]);
  };

  if (!token) {
    showLogin();
    return;
  }
  // A stored token may be expired or revoked; verify before painting the app,
  // otherwise the user sees a full UI that immediately fails every request.
  void api
    .me()
    .then(() => start(root))
    .catch(() => {
      setToken(null);
      showLogin();
    });
}

function start(root: HTMLElement): void {
  const conn = new Connection({
    onFrame: (frame) => {
      store.apply(frame);
      // Belt and braces. The server subscribes a user's live connections when
      // they gain access to a channel, so this is normally redundant — but
      // subscribing is idempotent, and a channel we can see but do not receive
      // messages for is an invisible failure.
      if ('chan' in frame) conn.subscribe([frame.chan.c.id]);
    },
    onState: (state) => store.connection.set(state),
    onResync: () => {
      // Events during the outage are gone — the server does not replay. Refetch
      // rather than continue with state that may have holes in it.
      const channel = store.currentChannel();
      if (channel) {
        const log = store.log(channel);
        log.messages.length = 0;
        log.index.clear();
        log.cursor = null;
        log.complete = false;
        void loadHistory(channel);
      }
    },
  });
  conn.connect();

  // -- History loading ----------------------------------------------------

  async function loadHistory(channel: Id): Promise<void> {
    const log = store.log(channel);
    if (log.loading || log.complete) return;
    log.loading = true;
    try {
      const page = await api.history(channel, log.cursor, PAGE_SIZE);
      store.prependPage(channel, page.messages, page.next_cursor);
    } catch {
      log.loading = false;
    }
  }

  // -- Navigation ---------------------------------------------------------

  function openChannel(id: Id): void {
    store.currentChannel.set(id);
    store.openThread.set(null);
    const log = store.log(id);
    if (log.messages.length === 0) void loadHistory(id);
    markCurrentRead();
  }

  function markCurrentRead(): void {
    const channel = store.currentChannel();
    if (!channel) return;
    const log = store.log(channel);
    const newest = log.messages[log.messages.length - 1];
    const target = newest?.id ?? store.channels().get(channel)?.last;
    if (!target) return;

    const state = store.unread(channel);
    // Only tell the server when the cursor actually advances.
    if (state && idCompare(state.lr, target) >= 0) return;
    store.clearUnread(channel);
    conn.markRead(channel, target);
  }

  async function openDmWith(user: Id): Promise<void> {
    try {
      const channel = await api.openDm([user]);
      store.channels.update((prev) => new Map(prev).set(channel.id, channel));
      conn.subscribe([channel.id]);
      openChannel(channel.id);
    } catch {
      /* the dialog surfaces failures; a click on an avatar can fail quietly */
    }
  }

  function adoptChannel(c: Channel): void {
    store.channels.update((prev) => new Map(prev).set(c.id, c));
    conn.subscribe([c.id]);
    openChannel(c.id);
  }

  // -- Message actions ----------------------------------------------------

  const messageActions: MessageActions = {
    react: (id, emoji, on) => conn.react(id, emoji, on),
    openThread: (rootId) => store.openThread.set(rootId),
    edit: (id, body) => {
      const next = prompt('Edit message', body);
      if (next !== null && next.trim() && next !== body) conn.editMessage(id, next.trim());
    },
    remove: (id) => conn.deleteMessage(id),
    loadOlder: () => {
      const channel = store.currentChannel();
      if (channel) void loadHistory(channel);
    },
    openMention: (user) => void openDmWith(user),
    openChannel,
  };

  // -- Layout -------------------------------------------------------------

  const sidebar = Sidebar(store, {
    open: openChannel,
    createChannel: () => createChannelDialog(adoptChannel),
    browseChannels: () => browseChannelsDialog(store, adoptChannel),
    newDm: () => newDmDialog(store, adoptChannel),
    openPreferences: () =>
      preferencesDialog(
        store,
        (u) => store.me.set(u),
        () => {
          void api.logout().catch(() => {});
          setToken(null);
          conn.close();
          location.reload();
        },
      ),
  });

  const messageList = MessageList(store, messageActions);

  const composer = Composer(store, {
    send: (body, attachments) => {
      const channel = store.currentChannel();
      if (!channel) return;
      const nonce = conn.sendMessage(channel, body, null, attachments);
      // Optimistic echo: the message is on screen before the server answers.
      store.addPending({
        nonce,
        channel,
        body,
        threadRoot: null,
        at: Date.now(),
        failed: false,
      });
    },
    typing: () => {
      const channel = store.currentChannel();
      if (channel) conn.typing(channel);
    },
    upload: (file) => api.upload(file),
  }, {
    placeholder: () => {
      const channel = store.currentChannel();
      const c = channel ? store.channels().get(channel) : undefined;
      return c ? `Message ${c.k === 'public' ? '#' : ''}${store.channelTitle(c)}` : 'Message';
    },
  });

  const threadComposer = Composer(store, {
    send: (body, attachments) => {
      const rootId = store.openThread();
      const channel = store.currentChannel();
      if (!rootId || !channel) return;
      conn.sendMessage(channel, body, rootId, attachments);
    },
    typing: () => {
      const channel = store.currentChannel();
      if (channel) conn.typing(channel);
    },
    upload: (file) => api.upload(file),
  }, { placeholder: () => 'Reply…' });

  const threadPane = ThreadPane(
    store,
    { ...messageActions, close: () => store.openThread.set(null) },
    threadComposer,
  );

  const memberPane = MemberList(store, (user) => void openDmWith(user));
  const search = SearchOverlay(store, (channel) => openChannel(channel));

  const channelHeader = ChannelHeader(store, {
    toggleMembers: () => (memberPane as HTMLElement & { toggle?: () => void }).toggle?.(),
    openSearch: () => search.open(),
  });

  const typingLine = TypingIndicator(store);

  replace(root, [
    el(
      'div',
      { class: 'app' },
      sidebar,
      el(
        'main',
        { class: 'main' },
        channelHeader,
        messageList,
        typingLine,
        composer,
      ),
      memberPane,
      threadPane,
      search,
    ),
  ]);

  // -- Cross-cutting behaviors -------------------------------------------

  // Open the first channel once the workspace snapshot arrives.
  effect(() => {
    const channels = store.sortedChannels();
    if (!store.currentChannel() && channels.length > 0) openChannel(channels[0].id);
  });

  // Unread badge in the tab title.
  effect(() => {
    const mentions = store.totalMentions();
    document.title = mentions > 0 ? `(${mentions}) TensorChat` : 'TensorChat';
  });

  // Reading the channel you are looking at clears its badge.
  effect(() => {
    const channel = store.currentChannel();
    if (!channel) return;
    store.log(channel).version();
    if (document.visibilityState === 'visible') markCurrentRead();
  });

  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') {
      conn.setPresence('online');
      markCurrentRead();
    } else {
      conn.setPresence('away');
    }
  });

  // Typing indicators expire on a timestamp; one shared tick re-renders them
  // rather than a timer per keystroke per user.
  setInterval(() => store.typing.update((m) => new Map(m)), 2000);

  document.addEventListener('keydown', (ev: KeyboardEvent) => {
    const mod = ev.metaKey || ev.ctrlKey;
    if (mod && ev.key === 'k') {
      ev.preventDefault();
      search.open();
      return;
    }
    if (ev.key === 'Escape') {
      if (store.openThread()) store.openThread.set(null);
      return;
    }
    // Typing anywhere focuses the composer, as long as the keystroke was not
    // meant for another input.
    const target = ev.target as HTMLElement | null;
    const inField =
      target instanceof HTMLInputElement ||
      target instanceof HTMLTextAreaElement ||
      target?.isContentEditable;
    if (!inField && !mod && ev.key.length === 1) {
      (composer as HTMLElement & { focusInput?: () => void }).focusInput?.();
    }
  });
}

function ChannelHeader(
  store: typeof import('./store.ts').store,
  actions: { toggleMembers: () => void; openSearch: () => void },
): HTMLElement {
  const root = el('header', { class: 'channel-header' });
  effect(() => {
    const id = store.currentChannel();
    const c = id ? store.channels().get(id) : undefined;
    store.users();
    replace(root, [
      el(
        'div',
        { class: 'header-main' },
        c
          ? el(
              'h1',
              { class: 'header-title' },
              c.k === 'public' ? icon(ICONS.hash, 16) : c.k === 'private' ? icon(ICONS.lock, 16) : null,
              el('span', { text: store.channelTitle(c) }),
            )
          : el('h1', { class: 'header-title', text: 'TensorChat' }),
        c?.t ? el('span', { class: 'header-topic', text: c.t }) : null,
      ),
      el(
        'div',
        { class: 'header-actions' },
        el(
          'button',
          { class: 'icon-button', title: 'Search (⌘K)', on: { click: actions.openSearch } },
          icon(ICONS.search, 17),
        ),
        el(
          'button',
          { class: 'icon-button', title: 'Members', on: { click: actions.toggleMembers } },
          icon(ICONS.people, 17),
        ),
      ),
    ]);
  });
  return root;
}

function TypingIndicator(store: typeof import('./store.ts').store): HTMLElement {
  const root = el('div', { class: 'typing-line' });
  effect(() => {
    const channel = store.currentChannel();
    store.typing();
    const who = channel ? store.typingIn(channel) : [];
    const names = who.map((id) => store.userName(id));
    root.textContent =
      names.length === 0
        ? ''
        : names.length === 1
          ? `${names[0]} is typing…`
          : names.length === 2
            ? `${names[0]} and ${names[1]} are typing…`
            : 'Several people are typing…';
  });
  return root;
}
