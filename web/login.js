/**
 * Sign-in panel. Renders into `#login-view`, posts to `/api/v1/auth/login` and
 * hands control back to the shell once the session cookie / token pair exists.
 */

import { API_BASE, ApiError, request, setTokens } from './api.js';
import { byId, setHidden, setText } from './dom.js';
import { toastSuccess } from './toast.js';

/** @type {null | (() => void | Promise<void>)} */
let onSignedIn = null;

/** @param {() => void | Promise<void>} handler */
export function setSignedInHandler(handler) {
  onSignedIn = handler;
}

const EMAIL_RE = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

function showFieldError(id, message) {
  const node = byId(id);
  if (message) setText(node, message);
  setHidden(node, !message);
}

/**
 * Wire the form once at boot.
 * @returns {() => void} a function that shows the panel and focuses the first field
 */
export function initLogin() {
  const form = byId('login-form');
  const email = byId('login-email');
  const password = byId('login-password');
  const submit = byId('login-submit');

  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    showFieldError('login-email-error', '');
    showFieldError('login-password-error', '');
    showFieldError('login-error', '');

    const address = email.value.trim();
    const secret = password.value;
    let invalid = false;

    if (address === '') {
      showFieldError('login-email-error', 'Enter your email address.');
      invalid = true;
    } else if (!EMAIL_RE.test(address)) {
      showFieldError('login-email-error', 'That does not look like an email address.');
      invalid = true;
    }
    if (secret === '') {
      showFieldError('login-password-error', 'Enter your password.');
      invalid = true;
    }
    if (invalid) {
      (invalid && address === '' ? email : password).focus();
      return;
    }

    submit.disabled = true;
    submit.textContent = 'Signing in…';
    try {
      const payload = await request(`${API_BASE}/auth/login`, {
        method: 'POST',
        body: { email: address, password: secret },
        toast: false,
        retryOn401: false,
      });
      setTokens(payload);
      password.value = '';
      toastSuccess('Signed in.');
      if (onSignedIn) await onSignedIn();
    } catch (error) {
      if (error instanceof ApiError && error.status === 401) {
        showFieldError('login-error', 'Wrong address or password.');
      } else if (error instanceof ApiError && error.status === 429) {
        showFieldError('login-error', error.message);
      } else if (error instanceof ApiError && error.network) {
        showFieldError('login-error', 'The server could not be reached. Check that Ferroma is running.');
      } else {
        showFieldError('login-error', error instanceof Error ? error.message : 'Sign-in failed.');
      }
      password.focus();
      password.select();
    } finally {
      submit.disabled = false;
      submit.textContent = 'Sign in';
    }
  });

  return function showLogin() {
    setHidden(byId('login-view'), false);
    setHidden(byId('app-view'), true);
    window.setTimeout(() => email.focus(), 0);
  };
}
