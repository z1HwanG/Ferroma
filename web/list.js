/**
 * Message list. Renders one page at a time, appends further pages through an
 * IntersectionObserver sentinel (with an explicit button for keyboards and for
 * browsers without the observer), and owns the row selection model:
 *
 *   click        select one
 *   Shift+click  select a range from the last anchor
 *   Ctrl/Cmd+click toggle one
 *   ↑ / ↓        move the active row, scroll it into view
 *   Space        toggle the active row
 *   Enter        open the active row
 */

import { API_BASE, ApiError, query, request } from '../shared/api.js';
import { messagesOf, totalOf } from '../shared/data.js';
import { byId, clear, el, labelWithTitle, setHidden, setText, svgIcon } from '../shared/dom.js';
import { timeElement } from '../shared/format.js';
import { t } from '../shared/i18n.js';
import { getPrefs, getState, mutate } from './store.js';
import { toastError } from '../shared/toast.js';

/** @type {null | {onOpen: (id: number) => void, onToggleStar: (message: object) => void}} */
let handlers = null;
let observer = null;
/** Id of the row that currently has DOM focus, so re-renders can restore it. */
let focusedRowId = 0;
/** Signature of the last paint; see `renderList`. */
let renderedSignature = '';

export function initList(options) {
  handlers = options;

  const list = byId('message-list');
  const loadMore = byId('load-more-button');
  const sentinel = byId('list-sentinel');

  list.addEventListener('click', onRowClick);
  list.addEventListener('keydown', onListKeydown);

  loadMore.addEventListener('click', () => {
    loadMoreMessages();
  });

  if (typeof IntersectionObserver === 'function') {
    observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (entry.isIntersecting) loadMoreMessages();
        }
      },
      { root: byId('list-scroll'), rootMargin: '240px 0px' },
    );
    observer.observe(sentinel);
  } else {
    setHidden(byId('list-more'), false);
  }

  // Remember which row had focus so a re-render can put focus back.
  list.addEventListener('focusin', (event) => {
    const row = event.target instanceof Element ? event.target.closest('.message-row') : null;
    if (row && row.dataset.id) focusedRowId = Number.parseInt(row.dataset.id, 10);
  });
}

/* ------------------------------------------------------------------ data load */

/**
 * Load one page of messages for the current folder / search.
 * @param {{reset?: boolean}} [options]
 */
export async function loadMessages(options = {}) {
  const state = getState();
  if (state.listLoading) return;
  if (!state.folder && !state.search) {
    mutate((draft) => {
      draft.messages = [];
      draft.total = 0;
    });
    return;
  }

  const offset = options.reset ? 0 : state.messages.length;
  mutate((draft) => {
    draft.listLoading = true;
    if (draft.messages.length === 0) setHidden(byId('list-skeleton'), false);
  });

  const params = {
    limit: getPrefs().perPage,
    offset,
    query: state.search || undefined,
    folder_id: state.folder && state.folder.id ? state.folder.id : undefined,
    mailbox_id: state.mailboxId || undefined,
  };

  try {
    const payload = await request(`${API_BASE}/messages${query(params)}`, { toast: false });
    const page = messagesOf(payload);
    mutate((draft) => {
      draft.messages = options.reset ? page : draft.messages.concat(page);
      draft.offset = draft.messages.length;
      draft.total = totalOf(payload);
      draft.hasMore = page.length > 0 && draft.messages.length < draft.total;
      draft.listLoading = false;
    });
  } catch (error) {
    mutate((draft) => {
      draft.listLoading = false;
      if (options.reset) draft.messages = [];
    });
    if (error instanceof ApiError && error.network) {
      setHidden(byId('offline-banner'), false);
    } else if (error instanceof ApiError) {
      setText(byId('list-empty'), error.message);
      setHidden(byId('list-empty'), false);
    }
  } finally {
    setHidden(byId('list-skeleton'), true);
  }
}

/** Fetch the next page, if there is one. */
async function loadMoreMessages() {
  const state = getState();
  if (!state.hasMore || state.listLoading) return;
  await loadMessages({ reset: false });
}

/* --------------------------------------------------------------------- render */

export function renderList() {
  const state = getState();
  const list = byId('message-list');
  const loadMore = byId('list-more');
  const empty = byId('list-empty');

  // Repainting on every state change would drop the row a keyboard user is on;
  // only a change to what the list actually shows is worth rebuilding for.
  const signature = [
    state.messages.length,
    state.total,
    state.selectedId,
    state.checked.size,
    state.listLoading ? 1 : 0,
    state.hasMore ? 1 : 0,
  ].join(':');
  if (signature === renderedSignature) return;
  renderedSignature = signature;

  const previouslyFocused = document.activeElement instanceof Element ? document.activeElement : null;
  const keepFocus =
    focusedRowId > 0 &&
    Boolean(previouslyFocused && previouslyFocused.closest('#message-list'));

  clear(list);
  list.setAttribute('aria-busy', state.listLoading && state.messages.length === 0 ? 'true' : 'false');

  if (state.messages.length === 0) {
    list.hidden = true;
    setHidden(loadMore, true);
    // Keep whatever the empty pane said while a load is in flight, so the pane
    // never flashes between "empty" and the arriving content.
    if (!state.listLoading) {
      setText(
        empty,
        state.search
          ? t('No messages match “{query}” in this folder.', { query: state.search })
          : t('This folder is empty.'),
      );
      setHidden(empty, false);
    }
    renderBulkBar();
    return;
  }

  list.hidden = false;
  setHidden(empty, true);

  const fragment = document.createDocumentFragment();
  state.messages.forEach((message, index) => {
    fragment.append(renderRow(message, index, state));
  });
  list.append(fragment);

  setHidden(loadMore, !state.hasMore);
  setText(byId('list-count'), `${state.messages.length} / ${state.total}`);
  renderBulkBar();

  if (keepFocus) {
    const restored = document.getElementById(`message-row-${focusedRowId}`);
    if (restored instanceof HTMLElement) {
      restored.tabIndex = 0;
      restored.focus();
    }
  }
}

/** Keep the list header and bulk bar in step with state. */
function renderBulkBar() {
  const state = getState();
  const bar = byId('bulk-bar');
  const count = state.checked.size;
  setHidden(bar, count === 0);
  setText(byId('bulk-count'), t('{count} selected', { count }));
}

/**
 * @param {object} message
 * @param {number} index
 * @param {ReturnType<typeof getState>} state
 */
function renderRow(message, index, state) {
  const selected = state.selectedId === message.id;
  const checked = state.checked.has(message.id);
  const row = el('li', {
    class:
      `message-row${message.seen ? '' : ' unread'}${selected ? ' is-active' : ''}` +
      `${checked ? ' is-checked' : ''}`,
    role: 'option',
    id: `message-row-${message.id}`,
    'aria-selected': checked ? 'true' : 'false',
    tabindex: selected || (state.selectedId === 0 && index === 0) ? '0' : '-1',
  });
  row.dataset.id = String(message.id);
  row.dataset.index = String(index);

  const dot = el('span', { class: 'row-dot' });
  if (!message.seen) dot.append(el('span', { class: 'dot' }));
  dot.setAttribute('aria-hidden', 'true');

  const sender = el('span', { class: 'row-sender' });
  labelWithTitle(sender, message.fromName || message.fromAddress || t('(unknown sender)'), t('From'));

  const subject = el('span', { class: 'row-subject' });
  labelWithTitle(subject, message.subject, t('Subject'));

  const line = el('span', { class: 'row-line' }, [sender, el('span', { class: 'row-sep', text: '—' }), subject]);
  const snippet = el('span', { class: 'row-snippet' });
  labelWithTitle(snippet, message.snippet || '', t('Preview'));

  const side = el('span', { class: 'row-side' });
  if (message.hasAttachments) {
    const clip = svgIcon('clip');
    clip.setAttribute('class', 'row-clip');
    clip.setAttribute('role', 'img');
    clip.setAttribute('aria-label', t('Has attachments'));
    side.append(clip);
  }
  side.append(timeElement(message.date));

  const star = el('button', {
    type: 'button',
    class: 'btn btn-icon btn-small row-flag',
    'aria-pressed': message.flagged ? 'true' : 'false',
    'aria-label': message.flagged
      ? t('Unstar “{subject}”', { subject: message.subject })
      : t('Star “{subject}”', { subject: message.subject }),
    title: message.flagged ? t('Remove star') : t('Add star'),
  });
  star.append(svgIcon('star'));
  if (!message.flagged) star.style.visibility = 'hidden';
  star.addEventListener('click', (event) => {
    event.stopPropagation();
    if (handlers) handlers.onToggleStar(message);
  });

  const sideWrap = el('span', { class: 'row-side' }, [star, side]);

  row.append(dot, el('span', { class: 'row-main' }, [line, snippet]), sideWrap);
  return row;
}

/* ------------------------------------------------------------------ selection */

function onRowClick(event) {
  const target = event.target instanceof Element ? event.target : null;
  if (!target) return;
  const row = target.closest('.message-row');
  if (!row) return;
  if (target.closest('button')) return;

  const id = Number.parseInt(row.dataset.id || '0', 10);
  const index = Number.parseInt(row.dataset.index || '-1', 10);
  if (!id || index < 0) return;

  const state = getState();
  if (event.shiftKey && state.lastCheckedIndex >= 0) {
    mutate((draft) => {
      const [from, to] = [state.lastCheckedIndex, index].sort((a, b) => a - b);
      draft.checked = new Set(draft.checked);
      for (let i = from; i <= to; i += 1) {
        const candidate = draft.messages[i];
        if (candidate) draft.checked.add(candidate.id);
      }
    });
    renderList();
    return;
  }

  if (event.ctrlKey || event.metaKey) {
    mutate((draft) => {
      draft.checked = new Set(draft.checked);
      if (draft.checked.has(id)) draft.checked.delete(id);
      else draft.checked.add(id);
      draft.lastCheckedIndex = index;
    });
    renderList();
    return;
  }

  mutate((draft) => {
    draft.checked = new Set();
    draft.lastCheckedIndex = index;
  });
  if (handlers) handlers.onOpen(id);
}

function onListKeydown(event) {
  const target = event.target instanceof Element ? event.target : null;
  const row = target ? target.closest('.message-row') : null;
  if (!row) return;
  const index = Number.parseInt(row.dataset.index || '-1', 10);
  const id = Number.parseInt(row.dataset.id || '0', 10);
  if (index < 0 || !id) return;

  const state = getState();
  const move = (delta) => {
    const next = state.messages[index + delta];
    if (!next) return;
    const node = document.getElementById(`message-row-${next.id}`);
    if (node instanceof HTMLElement) {
      node.tabIndex = 0;
      node.focus();
      node.scrollIntoView({ block: 'nearest' });
    }
  };

  if (event.key === 'ArrowDown') {
    event.preventDefault();
    move(1);
  } else if (event.key === 'ArrowUp') {
    event.preventDefault();
    move(-1);
  } else if (event.key === ' ') {
    event.preventDefault();
    mutate((draft) => {
      draft.checked = new Set(draft.checked);
      if (draft.checked.has(id)) draft.checked.delete(id);
      else draft.checked.add(id);
      draft.lastCheckedIndex = index;
    });
    renderList();
    document.getElementById(`message-row-${id}`)?.focus();
  } else if (event.key === 'Enter') {
    event.preventDefault();
    mutate((draft) => {
      draft.checked = new Set();
      draft.lastCheckedIndex = index;
    });
    if (handlers) handlers.onOpen(id);
  }
}

/** Focus the first row of the list, for the `/`-less keyboard path. */
export function focusList() {
  const state = getState();
  const list = byId('message-list');
  const row =
    list.querySelector(`.message-row[data-id="${state.selectedId}"]`) || list.querySelector('.message-row');
  if (row instanceof HTMLElement) row.focus();
}

/* ---------------------------------------------------------------- row actions */

/** Toggle the star of one message. */
export async function toggleStar(message) {
  const next = !message.flagged;
  try {
    await request(`${API_BASE}/messages/${message.id}`, {
      method: 'PATCH',
      body: { flagged: next },
      toast: false,
    });
    mutate((draft) => {
      draft.messages = draft.messages.map((candidate) =>
        candidate.id === message.id ? Object.assign({}, candidate, { flagged: next }) : candidate,
      );
      if (draft.selected && draft.selected.id === message.id) {
        draft.selected = Object.assign({}, draft.selected, { flagged: next });
      }
    });
    renderList();
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : t('The star could not be changed.'));
  }
}

/** Drop every checked row. */
export function clearChecked() {
  mutate((draft) => {
    draft.checked = new Set();
    draft.lastCheckedIndex = -1;
  });
  renderList();
}

/** Mark the given ids as seen/unseen locally, without a round trip. */
export function applySeen(ids, seen) {
  const wanted = new Set(ids);
  mutate((draft) => {
    draft.messages = draft.messages.map((message) =>
      wanted.has(message.id) ? Object.assign({}, message, { seen }) : message,
    );
    if (draft.selected && wanted.has(draft.selected.id)) {
      draft.selected = Object.assign({}, draft.selected, { seen });
    }
  });
  renderList();
}

/** Remove the given ids from the list after a delete or a move. */
export function removeFromList(ids) {
  const wanted = new Set(ids);
  mutate((draft) => {
    draft.messages = draft.messages.filter((message) => !wanted.has(message.id));
    draft.total = Math.max(0, draft.total - wanted.size);
    draft.checked = new Set();
    if (draft.selected && wanted.has(draft.selected.id)) {
      draft.selected = null;
      draft.selectedId = 0;
    }
  });
  renderList();
}
