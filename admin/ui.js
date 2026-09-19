/**
 * View-level building blocks for the Admin console: state-driven cards, tables
 * and pagers.
 *
 * Every view renders through `adminCard`, which reflects exactly one of four
 * states — loading, error, empty or data — so no section can silently show a
 * blank box. The previous content is replaced only when the new state is ready.
 */

import { clear, el, labelWithTitle, setHidden, setText } from '../shared/dom.js';
import { t } from '../shared/i18n.js';

/**
 * @param {string} title
 * @param {string} [subtitle]
 * @param {Node[]} [actions]
 */
export function viewHead(title, subtitle, actions = []) {
  return el('div', { class: 'view-head' }, [
    el('div', {}, [
      el('h1', { class: 'view-title', text: title }),
      subtitle ? el('p', { class: 'view-sub', text: subtitle }) : null,
    ]),
    actions.length ? el('div', { class: 'card-actions' }, actions) : null,
  ]);
}

/**
 * @param {{title: string, subtitle?: string, actions?: Node[], titleNode?: HTMLElement}} options
 */
export function sectionHead(options) {
  return el('div', { class: 'card-head' }, [
    el('div', {}, [
      options.titleNode || el('h2', { class: 'card-title', text: options.title }),
      options.subtitle ? el('p', { class: 'view-sub', text: options.subtitle }) : null,
    ]),
    options.actions && options.actions.length ? el('div', { class: 'card-actions' }, options.actions) : null,
  ]);
}

/** A `<p class="loading-state">`. */
export function loadingState(message = t('Loading…')) {
  return el('p', { class: 'loading-state', role: 'status', text: message });
}

/**
 * @param {string} message
 * @param {Node[]} [actions]
 */
export function errorState(message, actions = []) {
  return el('div', { class: 'error-state', role: 'alert' }, [
    el('p', { text: message }),
    actions.length ? el('div', { class: 'card-actions' }, actions) : null,
  ]);
}

/**
 * @param {{title: string, message?: string, actions?: Node[]}} options
 */
export function emptyState(options) {
  return el('div', { class: 'empty-state' }, [
    el('p', { class: 'empty-title', text: options.title }),
    options.message ? el('p', { text: options.message }) : null,
    options.actions && options.actions.length ? el('div', { class: 'card-actions' }, options.actions) : null,
  ]);
}

/**
 * Create a card whose body is driven by `setState`.
 *
 * @param {{
 *   title: string,
 *   subtitle?: string,
 *   actions?: Node[],
 *   titleNode?: HTMLElement,
 *   renderLoading?: () => Node,
 *   renderEmpty?: () => Node,
 *   renderError?: (message: string) => Node,
 *   renderData: (data: unknown) => Node,
 * }} options
 */
export function adminCard(options) {
  const body = el('div', { class: 'card-body' });
  const card = el('section', { class: 'card' }, [
    sectionHead({
      title: options.title,
      subtitle: options.subtitle,
      actions: options.actions,
      titleNode: options.titleNode,
    }),
    body,
  ]);

  const renderLoading = options.renderLoading || (() => loadingState());
  const renderEmpty = options.renderEmpty || (() => emptyState({ title: t('Nothing to show') }));
  const renderError = options.renderError || ((message) => errorState(message));

  let current = '';
  let first = true;

  return {
    node: card,
    body,
    /**
     * @param {{state: 'loading'|'error'|'empty'|'ready', data?: unknown, message?: string}} next
     */
    setState(next) {
      const key = `${next.state}:${next.message || ''}`;
      if (!first && key === current) return;
      first = false;
      current = key;
      clear(body);
      if (next.state === 'loading') body.append(renderLoading());
      else if (next.state === 'error') body.append(renderError(next.message || t('Something went wrong.')));
      else if (next.state === 'empty') body.append(renderEmpty());
      else body.append(options.renderData(next.data));
    },
  };
}

/**
 * @param {{caption?: string, columns: Array<{label: string, className?: string}>, rows: Node[][]}} options
 */
export function table(options) {
  const head = el('tr', {}, options.columns.map((column) => el('th', { scope: 'col', text: column.label })));
  const body = el('tbody');
  for (const cells of options.rows) {
    // The `td` is not optional. A `<tr>` whose children are the `<span>`/`<div>` a view
    // builds is not a table row: those boxes are laid out outside the column model, so
    // no column sizing, no alignment and — the first thing to disappear — no padding.
    // Every table in this console rendered its body that way, which is why body rows did
    // not line up with their own headers.
    body.append(el('tr', {}, cells.map((node) => el('td', {}, [node]))));
  }
  return el('div', { class: 'table-wrap' }, [
    el('table', { class: 'table' }, [
      options.caption ? el('caption', { text: options.caption }) : null,
      el('thead', {}, [head]),
      body,
    ]),
  ]);
}

/** Plain text cell. */
export function cell(text, className = '') {
  const node = el('span', { class: className });
  labelWithTitle(node, text === null || text === undefined ? '—' : String(text));
  return node;
}

/** Status pill. @param {string} status */
/**
 * A status pill.
 *
 * The argument is the *token* the server or the view uses — `enabled`, `pending`,
 * `error` — not display text. The tone class is derived from that token, and the label
 * is translated here, because both belong to the same closed vocabulary: translating at
 * the call site would hand this function Chinese and leave every pill in the neutral
 * tone, which is how a localised console silently loses its colour coding.
 *
 * @param {unknown} status
 */
export function badge(status) {
  const value = String(status || 'unknown').toLowerCase();
  // The families cover every value a view passes: entity states (`enabled`), queue
  // and delivery states (`pending`, `delivered`), and `tracing` levels (`error`),
  // which the System Logs view badges with the same pill.
  const kind =
    value === 'ok' || value === 'delivered' || value === 'enabled' || value === 'true' || value === 'active' || value === 'info'
      ? 'ok'
      : value === 'warn' || value === 'warning' || value === 'retry' || value === 'pending' || value === 'delivering'
        ? 'warn'
        : value === 'fail' || value === 'failed' || value === 'disabled' || value === 'cancelled' || value === 'error'
          ? 'fail'
          : 'muted';
  const token = status === '' || status === null || status === undefined ? 'unknown' : String(status);
  // The token is translated for display; a token with no catalog entry — a value the
  // server invented, an address, a count — is shown exactly as it arrived.
  return el('span', { class: `badge badge-${kind}`, text: t(token) });
}

/** A row of small buttons. */
export function actions(...buttons) {
  return el('div', { class: 'cell-actions' }, buttons.filter(Boolean));
}

/**
 * @param {{offset: number, limit: number, total: number, onPrev: () => void, onNext: () => void}} options
 */
export function pager(options) {
  const prev = el('button', { type: 'button', class: 'btn btn-small', text: t('Previous') });
  const next = el('button', { type: 'button', class: 'btn btn-small', text: t('Next') });
  prev.disabled = options.offset <= 0;
  next.disabled = options.offset + options.limit >= options.total;
  prev.addEventListener('click', options.onPrev);
  next.addEventListener('click', options.onNext);
  const from = options.total === 0 ? 0 : options.offset + 1;
  const to = Math.min(options.offset + options.limit, options.total);
  return el('div', { class: 'pager' }, [
    prev,
    next,
    el('span', { class: 'pager-info', text: t('{from}–{to} of {total}', { from, to, total: options.total }) }),
  ]);
}

/** A labelled form field. */
export function field(label, control, hint) {
  control.setAttribute('aria-label', label);
  const labelNode = control.id
    ? el('label', { class: 'field-label', for: control.id, text: label })
    : el('span', { class: 'field-label', text: label });
  return el('div', { class: 'field' }, [
    labelNode,
    control,
    hint ? el('p', { class: 'view-sub', text: hint }) : null,
  ]);
}

/** Copy `value` to the clipboard, with a visible fallback when it is blocked. */
export async function copyToClipboard(value, onFallback) {
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(value);
      return true;
    }
  } catch {
    /* fall through to the selectable fallback */
  }
  if (onFallback) onFallback();
  return false;
}

/**
 * One dashboard measurement.
 *
 * A value the API did not report renders as “—”, never as a misleading zero, and
 * the note says what the number covers rather than which endpoint produced it —
 * the endpoint names that used to sit here (`from /health queue.received_today`)
 * told an operator nothing they could act on.
 *
 * @param {{
 *   label: string,
 *   value?: unknown,
 *   note?: string,
 *   tone?: 'ok'|'warn'|'danger',
 *   hero?: boolean,
 * }} options
 */
export function statTile(options) {
  const raw = options.value;
  const missing = raw === undefined || raw === null || raw === '';
  const classes = ['stat'];
  if (options.hero) classes.push('stat-hero');
  if (options.tone) classes.push(`stat-${options.tone}`);
  return el('div', { class: classes.join(' ') }, [
    el('p', { class: 'stat-label', text: options.label }),
    el('p', { class: 'stat-value', text: missing ? '—' : String(raw) }),
    options.note ? el('p', { class: 'stat-note', text: missing ? t('not reported') : options.note }) : null,
  ]);
}

/**
 * The exceptions an operator should act on, or an explicit all-clear.
 *
 * This is the part of a dashboard that carries a decision rather than a number:
 * the caller derives the findings, and an empty list is reported as good news
 * instead of an empty box.
 *
 * @param {Array<{tone: 'warn'|'danger', title: string, detail?: string}>} issues
 */
export function attentionList(issues) {
  if (issues.length === 0) {
    return el('div', { class: 'attention attention-clear' }, [
      el('p', { class: 'attention-clear-text', text: t('Nothing needs attention.') }),
    ]);
  }
  return el(
    'ul',
    { class: 'attention' },
    issues.map((issue) =>
      el('li', { class: `attention-item attention-${issue.tone}` }, [
        el('p', { class: 'attention-title', text: issue.title }),
        issue.detail ? el('p', { class: 'attention-detail', text: issue.detail }) : null,
      ]),
    ),
  );
}

/* ------------------------------------------------ filter bars and list tables */

/**
 * The strip above a list: the filters in a grid, the actions beside them.
 *
 * Separate from `inline-form` on purpose. A modal's inline form is a handful of
 * fields submitted together; a list's filter bar is a toolbar that must stay aligned
 * while its hint text wraps to different heights and a search box grows to fill the
 * row. Both are grids — see `.filter-bar` in styles.css for why neither is a flex row.
 *
 * @param {{fields?: Node[], actions?: Node[], id?: string}} options
 */
export function filterBar(options) {
  const fields = el('form', { class: 'filter-fields', id: options.id }, options.fields || []);
  const actions = el('div', { class: 'filter-actions' }, options.actions || []);
  return el('section', { class: 'card filter-bar' }, [fields, actions]);
}

/**
 * A table whose headers sort and whose rows can be selected for bulk work.
 *
 * Sorting is **client-side over the rows it was given**, because none of the list
 * endpoints accept a sort parameter: `/users`, `/queue`, `/logs` and `/devices`
 * take `limit`/`offset` and nothing else. Sorting a page therefore orders that page,
 * which is the honest behaviour for the API that exists rather than a control that
 * silently does nothing.
 *
 * @param {{
 *   columns: Array<{key: string, label: string, className?: string, value?: (row: object) => unknown}>,
 *   rows: Array<{key: string|number, cells: Node[]}>,
 *   sort?: {key: string, dir: 'asc'|'desc'}|null,
 *   selectable?: boolean,
 *   onSelectionChange?: (keys: Array<string|number>) => void,
 *   bulkActions?: Array<{label: string, tone?: 'danger', onClick: (keys: Array<string|number>) => void}>,
 *   emptyMessage?: string,
 * }} options
 */
export function dataTable(options) {
  const columns = options.columns;
  const selectable = Boolean(options.selectable);
  const bulkActions = options.bulkActions || [];
  const selected = new Set();
  let rows = options.rows.slice();
  let sort = options.sort || null;

  const bulkCount = el('span', { class: 'bulk-count' });
  const bulkActionsHost = el('div', { class: 'bulk-actions' });
  const bulkBar = el('div', { class: 'bulk-bar', hidden: true }, [bulkCount, bulkActionsHost]);

  const selectAll = el('input', {
    type: 'checkbox',
    class: 'row-check',
    'aria-label': t('Select every row on this page'),
  });

  const head = el('tr');
  if (selectable) head.append(el('th', { scope: 'col', class: 'col-select' }, [selectAll]));
  for (const column of columns) {
    const th = el('th', { scope: 'col', class: column.className || '' });
    if (column.sortable === false) {
      th.textContent = column.label;
    } else {
      th.append(
        el('button', {
          type: 'button',
          class: 'th-sort',
          text: column.label,
          dataset: { key: column.key },
        }),
      );
    }
    head.append(th);
  }

  const tbody = el('tbody');
  const wrap = el('div', { class: 'table-wrap' }, [
    el('table', { class: `table${selectable ? ' table-selectable' : ''}` }, [
      el('thead', {}, [head]),
      tbody,
    ]),
  ]);
  const empty = el('p', {
    class: 'empty',
    hidden: true,
    text: options.emptyMessage || t('Nothing to show.'),
  });
  const node = el('div', { class: 'data-table' }, [bulkBar, wrap, empty]);

  /**
   * What a column sorts on: the view's own accessor when it provides one, otherwise
   * the rendered text. Without a `value` a date sorts as a string, so views that show
   * dates or sizes should supply one.
   */
  function valueFor(row, key) {
    const index = columns.findIndex((column) => column.key === key);
    const column = columns[index];
    if (column && column.value) return column.value(row);
    const cellNode = row.cells[index];
    return cellNode && cellNode.textContent ? cellNode.textContent.trim() : '';
  }

  function visible() {
    if (!sort) return rows;
    const direction = sort.dir === 'desc' ? -1 : 1;
    return rows.slice().sort((a, b) => {
      const left = valueFor(a, sort.key);
      const right = valueFor(b, sort.key);
      if (typeof left === 'number' && typeof right === 'number') return (left - right) * direction;
      return String(left).localeCompare(String(right), undefined, { numeric: true, sensitivity: 'base' }) * direction;
    });
  }

  function emitSelection() {
    if (options.onSelectionChange) options.onSelectionChange([...selected]);
  }

  function renderHead() {
    for (const button of head.querySelectorAll('.th-sort')) {
      const active = Boolean(sort) && sort.key === button.dataset.key;
      button.setAttribute('aria-sort', active ? (sort.dir === 'asc' ? 'ascending' : 'descending') : 'none');
      button.dataset.dir = active ? sort.dir : '';
    }
    selectAll.checked = selectable && rows.length > 0 && rows.every((row) => selected.has(row.key));
    selectAll.indeterminate = !selectAll.checked && rows.some((row) => selected.has(row.key));
  }

  function renderBulk() {
    bulkBar.hidden = selected.size === 0;
    bulkCount.textContent = t('{count} selected', { count: selected.size });
  }

  function renderBody() {
    clear(tbody);
    const list = visible();
    wrap.hidden = list.length === 0;
    empty.hidden = list.length !== 0;
    for (const row of list) {
      const tr = el('tr', { dataset: { key: String(row.key) } });
      if (selectable) {
        const check = el('input', {
          type: 'checkbox',
          class: 'row-check',
          'aria-label': t('Select this row'),
        });
        check.checked = selected.has(row.key);
        check.addEventListener('change', () => {
          if (check.checked) selected.add(row.key);
          else selected.delete(row.key);
          renderHead();
          renderBulk();
          emitSelection();
        });
        tr.append(el('td', { class: 'col-select' }, [check]));
      }
      for (const cellNode of row.cells) tr.append(el('td', {}, [cellNode]));
      tbody.append(tr);
    }
  }

  head.addEventListener('click', (event) => {
    const button = event.target.closest('.th-sort');
    if (!button) return;
    const key = button.dataset.key;
    const dir = sort && sort.key === key && sort.dir === 'asc' ? 'desc' : 'asc';
    sort = { key, dir };
    renderHead();
    renderBody();
  });

  if (selectable) {
    selectAll.addEventListener('change', () => {
      if (selectAll.checked) for (const row of visible()) selected.add(row.key);
      else selected.clear();
      renderHead();
      renderBody();
      renderBulk();
      emitSelection();
    });
    for (const action of bulkActions) {
      const button = el('button', {
        type: 'button',
        class: `btn btn-small${action.tone === 'danger' ? ' btn-danger' : ''}`,
        text: action.label,
      });
      button.addEventListener('click', () => action.onClick([...selected]));
      bulkActionsHost.append(button);
    }
  }

  renderHead();
  renderBody();
  renderBulk();

  return {
    node,
    setRows(next) {
      rows = next.slice();
      // Drop selections whose row is gone, or a bulk action would fire for an id the
      // operator can no longer see.
      const live = new Set(rows.map((row) => row.key));
      for (const key of [...selected]) if (!live.has(key)) selected.delete(key);
      renderHead();
      renderBody();
      renderBulk();
    },
    clearSelection() {
      selected.clear();
      renderHead();
      renderBody();
      renderBulk();
    },
    selectedKeys() {
      return [...selected];
    },
  };
}

/**
 * A right-hand detail panel.
 *
 * Deliberately not a modal. Reading one queue entry or one account should not cover
 * the list it came from — an operator compares the detail against the row above it,
 * and a modal makes that impossible. The scrim only dims; the panel docks right.
 *
 * @param {{title: string, subtitle?: string, body: Node, actions?: Node[], onClose?: () => void}} options
 */
export function openDrawer(options) {
  const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;

  const closeButton = el('button', {
    type: 'button',
    class: 'btn btn-icon',
    'aria-label': t('Close details'),
    text: '\u00d7',
  });
  const panel = el(
    'aside',
    { class: 'drawer', role: 'dialog', 'aria-label': options.title || t('Details'), tabindex: '-1' },
    [
      el('header', { class: 'drawer-head' }, [
        el('div', {}, [
          el('h2', { class: 'drawer-title', text: options.title || t('Details') }),
          options.subtitle ? el('p', { class: 'view-sub', text: options.subtitle }) : null,
        ]),
        closeButton,
      ]),
      el('div', { class: 'drawer-body' }, [options.body]),
      options.actions && options.actions.length
        ? el('footer', { class: 'drawer-foot' }, options.actions)
        : null,
    ],
  );
  const host = el('div', { class: 'drawer-host' }, [el('div', { class: 'drawer-scrim' }), panel]);
  document.body.append(host);

  function onKey(event) {
    if (event.key === 'Escape') {
      event.stopPropagation();
      close();
    }
  }

  /**
   * A route change takes the panel with it.
   *
   * The drawer is appended to `<body>`, so the router replacing `#view-root` does not
   * remove it: without this, opening a queue entry and then clicking another section
   * left the old entry's panel hanging over the new one.
   */
  function onRouteChange() {
    close();
  }

  function close() {
    document.removeEventListener('keydown', onKey);
    window.removeEventListener('hashchange', onRouteChange);
    host.remove();
    if (previous && previous.isConnected) previous.focus();
    if (options.onClose) options.onClose();
  }

  closeButton.addEventListener('click', close);
  host.querySelector('.drawer-scrim').addEventListener('click', close);
  document.addEventListener('keydown', onKey);
  window.addEventListener('hashchange', onRouteChange);
  panel.focus({ preventScroll: true });

  return { close, node: panel };
}

/**
 * A key/value block for a detail panel or drawer.
 *
 * @param {Array<[string, Node|string]>} rows
 */
export function definitionList(rows) {
  return el(
    'dl',
    { class: 'kv' },
    rows.map(([label, value]) => [
      el('dt', { class: 'kv-key', text: label }),
      el('dd', { class: 'kv-value' }, [value]),
    ]).flat(),
  );
}

export { setHidden, setText };
