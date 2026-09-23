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
  const list = el('div', { id: 'backup-destinations' });
  const draft = el('input', {
    class: 'input',
    id: 'backup-destination',
    type: 'text',
    spellcheck: 'false',
    placeholder: '/var/lib/ferroma/ferroma.tar',
  });
  const add = el('button', { type: 'button', class: 'btn btn-small', text: t('Save destination') });
  const exportSelected = el('button', { type: 'button', class: 'btn btn-small btn-primary', text: t('Export selected') });
  const command = el('pre', { class: 'code-block', id: 'backup-command' });
  const endpoint = el('input', { class: 'input', id: 's3-endpoint', type: 'url', placeholder: 'https://s3.example.com', autocomplete: 'off' });
  const region = el('input', { class: 'input', id: 's3-region', type: 'text', value: 'us-east-1', autocomplete: 'off' });
  const accessKey = el('input', { class: 'input', id: 's3-access-key', type: 'password', autocomplete: 'off' });
  const secretKey = el('input', { class: 'input', id: 's3-secret-key', type: 'password', autocomplete: 'off' });
  const credentialStatus = el('p', { class: 'view-sub', id: 's3-credential-status' });
  const saveS3 = el('button', { type: 'button', class: 'btn btn-small', text: t('Save S3 settings') });
  let items = [];

  function selected() {
    return [...list.querySelectorAll('input[type="checkbox"]:checked')].map((box) => box.value);
  }

  function paint() {
    list.replaceChildren();
    if (items.length === 0) {
      list.append(el('p', { class: 'view-sub', text: t('No destination saved yet.') }));
    }
    for (const item of items) {
      const box = el('input', { type: 'checkbox', value: item.to });
      box.addEventListener('change', refreshCommand);
      const remove = el('button', { type: 'button', class: 'btn btn-small btn-danger', text: t('Remove') });
      remove.addEventListener('click', async () => {
        items = items.filter((entry) => entry.id !== item.id);
        await save();
      });
      list.append(el('div', { class: 'row' }, [
        el('label', { class: 'checkbox' }, [box, el('span', { class: 'cell-mono', text: item.to })]),
        remove,
      ]));
    }
    refreshCommand();
  }

  function refreshCommand() {
    const chosen = selected();
    const where = chosen[0] || '/var/lib/ferroma/ferroma.tar';
    command.textContent = `ferroma storage import --from ${shellQuote(where)}`;
  }

  async function save() {
    const payload = await request(`${API_BASE}/storage/destinations`, {
      method: 'PUT',
      body: { items },
      toast: false,
    });
    items = Array.isArray(payload && payload.items) ? payload.items : items;
    paint();
  }

  add.addEventListener('click', async () => {
    const to = draft.value.trim();
    if (to === '' || items.some((item) => item.to === to)) return;
    items = [...items, { id: '', to }];
    draft.value = '';
    try {
      await save();
      toastSuccess(t('Destination saved.'));
    } catch (error) {
      setText(resultLine, error instanceof ApiError ? error.message : t('The destination could not be saved.'));
    }
  });

  exportSelected.addEventListener('click', () => exportArchives(exportSelected, selected(), resultLine));

  saveS3.addEventListener('click', async () => {
    saveS3.disabled = true;
    try {
      const settings = await request(`${API_BASE}/storage/transfer-settings`, {
        method: 'PUT',
        body: {
          endpoint: endpoint.value.trim(), region: region.value.trim(),
          access_key: accessKey.value, secret_key: secretKey.value,
        },
        toast: false,
      });
      accessKey.value = '';
      secretKey.value = '';
      setText(credentialStatus, settings.access_key_set && settings.secret_key_set
        ? t('S3 credentials saved on the server. Leave key fields blank to keep them.')
        : t('S3 credentials have not been saved.'));
      toastSuccess(t('S3 settings saved.'));
    } catch (error) {
      toastError(error instanceof ApiError ? error.message : t('S3 settings could not be saved.'));
    } finally {
      saveS3.disabled = false;
    }
  });
  request(`${API_BASE}/storage/transfer-settings`, { toast: false })
    .then((settings) => {
      endpoint.value = String(settings.endpoint || '');
      region.value = String(settings.region || 'us-east-1');
      setText(credentialStatus, settings.access_key_set && settings.secret_key_set
        ? t('S3 credentials saved on the server. Leave key fields blank to keep them.')
        : t('S3 credentials have not been saved.'));
    })
    .catch((error) => setText(credentialStatus, error instanceof ApiError ? error.message : t('S3 settings could not be loaded.')));

  request(`${API_BASE}/storage/destinations`, { toast: false })
    .then((payload) => {
      items = Array.isArray(payload && payload.items) ? payload.items : [];
      paint();
    })
    .catch(() => paint());

  return el('section', { class: 'card' }, [
    el('header', { class: 'card-head' }, [
      el('div', {}, [
        el('h2', { class: 'card-title', text: t('Move to another server') }),
        el('p', {
          class: 'card-sub',
          text: t('One archive: the database and the mail files. A path inside the container, an s3:// URL or a webdav:// URL. Saved destinations stay on this server.'),
        }),
      ]),
    ]),
    el('div', { class: 'card-body' }, [
      list,
      el('label', { class: 'field-label', for: 'backup-destination', text: t('Archive') }),
      el('div', { class: 'row' }, [draft, add]),
      el('div', { class: 'card-actions' }, [exportSelected]),
      el('h3', { text: t('S3-compatible storage') }),
      el('p', { class: 'view-sub', text: t('Enter the HTTPS endpoint of your S3 server. Leave it blank for AWS S3. The bucket name stays in the s3:// destination.') }),
      el('div', { class: 'row' }, [
        el('label', { class: 'field' }, [el('span', { class: 'field-label', text: t('S3 endpoint') }), endpoint]),
        el('label', { class: 'field' }, [el('span', { class: 'field-label', text: t('S3 region') }), region]),
      ]),
      el('div', { class: 'row' }, [
        el('label', { class: 'field' }, [el('span', { class: 'field-label', text: t('Access key') }), accessKey]),
        el('label', { class: 'field' }, [el('span', { class: 'field-label', text: t('Secret key') }), secretKey]),
      ]),
      credentialStatus,
      el('div', { class: 'card-actions' }, [saveS3]),
      el('p', {
        class: 'view-sub',
        text: t('Import refuses while a server is running. On the new host, with its server stopped, run:'),
      }),
      command,
      el('p', {
        class: 'view-sub',
        text: t('S3 keys are stored in a private server file, never in the archive or returned to the browser. WebDAV uses WEBDAV_USERNAME and WEBDAV_PASSWORD from the environment.'),
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
async function exportArchives(button, destinations, resultLine) {
  if (destinations.length === 0) {
    setText(resultLine, t('Select a destination first.'));
    return;
  }
  const confirmed = await confirmDialog({
    title: t('Export an archive?'),
    message: t('The database and the mail files are written to {count} destination(s). The server stays up, so a message delivered during the export may be missing.', { count: destinations.length }),
    confirmLabel: t('Export'),
  });
  if (!confirmed) return;

  button.disabled = true;
  const done = [];
  try {
    for (const to of destinations) {
      setText(resultLine, t('Exporting to {to}…', { to }));
      const payload = await request(`${API_BASE}/storage/export`, {
        method: 'POST',
        body: { to, live: true },
        toast: false,
      });
      done.push(t('Exported {size} to {to}.', {
        size: formatBytes(num(payload && payload.bytes, 0)),
        to: (payload && payload.destination) || to,
      }));
    }
    setText(resultLine, done.join(' '));
    toastSuccess(t('Archive exported.'));
  } catch (error) {
    const message = error instanceof ApiError ? error.message : t('The export failed.');
    setText(resultLine, [...done, message].join(' '));
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
