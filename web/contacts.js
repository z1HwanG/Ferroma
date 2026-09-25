/**
 * Contacts: addresses remembered from mail, and the ones the owner adds.
 *
 * A blocked address is delivered to Junk. Deleting forgets it until the next message
 * names it again.
 */

import { API_BASE, ApiError, request } from '../shared/api.js';
import { el, svgIcon } from '../shared/dom.js';
import { t } from '../shared/i18n.js';
import { toastError, toastSuccess } from '../shared/toast.js';

/**
 * Show the contact list in the message pane.
 *
 * @param {HTMLElement} host the pane the list replaces
 */
export async function showContacts(host) {
  // The list pane is a listbox of messages. Contacts are a form, so the role has
  // to come off while they are showing; the next folder render puts it back.
  if (host.getAttribute('role') === 'listbox') {
    host.dataset.contactsRole = 'listbox';
    host.removeAttribute('role');
    host.removeAttribute('aria-multiselectable');
  }
  host.hidden = false;
  const search = el('input', {
    class: 'input contact-search',
    id: 'contact-search',
    type: 'search',
    placeholder: t('Search contacts'),
    'aria-label': t('Search contacts'),
  });
  const address = el('input', {
    class: 'input',
    id: 'contact-address',
    type: 'email',
    placeholder: 'name@example.com',
    'aria-label': t('Address'),
    autocomplete: 'off',
  });
  const name = el('input', {
    class: 'input',
    id: 'contact-name',
    type: 'text',
    placeholder: t('Name'),
    'aria-label': t('Name'),
    autocomplete: 'name',
  });
  const add = el('button', { type: 'submit', class: 'btn btn-small btn-primary contact-add' }, [
    svgIcon('plus'),
    el('span', { text: t('Add contact') }),
  ]);
  const list = el('div', { class: 'contact-list', id: 'contact-list' });

  async function load() {
    const payload = await request(`${API_BASE}/contacts?q=${encodeURIComponent(search.value.trim())}`, { toast: false });
    const items = Array.isArray(payload && payload.items) ? payload.items : [];
    list.replaceChildren();
    if (items.length === 0) {
      list.append(el('p', { class: 'empty', text: t('No contacts yet.') }));
      return;
    }
    for (const contact of items) list.append(row(contact, load));
  }

  let timer = 0;
  search.addEventListener('input', () => {
    window.clearTimeout(timer);
    timer = window.setTimeout(() => load().catch((error) => toastError(messageOf(error))), 200);
  });
  const form = el('form', { class: 'contact-add-form' }, [address, name, add]);
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    const value = address.value.trim();
    if (value === '') {
      address.focus();
      return;
    }
    try {
      await request(`${API_BASE}/contacts`, {
        method: 'POST',
        body: { address: value, display_name: name.value.trim() },
        toast: false,
      });
      address.value = '';
      name.value = '';
      await load();
    } catch (error) {
      toastError(messageOf(error));
    }
  });

  host.replaceChildren(
    el('div', { class: 'contacts' }, [
      el('div', { class: 'contact-toolbar' }, [
        el('label', { class: 'contact-search-wrap' }, [
          svgIcon('search'),
          search,
        ]),
        form,
      ]),
      list,
    ]),
  );
  await load();
}

/**
 * One contact, with its editable fields.
 *
 * @param {Record<string, unknown>} contact
 * @param {() => Promise<void>} reload
 */
function row(contact, reload) {
  const id = Number(contact.id);
  const favoriteOn = Boolean(contact.favorite);
  const blockedOn = Boolean(contact.blocked);
  const address = String(contact.address || '');
  const display = String(contact.display_name || '');
  const name = el('input', {
    class: 'input',
    type: 'text',
    value: display,
    placeholder: t('Name'),
    'aria-label': t('Name'),
  });
  const note = el('input', {
    class: 'input',
    type: 'text',
    value: String(contact.note || ''),
    placeholder: t('Note'),
    'aria-label': t('Note'),
  });
  const favorite = chip(t('Favorite'), favoriteOn, 'star', (value) => patch(id, { favorite: value }, reload));
  const blocked = chip(t('Block'), blockedOn, 'junk', (value) => patch(id, { blocked: value }, reload));
  const save = el('button', { type: 'button', class: 'btn btn-small contact-save' }, [
    svgIcon('check'),
    el('span', { text: t('Save') }),
  ]);
  const remove = el('button', {
    type: 'button',
    class: 'btn btn-small btn-icon btn-danger-quiet contact-delete',
    'aria-label': t('Delete'),
    title: t('Delete'),
  }, [svgIcon('trash')]);
  save.addEventListener('click', () => patch(id, { display_name: name.value, note: note.value }, reload));
  remove.addEventListener('click', async () => {
    await request(`${API_BASE}/contacts/${id}`, { method: 'DELETE', toast: false });
    toastSuccess(t('Contact deleted.'));
    await reload();
  });
  const card = el('article', { class: 'contact-card' }, [
    el('div', { class: 'contact-identity' }, [
      el('span', { class: 'contact-avatar', 'aria-hidden': 'true', text: initialOf(display || address) }),
      el('div', { class: 'contact-who' }, [
        el('strong', { class: 'contact-address', text: address }),
        display === '' ? null : el('span', { class: 'contact-display', text: display }),
      ]),
    ]),
    el('div', { class: 'contact-fields' }, [name, note]),
    el('div', { class: 'contact-actions' }, [
      el('div', { class: 'contact-flags' }, [favorite, blocked]),
      el('div', { class: 'contact-commit' }, [save, remove]),
    ]),
  ]);
  if (blockedOn) card.classList.add('is-blocked');
  if (favoriteOn) card.classList.add('is-favorite');
  return card;
}

/**
 * A toggle that writes its new value immediately.
 *
 * @param {string} label
 * @param {boolean} pressed
 * @param {string} icon `star` or `junk`, both drawn by `svgIcon`
 * @param {(value: boolean) => Promise<void>} onChange
 */
function chip(label, pressed, icon, onChange) {
  const button = el('button', {
    type: 'button',
    class: `btn btn-small contact-chip${icon === 'star' ? ' contact-chip-star' : ' contact-chip-block'}`,
    'aria-pressed': pressed ? 'true' : 'false',
  }, [
    svgIcon(icon),
    el('span', { text: label }),
  ]);
  button.addEventListener('click', () => onChange(!pressed));
  return button;
}

/**
 * The letter drawn in a contact's avatar. An address with no display name still
 * gets one, taken from the local part rather than the `@`.
 *
 * @param {string} value
 */
function initialOf(value) {
  const source = value.includes('@') && !value.includes(' ') ? value.split('@')[0] : value;
  const trimmed = source.trim();
  if (trimmed === '') return '?';
  return Array.from(trimmed)[0].toUpperCase();
}

/**
 * @param {number} id
 * @param {Record<string, unknown>} body
 * @param {() => Promise<void>} reload
 */
async function patch(id, body, reload) {
  try {
    await request(`${API_BASE}/contacts/${id}`, { method: 'PATCH', body, toast: false });
    await reload();
  } catch (error) {
    toastError(messageOf(error));
  }
}

/** @param {unknown} error */
function messageOf(error) {
  return error instanceof ApiError ? error.message : t('The contacts could not be loaded.');
}
