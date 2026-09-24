/**
 * Settings dialog: display name, signature, theme, messages per page, the
 * "mark read on open" switch, the interface language, and the account password.
 *
 * The preferences are per-browser and live in localStorage (see `store.js`); the
 * password is the one thing here that belongs to the *account*, so it is the one
 * thing that goes to the server (`POST /api/v1/auth/password`).
 */

import { API_BASE, ApiError, request } from '../shared/api.js';
import { el, setHidden, setText, svgIcon } from '../shared/dom.js';
import { LOCALES, currentLocale, setLocale, t, tn } from '../shared/i18n.js';
import { openModal } from '../shared/modal.js';
import { MESSAGES_PER_PAGE_CHOICES, PREF_DEFAULTS, getPrefs, savePrefs } from './store.js';
import { THEME_MODES, currentTheme, setTheme } from '../shared/theme.js';
import { relativeStamp } from '../shared/format.js';
import { toastError, toastSuccess } from '../shared/toast.js';

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
  language: 'settings-language',
  perPage: 'settings-per-page',
  markReadOnOpen: 'settings-mark-read',
  passwordCurrent: 'settings-password-current',
  passwordNew: 'settings-password-new',
  passwordConfirm: 'settings-password-confirm',
  passwordStatus: 'settings-password-status',
  totpStatus: 'settings-totp-status',
  totpEnroll: 'settings-totp-enroll',
  totpPane: 'settings-totp-pane',
  totpSecret: 'settings-totp-secret',
  totpUri: 'settings-totp-uri',
  totpCode: 'settings-totp-code',
  totpConfirm: 'settings-totp-confirm',
  totpRecovery: 'settings-totp-recovery',
  totpDisable: 'settings-totp-disable',
  totpDisablePane: 'settings-totp-disable-pane',
  totpPassword: 'settings-totp-password',
  totpOff: 'settings-totp-off',
  totpError: 'settings-totp-error',
  appPasswordLabel: 'settings-app-password-label',
  appPasswordCreate: 'settings-app-password-create',
  appPasswordNew: 'settings-app-password-new',
  appPasswordList: 'settings-app-password-list',
};

/** The label for one theme mode. */
function themeLabel(mode) {
  if (mode === 'auto') return t('Follow the system');
  if (mode === 'light') return t('Light');
  return t('Dark');
}

/** The glyph for one theme mode. */
function themeGlyph(mode) {
  if (mode === 'auto') return 'contrast';
  if (mode === 'light') return 'sun';
  return 'moon';
}

/**
 * The three-way theme picker.
 *
 * A `<select>` cannot show what the three choices *are*; three glyphs can, and the
 * sliding selection state makes the current mode obvious at a glance. The control keeps
 * the field id so the label, the checker and the save path all still find it.
 *
 * @returns {{node: HTMLElement, selected: () => string, select: (mode: string) => void}}
 */
function themeControl() {
  const node = el('div', {
    class: 'segmented',
    id: FIELDS.theme,
    role: 'radiogroup',
    'aria-label': t('Theme'),
  });
  const buttons = new Map();

  const select = (mode) => {
    const wanted = THEME_MODES.includes(mode) ? mode : PREF_DEFAULTS.theme;
    for (const [value, button] of buttons) {
      button.setAttribute('aria-checked', value === wanted ? 'true' : 'false');
    }
    node.dataset.mode = wanted;
  };

  for (const mode of THEME_MODES) {
    const label = themeLabel(mode);
    const button = el('button', {
      type: 'button',
      class: 'segmented-option',
      role: 'radio',
      'aria-checked': 'false',
      title: label,
      dataset: { mode },
    }, [svgIcon(themeGlyph(mode)), el('span', { class: 'segmented-label', text: label })]);
    button.addEventListener('click', () => select(mode));
    buttons.set(mode, button);
    node.append(button);
  }

  select(currentTheme());
  return {
    node,
    selected: () => node.dataset.mode || currentTheme(),
    select,
  };
}


/**
 * The security section: the second factor and the application passwords.
 *
 * Three things about it are deliberate.
 *
 * There is no QR image. Rendering one would mean either a QR library or a request to
 * somebody else's service, and shipping the second is out of the question for a page
 * that is holding a shared secret. The secret is shown in groups an authenticator can
 * accept by hand, with the `otpauth://` URI beside it for an app that takes one.
 *
 * The recovery codes and a new application password are shown **once**, because only
 * digests are stored on the server. The panel says so rather than leaving the user to
 * discover it.
 *
 * Turning the factor off asks for the account password. A stolen session is the case
 * the factor exists for, so the session alone must not be able to remove it.
 *
 * @returns {{node: HTMLElement, reload: () => Promise<void>}}
 */
function securitySection() {
  const status = el('p', { class: 'modal-message', id: FIELDS.totpStatus, text: t('Loading…') });
  const error = el('p', { class: 'field-error', id: FIELDS.totpError, role: 'alert', hidden: true });
  const showError = (message) => {
    setText(error, message);
    setHidden(error, message === '');
  };

  /* ------------------------------------------------------------ enrollment */

  const secret = el('code', { class: 'secret-value', id: FIELDS.totpSecret });
  const uri = el('code', { class: 'secret-value', id: FIELDS.totpUri });
  const code = el('input', {
    class: 'input',
    id: FIELDS.totpCode,
    type: 'text',
    inputmode: 'numeric',
    autocomplete: 'one-time-code',
    maxlength: '7',
  });
  const confirm = el('button', { type: 'button', class: 'btn btn-primary', id: FIELDS.totpConfirm, text: t('Confirm') });
  const recovery = el('ul', { class: 'secret-list', id: FIELDS.totpRecovery, hidden: true });
  const pane = el('div', { class: 'settings-grid', id: FIELDS.totpPane, hidden: true }, [
    el('div', { class: 'field' }, [
      el('p', { class: 'modal-message', text: t('Enter this secret in your authenticator app.') }),
      secret,
      el('p', { class: 'modal-message', text: t('Or use this URI if your app accepts one:') }),
      uri,
    ]),
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.totpCode, text: t('Code from the app') }),
      code,
      confirm,
    ]),
    el('div', { class: 'field' }, [
      el('p', { class: 'modal-message', text: t('Save these recovery codes now. Each one works once, and they are not shown again.') }),
      recovery,
    ]),
  ]);

  /* --------------------------------------------------------------- disable */

  const disablePassword = el('input', {
    class: 'input',
    id: FIELDS.totpPassword,
    type: 'password',
    autocomplete: 'current-password',
  });
  const disable = el('button', { type: 'button', class: 'btn btn-danger', id: FIELDS.totpOff, text: t('Turn off') });
  const disablePane = el('div', { class: 'settings-grid', id: FIELDS.totpDisablePane, hidden: true }, [
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.totpPassword, text: t('Confirm with your account password.') }),
      disablePassword,
      disable,
    ]),
  ]);

  const enroll = el('button', { type: 'button', class: 'btn', id: FIELDS.totpEnroll, text: t('Set up two-factor authentication') });

  /* ---------------------------------------------------- application passwords */

  const label = el('input', {
    class: 'input',
    id: FIELDS.appPasswordLabel,
    type: 'text',
    autocomplete: 'off',
    placeholder: t('What is it for?'),
  });
  const create = el('button', { type: 'button', class: 'btn', id: FIELDS.appPasswordCreate, text: t('Create') });
  const fresh = el('p', { class: 'modal-message', id: FIELDS.appPasswordNew, hidden: true });
  const list = el('ul', { class: 'secret-list', id: FIELDS.appPasswordList });

  /**
   * One application password row, with its own revoke button.
   *
   * @param {{id: number, label: string, last_used_at: string|null, revoked_at: string|null}} entry
   */
  const appPasswordRow = (entry) => {
    const when = entry.last_used_at
      ? t('Last used {when}', { when: relativeStamp(entry.last_used_at) })
      : t('Never used');
    const trailing = entry.revoked_at
      ? el('span', { class: 'modal-message', text: t('Revoked') })
      : el('button', { type: 'button', class: 'btn btn-small', text: t('Revoke') });
    const row = el('li', { class: 'secret-row' }, [
      el('span', { class: 'secret-name', text: entry.label }),
      el('span', { class: 'modal-message', text: when }),
      trailing,
    ]);
    if (!entry.revoked_at) {
      trailing.addEventListener('click', async () => {
        trailing.disabled = true;
        await revoke(entry.id);
      });
    }
    return row;
  };

  /**
   * Whether the enrollment pane must stay on screen.
   *
   * It is set while a secret we just issued is displayed, and it stays set after a
   * successful confirmation, because the recovery codes are shown exactly once —
   * hiding the pane there would throw them away in the same breath as showing them.
   */
  let paneSticky = false;

  /** Refresh the status line, the buttons and the application-password list. */
  const reload = async () => {
    showError('');
    try {
      const state = await request(`${API_BASE}/auth/totp`, { toast: false });
      const enabled = state.status === 'enabled';
      const pending = state.status === 'pending';
      setText(status, securityStatusLine(state.status));
      // Starting a new enrollment replaces an unconfirmed one, so the button is
      // offered whenever the factor is not already in force.
      setHidden(enroll, enabled);
      // The server never returns a secret it has stored, so a `pending` enrollment
      // from an earlier visit cannot be resumed: the pane only opens for a secret
      // this dialog just issued.
      setHidden(pane, !paneSticky);
      setHidden(disablePane, !enabled);
    } catch (failure) {
      setText(status, t('The security settings could not be loaded.'));
      showError(failure instanceof ApiError ? failure.message : t('The security settings could not be loaded.'));
    }

    try {
      const payload = await request(`${API_BASE}/auth/app-passwords`, { toast: false });
      const items = Array.isArray(payload.items) ? payload.items : [];
      list.replaceChildren();
      if (items.length === 0) {
        list.append(el('li', { class: 'modal-message', text: t('No application passwords yet.') }));
      }
      for (const entry of items) list.append(appPasswordRow(entry));
    } catch {
      list.replaceChildren();
    }
  };

  /** Create one application password and show its secret once. */
  const createPassword = async () => {
    const name = label.value.trim();
    if (name === '') {
      showError(t('Give the application password a name.'));
      return;
    }
    create.disabled = true;
    try {
      const payload = await request(`${API_BASE}/auth/app-passwords`, {
        method: 'POST',
        body: { label: name },
        toast: false,
      });
      setText(fresh, t('Copy this password now. It is not shown again: {secret}', { secret: payload.secret }));
      setHidden(fresh, false);
      label.value = '';
      showError('');
      await reload();
    } catch (failure) {
      showError(
        failure instanceof ApiError ? failure.message : t('The application password could not be created.'),
      );
    } finally {
      create.disabled = false;
    }
  };

  /** Revoke one application password by id. */
  const revoke = async (id) => {
    try {
      await request(`${API_BASE}/auth/app-passwords/${id}`, { method: 'DELETE', toast: false });
      await reload();
    } catch (failure) {
      showError(
        failure instanceof ApiError ? failure.message : t('The application password could not be revoked.'),
      );
    }
  };

  enroll.addEventListener('click', async () => {
    enroll.disabled = true;
    showError('');
    try {
      const payload = await request(`${API_BASE}/auth/totp/enroll`, { method: 'POST', toast: false });
      setText(secret, groupSecret(payload.secret));
      setText(uri, payload.uri);
      recovery.replaceChildren();
      setHidden(pane, false);
      setHidden(recovery, true);
      paneSticky = true;
      code.value = '';
      code.focus();
    } catch (failure) {
      showError(
        failure instanceof ApiError
          ? failure.message
          : t('Two-factor authentication could not be set up.'),
      );
    } finally {
      enroll.disabled = false;
    }
  });

  confirm.addEventListener('click', async () => {
    const entered = code.value.trim();
    if (entered === '') {
      showError(t('Enter the code your app is showing.'));
      return;
    }
    confirm.disabled = true;
    showError('');
    try {
      const payload = await request(`${API_BASE}/auth/totp/confirm`, {
        method: 'POST',
        body: { code: entered },
        toast: false,
      });
      const codes = Array.isArray(payload.recovery_codes) ? payload.recovery_codes : [];
      recovery.replaceChildren();
      for (const one of codes) recovery.append(el('li', { class: 'secret-row', text: one }));
      setHidden(recovery, false);
      // The codes stay on screen until the dialog closes; `reload` must not clear it.
      paneSticky = true;
      code.value = '';
      toastSuccess(t('Two-factor authentication is on.'));
      await reload();
    } catch (failure) {
      showError(
        failure instanceof ApiError ? failure.message : t('That code did not match.'),
      );
    } finally {
      confirm.disabled = false;
    }
  });

  disable.addEventListener('click', async () => {
    const password = disablePassword.value;
    if (password === '') {
      showError(t('Enter your account password.'));
      return;
    }
    disable.disabled = true;
    showError('');
    try {
      await request(`${API_BASE}/auth/totp/disable`, {
        method: 'POST',
        body: { password },
        toast: false,
        // A wrong password is a `401` here, exactly like an expired session; the
        // request must not be replayed through the refresh path.
        retryOn401: false,
      });
      disablePassword.value = '';
      paneSticky = false;
      toastSuccess(t('Two-factor authentication is off.'));
      await reload();
    } catch (failure) {
      showError(
        failure instanceof ApiError
          ? failure.message
          : t('Two-factor authentication could not be turned off.'),
      );
    } finally {
      disable.disabled = false;
    }
  });

  create.addEventListener('click', createPassword);

  const node = el('div', { class: 'settings-grid' }, [
    el('h3', { class: 'card-title', text: t('Two-factor authentication') }),
    el('div', { class: 'field' }, [status, enroll, pane, disablePane, error]),
    el('h3', { class: 'card-title', text: t('Application passwords') }),
    el('p', {
      class: 'modal-message',
      text: t('For mail clients that cannot ask for a code. Each one is a full credential for this account.'),
    }),
    el('div', { class: 'field' }, [label, create, fresh]),
    list,
  ]);

  return { node, reload };
}

/**
 * The one-line description of an enrollment state.
 *
 * Pure and exported so the wording rules — in particular that a `pending`
 * enrollment is *not* protection — can be asserted without a browser.
 *
 * @param {string} state
 * @returns {string}
 */
export function securityStatusLine(state) {
  if (state === 'enabled') return t('On. Mail clients sign in with an application password.');
  if (state === 'pending') return t('Waiting for a code to confirm the new secret.');
  return t('Off. A password is the only thing protecting this account.');
}

/**
 * Group a base32 secret in fours, which is how an authenticator app's manual entry
 * field is usually laid out and how a person reads a secret off a screen without
 * losing their place.
 *
 * @param {string} value
 */
export function groupSecret(value) {
  return String(value || '')
    .replace(/\s+/g, '')
    .replace(/(.{4})/g, '$1 ')
    .trim();
}

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

  const theme = themeControl();

  // Each option is labelled in its own language, so a reader who cannot read the
  // current one can still find theirs. The chosen value is *staged* here and applied
  // by Save: reloading on `change` switched the whole interface the moment the arrow
  // keys moved over the list, before the operator had confirmed anything — and the
  // dialog's own buttons said the settings were still unsaved.
  const language = el('select', { class: 'input', id: FIELDS.language });
  for (const entry of LOCALES) {
    language.append(el('option', { value: entry.tag, text: entry.label }));
  }
  language.value = currentLocale();

  const perPage = el('select', { class: 'input', id: FIELDS.perPage });
  for (const choice of MESSAGES_PER_PAGE_CHOICES) {
    perPage.append(
      el('option', {
        value: String(choice),
        text: tn(choice, '{count} message', '{count} messages', { count: choice }),
      }),
    );
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
  const changePassword = el('button', { type: 'submit', class: 'btn', text: t('Change password') });

  const passwordForm = el('form', { class: 'settings-grid', id: 'settings-password' }, [
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.passwordCurrent, text: t('Current password') }),
      currentPassword,
    ]),
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.passwordNew, text: t('New password') }),
      newPassword,
    ]),
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.passwordConfirm, text: t('Repeat the new password') }),
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
      showPasswordStatus(t('Fill in your current password and the new one.'));
      return;
    }
    if (replacement !== confirmPassword.value) {
      showPasswordStatus(t('The two new passwords do not match.'));
      return;
    }
    if (replacement === secret) {
      showPasswordStatus(t('The new password must differ from the current one.'));
      return;
    }

    changePassword.disabled = true;
    setText(changePassword, t('Changing…'));
    try {
      await request(`${API_BASE}/auth/password`, {
        method: 'POST',
        body: { current_password: secret, new_password: replacement },
        toast: false,
        // A wrong current password is a `401`, the same status as an expired session.
        // Without this the shell would try to refresh the token and replay the request,
        // and a stale refresh token signed the user out instead of showing the error.
        retryOn401: false,
      });
      currentPassword.value = '';
      newPassword.value = '';
      confirmPassword.value = '';
      toastSuccess(t('Password changed.'));
      showPasswordStatus('');
    } catch (error) {
      const message = error instanceof ApiError ? error.message : t('The password could not be changed.');
      showPasswordStatus(message);
      toastError(message);
    } finally {
      changePassword.disabled = false;
      setText(changePassword, t('Change password'));
    }
  });

  const security = securitySection();

  const body = el('div', { class: 'settings-grid' }, [
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.displayName, text: t('Display name') }),
      displayName,
      el('p', { class: 'modal-message', text: t('Used for the From line of new messages.') }),
    ]),
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.signature, text: t('Signature') }),
      signature,
      el('p', { class: 'modal-message', text: t('Appended to every new message you compose.') }),
    ]),
    el('div', { class: 'field' }, [
      el('span', { class: 'field-label', text: t('Theme') }),
      theme.node,
    ]),
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.language, text: t('Language') }),
      language,
    ]),
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.perPage, text: t('Messages per page') }),
      perPage,
    ]),
    el('label', { class: 'checkbox', for: FIELDS.markReadOnOpen }, [
      markRead,
      el('span', { text: t('Mark messages as read when I open them') }),
    ]),
    el('h3', { class: 'card-title', text: t('Password') }),
    passwordForm,
    security.node,
  ]);

  const close = el('button', { type: 'button', class: 'btn', text: t('Close') });
  const reset = el('button', { type: 'button', class: 'btn', text: t('Restore defaults') });
  const save = el('button', { type: 'button', class: 'btn btn-primary', text: t('Save settings') });

  const modal = openModal({
    title: t('Settings'),
    body,
    footer: [reset, el('span', { class: 'spacer' }), close, save],
    onMount: () => {
      security.reload();
      close.addEventListener('click', () => modal.close('close'));
      save.addEventListener('click', () => {
        const perPageValue = Number.parseInt(perPage.value, 10);
        persist({
          displayName: displayName.value.trim(),
          signature: signature.value,
          theme: theme.selected(),
          perPage: MESSAGES_PER_PAGE_CHOICES.includes(perPageValue) ? perPageValue : PREF_DEFAULTS.perPage,
          markReadOnOpen: markRead.checked,
        });
        // A language change redraws every string in the app, so it is the one setting
        // that cannot be shown in place: apply it and reload, exactly as Save implies.
        if (language.value !== currentLocale()) {
          setLocale(language.value);
          window.location.reload();
          return;
        }
        toastSuccess(t('Settings saved.'));
        modal.close('save');
      });
      reset.addEventListener('click', () => {
        persist(Object.assign({}, PREF_DEFAULTS));
        displayName.value = '';
        signature.value = '';
        theme.select(PREF_DEFAULTS.theme);
        perPage.value = String(PREF_DEFAULTS.perPage);
        markRead.checked = PREF_DEFAULTS.markReadOnOpen;
        // The picker goes back to the language actually in force; restoring defaults
        // is about the preferences, not about swapping the interface language.
        language.value = currentLocale();
        toastSuccess(t('Settings restored to their defaults.'));
      });
    },
  });
}
