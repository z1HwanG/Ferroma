/**
 * Mail queue: filter by status, page through entries, retry a failed entry,
 * cancel one, and inspect the per-attempt delivery log from
 * `GET /api/v1/queue/:id`.
 */

import { API_BASE, ApiError, query, request } from '../api.js';
import { attemptLogOf, queueEntriesOf, totalOf } from '../data.js';
import { el, setText } from '../dom.js';
import { formatLogStamp } from '../format.js';
import { confirmDialog } from '../modal.js';
import { go } from '../router.js';
import { toastError, toastSuccess } from '../toast.js';
import { actions, adminCard, badge, cell, field, pager, table, viewHead } from '../ui.js';

const PAGE_SIZE = 50;
const STATUSES = ['', 'pending', 'delivering', 'delivered', 'retry', 'failed', 'cancelled'];

/**
 * @param {URLSearchParams} params
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render(params) {
  const state = {
    status: params.get('status') || '',
    offset: Number.parseInt(params.get('offset') || '0', 10) || 0,
  };

  const detailTitle = el('h2', { class: 'card-title', text: 'Delivery log' });
  const detailCard = adminCard({
    title: 'Delivery log',
    titleNode: detailTitle,
    subtitle: 'GET /api/v1/queue/:id',
    renderEmpty: () =>
      el('p', { class: 'view-sub', text: 'Choose “Delivery log” on an entry above to see every attempt.' }),
    renderData: (data) => data.node,
  });

  const statusSelect = el('select', { class: 'input', id: 'queue-status' });
  for (const status of STATUSES) {
    statusSelect.append(el('option', { value: status, text: status === '' ? 'all statuses' : status }));
  }
  statusSelect.value = state.status;

  const filterForm = el('form', { class: 'inline-form', id: 'queue-filter' }, [
    field('Status', statusSelect),
    el('button', { type: 'submit', class: 'btn', text: 'Apply' }),
  ]);
  filterForm.addEventListener('submit', (event) => {
    event.preventDefault();
    go('queue', { status: statusSelect.value, offset: 0 });
  });
  statusSelect.addEventListener('change', () => {
    go('queue', { status: statusSelect.value, offset: 0 });
  });

  const refreshButton = el('button', { type: 'button', class: 'btn', text: 'Refresh' });
  refreshButton.addEventListener('click', () => refresh());

  const card = adminCard({
    title: 'Queue entries',
    subtitle: 'GET /api/v1/queue',
    renderEmpty: () =>
      el('div', { class: 'empty-state' }, [
        el('p', { class: 'empty-title', text: 'Nothing queued' }),
        el('p', {
          text: state.status
            ? `No entries with status “${state.status}”.`
            : 'No outbound mail is waiting right now.',
        }),
      ]),
    renderData: (data) => data.node,
  });

  const root = el('div', {}, [
    viewHead('Mail queue', 'Outbound delivery, attempt by attempt', [refreshButton]),
    el('section', { class: 'card' }, [filterForm]),
    card.node,
    detailCard.node,
  ]);

  const handlers = {
    onLog: (entry) => loadDetail(entry),
    onRetry: async (entry) => {
      try {
        await request(`${API_BASE}/queue/${entry.id}/retry`, { method: 'POST', toast: false });
        toastSuccess(`Entry ${entry.id} requeued.`);
        refresh();
      } catch (error) {
        toastError(messageOf(error, 'The entry could not be retried.'));
      }
    },
    onCancel: async (entry) => {
      const confirmed = await confirmDialog({
        title: 'Cancel delivery',
        message: `Cancel the delivery to ${entry.recipient || `entry ${entry.id}`}?`,
        confirmLabel: 'Cancel delivery',
      });
      if (!confirmed) return;
      try {
        await request(`${API_BASE}/queue/${entry.id}`, { method: 'DELETE', toast: false });
        toastSuccess(`Entry ${entry.id} cancelled.`);
        refresh();
      } catch (error) {
        toastError(messageOf(error, 'The entry could not be cancelled.'));
      }
    },
    onPage: (offset) => go('queue', { status: state.status, offset }),
  };

  async function loadDetail(entry) {
    setText(detailTitle, `Delivery log — ${entry.recipient || `entry ${entry.id}`}`);
    detailCard.setState({ state: 'loading' });
    try {
      const payload = await request(`${API_BASE}/queue/${entry.id}`, { toast: false });
      const attempts = attemptLogOf(payload);
      const summary = el('div', {}, [
        table({
          columns: [{ label: 'Field' }, { label: 'Value' }],
          rows: [
            [cell('Status'), badge(entry.status)],
            [cell('Recipient'), cell(entry.recipient || '—', 'cell-mono')],
            [cell('Sender'), cell(entry.sender || '—', 'cell-mono')],
            [cell('Attempts'), cell(String(entry.attempts))],
            [cell('Next attempt'), cell(entry.nextAttemptAt ? formatLogStamp(entry.nextAttemptAt) : '—')],
            [cell('Last error'), cell(entry.lastError || '—')],
          ],
        }),
      ]);

      const log =
        attempts.length === 0
          ? el('p', { class: 'view-sub', text: 'This entry has no recorded attempts yet.' })
          : el(
              'div',
              { class: 'log-list' },
              attempts.map((attempt) =>
                el('div', { class: 'log-entry' }, [
                  el('div', { class: 'log-head' }, [
                    el('span', { class: 'log-time', text: attempt.at ? formatLogStamp(attempt.at) : 'no timestamp' }),
                    badge(attempt.status || 'unknown'),
                    attempt.smtpCode !== null ? cell(`SMTP ${attempt.smtpCode}`) : null,
                    attempt.host ? cell(attempt.host, 'cell-mono') : null,
                  ]),
                  el('p', { class: 'log-body', text: attempt.message || '(no detail)' }),
                ]),
              ),
            );

      detailCard.setState({ state: 'ready', data: { node: el('div', {}, [summary, log]) } });
    } catch (error) {
      detailCard.setState({ state: 'error', message: messageOf(error, 'The delivery log could not be loaded.') });
    }
  }

  async function refresh() {
    card.setState({ state: 'loading' });
    try {
      const payload = await request(
        `${API_BASE}/queue${query({ status: state.status || undefined, limit: PAGE_SIZE, offset: state.offset })}`,
        { toast: false },
      );
      const entries = queueEntriesOf(payload);
      if (entries.length === 0) {
        card.setState({ state: 'empty' });
        return;
      }
      card.setState({ state: 'ready', data: { node: renderTable(entries, handlers, totalOf(payload), state.offset) } });
    } catch (error) {
      card.setState({ state: 'error', message: messageOf(error, 'The queue could not be loaded.') });
    }
  }

  await refresh();
  return { node: root, cleanup() {} };
}

function renderTable(entries, handlers, total, offset) {
  const rows = entries.map((entry) => [
    cell(String(entry.id)),
    badge(entry.status),
    el('span', { class: 'truncate', title: entry.recipient, text: entry.recipient || '—' }),
    el('span', { class: 'truncate', title: entry.subject, text: entry.subject || '—' }),
    cell(String(entry.attempts)),
    cell(entry.nextAttemptAt ? formatLogStamp(entry.nextAttemptAt) : entry.createdAt ? formatLogStamp(entry.createdAt) : '—'),
    el('span', { class: 'truncate', title: entry.lastError || '', text: entry.lastError || '—' }),
    actions(
      button('Delivery log', () => handlers.onLog(entry)),
      entry.status === 'failed' || entry.status === 'retry' ? button('Retry', () => handlers.onRetry(entry)) : null,
      entry.status === 'delivered' || entry.status === 'cancelled'
        ? null
        : button('Cancel', () => handlers.onCancel(entry), 'btn-danger'),
    ),
  ]);

  return el('div', {}, [
    table({
      columns: [
        { label: 'Id' },
        { label: 'Status' },
        { label: 'Recipient' },
        { label: 'Subject' },
        { label: 'Attempts' },
        { label: 'Next / queued' },
        { label: 'Last error' },
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
