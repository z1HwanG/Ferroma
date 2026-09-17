/**
 * System logs: `GET /api/v1/logs?level=&target=&query=&since=&limit=&offset=`.
 *
 * The server keeps its most recent events in a bounded in-process ring rather than
 * shipping them anywhere, so this view reports how much the ring is holding and when
 * its oldest entry was recorded. That is the honest framing: the list is not a
 * complete log, it is lost on restart, and `buffer_entries` / `buffer_capacity` /
 * `oldest_at` are what say so.
 */

import { API_BASE, ApiError, query, request } from '../api.js';
import { listOf, num } from '../data.js';
import { el } from '../dom.js';
import { formatLogStamp } from '../format.js';
import { go } from '../router.js';
import { adminCard, badge, cell, field, pager, table, viewHead } from '../ui.js';

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

  const form = el('form', { class: 'inline-form', id: 'logs-filter' }, [
    field('Severity', level, 'A floor: “info” also returns warnings and errors.'),
    field('Target', target, 'Substring of the tracing target.'),
    field('Message', text, 'Substring of the message or of any field.'),
    field('Since', since, 'Local time; sent as UTC.'),
    el('button', { type: 'submit', class: 'btn', text: 'Apply' }),
    smallButton('Clear', () => go('logs', {})),
  ]);

  form.addEventListener('submit', (event) => {
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
    renderData: (payload) => payload.node,
  });

  const bufferLine = el('p', { class: 'view-sub', id: 'logs-buffer' });

  const root = el('div', {}, [
    viewHead('System logs', 'The most recent events this process has emitted', [refreshButton]),
    el('section', { class: 'card' }, [form]),
    bufferLine,
    card.node,
  ]);

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
      card.setState({ state: 'ready', data: { node: renderTable(entries, num(payload && payload.total, entries.length), state.offset, pageTo) } });
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

function renderTable(entries, total, offset, pageTo) {
  const rows = entries.map((entry) => [
    cell(entry.at ? formatLogStamp(entry.at) : '—'),
    badge(entry.level || 'unknown'),
    cell(entry.target || '—', 'cell-mono'),
    el('span', { class: 'truncate', title: entry.message || '', text: entry.message || '—' }),
    el('span', {
      class: 'truncate cell-mono',
      title: fieldsText(entry.fields),
      text: fieldsText(entry.fields),
    }),
  ]);

  return el('div', {}, [
    table({
      columns: [
        { label: 'When' },
        { label: 'Level' },
        { label: 'Target' },
        { label: 'Message' },
        { label: 'Fields' },
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

/** The structured fields of one entry, as one readable line. */
function fieldsText(fields) {
  if (!fields || typeof fields !== 'object') return '—';
  const parts = Object.entries(fields).map(([key, value]) => `${key}=${typeof value === 'string' ? value : JSON.stringify(value)}`);
  return parts.length ? parts.join(' ') : '—';
}

function smallButton(label, onClick) {
  const node = el('button', { type: 'button', class: 'btn btn-small', text: label });
  node.addEventListener('click', onClick);
  return node;
}
