/**
 * System logs: `GET /api/v1/logs?level=&target=&query=&since=&limit=&offset=`.
 *
 * The server keeps its most recent events in a bounded in-process ring rather than
 * shipping them anywhere, so this view reports how much the ring is holding and when
 * its oldest entry was recorded. That is the honest framing: the list is not a
 * complete log, it is lost on restart, and `buffer_entries` / `buffer_capacity` /
 * `oldest_at` are what say so.
 *
 * The ring has no write verb — `GET /logs` is the whole surface — so the table is not
 * selectable: there is no action a selection could carry, and offering checkboxes
 * would imply one that does not exist. What a row does offer is a detail drawer, where
 * one entry's structured fields are readable in full instead of flattened into the
 * single clipped line the table has room for.
 */

import { API_BASE, ApiError, query, request } from '../api.js';
import { listOf, num } from '../data.js';
import { el } from '../dom.js';
import { formatLogStamp } from '../format.js';
import { go } from '../router.js';
import { toastSuccess } from '../toast.js';
import {
  actions,
  adminCard,
  badge,
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

const PAGE_SIZE = 100;

/** The severities the API accepts. `level` is a floor: `info` also returns `warn`. */
const LEVELS = ['error', 'warn', 'info', 'debug', 'trace'];

/**
 * @param {URLSearchParams} params
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render(params) {
  const state = {
    level: params.get('level') || '',
    target: params.get('target') || '',
    text: params.get('query') || '',
    since: params.get('since') || '',
    offset: Number.parseInt(params.get('offset') || '0', 10) || 0,
  };

  const level = el('select', { class: 'input', id: 'logs-level' });
  level.append(el('option', { value: '', text: 'Any severity' }));
  for (const name of LEVELS) level.append(el('option', { value: name, text: name }));
  level.value = state.level;

  const target = el('input', { class: 'input', id: 'logs-target', type: 'text', placeholder: 'ferroma_smtp' });
  target.value = state.target;

  const text = el('input', { class: 'input', id: 'logs-query', type: 'text', placeholder: 'deferred' });
  text.value = state.text;

  const since = el('input', { class: 'input', id: 'logs-since', type: 'datetime-local' });
  since.value = state.since;

  const bar = filterBar({
    id: 'logs-filter',
    fields: [
      field('Severity', level, 'A floor: “info” also returns warnings and errors.'),
      field('Target', target, 'Substring of the tracing target.'),
      field('Message', text, 'Substring of the message or of any field.'),
      field('Since', since, 'Local time; sent as UTC.'),
      el('button', { type: 'submit', class: 'btn', text: 'Apply' }),
    ],
    // Clearing is not a filter and not a submit: it belongs beside the fields, with the
    // bar's other actions, so it cannot be triggered by pressing Enter in a text box.
    actions: [button('Clear', () => go('logs', {}))],
  });
  // `submit` bubbles, so the listener belongs on the bar rather than on the <form> that
  // `filterBar` builds internally.
  bar.addEventListener('submit', (event) => {
    event.preventDefault();
    const parsed = since.value ? new Date(since.value) : null;
    go('logs', {
      level: level.value,
      target: target.value.trim(),
      query: text.value.trim(),
      since: parsed && !Number.isNaN(parsed.getTime()) ? parsed.toISOString() : '',
      offset: 0,
    });
  });

  const refreshButton = el('button', { type: 'button', class: 'btn', text: 'Refresh' });
  refreshButton.addEventListener('click', () => refresh());

  const card = adminCard({
    title: 'Captured events',
    subtitle: 'GET /api/v1/logs',
    renderEmpty: () =>
      el('div', { class: 'empty-state' }, [
        el('p', { class: 'empty-title', text: 'Nothing captured yet' }),
        el('p', { text: 'The in-process ring is empty, or nothing matches the current filters.' }),
      ]),
    renderData: (data) => data.node,
  });

  const bufferLine = el('p', { class: 'view-sub', id: 'logs-buffer' });

  const root = el('div', {}, [
    viewHead('System logs', 'The most recent events this process has emitted', [refreshButton]),
    bar,
    bufferLine,
    card.node,
  ]);

  const handlers = {
    onDetails: (entry) => openEntryDrawer(entry),
    onPage: (offset) => pageTo(offset),
  };

  /** Keep the active filters while paging. */
  function pageTo(offset) {
    go('logs', {
      level: state.level,
      target: state.target,
      query: state.text,
      since: state.since,
      offset: Math.max(0, offset),
    });
  }

  async function refresh() {
    card.setState({ state: 'loading' });
    try {
      const payload = await request(
        `${API_BASE}/logs${query({
          level: state.level || undefined,
          target: state.target || undefined,
          query: state.text || undefined,
          since: state.since || undefined,
          limit: PAGE_SIZE,
          offset: state.offset,
        })}`,
        { toast: false },
      );
      const entries = listOf(payload);
      const held = num(payload && payload.buffer_entries, entries.length);
      const capacity = num(payload && payload.buffer_capacity, 0);
      const oldest = payload && payload.oldest_at ? formatLogStamp(payload.oldest_at) : '—';
      bufferLine.textContent = `Ring: ${held} of ${capacity} entries held; oldest recorded ${oldest}. The ring lives in memory and is lost on restart.`;

      if (entries.length === 0) {
        card.setState({ state: 'empty' });
        return;
      }
      card.setState({
        state: 'ready',
        data: { node: renderTable(entries, num(payload && payload.total, entries.length), state.offset, handlers) },
      });
    } catch (error) {
      bufferLine.textContent = '';
      card.setState({
        state: 'error',
        message: error instanceof ApiError ? error.message : 'The system log could not be loaded.',
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
  const rows = entries.map((entry, index) => ({
    // The ring assigns no id, so a row's key is its position on the page. It exists
    // only to give the <tr> a stable `data-key`; nothing keys state off it, because
    // selection is off.
    key: offset + index,
    entry,
    cells: [
      cell(entry.at ? formatLogStamp(entry.at) : '—', 'cell-mono'),
      badge(entry.level || 'unknown'),
      cell(entry.target || '—', 'cell-mono'),
      el('span', { class: 'truncate', title: entry.message || '', text: entry.message || '—' }),
      el('span', { class: 'truncate cell-mono', title: fieldsText(entry.fields), text: fieldsText(entry.fields) }),
      actions(button('Details', () => handlers.onDetails(entry))),
    ],
  }));

  const grid = dataTable({
    columns: [
      // A timestamp is shown as text but sorted as an instant: comparing the rendered
      // strings would order the page by the day of the month first.
      { key: 'when', label: 'When', value: (row) => (row.entry.at ? Date.parse(row.entry.at) : 0) },
      { key: 'level', label: 'Level', value: (row) => severityRank(row.entry.level) },
      { key: 'target', label: 'Target', value: (row) => row.entry.target || '' },
      { key: 'message', label: 'Message', value: (row) => row.entry.message || '' },
      { key: 'fields', label: 'Fields', value: (row) => fieldsText(row.entry.fields) },
      { key: 'actions', label: 'Actions', sortable: false },
    ],
    rows,
    // `GET /logs` has no companion write endpoint, so there is nothing a selection
    // could act on. Checkboxes here would promise a bulk action the API cannot serve.
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
 * The row has room for the message and one flattened field line; the drawer is where an
 * operator reads a single event in full. `fields` is a map of JSON **strings** — the
 * buffer stores every value as the text `tracing` produced, an attempt count and a
 * queue id alike — so a value is shown exactly as it was captured and is never parsed
 * back into a number, which would reformat the evidence.
 *
 * @param {object} entry
 */
function openEntryDrawer(entry) {
  const captured = Object.entries(entry.fields && typeof entry.fields === 'object' ? entry.fields : {});
  const raw = JSON.stringify(entry, null, 2);
  const rawNode = el('pre', { class: 'code-block', text: raw });

  const body = el('div', {}, [
    definitionList([
      ['When', entry.at ? formatLogStamp(entry.at) : '—'],
      ['Level', badge(entry.level || 'unknown')],
      ['Target', entry.target || '—'],
      ['Message', entry.message || '—'],
    ]),
    el('h3', { class: 'drawer-section', text: 'Fields' }),
    captured.length === 0
      ? el('p', { class: 'view-sub', text: 'This entry captured no structured fields.' })
      : definitionList(captured.map(([key, value]) => [key, fieldValue(value)])),
    el('h3', { class: 'drawer-section', text: 'Raw entry' }),
    rawNode,
  ]);

  const copy = button('Copy raw entry', async () => {
    const copied = await copyToClipboard(raw, () => selectNode(rawNode));
    toastSuccess(copied ? 'Entry copied.' : 'The entry is selected — press Ctrl/Cmd+C to copy it.');
  });

  return openDrawer({
    title: entry.message || 'Log entry',
    subtitle: `${entry.at ? formatLogStamp(entry.at) : 'unknown time'} · ${entry.level || 'unknown'}`,
    body,
    actions: [copy],
  });
}

/* -------------------------------------------------------------------- helpers */

/**
 * The sort value for the Level column.
 *
 * Sorting the column by its rendered text would order it `debug, error, info, trace,
 * warn` — an alphabet, not a severity. The rank follows `LEVELS`, so an ascending sort
 * puts the most severe entry first; a level the API adds later sorts last rather than
 * disappearing among the known ones.
 */
function severityRank(level) {
  const index = LEVELS.indexOf(String(level || '').toLowerCase());
  return index === -1 ? LEVELS.length : index;
}

/** The structured fields of one entry, as one readable line. */
function fieldsText(fields) {
  if (!fields || typeof fields !== 'object') return '—';
  const parts = Object.entries(fields).map(([key, value]) => `${key}=${fieldValue(value)}`);
  return parts.length ? parts.join(' ') : '—';
}

/**
 * One `fields` value as text.
 *
 * The contract is that every value is already a JSON string, numbers included, so it is
 * returned untouched. The object branch is defensive only: should a value ever arrive
 * as a nested object, it renders as JSON instead of `[object Object]`.
 */
function fieldValue(value) {
  if (typeof value === 'string') return value;
  if (value === null || value === undefined) return '—';
  return JSON.stringify(value);
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
