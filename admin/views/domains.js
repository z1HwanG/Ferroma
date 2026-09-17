/**
 * Domains: list, create, enable/disable, delete, and the per-domain DNS Health
 * panel built from `GET /api/v1/domains/:id/dns` plus the DKIM record from
 * `GET /api/v1/domains/:id/dkim` (with a copy button).
 *
 * `POST /api/v1/domains/:id/dkim` generates a key pair the first time.
 */

import { API_BASE, ApiError, request } from '../api.js';
import { domainsOf, normalizeDkim, normalizeDnsReport } from '../data.js';
import { el, setHidden, setText } from '../dom.js';
import { formatLogStamp } from '../format.js';
import { confirmDialog, openModal, promptDialog } from '../modal.js';
import { toastError, toastSuccess } from '../toast.js';
import { actions, adminCard, badge, cell, copyToClipboard, field, table, viewHead } from '../ui.js';

const STATUS_GLYPH = { ok: '✓', warn: '!', fail: '✗', skip: '–' };
const STATUS_LABEL = { ok: 'ok', warn: 'warn', fail: 'fail', skip: 'skipped' };

/**
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render() {
  const listCard = adminCard({
    title: 'Domains',
    subtitle: 'GET /api/v1/domains',
    actions: [],
    renderEmpty: () =>
      el('div', { class: 'empty-state' }, [
        el('p', { class: 'empty-title', text: 'No domains yet' }),
        el('p', { text: 'Add the first domain this server will receive mail for.' }),
      ]),
    renderData: (data) => data.node,
  });

  const dnsTitle = el('h2', { class: 'card-title', text: 'DNS health' });
  const dnsCard = adminCard({
    title: 'DNS health',
    titleNode: dnsTitle,
    subtitle: 'Select a domain to run its checks',
    renderEmpty: () =>
      el('p', { class: 'view-sub', text: 'Choose “DNS health” on a domain above to run the live checks.' }),
    renderData: (data) => data.node,
  });

  const createButton = el('button', { type: 'button', class: 'btn btn-primary', text: 'New domain' });
  createButton.addEventListener('click', () => openCreateDialog(refresh));

  const refreshButton = el('button', { type: 'button', class: 'btn', text: 'Refresh' });
  refreshButton.addEventListener('click', () => refresh());

  const root = el('div', {}, [
    viewHead('Domains', 'Mail domains hosted by this server', [createButton, refreshButton]),
    listCard.node,
    dnsCard.node,
  ]);

  // Which domain the DNS panel is currently showing, if any.
  let dnsTarget = null;

  async function refresh() {
    listCard.setState({ state: 'loading' });
    try {
      const payload = await request(`${API_BASE}/domains`, { toast: false });
      const domains = domainsOf(payload);
      if (domains.length === 0) {
        listCard.setState({ state: 'empty' });
      } else {
        listCard.setState({ state: 'ready', data: { node: renderTable(domains, handlers) } });
      }
      if (dnsTarget) {
        const stillThere = domains.find((domain) => domain.id === dnsTarget.id);
        if (stillThere) loadDns(stillThere);
        else {
          dnsTarget = null;
          dnsCard.setState({ state: 'empty' });
        }
      }
    } catch (error) {
      listCard.setState({ state: 'error', message: messageOf(error, 'Domains could not be loaded.') });
    }
  }

  const handlers = {
    onToggle: async (domain) => {
      try {
        await request(`${API_BASE}/domains/${domain.id}`, {
          method: 'PATCH',
          body: { enabled: !domain.enabled },
          toast: false,
        });
        toastSuccess(`Domain ${domain.name} ${domain.enabled ? 'disabled' : 'enabled'}.`);
        refresh();
      } catch (error) {
        toastError(messageOf(error, 'The domain could not be updated.'));
      }
    },
    onDelete: async (domain) => {
      const confirmed = await confirmDialog({
        title: 'Delete domain',
        message: `Delete ${domain.name}? Addresses that still exist block the deletion unless you force it.`,
        confirmLabel: 'Delete domain',
      });
      if (!confirmed) return;
      try {
        await request(`${API_BASE}/domains/${domain.id}`, { method: 'DELETE', toast: false });
        toastSuccess(`Domain ${domain.name} deleted.`);
        if (dnsTarget && dnsTarget.id === domain.id) {
          dnsTarget = null;
          dnsCard.setState({ state: 'empty' });
        }
        refresh();
      } catch (error) {
        if (error instanceof ApiError && error.status === 409) {
          const forced = await confirmDialog({
            title: 'Force delete',
            message: `${error.message} Delete ${domain.name} and everything it owns?`,
            confirmLabel: 'Delete everything',
          });
          if (!forced) return;
          try {
            await request(`${API_BASE}/domains/${domain.id}?force=true`, { method: 'DELETE', toast: false });
            toastSuccess(`Domain ${domain.name} and its addresses were deleted.`);
            refresh();
          } catch (again) {
            toastError(messageOf(again, 'The domain could not be deleted.'));
          }
          return;
        }
        toastError(messageOf(error, 'The domain could not be deleted.'));
      }
    },
    onEdit: async (domain) => {
      const description = await promptDialog({
        title: `Description for ${domain.name}`,
        label: 'Description',
        confirmLabel: 'Save',
        hint: 'Shown next to the domain in this table. Leave a dash to clear it.',
      });
      if (description === null) return;
      try {
        await request(`${API_BASE}/domains/${domain.id}`, {
          method: 'PATCH',
          body: { description: description === '—' ? '' : description },
          toast: false,
        });
        toastSuccess('Domain updated.');
        refresh();
      } catch (error) {
        toastError(messageOf(error, 'The domain could not be updated.'));
      }
    },
    onDns: (domain) => {
      dnsTarget = domain;
      dnsCard.setState({ state: 'loading' });
      setText(dnsTitle, `DNS health — ${domain.name}`);
      loadDns(domain);
    },
  };

  async function loadDns(domain) {
    dnsCard.setState({ state: 'loading' });
    try {
      const [dns, dkim] = await Promise.all([
        request(`${API_BASE}/domains/${domain.id}/dns`, { toast: false }),
        request(`${API_BASE}/domains/${domain.id}/dkim`, { toast: false }).catch((error) => {
          if (error instanceof ApiError && (error.status === 404 || error.status === 501)) return null;
          throw error;
        }),
      ]);
      const report = normalizeDnsReport(dns);
      const record = dkim ? normalizeDkim(dkim) : null;
      dnsCard.setState({ state: 'ready', data: { node: renderDns(domain, report, record, () => loadDns(domain)) } });
    } catch (error) {
      dnsCard.setState({
        state: 'error',
        message: messageOf(error, 'The DNS checks could not be run.'),
        actions: [],
      });
    }
  }

  await refresh();

  return { node: root, cleanup() {} };
}

/* ------------------------------------------------------------------ rendering */

function renderTable(domains, handlers) {
  const rows = domains.map((domain) => [
    cell(domain.name, 'cell-mono'),
    cell(domain.description || '—'),
    badge(domain.enabled ? 'enabled' : 'disabled'),
    cell(domain.createdAt ? formatLogStamp(domain.createdAt) : '—'),
    actions(
      button('DNS health', () => handlers.onDns(domain)),
      button('Aliases', () => {
        window.location.hash = `#/aliases?domain=${domain.id}`;
      }),
      button(domain.enabled ? 'Disable' : 'Enable', () => handlers.onToggle(domain)),
      button('Description', () => handlers.onEdit(domain)),
      button('Delete', () => handlers.onDelete(domain), 'btn-danger'),
    ),
  ]);

  return table({
    columns: [
      { label: 'Domain' },
      { label: 'Description' },
      { label: 'State' },
      { label: 'Created' },
      { label: 'Actions' },
    ],
    rows,
  });
}

function renderDns(domain, report, record, onReload) {
  const summary = el('div', { class: 'dns-head' }, [
    el('p', {}, [
      el('span', { class: 'score', text: `${report.score} / ${report.maxScore}` }),
      el('span', {
        class: 'view-sub',
        text: report.checkedAt ? ` · checked ${formatLogStamp(report.checkedAt)}` : '',
      }),
    ]),
    el('div', { class: 'card-actions' }, [
      button('Re-run checks', onReload),
      record ? button('Generate DKIM key', () => generateDkim(domain.id, onReload)) : null,
    ].filter(Boolean)),
  ]);

  const rows = report.records.map((entry) => [
    el('span', { class: `badge badge-${entry.status}`, text: `${STATUS_GLYPH[entry.status] || '?'} ${entry.kind}` }),
    cell(STATUS_LABEL[entry.status] || entry.status),
    cell(entry.expected === null ? '—' : String(entry.expected), 'cell-mono'),
    el(
      'span',
      { class: 'cell-mono' },
      entry.found.length === 0
        ? [document.createTextNode('—')]
        : entry.found.map((value) => el('div', { class: 'truncate', title: value, text: value })),
    ),
    cell(entry.hint || ''),
  ]);

  const children = [
    summary,
    table({
      caption: `DNS records checked for ${domain.name}`,
      columns: [
        { label: 'Record' },
        { label: 'Status' },
        { label: 'Expected' },
        { label: 'Found' },
        { label: 'Hint' },
      ],
      rows,
    }),
  ];

  if (record) {
    const valueNode = el('p', { class: 'code-block', text: record.recordValue });
    const copy = el('button', { type: 'button', class: 'btn btn-small', text: 'Copy value' });
    copy.addEventListener('click', async () => {
      const copied = await copyToClipboard(record.recordValue, () => {
        const range = document.createRange();
        range.selectNodeContents(valueNode);
        const selection = window.getSelection();
        if (selection) {
          selection.removeAllRanges();
          selection.addRange(range);
        }
      });
      if (copied) toastSuccess('DKIM record copied.');
      else toastSuccess('The record is selected — press Ctrl/Cmd+C to copy it.');
    });

    children.push(
      el('h3', { class: 'card-title', text: 'DKIM record to publish' }),
      table({
        columns: [{ label: 'Field' }, { label: 'Value' }],
        rows: [
          [cell('Selector'), cell(record.selector, 'cell-mono')],
          [cell('Name'), cell(record.recordName, 'cell-mono')],
          [cell('Type'), cell(record.recordType, 'cell-mono')],
        ],
      }),
      valueNode,
      el('div', { class: 'card-actions' }, [copy, button('Generate new key', () => generateDkim(domain.id, onReload))]),
    );
  } else {
    children.push(
      el('p', { class: 'view-sub', text: 'No DKIM key is published yet for this domain.' }),
      el('div', { class: 'card-actions' }, [button('Generate DKIM key', () => generateDkim(domain.id, onReload))]),
    );
  }

  return el('div', {}, children);
}

async function generateDkim(domainId, onDone) {
  const confirmed = await confirmDialog({
    title: 'Generate DKIM key',
    message: 'A new RSA key pair will be generated for this domain. Existing signatures stay valid, but you must publish the new TXT record.',
    confirmLabel: 'Generate',
    dangerous: false,
  });
  if (!confirmed) return;
  try {
    await request(`${API_BASE}/domains/${domainId}/dkim`, { method: 'POST', toast: false });
    toastSuccess('DKIM key generated. Publish the TXT record shown below.');
    onDone();
  } catch (error) {
    toastError(messageOf(error, 'The DKIM key could not be generated.'));
  }
}

/* --------------------------------------------------------------------- create */

function openCreateDialog(onDone) {
  const name = el('input', { class: 'input', id: 'domain-name', type: 'text', autocomplete: 'off', placeholder: 'example.com' });
  const description = el('input', { class: 'input', id: 'domain-description', type: 'text', autocomplete: 'off' });
  const error = el('p', { class: 'field-error', id: 'domain-error', hidden: true });
  const body = el('div', {}, [
    field('Domain name', name, 'The bare domain, without a leading @ or a trailing dot.'),
    field('Description', description, 'Optional, shown in the list.'),
    error,
  ]);
  const cancel = el('button', { type: 'button', class: 'btn', text: 'Cancel' });
  const create = el('button', { type: 'button', class: 'btn btn-primary', text: 'Create domain' });

  const modal = openModal({
    title: 'New domain',
    body,
    footer: [cancel, create],
    onMount: (card) => {
      cancel.addEventListener('click', () => modal.close('cancel'));
      create.addEventListener('click', async () => {
        const value = name.value.trim().toLowerCase();
        if (!/^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$/.test(value)) {
          setText(error, 'Enter a valid domain, for example example.com.');
          setHidden(error, false);
          name.focus();
          return;
        }
        create.disabled = true;
        setText(create, 'Creating…');
        try {
          await request(`${API_BASE}/domains`, {
            method: 'POST',
            body: { name: value, description: description.value.trim() || undefined },
            toast: false,
          });
          toastSuccess(`Domain ${value} created.`);
          modal.close('created');
          onDone();
        } catch (err) {
          setText(error, messageOf(err, 'The domain could not be created.'));
          setHidden(error, false);
        } finally {
          create.disabled = false;
          setText(create, 'Create domain');
        }
      });
      card.addEventListener('keydown', (event) => {
        if (event.key === 'Enter' && event.target !== cancel) {
          event.preventDefault();
          create.click();
        }
      });
    },
  });
  name.focus();
}

/* -------------------------------------------------------------------- helpers */

function button(label, onClick, className = '') {
  const node = el('button', { type: 'button', class: `btn btn-small ${className}`.trim(), text: label });
  node.addEventListener('click', onClick);
  return node;
}

function messageOf(error, fallback) {
  if (error instanceof ApiError) return error.message;
  if (error instanceof Error && error.message) return error.message;
  return fallback;
}
