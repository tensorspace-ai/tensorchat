/**
 * The sign-in / sign-up screen.
 *
 * Deliberately one screen with a mode toggle rather than two routes: it is the
 * only pre-authentication surface in the app, and keeping it in one place means
 * the session-establishing code path exists exactly once.
 */

import { el, replace } from '../dom.ts';
import { ApiError, api, setToken } from '../api.ts';
import type { User } from '../protocol.ts';

/**
 * Pull an invite token out of `#/join/{token}`, if the URL carries one.
 *
 * The fragment rather than a query string, deliberately: fragments are not sent
 * to the server on a page load and do not end up in access logs, which matters
 * for a credential that is live until it is spent.
 */
export function inviteFromLocation(hash: string): string | null {
  const m = /^#\/join\/([^/?#]+)$/.exec(hash);
  return m ? decodeURIComponent(m[1]!) : null;
}

export function LoginScreen(onAuthenticated: (user: User) => void): HTMLElement {
  const invite = inviteFromLocation(location.hash);
  // An invite link is an instruction to create an account, so open on the
  // sign-up form rather than making the recipient find the toggle.
  let mode: 'login' | 'register' = invite ? 'register' : 'login';
  let busy = false;
  // null while the check is in flight, so the form does not flash a rejection
  // before the server has answered.
  let inviteValid: boolean | null = invite ? null : false;

  const error = el('div', { class: 'auth-error', hidden: true, role: 'alert' });
  const form = el('form', { class: 'auth-form' });
  const root = el(
    'div',
    { class: 'auth-screen' },
    el(
      'div',
      { class: 'auth-card' },
      el('h1', { class: 'auth-title', text: 'TensorChat' }),
      el('p', { class: 'auth-sub', text: 'Fast, self-hosted team chat.' }),
      form,
    ),
  );

  const handle = el('input', {
    class: 'auth-input',
    type: 'text',
    placeholder: 'handle',
    autocomplete: 'username',
    required: 'required',
  }) as HTMLInputElement;

  const displayName = el('input', {
    class: 'auth-input',
    type: 'text',
    placeholder: 'display name',
    autocomplete: 'name',
  }) as HTMLInputElement;

  const password = el('input', {
    class: 'auth-input',
    type: 'password',
    placeholder: 'password',
    autocomplete: 'current-password',
    required: 'required',
  }) as HTMLInputElement;

  const submit = el('button', {
    class: 'auth-submit',
    type: 'submit',
    text: 'Sign in',
  }) as HTMLButtonElement;

  const toggle = el('button', {
    class: 'auth-toggle',
    type: 'button',
    text: 'Create an account',
    on: {
      click: () => {
        mode = mode === 'login' ? 'register' : 'login';
        render();
      },
    },
  });

  const inviteNote = el('p', { class: 'auth-invite' });

  /**
   * The "Sign in with …" button, once the server has said there is a provider.
   *
   * Built lazily rather than hidden, so a server without one renders exactly
   * the form it rendered before this existed.
   */
  let providerButton: HTMLElement | null = null;

  function render(): void {
    const registering = mode === 'register';
    password.autocomplete = registering ? 'new-password' : 'current-password';
    submit.textContent = registering ? 'Create account' : 'Sign in';
    toggle.textContent = registering ? 'I already have an account' : 'Create an account';

    // The banner only belongs on the sign-up form: an invite says nothing about
    // signing in to an account you already have.
    const showInvite = invite !== null && registering;
    if (showInvite) {
      inviteNote.textContent =
        inviteValid === null
          ? 'Checking your invite…'
          : inviteValid
            ? "You have been invited. Pick a handle and you're in."
            : 'That invite link has expired or has already been used.';
      inviteNote.classList.toggle('is-dead', inviteValid === false);
    }
    // Nothing to submit against a dead link, and disabling says so before the
    // person types out a password.
    submit.disabled = busy || (showInvite && inviteValid === false);

    replace(form, [
      showInvite ? inviteNote : null,
      handle,
      registering ? displayName : null,
      password,
      error,
      submit,
      toggle,
      providerButton,
    ]);
    if (!submit.disabled) handle.focus();
  }

  // Fire and forget, like the invite check above: if this never answers, the
  // password form is still perfectly usable.
  void api
    .authProviders()
    .then((p) => {
      if (!p.oidc) return;
      providerButton = el(
        'div',
        { class: 'auth-provider' },
        el('div', { class: 'auth-or', text: 'or' }),
        el('button', {
          class: 'auth-provider-button',
          type: 'button',
          text: `Sign in with ${p.oidc.label}`,
          on: {
            // A full navigation, not a fetch. The provider answers with a
            // redirect to its own login page, which an XHR cannot follow —
            // and the CSP would not allow reaching it if it could.
            click: () => location.assign('/api/oauth/start'),
          },
        }),
      );
      render();
    })
    .catch(() => {
      // No providers, or no answer. Either way there is no button to draw.
    });

  if (invite) {
    // Fire and forget: a failure here only downgrades the banner, and the
    // server re-checks under the write lock when the form is actually
    // submitted. This is a courtesy, not the enforcement.
    void api
      .checkInvite(invite)
      .then((r) => {
        inviteValid = r.valid;
      })
      .catch(() => {
        inviteValid = false;
      })
      .finally(render);
  }

  function showError(message: string): void {
    error.textContent = message;
    error.hidden = false;
  }

  form.addEventListener('submit', async (ev: Event) => {
    ev.preventDefault();
    if (busy) return;
    error.hidden = true;

    const h = handle.value.trim().toLowerCase().replace(/^@/, '');
    const p = password.value;
    if (!h || !p) {
      showError('Enter a handle and a password.');
      return;
    }

    busy = true;
    submit.disabled = true;
    submit.textContent = mode === 'register' ? 'Creating…' : 'Signing in…';
    try {
      const session =
        mode === 'register'
          ? await api.register(h, displayName.value.trim() || h, p, invite ?? undefined)
          : await api.login(h, p);
      setToken(session.token);
      // Drop the token out of the address bar before the app boots, so a
      // spent invite is not left sitting in history or a shared screenshot.
      if (invite) history.replaceState(null, '', location.pathname + location.search);
      onAuthenticated(session.user);
    } catch (err) {
      showError(
        err instanceof ApiError
          ? err.status === 401
            ? 'That handle and password do not match.'
            : err.message
          : 'Could not reach the server.',
      );
      // Never leave a password sitting in the DOM after a failure.
      password.value = '';
      password.focus();
    } finally {
      // `render` owns the disabled state now — it also has to account for a
      // dead invite, so setting it here as well would fight with that.
      busy = false;
      render();
    }
  });

  render();
  return root;
}
