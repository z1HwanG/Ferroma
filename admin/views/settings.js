/**
 * Settings: the DB-backed key/value store behind `GET /api/v1/settings` and
 * `PUT /api/v1/settings/:key`, plus the interface-language picker.
 *
 * The API returns "DB-backed settings" without freezing the envelope, so this
 * view accepts a flat object, a `{settings: …}` wrapper or a list of
 * `{key, value}` rows. Values are edited as JSON when they are structured and as
 * plain text when they are not.
 */

import { API_BASE, ApiError, request } from '../../shared/api.js';
import { listOf } from '../../shared/data.js';
import { el, setHidden, setText } from '../../shared/dom.js';
import { LOCALES, currentLocale, setLocale, t } from '../../shared/i18n.js';
import { openModal } from '../../shared/modal.js';
import { toastError, toastSuccess } from '../../shared/toast.js';
import { adminCard, cell, copyToClipboard, table, viewHead } from '../ui.js';

/**
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render() {
  const card = adminCard({
    title: t('Stored settings'),
    subtitle: 'GET /api/v1/settings',
    renderEmpty: () =>
      el('div', { class: 'empty-state' }, [
        el('p', { class: 'empty-title', text: t('No database settings') }),
        el('p', { text: t('This server keeps its configuration in ferroma.toml only.') }),
      ]),
    renderData: (data) => data.node,
  });

  const addButton = el('button', { type: 'button', class: 'btn btn-primary', text: t('Add setting') });
  addButton.addEventListener('click', () => openSettingDialog(null, () => refresh()));

  const refreshButton = el('button', { type: 'button', class: 'btn', text: t('Refresh') });
  refreshButton.addEventListener('click', () => refresh());

  const root = el('div', {}, [
    viewHead(t('Settings'), t('Runtime configuration persisted in the database'), [addButton, refreshButton]),
    languageCard(),
    el('section', { class: 'card' }, [
      el('p', {
        class: 'view-sub',
        text: t('File-level configuration lives in ferroma.toml; only keys returned by the API appear here.'),
      }),
    ]),
    card.node,
  ]);

  /** @param {unknown} payload */
  function rowsOf(payload) {
    if (Array.isArray(payload)) return listOf(payload);
    if (payload && typeof payload === 'object' && payload.items) return listOf(payload);
    if (payload && typeof payload === 'object' && payload.settings && typeof payload.settings === 'object') {
      return Object.entries(payload.settings).map(([key, value]) => ({ key, value }));
    }
    if (payload && typeof payload === 'object') {
      return Object.entries(payload).map(([key, value]) => ({ key, value }));
    }
    return [];
  }

  async function refresh() {
    card.setState({ state: 'loading' });
    try {
      const payload = await request(`${API_BASE}/settings`, { toast: false });
      const entries = rowsOf(payload);
      if (entries.length === 0) {
        card.setState({ state: 'empty' });
        return;
      }
      card.setState({ state: 'ready', data: { node: renderTable(entries, handlers) } });
    } catch (error) {
      card.setState({
        state: 'error',
        message: error instanceof ApiError ? error.message : t('Settings could not be loaded.'),
      });
    }
  }

  const handlers = {
    onEdit: (entry) => openSettingDialog(entry, () => refresh()),
  };

  await refresh();
  return { node: root, cleanup() {} };
}

/**
 * The interface-language picker.
 *
 * Each option is labelled in its own language, so a reader who cannot read the current
 * one can still find theirs. The choice is *staged* until Apply is pressed: a `change`
 * handler that reloaded immediately swapped the console's language the instant the
 * arrow keys moved over the list, with no confirmation and no way back but the picker
 * in a language the reader may not have meant to enter.
 */
function languageCard() {
  const select = el('select', { class: 'input', id: 'settings-language' });
  for (const entry of LOCALES) {
    select.append(el('option', { value: entry.tag, text: entry.label }));
  }
  select.value = currentLocale();

  const apply = el('button', { type: 'button', class: 'btn btn-primary', text: t('Apply') });
  apply.addEventListener('click', () => {
    if (select.value === currentLocale()) {
      toastSuccess(t('The language is already {language}.', {
        language: select.options[select.selectedIndex] ? select.options[select.selectedIndex].text : select.value,
      }));
      return;
    }
    setLocale(select.value);
    // Every view builds its DOM once and keeps a reference to it, so re-rendering from
    // here would leave the text already on screen in the old language. Reloading is
    // the one way to guarantee that every string is redrawn.
    window.location.reload();
  });

  return el('section', { class: 'card' }, [
    el('h2', { class: 'card-title', text: t('Language') }),
    el('p', {
      class: 'view-sub',
      text: t('The language of this console. The choice is remembered in this browser.'),
    }),
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: 'settings-language', text: t('Language') }),
      select,
    ]),
    el('div', { class: 'card-actions' }, [apply]),
  ]);
}

function renderTable(entries, handlers) {
  const rows = entries.map((entry) => {
    const display = typeof entry.value === 'string' ? entry.value : JSON.stringify(entry.value);
    const key = String(entry.key);
    const copy = button(t('Copy'), async () => {
      const copied = await copyToClipboard(display, () => {});
      if (copied) toastSuccess(t('Copied {key}.', { key }));
      else toastError(t('The clipboard is not available in this context.'));
    });
    return [
      cell(key, 'cell-mono'),
      el('span', { class: 'truncate', title: display, text: display === undefined ? '—' : display }),
      el('div', { class: 'cell-actions' }, [button(t('Edit'), () => handlers.onEdit(entry)), copy]),
    ];
  });

  return table({
    columns: [{ label: t('Key') }, { label: t('Value') }, { label: t('Actions') }],
    rows,
  });
}

/** @param {{key?: string, value?: unknown}|null} entry */
function openSettingDialog(entry, onDone) {
  const editing = Boolean(entry && entry.key);
  const key = el('input', {
    class: 'input',
    id: 'setting-key',
    type: 'text',
    autocomplete: 'off',
    placeholder: 'limits.max_message_size',
  });
  key.value = editing ? String(entry.key) : '';
  key.disabled = editing;

  const rawValue =
    editing && entry.value !== undefined && entry.value !== null
      ? typeof entry.value === 'string'
        ? entry.value
        : JSON.stringify(entry.value)
      : '';
  const value = el('textarea', { class: 'input', id: 'setting-value', rows: '6', spellcheck: 'false' });
  value.value = rawValue;

  const error = el('p', { class: 'field-error', id: 'setting-error', hidden: true });
  const body = el('div', {}, [
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: 'setting-key', text: t('Key') }),
      key,
    ]),
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: 'setting-value', text: t('Value (JSON or plain text)') }),
      value,
      el('p', { class: 'view-sub', text: t('“true”, “12” and “{…}” are stored as JSON; anything else as text.') }),
    ]),
    error,
  ]);

  const cancel = el('button', { type: 'button', class: 'btn', text: t('Cancel') });
  const save = el('button', { type: 'button', class: 'btn btn-primary', text: editing ? t('Save') : t('Add') });

  const modal = openModal({
    title: editing ? t('Edit {key}', { key: entry.key }) : t('Add setting'),
    body,
    footer: [cancel, save],
    onMount: () => {
      cancel.addEventListener('click', () => modal.close('cancel'));
      save.addEventListener('click', async () => {
        const settingKey = key.value.trim();
        if (settingKey === '') {
          setText(error, t('A key is required.'));
          setHidden(error, false);
          key.focus();
          return;
        }
        const parsed = parseValue(value.value);
        save.disabled = true;
        setText(save, t('Saving…'));
        try {
          await request(`${API_BASE}/settings/${encodeURIComponent(settingKey)}`, {
            method: 'PUT',
            body: { value: parsed },
            toast: false,
          });
          toastSuccess(t('Setting {key} saved.', { key: settingKey }));
          modal.close('saved');
          onDone();
        } catch (err) {
          setText(error, err instanceof ApiError ? err.message : t('The setting could not be saved.'));
          setHidden(error, false);
        } finally {
          save.disabled = false;
          setText(save, editing ? t('Save') : t('Add'));
        }
      });
    },
  });
  (editing ? value : key).focus();
}

/** Interpret the textarea: JSON when it parses, otherwise the raw string. */
function parseValue(text) {
  const trimmed = String(text || '').trim();
  if (trimmed === '') return '';
  if (/^(true|false|null|-?\d+(\.\d+)?([eE][-+]?\d+)?)$/.test(trimmed) || /^[[{]/.test(trimmed)) {
    try {
      return JSON.parse(trimmed);
    } catch {
      return trimmed;
    }
  }
  return trimmed;
}

function button(label, onClick, className = '') {
  const node = el('button', { type: 'button', class: `btn btn-small ${className}`.trim(), text: label });
  node.addEventListener('click', onClick);
  return node;
}
