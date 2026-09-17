/**
 * Dashboard: `GET /api/v1/health`, `GET /api/v1/queue/stats`, `GET /api/v1/storage`,
 * plus a hand-drawn SVG sparkline of queue depth sampled in the browser while the
 * section stays open (a maximum of 40 samples, one per poll).
 *
 * Values the API does not report render as “—” rather than as a misleading zero.
 */

import { API_BASE, ApiError, request } from '../api.js';
import { num } from '../data.js';
import { el, clear } from '../dom.js';
import { formatBytes, formatLogStamp } from '../format.js';
import { getState, setState } from '../store.js';
import { adminCard, table, cell, badge, viewHead } from '../ui.js';

const REFRESH_MS = 30000;
const MAX_SAMPLES = 40;

/** Id of the stat grid this view builds; the checker traces it to `byId` below. */
const STATS_GRID_ID = 'dashboard-stats';

/** First present key among several spellings. */
function pick(source, keys, fallback) {
  if (source && typeof source === 'object') {
    for (const key of keys) {
      const value = source[key];
      if (value !== undefined && value !== null) return value;
    }
  }
  return fallback;
}

/**
 * Render a measurement, or “—” with an explanatory note when the API omits it.
 * @param {{label: string, value: unknown, note?: string, format?: (value: unknown) => string}} options
 */
function stat(options) {
  const raw = options.value;
  const missing = raw === undefined || raw === null || raw === '';
  const format = options.format || ((value) => String(value));
  return el('div', { class: 'stat' }, [
    el('p', { class: 'stat-label', text: options.label }),
    el('p', { class: 'stat-value', text: missing ? '—' : format(raw) }),
    el('p', { class: 'stat-note', text: missing ? options.note || 'not reported by this API version' : options.note || '' }),
  ]);
}

/**
 * @param {URLSearchParams} params
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render(params) {
  const user = getState().user;
  const statsHost = el('div', { class: 'stat-grid', id: STATS_GRID_ID });

  const root = el('div', {}, [
    viewHead('Dashboard', `Signed in as ${(user && user.email) || 'administrator'}`),
    statsHost,
  ]);

  const stateCard = adminCard({
    title: 'System health',
    subtitle: 'GET /api/v1/health',
    actions: [],
    renderData: (data) => data.node,
  });

  const queueCard = adminCard({
    title: 'Mail queue',
    subtitle: 'GET /api/v1/queue/stats',
    actions: [],
    renderData: (data) => data.node,
  });

  const storageCard = adminCard({
    title: 'Storage',
    subtitle: 'GET /api/v1/storage',
    actions: [],
    renderData: (data) => data.node,
  });

  const historyCard = adminCard({
    title: 'Queue depth history',
    subtitle: 'sampled in this browser, 30 s apart',
    renderData: (data) => data.node,
  });

  root.append(historyCard.node, queueCard.node, stateCard.node, storageCard.node);

  const refresh = async () => {
    const [health, queueStats, storage] = await Promise.all([
      fetchHealth(),
      fetchQueueStats(),
      fetchStorage(),
    ]);

    renderStats(statsHost, health, queueStats, storage);
    renderQueue(queueCard, queueStats);
    renderHistory(historyCard, queueStats);
    renderHealth(stateCard, health);
    renderStorage(storageCard, storage);
  };

  await refresh();
  const timer = window.setInterval(() => {
    if (!document.hidden) refresh();
  }, REFRESH_MS);

  return {
    node: root,
    cleanup() {
      window.clearInterval(timer);
    },
  };
}

/* -------------------------------------------------------------------- fetches */

async function fetchHealth() {
  try {
    const health = await request(`${API_BASE}/health`, { toast: false });
    setState({ health });
    return { ok: true, data: health };
  } catch (error) {
    if (error instanceof ApiError) return { ok: false, message: error.message, network: error.network };
    return { ok: false, message: 'Health could not be read.' };
  }
}

async function fetchQueueStats() {
  try {
    return { ok: true, data: await request(`${API_BASE}/queue/stats`, { toast: false }) };
  } catch (error) {
    if (error instanceof ApiError) return { ok: false, message: error.message, network: error.network };
    return { ok: false, message: 'Queue statistics could not be read.' };
  }
}

async function fetchStorage() {
  try {
    return { ok: true, data: await request(`${API_BASE}/storage`, { toast: false }) };
  } catch (error) {
    if (error instanceof ApiError) return { ok: false, message: error.message, network: error.network };
    return { ok: false, message: 'Storage figures could not be read.' };
  }
}

/* -------------------------------------------------------------------- renders */

/**
 * @param {HTMLElement} host the stat grid
 * @param {{ok: boolean, data?: object, message?: string}} health
 * @param {{ok: boolean, data?: object, message?: string}} queueStats
 * @param {{ok: boolean, data?: object, message?: string}} storage
 */
function renderStats(host, health, queueStats, storage) {
  clear(host);
  const h = health.ok ? health.data || {} : {};
  const q = queueStats.ok ? queueStats.data || {} : {};
  const s = storage.ok ? storage.data || {} : {};
  const counts = q.counts || q;
  const database = h.database || {};
  const pool = database.pool || {};

  const receivedToday = pick(q, ['received_today', 'today_received'], pick(h, ['received_today'], undefined));
  const sentToday = pick(q, ['sent_today', 'today_sent'], pick(h, ['sent_today'], undefined));

  host.append(
    stat({ label: 'Users', value: pick(s, ['users', 'user_count'], undefined), note: 'from /storage' }),
    stat({ label: 'Domains', value: pick(s, ['domains', 'domain_count'], undefined), note: 'from /storage' }),
    stat({ label: 'Received today', value: receivedToday, note: 'from /queue/stats' }),
    stat({ label: 'Sent today', value: sentToday, note: 'from /queue/stats' }),
    stat({ label: 'Queue pending', value: pick(counts, ['pending'], pick(h.queue || {}, ['pending'], undefined)) }),
    stat({
      label: 'Queue retry',
      value: pick(counts, ['retry'], pick(h.queue || {}, ['retry'], undefined)),
    }),
    stat({
      label: 'Failed deliveries',
      value: pick(counts, ['failed'], pick(h.queue || {}, ['failed'], undefined)),
      note: 'entries in the failed state',
    }),
    stat({ label: 'Mailbox storage', value: pick(s, ['maildir_bytes'], undefined), format: (value) => formatBytes(num(value)) }),
    stat({
      label: 'Attachments',
      value: pick(s, ['attachment_bytes'], undefined),
      format: (value) => formatBytes(num(value)),
    }),
    stat({
      label: 'Database size',
      value: pick(s, ['database_bytes'], undefined),
      format: (value) => formatBytes(num(value)),
    }),
    stat({
      label: 'Connection pool',
      value:
        pool.size === undefined && pool.idle === undefined
          ? undefined
          : `${num(pick(pool, ['size'], 0))} open / ${num(pick(pool, ['idle'], 0))} idle of ${num(pick(pool, ['max'], 0))}`,
      note: database.server_version ? String(database.server_version) : '',
    }),
    stat({
      label: 'Active client sessions',
      value: pick(h, ['client_sessions', 'active_client_sessions', 'sessions'], undefined),
      note: 'not part of the documented /health payload',
    }),
    stat({
      label: 'Uptime',
      value: pick(h, ['uptime_secs'], undefined),
      format: (value) => formatUptime(num(value)),
    }),
  );
}

function formatUptime(seconds) {
  if (!Number.isFinite(seconds) || seconds <= 0) return '—';
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  if (days > 0) return `${days} d ${hours} h`;
  if (hours > 0) return `${hours} h ${minutes} min`;
  return `${minutes} min`;
}

function renderQueue(card, queueStats) {
  if (!queueStats.ok) {
    card.setState({ state: 'error', message: queueStats.message });
    return;
  }
  const counts = queueStats.data && queueStats.data.counts ? queueStats.data.counts : queueStats.data || {};
  const rows = Object.keys(counts)
    .filter((key) => typeof counts[key] === 'number' || typeof counts[key] === 'string')
    .map((key) => [cell(key), cell(String(counts[key]))]);
  const nextDue = queueStats.data ? queueStats.data.next_due_at : null;
  if (nextDue) rows.push([cell('next_due_at'), cell(formatLogStamp(nextDue))]);
  if (rows.length === 0) {
    card.setState({ state: 'empty' });
    return;
  }
  card.setState({ state: 'ready', data: { node: table({ columns: [{ label: 'Status' }, { label: 'Count' }], rows }) } });
}

function renderHealth(card, health) {
  if (!health.ok) {
    card.setState({ state: 'error', message: health.message });
    return;
  }
  const h = health.data || {};
  const rows = [
    [cell('Status'), badge(h.status || 'unknown')],
    [cell('Version'), cell(h.version || '—')],
    [cell('Protocol'), cell(h.protocol_version === undefined ? '—' : String(h.protocol_version))],
    [cell('Database'), badge(h.database && h.database.ok ? 'ok' : 'fail')],
    [cell('Server'), cell((h.database && h.database.server_version) || '—')],
    [
      cell('SMTP'),
      el('span', {}, [
        badge(h.smtp && h.smtp.enabled ? 'enabled' : 'disabled'),
        el('span', { class: 'view-sub', text: h.smtp && h.smtp.connections !== undefined ? ` ${h.smtp.connections} connections` : '' }),
      ]),
    ],
    [
      cell('IMAP'),
      el('span', {}, [
        badge(h.imap && h.imap.enabled ? 'enabled' : 'disabled'),
        el('span', { class: 'view-sub', text: h.imap && h.imap.connections !== undefined ? ` ${h.imap.connections} connections` : '' }),
      ]),
    ],
    [cell('Queue'), cell(formatQueueLine(h.queue))],
  ];
  card.setState({
    state: 'ready',
    data: { node: table({ columns: [{ label: 'Check' }, { label: 'Value' }], rows }) },
  });
}

function formatQueueLine(queue) {
  if (!queue || typeof queue !== 'object') return '—';
  return ['pending', 'delivering', 'retry', 'failed']
    .filter((key) => queue[key] !== undefined)
    .map((key) => `${key}: ${queue[key]}`)
    .join(' · ') || '—';
}

function renderStorage(card, storage) {
  if (!storage.ok) {
    card.setState({ state: 'error', message: storage.message });
    return;
  }
  const s = storage.data || {};
  const rows = [
    [cell('Maildir'), cell(formatBytes(num(pick(s, ['maildir_bytes'], 0))), 'cell-mono')],
    [cell('Attachment blobs'), cell(formatBytes(num(pick(s, ['attachment_bytes'], 0))), 'cell-mono')],
    [cell('Database'), cell(formatBytes(num(pick(s, ['database_bytes'], 0))), 'cell-mono')],
    [cell('Mailboxes'), cell(String(pick(s, ['mailboxes'], '—')))],
    [cell('Messages'), cell(String(pick(s, ['messages'], '—')))],
  ];
  card.setState({
    state: 'ready',
    data: { node: table({ columns: [{ label: 'Store' }, { label: 'Size' }], rows }) },
  });
}

/* ------------------------------------------------------------------ sparkline */

/** Sample the queue depth and redraw the sparkline. */
function renderHistory(card, queueStats) {
  if (!queueStats.ok) {
    card.setState({ state: 'error', message: queueStats.message });
    return;
  }
  const history = sampleQueueDepth(queueStats);
  const latest = history[history.length - 1];
  const node = el('div', {}, [
    sparkline(history),
    el('p', {
      class: 'view-sub',
      text:
        history.length < 2
          ? `First sample: depth ${latest.depth}, failed ${latest.failed}. The line fills in as the console stays open.`
          : `Now: depth ${latest.depth}, failed ${latest.failed} — ${history.length} samples.`,
    }),
  ]);
  card.setState({ state: 'ready', data: { node } });
}

/** Sample the queue depth into the shared history buffer. */
function sampleQueueDepth(queueStats) {
  const data = queueStats.data || {};
  const counts = data.counts || data;
  const depth = num(pick(counts, ['pending'], 0)) + num(pick(counts, ['retry'], 0));
  const failed = num(pick(counts, ['failed'], 0));
  const history = getState().queueHistory.slice();
  history.push({ t: Date.now(), depth, failed });
  while (history.length > MAX_SAMPLES) history.shift();
  setState({ queueHistory: history });
  return history;
}

/**
 * @param {{t: number, depth: number, failed: number}[]} history
 */
export function sparkline(history) {
  const width = 600;
  const height = 64;
  const pad = 6;
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('class', 'spark');
  svg.setAttribute('viewBox', `0 0 ${width} ${height}`);
  svg.setAttribute('preserveAspectRatio', 'none');
  svg.setAttribute('role', 'img');
  svg.setAttribute('aria-label', `Queue depth over the last ${history.length} samples`);

  if (history.length === 0) return svg;
  const max = Math.max(1, ...history.map((point) => point.depth));
  const step = history.length > 1 ? (width - pad * 2) / (history.length - 1) : 0;
  const points = history.map((point, index) => {
    const x = pad + index * step;
    const y = height - pad - (point.depth / max) * (height - pad * 2);
    return `${x.toFixed(1)},${y.toFixed(1)}`;
  });

  const area = document.createElementNS('http://www.w3.org/2000/svg', 'polygon');
  area.setAttribute('class', 'spark-area');
  area.setAttribute('points', `${pad},${height - pad} ${points.join(' ')} ${pad + (history.length - 1) * step},${height - pad}`);
  svg.append(area);

  const line = document.createElementNS('http://www.w3.org/2000/svg', 'polyline');
  line.setAttribute('class', 'spark-line');
  line.setAttribute('points', points.join(' '));
  svg.append(line);
  return svg;
}
