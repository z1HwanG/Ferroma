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

import { API_BASE, ApiError, request } from '../api.js';
import { num } from '../data.js';
import { el } from '../dom.js';
import { formatBytes, formatLogStamp } from '../format.js';
import { actions, adminCard, badge, cell, copyToClipboard, table, viewHead } from '../ui.js';
import { toastSuccess } from '../toast.js';

/**
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render() {
  const summary = el('div', { class: 'stat-grid', id: 'tls-summary' });
  const hint = el('p', { class: 'view-sub', id: 'tls-hint' });

  const refreshButton = el('button', { type: 'button', class: 'btn', text: 'Refresh' });
  refreshButton.addEventListener('click', () => refresh());

  const configCard = adminCard({
    title: 'Configuration',
    subtitle: '[tls] in the server configuration',
    renderData: (data) => data.node,
  });

  const certificateCard = adminCard({
    title: 'Certificate',
    subtitle: 'the PEM bundle: leaf certificate followed by intermediates',
    renderData: (data) => data.node,
  });

  const keyCard = adminCard({
    title: 'Private key',
    subtitle: 'never read, only stat-ed',
    renderData: (data) => data.node,
  });

  const listenersCard = adminCard({
    title: 'Where TLS is offered',
    subtitle: 'implicit-TLS ports and the published URL',
    renderData: (data) => data.node,
  });

  const root = el('div', {}, [
    viewHead('TLS', 'The certificates this server loads, and the ports that use them', [refreshButton]),
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
      stat({ label: 'TLS enabled', value: typeof status.enabled === 'boolean' ? (status.enabled ? 'enabled' : 'disabled') : undefined }),
      stat({ label: 'Minimum version', value: status.min_version }),
      stat({
        label: 'Certificate readable',
        value: status.certificate ? (status.certificate.readable ? 'readable' : 'unreadable') : undefined,
      }),
      stat({
        label: 'Private key readable',
        value: status.private_key ? (status.private_key.readable ? 'readable' : 'unreadable') : undefined,
      }),
      stat({ label: 'SMTPS port', value: listeners.smtps_port ? String(listeners.smtps_port) : 'off', note: 'implicit TLS' }),
      stat({ label: 'IMAPS port', value: listeners.imaps_port ? String(listeners.imaps_port) : 'off', note: 'implicit TLS' }),
      stat({
        label: 'HTTPS port',
        value: listeners.https_port ? String(listeners.https_port) : 'off',
        note: 'off means the reverse proxy terminates TLS',
      }),
    );

    const problems = [];
    if (status.enabled === false) problems.push('TLS is disabled: STARTTLS, SMTPS, IMAPS and HTTPS are all unavailable.');
    if (status.self_signed_fallback) problems.push('A self-signed certificate is generated when none is configured — development only.');
    if (status.certificate && status.certificate.present && !status.certificate.readable) problems.push('The certificate file cannot be read by this process.');
    if (status.private_key && status.private_key.present && !status.private_key.readable) problems.push('The private key cannot be read by this process; every handshake will fail.');
    if (!status.certificate || !status.certificate.present) problems.push('No certificate is configured (or the path does not exist).');
    hint.textContent = problems.length
      ? problems.join(' ')
      : 'The certificate and key are both present and readable by this process.';
  }

  function renderConfiguration(status, certificate, key) {
    configCard.setState({
      state: 'ready',
      data: {
        node: table({
          columns: [{ label: 'Setting' }, { label: 'Value' }],
          rows: [
            [cell('Enabled'), badge(status.enabled ? 'enabled' : 'disabled')],
            [cell('Minimum version'), cell(status.min_version || '—', 'cell-mono')],
            [cell('Self-signed fallback'), badge(status.self_signed_fallback ? 'enabled' : 'disabled')],
            [cell('Platform roots'), badge(status.use_platform_roots ? 'enabled' : 'disabled')],
            [cell('Allow insecure dev mode'), badge(status.allow_insecure_dev_mode ? 'enabled' : 'disabled')],
            [cell('Certificate path'), cell(certificate.path || 'not configured', 'cell-mono')],
            [cell('Key path'), cell(key.path || 'not configured', 'cell-mono')],
          ],
        }),
      },
    });
  }

  function renderFile(card, file, options) {
    const rows = [
      [cell('Path'), cell(file.path || 'not configured', 'cell-mono')],
      [cell('Present'), badge(file.present ? 'yes' : 'no')],
      [cell('Readable'), badge(file.readable ? 'yes' : 'no')],
      [cell('Size'), cell(file.size_bytes === null || file.size_bytes === undefined ? '—' : formatBytes(num(file.size_bytes)), 'cell-mono')],
      [cell('Modified'), cell(file.modified_at ? formatLogStamp(file.modified_at) : '—')],
    ];

    const children = [
      table({ columns: [{ label: 'Check' }, { label: 'Value' }], rows }),
    ];

    if (file.error) {
      children.push(el('p', { class: 'view-sub', id: options.errorId, text: file.error }));
    }

    if (options.fingerprint) {
      const valueNode = el('p', { class: 'code-block', id: options.fingerprintId, text: file.sha256 || '—' });
      const copy = el('button', { type: 'button', class: 'btn btn-small', text: 'Copy fingerprint' });
      copy.disabled = !file.sha256;
      copy.addEventListener('click', async () => {
        const copied = await copyToClipboard(file.sha256 || '', () => selectNode(valueNode));
        toastSuccess(copied ? 'Fingerprint copied.' : 'The fingerprint is selected — press Ctrl/Cmd+C to copy it.');
      });
      children.push(
        el('h3', { class: 'card-title', text: 'SHA-256 fingerprint' }),
        valueNode,
        actions(copy),
        el('p', {
          class: 'view-sub',
          text: 'Compare this with `openssl x509 -fingerprint -sha256 -noout -in <path>` on the host. Expiry is not parsed by this build.',
        }),
      );
    } else {
      children.push(
        el('p', {
          class: 'view-sub',
          text: 'The key is never read or fingerprinted: presence, size, mtime and readability are enough to catch a permissions mistake.',
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
          columns: [{ label: 'Endpoint' }, { label: 'TLS' }, { label: 'Address' }],
          rows: [
            [cell('SMTP (SMTPS)'), badge(listeners.smtps_port ? 'enabled' : 'disabled'), cell(String(listeners.smtps_port || 'off'), 'cell-mono')],
            [cell('IMAP (IMAPS)'), badge(listeners.imaps_port ? 'enabled' : 'disabled'), cell(String(listeners.imaps_port || 'off'), 'cell-mono')],
            [cell('HTTP API'), badge(listeners.https_port ? 'enabled' : 'disabled'), cell(String(listeners.https_port || 'off'), 'cell-mono')],
            [
              cell('Public URL'),
              badge(listeners.public_url_is_tls ? 'enabled' : 'disabled'),
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
      const message = error instanceof ApiError ? error.message : 'The TLS status could not be loaded.';
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
    el('p', { class: 'stat-note', text: missing ? options.note || 'not reported' : options.note || '' }),
  ]);
}
