/**
 * Users: search + paging over `GET /api/v1/users`, create / edit / enable /
 * disable / delete (with a typed confirmation), and the per-user address list
 * maintained through `POST /api/v1/users/:id/mailboxes`.
 */

import { API_BASE, ApiError, query, request } from '../api.js';
import { domainsOf, mailboxesOf, totalOf, usersOf } from '../data.js';
import { clear, el, setHidden, setText } from '../dom.js';
import { formatBytes, formatLogStamp } from '../format.js';
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
  pager,
  viewHead,
} from '../ui.js';

const PAGE_SIZE = 50;

/**
 * @param {URLSearchParams} params
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render(params) {
  const state = {
    query: params.get('query') || '',
    offset: Number.parseInt(params.get('offset') || '0', 10) || 0,
  };

  const card = adminCard({
    title: 'Accounts',
    subtitle: 'Sort by a column heading; select rows to act on several at once.',
    renderEmpty: () =>
      el('div', { class: 'empty-state' }, [
        el('p', { class: 'empty-title', text: state.query ? 'No matching users' : 'No users yet' }),
        el('p', {
          text: state.query
            ? `Nothing matches “${state.query}”.`
            : 'Create the first account, or run the setup wizard for a fresh install.',
        }),
      ]),
    renderData: (data) => data.node,
  });

  const search = el('input', {
    class: 'input',
    id: 'users-query',
    type: 'search',
    autocomplete: 'off',
    placeholder: 'Search by address or name',
  });
  search.value = state.query;

  const createButton = el('button', { type: 'button', class: 'btn btn-primary', text: 'New user' });
  createButton.addEventListener('click', () => openUserDialog({}, () => refresh()));

  const refreshButton = el('button', { type: 'button', class: 'btn', text: 'Refresh' });
  refreshButton.addEventListener('click', () => refresh());

  const bar = filterBar({
    id: 'users-search',
    fields: [
      field('Search', search, 'Substring of the address or the display name.'),
      el('button', { type: 'submit', class: 'btn', text: 'Search' }),
    ],
  });
  // `submit` bubbles, so the listener belongs on the bar rather than on the <form>
  // that `filterBar` builds internally.
  bar.addEventListener('submit', (event) => {
    event.preventDefault();
    go('users', { query: search.value.trim(), offset: 0 });
  });

  const root = el('div', {}, [
    viewHead('Users', 'Every account hosted by this server', [createButton, refreshButton]),
    bar,
    card.node,
  ]);

  const handlers = {
    onDetails: (user) => openUserDrawer(user, handlers),
    onCreateAddress: (user) => openAddressDialog(user, () => refresh()),
    onEdit: (user) => openUserDialog({ user }, () => refresh()),
    onToggle: async (user) => {
      try {
        await request(`${API_BASE}/users/${user.id}`, {
          method: 'PATCH',
          body: { enabled: !user.enabled },
          toast: false,
        });
        toastSuccess(`${user.email} ${user.enabled ? 'disabled' : 'enabled'}.`);
        refresh();
      } catch (error) {
        toastError(messageOf(error, 'The account could not be updated.'));
      }
    },
    onPassword: async (user) => {
      const password = await promptDialog({
        title: `New password for ${user.email}`,
        label: 'New password',
        confirmLabel: 'Set password',
        hint: 'Every other session for this account is revoked.',
      });
      if (password === null) return;
      try {
        await request(`${API_BASE}/users/${user.id}`, { method: 'PATCH', body: { password }, toast: false });
        toastSuccess('Password changed.');
      } catch (error) {
        toastError(messageOf(error, 'The password could not be changed.'));
      }
    },
    onDelete: async (user) => {
      const typed = await promptDialog({
        title: 'Delete account',
        label: `Type ${user.email} to confirm`,
        confirmLabel: 'Delete account',
        requireValue: user.email,
        hint: 'This cascades: addresses, folders, messages and queue rows go with it.',
      });
      if (typed === null) return;
      try {
        await request(`${API_BASE}/users/${user.id}`, { method: 'DELETE', toast: false });
        toastSuccess(`${user.email} deleted.`);
        refresh();
      } catch (error) {
        toastError(messageOf(error, 'The account could not be deleted.'));
      }
    },
    onBulkEnabled: (ids, enabled) => setEnabled(ids, enabled, () => refresh()),
    onBulkDelete: (ids) => deleteUsers(ids, () => refresh()),
    onPage: (offset) => go('users', { query: state.query, offset }),
  };

  async function refresh() {
    card.setState({ state: 'loading' });
    try {
      const payload = await request(
        `${API_BASE}/users${query({ query: state.query || undefined, limit: PAGE_SIZE, offset: state.offset })}`,
        { toast: false },
      );
      const users = usersOf(payload);
      if (users.length === 0) {
        card.setState({ state: 'empty' });
        return;
      }
      card.setState({
        state: 'ready',
        data: { node: renderTable(users, handlers, totalOf(payload), state.offset) },
      });
    } catch (error) {
      card.setState({ state: 'error', message: messageOf(error, 'Users could not be loaded.') });
    }
  }

  await refresh();
  return { node: root, cleanup() {} };
}

/* --------------------------------------------------------------- bulk actions */

/** Enable or disable several accounts, reporting only what failed. */
async function setEnabled(ids, enabled, refresh) {
  const results = await Promise.allSettled(
    ids.map((id) =>
      request(`${API_BASE}/users/${id}`, { method: 'PATCH', body: { enabled }, toast: false }),
    ),
  );
  const failed = results.filter((result) => result.status === 'rejected').length;
  if (failed > 0) toastError(`${failed} of ${ids.length} account(s) could not be updated.`);
  else toastSuccess(`${ids.length} account(s) ${enabled ? 'enabled' : 'disabled'}.`);
  refresh();
}

/**
 * Delete several accounts behind one confirmation.
 *
 * `confirmDialog` rather than the typed-address `promptDialog` the single-account path
 * uses: typing fifty addresses is not a confirmation, it is a deterrent, and a bulk
 * action that cannot be completed is worse than one that asks once.
 */
async function deleteUsers(ids, refresh) {
  const confirmed = await confirmDialog({
    title: `Delete ${ids.length} account(s)?`,
    message: 'This cascades: addresses, folders, messages and queue rows go with them.',
    confirmLabel: 'Delete',
  });
  if (!confirmed) return;
  const results = await Promise.allSettled(
    ids.map((id) => request(`${API_BASE}/users/${id}`, { method: 'DELETE', toast: false })),
  );
  const failed = results.filter((result) => result.status === 'rejected').length;
  if (failed > 0) toastError(`${failed} of ${ids.length} account(s) could not be deleted.`);
  else toastSuccess(`${ids.length} account(s) deleted.`);
  refresh();
}

/* ------------------------------------------------------------------ rendering */

function renderTable(users, handlers, total, offset) {
  const rows = users.map((user) => ({
    key: user.id,
    user,
    cells: [
      el('div', {}, [
        el('div', { class: 'truncate', title: user.email, text: user.email }),
        el('div', {
          class: 'view-sub',
          text: `${user.displayName || 'no display name'} · ${addressLabel(user)}`,
        }),
      ]),
      badge(user.enabled ? 'enabled' : 'disabled'),
      badge(user.isAdmin ? 'admin' : 'user'),
      cell(storageLabel(user)),
      cell(user.createdAt ? formatLogStamp(user.createdAt) : '—', 'cell-mono'),
      actions(
        button('Details', () => handlers.onDetails(user)),
        button('Edit', () => handlers.onEdit(user)),
        button('Password', () => handlers.onPassword(user)),
        button(user.enabled ? 'Disable' : 'Enable', () => handlers.onToggle(user)),
        button('Delete', () => handlers.onDelete(user), 'btn-danger'),
      ),
    ],
  }));

  const grid = dataTable({
    columns: [
      { key: 'account', label: 'Account', value: (row) => row.user.email },
      { key: 'state', label: 'State', value: (row) => (row.user.enabled ? 1 : 0) },
      { key: 'role', label: 'Role', value: (row) => (row.user.isAdmin ? 1 : 0) },
      { key: 'storage', label: 'Storage', value: (row) => row.user.usedBytes },
      {
        key: 'created',
        label: 'Created',
        value: (row) => (row.user.createdAt ? Date.parse(row.user.createdAt) : 0),
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
    emptyMessage: 'No accounts on this page.',
  });

  return el('div', {}, [
    grid.node,
    pager({
      offset,
      limit: PAGE_SIZE,
      total,
      onPrev: () => handlers.onPage(Math.max(0, offset - PAGE_SIZE)),
      onNext: () => handlers.onPage(offset + PAGE_SIZE),
    }),
  ]);
}

/**
 * How many addresses an account holds, or a plain statement when the response did not
 * say.
 *
 * `GET /users` carries no address list — only `GET /auth/me` and
 * `GET /users/:id/mailboxes` do — so a list row that printed "0 addresses" was
 * asserting something it had never been told. The drawer fetches the real list.
 */
function addressLabel(user) {
  if (!user.mailboxesKnown) return 'addresses not listed here';
  const count = user.mailboxes.length;
  return `${count} address${count === 1 ? '' : 'es'}`;
}

function storageLabel(user) {
  return user.quotaBytes > 0
    ? `${formatBytes(user.usedBytes)} of ${formatBytes(user.quotaBytes)}`
    : `${formatBytes(user.usedBytes)} used`;
}

/**
 * The detail drawer: the fields a row has no space for, plus the address list the list
 * endpoint never sends.
 *
 * @param {object} user
 */
function openUserDrawer(user, handlers) {
  const addresses = el('div', {}, [el('p', { class: 'loading-state', text: 'Loading addresses…' })]);

  const body = el('div', {}, [
    definitionList([
      ['Email', user.email],
      ['Display name', user.displayName || '—'],
      ['Administrator', user.isAdmin ? 'yes' : 'no'],
      ['Account', user.enabled ? 'enabled' : 'disabled'],
      ['Storage', storageLabel(user)],
      ['Created', user.createdAt ? formatLogStamp(user.createdAt) : '—'],
    ]),
    el('h3', { class: 'drawer-section', text: 'Addresses' }),
    addresses,
  ]);

  // The account-level actions live here rather than in the row: six buttons per row
  // pushed the table wider than the accounts it describes, and the two that open a
  // dialog close the drawer first so the operator is not left with both.
  const addAddress = el('button', { type: 'button', class: 'btn', text: 'Add address' });
  addAddress.addEventListener('click', () => {
    drawer.close();
    handlers.onCreateAddress(user);
  });
  const edit = el('button', { type: 'button', class: 'btn btn-primary', text: 'Edit account' });
  edit.addEventListener('click', () => {
    drawer.close();
    handlers.onEdit(user);
  });

  const drawer = openDrawer({
    title: user.email,
    subtitle: user.displayName || 'no display name',
    body,
    actions: [edit, addAddress],
  });

  request(`${API_BASE}/users/${user.id}/mailboxes`, { toast: false })
    .then((payload) => {
      const list = mailboxesOf(payload);
      clear(addresses);
      if (list.length === 0) {
        addresses.append(el('p', { class: 'view-sub', text: 'This account holds no address.' }));
        return;
      }
      addresses.append(
        el(
          'ul',
          { class: 'drawer-list' },
          list.map((mailbox) =>
            el('li', {}, [
              el('span', { class: 'cell-mono', text: mailbox.address }),
              mailbox.isPrimary ? badge('primary') : null,
            ]),
          ),
        ),
      );
    })
    .catch((error) => {
      clear(addresses);
      addresses.append(
        el('p', { class: 'view-sub', text: messageOf(error, 'The addresses could not be loaded.') }),
      );
    });
}

/* --------------------------------------------------------------------- forms */

/**
 * @param {{user?: object}} options
 */
function openUserDialog(options, onDone) {
  const user = options.user || null;
  const editing = Boolean(user);

  const email = el('input', {
    class: 'input',
    id: 'user-email',
    type: 'email',
    autocomplete: 'off',
    placeholder: 'alice@example.com',
  });
  email.value = editing ? user.email : '';
  email.disabled = editing;

  const displayName = el('input', { class: 'input', id: 'user-name', type: 'text', autocomplete: 'off' });
  displayName.value = editing ? user.displayName : '';

  const password = el('input', {
    class: 'input',
    id: 'user-password',
    type: 'password',
    autocomplete: 'new-password',
  });

  const quotaValue = editing ? Math.round(user.quotaBytes / (1024 * 1024)) || 0 : 1024;
  const quota = el('input', { class: 'input', id: 'user-quota', type: 'number', min: '0', step: '1' });
  quota.value = String(quotaValue);

  const admin = el('input', { type: 'checkbox', id: 'user-admin' });
  admin.checked = Boolean(editing && user.isAdmin);

  const enabled = el('input', { type: 'checkbox', id: 'user-enabled' });
  enabled.checked = editing ? user.enabled : true;

  const error = el('p', { class: 'field-error', id: 'user-error', hidden: true });

  const body = el('div', {}, [
    field('Email address', email, editing ? 'The address cannot be changed after creation.' : ''),
    field('Display name', displayName),
    editing ? null : field('Password', password, 'At least 12 characters is a good habit.'),
    field('Quota in MiB', quota, '0 means unlimited.'),
    el('label', { class: 'checkbox', for: 'user-admin' }, [admin, el('span', { text: 'Administrator' })]),
    el('label', { class: 'checkbox', for: 'user-enabled' }, [enabled, el('span', { text: 'Account enabled' })]),
    error,
  ]);

  const cancel = el('button', { type: 'button', class: 'btn', text: 'Cancel' });
  const submit = el('button', { type: 'button', class: 'btn btn-primary', text: editing ? 'Save changes' : 'Create user' });

  const modal = openModal({
    title: editing ? `Edit ${user.email}` : 'New user',
    body,
    footer: [cancel, submit],
    onMount: () => {
      cancel.addEventListener('click', () => modal.close('cancel'));
      submit.addEventListener('click', async () => {
        const quotaMiB = Number.parseInt(quota.value, 10);
        const payload = {
          display_name: displayName.value.trim() || undefined,
          quota_bytes: Number.isFinite(quotaMiB) ? Math.max(0, quotaMiB) * 1024 * 1024 : undefined,
          is_admin: admin.checked,
          enabled: enabled.checked,
        };
        if (!editing) payload.email = email.value.trim();

        if (!editing && !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email.value.trim())) {
          setText(error, 'Enter a valid email address.');
          setHidden(error, false);
          return;
        }
        if (!editing && password.value.length < 8) {
          setText(error, 'The password needs at least 8 characters.');
          setHidden(error, false);
          return;
        }
        if (!editing) payload.password = password.value;

        submit.disabled = true;
        setText(submit, 'Saving…');
        try {
          if (editing) {
            await request(`${API_BASE}/users/${user.id}`, { method: 'PATCH', body: payload, toast: false });
            toastSuccess('Account updated.');
          } else {
            await request(`${API_BASE}/users`, { method: 'POST', body: payload, toast: false });
            toastSuccess(`Account ${payload.email} created.`);
          }
          modal.close('saved');
          onDone();
        } catch (err) {
          setText(error, messageOf(err, 'The account could not be saved.'));
          setHidden(error, false);
        } finally {
          submit.disabled = false;
          setText(submit, editing ? 'Save changes' : 'Create user');
        }
      });
    },
  });
  email.focus();
}

/** Address list and creation for one user. */
async function openAddressDialog(user, onDone) {
  const listHost = el('div', {}, [el('p', { class: 'loading-state', text: 'Loading addresses…' })]);
  const domainSelect = el('select', { class: 'input', id: 'address-domain' });
  const localPart = el('input', { class: 'input', id: 'address-local', type: 'text', autocomplete: 'off' });
  const primary = el('input', { type: 'checkbox', id: 'address-primary' });
  const quota = el('input', { class: 'input', id: 'address-quota', type: 'number', min: '0', step: '1' });
  quota.value = '0';
  const error = el('p', { class: 'field-error', id: 'address-error', hidden: true });

  const body = el('div', {}, [
    listHost,
    el('h3', { class: 'card-title', text: 'Create an address' }),
    el('div', { class: 'row' }, [
      field('Domain', domainSelect),
      field('Local part', localPart, 'The part before the @.'),
    ]),
    el('div', { class: 'row' }, [
      field('Quota in MiB', quota, '0 inherits the account quota.'),
      el('label', { class: 'checkbox', for: 'address-primary' }, [
        primary,
        el('span', { text: 'Make this the primary address' }),
      ]),
    ]),
    error,
  ]);

  const close = el('button', { type: 'button', class: 'btn', text: 'Close' });
  const create = el('button', { type: 'button', class: 'btn btn-primary', text: 'Create address' });

  const modal = openModal({
    title: `Addresses for ${user.email}`,
    size: 'wide',
    body,
    footer: [close, create],
    onMount: () => {
      close.addEventListener('click', () => modal.close('close'));
      create.addEventListener('click', async () => {
        const domainId = Number.parseInt(domainSelect.value, 10);
        const value = localPart.value.trim().toLowerCase();
        if (!domainId) {
          setText(error, 'Choose a domain first.');
          setHidden(error, false);
          return;
        }
        if (!/^[a-z0-9!#$%&'*+/=?^_`{|}~.-]+$/.test(value) || value.startsWith('.') || value.endsWith('.')) {
          setText(error, 'Enter a valid local part.');
          setHidden(error, false);
          return;
        }
        const quotaMiB = Number.parseInt(quota.value, 10);
        create.disabled = true;
        setText(create, 'Creating…');
        try {
          await request(`${API_BASE}/users/${user.id}/mailboxes`, {
            method: 'POST',
            body: {
              domain: domainSelect.options[domainSelect.selectedIndex]
                ? domainSelect.options[domainSelect.selectedIndex].text
                : '',
              local_part: value,
              is_primary: primary.checked,
              quota_bytes: Number.isFinite(quotaMiB) && quotaMiB > 0 ? quotaMiB * 1024 * 1024 : undefined,
            },
            toast: false,
          });
          toastSuccess(`Address ${value} created.`);
          localPart.value = '';
          primary.checked = false;
          await loadAddresses();
          onDone();
        } catch (err) {
          setText(error, messageOf(err, 'The address could not be created.'));
          setHidden(error, false);
        } finally {
          create.disabled = false;
          setText(create, 'Create address');
        }
      });
    },
  });

  async function loadAddresses() {
    try {
      const [addresses, domains] = await Promise.all([
        request(`${API_BASE}/users/${user.id}/mailboxes`, { toast: false }),
        request(`${API_BASE}/domains`, { toast: false }).catch(() => null),
      ]);
      const list = mailboxesOf(addresses);
      listHost.replaceChildren(
        list.length === 0
          ? el('p', { class: 'view-sub', text: 'This account has no address yet.' })
          : table({
              columns: [{ label: 'Address' }, { label: 'Primary' }, { label: 'Storage' }],
              rows: list.map((mailbox) => [
                cell(mailbox.address, 'cell-mono'),
                badge(mailbox.isPrimary ? 'yes' : 'no'),
                cell(
                  mailbox.quotaBytes
                    ? `${formatBytes(mailbox.usedBytes)} of ${formatBytes(mailbox.quotaBytes)}`
                    : `${formatBytes(mailbox.usedBytes)} used`,
                ),
              ]),
            }),
      );

      const domains_ = domainsOf(domains);
      domainSelect.replaceChildren();
      if (domains_.length === 0) {
        domainSelect.append(el('option', { value: '', text: 'no domains available' }));
        domainSelect.disabled = true;
      } else {
        domainSelect.disabled = false;
        for (const domain of domains_) {
          domainSelect.append(el('option', { value: String(domain.id), text: domain.name }));
        }
      }
    } catch (err) {
      listHost.replaceChildren(
        el('p', { class: 'field-error', text: messageOf(err, 'Addresses could not be loaded.') }),
      );
    }
  }

  await loadAddresses();
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
