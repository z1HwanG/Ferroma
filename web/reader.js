/**
 * Reading pane.
 *
 * Message bodies are attacker-controlled, so:
 *   * the text part is written with `textContent` into a `<pre>`;
 *   * the HTML part is rendered only inside `<iframe sandbox="" srcdoc=…>`,
 *     which forbids scripts, forms, popups and top-level navigation;
 *   * attachment names are written with `textContent` and never used as markup.
 *
 * A message is marked seen once it has been visible for two seconds.
 */

import { API_BASE, ApiError, download, request } from './api.js';
import { normalizeMessage } from './data.js';
import {
  byId,
  clear,
  el,
  labelWithTitle,
  setHidden,
  setText,
  svgIcon,
} from './dom.js';
import { fileKind, formatBytes, fullStamp, timeElement } from './format.js';
import { confirmDialog, openModal } from './modal.js';
import { getPrefs, getState, mutate } from './store.js';
import { toastError, toastSuccess } from './toast.js';

const SEEN_DELAY_MS = 2000;
const MAX_IFRAME_HEIGHT = 4000;

/** @type {null | {onCompose: (mode: string, message: object) => void, onAfterChange: () => void, onBack: () => void}} */
let handlers = null;
/** @type {number} */
let seenTimer = 0;
/** @type {number} */
let seenForId = 0;
/** @type {IntersectionObserver|null} */
let visibilityObserver = null;
/** @type {string} */
let preferredPart = 'html';

export function initReader(options) {
  handlers = options;

  byId('reader-back').addEventListener('click', () => {
    if (handlers) handlers.onBack();
  });
  byId('action-reply').addEventListener('click', () => withMessage((message) => handlers.onCompose('reply', message)));
  byId('action-reply-all').addEventListener('click', () =>
    withMessage((message) => handlers.onCompose('reply-all', message)),
  );
  byId('action-forward').addEventListener('click', () =>
    withMessage((message) => handlers.onCompose('forward', message)),
  );
  byId('action-star').addEventListener('click', () => withMessage(toggleStar));
  byId('action-unread').addEventListener('click', () => withMessage(markUnread));
  byId('action-archive').addEventListener('click', () => withMessage(archive));
  byId('action-move').addEventListener('click', () => withMessage(openMoveDialog));
  byId('action-delete').addEventListener('click', () => withMessage(deleteMessage));

  byId('tab-html').addEventListener('click', () => {
    preferredPart = 'html';
    renderBody();
  });
  byId('tab-text').addEventListener('click', () => {
    preferredPart = 'text';
    renderBody();
  });

  const html = byId('reader-html');
  html.addEventListener('load', () => {
    sizeIframe(html);
  });

  if (typeof IntersectionObserver === 'function') {
    visibilityObserver = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (entry.isIntersecting) scheduleSeen();
          else cancelSeen();
        }
      },
      { threshold: 0.25 },
    );
    visibilityObserver.observe(byId('reader-pane'));
  }
}

/** @param {(message: object) => void} action */
function withMessage(action) {
  const state = getState();
  if (!state.selected) return;
  action(state.selected);
}

/* --------------------------------------------------------------- open / fetch */

/**
 * Load a message into the reading pane.
 * @param {number} id
 */
export async function openMessage(id) {
  cancelSeen();
  const state = getState();
  const inList = state.messages.find((message) => message.id === id);
  mutate((draft) => {
    draft.selectedId = id;
    draft.view = 'reader';
    draft.readerLoading = true;
    if (inList) draft.selected = inList;
  });
  renderReader();
  renderChrome();

  try {
    const payload = await request(`${API_BASE}/messages/${id}`, { toast: false });
    const message = normalizeMessage(payload);
    if (getState().selectedId !== id) return;
    mutate((draft) => {
      draft.selected = message;
      draft.readerLoading = false;
      draft.messages = draft.messages.map((candidate) =>
        candidate.id === id
          ? Object.assign({}, candidate, {
              seen: candidate.seen || message.seen,
              flagged: message.flagged,
              subject: message.subject || candidate.subject,
            })
          : candidate,
      );
    });
    renderReader();
    renderChrome();
    scheduleSeen();
  } catch (error) {
    mutate((draft) => {
      draft.readerLoading = false;
    });
    setText(
      byId('reader-error'),
      error instanceof ApiError ? error.message : 'This message could not be loaded.',
    );
    setHidden(byId('reader-error'), false);
    renderChrome();
  }
}

/** Close the reading pane (back button on narrow screens). */
export function closeReader() {
  cancelSeen();
  mutate((draft) => {
    draft.selectedId = 0;
    draft.selected = null;
    draft.view = 'list';
  });
  renderReader();
  renderChrome();
}

/* ------------------------------------------------------------------- rendering */

export function renderReader() {
  const state = getState();
  const reader = byId('reader');
  const empty = byId('reader-empty');
  const back = byId('reader-back');

  setHidden(back, state.view !== 'reader');
  if (state.view === 'list' || (!state.selected && !state.readerLoading)) {
    setHidden(reader, true);
    setHidden(empty, false);
    return;
  }

  setHidden(empty, true);
  setHidden(reader, false);

  if (!state.selected) return;

  const message = state.selected;
  const subject = byId('reader-subject');
  labelWithTitle(subject, message.subject || '(no subject)', 'Subject');

  const from = byId('reader-from');
  labelWithTitle(from, message.from || '(unknown sender)', 'From');

  const to = byId('reader-to');
  labelWithTitle(to, message.to.join(', ') || '(no recipients)', 'To');

  const ccLine = byId('reader-cc-line');
  setHidden(ccLine, message.cc.length === 0);
  if (message.cc.length) labelWithTitle(byId('reader-cc'), message.cc.join(', '), 'Cc');

  const date = byId('reader-date');
  clear(date);
  const stamp = timeElement(message.date);
  date.dateTime = stamp.dateTime || '';
  date.textContent = fullStamp(message.date);
  date.title = stamp.title || '';

  renderAttachments(message);
  renderBody();
  setHidden(byId('reader-error'), true);
}

function renderAttachments(message) {
  const section = byId('reader-attachments');
  const list = byId('attachment-list');
  clear(list);
  setHidden(section, message.attachments.length === 0);
  for (const attachment of message.attachments) {
    const chip = el('button', {
      type: 'button',
      class: 'attachment-chip',
      'aria-label': `Download ${attachment.filename}, ${formatBytes(attachment.sizeBytes)}`,
      title: `${attachment.filename} — ${formatBytes(attachment.sizeBytes)}`,
    });
    const icon = svgIcon('clip');
    icon.setAttribute('class', 'attachment-icon');
    const name = el('span', { class: 'attachment-name', text: attachment.filename });
    const size = el('span', { class: 'attachment-size', text: formatBytes(attachment.sizeBytes) });
    const kind = el('span', { class: 'attachment-size', text: fileKind(attachment.filename) });
    chip.append(icon, name, kind, size);
    chip.addEventListener('click', () => downloadAttachment(attachment));
    list.append(el('li', {}, [chip]));
  }
}

function renderBody() {
  const state = getState();
  const message = state.selected;
  if (!message) return;

  const hasHtml = typeof message.html === 'string' && message.html.trim() !== '';
  const hasText = typeof message.text === 'string' && message.text.trim() !== '';
  const tabs = byId('reader-tabs');
  const textNode = byId('reader-text');
  const frame = byId('reader-html');

  setHidden(tabs, !(hasHtml && hasText));

  const showHtml = hasHtml && (preferredPart === 'html' || !hasText);
  setText(byId('tab-html'), 'Rich text');
  setText(byId('tab-text'), 'Plain text');
  byId('tab-html').setAttribute('aria-pressed', showHtml ? 'true' : 'false');
  byId('tab-text').setAttribute('aria-pressed', showHtml ? 'false' : 'true');

  if (showHtml) {
    setHidden(textNode, true);
    setHidden(frame, false);
    const html = message.html;
    if (frame.getAttribute('srcdoc') !== html) {
      frame.setAttribute('srcdoc', html);
      // If the browser has already fired `load` for this document, size it now;
      // the handler covers the asynchronous case.
      window.requestAnimationFrame(() => sizeIframe(frame));
    } else {
      sizeIframe(frame);
    }
    return;
  }

  setHidden(frame, true);
  frame.removeAttribute('srcdoc');
  setHidden(textNode, false);
  textNode.textContent =
    (hasText ? message.text : '') ||
    (hasHtml ? 'This message has an HTML body only.' : 'This message has no body.');
}

/** Grow the sandboxed frame to its content so the pane scrolls as one page. */
function sizeIframe(frame) {
  try {
    const doc = frame.contentDocument;
    if (!doc || !doc.documentElement) return;
    const height = Math.max(
      doc.documentElement.scrollHeight,
      doc.body ? doc.body.scrollHeight : 0,
      160,
    );
    frame.style.height = `${Math.min(height + 16, MAX_IFRAME_HEIGHT)}px`;
    frame.style.overflow = height + 16 > MAX_IFRAME_HEIGHT ? 'auto' : 'hidden';
  } catch {
    frame.style.height = '420px';
  }
}

/** Header bits that depend on the selected message (star state, archive). */
export function renderChrome() {
  const state = getState();
  const message = state.selected;
  const star = byId('action-star');
  star.setAttribute('aria-pressed', message && message.flagged ? 'true' : 'false');
  star.textContent = message && message.flagged ? 'Unstar' : 'Star';
  star.disabled = !message;
  byId('action-unread').disabled = !message;
  byId('action-archive').disabled = !message;
  byId('action-move').disabled = !message;
  byId('action-delete').disabled = !message;
  byId('action-reply').disabled = !message;
  byId('action-reply-all').disabled = !message;
  byId('action-forward').disabled = !message;
}

/* ---------------------------------------------------------- auto mark as read */

function scheduleSeen() {
  const state = getState();
  if (!state.selected) return;
  if (!getPrefs().markReadOnOpen) return;
  if (state.selected.seen) return;
  if (seenForId === state.selected.id && seenTimer) return;
  cancelSeen();
  seenForId = state.selected.id;
  const id = state.selected.id;
  seenTimer = window.setTimeout(() => {
    seenTimer = 0;
    markSeen(id);
  }, SEEN_DELAY_MS);
}

function cancelSeen() {
  if (seenTimer) window.clearTimeout(seenTimer);
  seenTimer = 0;
  seenForId = 0;
}

/** @param {number} id */
async function markSeen(id) {
  const state = getState();
  if (!state.selected || state.selected.id !== id || state.selected.seen) return;
  try {
    await request(`${API_BASE}/messages/${id}`, {
      method: 'PATCH',
      body: { seen: true },
      toast: false,
    });
    mutate((draft) => {
      draft.messages = draft.messages.map((message) =>
        message.id === id ? Object.assign({}, message, { seen: true }) : message,
      );
      if (draft.selected && draft.selected.id === id) {
        draft.selected = Object.assign({}, draft.selected, { seen: true });
      }
    });
    if (handlers) handlers.onAfterChange();
  } catch (error) {
    if (error instanceof ApiError && error.network) setHidden(byId('offline-banner'), false);
  }
}

/* --------------------------------------------------------------- reader actions */

async function toggleStar(message) {
  const next = !message.flagged;
  try {
    await request(`${API_BASE}/messages/${message.id}`, {
      method: 'PATCH',
      body: { flagged: next },
      toast: false,
    });
    mutate((draft) => {
      draft.selected = Object.assign({}, draft.selected, { flagged: next });
      draft.messages = draft.messages.map((candidate) =>
        candidate.id === message.id ? Object.assign({}, candidate, { flagged: next }) : candidate,
      );
    });
    renderReader();
    renderChrome();
    if (handlers) handlers.onAfterChange();
    toastSuccess(next ? 'Starred.' : 'Star removed.');
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : 'The star could not be changed.');
  }
}

async function markUnread(message) {
  try {
    await request(`${API_BASE}/messages/${message.id}`, {
      method: 'PATCH',
      body: { seen: false },
      toast: false,
    });
    cancelSeen();
    mutate((draft) => {
      draft.selected = Object.assign({}, draft.selected, { seen: false });
      draft.messages = draft.messages.map((candidate) =>
        candidate.id === message.id ? Object.assign({}, candidate, { seen: false }) : candidate,
      );
    });
    renderReader();
    renderChrome();
    if (handlers) handlers.onAfterChange();
    toastSuccess('Marked unread.');
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : 'The message could not be updated.');
  }
}

async function archive(message) {
  const state = getState();
  const archiveFolder = state.folders.find((folder) => folder.specialUse === 'archive');
  if (!archiveFolder) {
    toastError('This mailbox has no Archive folder.');
    return;
  }
  await moveTo(message, archiveFolder, 'Archived.');
}

async function moveTo(message, folder, successText) {
  try {
    await request(`${API_BASE}/messages/${message.id}/move`, {
      method: 'POST',
      body: { folder_id: folder.id },
      toast: false,
    });
    mutate((draft) => {
      draft.messages = draft.messages.filter((candidate) => candidate.id !== message.id);
      draft.selected = null;
      draft.selectedId = 0;
      draft.view = 'list';
      draft.total = Math.max(0, draft.total - 1);
    });
    renderReader();
    if (handlers) handlers.onAfterChange();
    toastSuccess(successText);
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : 'The message could not be moved.');
  }
}

async function deleteMessage(message) {
  const state = getState();
  const trash = state.folders.find((folder) => folder.specialUse === 'trash');
  const alreadyInTrash = Boolean(state.folder && trash && state.folder.id === trash.id);

  if (alreadyInTrash) {
    const confirmed = await confirmDialog({
      title: 'Delete permanently',
      message: `“${message.subject}” will be removed for good. This cannot be undone.`,
      confirmLabel: 'Delete permanently',
    });
    if (!confirmed) return;
    try {
      await request(`${API_BASE}/messages/${message.id}?permanent=true`, { method: 'DELETE', toast: false });
      afterRemoval(message.id);
      toastSuccess('Message deleted permanently.');
    } catch (error) {
      toastError(error instanceof ApiError ? error.message : 'The message could not be deleted.');
    }
    return;
  }

  const confirmed = await confirmDialog({
    title: 'Move to Trash',
    message: `“${message.subject}” will be moved to Trash.`,
    confirmLabel: 'Move to Trash',
  });
  if (!confirmed) return;
  try {
    await request(`${API_BASE}/messages/${message.id}`, { method: 'DELETE', toast: false });
    afterRemoval(message.id);
    toastSuccess('Moved to Trash.');
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : 'The message could not be deleted.');
  }
}

function afterRemoval(id) {
  mutate((draft) => {
    draft.messages = draft.messages.filter((candidate) => candidate.id !== id);
    draft.selected = null;
    draft.selectedId = 0;
    draft.view = 'list';
    draft.total = Math.max(0, draft.total - 1);
  });
  renderReader();
  if (handlers) handlers.onAfterChange();
}

/** Folder picker for “Move to”. */
async function openMoveDialog(message) {
  const state = getState();
  const others = state.folders.filter((folder) => !state.folder || folder.id !== state.folder.id);
  if (others.length === 0) {
    toastError('There is no other folder to move this message to.');
    return;
  }

  const selectId = 'move-target-select';
  const select = el('select', { class: 'input', id: selectId, size: String(Math.min(others.length, 8)) });
  for (const folder of others) {
    select.append(el('option', { value: String(folder.id), text: folder.name }));
  }
  const body = el('div', {}, [
    el('p', { class: 'modal-message', text: `Move “${message.subject}” to:` }),
    el('label', { class: 'visually-hidden', for: selectId, text: 'Destination folder' }),
    select,
  ]);
  const cancel = el('button', { type: 'button', class: 'btn', text: 'Cancel' });
  const confirm = el('button', { type: 'button', class: 'btn btn-primary', text: 'Move' });

  const modal = openModal({
    title: 'Move message',
    body,
    footer: [cancel, confirm],
    onMount: () => {
      cancel.addEventListener('click', () => modal.close('cancel'));
      confirm.addEventListener('click', () => {
        const folder = others.find((candidate) => candidate.id === Number.parseInt(select.value, 10));
        modal.close('confirm');
        if (folder) moveTo(message, folder, `Moved to ${folder.name}.`);
      });
    },
  });
}

/** Stream an attachment to disk through the authenticated request path. */
async function downloadAttachment(attachment) {
  try {
    await download(`${API_BASE}/attachments/${attachment.id}`, attachment.filename);
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : 'The attachment could not be downloaded.');
  }
}
