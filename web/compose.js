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

import { API_BASE, ApiError, request } from './api.js';
import { createChipField } from './address.js';
import { byId, clear, el, setText } from './dom.js';
import {
  forwardSubject,
  fullStamp,
  htmlToText,
  quoteHtml,
  replyAllRecipients,
  replySubject,
  textToHtml,
} from './format.js';
import { confirmDialog, openModal } from './modal.js';
import { getPrefs, getState } from './store.js';
import { toastError, toastSuccess } from './toast.js';

const MAX_UPLOAD_BYTES = 25 * 1024 * 1024;

/**
 * @typedef {object} ComposeSeed
 * @property {'new'|'reply'|'reply-all'|'forward'} mode
 * @property {object} [message] the message being answered
 * @property {string[]} [to]
 * @property {string[]} [cc]
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
  const to = createChipField({ id: 'compose-to', label: 'To', placeholder: 'name@example.com', onInvalid });
  const cc = createChipField({ id: 'compose-cc', label: 'Cc', placeholder: 'name@example.com', onInvalid });
  const bcc = createChipField({ id: 'compose-bcc', label: 'Bcc', placeholder: 'name@example.com', onInvalid });

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

  const toolbar = el('div', { class: 'editor-toolbar', role: 'toolbar', 'aria-label': 'Formatting' });
  const editorWrap = el('div', {}, [
    el('span', { class: 'field-label', id: 'compose-body-label', text: 'Message' }),
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
  const attachButton = el('button', { type: 'button', class: 'btn btn-small', text: 'Attach files' });

  const plainToggle = el('button', { type: 'button', class: 'btn btn-small', 'aria-pressed': 'false', text: 'Plain text' });
  const fieldsToggle = el('button', { type: 'button', class: 'btn btn-small', 'aria-pressed': 'false', text: 'Cc / Bcc' });
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
  if (seed.cc) {
    cc.setValues(seed.cc);
    ccRow.hidden = false;
  }
  if (seed.subject !== undefined) subject.value = seed.subject;

  if (mode === 'forward' && source) {
    const header =
      '---------- Forwarded message ----------\n' +
      `From: ${source.from}\n` +
      `Date: ${fullStamp(source.date)}\n` +
      `Subject: ${source.subject}\n` +
      `To: ${source.to.join(', ')}\n\n`;
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

  const exec = (command, value) => {
    editor.focus();
    try {
      document.execCommand(command, false, value);
    } catch {
      toastError('This browser refused that formatting command.');
      return;
    }
    richHtml = editor.innerHTML;
    syncEditorHeight();
  };

  const toolbarSpec = [
    { label: 'Bold', command: 'bold', text: 'B' },
    { label: 'Italic', command: 'italic', text: 'I' },
    { label: 'Underline', command: 'underline', text: 'U' },
    { label: 'Bulleted list', command: 'insertUnorderedList', text: '• List' },
    { label: 'Numbered list', command: 'insertOrderedList', text: '1. List' },
    { label: 'Quote', command: 'formatBlock', value: 'blockquote', text: 'Quote' },
    { label: 'Remove formatting', command: 'removeFormat', text: 'Clear' },
  ];
  for (const item of toolbarSpec) {
    const button = el('button', {
      type: 'button',
      class: 'btn btn-small',
      text: item.text,
      title: item.label,
      'aria-label': item.label,
    });
    button.addEventListener('mousedown', (event) => event.preventDefault());
    button.addEventListener('click', () => exec(item.command, item.value));
    toolbar.append(button);
  }

  const linkButton = el('button', {
    type: 'button',
    class: 'btn btn-small',
    text: 'Link',
    'aria-label': 'Insert link',
  });
  linkButton.addEventListener('mousedown', (event) => event.preventDefault());
  linkButton.addEventListener('click', () => {
    const url = window.prompt('Link address (https://…)');
    if (!url) return;
    if (!/^https?:\/\//i.test(url)) {
      toastError('Links must start with http:// or https://.');
      return;
    }
    exec('createLink', url);
  });
  toolbar.append(linkButton);

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
      text: 'Remove',
      'aria-label': `Remove ${file.name}`,
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
      stateText.textContent = 'too large';
      remove.addEventListener('click', detach);
      toastError(`${file.name} is larger than the 25 MB this form accepts.`);
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
        stateText.textContent = 'uploaded';
        uploadedAttachments.push({ id: Number(payload.id), filename: String(payload.filename || file.name) });
        remove.textContent = 'Detach';
        remove.addEventListener('click', detach);
        return;
      }
      stateText.textContent = 'failed';
      const message =
        payload && payload.error && payload.error.message
          ? payload.error.message
          : `Upload failed (HTTP ${xhr.status}).`;
      toastError(`${file.name}: ${message}`);
      remove.addEventListener('click', detach);
    });

    xhr.addEventListener('error', () => {
      stateText.textContent = 'failed';
      toastError(`${file.name} could not be uploaded — the server was unreachable.`);
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
    plainToggle.textContent = plainMode ? 'Rich text' : 'Plain text';
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
      el('label', { class: 'field-label', for: 'compose-from', text: 'From' }),
      fromSelect,
    ]),
    to.root,
    el('div', { class: 'compose-tabs' }, [fieldsToggle, plainToggle]),
    ccRow,
    bccRow,
    el('div', { class: 'field' }, [
      el('label', { class: 'field-label', for: 'compose-subject', text: 'Subject' }),
      subject,
    ]),
    editorWrap,
    el('div', { class: 'row' }, [attachButton, attachmentsInput]),
    uploads,
    status,
  ]);

  const send = el('button', { type: 'submit', class: 'btn btn-primary', text: 'Send' });
  const saveDraft = el('button', { type: 'button', class: 'btn', text: 'Save draft' });
  const discard = el('button', { type: 'button', class: 'btn', text: 'Discard' });

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
      html: plainMode ? textToHtml(editor.textContent || '') : editor.innerHTML.trim(),
      text: htmlToText(plainMode ? editor.textContent || '' : editor.innerHTML),
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

  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    showStatus('');
    if (!to.commitPending() || !cc.commitPending() || !bcc.commitPending()) {
      showStatus('One of the addresses is not valid.');
      return;
    }
    const values = collect();
    if (values.to.length === 0) {
      to.focus();
      showStatus('Add at least one recipient in To.');
      return;
    }

    const extra = {};
    if (source) {
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
    }

    send.disabled = true;
    saveDraft.disabled = true;
    setText(send, 'Sending…');
    try {
      const payload = await request(`${API_BASE}/messages`, {
        method: 'POST',
        body: buildPayload(values, extra),
        toast: false,
      });
      const queued = payload && payload.queued !== undefined ? Number(payload.queued) : 0;
      const recipients = payload && Array.isArray(payload.recipients) ? payload.recipients : values.to;
      toastSuccess(
        queued > 0
          ? `Queued for ${queued} recipient${queued === 1 ? '' : 's'}: ${recipients.join(', ')}`
          : 'Message sent.',
      );
      dirty = false;
      closeCompose('sent');
      if (onSent) onSent();
    } catch (error) {
      const message = error instanceof ApiError ? error.message : 'The message could not be sent.';
      showStatus(message);
      toastError(message);
    } finally {
      send.disabled = false;
      saveDraft.disabled = false;
      setText(send, 'Send');
    }
  });

  saveDraft.addEventListener('click', async () => {
    showStatus('');
    if (!to.commitPending()) {
      showStatus('One of the addresses is not valid.');
      return;
    }
    const values = collect();
    if (values.subject === '' && values.text === '' && values.to.length === 0) {
      showStatus('Write something before saving a draft.');
      return;
    }
    saveDraft.disabled = true;
    setText(saveDraft, 'Saving…');
    try {
      await request(`${API_BASE}/drafts`, {
        method: 'POST',
        body: buildDraftPayload(values, extra),
        toast: false,
      });
      toastSuccess('Draft saved to the Drafts folder.');
      dirty = false;
      closeCompose('draft');
      if (onSent) onSent();
    } catch (error) {
      const message = error instanceof ApiError ? error.message : 'The draft could not be saved.';
      showStatus(message);
      toastError(message);
    } finally {
      saveDraft.disabled = false;
      setText(saveDraft, 'Save draft');
    }
  });

  const title = mode === 'new' ? 'New message' : mode === 'forward' ? 'Forward' : 'Reply';

  /** Ask before throwing away unsaved text; `reason` is informational. */
  const requestClose = async (reason) => {
    if (dirty) {
      const confirmed = await confirmDialog({
        title: 'Discard this message?',
        message: 'What you have written will be lost.',
        confirmLabel: 'Discard',
      });
      if (!confirmed) return;
    }
    dirty = false;
    closeCompose(reason);
  };

  discard.addEventListener('click', () => requestClose('discard'));

  if (standalone()) {
    const back = el('button', { type: 'button', class: 'btn btn-small', text: 'Back to list' });
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
            title: 'Discard this message?',
            message: 'What you have written will be lost.',
            confirmLabel: 'Discard',
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
