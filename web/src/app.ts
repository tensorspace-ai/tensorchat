/**
 * The application shell: layout, wiring, and the handful of behaviors that do
 * not belong to any single component (keyboard shortcuts, the title badge,
 * marking a channel read when you look at it).
 */

import { ICONS, el, icon, replace } from './dom.ts';
import { effect } from './signals.ts';
import { api, setToken } from './api.ts';
import { clearAllDrafts } from './drafts.ts';
import { Connection } from './ws.ts';
import { store } from './store.ts';
import { createNotifier } from './notify.ts';
import { idCompare } from './protocol.ts';
import type { Channel, Id } from './protocol.ts';
import { Composer } from './ui/composer.ts';
import { LoginScreen } from './ui/login.ts';
import { MessageList, beginEdit, cancelEdit, isEditing, type MessageActions } from './ui/messages.ts';
import { MemberList, PinnedPane, SavedPane, SearchOverlay, ThreadPane } from './ui/panels.ts';
import { Sidebar } from './ui/sidebar.ts';
import {
  browseChannelsDialog,
  createChannelDialog,
  newDmDialog,
  preferencesDialog,
} from './ui/modals.ts';

/** Messages fetched per history page. */
const PAGE_SIZE = 50;

/** How long a jumped-to message stays highlighted. Matches the CSS fade. */
const HIGHLIGHT_MS = 2200;

export function mount(root: HTMLElement): void {
  const token = api === undefined ? null : localStorage.getItem('tc_token');

  const showLogin = () => {
    replace(root, [LoginScreen(() => start(root))]);
  };

  // Ask the server even with no token stored. A sign-in through an identity
  // provider comes back as a redirect carrying only the session *cookie* —
  // there is no token for the callback to hand over, since putting one in the
  // URL would write a live credential into the browser's history. So the
  // absence of a stored token no longer means the absence of a session, and
  // short-circuiting to the login screen would have shown it to somebody who
  // had just finished signing in.
  //
  // For a first-time visitor this costs one 401 before the login screen.
  void api
    .me()
    .then(() => start(root))
    .catch(() => {
      if (token) setToken(null);
      showLogin();
    });
}

function start(root: HTMLElement): void {
  // Declared before the connection so `onFrame` can reach it; it only needs
  // `openChannel`, which is hoisted.
  const notifier = createNotifier(store, (channel) => openChannel(channel));

  const conn = new Connection({
    onFrame: (frame) => {
      store.apply(frame);
      // After `apply`, so the notifier sees the channel and mute state the
      // frame may have just established.
      notifier.consider(frame);
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

  // -- Jumping to a message -----------------------------------------------

  /**
   * Open a channel positioned on one message, from a search hit, a pinned or
   * saved item, or a permalink.
   *
   * The log becomes a *window* rather than the live tail — see `ChannelLog.anchor`
   * — so a "Jump to latest" bar appears until the reader returns to the present.
   */
  async function jumpToMessage(channel: Id, message: Id): Promise<void> {
    store.currentChannel.set(channel);
    store.openThread.set(null);
    void loadPins(channel);
    try {
      const page = await api.historyAround(channel, message, PAGE_SIZE);
      store.showAround(channel, message, page.messages, page.next_cursor);
      store.highlight.set(message);
      // Cleared after the flash, so jumping to the same message twice flashes
      // twice instead of silently doing nothing the second time.
      setTimeout(() => {
        if (store.highlight() === message) store.highlight.set(null);
      }, HIGHLIGHT_MS);
    } catch {
      // A message that has been deleted, or that lives in a channel this user
      // has since left. Fall back to opening the channel normally rather than
      // leaving them on a blank pane.
      openChannel(channel);
    }
  }

  /** Leave a historical window and reload the newest page. */
  function jumpToLatest(): void {
    const channel = store.currentChannel();
    if (!channel) return;
    store.highlight.set(null);
    store.resetToLive(channel);
    void loadHistory(channel);
  }

  /**
   * `#/c/{channel}/{message}` — a permalink. Read once at startup and on every
   * hash change, so a pasted link works in an already-open tab too.
   */
  function openFromHash(): boolean {
    const m = /^#\/c\/(\d+)(?:\/(\d+))?$/.exec(location.hash);
    if (!m) return false;
    const [, channel, message] = m;
    if (message) void jumpToMessage(channel, message);
    else openChannel(channel);
    return true;
  }

  window.addEventListener('hashchange', () => openFromHash());

  // -- Navigation ---------------------------------------------------------

  function openChannel(id: Id): void {
    store.currentChannel.set(id);
    store.openThread.set(null);
    store.highlight.set(null);
    const log = store.log(id);
    // Opening a channel deliberately means "take me to this channel", so a
    // window left over from an earlier jump is discarded rather than returned
    // to. Jumping back is one click from wherever the link was.
    if (log.anchor !== null) store.resetToLive(id);
    if (log.messages.length === 0) void loadHistory(id);
    void loadPins(id);
    markCurrentRead();
  }

  /**
   * Fetch a channel's pins once, on open.
   *
   * Deliberately not part of the history query: pins are a small bounded set
   * that changes rarely, and joining them onto every page would put a pin
   * lookup on the hottest read in the product to serve a marker and a panel.
   */
  async function loadPins(channel: Id): Promise<void> {
    try {
      const pinned = await api.pins(channel);
      store.setPins(channel, pinned.map((m) => m.id));
    } catch {
      /* a missing pin marker is not worth interrupting the channel for */
    }
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
    setPin: (id, on) => {
      const channel = store.currentChannel();
      if (!channel) return;
      // Optimistic, then reconciled by the server's `pin` broadcast — which
      // arrives even for your own toggle, so a rejected pin snaps back.
      store.setPinned(channel, id, on);
      void api.setPin(id, on).catch(() => store.setPinned(channel, id, !on));
    },
    setSaved: (id, on) => {
      // Optimistic, then reconciled by the server's `saved` echo — which also
      // reaches this user's other tabs.
      store.setSavedLocal(id, on);
      void api.setSaved(id, on).catch(() => store.setSavedLocal(id, !on));
    },
    copyLink: (channel, message) => {
      const url = `${location.origin}${location.pathname}#/c/${channel}/${message}`;
      // `writeText` needs a secure context; over plain HTTP on a LAN address it
      // rejects, so fall back to putting the link where it can still be copied
      // by hand rather than failing silently.
      void navigator.clipboard?.writeText(url).catch(() => prompt('Link to message', url));
    },
    openThread: (rootId) => store.openThread.set(rootId),
    // The editor itself lives in the message row; by the time this is called
    // the new body has already been composed and confirmed.
    edit: (id, body) => conn.editMessage(id, body),
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
    openSaved: () => (savedPane as HTMLElement & { toggle?: () => void }).toggle?.(),
    openPreferences: () =>
      preferencesDialog(
        store,
        notifier,
        (u) => store.me.set(u),
        () => {
          void api.logout().catch(() => {});
          setToken(null);
          // Unsent text is the one thing here the server cannot re-supply, so
          // it must not be left in storage for whoever uses this browser next.
          clearAllDrafts();
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
    // Up-arrow on an empty composer opens the last thing you said. The hook
    // existed in the composer but had never been connected to anything.
    editLast: () => {
      const channel = store.currentChannel();
      const meId = store.me()?.id;
      if (!channel || !meId) return;
      const mine = store.log(channel).messages.filter((m) => m.au === meId && !m.del && !m.th);
      const last = mine[mine.length - 1];
      if (last) beginEdit(last.id, last.b);
    },
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
  const pinnedPane = PinnedPane(store, messageActions);
  const savedPane = SavedPane(store, messageActions, (channel, message) =>
    void jumpToMessage(channel, message),
  );
  // The overlay has always passed the hit's message id; until history could be
  // fetched around an anchor there was nothing to do with it but drop it.
  const search = SearchOverlay(store, (channel, message) => void jumpToMessage(channel, message));

  const channelHeader = ChannelHeader(store, {
    toggleMembers: () => (memberPane as HTMLElement & { toggle?: () => void }).toggle?.(),
    togglePinned: () => (pinnedPane as HTMLElement & { toggle?: () => void }).toggle?.(),
    toggleMuted: () => {
      const channel = store.currentChannel();
      if (!channel) return;
      // The server answers with the whole read state and also echoes it to this
      // user's other tabs, so there is nothing to fold in by hand.
      void api.muteChannel(channel, !store.isMuted(channel)).catch(() => {});
    },
    openSearch: () => search.open(),
  });

  const typingLine = TypingIndicator(store);
  const anchorBar = AnchorBar(store, jumpToLatest);

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
        anchorBar,
        typingLine,
        composer,
      ),
      pinnedPane,
      savedPane,
      memberPane,
      threadPane,
      search,
    ),
  ]);

  // -- Cross-cutting behaviors -------------------------------------------

  // The saved set is not part of the connect snapshot — it is personal state
  // nothing else depends on — so fetch it once to mark saved messages in
  // history. The panel refetches on open.
  void api
    .saved()
    .then((list) => store.setSaved(list.map((m) => m.id)))
    .catch(() => {});

  // Open whatever the URL asks for, or the first channel, once the workspace
  // snapshot arrives. A permalink wins: it is the more specific request, and
  // it is what the person clicking the link actually wanted to see.
  let opened = false;
  effect(() => {
    const channels = store.sortedChannels();
    if (opened || store.currentChannel() || channels.length === 0) return;
    opened = true;
    if (!openFromHash()) openChannel(channels[0].id);
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
      // Reached only when focus has left the editor's textarea — the editor
      // stops the event itself while focused. Closing it still takes priority
      // over the thread pane, since it is the more local thing on screen.
      if (isEditing()) {
        cancelEdit();
        return;
      }
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
  actions: {
    toggleMembers: () => void;
    togglePinned: () => void;
    toggleMuted: () => void;
    openSearch: () => void;
  },
): HTMLElement {
  const root = el('header', { class: 'channel-header' });
  effect(() => {
    const id = store.currentChannel();
    const c = id ? store.channels().get(id) : undefined;
    store.users();
    store.readStates();
    const pinCount = id ? store.pinsIn(id).size : 0;
    const muted = id ? store.isMuted(id) : false;
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
        // Only offered once there is something to show — an always-present
        // button that usually opens an empty panel is just noise in the header.
        pinCount > 0
          ? el(
              'button',
              {
                class: 'icon-button with-count',
                title: `${pinCount} pinned`,
                on: { click: actions.togglePinned },
              },
              icon(ICONS.pin, 17),
              el('span', { class: 'icon-count', text: String(pinCount) }),
            )
          : null,
        c
          ? el(
              'button',
              {
                class: `icon-button${muted ? ' active' : ''}`,
                title: muted ? 'Unmute this channel' : 'Mute this channel',
                on: { click: actions.toggleMuted },
              },
              icon(muted ? ICONS.bellOff : ICONS.bell, 17),
            )
          : null,
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

/**
 * "You are viewing older messages" — shown while the open channel's log is a
 * window around some older message rather than the live tail.
 *
 * Necessary rather than decorative: while anchored, new messages are
 * deliberately not appended, so without this the channel would look as though
 * it had gone silent.
 */
function AnchorBar(
  store: typeof import('./store.ts').store,
  onJumpToLatest: () => void,
): HTMLElement {
  const root = el('div', { class: 'anchor-bar', hidden: true });
  effect(() => {
    const channel = store.currentChannel();
    if (!channel) {
      root.hidden = true;
      return;
    }
    const log = store.log(channel);
    log.version();
    root.hidden = log.anchor === null;
    if (root.hidden) return;
    replace(root, [
      el('span', { text: 'Viewing older messages' }),
      el('button', {
        class: 'anchor-jump',
        text: 'Jump to latest',
        on: { click: onJumpToLatest },
      }),
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
