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
import type { Channel, Id, User } from '../protocol.ts';
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
