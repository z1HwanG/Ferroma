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

import { API_BASE, ApiError, request } from '../../shared/api.js';
import { num } from '../../shared/data.js';
import { el, setText } from '../../shared/dom.js';
import { formatBytes } from '../../shared/format.js';
import { t, tn } from '../../shared/i18n.js';
import { confirmDialog } from '../../shared/modal.js';
import { adminCard, table, cell, viewHead } from '../ui.js';
import { toastError, toastSuccess } from '../../shared/toast.js';

/**
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render() {
  const used = el('div', { class: 'stat-grid', id: 'storage-stats' });
  const countsHost = el('div', { class: 'stat-grid', id: 'storage-counts' });
  const resultLine = el('p', { class: 'view-sub', id: 'storage-result' });

  const refreshButton = el('button', { type: 'button', class: 'btn', text: t('Refresh') });
  refreshButton.addEventListener('click', () => refresh());

  const gcButton = el('button', { type: 'button', class: 'btn btn-danger', text: t('Collect garbage') });
  gcButton.addEventListener('click', () => collectGarbage(gcButton));

  const usage = adminCard({
    title: t('Bytes on disk'),
    subtitle: 'GET /api/v1/storage',
    renderData: (data) => data.node,
  });

  const counts = adminCard({
    title: t('What is stored'),
    subtitle: 'GET /api/v1/storage',
    renderData: (data) => data.node,
  });

  const backup = backupCard(resultLine);

  const root = el('div', {}, [
    viewHead(t('Storage'), t('Space used by mail, attachments and the database'), [refreshButton, gcButton]),
    countsHost,
    used,
    resultLine,
    usage.node,
    counts.node,
    backup,
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
      stat({ label: t('Disk used'), value: usedBytes, format: (value) => formatBytes(num(value)) }),
      stat({
        label: t('Disk usage'),
        value: usedPercent,
        note:
          typeof totalBytes === 'number' && typeof freeBytes === 'number'
            ? t('{free} free of {total}', { free: formatBytes(freeBytes), total: formatBytes(totalBytes) })
            : t('this host does not report its filesystem'),
      }),
      stat({ label: t('Maildir'), value: pick(s, 'maildir_bytes'), format: (value) => formatBytes(num(value)) }),
      stat({ label: t('Attachment blobs'), value: pick(s, 'attachment_bytes'), format: (value) => formatBytes(num(value)) }),
      stat({ label: t('Database'), value: pick(s, 'database_bytes'), format: (value) => formatBytes(num(value)) }),
    );

    fill(
      countsHost,
      stat({ label: t('Users'), value: pick(s, 'users') }),
      stat({ label: t('Domains'), value: pick(s, 'domains') }),
      stat({ label: t('Mailboxes'), value: pick(s, 'mailboxes') }),
      stat({ label: t('Messages'), value: pick(s, 'messages') }),
    );

    usage.setState({
      state: 'ready',
      data: {
        node: table({
          columns: [{ label: t('Store') }, { label: t('Size') }],
          rows: [
            [cell(t('Maildir')), cell(formatBytes(num(pick(s, 'maildir_bytes'))), 'cell-mono')],
            [cell(t('Attachment blobs')), cell(formatBytes(num(pick(s, 'attachment_bytes'))), 'cell-mono')],
            [cell(t('Database')), cell(formatBytes(num(pick(s, 'database_bytes'))), 'cell-mono')],
            [
              cell(t('Free on disk')),
              cell(
                typeof freeBytes === 'number'
                  ? t('{free} of {total}', { free: formatBytes(freeBytes), total: formatBytes(num(totalBytes)) })
                  : t('not reported by this host'),
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
          columns: [{ label: t('Entity') }, { label: t('Count') }],
          rows: [
            [cell(t('Users')), cell(String(pick(s, 'users') ?? '—'), 'cell-mono')],
            [cell(t('Domains')), cell(String(pick(s, 'domains') ?? '—'), 'cell-mono')],
            [cell(t('Mailboxes')), cell(String(pick(s, 'mailboxes') ?? '—'), 'cell-mono')],
            [cell(t('Messages')), cell(String(pick(s, 'messages') ?? '—'), 'cell-mono')],
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
      const message = error instanceof ApiError ? error.message : t('The storage figures could not be loaded.');
      usage.setState({ state: 'error', message });
      counts.setState({ state: 'error', message });
    }
  }

  /** Run the maintenance pass and report what it freed. */
  async function collectGarbage(button) {
    const confirmed = await confirmDialog({
      title: t('Collect garbage?'),
      message: t('Attachment blobs and Maildir scratch files that no message references any more are deleted. Referenced messages are never touched.'),
      confirmLabel: t('Collect'),
    });
    if (!confirmed) return;

    button.disabled = true;
    setText(resultLine, t('Collecting…'));
    try {
      const payload = await request(`${API_BASE}/storage/gc`, { method: 'POST', toast: false });
      const freed = num(payload && payload.freed_bytes, 0);
      const attachments = num(payload && payload.removed_attachments, 0);
      const scratch = num(payload && payload.removed_attachment_scratch, 0) + num(payload && payload.removed_maildir_scratch, 0);
      setText(
        resultLine,
        t('Freed {size}: {attachments} and {scratch} removed in {ms} ms.', {
          size: formatBytes(freed),
          attachments: tn(attachments, '{count} orphaned attachment', '{count} orphaned attachments', {
            count: attachments,
          }),
          scratch: tn(scratch, '{count} scratch file', '{count} scratch files', { count: scratch }),
          ms: num(payload && payload.duration_ms, 0),
        }),
      );
      toastSuccess(t('Garbage collected.'));
      await refresh();
    } catch (error) {
      const message = error instanceof ApiError ? error.message : t('Garbage collection failed.');
      setText(resultLine, message);
      toastError(message);
    } finally {
      button.disabled = false;
    }
  }

  await refresh();
  return { node: root, cleanup() {} };
}

/**
 * The move-to-another-server card.
 *
 * Export runs the same command the operator would, inside this container, while the
 * server stays up (`--live`): stopping it from the page that is talking to it is not
 * a thing the page can do. Import is the line printed underneath. It refuses while a
 * server is listening, because a restore writes both halves underneath that process,
 * so it is run on the new host after that host's server is stopped.
 *
 * @param {HTMLElement} resultLine
 */
function backupCard(resultLine) {
  const destination = el('input', {
    class: 'input',
    id: 'backup-destination',
    type: 'text',
    spellcheck: 'false',
    placeholder: '/var/lib/ferroma/ferroma.tar',
  });
  const command = el('pre', { class: 'code-block', id: 'backup-command' });
  const button = el('button', { type: 'button', class: 'btn', text: t('Export archive') });

  const target = () => destination.value.trim();
  const refreshCommand = () => {
    const where = target() || '/var/lib/ferroma/ferroma.tar';
    command.textContent = `ferroma storage import --from ${shellQuote(where)}`;
  };
  destination.addEventListener('input', refreshCommand);
  refreshCommand();

  button.addEventListener('click', () => exportArchive(button, destination, resultLine));

  return el('section', { class: 'card' }, [
    el('header', { class: 'card-head' }, [
      el('div', {}, [
        el('h2', { class: 'card-title', text: t('Move to another server') }),
        el('p', {
          class: 'card-sub',
          text: t('One archive: the database and the mail files. A path inside the container, an s3:// URL or a webdav:// URL.'),
        }),
      ]),
    ]),
    el('div', { class: 'card-body' }, [
      el('label', { class: 'field-label', for: 'backup-destination', text: t('Archive') }),
      destination,
      el('div', { class: 'card-actions' }, [button]),
      el('p', {
        class: 'view-sub',
        text: t('Import refuses while a server is running. On the new host, with its server stopped, run:'),
      }),
      command,
      el('p', {
        class: 'view-sub',
        text: t('S3 reads AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY from the environment. WebDAV reads WEBDAV_USERNAME and WEBDAV_PASSWORD. Neither is stored here.'),
      }),
    ]),
  ]);
}

/**
 * Ask the server to write the archive.
 *
 * @param {HTMLButtonElement} button
 * @param {HTMLInputElement} destination
 * @param {HTMLElement} resultLine
 */
async function exportArchive(button, destination, resultLine) {
  const to = destination.value.trim();
  if (to === '') {
    setText(resultLine, t('Say where the archive should go.'));
    destination.focus();
    return;
  }
  const confirmed = await confirmDialog({
    title: t('Export an archive?'),
    message: t('The database and the mail files are written to {to}. The server stays up, so a message delivered during the export may be missing.', { to }),
    confirmLabel: t('Export'),
  });
  if (!confirmed) return;

  button.disabled = true;
  setText(resultLine, t('Exporting…'));
  try {
    const payload = await request(`${API_BASE}/storage/export`, {
      method: 'POST',
      body: { to, live: true },
      toast: false,
    });
    const bytes = num(payload && payload.bytes, 0);
    setText(
      resultLine,
      t('Exported {size} to {to}.', { size: formatBytes(bytes), to: (payload && payload.destination) || to }),
    );
    toastSuccess(t('Archive exported.'));
  } catch (error) {
    const message = error instanceof ApiError ? error.message : t('The export failed.');
    setText(resultLine, message);
    toastError(message);
  } finally {
    button.disabled = false;
  }
}

/** A destination safe to paste into a shell. */
function shellQuote(value) {
  return `'${String(value).replace(/'/g, `'\\''`)}'`;
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
    el('p', { class: 'stat-note', text: missing ? options.note || t('not reported by this API version') : options.note || '' }),
  ]);
}
