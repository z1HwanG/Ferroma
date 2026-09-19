/**
 * View-level building blocks for the Admin console: state-driven cards, tables
 * and pagers.
 *
 * Every view renders through `adminCard`, which reflects exactly one of four
 * states — loading, error, empty or data — so no section can silently show a
 * blank box. The previous content is replaced only when the new state is ready.
 */

import { clear, el, labelWithTitle, setHidden, setText } from './dom.js';

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
export function loadingState(message = 'Loading…') {
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
  const renderEmpty = options.renderEmpty || (() => emptyState({ title: 'Nothing to show' }));
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
      else if (next.state === 'error') body.append(renderError(next.message || 'Something went wrong.'));
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
    body.append(el('tr', {}, cells));
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
  return el('span', { class: `badge badge-${kind}`, text: status === '' ? 'unknown' : String(status) });
}

/** A row of small buttons. */
export function actions(...buttons) {
  return el('div', { class: 'cell-actions' }, buttons.filter(Boolean));
}

/**
 * @param {{offset: number, limit: number, total: number, onPrev: () => void, onNext: () => void}} options
 */
export function pager(options) {
  const prev = el('button', { type: 'button', class: 'btn btn-small', text: 'Previous' });
  const next = el('button', { type: 'button', class: 'btn btn-small', text: 'Next' });
  prev.disabled = options.offset <= 0;
  next.disabled = options.offset + options.limit >= options.total;
  prev.addEventListener('click', options.onPrev);
  next.addEventListener('click', options.onNext);
  const from = options.total === 0 ? 0 : options.offset + 1;
  const to = Math.min(options.offset + options.limit, options.total);
  return el('div', { class: 'pager' }, [
    prev,
    next,
    el('span', { class: 'pager-info', text: `${from}–${to} of ${options.total}` }),
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
    options.note ? el('p', { class: 'stat-note', text: missing ? 'not reported' : options.note }) : null,
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
      el('p', { class: 'attention-clear-text', text: 'Nothing needs attention.' }),
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

export { setHidden, setText };
