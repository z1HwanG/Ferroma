/**
 * Users: search + paging over `GET /api/v1/users`, create / edit / enable /
 * disable / delete (with a typed confirmation), and the per-user address list
 * maintained through `POST /api/v1/users/:id/mailboxes`.
 */

import { API_BASE, ApiError, query, request } from '../api.js';
import { domainsOf, mailboxesOf, totalOf, usersOf } from '../data.js';
import { el, setHidden, setText } from '../dom.js';
import { formatBytes, formatLogStamp } from '../format.js';
import { openModal, promptDialog } from '../modal.js';
import { go } from '../router.js';
import { toastError, toastSuccess } from '../toast.js';
import { actions, adminCard, badge, cell, field, pager, table, viewHead } from '../ui.js';

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
    subtitle: 'GET /api/v1/users',
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

  const searchForm = el('form', { class: 'inline-form', id: 'users-search' }, [
    field('Search', search),
    el('button', { type: 'submit', class: 'btn', text: 'Search' }),
  ]);
  searchForm.addEventListener('submit', (event) => {
    event.preventDefault();
    go('users', { query: search.value.trim(), offset: 0 });
  });

  const createButton = el('button', { type: 'button', class: 'btn btn-primary', text: 'New user' });
  createButton.addEventListener('click', () => openUserDialog({}, () => refresh()));

  const refreshButton = el('button', { type: 'button', class: 'btn', text: 'Refresh' });
  refreshButton.addEventListener('click', () => refresh());

  const root = el('div', {}, [
    viewHead('Users', 'Every account hosted by this server', [createButton, refreshButton]),
    el('section', { class: 'card' }, [searchForm]),
    card.node,
  ]);

  const handlers = {
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

/* ------------------------------------------------------------------ rendering */

function renderTable(users, handlers, total, offset) {
  const rows = users.map((user) => {
    const addressCount = user.mailboxes.length;
    return [
      el('div', {}, [
        el('div', { class: 'truncate', title: user.email, text: user.email }),
        el('div', {
          class: 'view-sub',
          text: `${user.displayName || 'no display name'} · ${addressCount} address${addressCount === 1 ? '' : 'es'}`,
        }),
      ]),
      badge(user.enabled ? 'enabled' : 'disabled'),
      badge(user.isAdmin ? 'admin' : 'user'),
      cell(
        user.quotaBytes > 0
          ? `${formatBytes(user.usedBytes)} of ${formatBytes(user.quotaBytes)}`
          : `${formatBytes(user.usedBytes)} used`,
      ),
      cell(user.createdAt ? formatLogStamp(user.createdAt) : '—'),
      actions(
        button('Addresses', () => handlers.onCreateAddress(user)),
        button('Edit', () => handlers.onEdit(user)),
        button('Password', () => handlers.onPassword(user)),
        button(user.enabled ? 'Disable' : 'Enable', () => handlers.onToggle(user)),
        button('Delete', () => handlers.onDelete(user), 'btn-danger'),
      ),
    ];
  });

  return el('div', {}, [
    table({
      columns: [
        { label: 'Account' },
        { label: 'State' },
        { label: 'Role' },
        { label: 'Storage' },
        { label: 'Created' },
        { label: 'Actions' },
      ],
      rows,
    }),
    pager({
      offset,
      limit: PAGE_SIZE,
      total,
      onPrev: () => handlers.onPage(Math.max(0, offset - PAGE_SIZE)),
      onNext: () => handlers.onPage(offset + PAGE_SIZE),
    }),
  ]);
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
