/**
 * Settings dialog: display name, signature, theme, messages per page, and the
 * "mark read on open" switch. Everything is stored in localStorage (see
 * `store.js`) except the theme, which `theme.js` owns.
 */

import { el } from './dom.js';
import { openModal } from './modal.js';
import { MESSAGES_PER_PAGE_CHOICES, PREF_DEFAULTS, getPrefs, savePrefs } from './store.js';
import { THEME_MODES, currentTheme, setTheme } from './theme.js';
import { toastSuccess } from './toast.js';

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
