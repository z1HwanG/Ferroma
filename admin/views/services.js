/**
 * Runtime SMTP and IMAP listener switches.
 *
 * The control is deliberately separate from generic stored settings: applying one of
 * these choices waits for the server to release or bind the real sockets, then records
 * the choice used at the next start.
 */

import { API_BASE, ApiError, request } from '../../shared/api.js';
import { el } from '../../shared/dom.js';
import { t } from '../../shared/i18n.js';
import { toastError, toastSuccess } from '../../shared/toast.js';
import { adminCard, badge, cell, table, viewHead } from '../ui.js';

/** @returns {Promise<{node: Node, cleanup: () => void}>} */
export async function render() {
  const card = adminCard({
    title: t('Mail listener services'),
    subtitle: t('Changes take effect now and are remembered after a restart.'),
    renderData: (data) => data,
  });
  const root = el('div', {}, [
    viewHead(t('Mail services'), t('Control the SMTP and IMAP listeners without editing ferroma.toml.')),
    el('section', { class: 'card' }, [
      el('p', { class: 'view-sub', text: t('Disabling SMTP stops the MX and submission listeners. Disabling IMAP does not affect SMTP or stored mail.') }),
      el('p', { class: 'view-sub', text: t('Configured SMTPS and IMAPS ports remain controlled by TLS and their port settings.') }),
      el('p', { class: 'view-sub', text: t('Disabling JMAP withdraws its discovery document and HTTP API without affecting Webmail or the management API.') }),
    ]),
    card.node,
  ]);

  async function refresh() {
    card.setState({ state: 'loading' });
    try {
      const services = await request(`${API_BASE}/services`, { toast: false });
      card.setState({ state: 'ready', data: renderServices(services || {}, setEnabled) });
    } catch (error) {
      card.setState({ state: 'error', message: error instanceof ApiError ? error.message : t('Mail services could not be loaded.') });
    }
  }

  async function setEnabled(name, enabled, button) {
    button.disabled = true;
    try {
      await request(`${API_BASE}/services/${name}`, { method: 'PUT', body: { enabled }, toast: false });
      toastSuccess(enabled ? t('{service} is accepting connections.', { service: name.toUpperCase() }) : t('{service} is no longer accepting connections.', { service: name.toUpperCase() }));
      await refresh();
    } catch (error) {
      toastError(error instanceof ApiError ? error.message : t('The listener change could not be applied.'));
      button.disabled = false;
    }
  }

  await refresh();
  return { node: root, cleanup() {} };
}

function renderServices(services, setEnabled) {
  const root = el('div', {});
  const rows = [
    row('smtp', t('SMTP'), services.smtp, t('MX, submission, and configured SMTPS listener'), setEnabled),
    row('imap', t('IMAP'), services.imap, t('STARTTLS and configured IMAPS listener'), setEnabled),
    row('jmap', t('JMAP'), services.jmap, t('JMAP discovery, API, uploads, and downloads'), setEnabled),
  ];
  root.append(table({
    columns: [{ label: t('Service') }, { label: t('Status') }, { label: t('Scope') }, { label: t('Actions') }],
    rows,
  }));
  return root;
}

function row(name, label, value, scope, setEnabled) {
  const state = value && typeof value === 'object' ? value : {};
  const available = state.available === true;
  const enabled = state.enabled === true;
  const action = el('button', {
    type: 'button',
    class: enabled ? 'btn btn-small btn-danger' : 'btn btn-small btn-primary',
    text: enabled ? t('Disable') : t('Enable'),
    disabled: !available,
    title: available ? '' : t('This listener is disabled by this process’s startup selection.'),
  });
  action.addEventListener('click', () => setEnabled(name, !enabled, action));
  return [
    cell(label),
    badge(enabled ? 'enabled' : 'disabled'),
    cell(scope),
    el('div', { class: 'cell-actions' }, [action]),
  ];
}
