/**
 * Storage: `GET /api/v1/storage` and the maintenance action `POST /api/v1/storage/gc`.
 *
 * The figures come from one call: bytes on disk (Maildir, attachment blobs, the
 * database) plus the row counts that explain them. `disk_total_bytes` and
 * `disk_free_bytes` are what turn "how much is stored" into "how much is left",
 * which is the question an operator actually has.
 *
 * Garbage collection removes attachment blobs and Maildir scratch files that no
 * message references any more. It is the one destructive action here, so it asks
 * first and then reports exactly what it freed.
 */

import { API_BASE, ApiError, request } from '../api.js';
import { num } from '../data.js';
import { el, setText } from '../dom.js';
import { formatBytes } from '../format.js';
import { confirmDialog } from '../modal.js';
import { adminCard, table, cell, viewHead } from '../ui.js';
import { toastError, toastSuccess } from '../toast.js';

/**
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render() {
  const used = el('div', { class: 'stat-grid', id: 'storage-stats' });
  const countsHost = el('div', { class: 'stat-grid', id: 'storage-counts' });
  const resultLine = el('p', { class: 'view-sub', id: 'storage-result' });

  const refreshButton = el('button', { type: 'button', class: 'btn', text: 'Refresh' });
  refreshButton.addEventListener('click', () => refresh());

  const gcButton = el('button', { type: 'button', class: 'btn btn-danger', text: 'Collect garbage' });
  gcButton.addEventListener('click', () => collectGarbage(gcButton));

  const usage = adminCard({
    title: 'Bytes on disk',
    subtitle: 'GET /api/v1/storage',
    renderData: (data) => data.node,
  });

  const counts = adminCard({
    title: 'What is stored',
    subtitle: 'GET /api/v1/storage',
    renderData: (data) => data.node,
  });

  const root = el('div', {}, [
    viewHead('Storage', 'What this server is holding, and how much room is left', [refreshButton, gcButton]),
    countsHost,
    used,
    resultLine,
    usage.node,
    counts.node,
  ]);

  /** Pull one figure out of a payload, tolerating an absent key. */
  function pick(source, key) {
    const value = source ? source[key] : undefined;
    return value === undefined || value === null ? undefined : value;
  }

  function renderStats(payload) {
    const s = payload || {};
    const totalBytes = pick(s, 'disk_total_bytes');
    const freeBytes = pick(s, 'disk_free_bytes');
    const usedBytes =
      typeof totalBytes === 'number' && typeof freeBytes === 'number' ? Math.max(0, totalBytes - freeBytes) : undefined;
    const usedPercent =
      typeof usedBytes === 'number' && typeof totalBytes === 'number' && totalBytes > 0
        ? `${((usedBytes / totalBytes) * 100).toFixed(1)} %`
        : undefined;

    fill(
      used,
      stat({ label: 'Disk used', value: usedBytes, format: (value) => formatBytes(num(value)) }),
      stat({
        label: 'Disk usage',
        value: usedPercent,
        note:
          typeof totalBytes === 'number' && typeof freeBytes === 'number'
            ? `${formatBytes(freeBytes)} free of ${formatBytes(totalBytes)}`
            : 'this host does not report its filesystem',
      }),
      stat({ label: 'Maildir', value: pick(s, 'maildir_bytes'), format: (value) => formatBytes(num(value)) }),
      stat({ label: 'Attachment blobs', value: pick(s, 'attachment_bytes'), format: (value) => formatBytes(num(value)) }),
      stat({ label: 'Database', value: pick(s, 'database_bytes'), format: (value) => formatBytes(num(value)) }),
    );

    fill(
      countsHost,
      stat({ label: 'Users', value: pick(s, 'users') }),
      stat({ label: 'Domains', value: pick(s, 'domains') }),
      stat({ label: 'Mailboxes', value: pick(s, 'mailboxes') }),
      stat({ label: 'Messages', value: pick(s, 'messages') }),
    );

    usage.setState({
      state: 'ready',
      data: {
        node: table({
          columns: [{ label: 'Store' }, { label: 'Size' }],
          rows: [
            [cell('Maildir'), cell(formatBytes(num(pick(s, 'maildir_bytes'))), 'cell-mono')],
            [cell('Attachment blobs'), cell(formatBytes(num(pick(s, 'attachment_bytes'))), 'cell-mono')],
            [cell('Database'), cell(formatBytes(num(pick(s, 'database_bytes'))), 'cell-mono')],
            [
              cell('Free on disk'),
              cell(
                typeof freeBytes === 'number'
                  ? `${formatBytes(freeBytes)} of ${formatBytes(num(totalBytes))}`
                  : 'not reported by this host',
                'cell-mono',
              ),
            ],
          ],
        }),
      },
    });

    counts.setState({
      state: 'ready',
      data: {
        node: table({
          columns: [{ label: 'Entity' }, { label: 'Count' }],
          rows: [
            [cell('Users'), cell(String(pick(s, 'users') ?? '—'), 'cell-mono')],
            [cell('Domains'), cell(String(pick(s, 'domains') ?? '—'), 'cell-mono')],
            [cell('Mailboxes'), cell(String(pick(s, 'mailboxes') ?? '—'), 'cell-mono')],
            [cell('Messages'), cell(String(pick(s, 'messages') ?? '—'), 'cell-mono')],
          ],
        }),
      },
    });
  }

  async function refresh() {
    usage.setState({ state: 'loading' });
    counts.setState({ state: 'loading' });
    try {
      renderStats(await request(`${API_BASE}/storage`, { toast: false }));
    } catch (error) {
      const message = error instanceof ApiError ? error.message : 'The storage figures could not be loaded.';
      usage.setState({ state: 'error', message });
      counts.setState({ state: 'error', message });
    }
  }

  /** Run the maintenance pass and report what it freed. */
  async function collectGarbage(button) {
    const confirmed = await confirmDialog({
      title: 'Collect garbage?',
      message:
        'Attachment blobs and Maildir scratch files that no message references any more are deleted. Referenced messages are never touched.',
      confirmLabel: 'Collect',
    });
    if (!confirmed) return;

    button.disabled = true;
    setText(resultLine, 'Collecting…');
    try {
      const payload = await request(`${API_BASE}/storage/gc`, { method: 'POST', toast: false });
      const freed = num(payload && payload.freed_bytes, 0);
      const attachments = num(payload && payload.removed_attachments, 0);
      const scratch = num(payload && payload.removed_attachment_scratch, 0) + num(payload && payload.removed_maildir_scratch, 0);
      setText(
        resultLine,
        `Freed ${formatBytes(freed)}: ${attachments} orphaned attachment(s) and ${scratch} scratch file(s) removed in ${num(payload && payload.duration_ms, 0)} ms.`,
      );
      toastSuccess('Garbage collected.');
      await refresh();
    } catch (error) {
      const message = error instanceof ApiError ? error.message : 'Garbage collection failed.';
      setText(resultLine, message);
      toastError(message);
    } finally {
      button.disabled = false;
    }
  }

  await refresh();
  return { node: root, cleanup() {} };
}

/** Replace a host's children. */
function fill(host, ...nodes) {
  host.replaceChildren(...nodes);
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
