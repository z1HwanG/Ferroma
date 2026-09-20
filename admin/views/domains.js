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

import { API_BASE, ApiError, request } from '../../shared/api.js';
import { domainsOf, normalizeDkim, normalizeDnsReport } from '../../shared/data.js';
import { clear, el, setHidden, setText } from '../../shared/dom.js';
import { formatLogStamp } from '../../shared/format.js';
import { t, tn } from '../../shared/i18n.js';
import { confirmDialog, openModal, promptDialog } from '../../shared/modal.js';
import { icon } from '../icons.js';
import { go } from '../router.js';
import { toastError, toastSuccess } from '../../shared/toast.js';
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
  isValidDomain,
  loadingState,
  openDrawer,
  viewHead,
} from '../ui.js';

const STATUS_GLYPH = { ok: '✓', warn: '!', fail: '✗', skip: '–' };
const STATUS_LABEL = { ok: t('ok'), warn: t('warn'), fail: t('fail'), skip: t('skipped') };

/**
 * How the Status column sorts.
 *
 * Sorting by the rendered text would file `fail`, `ok`, `skip`, `warn`
 * alphabetically, which puts the records that need work in the middle. Ranking the
 * verdicts instead means one click on Status lifts every failure to the top.
 */

/**
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render() {
  const listCard = adminCard({
    title: t('Domains'),
    subtitle: t('Sort by a column heading; select rows to act on several at once.'),
    renderEmpty: () =>
      el('div', { class: 'empty-state' }, [
        el('p', { class: 'empty-title', text: t('No domains yet') }),
        el('p', { text: t('Add the first domain this server will receive mail for.') }),
      ]),
    renderData: (data) => data.node,
  });

  const filterInput = el('input', {
    class: 'input',
    id: 'domains-filter',
    type: 'search',
    autocomplete: 'off',
    placeholder: t('Filter by name or description'),
  });

  const bar = filterBar({
    id: 'domains-filter-bar',
    fields: [
      field(t('Filter'), filterInput, t('Matches a domain name or its description.')),
      el('button', { type: 'submit', class: 'btn', text: t('Filter') }),
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

  const createButton = el('button', { type: 'button', class: 'btn btn-primary', text: t('New domain') });
  createButton.addEventListener('click', () => openCreateDialog(refresh));

  const refreshButton = el('button', { type: 'button', class: 'btn', text: t('Refresh') });
  refreshButton.addEventListener('click', () => refresh());

  const root = el('div', {}, [
    viewHead(t('Domains'), t('Domains this server accepts and delivers mail for'), [createButton, refreshButton]),
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
        toastSuccess(t('Domain {name} {state}.', { name: domain.name, state: domain.enabled ? t('disabled') : t('enabled') }));
        refresh();
      } catch (error) {
        toastError(messageOf(error, t('The domain could not be updated.')));
      }
    },
    onEdit: async (domain) => {
      const description = await promptDialog({
        title: t('Description for {name}', { name: domain.name }),
        label: t('Description'),
        confirmLabel: t('Save'),
        hint: t('Shown next to the domain in this table. Leave a dash to clear it.'),
      });
      if (description === null) return;
      try {
        await request(`${API_BASE}/domains/${domain.id}`, {
          method: 'PATCH',
          body: { description: description === '—' ? '' : description },
          toast: false,
        });
        toastSuccess(t('Domain updated.'));
        refresh();
      } catch (error) {
        toastError(messageOf(error, t('The domain could not be updated.')));
      }
    },
    onDelete: async (domain) => {
      const confirmed = await confirmDialog({
        title: t('Delete domain'),
        message: t('Delete {name}? Addresses that still exist block the deletion unless you force it.', { name: domain.name }),
        confirmLabel: t('Delete domain'),
      });
      if (!confirmed) return;
      try {
        await request(`${API_BASE}/domains/${domain.id}`, { method: 'DELETE', toast: false });
        toastSuccess(t('Domain {name} deleted.', { name: domain.name }));
        refresh();
      } catch (error) {
        if (error instanceof ApiError && error.status === 409) {
          const forced = await confirmDialog({
            title: t('Force delete'),
            message: t('{message} Delete {name} and everything it owns?', { message: error.message, name: domain.name }),
            confirmLabel: t('Delete everything'),
          });
          if (!forced) return;
          try {
            await request(`${API_BASE}/domains/${domain.id}?force=true`, { method: 'DELETE', toast: false });
            toastSuccess(t('Domain {name} and its addresses were deleted.', { name: domain.name }));
            refresh();
          } catch (again) {
            toastError(messageOf(again, t('The domain could not be deleted.')));
          }
          return;
        }
        toastError(messageOf(error, t('The domain could not be deleted.')));
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
      listCard.setState({ state: 'error', message: messageOf(error, t('Domains could not be loaded.')) });
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
  if (failed > 0) {
    toastError(tn(ids.length, '{failed} of {count} domain could not be updated.', '{failed} of {count} domains could not be updated.', { failed, count: ids.length }));
  } else {
    toastSuccess(tn(ids.length, '{count} domain {state}.', '{count} domains {state}.', { count: ids.length, state: enabled ? t('enabled') : t('disabled') }));
  }
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
    title: tn(ids.length, 'Delete {count} domain?', 'Delete {count} domains?', { count: ids.length }),
    message: t('A domain that still holds addresses is refused; delete those one at a time to force it.'),
    confirmLabel: t('Delete'),
  });
  if (!confirmed) return;
  const results = await Promise.allSettled(
    ids.map((id) => request(`${API_BASE}/domains/${id}`, { method: 'DELETE', toast: false })),
  );
  const failed = results.filter((result) => result.status === 'rejected').length;
  if (failed > 0) {
    toastError(tn(ids.length, '{failed} of {count} domain could not be deleted — they may still hold addresses.', '{failed} of {count} domains could not be deleted — they may still hold addresses.', { failed, count: ids.length }));
  } else {
    toastSuccess(tn(ids.length, '{count} domain deleted.', '{count} domains deleted.', { count: ids.length }));
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
        button(t('Details'), () => handlers.onDetails(domain), '', 'details'),
        button(t('Aliases'), () => handlers.onAliases(domain), '', 'aliases'),
        button(t('Delete'), () => handlers.onDelete(domain), 'btn-danger', 'trash'),
      ),
    ],
  }));

  const grid = dataTable({
    columns: [
      { key: 'domain', label: t('Domain'), value: (row) => row.domain.name },
      { key: 'description', label: t('Description'), value: (row) => row.domain.description },
      { key: 'addresses', label: t('Addresses'), value: (row) => row.domain.mailboxCount ?? 0 },
      { key: 'state', label: t('State'), value: (row) => (row.domain.enabled ? 1 : 0) },
      {
        key: 'created',
        label: t('Created'),
        value: (row) => (row.domain.createdAt ? Date.parse(row.domain.createdAt) : 0),
      },
      { key: 'actions', label: t('Actions'), sortable: false },
    ],
    rows,
    selectable: true,
    bulkActions: [
      { label: t('Enable'), onClick: (ids) => handlers.onBulkEnabled(ids, true) },
      { label: t('Disable'), onClick: (ids) => handlers.onBulkEnabled(ids, false) },
      { label: t('Delete'), tone: 'danger', onClick: (ids) => handlers.onBulkDelete(ids) },
    ],
    emptyMessage: t('No domain matches the filter.'),
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
  const dns = el('div', {}, [loadingState(t('Running the DNS checks…'))]);
  const updatedAt = domain.updatedAt;

  const body = el('div', {}, [
    definitionList([
      [t('Description'), domain.description || '—'],
      [t('State'), domain.enabled ? t('enabled') : t('disabled')],
      [t('Catch-all'), domain.catchAll || '—'],
      [t('Addresses'), addressCount(domain)],
      [t('Created'), domain.createdAt ? formatLogStamp(domain.createdAt) : '—'],
      [t('Last changed'), updatedAt ? formatLogStamp(updatedAt) : '—'],
    ]),
    el('h3', { class: 'drawer-section', text: t('DNS health') }),
    dns,
  ]);

  const toggle = button(domain.enabled ? t('Disable') : t('Enable'), () => {
    drawer.close();
    handlers.onToggle(domain);
  });
  const edit = button(t('Edit description'), () => {
    drawer.close();
    handlers.onEdit(domain);
  });
  const remove = button(t('Delete'), () => {
    drawer.close();
    handlers.onDelete(domain);
  }, 'btn-danger');

  const drawer = openDrawer({
    title: domain.name,
    subtitle: domain.description || t('no description'),
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
  host.append(loadingState(t('Running the DNS checks…')));
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
      errorState(messageOf(error, t('The DNS checks could not be run.')), [
        button(t('Retry'), () => loadDns(domain, host), '', 'refresh'),
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
        text: report.checkedAt ? t(' · checked {when}', { when: formatLogStamp(report.checkedAt) }) : '',
      }),
    ]),
    el('div', { class: 'card-actions' }, [button(t('Re-run checks'), onReload, '', 'refresh')]),
  ]);

  // One card per record, read top to bottom.
  //
  // This was a five-column table, and a DNS record is the wrong shape for one: a single
  // SPF or DMARC value is longer than the column that held it, so the drawer showed
  // `a:localho / st -all` and `p=quarant / ine` broken mid-word, next to a horizontal
  // scrollbar. A record is a label, a verdict and three short facts — a card reads in one
  // pass and its values wrap instead of being chopped.
  const cards = report.records.map((entry) => {
    const facts = [
      [t('Expected'), entry.expected === null ? '—' : String(entry.expected), true],
      [t('Found'), entry.found.length === 0 ? '—' : entry.found.join('\n'), true],
      [t('Hint'), entry.hint || '—', false],
    ];
    return el('article', { class: `dns-record dns-record-${entry.status}` }, [
      el('header', { class: 'dns-record-head' }, [
        el('span', {
          class: `badge badge-${entry.status}`,
          text: `${STATUS_GLYPH[entry.status] || '?'} ${entry.kind}`,
        }),
        el('span', { class: 'dns-record-status', text: STATUS_LABEL[entry.status] || entry.status }),
      ]),
      el(
        'dl',
        { class: 'dns-record-facts' },
        facts.flatMap(([label, value, mono]) => [
          el('dt', { text: label }),
          el('dd', { class: mono ? 'cell-mono' : '', text: value }),
        ]),
      ),
    ]);
  });

  const children = [
    summary,
    cards.length === 0
      ? el('p', { class: 'view-sub', text: t('No DNS record was checked.') })
      : el('div', { class: 'dns-records' }, cards),
  ];

  if (record) {
    const valueNode = el('p', { class: 'code-block', text: record.recordValue });
    const copy = el('button', { type: 'button', class: 'btn btn-small', text: t('Copy value') });
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
      if (copied) toastSuccess(t('DKIM record copied.'));
      else toastSuccess(t('The record is selected — press Ctrl/Cmd+C to copy it.'));
    });

    children.push(
      el('h3', { class: 'drawer-section', text: t('DKIM record to publish') }),
      definitionList([
        [t('Selector'), cell(record.selector, 'cell-mono')],
        [t('Name'), cell(record.recordName, 'cell-mono')],
        [t('Type'), cell(record.recordType, 'cell-mono')],
      ]),
      valueNode,
      el('div', { class: 'card-actions' }, [
        copy,
        button(t('Generate new key'), () => generateDkim(domain.id, onReload), '', 'key'),
      ]),
    );
  } else {
    children.push(
      el('p', { class: 'view-sub', text: t('No DKIM key is published yet for this domain.') }),
      el('div', { class: 'card-actions' }, [button(t('Generate DKIM key'), () => generateDkim(domain.id, onReload), '', 'key')]),
    );
  }

  return el('div', {}, children);
}

async function generateDkim(domainId, onDone) {
  const confirmed = await confirmDialog({
    title: t('Generate DKIM key'),
    message: t('A new RSA key pair will be generated for this domain. Existing signatures stay valid, but you must publish the new TXT record.'),
    confirmLabel: t('Generate'),
    dangerous: false,
  });
  if (!confirmed) return;
  try {
    await request(`${API_BASE}/domains/${domainId}/dkim`, { method: 'POST', toast: false });
    toastSuccess(t('DKIM key generated. Publish the TXT record shown below.'));
    onDone();
  } catch (error) {
    toastError(messageOf(error, t('The DKIM key could not be generated.')));
  }
}

/* --------------------------------------------------------------------- create */

function openCreateDialog(onDone) {
  const name = el('input', { class: 'input', id: 'domain-name', type: 'text', autocomplete: 'off', placeholder: t('example.com') });
  const description = el('input', { class: 'input', id: 'domain-description', type: 'text', autocomplete: 'off' });
  const error = el('p', { class: 'field-error', id: 'domain-error', hidden: true });
  const body = el('div', {}, [
    field(t('Domain name'), name, t('The bare domain, without a leading @ or a trailing dot.')),
    field(t('Description'), description, t('Optional, shown in the list.')),
    error,
  ]);
  const cancel = el('button', { type: 'button', class: 'btn', text: t('Cancel') });
  const create = el('button', { type: 'button', class: 'btn btn-primary', text: t('Create domain') });

  const modal = openModal({
    title: t('New domain'),
    body,
    footer: [cancel, create],
    onMount: (card) => {
      cancel.addEventListener('click', () => modal.close('cancel'));
      create.addEventListener('click', async () => {
        const value = name.value.trim().toLowerCase();
        if (/[^\x00-\x7f]/.test(value)) {
          // The server is ASCII-only, so a Chinese domain has to arrive as punycode.
          setText(error, t('Use the punycode (xn--) form of an internationalised domain, for example xn--fsq.com.'));
          setHidden(error, false);
          name.focus();
          return;
        }
        if (!isValidDomain(value)) {
          setText(error, t('Enter a valid domain, for example example.com.'));
          setHidden(error, false);
          name.focus();
          return;
        }
        create.disabled = true;
        setText(create, t('Creating…'));
        try {
          await request(`${API_BASE}/domains`, {
            method: 'POST',
            body: { name: value, description: description.value.trim() || undefined },
            toast: false,
          });
          toastSuccess(t('Domain {name} created.', { name: value }));
          modal.close('created');
          onDone();
        } catch (err) {
          setText(error, messageOf(err, t('The domain could not be created.')));
          setHidden(error, false);
        } finally {
          create.disabled = false;
          setText(create, t('Create domain'));
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

function button(label, onClick, className = '', glyph = '') {
  const node = el('button', { type: 'button', class: `btn btn-small ${className}`.trim() }, [
    glyph ? icon(glyph, 'icon') : null,
    el('span', { text: label }),
  ]);
  node.addEventListener('click', onClick);
  return node;
}

function messageOf(error, fallback) {
  if (error instanceof ApiError) return error.message;
  if (error instanceof Error && error.message) return error.message;
  return fallback;
}
