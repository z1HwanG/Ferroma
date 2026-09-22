/**
 * Contacts: addresses remembered from mail, and the ones the owner adds.
 *
 * A blocked address is delivered to Junk. Deleting forgets it until the next message
 * names it again.
 */

import { API_BASE, ApiError, request } from '../shared/api.js';
import { el, setText } from '../shared/dom.js';
import { t } from '../shared/i18n.js';
import { toastError, toastSuccess } from '../shared/toast.js';

/**
 * Show the contact list in the message pane.
 *
 * @param {HTMLElement} host the pane the list replaces
 */
export async function showContacts(host) {
  const search = el('input', {
    class: 'input',
    id: 'contact-search',
    type: 'search',
    placeholder: t('Search contacts'),
  });
  const address = el('input', { class: 'input', id: 'contact-address', type: 'email', placeholder: 'name@example.com' });
  const name = el('input', { class: 'input', id: 'contact-name', type: 'text', placeholder: t('Name') });
  const add = el('button', { type: 'button', class: 'btn btn-small btn-primary', text: t('Add contact') });
  const list = el('div', { id: 'contact-list' });

  async function load() {
    const payload = await request(`${API_BASE}/contacts?q=${encodeURIComponent(search.value.trim())}`, { toast: false });
    const items = Array.isArray(payload && payload.items) ? payload.items : [];
    list.replaceChildren();
    if (items.length === 0) {
      list.append(el('p', { class: 'view-sub', text: t('No contacts yet.') }));
      return;
    }
    for (const contact of items) list.append(row(contact, load));
  }

  let timer = 0;
  search.addEventListener('input', () => {
    window.clearTimeout(timer);
    timer = window.setTimeout(() => load().catch((error) => toastError(messageOf(error))), 200);
  });
  add.addEventListener('click', async () => {
    const value = address.value.trim();
    if (value === '') return;
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
      el('div', { class: 'row' }, [search]),
      el('div', { class: 'row' }, [address, name, add]),
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
  const name = el('input', { class: 'input', type: 'text', value: String(contact.display_name || '') });
  const note = el('input', { class: 'input', type: 'text', value: String(contact.note || ''), placeholder: t('Note') });
  const favorite = toggle(t('Favorite'), Boolean(contact.favorite), (value) => patch(id, { favorite: value }, reload));
  const blocked = toggle(t('Block'), Boolean(contact.blocked), (value) => patch(id, { blocked: value }, reload));
  const save = el('button', { type: 'button', class: 'btn btn-small', text: t('Save') });
  const remove = el('button', { type: 'button', class: 'btn btn-small btn-danger', text: t('Delete') });
  save.addEventListener('click', () => patch(id, { display_name: name.value, note: note.value }, reload));
  remove.addEventListener('click', async () => {
    await request(`${API_BASE}/contacts/${id}`, { method: 'DELETE', toast: false });
    toastSuccess(t('Contact deleted.'));
    await reload();
  });
  return el('div', { class: 'contact-row' }, [
    el('div', {}, [
      el('strong', { class: 'cell-mono', text: String(contact.address || '') }),
      el('div', { class: 'row' }, [name, note]),
    ]),
    el('div', { class: 'row-actions' }, [favorite, blocked, save, remove]),
  ]);
}

/**
 * A checkbox that writes its new value immediately.
 *
 * @param {string} label
 * @param {boolean} checked
 * @param {(value: boolean) => Promise<void>} onChange
 */
function toggle(label, checked, onChange) {
  const box = el('input', { type: 'checkbox' });
  box.checked = checked;
  box.addEventListener('change', () => onChange(box.checked));
  return el('label', { class: 'checkbox' }, [box, el('span', { text: label })]);
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

void setText;
