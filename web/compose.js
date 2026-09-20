/**
 * Compose window: a modal dialog (or a full-height panel on narrow screens) with
 * address chips, a `contenteditable` rich-text body that also yields a plain-text
 * part, attachment uploads with a progress bar, and reply / reply-all / forward
 * prefill including the quoted body and `references`.
 *
 * The API of record is `POST /api/v1/messages` for sending and `POST /api/v1/drafts`
 * for a draft, which the server mirrors into the `Drafts` folder — see `README.md`,
 * "Endpoints wired".
 */

import { API_BASE, ApiError, request } from '../shared/api.js';
import { createChipField } from './address.js';
import { byId, clear, el, setText } from '../shared/dom.js';
import {
  forwardSubject,
  fullStamp,
  htmlToText,
  quoteHtml,
  replyAllRecipients,
  replySubject,
  textToHtml,
} from '../shared/format.js';
import { t, tn } from '../shared/i18n.js';
import { confirmDialog, openModal, promptDialog } from '../shared/modal.js';
import { getPrefs, getState } from './store.js';
import { toastError, toastSuccess } from '../shared/toast.js';

const MAX_UPLOAD_BYTES = 25 * 1024 * 1024;

/**
 * @typedef {object} ComposeSeed
 * @property {'new'|'reply'|'reply-all'|'forward'|'draft'} mode
 * @property {object} [message] the message being answered
 * @property {number} [draftId] the draft record this window continues
 * @property {string[]} [to]
 * @property {string[]} [cc]
 * @property {string[]} [bcc]
 * @property {string} [subject]
 * @property {string} [text]
 * @property {string} [html]
 */

/** @type {null | (() => void)} */
let onSent = null;
/** @type {null | {close: (reason?: string) => void}} */
let openForm = null;

/** @param {() => void} handler */
export function setSentHandler(handler) {
  onSent = handler;
}

/** Is the compose window open? */
export function isOpen() {
  return openForm !== null;
}

/** Close the compose window from outside. */
export function closeCompose(reason = 'programmatic') {
  if (openForm) openForm.close(reason);
}

/** The compose window is a modal except on narrow screens, where it is a panel. */
function standalone() {
  return window.matchMedia('(max-width: 900px)').matches;
}

/**
 * Open the compose window.
 * @param {ComposeSeed} seed
 */
export function openCompose(seed) {
  const state = getState();
  const mode = seed.mode || 'new';
  const source = seed.message || null;

  /* ------------------------------------------------------------------ fields */

  const fromSelect = el('select', { class: 'input', id: 'compose-from' });
  const mailboxes = state.mailboxes.length
    ? state.mailboxes
    : [{ id: state.mailboxId, address: state.user ? state.user.email : '', isPrimary: true }];
  for (const mailbox of mailboxes) {
    fromSelect.append(el('option', { value: String(mailbox.id), text: mailbox.address }));
  }
  fromSelect.value = String(state.mailboxId || (mailboxes[0] ? mailboxes[0].id : 0));

  const onInvalid = (message) => toastError(message);
  const to = createChipField({ id: 'compose-to', label: t('To'), placeholder: 'name@example.com', onInvalid });
  const cc = createChipField({ id: 'compose-cc', label: t('Cc'), placeholder: 'name@example.com', onInvalid });
  const bcc = createChipField({ id: 'compose-bcc', label: t('Bcc'), placeholder: 'name@example.com', onInvalid });

  const ccRow = el('div', {}, [cc.root]);
  ccRow.hidden = true;
  const bccRow = el('div', {}, [bcc.root]);
  bccRow.hidden = true;

  const subject = el('input', { class: 'input', id: 'compose-subject', type: 'text', autocomplete: 'off' });

  const editor = el('div', {
    class: 'editor',
    id: 'compose-editor',
    contenteditable: 'true',
    role: 'textbox',
    'aria-multiline': 'true',
    'aria-labelledby': 'compose-body-label',
    spellcheck: 'true',
  });

  // A `contenteditable` with no block element refuses the list and quote commands, and
  // leaves a collapsed caret in a state where `bold` can claim the whole text node. One
  // empty paragraph gives every command a block to work on.
  editor.append(el('div', {}, [el('br')]));

  const toolbar = el('div', { class: 'editor-toolbar', role: 'toolbar', 'aria-label': t('Formatting') });
  const editorWrap = el('div', {}, [
    el('span', { class: 'field-label', id: 'compose-body-label', text: t('Message') }),
    toolbar,
    editor,
  ]);

  const attachmentsInput = el('input', {
    class: 'visually-hidden',
    id: 'compose-files',
    type: 'file',
    multiple: true,
  });
  const uploads = el('ul', { class: 'uploads', id: 'compose-uploads', 'aria-live': 'polite' });
  const attachButton = el('button', { type: 'button', class: 'btn btn-small', text: t('Attach files') });

  const plainToggle = el('button', { type: 'button', class: 'btn btn-small', 'aria-pressed': 'false', text: t('Plain text') });
  const fieldsToggle = el('button', { type: 'button', class: 'btn btn-small', 'aria-pressed': 'false', text: t('Cc / Bcc') });
  const status = el('p', { class: 'field-error', id: 'compose-status', role: 'alert' });
  status.hidden = true;

  /* ---------------------------------------------------------------- prefill */

  const selfAddresses = mailboxes.map((mailbox) => mailbox.address).filter(Boolean);

  if (source && (mode === 'reply' || mode === 'reply-all')) {
    const recipients =
      mode === 'reply-all' ? replyAllRecipients(source, selfAddresses) : { to: [source.from], cc: [] };
    to.setValues(recipients.to);
    if (recipients.cc.length) {
      cc.setValues(recipients.cc);
      ccRow.hidden = false;
    }
    subject.value = replySubject(source.subject);
  } else if (source && mode === 'forward') {
    subject.value = forwardSubject(source.subject);
  }

  if (seed.to) to.setValues(seed.to);
  if (seed.cc && seed.cc.length) {
    cc.setValues(seed.cc);
    ccRow.hidden = false;
  }
  if (seed.bcc && seed.bcc.length) {
    bcc.setValues(seed.bcc);
    bccRow.hidden = false;
    fieldsToggle.setAttribute('aria-pressed', 'true');
  }
  if (seed.subject !== undefined) subject.value = seed.subject;

  if (mode === 'forward' && source) {
    const header = t(
      '---------- Forwarded message ----------\nFrom: {from}\nDate: {date}\nSubject: {subject}\nTo: {to}\n\n',
      {
        from: source.from,
        date: fullStamp(source.date),
        subject: source.subject,
        to: source.to.join(', '),
      },
    );
    editor.innerHTML =
      textToHtml(header) + (source.html && source.html.trim() ? source.html : textToHtml(source.text));
  } else if ((mode === 'reply' || mode === 'reply-all') && source) {
    editor.innerHTML = quoteHtml(source);
  } else if (seed.html !== undefined) {
    editor.innerHTML = seed.html;
  } else if (seed.text !== undefined) {
    editor.innerHTML = textToHtml(seed.text);
  }

  const signature = getPrefs().signature.trim();
  if (signature) {
    editor.innerHTML = `${editor.innerHTML}<p><br></p><p>-- </p>${textToHtml(signature)}`;
  }

  /** @type {string} the rich-text body, kept while the plain-text view is active */
  let richHtml = editor.innerHTML;

  /* ---------------------------------------------------------------- toolbar */

  const syncEditorHeight = () => {
    editor.style.height = 'auto';
    editor.style.height = `${Math.min(Math.max(editor.scrollHeight, 200), 460)}px`;
  };

  /**
   * The editor's HTML, with the empty-block scaffolding treated as empty.
   *
   * An editor that was never touched still holds `<div><br></div>`; sending that would
   * put a body on a message the reader deliberately left blank.
   */
  const editorHtml = () => {
    if ((editor.textContent || '').trim() === '' && editor.querySelector('img, hr, table') === null) {
      return '';
    }
    return editor.innerHTML.trim();
  };

  /*
   * The selection is saved and restored around every command.
   *
   * A toolbar button, and above all a dialog, takes the focus; `execCommand` then acts on
   * whatever selection is left — a collapsed caret at best, and at worst the whole
   * editor, which is how one click of "bold" could bold everything. Restoring the range
   * the reader actually had is what makes the buttons mean what they say.
   */
  let savedRange = null;

  const saveSelection = () => {
    const selection = window.getSelection();
    if (!selection || selection.rangeCount === 0) return;
    const range = selection.getRangeAt(0);
    if (editor.contains(range.commonAncestorContainer)) savedRange = range.cloneRange();
  };

  const restoreSelection = () => {
    editor.focus();
    if (!savedRange) return;
    const selection = window.getSelection();
    if (!selection) return;
    selection.removeAllRanges();
    selection.addRange(savedRange);
  };

  const pressedButtons = [];

  /** Reflect bold/italic/underline state on the toolbar, so the buttons are readable. */
  const syncToolbarState = () => {
    for (const { button, command } of pressedButtons) {
      let on = false;
      try {
        on = document.queryCommandState(command);
      } catch {
        on = false;
      }
      button.setAttribute('aria-pressed', on ? 'true' : 'false');
    }
  };

  const afterCommand = () => {
    richHtml = editor.innerHTML;
    saveSelection();
    syncEditorHeight();
    syncToolbarState();
  };

  const exec = (command, value) => {
    restoreSelection();
    try {
      document.execCommand(command, false, value);
    } catch {
      toastError(t('This browser refused that formatting command.'));
      return;
    }
    afterCommand();
  };

  /**
   * Clear every format the editor can apply, not just the inline three.
   *
   * `removeFormat` undoes bold/italic/underline/colour; a list and a quote are *block*
   * formats and survive it, which is why "Clear" used to leave a bulleted line bulleted.
   */
  const clearFormatting = () => {
    restoreSelection();
    try {
      document.execCommand('removeFormat');
      document.execCommand('unlink');
      if (document.queryCommandState('insertUnorderedList')) document.execCommand('insertUnorderedList');
      if (document.queryCommandState('insertOrderedList')) document.execCommand('insertOrderedList');
      document.execCommand('formatBlock', false, 'div');
    } catch {
      toastError(t('This browser refused that formatting command.'));
      return;
    }
    afterCommand();
  };

  /** Escape a URL for the one place it becomes markup, in the editor only. */
  const escapeAttribute = (value) =>
    String(value)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');

  const toolbarSpec = [
    { label: t('Bold'), command: 'bold', text: 'B', pressed: true },
    { label: t('Italic'), command: 'italic', text: 'I', pressed: true },
    { label: t('Underline'), command: 'underline', text: 'U', pressed: true },
    { label: t('Bulleted list'), command: 'insertUnorderedList', text: t('• List') },
    { label: t('Numbered list'), command: 'insertOrderedList', text: t('1. List') },
    { label: t('Quote'), command: 'formatBlock', value: 'blockquote', text: t('Quote') },
    { label: t('Remove formatting'), command: 'removeFormat', text: t('Clear'), clear: true },
  ];
  for (const item of toolbarSpec) {
    const button = el('button', {
      type: 'button',
      class: 'btn btn-small',
      text: item.text,
      title: item.label,
      'aria-label': item.label,
      'aria-pressed': 'false',
    });
    // Keeping the mousedown from moving focus preserves the selection; the range itself
    // is saved as well, because a dialog cannot help stealing it.
    button.addEventListener('mousedown', (event) => event.preventDefault());
    button.addEventListener('click', () => {
      if (item.clear) clearFormatting();
      else exec(item.command, item.value);
    });
    if (item.pressed) pressedButtons.push({ button, command: item.command });
    toolbar.append(button);
  }

  const linkButton = el('button', {
    type: 'button',
    class: 'btn btn-small',
    text: t('Link'),
    'aria-label': t('Insert link'),
  });
  linkButton.addEventListener('mousedown', (event) => event.preventDefault());
  linkButton.addEventListener('click', async () => {
    saveSelection();
    const selection = window.getSelection();
    const hasText = Boolean(selection && selection.toString().trim() !== '');
    // The app's own dialog, not `window.prompt`: the native prompt cannot be styled, and
    // it returns focus to a page state in which the selection is gone.
    const url = await promptDialog({
      title: t('Insert link'),
      label: t('Link address (https://…)'),
      confirmLabel: t('Add'),
      value: 'https://',
      placeholder: 'https://example.com',
      hint: hasText
        ? t('The link is added to the text you selected.')
        : t('Nothing is selected, so the address becomes the link text.'),
    });
    if (!url) return;
    if (!/^https?:\/\//i.test(url)) {
      toastError(t('Links must start with http:// or https://.'));
      return;
    }
    if (hasText) {
      exec('createLink', url);
      return;
    }
    // Nothing selected: the address becomes the link text, which is what a reader who
    // pastes a URL into an empty line meant.
    restoreSelection();
    try {
      const safe = escapeAttribute(url);
      document.execCommand('insertHTML', false, `<a href="${safe}">${safe}</a>`);
    } catch {
      document.execCommand('insertText', false, url);
    }
    afterCommand();
  });
  toolbar.append(linkButton);

  document.addEventListener('selectionchange', () => {
    saveSelection();
    syncToolbarState();
  });
  editor.addEventListener('keyup', syncToolbarState);
  editor.addEventListener('mouseup', syncToolbarState);

  /* ------------------------------------------------------------ attachments */

  /** @type {Array<{id: number, filename: string}>} */
  const uploadedAttachments = [];

  const uploadFile = (file) => {
    const bar = el('span');
    const progress = el('span', { class: 'upload-bar' }, [bar]);
    const stateText = el('span', { class: 'upload-state', text: '0%' });
    const remove = el('button', {
      type: 'button',
      class: 'btn btn-small',
      text: t('Remove'),
      'aria-label': t('Remove {name}', { name: file.name }),
    });
    const row = el('li', { class: 'upload-row' }, [
      el('span', { class: 'upload-name', title: file.name, text: file.name }),
      progress,
      stateText,
      remove,
    ]);
    uploads.append(row);

    const detach = () => {
      const index = uploadedAttachments.findIndex((item) => item.filename === file.name);
      if (index >= 0) uploadedAttachments.splice(index, 1);
      row.remove();
    };

    if (file.size > MAX_UPLOAD_BYTES) {
      stateText.textContent = t('too large');
      remove.addEventListener('click', detach);
      toastError(t('{name} is larger than the 25 MB this form accepts.', { name: file.name }));
      return;
    }

    const body = new FormData();
    body.append('file', file, file.name);
    const xhr = new XMLHttpRequest();
    xhr.open('POST', `${API_BASE}/attachments`);
    xhr.withCredentials = true;
    let token = null;
    try {
      token = window.localStorage.getItem('ferroma.access_token');
    } catch {
      token = null;
    }
    if (token) xhr.setRequestHeader('Authorization', `Bearer ${token}`);

    xhr.upload.addEventListener('progress', (event) => {
      if (!event.lengthComputable) return;
      const percent = Math.round((event.loaded / event.total) * 100);
      bar.style.width = `${percent}%`;
      stateText.textContent = `${percent}%`;
    });

    xhr.addEventListener('load', () => {
      let payload = null;
      try {
        payload = JSON.parse(xhr.responseText);
      } catch {
        payload = null;
      }
      if (xhr.status >= 200 && xhr.status < 300 && payload && payload.id) {
        bar.style.width = '100%';
        stateText.textContent = t('uploaded');
        uploadedAttachments.push({ id: Number(payload.id), filename: String(payload.filename || file.name) });
        remove.textContent = t('Detach');
        remove.addEventListener('click', detach);
        return;
      }
      stateText.textContent = t('failed');
      const message =
        payload && payload.error && payload.error.message
          ? payload.error.message
          : t('Upload failed (HTTP {status}).', { status: xhr.status });
      toastError(`${file.name}: ${message}`);
      remove.addEventListener('click', detach);
    });

    xhr.addEventListener('error', () => {
      stateText.textContent = t('failed');
      toastError(t('{name} could not be uploaded — the server was unreachable.', { name: file.name }));
      remove.addEventListener('click', detach);
    });

    xhr.send(body);
  };

  attachButton.addEventListener('click', () => attachmentsInput.click());
  attachmentsInput.addEventListener('change', () => {
    for (const file of Array.from(attachmentsInput.files || [])) uploadFile(file);
    attachmentsInput.value = '';
  });

  /* ----------------------------------------------------------------- toggles */

  let plainMode = false;
  plainToggle.addEventListener('click', () => {
    if (!plainMode) {
      richHtml = editor.innerHTML;
      editor.textContent = htmlToText(editor.innerHTML);
      editor.classList.add('editor-plain');
    } else {
      editor.innerHTML = richHtml;
      editor.classList.remove('editor-plain');
    }
    plainMode = !plainMode;
    toolbar.hidden = plainMode;
    plainToggle.setAttribute('aria-pressed', plainMode ? 'true' : 'false');
    plainToggle.textContent = plainMode ? t('Rich text') : t('Plain text');
    syncEditorHeight();
  });

  fieldsToggle.addEventListener('click', () => {
    const showing = !ccRow.hidden;
    ccRow.hidden = showing;
    bccRow.hidden = showing;
    fieldsToggle.setAttribute('aria-pressed', showing ? 'false' : 'true');
  });

  editor.addEventListener('input', () => {
    if (!plainMode) richHtml = editor.innerHTML;
    syncEditorHeight();
  });

  /* -------------------------------------------------------------------- form */

  const form = el('form', { id: 'compose-form', novalidate: true }, [
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: 'compose-from', text: t('From') }),
      fromSelect,
    ]),
    to.root,
    el('div', { class: 'compose-tabs' }, [fieldsToggle, plainToggle]),
    ccRow,
    bccRow,
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: 'compose-subject', text: t('Subject') }),
      subject,
    ]),
    editorWrap,
    el('div', { class: 'row' }, [attachButton, attachmentsInput]),
    uploads,
    status,
  ]);

  // The button is rendered into the dialog footer, which is a *sibling* of
  // `<form id="compose-form">` — `modal.js` appends `.modal-body` and `.modal-foot`
  // as separate children of the card, and the standalone layout does the same. A
  // `type="submit"` button that is neither a descendant of the form nor associated
  // by the `form` attribute is inert: the browser discards the click, no `submit`
  // event fires, and the handler below never runs, so Send looked dead.
  const send = el('button', {
    type: 'submit',
    form: 'compose-form',
    class: 'btn btn-primary',
    text: t('Send'),
  });
  const saveDraft = el('button', { type: 'button', class: 'btn', text: t('Save draft') });
  const discard = el('button', { type: 'button', class: 'btn', text: t('Discard') });

  let dirty = false;
  editor.addEventListener('input', () => {
    dirty = true;
  });
  subject.addEventListener('input', () => {
    dirty = true;
  });

  const showStatus = (message) => {
    setText(status, message);
    status.hidden = message === '';
  };

  const collect = () => {
    const selected = fromSelect.options[fromSelect.selectedIndex];
    return {
      from: selected ? selected.text : '',
      mailboxId: Number.parseInt(fromSelect.value, 10),
      to: to.getValues(),
      cc: cc.getValues(),
      bcc: bcc.getValues(),
      subject: subject.value.trim(),
      html: plainMode ? textToHtml(editor.textContent || '') : editorHtml(),
      text: htmlToText(plainMode ? editor.textContent || '' : editorHtml()),
    };
  };

  const buildPayload = (values, extra) =>
    Object.assign(
      {
        from: values.from,
        to: values.to,
        subject: values.subject,
        text: values.text,
        html: values.html === '' ? undefined : values.html,
        attachments: uploadedAttachments.map((item) => item.id),
      },
      values.cc.length ? { cc: values.cc } : {},
      values.bcc.length ? { bcc: values.bcc } : {},
      extra,
    );

  /**
   * The `POST /api/v1/drafts` body.
   *
   * A draft is a *record*, not a message that happens to be unsent: it is what the
   * desktop client reads through `GET /api/v1/drafts` and what the sync journal
   * reports, and the server mirrors it into the `Drafts` folder so an IMAP client sees
   * the same thing. Posting to `/messages` with `draft: true` filed a message in the
   * folder and left the record behind, which is why a draft written here was invisible
   * to every other client.
   */
  const buildDraftPayload = (values, extra) =>
    Object.assign(
      {
        mailbox_id: Number.isFinite(values.mailboxId) && values.mailboxId > 0 ? values.mailboxId : undefined,
        subject: values.subject,
        text: values.text,
        html: values.html === '' ? undefined : values.html,
        to: values.to,
        cc: values.cc,
        bcc: values.bcc,
        attachment_ids: uploadedAttachments.map((item) => item.id),
      },
      extra,
    );

  /**
   * Reply/forward threading headers.
   *
   * A function rather than a local in one handler because both Send and Save
   * draft need it: the draft handler referenced a binding that existed only
   * inside the submit handler's scope, so every "Save draft" click threw
   * `ReferenceError: extra is not defined` and reported a generic failure.
   */
  const threadingExtra = () => {
    const extra = {};
    if (!source) return extra;
    const raw = source.raw || {};
    const header = raw.message_id_header || raw.rfc_message_id || raw.message_id_header_value;
    if (typeof header === 'string' && header !== '') extra.in_reply_to = header;
    const references = Array.isArray(raw.references)
      ? raw.references.slice()
      : typeof raw.references === 'string' && raw.references
        ? raw.references.split(/\s+/).filter(Boolean)
        : [];
    if (extra.in_reply_to) references.push(extra.in_reply_to);
    if (references.length) extra.references = references;
    return extra;
  };

  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    showStatus('');
    if (!to.commitPending() || !cc.commitPending() || !bcc.commitPending()) {
      showStatus(t('One of the addresses is not valid.'));
      return;
    }
    const values = collect();
    if (values.to.length === 0) {
      to.focus();
      showStatus(t('Add at least one recipient in To.'));
      return;
    }

    const extra = threadingExtra();

    send.disabled = true;
    saveDraft.disabled = true;
    setText(send, t('Sending…'));
    try {
      // `POST /drafts/:id/send` takes the same body and falls back to the stored draft for
      // anything omitted, so continuing a draft and sending it is one call — and the draft
      // record goes away with it, instead of staying behind as a copy of the sent message.
      const target = draftId ? `${API_BASE}/drafts/${draftId}/send` : `${API_BASE}/messages`;
      const payload = await request(target, {
        method: 'POST',
        body: draftId ? buildDraftPayload(values, extra) : buildPayload(values, extra),
        toast: false,
      });
      const queued = payload && payload.queued !== undefined ? Number(payload.queued) : 0;
      const recipients = payload && Array.isArray(payload.recipients) ? payload.recipients : values.to;
      toastSuccess(
        queued > 0
          ? tn(
              queued,
              'Queued for {count} recipient: {recipients}',
              'Queued for {count} recipients: {recipients}',
              { count: queued, recipients: recipients.join(', ') },
            )
          : t('Message sent.'),
      );
      dirty = false;
      closeCompose('sent');
      if (onSent) onSent();
    } catch (error) {
      const message = error instanceof ApiError ? error.message : t('The message could not be sent.');
      showStatus(message);
      toastError(message);
    } finally {
      send.disabled = false;
      saveDraft.disabled = false;
      setText(send, t('Send'));
    }
  });

  saveDraft.addEventListener('click', async () => {
    showStatus('');
    if (!to.commitPending()) {
      showStatus(t('One of the addresses is not valid.'));
      return;
    }
    const values = collect();
    if (values.subject === '' && values.text === '' && values.to.length === 0) {
      showStatus(t('Write something before saving a draft.'));
      return;
    }
    saveDraft.disabled = true;
    setText(saveDraft, t('Saving…'));
    try {
      await request(draftId ? `${API_BASE}/drafts/${draftId}` : `${API_BASE}/drafts`, {
        method: draftId ? 'PATCH' : 'POST',
        body: buildDraftPayload(values, threadingExtra()),
        toast: false,
      });
      toastSuccess(t('Draft saved to the Drafts folder.'));
      dirty = false;
      closeCompose('draft');
      if (onSent) onSent();
    } catch (error) {
      const message = error instanceof ApiError ? error.message : t('The draft could not be saved.');
      showStatus(message);
      toastError(message);
    } finally {
      saveDraft.disabled = false;
      setText(saveDraft, t('Save draft'));
    }
  });

  // A draft is a message being continued, not a new one: the window says so.
  const draftId = Number(seed.draftId) || 0;
  const title = draftId
    ? t('Edit draft')
    : mode === 'new'
      ? t('New message')
      : mode === 'forward'
        ? t('Forward')
        : t('Reply');

  /** Ask before throwing away unsaved text; `reason` is informational. */
  const requestClose = async (reason) => {
    if (dirty) {
      const confirmed = await confirmDialog({
        title: t('Discard this message?'),
        message: t('What you have written will be lost.'),
        confirmLabel: t('Discard'),
      });
      if (!confirmed) return;
    }
    dirty = false;
    closeCompose(reason);
  };

  discard.addEventListener('click', () => requestClose('discard'));

  if (standalone()) {
    const back = el('button', { type: 'button', class: 'btn btn-small', text: t('Back to list') });
    const panel = el('section', { class: 'reader', id: 'compose-panel' }, [
      el('header', { class: 'reader-head' }, [back, el('h2', { class: 'reader-subject', text: title })]),
      el('div', { class: 'reader-body' }, [form]),
      el('div', { class: 'modal-foot' }, [discard, saveDraft, send]),
    ]);
    const host = byId('compose-host');
    clear(host);
    host.append(panel);
    host.hidden = false;
    const listPane = byId('list-pane');
    listPane.hidden = true;
    document.body.classList.add('no-scroll');

    openForm = {
      close() {
        host.hidden = true;
        clear(host);
        listPane.hidden = false;
        document.body.classList.remove('no-scroll');
        openForm = null;
      },
    };
    back.addEventListener('click', () => requestClose('back'));
    syncEditorHeight();
    to.focus();
    return;
  }

  const modal = openModal({
    title,
    size: 'wide',
    body: form,
    footer: [discard, saveDraft, send],
    closeOnOutside: false,
    beforeClose: (reason) =>
      reason === 'escape'
        ? confirmDialog({
            title: t('Discard this message?'),
            message: t('What you have written will be lost.'),
            confirmLabel: t('Discard'),
          })
        : true,
    onClose: () => {
      dirty = false;
      openForm = null;
      return true;
    },
  });

  openForm = {
    close(reason) {
      modal.close(reason);
    },
  };

  syncEditorHeight();
  to.focus();
}
