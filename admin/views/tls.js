/**
 * TLS: `GET /api/v1/tls`.
 *
 * "Is TLS on?" is a question about files on the host and ports on the listener, not
 * about a database row, so this view reports the configured PEM files the way the
 * server's own user sees them. `readable: false` on a key that is present is the
 * failure this screen exists for: the configuration looks complete and every
 * handshake fails.
 *
 * The certificate's fingerprint is shown because a certificate is public data — that
 * is the value to compare against `openssl x509 -fingerprint -sha256 -noout` on the
 * host. The private key is never fingerprinted, and neither file's validity window is
 * parsed; see `docs/api.md` §4.9.
 */

import { API_BASE, ApiError, request } from '../../shared/api.js';
import { num } from '../../shared/data.js';
import { el } from '../../shared/dom.js';
import { formatBytes, formatLogStamp } from '../../shared/format.js';
import { t } from '../../shared/i18n.js';
import { actions, adminCard, badge, cell, copyToClipboard, table, viewHead } from '../ui.js';
import { toastSuccess } from '../../shared/toast.js';

/**
 * A person-readable label for an on/off state.
 *
 * The state word is a value this view derives, not one the API sends, but it is still
 * data where the badge's colour is decided — so it is translated only at the point of
 * display, and each label is a literal inside `t()` so the catalog stays checkable.
 *
 * @param {unknown} state either `enabled` or `disabled`
 * @returns {string}
 */
function onOffLabel(state) {
  switch (String(state || '').toLowerCase()) {
    case 'enabled':
      return t('Enabled');
    case 'disabled':
      return t('Disabled');
    default:
      return t('Unknown');
  }
}

/**
 * The on/off pill, showing the translated state.
 *
 * The raw value still decides the pill's colour; only the text is replaced.
 *
 * @param {unknown} state
 * @returns {Element}
 */
function onOffBadge(state) {
  const node = badge(state);
  node.textContent = onOffLabel(state);
  return node;
}

/**
 * The presence pill, translated. The raw `yes` / `no` still decides the colour.
 *
 * @param {unknown} present
 * @returns {Element}
 */
function yesNoBadge(present) {
  const node = badge(present ? 'yes' : 'no');
  node.textContent = present ? t('Yes') : t('No');
  return node;
}

/**
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render() {
  const summary = el('div', { class: 'stat-grid', id: 'tls-summary' });
  const hint = el('p', { class: 'view-sub', id: 'tls-hint' });

  const refreshButton = el('button', { type: 'button', class: 'btn', text: t('Refresh') });
  refreshButton.addEventListener('click', () => refresh());

  const configCard = adminCard({
    title: t('Configuration'),
    subtitle: t('[tls] in the server configuration'),
    renderData: (data) => data.node,
  });

  const certificateCard = adminCard({
    title: t('Certificate'),
    subtitle: t('the PEM bundle: leaf certificate followed by intermediates'),
    renderData: (data) => data.node,
  });

  const keyCard = adminCard({
    title: t('Private key'),
    subtitle: t('never read, only stat-ed'),
    renderData: (data) => data.node,
  });

  const listenersCard = adminCard({
    title: t('Where TLS is offered'),
    subtitle: t('implicit-TLS ports and the published URL'),
    renderData: (data) => data.node,
  });

  const root = el('div', {}, [
    viewHead(
      t('TLS certificates'),
      t('The certificates this server loads, and the ports that use them'),
      [refreshButton],
    ),
    summary,
    hint,
    configCard.node,
    certificateCard.node,
    keyCard.node,
    listenersCard.node,
  ]);

  function renderSummary(status) {
    const listeners = status.listeners || {};
    summary.replaceChildren(
      stat({ label: t('TLS enabled'), value: typeof status.enabled === 'boolean' ? (status.enabled ? t('Enabled') : t('Disabled')) : undefined }),
      stat({ label: t('Minimum version'), value: status.min_version }),
      stat({
        label: t('Certificate readable'),
        value: status.certificate ? (status.certificate.readable ? t('Readable') : t('Unreadable')) : undefined,
      }),
      stat({
        label: t('Private key readable'),
        value: status.private_key ? (status.private_key.readable ? t('Readable') : t('Unreadable')) : undefined,
      }),
      stat({ label: t('SMTPS port'), value: listeners.smtps_port ? String(listeners.smtps_port) : t('Off'), note: t('implicit TLS') }),
      stat({ label: t('IMAPS port'), value: listeners.imaps_port ? String(listeners.imaps_port) : t('Off'), note: t('implicit TLS') }),
      stat({
        label: t('HTTPS port'),
        value: listeners.https_port ? String(listeners.https_port) : t('Off'),
        note: t('off means the reverse proxy terminates TLS'),
      }),
    );

    const problems = [];
    if (status.enabled === false) problems.push(t('TLS is disabled: STARTTLS, SMTPS, IMAPS and HTTPS are all unavailable.'));
    if (status.self_signed_fallback) problems.push(t('A self-signed certificate is generated when none is configured — development only.'));
    if (status.certificate && status.certificate.present && !status.certificate.readable) problems.push(t('The certificate file cannot be read by this process.'));
    if (status.private_key && status.private_key.present && !status.private_key.readable) problems.push(t('The private key cannot be read by this process; every handshake will fail.'));
    if (!status.certificate || !status.certificate.present) problems.push(t('No certificate is configured (or the path does not exist).'));
    hint.textContent = problems.length
      ? problems.join(' ')
      : t('The certificate and key are both present and readable by this process.');
  }

  function renderConfiguration(status, certificate, key) {
    configCard.setState({
      state: 'ready',
      data: {
        node: table({
          columns: [{ label: t('Setting') }, { label: t('Value') }],
          rows: [
            [cell(t('Enabled')), onOffBadge(status.enabled ? 'enabled' : 'disabled')],
            [cell(t('Minimum version')), cell(status.min_version || '—', 'cell-mono')],
            [cell(t('Self-signed fallback')), onOffBadge(status.self_signed_fallback ? 'enabled' : 'disabled')],
            [cell(t('Platform roots')), onOffBadge(status.use_platform_roots ? 'enabled' : 'disabled')],
            [cell(t('Allow insecure dev mode')), onOffBadge(status.allow_insecure_dev_mode ? 'enabled' : 'disabled')],
            [cell(t('Certificate path')), cell(certificate.path || t('not configured'), 'cell-mono')],
            [cell(t('Key path')), cell(key.path || t('not configured'), 'cell-mono')],
          ],
        }),
      },
    });
  }

  function renderFile(card, file, options) {
    const rows = [
      [cell(t('Path')), cell(file.path || t('not configured'), 'cell-mono')],
      [cell(t('Present')), yesNoBadge(file.present)],
      [cell(t('Readable')), yesNoBadge(file.readable)],
      [cell(t('Size')), cell(file.size_bytes === null || file.size_bytes === undefined ? '—' : formatBytes(num(file.size_bytes)), 'cell-mono')],
      [cell(t('Modified')), cell(file.modified_at ? formatLogStamp(file.modified_at) : '—')],
    ];

    const children = [
      table({ columns: [{ label: t('Check') }, { label: t('Value') }], rows }),
    ];

    if (file.error) {
      children.push(el('p', { class: 'view-sub', id: options.errorId, text: file.error }));
    }

    if (options.fingerprint) {
      const valueNode = el('p', { class: 'code-block', id: options.fingerprintId, text: file.sha256 || '—' });
      const copy = el('button', { type: 'button', class: 'btn btn-small', text: t('Copy fingerprint') });
      copy.disabled = !file.sha256;
      copy.addEventListener('click', async () => {
        const copied = await copyToClipboard(file.sha256 || '', () => selectNode(valueNode));
        toastSuccess(copied ? t('Fingerprint copied.') : t('The fingerprint is selected — press Ctrl/Cmd+C to copy it.'));
      });
      children.push(
        el('h3', { class: 'card-title', text: t('SHA-256 fingerprint') }),
        valueNode,
        actions(copy),
        el('p', {
          class: 'view-sub',
          text: t('Compare this with `openssl x509 -fingerprint -sha256 -noout -in <path>` on the host. Expiry is not parsed by this build.'),
        }),
      );
    } else {
      children.push(
        el('p', {
          class: 'view-sub',
          text: t('The key is never read or fingerprinted: presence, size, mtime and readability are enough to catch a permissions mistake.'),
        }),
      );
    }

    card.setState({ state: 'ready', data: { node: el('div', {}, children) } });
  }

  function renderListeners(listeners) {
    listenersCard.setState({
      state: 'ready',
      data: {
        node: table({
          columns: [{ label: t('Endpoint') }, { label: t('TLS') }, { label: t('Address') }],
          rows: [
            [cell(t('SMTP (SMTPS)')), onOffBadge(listeners.smtps_port ? 'enabled' : 'disabled'), cell(listeners.smtps_port ? String(listeners.smtps_port) : t('Off'), 'cell-mono')],
            [cell(t('IMAP (IMAPS)')), onOffBadge(listeners.imaps_port ? 'enabled' : 'disabled'), cell(listeners.imaps_port ? String(listeners.imaps_port) : t('Off'), 'cell-mono')],
            [cell(t('HTTP API')), onOffBadge(listeners.https_port ? 'enabled' : 'disabled'), cell(listeners.https_port ? String(listeners.https_port) : t('Off'), 'cell-mono')],
            [
              cell(t('Public URL')),
              onOffBadge(listeners.public_url_is_tls ? 'enabled' : 'disabled'),
              cell(listeners.public_url || '—', 'cell-mono'),
            ],
          ],
        }),
      },
    });
  }

  async function refresh() {
    summary.replaceChildren();
    configCard.setState({ state: 'loading' });
    certificateCard.setState({ state: 'loading' });
    keyCard.setState({ state: 'loading' });
    listenersCard.setState({ state: 'loading' });
    hint.textContent = '';
    try {
      const status = (await request(`${API_BASE}/tls`, { toast: false })) || {};
      const certificate = status.certificate || {};
      const key = status.private_key || {};
      renderSummary(status);
      renderConfiguration(status, certificate, key);
      renderFile(certificateCard, certificate, {
        fingerprint: true,
        fingerprintId: 'tls-cert-fingerprint',
        errorId: 'tls-cert-error',
      });
      renderFile(keyCard, key, { fingerprint: false, errorId: 'tls-key-error' });
      renderListeners(status.listeners || {});
    } catch (error) {
      const message = error instanceof ApiError ? error.message : t('The TLS status could not be loaded.');
      configCard.setState({ state: 'error', message });
      certificateCard.setState({ state: 'error', message });
      keyCard.setState({ state: 'error', message });
      listenersCard.setState({ state: 'error', message });
    }
  }

  await refresh();
  return { node: root, cleanup() {} };
}

/** Select an element's text so the keyboard copy shortcut has something to take. */
function selectNode(node) {
  const range = document.createRange();
  range.selectNodeContents(node);
  const selection = window.getSelection();
  if (selection) {
    selection.removeAllRanges();
    selection.addRange(range);
  }
}

/**
 * Render a measurement, or “—” with an explanatory note when the API omits it.
 * @param {{label: string, value: unknown, note?: string}} options
 */
function stat(options) {
  const raw = options.value;
  const missing = raw === undefined || raw === null || raw === '';
  return el('div', { class: 'stat' }, [
    el('p', { class: 'stat-label', text: options.label }),
    el('p', { class: 'stat-value', text: missing ? '—' : String(raw) }),
    el('p', { class: 'stat-note', text: missing ? options.note || t('not reported') : options.note || '' }),
  ]);
}
