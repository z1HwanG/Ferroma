/**
 * Mail queue: filter by status, page through entries, retry or cancel one or
 * several, and inspect the per-entry delivery log from `GET /api/v1/queue/:id`.
 */

import { API_BASE, ApiError, query, request } from '../api.js';
import { attemptLogOf, normalizeQueueEntry, num, queueEntriesOf, totalOf } from '../data.js';
import { el } from '../dom.js';
import { formatLogStamp } from '../format.js';
import { confirmDialog } from '../modal.js';
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

  const card = adminCard({
    title: 'Queue entries',
    subtitle: 'Newest first; sort by a column heading, or select rows to retry or cancel several at once.',
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

  const statusSelect = el('select', { class: 'input', id: 'queue-status' });
  for (const status of STATUSES) {
    statusSelect.append(el('option', { value: status, text: status === '' ? 'all statuses' : status }));
  }
  statusSelect.value = state.status;

  const bar = filterBar({
    id: 'queue-filter',
    fields: [
      field('Status', statusSelect, 'Only entries in this state.'),
      el('button', { type: 'submit', class: 'btn', text: 'Apply' }),
    ],
  });
  // `submit` bubbles, so the listener belongs on the bar rather than on the <form>
  // that `filterBar` builds internally.
  bar.addEventListener('submit', (event) => {
    event.preventDefault();
    go('queue', { status: statusSelect.value, offset: 0 });
  });
  // Picking a status reloads immediately; the Apply button stays for the keyboard
  // path, where submitting the form is the only way to reach the filter.
  statusSelect.addEventListener('change', () => {
    go('queue', { status: statusSelect.value, offset: 0 });
  });

  const refreshButton = el('button', { type: 'button', class: 'btn', text: 'Refresh' });
  refreshButton.addEventListener('click', () => refresh());

  const root = el('div', {}, [
    viewHead('Mail queue', 'Outbound delivery, attempt by attempt', [refreshButton]),
    bar,
    card.node,
  ]);

  const handlers = {
    onDetails: (entry) => openEntryDrawer(entry, handlers),
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
    onBulkRetry: (ids) => retryEntries(ids, () => refresh()),
    onBulkCancel: (ids) => cancelEntries(ids, () => refresh()),
    onPage: (offset) => go('queue', { status: state.status, offset }),
  };

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

/* --------------------------------------------------------------- bulk actions */

/**
 * Requeue several entries, reporting only what failed.
 *
 * `Promise.allSettled`, not `all`: `POST /queue/:id/retry` answers 409 for an entry
 * that is already delivered or cancelled, and one such row must not stop the rest of
 * the selection from being requeued.
 */
async function retryEntries(ids, refresh) {
  const results = await Promise.allSettled(
    ids.map((id) => request(`${API_BASE}/queue/${id}/retry`, { method: 'POST', toast: false })),
  );
  const failed = results.filter((result) => result.status === 'rejected').length;
  if (failed > 0) toastError(`${failed} of ${ids.length} entry(s) could not be retried.`);
  else toastSuccess(`${ids.length} entry(s) requeued.`);
  refresh();
}

/**
 * Withdraw several entries behind one confirmation.
 *
 * Deleting a queue row is how a delivery is stopped, and it cannot be undone, so a
 * bulk selection asks once rather than per row — the same trade `deleteUsers` makes.
 */
async function cancelEntries(ids, refresh) {
  const confirmed = await confirmDialog({
    title: `Cancel ${ids.length} entry(s)?`,
    message: 'Every entry still pending, retrying or in flight is withdrawn; nothing is sent for it.',
    confirmLabel: 'Cancel entries',
  });
  if (!confirmed) return;
  const results = await Promise.allSettled(
    ids.map((id) => request(`${API_BASE}/queue/${id}`, { method: 'DELETE', toast: false })),
  );
  const failed = results.filter((result) => result.status === 'rejected').length;
  if (failed > 0) toastError(`${failed} of ${ids.length} entry(s) could not be cancelled.`);
  else toastSuccess(`${ids.length} entry(s) cancelled.`);
  refresh();
}

/* ------------------------------------------------------------------ rendering */

function renderTable(entries, handlers, total, offset) {
  const rows = entries.map((entry) => ({
    key: entry.id,
    entry,
    cells: [
      cell(`#${entry.id}`, 'cell-mono'),
      badge(entry.status),
      truncateCell(entry.recipient),
      cell(String(entry.attempts)),
      cell(
        entry.nextAttemptAt
          ? formatLogStamp(entry.nextAttemptAt)
          : entry.createdAt
            ? formatLogStamp(entry.createdAt)
            : '—',
        'cell-mono',
      ),
      truncateCell(entry.lastError),
      actions(
        button('Details', () => handlers.onDetails(entry)),
        retryable(entry) ? button('Retry', () => handlers.onRetry(entry)) : null,
        cancelable(entry) ? button('Cancel', () => handlers.onCancel(entry), 'btn-danger') : null,
      ),
    ],
  }));

  // There is no Subject column: `GET /queue` does not send a subject at all — only
  // `GET /queue/:id` does — so the column could only ever print “—”. The drawer shows
  // it, where the value actually exists. See `normalizeQueueEntry`'s `subjectKnown`.
  const grid = dataTable({
    columns: [
      { key: 'id', label: 'Id', value: (row) => row.entry.id },
      { key: 'status', label: 'Status', value: (row) => row.entry.status },
      { key: 'recipient', label: 'Recipient', value: (row) => row.entry.recipient },
      { key: 'attempts', label: 'Attempts', value: (row) => row.entry.attempts },
      // The column shows a date, so it must sort on one: as text, “2026-09-18” would
      // order by day-of-month before year.
      {
        key: 'due',
        label: 'Next / queued',
        value: (row) => stampValue(row.entry.nextAttemptAt || row.entry.createdAt),
      },
      { key: 'error', label: 'Last error', value: (row) => row.entry.lastError || '' },
      { key: 'actions', label: 'Actions', sortable: false },
    ],
    rows,
    selectable: true,
    bulkActions: [
      { label: 'Retry selected', onClick: (ids) => handlers.onBulkRetry(ids) },
      { label: 'Cancel selected', tone: 'danger', onClick: (ids) => handlers.onBulkCancel(ids) },
    ],
    emptyMessage: 'No queue entries on this page.',
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
 * Whether an entry can be requeued.
 *
 * `POST /queue/:id/retry` refuses a delivered or cancelled row with 409, so neither
 * offers the action; and a `pending` or `delivering` row is already on its way, so
 * requeuing it is not what the operator means by retry. Only the two states that are
 * stuck waiting for another attempt are actionable.
 */
function retryable(entry) {
  return entry.status === 'failed' || entry.status === 'retry';
}

/**
 * Whether an entry can still be withdrawn.
 *
 * The repository's cancel is `UPDATE … WHERE status IN ('pending','retry','delivering')`,
 * so a bounced (`failed`) entry is as uncancellable as a delivered one. The previous
 * grid offered Cancel on failures anyway, which meant the button always ended in a 409.
 */
function cancelable(entry) {
  return entry.status === 'pending' || entry.status === 'retry' || entry.status === 'delivering';
}

/* --------------------------------------------------------------------- drawer */

/**
 * The detail drawer: the fields a row has no space for, the subject only
 * `GET /queue/:id` sends, and the attempt log the whole view exists to show.
 *
 * @param {object} entry the row that was clicked
 * @param {object} handlers
 */
function openEntryDrawer(entry, handlers) {
  const summary = el('div', {}, [el('p', { class: 'loading-state', text: 'Loading the entry…' })]);
  const log = el('div', {}, [el('p', { class: 'loading-state', text: 'Loading the delivery log…' })]);

  const body = el('div', {}, [
    summary,
    el('h3', { class: 'drawer-section', text: 'Delivery attempts' }),
    log,
  ]);

  // Acting from the drawer closes it first: the action reloads the list, and a panel
  // still showing the entry's old state would be lying about what just happened.
  const retry = el('button', { type: 'button', class: 'btn', text: 'Retry' });
  retry.addEventListener('click', () => {
    drawer.close();
    handlers.onRetry(entry);
  });

  const cancel = el('button', { type: 'button', class: 'btn btn-danger', text: 'Cancel delivery' });
  cancel.addEventListener('click', () => {
    drawer.close();
    handlers.onCancel(entry);
  });

  /**
   * Keep the footer buttons' availability — and the reason for it — in step with an
   * entry: a button that is disabled with no explanation is a dead end.
   */
  function syncActions(current) {
    retry.disabled = !retryable(current);
    cancel.disabled = !cancelable(current);
    retry.title = retry.disabled ? 'Only a failed or retrying entry can be requeued.' : '';
    cancel.title = cancel.disabled
      ? 'Only an entry still pending, retrying or in flight can be cancelled.'
      : '';
  }
  syncActions(entry);

  const drawer = openDrawer({
    title: `Queue entry ${entry.id}`,
    subtitle: `${entry.sender || 'unknown sender'} → ${entry.recipient || 'unknown recipient'}`,
    body,
    actions: [retry, cancel],
  });

  request(`${API_BASE}/queue/${entry.id}`, { toast: false })
    .then((payload) => {
      const source = payload && typeof payload === 'object' ? payload : {};
      // The detail response wraps the row under `entry`; the fallback keeps this
      // working against an API that answers with the row itself.
      const detail = normalizeQueueEntry(source.entry || source);
      summary.replaceChildren(
        entryFields(
          detail,
          typeof source.subject === 'string' ? source.subject : null,
          num(source.recipient_count, null),
        ),
      );
      // The row's status was read before this request went out and may already be stale,
      // so the buttons are re-decided against the entry the server just returned.
      syncActions(detail);
      log.replaceChildren(attemptTable(attemptLogOf(source)));
    })
    .catch((error) => {
      // The clicked row still describes the entry well enough to read while the log is
      // out of reach, so it stands in rather than leaving an empty panel.
      summary.replaceChildren(entryFields(entry, entry.subjectKnown ? entry.subject : null, null));
      log.replaceChildren(
        el('p', { class: 'field-error', text: messageOf(error, 'The delivery log could not be loaded.') }),
      );
    });
}

/**
 * The entry summary above the attempt log.
 *
 * `subject` is `null` until `GET /queue/:id` answers, because the list response carries
 * none — keeping that apart from an empty string is the difference between “this
 * message has no subject” and “this response did not say”, and the two must not read
 * the same.
 *
 * @param {object} entry
 * @param {string|null} subject
 * @param {number|null} recipientCount
 */
function entryFields(entry, subject, recipientCount) {
  return definitionList([
    ['Status', badge(entry.status)],
    ['Sender', cell(entry.sender || '—', 'cell-mono')],
    ['Recipient', cell(entry.recipient || '—', 'cell-mono')],
    ['Subject', subject === null ? 'not reported' : subject || '(no subject)'],
    ['Message recipients', recipientCount === null ? 'not reported' : String(recipientCount)],
    ['Attempts', String(entry.attempts)],
    ['Next attempt', entry.nextAttemptAt ? formatLogStamp(entry.nextAttemptAt) : '—'],
    ['Last error', entry.lastError || '—'],
    ['Queued', entry.createdAt ? formatLogStamp(entry.createdAt) : '—'],
    ['Updated', entry.updatedAt ? formatLogStamp(entry.updatedAt) : '—'],
  ]);
}

/**
 * The per-attempt table.
 *
 * Every column is `sortable: false`: the server returns the attempts oldest first, and
 * re-ordering the handful of rows belonging to one entry would only hide that ordering.
 *
 * @param {Array<object>} attempts as `attemptLogOf` returns them
 */
function attemptTable(attempts) {
  if (attempts.length === 0) {
    return el('p', { class: 'view-sub', text: 'This entry has no recorded attempt yet.' });
  }
  return dataTable({
    columns: [
      { key: 'attempt', label: '#', sortable: false },
      { key: 'at', label: 'When', sortable: false },
      { key: 'outcome', label: 'Outcome', sortable: false },
      { key: 'code', label: 'SMTP', sortable: false },
      { key: 'host', label: 'MX host', sortable: false },
      { key: 'duration', label: 'Duration', sortable: false },
      { key: 'detail', label: 'Detail', sortable: false },
    ],
    rows: attempts.map((attempt, index) => {
      // `error` on a failed attempt, the SMTP status text on a successful one — the
      // server sends whichever applies, and the other is absent.
      const detail = attempt.message || attempt.status;
      return {
        key: index,
        cells: [
          cell(attempt.attempt === null || attempt.attempt === undefined ? '—' : String(attempt.attempt), 'cell-mono'),
          // `truncate` here is about keeping the timestamp on one line: at the drawer's
          // width a wrapped “2026-09-18 19:30:20” turns one attempt into three rows.
          cell(attempt.at ? formatLogStamp(attempt.at) : '—', 'cell-mono truncate'),
          badge(outcomeOf(attempt)),
          cell(attempt.smtpCode === null || attempt.smtpCode === undefined ? '—' : String(attempt.smtpCode), 'cell-mono'),
          truncateCell(attempt.host),
          cell(attempt.durationMs === null || attempt.durationMs === undefined ? '—' : `${attempt.durationMs} ms`, 'cell-mono'),
          truncateCell(detail),
        ],
      };
    }),
    emptyMessage: 'No attempts recorded.',
  }).node;
}

/**
 * The outcome pill for one attempt.
 *
 * `GET /queue/:id` sends a numeric SMTP code and its status text but no outcome word of
 * its own, so the pill is derived from the code's first digit — the way an operator
 * already reads `250` / `451` / `550` — and the status text is shown in the Detail
 * column.
 *
 * @param {{smtpCode: unknown}} attempt
 */
function outcomeOf(attempt) {
  const code = Number(attempt.smtpCode);
  if (!Number.isFinite(code)) return 'unknown';
  const family = Math.floor(code / 100);
  if (family === 2) return 'ok';
  if (family === 4) return 'retry';
  if (family === 5) return 'failed';
  return 'unknown';
}

/* -------------------------------------------------------------------- helpers */

/** A text cell that keeps its full value in the `title` while showing the ellipsis. */
function truncateCell(text) {
  const value = text || '—';
  return el('span', { class: 'truncate', title: value, text: value });
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

function messageOf(error, fallback) {
  if (error instanceof ApiError) return error.message;
  if (error instanceof Error && error.message) return error.message;
  return fallback;
}
