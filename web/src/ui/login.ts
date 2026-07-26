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

export function LoginScreen(onAuthenticated: (user: User) => void): HTMLElement {
  let mode: 'login' | 'register' = 'login';
  let busy = false;

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

  function render(): void {
    const registering = mode === 'register';
    password.autocomplete = registering ? 'new-password' : 'current-password';
    submit.textContent = registering ? 'Create account' : 'Sign in';
    toggle.textContent = registering ? 'I already have an account' : 'Create an account';
    replace(form, [
      handle,
      registering ? displayName : null,
      password,
      error,
      submit,
      toggle,
    ]);
    handle.focus();
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
          ? await api.register(h, displayName.value.trim() || h, p)
          : await api.login(h, p);
      setToken(session.token);
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
      busy = false;
      submit.disabled = false;
      render();
    }
  });

  render();
  return root;
}
