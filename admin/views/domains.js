/**
 * Domains: one list with a client-side filter, row selection for bulk
 * enable/disable/delete, and a per-domain drawer carrying the fields a row has no
 * room for plus the live DNS health report and the DKIM record.
 *
 * `GET /api/v1/domains` is not a `Page<T>`: it answers `{items, total}` with no
 * `limit`/`offset` and no query parameters, so there is no pager and the filter
 * narrows the rows already in hand. `DELETE /api/v1/domains/:id` refuses with 409
 * while the domain still holds addresses, which is why the single-domain path
 * escalates to `?force=true` behind a second confirmation.
 */

import { API_BASE, ApiError, request } from '../api.js';
import { domainsOf, normalizeDkim, normalizeDnsReport } from '../data.js';
import { clear, el, setHidden, setText } from '../dom.js';
import { formatLogStamp } from '../format.js';
import { confirmDialog, openModal, promptDialog } from '../modal.js';
import { go } from '../router.js';
import { toastError, toastSuccess } from '../toast.js';
import {
  actions,
  adminCard,
  badge,
  cell,
  copyToClipboard,
  dataTable,
  definitionList,
  errorState,
  field,
  filterBar,
  loadingState,
  openDrawer,
  viewHead,
} from '../ui.js';

const STATUS_GLYPH = { ok: '✓', warn: '!', fail: '✗', skip: '–' };
const STATUS_LABEL = { ok: 'ok', warn: 'warn', fail: 'fail', skip: 'skipped' };

/**
 * How the Status column sorts.
 *
 * Sorting by the rendered text would file `fail`, `ok`, `skip`, `warn`
 * alphabetically, which puts the records that need work in the middle. Ranking the
 * verdicts instead means one click on Status lifts every failure to the top.
 */
const STATUS_RANK = { fail: 0, warn: 1, ok: 2, skip: 3 };

/**
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render() {
  const listCard = adminCard({
    title: 'Domains',
    subtitle: 'Sort by a column heading; select rows to act on several at once.',
    renderEmpty: () =>
      el('div', { class: 'empty-state' }, [
        el('p', { class: 'empty-title', text: 'No domains yet' }),
        el('p', { text: 'Add the first domain this server will receive mail for.' }),
      ]),
    renderData: (data) => data.node,
  });

  const filterInput = el('input', {
    class: 'input',
    id: 'domains-filter',
    type: 'search',
    autocomplete: 'off',
    placeholder: 'Filter by name or description',
  });

  const bar = filterBar({
    id: 'domains-filter-bar',
    fields: [
      field('Filter', filterInput, 'Matches the domain name or its description.'),
      el('button', { type: 'submit', class: 'btn', text: 'Filter' }),
    ],
  });
  // `GET /domains` takes no query parameter, so this is a filter over what is
  // already on screen, not a re-fetch: `input` narrows as the operator types and
  // `submit` (Enter, or the button) only repeats that pass. `submit` bubbles, so the
  // listener belongs on the bar rather than on the <form> `filterBar` builds inside.
  filterInput.addEventListener('input', () => applyFilter());
  bar.addEventListener('submit', (event) => {
    event.preventDefault();
    applyFilter();
  });

  const createButton = el('button', { type: 'button', class: 'btn btn-primary', text: 'New domain' });
  createButton.addEventListener('click', () => openCreateDialog(refresh));

  const refreshButton = el('button', { type: 'button', class: 'btn', text: 'Refresh' });
  refreshButton.addEventListener('click', () => refresh());

  const root = el('div', {}, [
    viewHead('Domains', 'Mail domains hosted by this server', [createButton, refreshButton]),
    bar,
    listCard.node,
  ]);

  // The live table, so the filter can narrow it without a second request. Only ever
  // assigned from `refresh`, after a payload actually arrived.
  let grid = null;

  const handlers = {
    onDetails: (domain) => openDomainDrawer(domain, handlers),
    onAliases: (domain) => go('aliases', { domain: domain.id }),
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
    onBulkEnabled: (ids, enabled) => setEnabled(ids, enabled, () => refresh()),
    onBulkDelete: (ids) => deleteDomains(ids, () => refresh()),
  };

  /** Re-apply the current filter text to the table on screen, if there is one. */
  function applyFilter() {
    if (grid) grid.setFilter(filterInput.value);
  }

  async function refresh() {
    listCard.setState({ state: 'loading' });
    try {
      const payload = await request(`${API_BASE}/domains`, { toast: false });
      const domains = domainsOf(payload);
      if (domains.length === 0) {
        grid = null;
        listCard.setState({ state: 'empty' });
        return;
      }
      grid = renderTable(domains, handlers);
      // A refresh must not silently drop what the operator has typed; the filter is
      // applied to the fresh rows before the table reaches the screen.
      applyFilter();
      listCard.setState({ state: 'ready', data: { node: grid.node } });
    } catch (error) {
      // Drop the reference: the table it points at has just been taken off the screen,
      // and filtering a detached node would look like a filter that does nothing.
      grid = null;
      listCard.setState({ state: 'error', message: messageOf(error, 'Domains could not be loaded.') });
    }
  }

  await refresh();

  return { node: root, cleanup() {} };
}

/* --------------------------------------------------------------- bulk actions */

/** Enable or disable several domains, reporting only what failed. */
async function setEnabled(ids, enabled, refresh) {
  const results = await Promise.allSettled(
    ids.map((id) =>
      request(`${API_BASE}/domains/${id}`, { method: 'PATCH', body: { enabled }, toast: false }),
    ),
  );
  const failed = results.filter((result) => result.status === 'rejected').length;
  if (failed > 0) toastError(`${failed} of ${ids.length} domain(s) could not be updated.`);
  else toastSuccess(`${ids.length} domain(s) ${enabled ? 'enabled' : 'disabled'}.`);
  refresh();
}

/**
 * Delete several domains behind one confirmation.
 *
 * The bulk path never sends `?force=true`. A domain that still holds addresses is
 * refused with 409, and escalating here would delete those addresses without the
 * second look the single-domain flow gives them; the refusals are reported instead
 * and left to that flow, which shows the count before it deletes anything.
 */
async function deleteDomains(ids, refresh) {
  const confirmed = await confirmDialog({
    title: `Delete ${ids.length} domain(s)?`,
    message: 'A domain that still holds addresses is refused; delete those one at a time to force it.',
    confirmLabel: 'Delete',
  });
  if (!confirmed) return;
  const results = await Promise.allSettled(
    ids.map((id) => request(`${API_BASE}/domains/${id}`, { method: 'DELETE', toast: false })),
  );
  const failed = results.filter((result) => result.status === 'rejected').length;
  if (failed > 0) {
    toastError(`${failed} of ${ids.length} domain(s) could not be deleted — they may still hold addresses.`);
  } else {
    toastSuccess(`${ids.length} domain(s) deleted.`);
  }
  refresh();
}

/* ------------------------------------------------------------------ rendering */

/**
 * The list table: one row per domain, with the name filter applied on demand.
 *
 * @param {Array<object>} domains
 * @returns {{node: Node, setFilter: (text: string) => void}}
 */
function renderTable(domains, handlers) {
  const rows = domains.map((domain) => ({
    key: domain.id,
    domain,
    cells: [
      cell(domain.name, 'cell-mono'),
      cell(domain.description || '—'),
      cell(addressCount(domain)),
      badge(domain.enabled ? 'enabled' : 'disabled'),
      cell(domain.createdAt ? formatLogStamp(domain.createdAt) : '—', 'cell-mono'),
      actions(
        button('Details', () => handlers.onDetails(domain)),
        button('Aliases', () => handlers.onAliases(domain)),
        button('Delete', () => handlers.onDelete(domain), 'btn-danger'),
      ),
    ],
  }));

  const grid = dataTable({
    columns: [
      { key: 'domain', label: 'Domain', value: (row) => row.domain.name },
      { key: 'description', label: 'Description', value: (row) => row.domain.description },
      { key: 'addresses', label: 'Addresses', value: (row) => row.domain.mailboxCount ?? 0 },
      { key: 'state', label: 'State', value: (row) => (row.domain.enabled ? 1 : 0) },
      {
        key: 'created',
        label: 'Created',
        value: (row) => (row.domain.createdAt ? Date.parse(row.domain.createdAt) : 0),
      },
      { key: 'actions', label: 'Actions', sortable: false },
    ],
    rows,
    selectable: true,
    bulkActions: [
      { label: 'Enable', onClick: (ids) => handlers.onBulkEnabled(ids, true) },
      { label: 'Disable', onClick: (ids) => handlers.onBulkEnabled(ids, false) },
      { label: 'Delete', tone: 'danger', onClick: (ids) => handlers.onBulkDelete(ids) },
    ],
    emptyMessage: 'No domain matches the filter.',
  });

  return {
    node: grid.node,
    /**
     * Narrow the rows to those whose name or description contains `text`.
     *
     * `setRows` rather than a second fetch: the endpoint pages nothing, so the whole
     * list is already in hand and re-requesting it to hide rows would be a round-trip
     * that returns the same payload.
     */
    setFilter(text) {
      const needle = text.trim().toLowerCase();
      grid.setRows(
        needle === ''
          ? rows
          : rows.filter((row) =>
              `${row.domain.name} ${row.domain.description}`.toLowerCase().includes(needle),
            ),
      );
    },
  };
}

/**
 * The detail drawer: the fields a single row cannot carry, plus the live DNS health
 * report and the DKIM record.
 *
 * The DNS panel used to be a second card under the list, and it always described
 * whichever row had been clicked last — a panel beside the whole table claimed a
 * relationship to every row in it. Docked to the row it came from, the same report
 * cannot be misread. Every action the row carried stays reachable here, and the two
 * that open a dialog close the panel first so the operator is not left with a drawer
 * behind a modal about the same domain.
 *
 * @param {object} domain
 */
function openDomainDrawer(domain, handlers) {
  const dns = el('div', {}, [loadingState('Running the DNS checks…')]);
  const updatedAt = domain.updatedAt;

  const body = el('div', {}, [
    definitionList([
      ['Description', domain.description || '—'],
      ['State', domain.enabled ? 'enabled' : 'disabled'],
      ['Catch-all', domain.catchAll || '—'],
      ['Addresses', addressCount(domain)],
      ['Created', domain.createdAt ? formatLogStamp(domain.createdAt) : '—'],
      ['Last changed', updatedAt ? formatLogStamp(updatedAt) : '—'],
    ]),
    el('h3', { class: 'drawer-section', text: 'DNS health' }),
    dns,
  ]);

  const toggle = button(domain.enabled ? 'Disable' : 'Enable', () => {
    drawer.close();
    handlers.onToggle(domain);
  });
  const edit = button('Edit description', () => {
    drawer.close();
    handlers.onEdit(domain);
  });
  const remove = button('Delete', () => {
    drawer.close();
    handlers.onDelete(domain);
  }, 'btn-danger');

  const drawer = openDrawer({
    title: domain.name,
    subtitle: domain.description || 'no description',
    body,
    actions: [toggle, edit, remove],
  });

  loadDns(domain, dns);
}

/**
 * Run the live checks for one domain and fill the drawer's DNS section.
 *
 * A domain with no DKIM key answers 404 — or 501 on a build without DKIM — and that
 * is a normal state the panel reports as "no key yet", not a failed request: failing
 * the whole panel because one of its two halves is empty would hide the DNS verdicts
 * behind a message about DKIM.
 */
async function loadDns(domain, host) {
  clear(host);
  host.append(loadingState('Running the DNS checks…'));
  try {
    const [dns, dkim] = await Promise.all([
      request(`${API_BASE}/domains/${domain.id}/dns`, { toast: false }),
      request(`${API_BASE}/domains/${domain.id}/dkim`, { toast: false }).catch((error) => {
        if (error instanceof ApiError && (error.status === 404 || error.status === 501)) return null;
        throw error;
      }),
    ]);
    clear(host);
    host.append(
      renderDns(domain, normalizeDnsReport(dns), dkim ? normalizeDkim(dkim) : null, () =>
        loadDns(domain, host),
      ),
    );
  } catch (error) {
    clear(host);
    host.append(
      errorState(messageOf(error, 'The DNS checks could not be run.'), [
        button('Retry', () => loadDns(domain, host)),
      ]),
    );
  }
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
    el('div', { class: 'card-actions' }, [button('Re-run checks', onReload)]),
  ]);

  const rows = report.records.map((entry) => ({
    key: entry.kind,
    entry,
    cells: [
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
    ],
  }));

  const children = [
    summary,
    dataTable({
      columns: [
        { key: 'record', label: 'Record', value: (row) => row.entry.kind },
        { key: 'status', label: 'Status', value: (row) => STATUS_RANK[row.entry.status] ?? 9 },
        {
          key: 'expected',
          label: 'Expected',
          value: (row) => (row.entry.expected === null ? '' : String(row.entry.expected)),
        },
        { key: 'found', label: 'Found', value: (row) => row.entry.found.join(' ') },
        { key: 'hint', label: 'Hint', value: (row) => row.entry.hint || '' },
      ],
      rows,
      emptyMessage: 'No DNS record was checked.',
    }).node,
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
      el('h3', { class: 'drawer-section', text: 'DKIM record to publish' }),
      definitionList([
        ['Selector', cell(record.selector, 'cell-mono')],
        ['Name', cell(record.recordName, 'cell-mono')],
        ['Type', cell(record.recordType, 'cell-mono')],
      ]),
      valueNode,
      el('div', { class: 'card-actions' }, [
        copy,
        button('Generate new key', () => generateDkim(domain.id, onReload)),
      ]),
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

/** How many addresses the domain holds, or a dash when the response did not say. */
function addressCount(domain) {
  const count = domain.mailboxCount;
  return count === null || count === undefined ? '—' : String(count);
}

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
