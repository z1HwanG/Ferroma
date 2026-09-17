/**
 * Aliases: one domain at a time, from `GET /api/v1/domains/:id/aliases`, with
 * create / edit / delete through the documented per-domain and per-alias routes.
 */

import { API_BASE, ApiError, request } from '../api.js';
import { aliasesOf, domainsOf } from '../data.js';
import { el, setHidden, setText } from '../dom.js';
import { openModal, promptDialog } from '../modal.js';
import { go } from '../router.js';
import { toastError, toastSuccess } from '../toast.js';
import { actions, adminCard, badge, cell, field, table, viewHead } from '../ui.js';

/**
 * @param {URLSearchParams} params
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render(params) {
  const selected = Number.parseInt(params.get('domain') || '0', 10) || 0;

  const picker = el('select', { class: 'input', id: 'alias-domain' });
  const aliasTitle = el('h2', { class: 'card-title', text: 'Aliases' });
  const card = adminCard({
    title: 'Aliases',
    titleNode: aliasTitle,
    subtitle: 'GET /api/v1/domains/:id/aliases',
    renderEmpty: () =>
      el('div', { class: 'empty-state' }, [
        el('p', { class: 'empty-title', text: 'No aliases for this domain' }),
        el('p', { text: 'An alias forwards one local part to another address.' }),
      ]),
    renderData: (data) => data.node,
  });

  const createButton = el('button', { type: 'button', class: 'btn btn-primary', text: 'New alias' });
  createButton.addEventListener('click', () => {
    const domainId = Number.parseInt(picker.value, 10);
    if (!domainId) {
      toastError('Choose a domain first.');
      return;
    }
    openAliasDialog({ domainId }, () => refresh());
  });

  const root = el('div', {}, [
    viewHead('Aliases', 'Forwarding addresses', [createButton]),
    el('section', { class: 'card' }, [
      field('Domain', picker, 'Aliases belong to one domain at a time.'),
    ]),
    card.node,
  ]);

  let domains = [];

  picker.addEventListener('change', () => {
    go('aliases', { domain: picker.value }, { replace: true });
    refresh();
  });

  function currentDomainId() {
    const fromPicker = Number.parseInt(picker.value, 10);
    if (Number.isFinite(fromPicker) && fromPicker > 0) return fromPicker;
    if (selected > 0) return selected;
    return domains.length ? domains[0].id : 0;
  }

  async function loadDomains() {
    try {
      const payload = await request(`${API_BASE}/domains`, { toast: false });
      domains = domainsOf(payload);
      picker.replaceChildren();
      if (domains.length === 0) {
        picker.append(el('option', { value: '', text: 'no domains yet' }));
        picker.disabled = true;
        return false;
      }
      picker.disabled = domains.length < 2;
      for (const domain of domains) {
        picker.append(el('option', { value: String(domain.id), text: domain.name }));
      }
      const wanted = domains.find((domain) => domain.id === currentDomainId()) || domains[0];
      picker.value = String(wanted.id);
      setText(aliasTitle, `Aliases — ${wanted.name}`);
      return true;
    } catch (error) {
      picker.replaceChildren(el('option', { value: '', text: 'domains unavailable' }));
      picker.disabled = true;
      card.setState({ state: 'error', message: messageOf(error, 'Domains could not be loaded.') });
      return false;
    }
  }

  async function refresh() {
    const domainId = currentDomainId();
    if (!domainId) {
      if (domains.length === 0 && picker.disabled) card.setState({ state: 'empty' });
      return;
    }
    card.setState({ state: 'loading' });
    try {
      const payload = await request(`${API_BASE}/domains/${domainId}/aliases`, { toast: false });
      const aliases = aliasesOf(payload);
      if (aliases.length === 0) {
        card.setState({ state: 'empty' });
        return;
      }
      card.setState({ state: 'ready', data: { node: renderTable(aliases, domainId, handlers) } });
    } catch (error) {
      card.setState({ state: 'error', message: messageOf(error, 'Aliases could not be loaded.') });
    }
  }

  const handlers = {
    onEdit: async (alias) => {
      const target = await promptDialog({
        title: `Forward ${alias.localPart}`,
        label: 'Destination address',
        confirmLabel: 'Save',
      });
      if (target === null) return;
      try {
        await request(`${API_BASE}/aliases/${alias.id}`, { method: 'PATCH', body: { target }, toast: false });
        toastSuccess('Alias updated.');
        refresh();
      } catch (error) {
        toastError(messageOf(error, 'The alias could not be updated.'));
      }
    },
    onToggle: async (alias) => {
      try {
        await request(`${API_BASE}/aliases/${alias.id}`, {
          method: 'PATCH',
          body: { enabled: !alias.enabled },
          toast: false,
        });
        toastSuccess('Alias updated.');
        refresh();
      } catch (error) {
        toastError(messageOf(error, 'The alias could not be updated.'));
      }
    },
    onDelete: async (alias) => {
      const target = await promptDialog({
        title: 'Delete alias',
        label: `Type ${alias.localPart} to confirm`,
        confirmLabel: 'Delete alias',
        requireValue: alias.localPart,
      });
      if (target === null) return;
      try {
        await request(`${API_BASE}/aliases/${alias.id}`, { method: 'DELETE', toast: false });
        toastSuccess('Alias deleted.');
        refresh();
      } catch (error) {
        toastError(messageOf(error, 'The alias could not be deleted.'));
      }
    },
  };

  const ok = await loadDomains();
  if (ok) await refresh();
  else card.setState({ state: 'error', message: 'Domains could not be loaded.' });

  return { node: root, cleanup() {} };
}

function renderTable(aliases, domainId, handlers) {
  const rows = aliases.map((alias) => [
    cell(alias.localPart, 'cell-mono'),
    cell(alias.target || '—', 'cell-mono'),
    badge(alias.enabled ? 'enabled' : 'disabled'),
    actions(
      button('Retarget', () => handlers.onEdit(alias)),
      button(alias.enabled ? 'Disable' : 'Enable', () => handlers.onToggle(alias)),
      button('Delete', () => handlers.onDelete(alias), 'btn-danger'),
    ),
  ]);
  return el('div', {}, [
    el('p', { class: 'view-sub', text: `Domain id ${domainId}` }),
    table({
      columns: [{ label: 'Local part' }, { label: 'Forwards to' }, { label: 'State' }, { label: 'Actions' }],
      rows,
    }),
  ]);
}

function openAliasDialog(options, onDone) {
  const localPart = el('input', { class: 'input', id: 'alias-local', type: 'text', autocomplete: 'off' });
  const target = el('input', { class: 'input', id: 'alias-target', type: 'email', autocomplete: 'off' });
  const error = el('p', { class: 'field-error', id: 'alias-error', hidden: true });
  const body = el('div', {}, [
    field('Local part', localPart, 'The part before the @ for the alias.'),
    field('Forward to', target),
    error,
  ]);
  const cancel = el('button', { type: 'button', class: 'btn', text: 'Cancel' });
  const create = el('button', { type: 'button', class: 'btn btn-primary', text: 'Create alias' });

  const modal = openModal({
    title: 'New alias',
    body,
    footer: [cancel, create],
    onMount: () => {
      cancel.addEventListener('click', () => modal.close('cancel'));
      create.addEventListener('click', async () => {
        const from = localPart.value.trim().toLowerCase();
        const to = target.value.trim();
        if (!/^[a-z0-9!#$%&'*+/=?^_`{|}~.-]+$/.test(from)) {
          setText(error, 'Enter a valid local part.');
          setHidden(error, false);
          return;
        }
        if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(to)) {
          setText(error, 'Enter a valid destination address.');
          setHidden(error, false);
          return;
        }
        create.disabled = true;
        setText(create, 'Creating…');
        try {
          await request(`${API_BASE}/domains/${options.domainId}/aliases`, {
            method: 'POST',
            body: { local_part: from, target: to },
            toast: false,
          });
          toastSuccess(`Alias ${from} created.`);
          modal.close('created');
          onDone();
        } catch (err) {
          setText(error, messageOf(err, 'The alias could not be created.'));
          setHidden(error, false);
        } finally {
          create.disabled = false;
          setText(create, 'Create alias');
        }
      });
    },
  });
  localPart.focus();
}

function button(label, onClick, className = '') {
  const node = el('button', { type: 'button', class: `btn btn-small ${className}`.trim(), text: label });
  node.addEventListener('click', onClick);
  return node;
}

function messageOf(error, fallback) {
  if (error instanceof ApiError) return error.message;
  if (error instanceof Error && error.message) return error.message;
  return fallback;
}
