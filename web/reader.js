/**
 * Reading pane.
 *
 * Message bodies are attacker-controlled, so:
 *   * the text part is written with `textContent` into a `<pre>`;
 *   * the HTML part is rendered only inside `<iframe sandbox="" srcdoc=…>`,
 *     which forbids scripts, forms, popups and top-level navigation;
 *   * attachment names are written with `textContent` and never used as markup.
 *
 * A sandboxed frame has an opaque origin, so the parent cannot read its
 * `contentDocument` and cannot grow it to fit its content. The frame therefore has a
 * fixed, generous height from CSS (`styles.css`, `.reader-html`) and scrolls inside
 * itself. Everything a message must not be able to do is still forbidden; what is
 * given up is only the automatic height.
 *
 * A message is marked seen as soon as it is opened, unless the reader turned that
 * preference off in Settings.
 */

import { API_BASE, ApiError, download, request } from '../shared/api.js';
import { normalizeMessage } from '../shared/data.js';
import {
  byId,
  clear,
  el,
  labelWithTitle,
  setHidden,
  setText,
  svgIcon,
} from '../shared/dom.js';
import { fileKind, formatBytes, fullStamp, timeElement } from '../shared/format.js';
import { t } from '../shared/i18n.js';
import { confirmDialog, openModal } from '../shared/modal.js';
import { getPrefs, getState, mutate } from './store.js';
import { onThemeChange } from '../shared/theme.js';
import { toastError, toastSuccess } from '../shared/toast.js';

/** @type {null | {onCompose: (mode: string, message: object) => void, onAfterChange: () => void, onBack: () => void}} */
let handlers = null;
/**
 * The stylesheet injected into the sandboxed frame when the client is in dark mode.
 *
 * A message is authored against white paper, and a white rectangle in the middle of a dark
 * client is the one place the theme visibly stops. The frame's origin is opaque, so the
 * parent cannot restyle it after load — but the parent *composes* the `srcdoc`, which is
 * what makes this possible at all: the transform is applied where the document is built.
 *
 * Media is inverted back, so photographs, logos and screenshots keep their colours. A
 * message that was already authored dark will come out light; that is the trade, and it is
 * the same one every mail client that inverts makes.
 */
const DARK_MESSAGE_CSS =
  '<style>' +
  // The background goes on `html`, unfiltered, and only the *body* is inverted: a filter
  // on the root element leaves the viewport canvas to the browser, which paints it white
  // — the transform then applies to content that is already sitting on white paper.
  'html{background:#14181f}' +
  'body{filter:invert(0.92) hue-rotate(180deg)}' +
  // Media is inverted back, so photographs, logos and screenshots keep their colours.
  'img,video,picture,canvas,svg image{filter:invert(1) hue-rotate(180deg)}' +
  '</style>';

/** Whether the client is currently rendering in dark. */
function darkMode() {
  return document.documentElement.getAttribute('data-theme-resolved') === 'dark';
}

/**
 * Every link in a message opens a new tab.
 *
 * A message is a document the reader did not navigate to, and following a link inside the
 * reading pane would replace the message with the destination — there is no address bar to
 * come back from. `<base target="_blank">` does it for every anchor at once, including the
 * ones the sanitiser rewrote; the frame's `sandbox` allows the popup (and nothing else:
 * scripts and same-origin access stay denied — see `docs/security.md` §10.2).
 */
/**
 * The stylesheet every message frame gets, and it is here for the *quoted* part.
 *
 * A reply carries the message it answers, and it arrives however the other client wrapped it:
 * `<blockquote>` for most, `div.gmail_quote`, `.yahoo_quoted`, Outlook's `#appendonly`. In every
 * one of those cases it lands in the body with nothing to separate it from what the sender wrote
 * today — which is the part a reader is looking for, and the part that becomes unreadable when a
 * wall of older text looks exactly like it. One muted left edge says "the older message starts
 * here"; the colour says where it ends.
 */
const FRAME_QUOTE_CSS = `<style>
  blockquote, div.gmail_quote, div.yahoo_quoted, #appendonly {
    margin: 12px 0 0 0;
    padding-left: 12px;
    border-left: 2px solid rgba(127, 127, 127, 0.45);
  }
  blockquote { color: rgba(127, 127, 127, 1); }
</style>`;

const FRAME_BASE = '<base target="_blank">' + FRAME_QUOTE_CSS;

/** A message body ready for `srcdoc`, in the colour scheme currently in force. */
function frameSource(html) {
  return (darkMode() ? DARK_MESSAGE_CSS : '') + FRAME_BASE + html;
}

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
  byId('action-source').addEventListener('click', () => withMessage(openSourceDialog));
  byId('action-delete').addEventListener('click', () => withMessage(deleteMessage));

  byId('tab-html').addEventListener('click', () => {
    preferredPart = 'html';
    renderBody();
  });
  byId('tab-text').addEventListener('click', () => {
    preferredPart = 'text';
    renderBody();
  });

  // The HTML frame is sandboxed to an opaque origin, so `contentDocument` is `null`
  // and its content height cannot be read from here. Its size therefore comes from CSS;
  // see the module comment above.

  // The dark-mode transform is part of the frame's `srcdoc`, so a theme change has to
  // rebuild it; nothing else would repaint the message.
  onThemeChange(() => {
    if (getState().selected) renderBody();
  });
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
    // Opening a message *is* reading it. This used to wait two seconds and only count if
    // the pane was still on screen, which left a message the reader had plainly opened
    // — and read — sitting in the list with its unread dot.
    markSeen(id);
  } catch (error) {
    mutate((draft) => {
      draft.readerLoading = false;
    });
    setText(
      byId('reader-error'),
      error instanceof ApiError ? error.message : t('This message could not be loaded.'),
    );
    setHidden(byId('reader-error'), false);
    renderChrome();
  }
}

/** Close the reading pane (back button on narrow screens). */
export function closeReader() {
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
  labelWithTitle(subject, message.subject || t('(no subject)'), t('Subject'));

  const from = byId('reader-from');
  labelWithTitle(from, message.from || t('(unknown sender)'), t('From'));

  const to = byId('reader-to');
  labelWithTitle(to, message.to.join(', ') || t('(no recipients)'), t('To'));

  const ccLine = byId('reader-cc-line');
  setHidden(ccLine, message.cc.length === 0);
  if (message.cc.length) labelWithTitle(byId('reader-cc'), message.cc.join(', '), t('Cc'));

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
      'aria-label': t('Download {filename}, {size}', {
        filename: attachment.filename,
        size: formatBytes(attachment.sizeBytes),
      }),
      title: t('{filename} — {size}', {
        filename: attachment.filename,
        size: formatBytes(attachment.sizeBytes),
      }),
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
  setText(byId('tab-html'), t('Rich text'));
  setText(byId('tab-text'), t('Plain text'));
  byId('tab-html').setAttribute('aria-pressed', showHtml ? 'true' : 'false');
  byId('tab-text').setAttribute('aria-pressed', showHtml ? 'false' : 'true');

  if (showHtml) {
    setHidden(textNode, true);
    setHidden(frame, false);
    const html = frameSource(message.html);
    if (frame.getAttribute('srcdoc') !== html) frame.setAttribute('srcdoc', html);
    return;
  }

  setHidden(frame, true);
  frame.removeAttribute('srcdoc');
  setHidden(textNode, false);
  textNode.textContent =
    (hasText ? message.text : '') ||
    (hasHtml ? t('This message has an HTML body only.') : t('This message has no body.'));
}

/** Header bits that depend on the selected message (star state, archive). */
export function renderChrome() {
  const state = getState();
  const message = state.selected;
  const star = byId('action-star');
  star.setAttribute('aria-pressed', message && message.flagged ? 'true' : 'false');
  // The label lives in its own span: assigning to the button's `textContent` would delete
  // the icon beside it, which is exactly how this row lost its glyphs once before.
  const starLabel = star.querySelector('.btn-label');
  if (starLabel) setText(starLabel, message && message.flagged ? t('Unstar') : t('Star'));
  star.disabled = !message;
  byId('action-unread').disabled = !message;
  byId('action-archive').disabled = !message;
  byId('action-move').disabled = !message;
  byId('action-source').disabled = !message;
  byId('action-delete').disabled = !message;
  byId('action-reply').disabled = !message;
  byId('action-reply-all').disabled = !message;
  byId('action-forward').disabled = !message;
}

/* ---------------------------------------------------------- auto mark as read */

/**
 * Mark a message read, the moment it is opened.
 *
 * The preference is a courtesy for readers who treat the pane as a preview and want a
 * message to stay unread until they act on it; everything else about it is immediate.
 * There is no timer and no visibility test any more — a message the reader has opened
 * and read has been read, and the two-second delay only produced rows that stayed
 * unread while the message behind them was plainly on screen.
 *
 * @param {number} id
 */
async function markSeen(id) {
  if (!getPrefs().markReadOnOpen) return;
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
    toastSuccess(next ? t('Starred.') : t('Star removed.'));
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : t('The star could not be changed.'));
  }
}

async function markUnread(message) {
  try {
    await request(`${API_BASE}/messages/${message.id}`, {
      method: 'PATCH',
      body: { seen: false },
      toast: false,
    });
    mutate((draft) => {
      draft.selected = Object.assign({}, draft.selected, { seen: false });
      draft.messages = draft.messages.map((candidate) =>
        candidate.id === message.id ? Object.assign({}, candidate, { seen: false }) : candidate,
      );
    });
    renderReader();
    renderChrome();
    if (handlers) handlers.onAfterChange();
    toastSuccess(t('Marked unread.'));
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : t('The message could not be updated.'));
  }
}

async function archive(message) {
  const state = getState();
  const archiveFolder = state.folders.find((folder) => folder.specialUse === 'archive');
  if (!archiveFolder) {
    toastError(t('This mailbox has no Archive folder.'));
    return;
  }
  await moveTo(message, archiveFolder, t('Archived.'));
}

/**
 * Show the message exactly as it is stored — RFC 5322 headers and all.
 *
 * This is the one place a message body is displayed without any interpretation: the
 * bytes go into a `<pre>` through `textContent`, so a header an attacker controls
 * cannot become markup. It is also what makes a suspicious message diagnosable, since
 * the reading pane deliberately shows only the decoded, sanitised parts.
 */
async function openSourceDialog(message) {
  let text;
  try {
    const response = await request(`${API_BASE}/messages/${message.id}/raw`, { raw: true, toast: false });
    text = await response.text();
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : t('The message source could not be read.'));
    return;
  }

  const pre = el('pre', { class: 'reader-text source-view', tabindex: '0', text });
  const downloadButton = el('button', { type: 'button', class: 'btn', text: t('Download .eml') });
  const closeButton = el('button', { type: 'button', class: 'btn btn-primary', text: t('Close') });

  const modal = openModal({
    title: message.subject ? t('Source — {subject}', { subject: message.subject }) : t('Message source'),
    body: el('div', {}, [
      el('p', {
        class: 'modal-message',
        text: t('The stored bytes: every header, including the ones the reading pane hides.'),
      }),
      pre,
    ]),
    footer: [downloadButton, el('span', { class: 'spacer' }), closeButton],
    size: 'wide',
    onMount: () => {
      closeButton.addEventListener('click', () => modal.close('close'));
      downloadButton.addEventListener('click', () => {
        download(`${API_BASE}/messages/${message.id}/raw`, `message-${message.id}.eml`);
      });
    },
  });
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
    toastError(error instanceof ApiError ? error.message : t('The message could not be moved.'));
  }
}

async function deleteMessage(message) {
  const state = getState();
  const trash = state.folders.find((folder) => folder.specialUse === 'trash');
  const alreadyInTrash = Boolean(state.folder && trash && state.folder.id === trash.id);

  if (alreadyInTrash) {
    const confirmed = await confirmDialog({
      title: t('Delete permanently'),
      message: t('“{subject}” will be removed for good. This cannot be undone.', {
        subject: message.subject,
      }),
      confirmLabel: t('Delete permanently'),
    });
    if (!confirmed) return;
    try {
      await request(`${API_BASE}/messages/${message.id}?permanent=true`, { method: 'DELETE', toast: false });
      afterRemoval(message.id);
      toastSuccess(t('Message deleted permanently.'));
    } catch (error) {
      toastError(error instanceof ApiError ? error.message : t('The message could not be deleted.'));
    }
    return;
  }

  const confirmed = await confirmDialog({
    title: t('Move to Trash'),
    message: t('“{subject}” will be moved to Trash.', { subject: message.subject }),
    confirmLabel: t('Move to Trash'),
  });
  if (!confirmed) return;
  try {
    await request(`${API_BASE}/messages/${message.id}`, { method: 'DELETE', toast: false });
    afterRemoval(message.id);
    toastSuccess(t('Moved to Trash.'));
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : t('The message could not be deleted.'));
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
    toastError(t('There is no other folder to move this message to.'));
    return;
  }

  const selectId = 'move-target-select';
  const select = el('select', { class: 'input', id: selectId, size: String(Math.min(others.length, 8)) });
  for (const folder of others) {
    select.append(el('option', { value: String(folder.id), text: folder.name }));
  }
  const body = el('div', {}, [
    el('p', { class: 'modal-message', text: t('Move “{subject}” to:', { subject: message.subject }) }),
    el('label', { class: 'visually-hidden', for: selectId, text: t('Destination folder') }),
    select,
  ]);
  const cancel = el('button', { type: 'button', class: 'btn', text: t('Cancel') });
  const confirm = el('button', { type: 'button', class: 'btn btn-primary', text: t('Move') });

  const modal = openModal({
    title: t('Move message'),
    body,
    footer: [cancel, confirm],
    onMount: () => {
      cancel.addEventListener('click', () => modal.close('cancel'));
      confirm.addEventListener('click', () => {
        const folder = others.find((candidate) => candidate.id === Number.parseInt(select.value, 10));
        modal.close('confirm');
        if (folder) moveTo(message, folder, t('Moved to {folder}.', { folder: folder.name }));
      });
    },
  });
}

/** Stream an attachment to disk through the authenticated request path. */
async function downloadAttachment(attachment) {
  try {
    await download(`${API_BASE}/attachments/${attachment.id}`, attachment.filename);
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : t('The attachment could not be downloaded.'));
  }
}
