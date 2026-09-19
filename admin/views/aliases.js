/**
 * Aliases: one domain at a time, from `GET /api/v1/domains/:id/aliases`, with
 * create / retarget / enable / disable / delete through the documented per-domain and
 * per-alias routes, row selection for bulk work, and a per-alias detail drawer.
 *
 * The list is not a `Page<T>`: it answers `{items, total}` with no `limit`/`offset`,
 * so the whole domain is on screen at once and there is no pager. Switching domains
 * rebuilds the table, which is also what makes the bulk actions unambiguous — they
 * can only ever act on aliases of the domain the picker is showing.
 */

import { API_BASE, ApiError, request } from '../api.js';
import { aliasesOf, domainsOf } from '../data.js';
import { el, setHidden, setText } from '../dom.js';
import { formatLogStamp } from '../format.js';
import { confirmDialog, openModal, promptDialog } from '../modal.js';
import { go } from '../router.js';
import { toastError, toastSuccess } from '../toast.js';
import {
  actions,
  adminCard,
  badge,
  cell,
  dataTable,
  definitionList,
  field,
  filterBar,
  openDrawer,
  viewHead,
} from '../ui.js';

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
    subtitle: 'Sort by a column heading; select rows to act on several at once.',
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

  const bar = filterBar({
    id: 'aliases-domain-bar',
    fields: [
      field('Domain', picker, 'Aliases belong to one domain at a time.'),
      el('button', { type: 'submit', class: 'btn', text: 'Show' }),
    ],
  });
  // The picker already applies on `change`; this keeps Enter from submitting the
  // <form> `filterBar` builds, which would reload the console out from under the hash
  // router and land the operator on the dashboard.
  bar.addEventListener('submit', (event) => {
    event.preventDefault();
    applyDomain();
  });

  const root = el('div', {}, [
    viewHead('Aliases', 'Forwarding addresses', [createButton]),
    bar,
    card.node,
  ]);

  let domains = [];
  // The domain the table currently shows, so a drawer opened from a row can name it
  // without a second lookup.
  let currentDomain = null;

  picker.addEventListener('change', applyDomain);

  function currentDomainId() {
    const fromPicker = Number.parseInt(picker.value, 10);
    if (Number.isFinite(fromPicker) && fromPicker > 0) return fromPicker;
    if (selected > 0) return selected;
    return domains.length ? domains[0].id : 0;
  }

  function applyDomain() {
    // The heading is retitled here as well as in `loadDomains`, which runs once at
    // mount: without it, switching domains left the previous domain's name over a
    // table of the new domain's aliases.
    const wanted = domains.find((domain) => String(domain.id) === picker.value);
    if (wanted) setText(aliasTitle, `Aliases — ${wanted.name}`);
    go('aliases', { domain: picker.value }, { replace: true });
    refresh();
  }

  async function loadDomains() {
    try {
      const payload = await request(`${API_BASE}/domains`, { toast: false });
      domains = domainsOf(payload);
      // Resolved before the options are appended. Once a <select> holds options an
      // untouched one already reports the first of them rather than '', so asking
      // `currentDomainId()` after this loop would always answer with the first domain
      // and the `?domain=` parameter would be ignored — which is how the Domains view
      // links here, so choosing "Aliases" on a row landed on the wrong domain.
      const wanted = domains.find((domain) => domain.id === currentDomainId()) || domains[0] || null;
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

  const handlers = {
    onDetails: (alias) => openAliasDrawer(alias, currentDomain, handlers),
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
    onBulkEnabled: (ids, enabled) => setEnabled(ids, enabled, () => refresh()),
    onBulkDelete: (ids) => deleteAliases(ids, () => refresh()),
  };

  async function refresh() {
    const domainId = currentDomainId();
    if (!domainId) {
      if (domains.length === 0 && picker.disabled) card.setState({ state: 'empty' });
      return;
    }
    currentDomain = domains.find((domain) => domain.id === domainId) || null;
    card.setState({ state: 'loading' });
    try {
      const payload = await request(`${API_BASE}/domains/${domainId}/aliases`, { toast: false });
      const aliases = aliasesOf(payload);
      if (aliases.length === 0) {
        card.setState({ state: 'empty' });
        return;
      }
      card.setState({ state: 'ready', data: { node: renderTable(aliases, handlers) } });
    } catch (error) {
      card.setState({ state: 'error', message: messageOf(error, 'Aliases could not be loaded.') });
    }
  }

  const ok = await loadDomains();
  if (ok) await refresh();
  else card.setState({ state: 'error', message: 'Domains could not be loaded.' });

  return { node: root, cleanup() {} };
}

/* --------------------------------------------------------------- bulk actions */

/** Enable or disable several aliases, reporting only what failed. */
async function setEnabled(ids, enabled, refresh) {
  const results = await Promise.allSettled(
    ids.map((id) =>
      request(`${API_BASE}/aliases/${id}`, { method: 'PATCH', body: { enabled }, toast: false }),
    ),
  );
  const failed = results.filter((result) => result.status === 'rejected').length;
  if (failed > 0) toastError(`${failed} of ${ids.length} alias(es) could not be updated.`);
  else toastSuccess(`${ids.length} alias(es) ${enabled ? 'enabled' : 'disabled'}.`);
  refresh();
}

/**
 * Delete several aliases behind one confirmation.
 *
 * `confirmDialog` rather than the typed-local-part `promptDialog` the single-alias
 * path uses: typing twenty local parts is not a confirmation, it is a deterrent, and
 * a bulk action nobody can complete is worse than one that asks once.
 */
async function deleteAliases(ids, refresh) {
  const confirmed = await confirmDialog({
    title: `Delete ${ids.length} alias(es)?`,
    message: 'Mail sent to a deleted alias stops being forwarded; the destination is untouched.',
    confirmLabel: 'Delete',
  });
  if (!confirmed) return;
  const results = await Promise.allSettled(
    ids.map((id) => request(`${API_BASE}/aliases/${id}`, { method: 'DELETE', toast: false })),
  );
  const failed = results.filter((result) => result.status === 'rejected').length;
  if (failed > 0) toastError(`${failed} of ${ids.length} alias(es) could not be deleted.`);
  else toastSuccess(`${ids.length} alias(es) deleted.`);
  refresh();
}

/* ------------------------------------------------------------------ rendering */

/**
 * The alias table for one domain.
 *
 * @param {Array<object>} aliases
 */
function renderTable(aliases, handlers) {
  const rows = aliases.map((alias) => {
    const createdAt = alias.createdAt;
    return {
      key: alias.id,
      alias,
      createdAt,
      cells: [
        cell(alias.localPart, 'cell-mono'),
        cell(alias.target || '—', 'cell-mono'),
        badge(alias.enabled ? 'enabled' : 'disabled'),
        cell(createdAt ? formatLogStamp(createdAt) : '—', 'cell-mono'),
        actions(
          button('Details', () => handlers.onDetails(alias)),
          button('Retarget', () => handlers.onEdit(alias)),
          button('Delete', () => handlers.onDelete(alias), 'btn-danger'),
        ),
      ],
    };
  });

  return dataTable({
    columns: [
      { key: 'alias', label: 'Alias', value: (row) => row.alias.localPart },
      { key: 'target', label: 'Forwards to', value: (row) => row.alias.target },
      { key: 'state', label: 'State', value: (row) => (row.alias.enabled ? 1 : 0) },
      {
        key: 'created',
        label: 'Created',
        value: (row) => (row.createdAt ? Date.parse(row.createdAt) : 0),
      },
      { key: 'actions', label: 'Actions', sortable: false },
    ],
    rows,
    selectable: true,
    bulkActions: [
      { label: 'Enable', onClick: (ids) => handlers.onBulkEnabled(ids, true) },
      { label: 'Disable', onClick: (ids) => handlers.onBulkEnabled(ids, false) },
      { label: 'Delete', tone: 'danger', onClick: (ids) => handlers.onBulkDelete(ids) },
    ],
    emptyMessage: 'No aliases for this domain.',
  }).node;
}

/**
 * The detail drawer: the fields a row has no space for.
 *
 * The two dialogs the row used to open stay reachable here as well, so trimming the
 * row to its three most useful actions cannot strand a retarget or a delete; both
 * close the panel first so the operator is not left with a drawer behind a modal
 * about the same alias.
 *
 * @param {object} alias
 * @param {object|null} domain the domain the alias belongs to, when it is known
 */
function openAliasDrawer(alias, domain, handlers) {
  const address = domain ? `${alias.localPart}@${domain.name}` : alias.localPart;
  const createdAt = alias.createdAt;

  const body = el('div', {}, [
    definitionList([
      ['Alias', cell(address, 'cell-mono')],
      ['Forwards to', cell(alias.target || '—', 'cell-mono')],
      ['State', alias.enabled ? 'enabled' : 'disabled'],
      ['Domain', domain ? domain.name : '—'],
      ['Created', createdAt ? formatLogStamp(createdAt) : '—'],
    ]),
  ]);

  const retarget = button('Retarget', () => {
    drawer.close();
    handlers.onEdit(alias);
  });
  const toggle = button(alias.enabled ? 'Disable' : 'Enable', () => {
    drawer.close();
    handlers.onToggle(alias);
  });
  const remove = button('Delete', () => {
    drawer.close();
    handlers.onDelete(alias);
  }, 'btn-danger');

  const drawer = openDrawer({
    title: address,
    subtitle: alias.target ? `forwards to ${alias.target}` : 'no destination set',
    body,
    actions: [retarget, toggle, remove],
  });
}

/* --------------------------------------------------------------------- create */

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

/* -------------------------------------------------------------------- helpers */

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
