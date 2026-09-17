/**
 * Devices: `GET /api/v1/devices?user_id=&platform=&include_revoked=&limit=&offset=`.
 *
 * Server-wide, with each owner's address resolved — the client API's own device list
 * is bearer-only and scoped to one account. Revoking marks the device revoked *and*
 * revokes its sessions, which is what makes a live socket for it disconnect.
 */

import { API_BASE, ApiError, query, request } from '../api.js';
import { listOf, num } from '../data.js';
import { el } from '../dom.js';
import { formatLogStamp } from '../format.js';
import { confirmDialog } from '../modal.js';
import { go } from '../router.js';
import { adminCard, actions, badge, cell, field, pager, table, viewHead } from '../ui.js';
import { toastError, toastSuccess } from '../toast.js';

const PAGE_SIZE = 50;

/** The platforms a client can report. */
const PLATFORMS = ['windows', 'linux', 'macos', 'android', 'ios'];

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

  const userId = el('input', { class: 'input', id: 'devices-user', type: 'number', min: '1', placeholder: 'any account' });
  userId.value = state.userId;

  const platform = el('select', { class: 'input', id: 'devices-platform' });
  platform.append(el('option', { value: '', text: 'Any platform' }));
  for (const name of PLATFORMS) platform.append(el('option', { value: name, text: name }));
  platform.value = state.platform;

  const includeRevoked = el('input', { type: 'checkbox', id: 'devices-include-revoked' });
  includeRevoked.checked = state.includeRevoked;

  const form = el('form', { class: 'inline-form', id: 'devices-filter' }, [
    field('Owner user id', userId),
    field('Platform', platform),
    el('label', { class: 'checkbox', for: 'devices-include-revoked' }, [
      includeRevoked,
      el('span', { text: 'Include revoked devices' }),
    ]),
    el('button', { type: 'submit', class: 'btn', text: 'Apply' }),
    clearButton(() => go('devices', {})),
  ]);

  form.addEventListener('submit', (event) => {
    event.preventDefault();
    go('devices', {
      user_id: userId.value.trim(),
      platform: platform.value,
      include_revoked: includeRevoked.checked ? 'true' : '',
      offset: 0,
    });
  });

  const refreshButton = el('button', { type: 'button', class: 'btn', text: 'Refresh' });
  refreshButton.addEventListener('click', () => refresh());

  const card = adminCard({
    title: 'Client installations',
    subtitle: 'GET /api/v1/devices',
    renderEmpty: () =>
      el('div', { class: 'empty-state' }, [
        el('p', { class: 'empty-title', text: 'No devices' }),
        el('p', { text: 'No client has signed in yet, or nothing matches the current filters.' }),
      ]),
    renderData: (payload) => payload.node,
  });

  const root = el('div', {}, [
    viewHead('Devices', 'Every client installation known to this server', [refreshButton]),
    el('section', { class: 'card' }, [form]),
    card.node,
  ]);

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
    const label = device.name || device.deviceUid || `device ${device.id}`;
    const confirmed = await confirmDialog({
      title: 'Revoke this device?',
      message: `${label} will be marked revoked and every session it holds will be signed out. It cannot be un-revoked; the client signs in again to register a new one.`,
      confirmLabel: 'Revoke',
    });
    if (!confirmed) return;
    try {
      await request(`${API_BASE}/devices/${device.id}/revoke`, { method: 'POST', toast: false });
      toastSuccess('Device revoked.');
      await refresh();
    } catch (error) {
      toastError(error instanceof ApiError ? error.message : 'The device could not be revoked.');
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
        data: {
          node: renderTable(devices, num(payload && payload.total, devices.length), state.offset, pageTo, revoke),
        },
      });
    } catch (error) {
      card.setState({
        state: 'error',
        message: error instanceof ApiError ? error.message : 'The device list could not be loaded.',
      });
    }
  }

  await refresh();
  return { node: root, cleanup() {} };
}

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
    revoked: source.revoked === true,
  };
}

function renderTable(devices, total, offset, pageTo, onRevoke) {
  const rows = devices.map((device) => {
    const revokeButton = el('button', {
      type: 'button',
      class: 'btn btn-small btn-danger',
      text: 'Revoke',
      'aria-label': `Revoke ${device.name || device.deviceUid || `device ${device.id}`}`,
    });
    revokeButton.disabled = device.revoked;
    revokeButton.addEventListener('click', () => onRevoke(device));

    return [
      cell(device.name || device.deviceUid || `Device ${device.id}`),
      cell(device.email || `user ${device.userId}`, 'cell-mono'),
      cell(device.platform || '—'),
      cell(device.clientVersion || '—'),
      cell(device.protocolVersion || '—', 'cell-mono'),
      cell(device.lastSeenAt ? formatLogStamp(device.lastSeenAt) : '—'),
      cell(device.lastIp || '—', 'cell-mono'),
      badge(device.revoked ? 'revoked' : 'active'),
      actions(revokeButton),
    ];
  });

  return el('div', {}, [
    table({
      columns: [
        { label: 'Device' },
        { label: 'Owner' },
        { label: 'Platform' },
        { label: 'Client' },
        { label: 'Protocol' },
        { label: 'Last seen' },
        { label: 'Last IP' },
        { label: 'Status' },
        { label: 'Actions' },
      ],
      rows,
    }),
    pager({
      offset,
      limit: PAGE_SIZE,
      total,
      onPrev: () => pageTo(offset - PAGE_SIZE),
      onNext: () => pageTo(offset + PAGE_SIZE),
    }),
  ]);
}

function clearButton(onClick) {
  const node = el('button', { type: 'button', class: 'btn btn-small', text: 'Clear' });
  node.addEventListener('click', onClick);
  return node;
}
