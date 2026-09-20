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

import { API_BASE, ApiError, request } from '../../shared/api.js';
import { dashboardStats, num } from '../../shared/data.js';
import { el, clear } from '../../shared/dom.js';
import { formatBytes, formatLogStamp } from '../../shared/format.js';
import { t, tn } from '../../shared/i18n.js';
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
    title: t('Needs attention'),
    subtitle: t('Conditions derived from the figures below; empty when none apply'),
    renderData: (node) => node,
  });

  const root = el('div', {}, [
    viewHead(
      t('Dashboard'),
      t('Signed in as {email}', { email: (user && user.email) || t('administrator') }),
    ),
    attentionCard.node,
    heroHost,
    statsHost,
  ]);

  const stateCard = adminCard({
    title: t('System health'),
    // The five cards below hand `adminCard` a finished node, as `attentionCard`
    // already did. Declaring `data.node` here destructured a property no call site
    // sets, so these four cards rendered the string "undefined" and nothing else.
    renderData: (node) => node,
  });

  const queueCard = adminCard({
    title: t('Mail queue by state'),
    // The five cards below hand `adminCard` a finished node, as `attentionCard`
    // already did. Declaring `data.node` here destructured a property no call site
    // sets, so these four cards rendered the string "undefined" and nothing else.
    renderData: (node) => node,
  });

  const storageCard = adminCard({
    title: t('Storage'),
    // The five cards below hand `adminCard` a finished node, as `attentionCard`
    // already did. Declaring `data.node` here destructured a property no call site
    // sets, so these four cards rendered the string "undefined" and nothing else.
    renderData: (node) => node,
  });

  const historyCard = adminCard({
    title: t('Queue depth'),
    subtitle: t('Sampled by this console, one point every 30 seconds while it is open'),
    // The five cards below hand `adminCard` a finished node, as `attentionCard`
    // already did. Declaring `data.node` here destructured a property no call site
    // sets, so these four cards rendered the string "undefined" and nothing else.
    renderData: (node) => node,
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
    // A degraded server answers `503` with the health document itself, not the error
    // envelope. Reading only the status threw away the one thing worth showing — which
    // subsystem is down — and left two of this view's findings unreachable.
    if (error instanceof ApiError && error.payload && typeof error.payload === 'object') {
      setState({ health: error.payload });
      return { ok: true, data: error.payload };
    }
    if (error instanceof ApiError) return { ok: false, message: error.message, network: error.network };
    return { ok: false, message: t('Health could not be read.') };
  }
}

async function fetchQueueStats() {
  try {
    return { ok: true, data: await request(`${API_BASE}/queue/stats`, { toast: false }) };
  } catch (error) {
    if (error instanceof ApiError) return { ok: false, message: error.message, network: error.network };
    return { ok: false, message: t('Queue statistics could not be read.') };
  }
}

async function fetchStorage() {
  try {
    return { ok: true, data: await request(`${API_BASE}/storage`, { toast: false }) };
  } catch (error) {
    if (error instanceof ApiError) return { ok: false, message: error.message, network: error.network };
    return { ok: false, message: t('Storage figures could not be read.') };
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
    issues.push({ tone: 'danger', title: t('System health could not be read'), detail: health.message });
  }
  if (h && h.database && h.database.ok === false) {
    issues.push({ tone: 'danger', title: t('The database is not reachable'), detail: t('Mail cannot be stored or read until it recovers.') });
  }
  if (h && h.smtp && h.smtp.enabled === false) {
    issues.push({ tone: 'warn', title: t('SMTP is disabled'), detail: t('No inbound mail is being accepted.') });
  }
  if (h && h.imap && h.imap.enabled === false) {
    issues.push({ tone: 'warn', title: t('IMAP is disabled'), detail: t('Mail clients cannot connect.') });
  }
  if (h && h.status && String(h.status).toLowerCase() === 'degraded') {
    issues.push({ tone: 'warn', title: t('The server reports itself as degraded') });
  }

  const counts = queueStats.ok ? queueCounts(queueStats) : {};
  const failed = num(pick(counts, ['failed'], 0));
  const retry = num(pick(counts, ['retry'], 0));
  if (failed > 0) {
    issues.push({
      tone: 'danger',
      title: tn(failed, '{count} delivery attempt has failed permanently', '{count} delivery attempts have failed permanently', { count: failed }),
      detail: t('Failed entries are kept in the queue; open Mail queue and filter by “failed”.'),
    });
  }
  if (retry > 0) {
    issues.push({ tone: 'warn', title: tn(retry, '{count} message is waiting to be retried', '{count} messages are waiting to be retried', { count: retry }), detail: t('Delivery is retrying with backoff.') });
  }
  if (!queueStats.ok) {
    issues.push({ tone: 'warn', title: t('Queue statistics could not be read'), detail: queueStats.message });
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
      label: t('Queue pending'),
      value: stats.queuePending,
      note: t('waiting to be delivered'),
      hero: true,
      icon: 'inbox',
    }),
    statTile({
      label: t('Failed deliveries'),
      value: failed,
      note: num(failed) > 0 ? t('need a decision') : t('none'),
      hero: true,
      tone: num(failed) > 0 ? 'danger' : 'ok',
      icon: 'warning',
    }),
    statTile({ label: t('Mailboxes'), value: stats.users, note: t('accounts'), hero: true, icon: 'users' }),
    statTile({
      label: t('Mailbox storage'),
      value: stats.maildirBytes === undefined ? undefined : formatBytes(num(stats.maildirBytes)),
      note: t('Maildir on disk'),
      hero: true,
      icon: 'storage',
    }),
  );

  statsHost.append(
    statTile({ label: t('Domains'), value: stats.domains, note: t('hosted here'), icon: 'domains' }),
    statTile({ label: t('Received today'), value: stats.receivedToday, icon: 'download' }),
    statTile({ label: t('Sent today'), value: stats.sentToday, icon: 'sent' }),
    statTile({
      label: t('Queue retry'),
      value: stats.queueRetry,
      tone: num(stats.queueRetry) > 0 ? 'warn' : undefined,
      icon: 'refresh',
    }),
    statTile({
      label: t('Attachments'),
      value: stats.attachmentBytes === undefined ? undefined : formatBytes(num(stats.attachmentBytes)),
      note: t('blob store'),
      icon: 'clip',
    }),
    statTile({
      label: t('Database size'),
      value: stats.databaseBytes === undefined ? undefined : formatBytes(num(stats.databaseBytes)),
      note: database.server_version ? String(database.server_version) : '',
      icon: 'database',
    }),
    statTile({
      label: t('Active client sessions'),
      value: stats.activeClientSessions,
      icon: 'devices',
    }),
    statTile({
      label: t('Uptime'),
      value: stats.uptimeSecs === undefined ? undefined : formatUptime(num(stats.uptimeSecs)),
      icon: 'clock',
    }),
    statTile({
      label: t('Connection pool'),
      value:
        pool.size === undefined && pool.idle === undefined
          ? undefined
          : `${num(pick(pool, ['size'], 0))} / ${num(pick(pool, ['max'], 0))}`,
      note: t('in use of max'),
      icon: 'activity',
    }),
  );
}

function formatUptime(seconds) {
  if (!Number.isFinite(seconds) || seconds <= 0) return '—';
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  if (days > 0) return t('{days} d {hours} h', { days, hours });
  if (hours > 0) return t('{hours} h {minutes} min', { hours, minutes });
  return t('{minutes} min', { minutes });
}

function renderQueue(card, queueStats) {
  if (!queueStats.ok) {
    card.setState({ state: 'error', message: queueStats.message });
    return;
  }
  const counts = queueCounts(queueStats);
  const rows = Object.keys(counts)
    .filter((key) => typeof counts[key] === 'number' || typeof counts[key] === 'string')
    .map((key) => [cell(stateLabel(key)), cell(String(counts[key]))]);
  const nextDue = queueStats.data ? queueStats.data.next_due_at : null;
  if (rows.length === 0) {
    card.setState({ state: 'empty' });
    return;
  }
  const node = el('div', {}, [
    table({ columns: [{ label: t('State') }, { label: t('Messages') }], rows }),
    nextDue ? el('p', { class: 'view-sub', text: t('Next attempt scheduled for {when}.', { when: formatLogStamp(nextDue) }) }) : null,
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
    [cell(t('Status')), badge(h.status || 'unknown')],
    [cell(t('Version')), cell(h.version || '—')],
    [cell(t('Protocol')), cell(h.protocol_version === undefined ? '—' : String(h.protocol_version))],
    [cell(t('Database')), badge(h.database && h.database.ok ? 'ok' : 'fail')],
    [cell(t('Server')), cell((h.database && h.database.server_version) || '—')],
    [
      cell(t('SMTP')),
      el('span', {}, [
        badge(h.smtp && h.smtp.enabled ? 'enabled' : 'disabled'),
        el('span', { class: 'view-sub', text: h.smtp && h.smtp.connections !== undefined ? ` ${tn(h.smtp.connections, '{count} connection', '{count} connections', { count: h.smtp.connections })}` : '' }),
      ]),
    ],
    [
      cell(t('IMAP')),
      el('span', {}, [
        badge(h.imap && h.imap.enabled ? 'enabled' : 'disabled'),
        el('span', { class: 'view-sub', text: h.imap && h.imap.connections !== undefined ? ` ${tn(h.imap.connections, '{count} connection', '{count} connections', { count: h.imap.connections })}` : '' }),
      ]),
    ],
    [cell(t('Queue')), cell(formatQueueLine(h.queue))],
  ];
  card.setState({
    state: 'ready',
    data: table({ columns: [{ label: t('Check') }, { label: t('Value') }], rows }),
  });
}

/**
 * The label for one row of the by-state table.
 *
 * The keys come from `/queue/stats`, so they are data — but the table shows them as
 * labels, and a Chinese console must not print `pending`. A key this build does not
 * know keeps its own name: `outstanding` is a derived total rather than a state, and
 * calling it "Unknown" would be a lie about the data.
 *
 * @param {string} state
 */
function stateLabel(state) {
  switch (String(state).toLowerCase()) {
    case 'pending':
      return t('Pending');
    case 'delivering':
      return t('Delivering');
    case 'delivered':
      return t('Delivered');
    case 'retry':
      return t('Retry');
    case 'failed':
      return t('Failed');
    case 'cancelled':
      return t('Cancelled');
    case 'outstanding':
      return t('Outstanding');
    default:
      return String(state);
  }
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
    [cell(t('Maildir')), cell(formatBytes(num(pick(s, ['maildir_bytes'], 0))), 'cell-mono')],
    [cell(t('Attachment blobs')), cell(formatBytes(num(pick(s, ['attachment_bytes'], 0))), 'cell-mono')],
    [cell(t('Database')), cell(formatBytes(num(pick(s, ['database_bytes'], 0))), 'cell-mono')],
    [cell(t('Mailboxes')), cell(String(pick(s, ['mailboxes'], '—')))],
    [cell(t('Messages')), cell(String(pick(s, ['messages'], '—')))],
  ];
  card.setState({
    state: 'ready',
    data: table({ columns: [{ label: t('Store') }, { label: t('Size') }], rows }),
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
          ? t('First sample: depth {depth}, failed {failed}. The line fills in as the console stays open.', { depth: latest.depth, failed: latest.failed })
          : t('Now: depth {depth}, failed {failed} — {count} samples.', { depth: latest.depth, failed: latest.failed, count: history.length }),
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
  svg.setAttribute('aria-label', t('Queue depth over the last {count} samples', { count: history.length }));

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
