/**
 * Audit log: `GET /api/v1/audit?actor_user_id=&action=&target_type=&since=&limit=&offset=`.
 *
 * The trail is append-only on the server and this console has no verb that writes to it,
 * so the table is not selectable: an audit row cannot be edited, retried or cleared, and
 * checkboxes would imply otherwise. The list is for reading, sorting and opening one
 * entry — its full `details` object and the client's user agent live in a detail drawer.
 */

import { API_BASE, ApiError, query, request } from '../api.js';
import { listOf, normalizeAuditEntry, totalOf } from '../data.js';
import { el } from '../dom.js';
import { formatLogStamp } from '../format.js';
import { go } from '../router.js';
import { toastSuccess } from '../toast.js';
import {
  actions,
  adminCard,
  cell,
  copyToClipboard,
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
    actor: params.get('actor') || '',
    action: params.get('action') || '',
    target: params.get('target') || '',
    since: params.get('since') || '',
    offset: Number.parseInt(params.get('offset') || '0', 10) || 0,
  };

  const actor = el('input', { class: 'input', id: 'audit-actor', type: 'text', placeholder: 'user id' });
  actor.value = state.actor;
  const action = el('input', { class: 'input', id: 'audit-action', type: 'text', placeholder: 'mail.sent' });
  action.value = state.action;
  const target = el('input', { class: 'input', id: 'audit-target', type: 'text', placeholder: 'user' });
  target.value = state.target;
  const since = el('input', { class: 'input', id: 'audit-since', type: 'datetime-local' });
  since.value = state.since;

  const bar = filterBar({
    id: 'audit-filter',
    fields: [
      field('Actor user id', actor, 'The numeric id of the account that acted.'),
      field('Action', action, 'Exact action name, e.g. “user.updated”.'),
      // `?target_type=` is the API's own parameter for what the entry acted on, and it is
      // the only target filter the trail offers — there is no search by `target_id`.
      field('Target kind', target, 'Substring of the target type, e.g. “user” or “storage”.'),
      field('Since', since, 'Local time; sent as UTC.'),
      el('button', { type: 'submit', class: 'btn', text: 'Apply' }),
    ],
    // Clearing is not a filter and not a submit: it belongs beside the fields, with the
    // bar's other actions, so it cannot be triggered by pressing Enter in a text box.
    actions: [button('Clear', () => go('audit', {}))],
  });
  // `submit` bubbles, so the listener belongs on the bar rather than on the <form> that
  // `filterBar` builds internally.
  bar.addEventListener('submit', (event) => {
    event.preventDefault();
    const parsed = since.value ? new Date(since.value) : null;
    go('audit', {
      actor: actor.value.trim(),
      action: action.value.trim(),
      target: target.value.trim(),
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
    bar,
    card.node,
  ]);

  const handlers = {
    onDetails: (entry) => openEntryDrawer(entry),
    onPage: (offset) => pageTo(offset),
  };

  /** Keep the active filters while paging. */
  function pageTo(offset) {
    go('audit', {
      actor: state.actor,
      action: state.action,
      target: state.target,
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
          target_type: state.target || undefined,
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
      card.setState({
        state: 'ready',
        data: { node: renderTable(entries, totalOf(payload), state.offset, handlers) },
      });
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

/* ------------------------------------------------------------------ rendering */

/**
 * @param {object[]} entries
 * @param {number} total
 * @param {number} offset
 * @param {{onDetails: (entry: object) => void, onPage: (offset: number) => void}} handlers
 */
function renderTable(entries, total, offset, handlers) {
  const rows = entries.map((entry) => ({
    // `audit_log.id` is a real primary key, so it is the row key rather than a position.
    key: entry.id,
    entry,
    cells: [
      cell(entry.at ? formatLogStamp(entry.at) : '—', 'cell-mono'),
      cell(actorText(entry)),
      cell(entry.action || '—', 'cell-mono'),
      cell(targetText(entry)),
      cell(detailText(entry), 'truncate cell-mono'),
      cell(entry.ip || '—', 'cell-mono'),
      actions(button('Details', () => handlers.onDetails(entry))),
    ],
  }));

  const grid = dataTable({
    columns: [
      // A timestamp is shown as text but sorted as an instant: comparing the rendered
      // strings would order the page by the day of the month first.
      { key: 'when', label: 'When', value: (row) => (row.entry.at ? Date.parse(row.entry.at) : 0) },
      // Ids are numbers, so they sort as numbers. As text, "10" would come before "9".
      {
        key: 'actor',
        label: 'Actor',
        value: (row) => (row.entry.actorUserId === null || row.entry.actorUserId === undefined ? 0 : Number(row.entry.actorUserId)),
      },
      { key: 'action', label: 'Action', value: (row) => row.entry.action || '' },
      { key: 'target', label: 'Target', value: (row) => targetText(row.entry) },
      { key: 'detail', label: 'Detail', value: (row) => detailText(row.entry) },
      { key: 'source', label: 'Source', value: (row) => row.entry.ip || '' },
      { key: 'actions', label: 'Actions', sortable: false },
    ],
    rows,
    // The audit trail is append-only and this console exposes no endpoint that writes to
    // it. There is no honest bulk action to offer, so there is no selection either.
    selectable: false,
    emptyMessage: 'No entries on this page.',
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

/**
 * The detail drawer for one entry.
 *
 * A row can only carry a clipped JSON blob of `details`; the drawer is where the full
 * object and the client's user agent are readable. `details` is always an object on the
 * wire — `{}` for an action that recorded none — so an empty one is stated in words
 * rather than printed as a bare brace pair.
 *
 * @param {object} entry
 */
function openEntryDrawer(entry) {
  const detail = entry.detail && typeof entry.detail === 'object' ? entry.detail : null;
  const hasDetail = detail !== null && Object.keys(detail).length > 0;
  const raw = JSON.stringify(entry.raw && typeof entry.raw === 'object' ? entry.raw : entry, null, 2);
  const rawNode = el('pre', { class: 'code-block', text: raw });

  const body = el('div', {}, [
    definitionList([
      ['When', entry.at ? formatLogStamp(entry.at) : '—'],
      ['Actor user id', entry.actorUserId === null || entry.actorUserId === undefined ? '—' : String(entry.actorUserId)],
      ['Action', entry.action || '—'],
      ['Target type', entry.targetType || '—'],
      ['Target id', entry.targetId === null || entry.targetId === undefined ? '—' : String(entry.targetId)],
      ['Source', entry.ip || '—'],
      ['User agent', userAgentText(entry)],
    ]),
    el('h3', { class: 'drawer-section', text: 'Details' }),
    hasDetail
      ? el('pre', { class: 'code-block', text: JSON.stringify(detail, null, 2) })
      : el('p', { class: 'view-sub', text: 'This action recorded no details.' }),
    el('h3', { class: 'drawer-section', text: 'Raw entry' }),
    rawNode,
  ]);

  const copy = button('Copy raw entry', async () => {
    const copied = await copyToClipboard(raw, () => selectNode(rawNode));
    toastSuccess(copied ? 'Entry copied.' : 'The entry is selected — press Ctrl/Cmd+C to copy it.');
  });

  return openDrawer({
    title: entry.action || 'Audit entry',
    subtitle: `${entry.at ? formatLogStamp(entry.at) : 'unknown time'} · ${targetText(entry)}`,
    body,
    actions: [copy],
  });
}

/* -------------------------------------------------------------------- helpers */

/**
 * The Actor cell.
 *
 * `GET /audit` reports `actor_user_id` and nothing about the account behind it — no
 * address and no display name — so the id is the honest thing to show, and the drawer
 * labels it as an id so it is not mistaken for a name. A normaliser spelling that does
 * carry a name (`actor`, `actor_email`) still wins.
 */
function actorText(entry) {
  if (entry.actor) return entry.actor;
  if (entry.actorUserId === null || entry.actorUserId === undefined) return '—';
  return String(entry.actorUserId);
}

/**
 * The Target cell: the kind of thing acted on, and its id when the entry names one.
 *
 * The server sends `target_type` and `target_id`, and `target_id` is a **string** that is
 * null for an action naming no single row (`storage.gc` sweeps the whole store), so an
 * entry with a type alone shows just the type rather than a dangling `#null`.
 */
function targetText(entry) {
  const kind = entry.targetType || '';
  const id = entry.targetId === null || entry.targetId === undefined || entry.targetId === '' ? '' : String(entry.targetId);
  if (!kind) return id ? `#${id}` : '—';
  return id ? `${kind} #${id}` : kind;
}

/**
 * One row's `details` as a single line.
 *
 * `details` is always an object, so it is serialised rather than stringified blindly; the
 * string branch is defensive, for a value that is already text.
 */
function detailText(entry) {
  if (entry.detail === null || entry.detail === undefined) return '—';
  return typeof entry.detail === 'string' ? entry.detail : JSON.stringify(entry.detail);
}

/**
 * The client's `User-Agent`.
 *
 * `normalizeAuditEntry` surfaces this as `userAgent`; it used to be read out of the
 * entry's `raw` payload from here, which made this the one place in the console that
 * bypassed the normalised shape.
 */
function userAgentText(entry) {
  return entry.userAgent || '—';
}

/** Select a node's text, so the operator can copy it when the clipboard API is blocked. */
function selectNode(node) {
  const range = document.createRange();
  range.selectNodeContents(node);
  const selection = window.getSelection();
  if (selection) {
    selection.removeAllRanges();
    selection.addRange(range);
  }
}

function button(label, onClick, className = '') {
  const node = el('button', { type: 'button', class: `btn btn-small ${className}`.trim(), text: label });
  node.addEventListener('click', onClick);
  return node;
}
