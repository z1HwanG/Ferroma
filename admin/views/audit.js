/**
 * Audit log: `GET /api/v1/audit?actor_user_id=&action=&since=&limit=&offset=`.
 */

import { API_BASE, ApiError, query, request } from '../api.js';
import { listOf, normalizeAuditEntry, totalOf } from '../data.js';
import { el } from '../dom.js';
import { formatLogStamp } from '../format.js';
import { go } from '../router.js';
import { adminCard, cell, field, pager, table, viewHead } from '../ui.js';

const PAGE_SIZE = 50;

/**
 * @param {URLSearchParams} params
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render(params) {
  const state = {
    actor: params.get('actor') || '',
    action: params.get('action') || '',
    since: params.get('since') || '',
    offset: Number.parseInt(params.get('offset') || '0', 10) || 0,
  };

  const actor = el('input', { class: 'input', id: 'audit-actor', type: 'text', placeholder: 'user id' });
  actor.value = state.actor;
  const action = el('input', { class: 'input', id: 'audit-action', type: 'text', placeholder: 'mail.sent' });
  action.value = state.action;
  const since = el('input', { class: 'input', id: 'audit-since', type: 'datetime-local' });
  since.value = state.since;

  const form = el('form', { class: 'inline-form', id: 'audit-filter' }, [
    field('Actor user id', actor),
    field('Action', action),
    field('Since', since, 'Local time; sent as UTC.'),
    el('button', { type: 'submit', class: 'btn', text: 'Apply' }),
    button('Clear', () => go('audit', {})),
  ]);

  form.addEventListener('submit', (event) => {
    event.preventDefault();
    const parsed = since.value ? new Date(since.value) : null;
    go('audit', {
      actor: actor.value.trim(),
      action: action.value.trim(),
      since: parsed && !Number.isNaN(parsed.getTime()) ? parsed.toISOString() : '',
      offset: 0,
    });
  });

  const refreshButton = el('button', { type: 'button', class: 'btn', text: 'Refresh' });
  refreshButton.addEventListener('click', () => refresh());

  const card = adminCard({
    title: 'Audit entries',
    subtitle: 'GET /api/v1/audit',
    renderEmpty: () =>
      el('div', { class: 'empty-state' }, [
        el('p', { class: 'empty-title', text: 'No audit entries' }),
        el('p', { text: 'Nothing matches the current filters.' }),
      ]),
    renderData: (data) => data.node,
  });

  const root = el('div', {}, [
    viewHead('Audit log', 'Who did what, and when', [refreshButton]),
    el('section', { class: 'card' }, [form]),
    card.node,
  ]);

  /** Keep the active filters while paging. */
  function pageTo(offset) {
    go('audit', {
      actor: state.actor,
      action: state.action,
      since: state.since,
      offset: Math.max(0, offset),
    });
  }

  async function refresh() {
    card.setState({ state: 'loading' });
    try {
      const payload = await request(
        `${API_BASE}/audit${query({
          actor_user_id: state.actor || undefined,
          action: state.action || undefined,
          since: state.since || undefined,
          limit: PAGE_SIZE,
          offset: state.offset,
        })}`,
        { toast: false },
      );
      const entries = listOf(payload).map(normalizeAuditEntry);
      if (entries.length === 0) {
        card.setState({ state: 'empty' });
        return;
      }
      card.setState({ state: 'ready', data: { node: renderTable(entries, totalOf(payload), state.offset, pageTo) } });
    } catch (error) {
      card.setState({
        state: 'error',
        message: error instanceof ApiError ? error.message : 'The audit log could not be loaded.',
      });
    }
  }

  await refresh();
  return { node: root, cleanup() {} };
}

function renderTable(entries, total, offset, pageTo) {
  const rows = entries.map((entry) => [
    cell(entry.at ? formatLogStamp(entry.at) : '—'),
    cell(entry.actor || (entry.actorUserId === null ? '—' : String(entry.actorUserId))),
    cell(entry.action || '—', 'cell-mono'),
    cell(entry.target || '—'),
    el('span', {
      class: 'truncate',
      title: entry.detail === null || entry.detail === undefined ? '' : JSON.stringify(entry.detail),
      text: entry.detail === null || entry.detail === undefined ? '—' : JSON.stringify(entry.detail),
    }),
    cell(entry.ip || '—', 'cell-mono'),
  ]);

  return el('div', {}, [
    table({
      columns: [
        { label: 'When' },
        { label: 'Actor' },
        { label: 'Action' },
        { label: 'Target' },
        { label: 'Detail' },
        { label: 'Source' },
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

function button(label, onClick, className = '') {
  const node = el('button', { type: 'button', class: `btn btn-small ${className}`.trim(), text: label });
  node.addEventListener('click', onClick);
  return node;
}
