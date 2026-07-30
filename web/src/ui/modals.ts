/**
 * Dialogs: create a channel, browse the directory, start a DM, edit your
 * profile.
 *
 * Built on the native `<dialog>` element, which brings focus trapping, Escape
 * to dismiss, and the top-layer backdrop for free — all of which would
 * otherwise be a few hundred lines of accessibility code to get wrong.
 */

import { el, replace } from '../dom.ts';
import { ApiError, api } from '../api.ts';
import { readPreference, setPreference, type ThemePreference } from '../theme.ts';
import type { Channel, Id, Invite, User } from '../protocol.ts';
import type { Notifier } from '../notify.ts';
import type { Store } from '../store.ts';
import { avatar } from './sidebar.ts';

function dialog(title: string, ...content: (Node | null)[]): HTMLDialogElement {
  const d = el('dialog', { class: 'modal' }) as HTMLDialogElement;
  d.appendChild(
    el(
      'div',
      { class: 'modal-inner' },
      el(
        'div',
        { class: 'modal-header' },
        el('h2', { class: 'modal-title', text: title }),
        el('button', {
          class: 'icon-button',
          text: '×',
          title: 'Close',
          on: { click: () => d.close() },
        }),
      ),
      ...content.filter((c): c is Node => c !== null),
    ),
  );
  // Dialogs are single-use; removing on close keeps the DOM from accumulating.
  d.addEventListener('close', () => d.remove());
  document.body.appendChild(d);
  d.showModal();
  return d;
}

function errorLine(): HTMLElement {
  return el('div', { class: 'modal-error', hidden: true, role: 'alert' });
}

function showError(node: HTMLElement, err: unknown): void {
  node.textContent =
    err instanceof ApiError ? err.message : 'Something went wrong. Please try again.';
  node.hidden = false;
}

export function createChannelDialog(onCreated: (c: Channel) => void): void {
  const name = el('input', {
    class: 'modal-input',
    placeholder: 'e.g. design-review',
    'aria-label': 'Channel name',
  }) as HTMLInputElement;
  const topic = el('input', {
    class: 'modal-input',
    placeholder: 'Topic (optional)',
    'aria-label': 'Topic',
  }) as HTMLInputElement;
  const isPrivate = el('input', { type: 'checkbox' }) as HTMLInputElement;
  const error = errorLine();
  const submit = el('button', { class: 'modal-submit', text: 'Create' }) as HTMLButtonElement;

  const d = dialog(
    'Create a channel',
    el('p', {
      class: 'modal-hint',
      text: 'Lowercase letters, numbers, dashes and underscores.',
    }),
    name,
    topic,
    el('label', { class: 'modal-check' }, isPrivate, el('span', { text: 'Make private' })),
    error,
    submit,
  );

  const create = async () => {
    const raw = name.value
      .trim()
      .toLowerCase()
      .replace(/^#/, '')
      // Spaces are what people actually type; turn them into the dashes the
      // server requires rather than rejecting the input.
      .replace(/\s+/g, '-');
    if (!raw) return;
    submit.disabled = true;
    try {
      onCreated(await api.createChannel(raw, isPrivate.checked, topic.value.trim()));
      d.close();
    } catch (err) {
      showError(error, err);
      submit.disabled = false;
    }
  };

  submit.addEventListener('click', () => void create());
  name.addEventListener('keydown', (ev: KeyboardEvent) => {
    if (ev.key === 'Enter') void create();
  });
  name.focus();
}

export function browseChannelsDialog(store: Store, onJoin: (c: Channel) => void): void {
  const list = el('div', { class: 'modal-list' });
  const d = dialog('Browse channels', list);

  void api
    .browseChannels()
    .then((channels) => {
      const mine = store.channels();
      if (channels.length === 0) {
        replace(list, [el('div', { class: 'empty', text: 'No public channels yet.' })]);
        return;
      }
      replace(
        list,
        channels.map((c) => {
          const joined = mine.has(c.id);
          return el(
            'div',
            { class: 'browse-row' },
            el(
              'div',
              { class: 'browse-main' },
              el('span', { class: 'browse-name', text: `#${c.n ?? ''}` }),
              c.t ? el('span', { class: 'browse-topic', text: c.t }) : null,
            ),
            el('button', {
              class: 'browse-join',
              text: joined ? 'Joined' : 'Join',
              disabled: joined,
              on: {
                click: async (ev: Event) => {
                  const button = ev.currentTarget as HTMLButtonElement;
                  button.disabled = true;
                  try {
                    onJoin(await api.joinChannel(c.id));
                    d.close();
                  } catch {
                    button.disabled = false;
                  }
                },
              },
            }),
          );
        }),
      );
    })
    .catch(() => replace(list, [el('div', { class: 'empty', text: 'Could not load channels.' })]));
}

export function newDmDialog(store: Store, onOpened: (c: Channel) => void): void {
  const selected = new Set<Id>();
  const search = el('input', {
    class: 'modal-input',
    placeholder: 'Find people…',
    'aria-label': 'Find people',
  }) as HTMLInputElement;
  const list = el('div', { class: 'modal-list' });
  const error = errorLine();
  const submit = el('button', {
    class: 'modal-submit',
    text: 'Start conversation',
    disabled: true,
  }) as HTMLButtonElement;

  const d = dialog('New message', search, list, error, submit);

  const render = () => {
    const q = search.value.trim().toLowerCase();
    const meId = store.me()?.id;
    const people = [...store.users().values()]
      .filter((u) => u.id !== meId && !u.d)
      .filter((u) => !q || u.h.includes(q) || u.n.toLowerCase().includes(q))
      .slice(0, 40);

    replace(
      list,
      people.map((u: User) =>
        el(
          'button',
          {
            class: `person-row${selected.has(u.id) ? ' selected' : ''}`,
            on: {
              click: () => {
                if (selected.has(u.id)) selected.delete(u.id);
                else selected.add(u.id);
                submit.disabled = selected.size === 0;
                render();
              },
            },
          },
          avatar(u.id, u.n || u.h, 28),
          el(
            'span',
            { class: 'person-main' },
            el('span', { class: 'person-name', text: u.n || u.h }),
            el('span', { class: 'person-handle', text: `@${u.h}` }),
          ),
          el('span', { class: `presence presence-${store.presenceOf(u.id)}` }),
        ),
      ),
    );
  };

  search.addEventListener('input', render);
  submit.addEventListener('click', async () => {
    submit.disabled = true;
    try {
      onOpened(await api.openDm([...selected]));
      d.close();
    } catch (err) {
      showError(error, err);
      submit.disabled = false;
    }
  });

  render();
  search.focus();
}

/**
 * Add people to a channel.
 *
 * This is the only route into a private channel, so it is deliberately a
 * member-facing action rather than something buried in an admin screen.
 * `existing` is excluded from the list — offering to add someone who is
 * already there just invites a confusing no-op.
 */
export function addMembersDialog(
  store: Store,
  channel: Id,
  existing: Iterable<Id>,
  onAdded: (added: Id[]) => void,
): void {
  const already = new Set<Id>(existing);
  const selected = new Set<Id>();
  const search = el('input', {
    class: 'modal-input',
    placeholder: 'Find people…',
    'aria-label': 'Find people',
  }) as HTMLInputElement;
  const list = el('div', { class: 'modal-list' });
  const error = errorLine();
  const submit = el('button', {
    class: 'modal-submit',
    text: 'Add',
    disabled: true,
  }) as HTMLButtonElement;

  const d = dialog('Add people', search, list, error, submit);

  const render = () => {
    const q = search.value.trim().toLowerCase();
    const people = [...store.users().values()]
      .filter((u) => !already.has(u.id) && !u.d)
      .filter((u) => !q || u.h.includes(q) || u.n.toLowerCase().includes(q))
      .slice(0, 40);

    if (people.length === 0) {
      replace(list, [
        el('div', {
          class: 'empty',
          text: q ? 'Nobody matches that.' : 'Everyone is already here.',
        }),
      ]);
      return;
    }

    replace(
      list,
      people.map((u: User) =>
        el(
          'button',
          {
            class: `person-row${selected.has(u.id) ? ' selected' : ''}`,
            on: {
              click: () => {
                if (selected.has(u.id)) selected.delete(u.id);
                else selected.add(u.id);
                submit.disabled = selected.size === 0;
                render();
              },
            },
          },
          avatar(u.id, u.n || u.h, 28),
          el(
            'span',
            { class: 'person-main' },
            el('span', { class: 'person-name', text: u.n || u.h }),
            el('span', { class: 'person-handle', text: `@${u.h}` }),
          ),
          el('span', { class: `presence presence-${store.presenceOf(u.id)}` }),
        ),
      ),
    );
  };

  search.addEventListener('input', render);
  submit.addEventListener('click', async () => {
    submit.disabled = true;
    try {
      const { added } = await api.addMembers(channel, [...selected]);
      onAdded(added);
      d.close();
    } catch (err) {
      showError(error, err);
      submit.disabled = false;
    }
  });

  render();
  search.focus();
}

/**
 * Administer accounts: grant or revoke administrator, deactivate or reactivate.
 *
 * Deactivation rather than deletion — the account's messages, mentions and
 * thread structure stay intact, which is why the copy says "deactivate" and
 * never "delete".
 */
export function manageUsersDialog(store: Store): void {
  const list = el('div', { class: 'modal-list' });
  const error = errorLine();
  const search = el('input', {
    class: 'modal-input',
    placeholder: 'Find people…',
    'aria-label': 'Find people',
  }) as HTMLInputElement;

  dialog(
    'Manage people',
    el('p', {
      class: 'modal-hint',
      text: 'Deactivating an account signs it out everywhere and blocks sign-in. Its messages stay where they are.',
    }),
    search,
    list,
    error,
  );

  const meId = store.me()?.id;

  const apply = async (user: User, patch: { admin?: boolean; deactivated?: boolean }) => {
    try {
      const updated = await api.adminUpdateUser(user.id, patch);
      // The server also broadcasts a `user_upd`, but folding the response in
      // directly means the row redraws without waiting for the round trip.
      store.users.update((prev) => new Map(prev).set(updated.id, updated));
      error.hidden = true;
      render();
    } catch (err) {
      showError(error, err);
    }
  };

  const render = () => {
    const q = search.value.trim().toLowerCase();
    const people = [...store.users().values()]
      .filter((u) => !q || u.h.includes(q) || u.n.toLowerCase().includes(q))
      .sort((a, b) => (a.n || a.h).localeCompare(b.n || b.h));

    replace(
      list,
      people.map((u: User) => {
        // The server refuses both of these applied to yourself, so that a
        // workspace cannot end up with nobody able to administer it. Reflect
        // that here rather than offering a button that always fails.
        const isMe = u.id === meId;
        return el(
          'div',
          { class: `manage-row${u.d ? ' deactivated' : ''}` },
          avatar(u.id, u.n || u.h, 28),
          el(
            'span',
            { class: 'person-main' },
            el('span', { class: 'person-name', text: u.n || u.h }),
            el('span', { class: 'person-handle', text: `@${u.h}` }),
          ),
          u.adm ? el('span', { class: 'badge', text: 'ADMIN' }) : null,
          u.d ? el('span', { class: 'badge muted-badge', text: 'INACTIVE' }) : null,
          el('button', {
            class: 'manage-action',
            text: u.adm ? 'Revoke admin' : 'Make admin',
            disabled: isMe,
            title: isMe ? 'Another administrator has to change your own role' : '',
            on: { click: () => void apply(u, { admin: !u.adm }) },
          }),
          el('button', {
            class: `manage-action${u.d ? '' : ' danger'}`,
            text: u.d ? 'Reactivate' : 'Deactivate',
            disabled: isMe,
            title: isMe ? 'Another administrator has to deactivate you' : '',
            on: { click: () => void apply(u, { deactivated: !u.d }) },
          }),
        );
      }),
    );
  };

  search.addEventListener('input', render);
  render();
  search.focus();
}

/**
 * Bots and their API tokens.
 *
 * The secret is shown exactly once, in the response that mints it, and is
 * unrecoverable afterwards — so the dialog puts it in a copyable box with an
 * explicit warning rather than tucking it away somewhere it could be missed.
 */
export function manageBotsDialog(store: Store): void {
  const list = el('div', { class: 'modal-list' });
  const error = errorLine();
  const handle = el('input', {
    class: 'modal-input',
    placeholder: 'New bot handle, e.g. deploybot',
    'aria-label': 'New bot handle',
  }) as HTMLInputElement;
  const create = el('button', { class: 'modal-submit', text: 'Create bot' }) as HTMLButtonElement;

  dialog(
    'Bots & integrations',
    el('p', {
      class: 'modal-hint',
      text: 'A bot posts through the API. Add it to a channel like anyone else — that is what decides where it can read and write.',
    }),
    handle,
    create,
    error,
    list,
  );

  const showSecret = (secret: string) => {
    const box = el('input', {
      class: 'modal-input secret-box',
      value: secret,
      readonly: true,
      'aria-label': 'New token',
    }) as HTMLInputElement;
    const note = el(
      'div',
      { class: 'modal-note' },
      el('div', { text: 'Copy this now — it will not be shown again.' }),
      box,
    );
    list.prepend(note);
    box.focus();
    box.select();
  };

  const render = async () => {
    try {
      const bots = await api.bots();
      const withTokens = await Promise.all(
        bots.map(async (b) => ({ bot: b, tokens: await api.botTokens(b.id) })),
      );
      replace(
        list,
        withTokens.length === 0
          ? [el('div', { class: 'empty', text: 'No bots yet.' })]
          : withTokens.map(({ bot, tokens }) =>
              el(
                'div',
                { class: 'bot-block' },
                el(
                  'div',
                  { class: 'manage-row' },
                  avatar(bot.id, bot.n || bot.h, 28),
                  el(
                    'span',
                    { class: 'person-main' },
                    el('span', { class: 'person-name', text: bot.n || bot.h }),
                    el('span', { class: 'person-handle', text: `@${bot.h}` }),
                  ),
                  el('span', { class: 'badge', text: 'APP' }),
                  el('button', {
                    class: 'manage-action',
                    text: 'New token',
                    on: {
                      click: async () => {
                        const label = prompt('What is this token for?', 'integration');
                        if (!label) return;
                        try {
                          const t = await api.createBotToken(bot.id, label);
                          await render();
                          if (t.secret) showSecret(t.secret);
                        } catch (err) {
                          showError(error, err);
                        }
                      },
                    },
                  }),
                ),
                ...tokens.map((t) =>
                  el(
                    'div',
                    { class: 'token-row' },
                    el('span', { class: 'token-label', text: t.label }),
                    el('span', {
                      class: 'token-used',
                      text: t.last_used
                        ? `last used ${new Date(t.last_used).toLocaleDateString()}`
                        : 'never used',
                    }),
                    el('button', {
                      class: 'manage-action danger',
                      text: 'Revoke',
                      on: {
                        click: async () => {
                          await api.revokeBotToken(t.id);
                          await render();
                        },
                      },
                    }),
                  ),
                ),
              ),
            ),
      );
    } catch (err) {
      showError(error, err);
    }
  };

  create.addEventListener('click', async () => {
    const raw = handle.value.trim().toLowerCase().replace(/^@/, '');
    if (!raw) return;
    create.disabled = true;
    try {
      const bot = await api.createBot(raw, raw);
      store.users.update((prev) => new Map(prev).set(bot.id, bot));
      handle.value = '';
      error.hidden = true;
      await render();
    } catch (err) {
      showError(error, err);
    } finally {
      create.disabled = false;
    }
  });

  void render();
  handle.focus();
}

/**
 * Describe an invite's remaining life in the terms an administrator thinks in.
 *
 * Exported for tests: the interesting cases are the boundaries (spent, expired,
 * unlimited), and they are easier to pin down here than through the DOM.
 */
export function describeInvite(inv: Invite, now: number): string {
  if (!inv.live) {
    // Say *why* it is dead. "Expired" and "all used up" call for different
    // fixes: one needs a longer link, the other needs a bigger one.
    if (inv.expires_at != null && inv.expires_at <= now) return 'expired';
    return inv.max_uses === 1 ? 'used' : `all ${inv.max_uses} uses taken`;
  }

  const uses = inv.max_uses === 0 ? `${inv.uses} joined` : `${inv.uses}/${inv.max_uses} used`;
  if (inv.expires_at == null) return `${uses} · never expires`;

  const hours = Math.max(0, Math.round((inv.expires_at - now) / 3_600_000));
  const left = hours >= 48 ? `${Math.round(hours / 24)}d left` : `${hours}h left`;
  return `${uses} · ${left}`;
}

export function manageInvitesDialog(): void {
  const list = el('div', { class: 'modal-list' });
  const error = errorLine();

  const label = el('input', {
    class: 'modal-input',
    placeholder: 'What is this link for? (optional)',
    'aria-label': 'Invite label',
  }) as HTMLInputElement;

  const uses = el('select', { class: 'modal-input', 'aria-label': 'How many people' }) as
    HTMLSelectElement;
  for (const [value, text] of [
    ['1', 'One person'],
    ['5', 'Up to 5 people'],
    ['25', 'Up to 25 people'],
    ['0', 'No limit'],
  ] as const) {
    uses.appendChild(el('option', { value, text }));
  }

  const expiry = el('select', { class: 'modal-input', 'aria-label': 'How long' }) as
    HTMLSelectElement;
  for (const [value, text] of [
    ['24', 'Expires in a day'],
    ['168', 'Expires in a week'],
    ['720', 'Expires in 30 days'],
    ['0', 'Never expires'],
  ] as const) {
    expiry.appendChild(el('option', { value, text }));
  }
  expiry.value = '168';

  const create = el('button', {
    class: 'modal-submit',
    text: 'Create invite link',
  }) as HTMLButtonElement;

  dialog(
    'Invite people',
    el('p', {
      class: 'modal-hint',
      text: 'An invite link lets someone create an account even when registration is closed. It grants nothing else — they arrive as an ordinary member.',
    }),
    label,
    el('div', { class: 'modal-row' }, uses, expiry),
    create,
    error,
    list,
  );

  /**
   * Show a freshly minted link. It is the one and only time the token exists
   * outside the recipient's hands, so it is presented for copying rather than
   * buried in the list below.
   */
  const showLink = (token: string) => {
    const url = `${location.origin}${location.pathname}#/join/${token}`;
    const box = el('input', {
      class: 'modal-input secret-box',
      value: url,
      readonly: true,
      'aria-label': 'Invite link',
    }) as HTMLInputElement;
    const copy = el('button', {
      class: 'manage-action',
      text: 'Copy',
      on: {
        click: async () => {
          try {
            await navigator.clipboard.writeText(url);
            copy.textContent = 'Copied';
          } catch {
            // Clipboard access needs a secure origin; selecting the text is
            // the fallback that works everywhere.
            box.focus();
            box.select();
          }
        },
      },
    });
    list.prepend(
      el(
        'div',
        { class: 'modal-note' },
        el('div', { text: 'Copy this now — it will not be shown again.' }),
        el('div', { class: 'modal-row' }, box, copy),
      ),
    );
    box.focus();
    box.select();
  };

  const render = async () => {
    try {
      const invites = await api.invites();
      const now = Date.now();
      replace(
        list,
        invites.length === 0
          ? [el('div', { class: 'empty', text: 'No invite links yet.' })]
          : invites.map((inv) =>
              el(
                'div',
                { class: `token-row${inv.live ? '' : ' is-dead'}` },
                el('span', {
                  class: 'token-label',
                  text: inv.label || 'Invite link',
                }),
                el('span', { class: 'token-used', text: describeInvite(inv, now) }),
                el('button', {
                  class: 'manage-action danger',
                  text: inv.live ? 'Revoke' : 'Remove',
                  on: {
                    click: async () => {
                      try {
                        await api.revokeInvite(inv.id);
                        await render();
                      } catch (err) {
                        showError(error, err);
                      }
                    },
                  },
                }),
              ),
            ),
      );
    } catch (err) {
      showError(error, err);
    }
  };

  create.addEventListener('click', async () => {
    create.disabled = true;
    error.hidden = true;
    try {
      const inv = await api.createInvite({
        label: label.value.trim(),
        max_uses: Number(uses.value),
        expires_in_hours: Number(expiry.value),
      });
      label.value = '';
      await render();
      if (inv.token) showLink(inv.token);
    } catch (err) {
      showError(error, err);
    } finally {
      create.disabled = false;
    }
  });

  void render();
  label.focus();
}

/**
 * The three-way theme control.
 *
 * A segmented control rather than a checkbox, because there are genuinely three
 * answers: a checkbox could only express light-or-dark and would lose "follow
 * the system", which is both the default and the one that keeps working when
 * the machine switches at dusk.
 */
function themeChoice(): HTMLElement {
  const group = el('div', { class: 'theme-choice', role: 'radiogroup', 'aria-label': 'Theme' });
  const options: [ThemePreference, string][] = [
    ['system', 'System'],
    ['light', 'Light'],
    ['dark', 'Dark'],
  ];

  const buttons = options.map(([value, label]) =>
    el('button', {
      class: 'theme-option',
      type: 'button',
      role: 'radio',
      text: label,
      on: {
        click: () => {
          setPreference(value);
          paint();
        },
      },
    }),
  );

  function paint(): void {
    const current = readPreference();
    buttons.forEach((b, i) => {
      const selected = options[i]![0] === current;
      b.classList.toggle('is-selected', selected);
      b.setAttribute('aria-checked', String(selected));
    });
  }

  paint();
  group.append(...buttons);
  return group;
}

export function preferencesDialog(
  store: Store,
  notifier: Notifier,
  onSaved: (u: User) => void,
  onLogout: () => void,
): void {
  const me = store.me();
  if (!me) return;

  const displayName = el('input', {
    class: 'modal-input',
    value: me.n,
    'aria-label': 'Display name',
  }) as HTMLInputElement;
  const status = el('input', {
    class: 'modal-input',
    value: me.st ?? '',
    placeholder: 'What are you up to?',
    'aria-label': 'Status',
  }) as HTMLInputElement;
  const error = errorLine();
  const submit = el('button', { class: 'modal-submit', text: 'Save' }) as HTMLButtonElement;

  const password = (label: string, autocomplete: string) =>
    el('input', {
      class: 'modal-input',
      type: 'password',
      placeholder: label,
      autocomplete,
      'aria-label': label,
    }) as HTMLInputElement;

  const notifyToggle = el('input', {
    type: 'checkbox',
    checked: notifier.enabled(),
  }) as HTMLInputElement;
  const notifyNote = el('p', { class: 'modal-hint', hidden: true, role: 'status' });

  notifyToggle.addEventListener('change', () => {
    const asked = notifyToggle.checked;
    // Permission can only be requested from a user gesture, which is exactly
    // what this handler is — so the prompt appears rather than being ignored.
    void notifier.setEnabled(asked).then((on) => {
      // The checkbox follows what actually happened, not what was clicked: a
      // browser that blocks notifications must not leave it looking enabled.
      notifyToggle.checked = on;
      if (asked && !on) {
        notifyNote.textContent =
          'Your browser is blocking notifications for this site. Allow them in its site settings to turn this on.';
        notifyNote.hidden = false;
      } else {
        notifyNote.hidden = true;
      }
    });
  });

  const currentPassword = password('Current password', 'current-password');
  const newPassword = password('New password', 'new-password');
  const confirmPassword = password('Confirm new password', 'new-password');
  const passwordNote = el('div', { class: 'modal-note', hidden: true, role: 'status' });
  const passwordError = errorLine();
  const changePassword = el('button', {
    class: 'modal-submit',
    text: 'Change password',
  }) as HTMLButtonElement;
  const signOutOthers = el('button', {
    class: 'modal-secondary',
    text: 'Sign out other devices',
  }) as HTMLButtonElement;

  const d = dialog(
    'Preferences',
    el('p', { class: 'modal-hint', text: `Signed in as @${me.h}` }),
    displayName,
    status,
    error,
    submit,

    // Only administrators see this, and the server enforces the same rule —
    // hiding the button is a courtesy, not the check.
    me.adm
      ? el('hr', { class: 'modal-divider' })
      : null,
    me.adm
      ? el('button', {
          class: 'modal-secondary',
          text: 'Manage people',
          on: {
            click: () => {
              d.close();
              manageUsersDialog(store);
            },
          },
        })
      : null,
    me.adm
      ? el('button', {
          class: 'modal-secondary',
          text: 'Invite people',
          on: {
            click: () => {
              d.close();
              manageInvitesDialog();
            },
          },
        })
      : null,
    me.adm
      ? el('button', {
          class: 'modal-secondary',
          text: 'Bots & integrations',
          on: {
            click: () => {
              d.close();
              manageBotsDialog(store);
            },
          },
        })
      : null,

    el('hr', { class: 'modal-divider' }),
    el('h3', { class: 'modal-section', text: 'Appearance' }),
    themeChoice(),

    el('hr', { class: 'modal-divider' }),
    el('h3', { class: 'modal-section', text: 'Notifications' }),
    notifier.supported
      ? el(
          'label',
          { class: 'modal-check' },
          notifyToggle,
          el('span', { text: 'Notify me about mentions and direct messages' }),
        )
      : el('p', {
          class: 'modal-hint',
          text: 'This browser does not support desktop notifications.',
        }),
    notifyNote,

    el('hr', { class: 'modal-divider' }),
    el('h3', { class: 'modal-section', text: 'Password' }),
    el('p', {
      class: 'modal-hint',
      text: 'Changing your password signs out every other device.',
    }),
    currentPassword,
    newPassword,
    confirmPassword,
    passwordNote,
    passwordError,
    changePassword,
    signOutOthers,

    el('hr', { class: 'modal-divider' }),
    el('button', {
      class: 'modal-danger',
      text: 'Sign out',
      on: { click: () => { d.close(); onLogout(); } },
    }),
  );

  submit.addEventListener('click', async () => {
    submit.disabled = true;
    try {
      onSaved(
        await api.updateMe({
          display_name: displayName.value.trim(),
          status: status.value.trim(),
        }),
      );
      d.close();
    } catch (err) {
      showError(error, err);
      submit.disabled = false;
    }
  });

  /** Report an outcome in place. The dialog stays open — you may want both. */
  const note = (text: string) => {
    passwordNote.textContent = text;
    passwordNote.hidden = false;
    passwordError.hidden = true;
  };

  changePassword.addEventListener('click', async () => {
    passwordNote.hidden = true;
    // Checked here as well as on the server, because a typo in the confirmation
    // is the one failure the server cannot see.
    if (newPassword.value !== confirmPassword.value) {
      showError(passwordError, new ApiError(400, 'bad_request', 'The new passwords do not match.'));
      return;
    }
    changePassword.disabled = true;
    try {
      const { revoked } = await api.changePassword(currentPassword.value, newPassword.value);
      currentPassword.value = newPassword.value = confirmPassword.value = '';
      note(
        revoked === 0
          ? 'Password changed.'
          : `Password changed. ${revoked} other ${revoked === 1 ? 'device' : 'devices'} signed out.`,
      );
    } catch (err) {
      showError(passwordError, err);
    } finally {
      changePassword.disabled = false;
    }
  });

  signOutOthers.addEventListener('click', async () => {
    signOutOthers.disabled = true;
    try {
      const { revoked } = await api.revokeOtherSessions();
      note(
        revoked === 0
          ? 'No other devices were signed in.'
          : `${revoked} other ${revoked === 1 ? 'device' : 'devices'} signed out.`,
      );
    } catch (err) {
      showError(passwordError, err);
    } finally {
      signOutOthers.disabled = false;
    }
  });

  displayName.focus();
}
