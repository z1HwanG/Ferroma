/**
 * Settings dialog: display name, signature, theme, messages per page, the
 * "mark read on open" switch, and the account password.
 *
 * The preferences are per-browser and live in localStorage (see `store.js`); the
 * password is the one thing here that belongs to the *account*, so it is the one
 * thing that goes to the server (`POST /api/v1/auth/password`).
 */

import { API_BASE, ApiError, request } from './api.js';
import { el, setHidden, setText } from './dom.js';
import { openModal } from './modal.js';
import { MESSAGES_PER_PAGE_CHOICES, PREF_DEFAULTS, getPrefs, savePrefs } from './store.js';
import { THEME_MODES, currentTheme, setTheme } from './theme.js';
import { toastError, toastSuccess } from './toast.js';

/** Save a value and keep the theme module in step. */
function persist(patch) {
  const next = savePrefs(patch);
  if (patch.theme) setTheme(patch.theme);
  return next;
}

/** Field ids used by this dialog. */
const FIELDS = {
  displayName: 'settings-display-name',
  signature: 'settings-signature',
  theme: 'settings-theme',
  perPage: 'settings-per-page',
  markReadOnOpen: 'settings-mark-read',
  passwordCurrent: 'settings-password-current',
  passwordNew: 'settings-password-new',
  passwordConfirm: 'settings-password-confirm',
  passwordStatus: 'settings-password-status',
};

export function openSettings() {
  const prefs = getPrefs();

  const displayName = el('input', {
    class: 'input',
    id: FIELDS.displayName,
    type: 'text',
    autocomplete: 'name',
    value: prefs.displayName,
  });

  const signature = el('textarea', { class: 'input', id: FIELDS.signature, rows: '6', spellcheck: 'true' });
  signature.value = prefs.signature;

  const theme = el('select', { class: 'input', id: FIELDS.theme });
  for (const mode of THEME_MODES) {
    theme.append(
      el('option', {
        value: mode,
        text: mode === 'auto' ? 'Follow the system' : mode === 'light' ? 'Light' : 'Dark',
      }),
    );
  }
  theme.value = currentTheme();

  const perPage = el('select', { class: 'input', id: FIELDS.perPage });
  for (const choice of MESSAGES_PER_PAGE_CHOICES) {
    perPage.append(el('option', { value: String(choice), text: `${choice} messages` }));
  }
  perPage.value = String(prefs.perPage);

  const markRead = el('input', { type: 'checkbox', id: FIELDS.markReadOnOpen });
  markRead.checked = prefs.markReadOnOpen;

  /* ------------------------------------------------------------- password */

  const currentPassword = el('input', {
    class: 'input',
    id: FIELDS.passwordCurrent,
    type: 'password',
    autocomplete: 'current-password',
  });
  const newPassword = el('input', {
    class: 'input',
    id: FIELDS.passwordNew,
    type: 'password',
    autocomplete: 'new-password',
  });
  const confirmPassword = el('input', {
    class: 'input',
    id: FIELDS.passwordConfirm,
    type: 'password',
    autocomplete: 'new-password',
  });
  const passwordStatus = el('p', { class: 'field-error', id: FIELDS.passwordStatus, role: 'alert', hidden: true });
  const changePassword = el('button', { type: 'submit', class: 'btn', text: 'Change password' });

  const passwordForm = el('form', { class: 'settings-grid', id: 'settings-password' }, [
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.passwordCurrent, text: 'Current password' }),
      currentPassword,
    ]),
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.passwordNew, text: 'New password' }),
      newPassword,
    ]),
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.passwordConfirm, text: 'Repeat the new password' }),
      confirmPassword,
    ]),
    el('div', { class: 'field' }, [changePassword, passwordStatus]),
  ]);

  /** Show or clear the inline message under the password form. */
  const showPasswordStatus = (message) => {
    setText(passwordStatus, message);
    setHidden(passwordStatus, message === '');
  };

  passwordForm.addEventListener('submit', async (event) => {
    event.preventDefault();
    showPasswordStatus('');

    const secret = currentPassword.value;
    const replacement = newPassword.value;
    if (secret === '' || replacement === '') {
      showPasswordStatus('Fill in your current password and the new one.');
      return;
    }
    if (replacement !== confirmPassword.value) {
      showPasswordStatus('The two new passwords do not match.');
      return;
    }
    if (replacement === secret) {
      showPasswordStatus('The new password must differ from the current one.');
      return;
    }

    changePassword.disabled = true;
    setText(changePassword, 'Changing…');
    try {
      await request(`${API_BASE}/auth/password`, {
        method: 'POST',
        body: { current_password: secret, new_password: replacement },
        toast: false,
      });
      currentPassword.value = '';
      newPassword.value = '';
      confirmPassword.value = '';
      toastSuccess('Password changed.');
      showPasswordStatus('');
    } catch (error) {
      const message = error instanceof ApiError ? error.message : 'The password could not be changed.';
      showPasswordStatus(message);
      toastError(message);
    } finally {
      changePassword.disabled = false;
      setText(changePassword, 'Change password');
    }
  });

  const body = el('div', { class: 'settings-grid' }, [
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.displayName, text: 'Display name' }),
      displayName,
      el('p', { class: 'modal-message', text: 'Used for the From line of new messages.' }),
    ]),
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.signature, text: 'Signature' }),
      signature,
      el('p', { class: 'modal-message', text: 'Appended to every new message you compose.' }),
    ]),
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.theme, text: 'Theme' }),
      theme,
    ]),
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.perPage, text: 'Messages per page' }),
      perPage,
    ]),
    el('label', { class: 'checkbox', for: FIELDS.markReadOnOpen }, [
      markRead,
      el('span', { text: 'Mark messages read after 2 seconds in the reading pane' }),
    ]),
    el('h3', { class: 'card-title', text: 'Password' }),
    passwordForm,
  ]);

  const close = el('button', { type: 'button', class: 'btn', text: 'Close' });
  const reset = el('button', { type: 'button', class: 'btn', text: 'Restore defaults' });
  const save = el('button', { type: 'button', class: 'btn btn-primary', text: 'Save settings' });

  const modal = openModal({
    title: 'Settings',
    body,
    footer: [reset, el('span', { class: 'spacer' }), close, save],
    onMount: () => {
      close.addEventListener('click', () => modal.close('close'));
      save.addEventListener('click', () => {
        const perPageValue = Number.parseInt(perPage.value, 10);
        persist({
          displayName: displayName.value.trim(),
          signature: signature.value,
          theme: theme.value,
          perPage: MESSAGES_PER_PAGE_CHOICES.includes(perPageValue) ? perPageValue : PREF_DEFAULTS.perPage,
          markReadOnOpen: markRead.checked,
        });
        toastSuccess('Settings saved.');
        modal.close('save');
      });
      reset.addEventListener('click', () => {
        persist(Object.assign({}, PREF_DEFAULTS));
        displayName.value = '';
        signature.value = '';
        theme.value = PREF_DEFAULTS.theme;
        perPage.value = String(PREF_DEFAULTS.perPage);
        markRead.checked = PREF_DEFAULTS.markReadOnOpen;
        toastSuccess('Settings restored to their defaults.');
      });
    },
  });
}
