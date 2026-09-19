/**
 * Devices: `GET /api/v1/devices?user_id=&platform=&include_revoked=&limit=&offset=`.
 *
 * Server-wide, with each owner's address resolved — the client API's own device list
 * is bearer-only and scoped to one account. Revoking marks the device revoked *and*
 * revokes its sessions, which is what makes a live socket for it disconnect.
 */

import { API_BASE, ApiError, query, request } from '../../shared/api.js';
import { listOf, num } from '../../shared/data.js';
import { el } from '../../shared/dom.js';
import { formatLogStamp } from '../../shared/format.js';
import { t, tn } from '../../shared/i18n.js';
import { confirmDialog } from '../../shared/modal.js';
import { go } from '../router.js';
import { toastError, toastSuccess } from '../../shared/toast.js';
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

/** The platforms a client can report. */
const PLATFORMS = ['windows', 'linux', 'macos', 'android', 'ios'];

/**
 * A person-readable label for a reported platform.
 *
 * `platform` is API data — it is what the query sends and what the filter compares — so
 * it is translated only where it is displayed, and each label is a literal inside `t()`
 * so the catalog stays checkable.
 *
 * @param {unknown} platform
 * @returns {string}
 */
function platformLabel(platform) {
  switch (String(platform || '').toLowerCase()) {
    case 'windows':
      return t('Windows');
    case 'linux':
      return t('Linux');
    case 'macos':
      return t('macOS');
    case 'android':
      return t('Android');
    case 'ios':
      return t('iOS');
    default:
      return t('Unknown');
  }
}

/**
 * A person-readable label for a device's revocation state.
 *
 * @param {unknown} state either `revoked` or `active`
 * @returns {string}
 */
function stateLabel(state) {
  switch (String(state || '').toLowerCase()) {
    case 'revoked':
      return t('Revoked');
    case 'active':
      return t('Active');
    default:
      return t('Unknown');
  }
}

/**
 * The state pill, showing the translated state.
 *
 * The raw value still decides the pill's colour; only the text is replaced.
 *
 * @param {unknown} state
 * @returns {Element}
 */
function stateBadge(state) {
  const node = badge(state);
  node.textContent = stateLabel(state);
  return node;
}

/**
 * @param {URLSearchParams} params
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render(params) {
  const state = {
    userId: params.get('user_id') || '',
    platform: params.get('platform') || '',
    includeRevoked: params.get('include_revoked') === 'true',
    offset: Number.parseInt(params.get('offset') || '0', 10) || 0,
  };

  const card = adminCard({
    title: t('Client installations'),
    subtitle: t('Sort by a column heading; select rows to revoke several at once.'),
    renderEmpty: () =>
      el('div', { class: 'empty-state' }, [
        el('p', { class: 'empty-title', text: t('No devices') }),
        el('p', { text: t('No client has signed in yet, or nothing matches the current filters.') }),
      ]),
    renderData: (data) => data.node,
  });

  const userId = el('input', { class: 'input', id: 'devices-user', type: 'number', min: '1', placeholder: t('any account') });
  userId.value = state.userId;

  const platform = el('select', { class: 'input', id: 'devices-platform' });
  platform.append(el('option', { value: '', text: t('Any platform') }));
  for (const name of PLATFORMS) platform.append(el('option', { value: name, text: platformLabel(name) }));
  platform.value = state.platform;

  const includeRevoked = el('input', { type: 'checkbox', id: 'devices-include-revoked' });
  includeRevoked.checked = state.includeRevoked;

  const bar = filterBar({
    id: 'devices-filter',
    fields: [
      field(t('Owner user id'), userId, t('The numeric account id; leave empty for every account.')),
      field(t('Platform'), platform),
      el('label', { class: 'checkbox', for: 'devices-include-revoked' }, [
        includeRevoked,
        el('span', { text: t('Include revoked devices') }),
      ]),
      el('button', { type: 'submit', class: 'btn', text: t('Apply') }),
    ],
    actions: [clearButton(() => go('devices', {}))],
  });
  // `submit` bubbles, so the listener belongs on the bar rather than on the <form>
  // that `filterBar` builds internally.
  bar.addEventListener('submit', (event) => {
    event.preventDefault();
    go('devices', {
      user_id: userId.value.trim(),
      platform: platform.value,
      include_revoked: includeRevoked.checked ? 'true' : '',
      offset: 0,
    });
  });

  const refreshButton = el('button', { type: 'button', class: 'btn', text: t('Refresh') });
  refreshButton.addEventListener('click', () => refresh());

  const root = el('div', {}, [
    viewHead(t('Devices'), t('Every client installation known to this server'), [refreshButton]),
    bar,
    card.node,
  ]);

  const handlers = {
    onDetails: (device) => openDeviceDrawer(device, handlers),
    onRevoke: (device) => revoke(device),
    onBulkRevoke: (ids) => revokeDevices(ids, () => refresh()),
    onPage: (offset) => pageTo(offset),
  };

  /** Keep the active filters while paging. */
  function pageTo(offset) {
    go('devices', {
      user_id: state.userId,
      platform: state.platform,
      include_revoked: state.includeRevoked ? 'true' : '',
      offset: Math.max(0, offset),
    });
  }

  /** Revoke one device, then repaint the table. */
  async function revoke(device) {
    const label = deviceLabel(device);
    const confirmed = await confirmDialog({
      title: t('Revoke this device?'),
      message: t('{device} will be marked revoked and every session it holds will be signed out. It cannot be un-revoked; the client signs in again to register a new one.', { device: label }),
      confirmLabel: t('Revoke'),
    });
    if (!confirmed) return;
    try {
      await request(`${API_BASE}/devices/${device.id}/revoke`, { method: 'POST', toast: false });
      toastSuccess(t('Device revoked.'));
      await refresh();
    } catch (error) {
      toastError(error instanceof ApiError ? error.message : t('The device could not be revoked.'));
    }
  }

  async function refresh() {
    card.setState({ state: 'loading' });
    try {
      const payload = await request(
        `${API_BASE}/devices${query({
          user_id: state.userId || undefined,
          platform: state.platform || undefined,
          include_revoked: state.includeRevoked ? 'true' : undefined,
          limit: PAGE_SIZE,
          offset: state.offset,
        })}`,
        { toast: false },
      );
      const devices = listOf(payload).map(normalizeDevice);
      if (devices.length === 0) {
        card.setState({ state: 'empty' });
        return;
      }
      card.setState({
        state: 'ready',
        data: { node: renderTable(devices, handlers, num(payload && payload.total, devices.length), state.offset) },
      });
    } catch (error) {
      card.setState({
        state: 'error',
        message: error instanceof ApiError ? error.message : t('The device list could not be loaded.'),
      });
    }
  }

  await refresh();
  return { node: root, cleanup() {} };
}

/* --------------------------------------------------------------- bulk actions */

/**
 * Revoke several devices behind one confirmation.
 *
 * One confirmation for the batch, not a dialog per device: revoking ten of them is one
 * decision, and `Promise.allSettled` keeps a single refusal from silently skipping the
 * devices after it. Only the failures are reported, because the count that succeeded is
 * the count the operator selected when nothing failed.
 */
async function revokeDevices(ids, refresh) {
  const confirmed = await confirmDialog({
    title: tn(ids.length, 'Revoke {count} device?', 'Revoke {count} devices?', { count: ids.length }),
    message: t('Each is marked revoked and every session it holds is signed out. This cannot be undone; a client signs in again to register a new install.'),
    confirmLabel: t('Revoke'),
  });
  if (!confirmed) return;
  const results = await Promise.allSettled(
    ids.map((id) => request(`${API_BASE}/devices/${id}/revoke`, { method: 'POST', toast: false })),
  );
  const failed = results.filter((result) => result.status === 'rejected').length;
  if (failed > 0) {
    toastError(
      tn(
        failed,
        '{failed} of {count} device could not be revoked.',
        '{failed} of {count} devices could not be revoked.',
        { failed, count: ids.length },
      ),
    );
  } else {
    toastSuccess(tn(ids.length, '{count} device revoked.', '{count} devices revoked.', { count: ids.length }));
  }
  refresh();
}

/* ------------------------------------------------------------------ rendering */

/**
 * One device row, flattened.
 *
 * `device_uid` is the client-generated installation id and is what identifies a
 * device whose owner never named it.
 *
 * @param {Record<string, unknown>} value
 */
function normalizeDevice(value) {
  const source = value && typeof value === 'object' ? value : {};
  return {
    id: num(source.id, 0),
    userId: num(source.user_id, 0),
    email: typeof source.email === 'string' ? source.email : '',
    deviceUid: typeof source.device_uid === 'string' ? source.device_uid : '',
    name: typeof source.name === 'string' ? source.name : '',
    platform: typeof source.platform === 'string' ? source.platform : '',
    clientVersion: typeof source.client_version === 'string' ? source.client_version : '',
    protocolVersion: source.protocol_version === null || source.protocol_version === undefined ? '' : String(source.protocol_version),
    lastSeenAt: source.last_seen_at || null,
    lastIp: typeof source.last_ip === 'string' ? source.last_ip : '',
    createdAt: source.created_at || null,
    revoked: source.revoked === true,
  };
}

function renderTable(devices, handlers, total, offset) {
  // The client version, the FCP version and the peer address are in the drawer: a
  // column for each made the table wider than the devices it described, and none of
  // them is what an operator scans a list for.
  const rows = devices.map((device) => ({
    key: device.id,
    device,
    cells: [
      // The name is the label, the installation id the identity — the second line
      // appears only when the two differ, so an unnamed device is not labelled twice.
      el('div', {}, [
        el('div', { class: 'truncate', title: deviceLabel(device), text: deviceLabel(device) }),
        device.name && device.deviceUid
          ? el('div', { class: 'view-sub cell-mono truncate', text: device.deviceUid })
          : null,
      ]),
      cell(device.email || t('user {id}', { id: device.userId }), 'cell-mono'),
      cell(device.platform ? platformLabel(device.platform) : '—'),
      cell(device.lastSeenAt ? formatLogStamp(device.lastSeenAt) : '—', 'cell-mono'),
      stateBadge(device.revoked ? 'revoked' : 'active'),
      actions(
        button(t('Details'), () => handlers.onDetails(device)),
        revokeButton(device, handlers),
      ),
    ],
  }));

  const grid = dataTable({
    columns: [
      { key: 'device', label: t('Device'), value: (row) => deviceLabel(row.device) },
      { key: 'owner', label: t('Owner'), value: (row) => row.device.email || String(row.device.userId) },
      { key: 'platform', label: t('Platform'), value: (row) => row.device.platform },
      // A date sorts as a date, never as its rendered text.
      { key: 'seen', label: t('Last seen'), value: (row) => stampValue(row.device.lastSeenAt) },
      { key: 'state', label: t('Status'), value: (row) => (row.device.revoked ? 0 : 1) },
      { key: 'actions', label: t('Actions'), sortable: false },
    ],
    rows,
    selectable: true,
    bulkActions: [{ label: t('Revoke selected'), tone: 'danger', onClick: (ids) => handlers.onBulkRevoke(ids) }],
    emptyMessage: t('No devices on this page.'),
  });

  return el('div', {}, [
    grid.node,
    pager({
      offset,
      limit: PAGE_SIZE,
      total,
      onPrev: () => handlers.onPage(offset - PAGE_SIZE),
      onNext: () => handlers.onPage(offset + PAGE_SIZE),
    }),
  ]);
}

/* --------------------------------------------------------------------- drawer */

/**
 * The device drawer: the identifying fields a list row has no space for, and the revoke
 * action, which deserves a read of the whole record before it is pressed.
 *
 * @param {object} device
 * @param {object} handlers
 */
function openDeviceDrawer(device, handlers) {
  const revoke = el('button', { type: 'button', class: 'btn btn-danger', text: t('Revoke device') });
  revoke.disabled = device.revoked;
  if (device.revoked) {
    revoke.title = t('This device is already revoked; a client signs in again to register a new one.');
  }
  revoke.addEventListener('click', () => {
    drawer.close();
    handlers.onRevoke(device);
  });

  const drawer = openDrawer({
    title: deviceLabel(device),
    subtitle: device.email || t('user {id}', { id: device.userId }),
    body: el('div', {}, [
      definitionList([
        [t('Status'), stateBadge(device.revoked ? 'revoked' : 'active')],
        [t('Device UID'), cell(device.deviceUid || '—', 'cell-mono')],
        [t('Owner'), cell(device.email || '—', 'cell-mono')],
        // The id is repeated here on purpose: it is the value the list's own
        // `user_id` filter takes, and it is not otherwise recoverable from the row.
        [t('Owner user id'), cell(String(device.userId), 'cell-mono')],
        [t('Platform'), device.platform ? platformLabel(device.platform) : '—'],
        [t('Client version'), device.clientVersion || '—'],
        [t('Protocol version'), device.protocolVersion || '—'],
        [t('Registered'), device.createdAt ? formatLogStamp(device.createdAt) : '—'],
        [t('Last seen'), device.lastSeenAt ? formatLogStamp(device.lastSeenAt) : '—'],
        [t('Last IP'), cell(device.lastIp || '—', 'cell-mono')],
        [t('Device id'), cell(String(device.id), 'cell-mono')],
      ]),
    ]),
    actions: [revoke],
  });
}

/* -------------------------------------------------------------------- helpers */

/** What a device is called, in the order that identifies it: name, uid, row id. */
function deviceLabel(device) {
  return device.name || device.deviceUid || t('Device {id}', { id: device.id });
}

/**
 * The per-row revoke button.
 *
 * A revoked device has nothing left to revoke, so the button stays in place but
 * disabled: removing it would make a row shorter than its neighbours and leave no sign
 * that the action exists at all.
 */
function revokeButton(device, handlers) {
  const node = button(t('Revoke'), () => handlers.onRevoke(device), 'btn-danger');
  node.disabled = device.revoked;
  node.setAttribute('aria-label', t('Revoke {device}', { device: deviceLabel(device) }));
  return node;
}

/**
 * Milliseconds for a timestamp, for a numeric sort.
 *
 * `Date.parse` answers `NaN` for a value the server did not send in a form the browser
 * understands, and `NaN` comparisons make `Array#sort` order arbitrarily; an absent or
 * unparseable stamp sorts as the oldest row instead.
 */
function stampValue(value) {
  const parsed = value ? Date.parse(value) : Number.NaN;
  return Number.isFinite(parsed) ? parsed : 0;
}

function button(label, onClick, className = '') {
  const node = el('button', { type: 'button', class: `btn btn-small ${className}`.trim(), text: label });
  node.addEventListener('click', onClick);
  return node;
}

function clearButton(onClick) {
  const node = el('button', { type: 'button', class: 'btn btn-small', text: t('Clear') });
  node.addEventListener('click', onClick);
  return node;
}
