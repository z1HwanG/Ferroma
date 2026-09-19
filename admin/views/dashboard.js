/**
 * Dashboard: `GET /api/v1/health`, `GET /api/v1/queue/stats`, `GET /api/v1/storage`,
 * plus a hand-drawn SVG sparkline of queue depth sampled in the browser while the
 * section stays open (a maximum of 40 samples, one per poll).
 *
 * The page is organised by the question it answers, not by the endpoint it calls:
 *
 *   1. what needs attention — exceptions only, or an explicit all-clear;
 *   2. the four figures an operator checks first, as a hero row;
 *   3. the supporting figures;
 *   4. queue depth over time, then the raw detail for debugging.
 *
 * Values the API does not report render as “—” rather than as a misleading zero.
 */

import { API_BASE, ApiError, request } from '../api.js';
import { dashboardStats, num } from '../data.js';
import { el, clear } from '../dom.js';
import { formatBytes, formatLogStamp } from '../format.js';
import { getState, setState } from '../store.js';
import { adminCard, attentionList, badge, cell, statTile, table, viewHead } from '../ui.js';

const REFRESH_MS = 30000;
const MAX_SAMPLES = 40;

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
 * @param {URLSearchParams} params
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render(params) {
  const user = getState().user;
  const heroHost = el('div', { class: 'stat-grid stat-grid-hero' });
  const statsHost = el('div', { class: 'stat-grid' });

  const attentionCard = adminCard({
    title: 'Needs attention',
    subtitle: 'problems derived from the figures below; silent when there are none',
    renderData: (node) => node,
  });

  const root = el('div', {}, [
    viewHead('Dashboard', `Signed in as ${(user && user.email) || 'administrator'}`),
    attentionCard.node,
    heroHost,
    statsHost,
  ]);

  const stateCard = adminCard({
    title: 'System health',
    renderData: (data) => data.node,
  });

  const queueCard = adminCard({
    title: 'Mail queue by state',
    renderData: (data) => data.node,
  });

  const storageCard = adminCard({
    title: 'Storage',
    renderData: (data) => data.node,
  });

  const historyCard = adminCard({
    title: 'Queue depth',
    subtitle: 'sampled in this browser, one point every 30 s while the console is open',
    renderData: (data) => data.node,
  });

  root.append(historyCard.node, queueCard.node, stateCard.node, storageCard.node);

  const refresh = async () => {
    const [health, queueStats, storage] = await Promise.all([
      fetchHealth(),
      fetchQueueStats(),
      fetchStorage(),
    ]);

    renderAttention(attentionCard, health, queueStats);
    renderStats(heroHost, statsHost, health, queueStats, storage);
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
 * Derive the exceptions worth an operator's time. Everything here is a finding,
 * not a reading: a number that is merely interesting belongs in the grids below.
 *
 * @param {{ok: boolean, data?: object, message?: string}} health
 * @param {{ok: boolean, data?: object, message?: string}} queueStats
 */
function findings(health, queueStats) {
  /** @type {Array<{tone: 'warn'|'danger', title: string, detail?: string}>} */
  const issues = [];
  const h = health.ok ? health.data || {} : null;

  if (!health.ok) {
    issues.push({ tone: 'danger', title: 'System health could not be read', detail: health.message });
  }
  if (h && h.database && h.database.ok === false) {
    issues.push({ tone: 'danger', title: 'The database is not reachable', detail: 'Mail cannot be stored or read until it recovers.' });
  }
  if (h && h.smtp && h.smtp.enabled === false) {
    issues.push({ tone: 'warn', title: 'SMTP is disabled', detail: 'No inbound mail is being accepted.' });
  }
  if (h && h.imap && h.imap.enabled === false) {
    issues.push({ tone: 'warn', title: 'IMAP is disabled', detail: 'Mail clients cannot connect.' });
  }
  if (h && h.status && String(h.status).toLowerCase() === 'degraded') {
    issues.push({ tone: 'warn', title: 'The server reports itself as degraded' });
  }

  const counts = queueStats.ok ? queueCounts(queueStats) : {};
  const failed = num(pick(counts, ['failed'], 0));
  const retry = num(pick(counts, ['retry'], 0));
  if (failed > 0) {
    issues.push({
      tone: 'danger',
      title: `${failed} delivery attempt(s) have failed permanently`,
      detail: 'Failed entries are kept in the queue; open Mail queue and filter by “failed”.',
    });
  }
  if (retry > 0) {
    issues.push({ tone: 'warn', title: `${retry} message(s) are waiting to be retried`, detail: 'Delivery is retrying with backoff.' });
  }
  if (!queueStats.ok) {
    issues.push({ tone: 'warn', title: 'Queue statistics could not be read', detail: queueStats.message });
  }

  return issues;
}

function renderAttention(card, health, queueStats) {
  card.setState({ state: 'ready', data: attentionList(findings(health, queueStats)) });
}

/** The counters object, whether the API nests it under `counts` or not. */
function queueCounts(queueStats) {
  const data = queueStats.data || {};
  return data.counts || data;
}

/**
 * @param {HTMLElement} heroHost the four headline tiles
 * @param {HTMLElement} statsHost the supporting tiles
 * @param {{ok: boolean, data?: object}} health
 * @param {{ok: boolean, data?: object}} queueStats
 * @param {{ok: boolean, data?: object}} storage
 */
function renderStats(heroHost, statsHost, health, queueStats, storage) {
  clear(heroHost);
  clear(statsHost);
  const h = health.ok ? health.data || {} : {};
  const s = storage.ok ? storage.data || {} : {};
  const database = h.database || {};
  const pool = database.pool || {};
  // Every lookup — including the nested `health.queue` and `health.clients` paths
  // that the grid used to miss — lives in `data.js` so a test can prove it.
  const stats = dashboardStats(health.ok ? health.data : {}, queueStats.ok ? queueStats.data : {}, s);

  const failed = stats.failedDeliveries;

  heroHost.append(
    statTile({
      label: 'Queue pending',
      value: stats.queuePending,
      note: 'waiting to be delivered',
      hero: true,
    }),
    statTile({
      label: 'Failed deliveries',
      value: failed,
      note: num(failed) > 0 ? 'need a decision' : 'none',
      hero: true,
      tone: num(failed) > 0 ? 'danger' : 'ok',
    }),
    statTile({ label: 'Mailboxes', value: stats.users, note: 'accounts', hero: true }),
    statTile({
      label: 'Mailbox storage',
      value: stats.maildirBytes === undefined ? undefined : formatBytes(num(stats.maildirBytes)),
      note: 'Maildir on disk',
      hero: true,
    }),
  );

  statsHost.append(
    statTile({ label: 'Domains', value: stats.domains, note: 'hosted here' }),
    statTile({ label: 'Received today', value: stats.receivedToday }),
    statTile({ label: 'Sent today', value: stats.sentToday }),
    statTile({
      label: 'Queue retry',
      value: stats.queueRetry,
      tone: num(stats.queueRetry) > 0 ? 'warn' : undefined,
    }),
    statTile({
      label: 'Attachments',
      value: stats.attachmentBytes === undefined ? undefined : formatBytes(num(stats.attachmentBytes)),
      note: 'blob store',
    }),
    statTile({
      label: 'Database size',
      value: stats.databaseBytes === undefined ? undefined : formatBytes(num(stats.databaseBytes)),
      note: database.server_version ? String(database.server_version) : '',
    }),
    statTile({
      label: 'Active client sessions',
      value: stats.activeClientSessions,
    }),
    statTile({
      label: 'Uptime',
      value: stats.uptimeSecs === undefined ? undefined : formatUptime(num(stats.uptimeSecs)),
    }),
    statTile({
      label: 'Connection pool',
      value:
        pool.size === undefined && pool.idle === undefined
          ? undefined
          : `${num(pick(pool, ['size'], 0))} / ${num(pick(pool, ['max'], 0))}`,
      note: 'in use of max',
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
  const counts = queueCounts(queueStats);
  const rows = Object.keys(counts)
    .filter((key) => typeof counts[key] === 'number' || typeof counts[key] === 'string')
    .map((key) => [cell(key), cell(String(counts[key]))]);
  const nextDue = queueStats.data ? queueStats.data.next_due_at : null;
  if (rows.length === 0) {
    card.setState({ state: 'empty' });
    return;
  }
  const node = el('div', {}, [
    table({ columns: [{ label: 'State' }, { label: 'Messages' }], rows }),
    nextDue ? el('p', { class: 'view-sub', text: `Next attempt scheduled for ${formatLogStamp(nextDue)}.` }) : null,
  ]);
  card.setState({ state: 'ready', data: node });
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
    data: table({ columns: [{ label: 'Check' }, { label: 'Value' }], rows }),
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
    data: table({ columns: [{ label: 'Store' }, { label: 'Size' }], rows }),
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
  card.setState({ state: 'ready', data: node });
}

/** Sample the queue depth into the shared history buffer. */
function sampleQueueDepth(queueStats) {
  const counts = queueCounts(queueStats);
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
