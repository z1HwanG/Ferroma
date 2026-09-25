/**
 * Settings dialog: display name, signature, theme, messages per page, the
 * "mark read on open" switch, the interface language, and the account password.
 *
 * The preferences are per-browser and live in localStorage (see `store.js`); the
 * password is the one thing here that belongs to the *account*, so it is the one
 * thing that goes to the server (`POST /api/v1/auth/password`).
 *
 * The dialog is three pages — writing, appearance, security — because a single
 * column of every field made the common ones (a signature, a theme) sit under a
 * scroll the reader had to earn. Password, the second factor and application
 * passwords each stay collapsed until asked for: they are the rare visits, and
 * an open enrollment form is a wall of secret the rest of the page does not need.
 */

import { API_BASE, ApiError, request } from '../shared/api.js';
import { el, setHidden, setText, svgIcon } from '../shared/dom.js';
import { qrSvg } from '../shared/qr.js';
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
  totpQr: 'settings-totp-qr',
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
 * The QR image is drawn in the page, from the `otpauth://` URI, and never fetched:
 * this page is holding the shared secret, so handing it to another service to render
 * is out of the question, and the front ends have no build step to vendor a QR
 * library into. The grouped secret stays beside the picture, because a camera that
 * cannot read it is the case the manual entry exists for, and a picture that failed
 * to draw must never be the only copy of the secret.
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
  const qr = el('div', { class: 'totp-qr', id: FIELDS.totpQr, hidden: true });
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
  // The codes are shown once and only as text, so a download is the only copy a
  // person can keep without transcribing ten lines by hand. The button stays hidden
  // until confirmation has something to save.
  const downloadCodes = el('button', {
    type: 'button',
    class: 'btn',
    id: 'settings-totp-recovery-download',
    hidden: true,
    text: t('Download recovery codes'),
  });
  const pane = el('div', { class: 'settings-grid', id: FIELDS.totpPane, hidden: true }, [
    el('div', { class: 'field' }, [
      el('p', { class: 'modal-message', text: t('Scan this code with your authenticator app.') }),
      qr,
      el('p', { class: 'modal-message', text: t('Or enter this secret by hand.') }),
      secret,
      el('p', { class: 'modal-message', text: t('Or use this URI if your app accepts one:') }),
      uri,
    ]),
    el('div', { class: 'field field-stack' }, [
      el('label', { class: 'field-label', for: FIELDS.totpCode, text: t('Code from the app') }),
      code,
      confirm,
    ]),
    el('div', { class: 'field field-stack' }, [
      el('p', { class: 'modal-message', text: t('Save these recovery codes now. Each one works once, and they are not shown again.') }),
      recovery,
      downloadCodes,
    ]),
  ]);

  /* --------------------------------------------------------------- disable */

  const disablePassword = el('input', {
    class: 'input',
    id: FIELDS.totpPassword,
    type: 'password',
    autocomplete: 'current-password',
  });
  const disable = el('button', { type: 'button', class: 'btn btn-danger', id: FIELDS.totpOff, text: t('Turn off two-factor authentication') });
  const disablePane = el('div', { class: 'settings-grid', id: FIELDS.totpDisablePane, hidden: true }, [
    el('div', { class: 'field field-stack' }, [
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
   * One live application password, with its own revoke button.
   *
   * A revoked credential is not drawn. The server keeps the row so an administrator
   * can still see that it existed; this list is the user's, and a password they
   * just revoked is one they asked to be rid of.
   *
   * @param {{id: number, label: string, last_used_at: string|null}} entry
   */
  const appPasswordRow = (entry) => {
    const when = entry.last_used_at
      ? t('Last used {when}', { when: relativeStamp(entry.last_used_at) })
      : t('Never used');
    const revokeButton = el('button', { type: 'button', class: 'btn btn-small', text: t('Revoke') });
    const row = el('li', { class: 'secret-row' }, [
      el('span', { class: 'secret-name', text: entry.label }),
      el('span', { class: 'modal-message', text: when }),
      revokeButton,
    ]);
    revokeButton.addEventListener('click', async () => {
      revokeButton.disabled = true;
      await revoke(entry.id);
    });
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
      const items = (Array.isArray(payload.items) ? payload.items : []).filter((entry) => !entry.revoked_at);
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
      showQr(qr, payload.uri);
      recovery.replaceChildren();
      setHidden(pane, false);
      setHidden(recovery, true);
      setHidden(downloadCodes, true);
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
      setHidden(recovery, codes.length === 0);
      setHidden(downloadCodes, codes.length === 0);
      downloadCodes.onclick = () => saveRecoveryCodes(codes);
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

  // Both blocks start closed. Enrollment, the disable form and the create form
  // are the long part of this page; a status line is enough until someone asks.
  const factorBody = el('div', { class: 'settings-fold-body' }, [
    status,
    enroll,
    pane,
    disablePane,
    error,
  ]);
  const factor = el('details', { class: 'settings-fold' }, [
    el('summary', { class: 'settings-fold-summary', text: t('Two-factor authentication') }),
    factorBody,
  ]);

  const passwordsBody = el('div', { class: 'settings-fold-body' }, [
    el('p', {
      class: 'modal-message',
      text: t('For mail clients that cannot ask for a code. Each one is a full credential for this account.'),
    }),
    el('div', { class: 'settings-inline' }, [label, create]),
    fresh,
    list,
  ]);
  const passwords = el('details', { class: 'settings-fold' }, [
    el('summary', { class: 'settings-fold-summary', text: t('Application passwords') }),
    passwordsBody,
  ]);

  const node = el('div', { class: 'settings-stack' }, [factor, passwords]);

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

/**
 * Offer `codes` as a text file the browser downloads.
 *
 * The server returns a recovery code once and stores only its digest, so the file
 * is built here, in the page, from the codes already on screen. Nothing is sent
 * anywhere to produce it. An empty list downloads nothing: there is no file to save.
 *
 * @param {string[]} codes
 */
export function saveRecoveryCodes(codes) {
  const lines = (Array.isArray(codes) ? codes : []).map((code) => String(code).trim()).filter(Boolean);
  if (lines.length === 0 || typeof document === 'undefined') return;
  const blob = new Blob([`${lines.join('\n')}\n`], { type: 'text/plain;charset=utf-8' });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement('a');
  anchor.href = url;
  anchor.download = 'ferroma-recovery-codes.txt';
  anchor.rel = 'noopener';
  document.body.append(anchor);
  anchor.click();
  anchor.remove();
  window.setTimeout(() => URL.revokeObjectURL(url), 30000);
}

/**
 * Draw `uri` into `node` as a QR code, or hide the node when it cannot be drawn.
 *
 * The markup comes from [`qrSvg`], which builds one path from the encoded modules.
 * It is parsed into a detached document and the node moved across, rather than
 * assigned as markup: the reading pane is the one place this app renders markup,
 * and it does so in a sandboxed frame. The secret stays on screen as text either
 * way, so a picture that failed to draw is an inconvenience rather than the loss
 * of the only copy.
 *
 * @param {HTMLElement} node
 * @param {string} uri
 */
export function showQr(node, uri) {
  const svg = qrSvg(String(uri || ''));
  if (svg === null) {
    node.replaceChildren();
    setHidden(node, true);
    return;
  }
  const picture = new DOMParser().parseFromString(svg, 'image/svg+xml').documentElement;
  picture.setAttribute('aria-label', t('QR code for your authenticator app'));
  node.replaceChildren(node.ownerDocument.importNode(picture, true));
  setHidden(node, false);
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

  const signature = el('textarea', { class: 'input', id: FIELDS.signature, rows: '3', spellcheck: 'true' });
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

  const passwordForm = el('form', { class: 'settings-stack', id: 'settings-password' }, [
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
    el('div', { class: 'settings-actions' }, [changePassword]),
    passwordStatus,
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

  // Three pages, not one column. Writing is what someone opens Settings for;
  // appearance is a glance; security is the rare visit and starts collapsed.
  const writing = el('div', { class: 'settings-panel', role: 'tabpanel', id: 'settings-panel-writing' }, [
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.displayName, text: t('Display name') }),
      displayName,
      el('p', { class: 'field-hint', text: t('Used for the From line of new messages.') }),
    ]),
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: FIELDS.signature, text: t('Signature') }),
      signature,
      el('p', { class: 'field-hint', text: t('Appended to every new message you compose.') }),
    ]),
  ]);

  const appearance = el('div', {
    class: 'settings-panel',
    role: 'tabpanel',
    id: 'settings-panel-appearance',
    hidden: true,
  }, [
    el('div', { class: 'field' }, [
      el('span', { class: 'field-label', text: t('Theme') }),
      theme.node,
    ]),
    el('div', { class: 'settings-pair' }, [
      el('div', { class: 'field' }, [
        el('label', { class: 'field-label', for: FIELDS.language, text: t('Language') }),
        language,
      ]),
      el('div', { class: 'field' }, [
        el('label', { class: 'field-label', for: FIELDS.perPage, text: t('Messages per page') }),
        perPage,
      ]),
    ]),
    el('label', { class: 'checkbox', for: FIELDS.markReadOnOpen }, [
      markRead,
      el('span', { text: t('Mark messages as read when I open them') }),
    ]),
  ]);

  const passwordFold = el('details', { class: 'settings-fold' }, [
    el('summary', { class: 'settings-fold-summary', text: t('Password') }),
    el('div', { class: 'settings-fold-body' }, [passwordForm]),
  ]);

  const account = el('div', {
    class: 'settings-panel',
    role: 'tabpanel',
    id: 'settings-panel-security',
    hidden: true,
  }, [
    passwordFold,
    security.node,
  ]);

  const panels = [
    { id: 'writing', label: t('Writing'), node: writing },
    { id: 'appearance', label: t('Appearance'), node: appearance },
    { id: 'security', label: t('Security'), node: account },
  ];

  const tabs = el('div', { class: 'settings-tabs', role: 'tablist', 'aria-label': t('Settings') });
  const tabButtons = [];
  const showPanel = (id) => {
    for (const panel of panels) setHidden(panel.node, panel.id !== id);
    for (const button of tabButtons) {
      const on = button.dataset.panel === id;
      button.setAttribute('aria-selected', on ? 'true' : 'false');
      button.tabIndex = on ? 0 : -1;
    }
  };
  for (const panel of panels) {
    const button = el('button', {
      type: 'button',
      class: 'settings-tab',
      role: 'tab',
      id: `settings-tab-${panel.id}`,
      'aria-controls': panel.node.id,
      'aria-selected': panel.id === 'writing' ? 'true' : 'false',
      tabindex: panel.id === 'writing' ? '0' : '-1',
      dataset: { panel: panel.id },
      text: panel.label,
    });
    button.addEventListener('click', () => showPanel(panel.id));
    tabButtons.push(button);
    tabs.append(button);
    panel.node.setAttribute('aria-labelledby', button.id);
  }
  // Arrow keys move between pages the way a tablist does; the buttons are not a
  // toolbar, so Left and Right are the reading direction rather than a shortcut.
  tabs.addEventListener('keydown', (event) => {
    const current = tabButtons.findIndex((button) => button.getAttribute('aria-selected') === 'true');
    if (current < 0) return;
    let next = current;
    if (event.key === 'ArrowRight') next = (current + 1) % tabButtons.length;
    else if (event.key === 'ArrowLeft') next = (current - 1 + tabButtons.length) % tabButtons.length;
    else if (event.key === 'Home') next = 0;
    else if (event.key === 'End') next = tabButtons.length - 1;
    else return;
    event.preventDefault();
    showPanel(panels[next].id);
    tabButtons[next].focus();
  });

  const body = el('div', { class: 'settings-layout' }, [
    tabs,
    writing,
    appearance,
    account,
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
