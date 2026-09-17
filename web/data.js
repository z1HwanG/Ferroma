/**
 * Normalisation of API payloads.
 *
 * `docs/api.md` freezes the endpoint surface but not every field name inside a
 * list item, so each normaliser accepts the plausible spellings for a value and
 * keeps the untouched payload under `raw`. UI code only ever reads the
 * normalised shape, which is why one field being named differently upstream
 * cannot produce `undefined` in the interface.
 */

/** First defined value among the candidate keys. */
function pick(source, keys, fallback) {
  if (source && typeof source === 'object') {
    for (const key of keys) {
      const value = source[key];
      if (value !== undefined && value !== null && value !== '') return value;
    }
  }
  return fallback;
}

/** Coerce to a finite number with a fallback. */
export function num(value, fallback = 0) {
  const parsed = typeof value === 'number' ? value : Number(value);
  return Number.isFinite(parsed) ? parsed : fallback;
}

/** Coerce to a boolean, understanding `"true"`, `"1"` and IMAP flags. */
export function bool(value) {
  if (typeof value === 'boolean') return value;
  if (typeof value === 'number') return value !== 0;
  if (typeof value === 'string') {
    const normalised = value.trim().toLowerCase();
    return normalised === 'true' || normalised === '1' || normalised === 'yes';
  }
  return false;
}

/**
 * The collection keys this API wraps a list in.
 *
 * Most endpoints page with `{items, total}`, but two answer with the name of the
 * collection instead: `GET /mailboxes` sends `{mailboxes: […]}` and
 * `GET /mailboxes/:id/folders` sends `{mailbox_id, folders: […]}`. Every envelope
 * the server emits is listed here on purpose — a key missing from this list turns a
 * response that has rows into an empty screen, which is how the folder tree once
 * shipped empty and left the whole message list unreachable.
 */
const LIST_ENVELOPES = ['items', 'data', 'mailboxes', 'folders'];

/** Unwrap a list envelope of any of [`LIST_ENVELOPES`], or a bare array. */
export function listOf(payload) {
  if (Array.isArray(payload)) return payload;
  if (payload && typeof payload === 'object') {
    for (const key of LIST_ENVELOPES) {
      if (Array.isArray(payload[key])) return payload[key];
    }
  }
  return [];
}

/** @param {unknown} payload */
export function totalOf(payload) {
  if (payload && typeof payload === 'object' && payload.total !== undefined) return num(payload.total, 0);
  return listOf(payload).length;
}

/** Flatten `[{name, address}]` and `"Name <a@b>"` into one display string. */
export function flattenAddresses(value) {
  if (Array.isArray(value)) return value.map(flattenAddresses).filter(Boolean).join(', ');
  if (value && typeof value === 'object') {
    const name = pick(value, ['name', 'display_name'], '');
    const address = pick(value, ['address', 'email', 'addr'], '');
    if (name && address) return `${name} <${address}>`;
    return String(address || name || '');
  }
  return value === undefined || value === null ? '' : String(value);
}

/** The bare `a@b` part of a display string. */
export function addressOnly(value) {
  const text = flattenAddresses(value);
  const match = text.match(/<([^>]+)>/);
  return (match ? match[1] : text).trim();
}

/** The display name, falling back to the address. */
export function displayName(value) {
  const text = flattenAddresses(value);
  const match = text.match(/^([^<]+)</);
  if (match) return match[1].trim().replace(/^"|"$/g, '');
  return addressOnly(text);
}

/** Split a header value into individual addresses. */
export function addressList(value) {
  if (Array.isArray(value)) return value.map(flattenAddresses).filter(Boolean);
  const text = flattenAddresses(value);
  if (text === '') return [];
  const parts = [];
  let depth = 0;
  let current = '';
  for (const char of text) {
    if (char === '<' || char === '(') depth += 1;
    if (char === '>' || char === ')') depth = Math.max(0, depth - 1);
    if (char === ',' && depth === 0) {
      if (current.trim()) parts.push(current.trim());
      current = '';
    } else {
      current += char;
    }
  }
  if (current.trim()) parts.push(current.trim());
  return parts;
}

/* ------------------------------------------------------------------- folders */

const SPECIAL_ALIASES = {
  inbox: 'inbox',
  sent: 'sent',
  'sent items': 'sent',
  'sent messages': 'sent',
  drafts: 'drafts',
  draft: 'drafts',
  trash: 'trash',
  'deleted items': 'trash',
  'deleted messages': 'trash',
  junk: 'junk',
  spam: 'junk',
  archive: 'archive',
  all: 'archive',
  allmail: 'archive',
};

/**
 * Map an IMAP special-use attribute to a stable slug.
 * `\\Sent`, `Sent`, and `\\sent` all become `sent`; folder *names* never decide.
 * @param {unknown} value
 */
export function specialUseOf(value) {
  if (value === undefined || value === null) return null;
  const raw = String(Array.isArray(value) ? value[0] : value).trim();
  if (raw === '') return null;
  const normalised = raw.replace(/^\\+/, '').toLowerCase().replace(/[_-]/g, ' ').trim();
  if (SPECIAL_ALIASES[normalised]) return SPECIAL_ALIASES[normalised];
  const squashed = normalised.replace(/ /g, '');
  return SPECIAL_ALIASES[squashed] || null;
}

const FOLDER_ORDER = ['inbox', 'drafts', 'sent', 'archive', 'junk', 'trash'];

/** @param {Record<string, unknown>} value */
export function normalizeFolder(value) {
  const source = value && typeof value === 'object' ? value : {};
  const name = String(pick(source, ['name', 'display_name', 'path'], ''));
  const special = specialUseOf(pick(source, ['special_use', 'specialUse', 'special', 'attributes'], null));
  return {
    id: num(pick(source, ['id', 'folder_id'], 0), 0),
    name,
    parent: pick(source, ['parent', 'parent_id', 'parent_name'], null),
    specialUse: special,
    subscribed: bool(pick(source, ['subscribed'], true)),
    messageCount: num(pick(source, ['message_count', 'messages', 'total', 'count'], 0), 0),
    unseenCount: num(pick(source, ['unseen_count', 'unread', 'unread_count', 'unseen'], 0), 0),
    sortKey: special ? FOLDER_ORDER.indexOf(special) : FOLDER_ORDER.length,
    raw: source,
  };
}

/** @param {unknown} payload */
export function foldersOf(payload) {
  return listOf(payload).map(normalizeFolder);
}

/* ------------------------------------------------------------------ mailboxes */

/** @param {Record<string, unknown>} value */
export function normalizeMailbox(value) {
  const source = value && typeof value === 'object' ? value : {};
  const localPart = String(pick(source, ['local_part'], ''));
  const domain = String(pick(source, ['domain', 'domain_name'], ''));
  const fromParts = localPart && domain ? `${localPart}@${domain}` : '';
  const address = String(pick(source, ['address', 'email', 'name'], fromParts));
  return {
    id: num(pick(source, ['id', 'mailbox_id'], 0), 0),
    address,
    localPart,
    domain,
    isPrimary: bool(pick(source, ['is_primary', 'primary'], false)),
    quotaBytes: pick(source, ['quota_bytes'], null) === null ? null : num(pick(source, ['quota_bytes'], 0), 0),
    usedBytes: num(pick(source, ['used_bytes', 'size_bytes'], 0), 0),
    raw: source,
  };
}

/** @param {unknown} payload */
export function mailboxesOf(payload) {
  return listOf(payload).map(normalizeMailbox);
}

/* ---------------------------------------------------------------- attachments */

/** @param {Record<string, unknown>} value */
export function normalizeAttachment(value) {
  const source = value && typeof value === 'object' ? value : {};
  return {
    id: num(pick(source, ['id', 'attachment_id'], 0), 0),
    filename: String(pick(source, ['filename', 'name', 'file_name'], 'attachment')),
    contentType: String(pick(source, ['content_type', 'mime', 'type'], 'application/octet-stream')),
    sizeBytes: num(pick(source, ['size_bytes', 'size'], 0), 0),
    sha256: pick(source, ['sha256'], null),
    raw: source,
  };
}

/* ------------------------------------------------------------------- messages */

/** @param {Record<string, unknown>} value */
export function normalizeMessage(value) {
  const source = value && typeof value === 'object' ? value : {};
  const from = flattenAddresses(pick(source, ['from', 'sender', 'from_address'], ''));
  const to = addressList(pick(source, ['to', 'recipients'], []));
  const cc = addressList(pick(source, ['cc'], []));
  const bcc = addressList(pick(source, ['bcc'], []));
  const flags = pick(source, ['flags', 'keywords'], []);
  const flagText = Array.isArray(flags) ? flags.map(String).join(' ').toLowerCase() : String(flags).toLowerCase();
  const attachmentList = listOf(pick(source, ['attachments', 'attachment_metadata'], []))
    .map(normalizeAttachment)
    .filter((item) => item.id > 0);

  return {
    id: num(pick(source, ['id', 'message_id'], 0), 0),
    subject: String(pick(source, ['subject'], '(no subject)')),
    from,
    fromName: displayName(from),
    fromAddress: addressOnly(from),
    to,
    cc,
    bcc,
    date: pick(source, ['date', 'internal_date', 'received_at', 'sent_at', 'created_at'], null),
    snippet: String(pick(source, ['snippet', 'preview', 'summary'], '')),
    seen: bool(pick(source, ['seen', 'is_seen', 'read'], false)) || flagText.includes('\\seen'),
    flagged: bool(pick(source, ['flagged', 'is_flagged', 'starred'], false)) || flagText.includes('\\flagged'),
    answered: bool(pick(source, ['answered'], false)) || flagText.includes('\\answered'),
    draft: bool(pick(source, ['draft', 'is_draft'], false)) || flagText.includes('\\draft'),
    sizeBytes: num(pick(source, ['size_bytes', 'size'], 0), 0),
    text: pick(source, ['text', 'text_body', 'body_text', 'plain'], ''),
    html: pick(source, ['html', 'html_body', 'body_html'], ''),
    attachments: attachmentList,
    hasAttachments: bool(pick(source, ['has_attachments'], false)) || attachmentList.length > 0,
    folderId: num(pick(source, ['folder_id', 'mailbox_folder_id'], 0), 0),
    mailboxId: num(pick(source, ['mailbox_id'], 0), 0),
    raw: source,
  };
}

/** @param {unknown} payload */
export function messagesOf(payload) {
  return listOf(payload).map(normalizeMessage);
}

/* -------------------------------------------------------------------- threads */

/**
 * The `in_reply_to` / `references` pair for a reply, read from the raw payload.
 * @param {Record<string, unknown>|null|undefined} raw
 */
export function threadHeaders(raw) {
  const source = raw && typeof raw === 'object' ? raw : {};
  const messageId = pick(source, ['message_id_header', 'rfc_message_id', 'message_id_header_value'], null);
  const inReplyTo = pick(source, ['in_reply_to'], null);
  const references = pick(source, ['references'], []);
  const list = Array.isArray(references)
    ? references.map(String)
    : String(references || '')
        .split(/\s+/)
        .filter(Boolean);
  return {
    messageId: messageId === null ? null : String(messageId),
    inReplyTo: inReplyTo === null ? null : String(inReplyTo),
    references: list,
  };
}

/* ------------------------------------------------------------------ mail queue */

/** @param {Record<string, unknown>} value */
export function normalizeQueueEntry(value) {
  const source = value && typeof value === 'object' ? value : {};
  return {
    id: num(pick(source, ['id', 'queue_id'], 0), 0),
    recipient: String(pick(source, ['recipient', 'to', 'rcpt_to'], '')),
    sender: String(pick(source, ['sender', 'from', 'mail_from'], '')),
    subject: String(pick(source, ['subject'], '')),
    messageId: num(pick(source, ['message_id'], 0), 0),
    status: String(pick(source, ['status', 'state'], 'unknown')).toLowerCase(),
    attempts: num(pick(source, ['attempts', 'attempt_count'], 0), 0),
    nextAttemptAt: pick(source, ['next_attempt_at', 'next_due_at'], null),
    lastError: pick(source, ['last_error', 'error'], null),
    createdAt: pick(source, ['created_at', 'queued_at'], null),
    updatedAt: pick(source, ['updated_at'], null),
    raw: source,
  };
}

/** @param {unknown} payload */
export function queueEntriesOf(payload) {
  return listOf(payload).map(normalizeQueueEntry);
}

/**
 * The per-attempt delivery log of one queue entry, whatever container it arrives
 * in (`attempts`, `log`, `history`, or the whole detail object).
 * @param {Record<string, unknown>|null|undefined} payload
 */
export function attemptLogOf(payload) {
  const source = payload && typeof payload === 'object' ? payload : {};
  const container = pick(source, ['attempts', 'log', 'history', 'delivery_log', 'entries'], []);
  return listOf(container).map((item) => {
    const entry = item && typeof item === 'object' ? item : {};
    return {
      at: pick(entry, ['at', 'attempted_at', 'created_at', 'timestamp', 'time'], null),
      status: String(pick(entry, ['status', 'result', 'state'], '')),
      smtpCode: pick(entry, ['smtp_code', 'code', 'response_code'], null),
      message: String(pick(entry, ['message', 'error', 'detail', 'reason', 'response'], '')),
      host: String(pick(entry, ['host', 'mx_host', 'remote_host'], '')),
    };
  });
}

/* -------------------------------------------------------------------- records */

/** @param {Record<string, unknown>} value */
export function normalizeUser(value) {
  const source = value && typeof value === 'object' ? value : {};
  return {
    id: num(pick(source, ['id', 'user_id'], 0), 0),
    email: String(pick(source, ['email'], '')),
    displayName: String(pick(source, ['display_name', 'name'], '')),
    isAdmin: bool(pick(source, ['is_admin', 'admin'], false)),
    enabled: bool(pick(source, ['enabled', 'active'], true)),
    quotaBytes: num(pick(source, ['quota_bytes', 'quota'], 0), 0),
    usedBytes: num(pick(source, ['used_bytes', 'used'], 0), 0),
    createdAt: pick(source, ['created_at', 'created'], null),
    mailboxes: listOf(pick(source, ['mailboxes', 'addresses'], [])).map(normalizeMailbox),
    raw: source,
  };
}

/** @param {unknown} payload */
export function usersOf(payload) {
  return listOf(payload).map(normalizeUser);
}

/** @param {Record<string, unknown>} value */
export function normalizeDomain(value) {
  const source = value && typeof value === 'object' ? value : {};
  return {
    id: num(pick(source, ['id', 'domain_id'], 0), 0),
    name: String(pick(source, ['name', 'domain'], '')),
    description: String(pick(source, ['description'], '')),
    enabled: bool(pick(source, ['enabled', 'active'], true)),
    catchAll: pick(source, ['catch_all'], null),
    createdAt: pick(source, ['created_at', 'created'], null),
    raw: source,
  };
}

/** @param {unknown} payload */
export function domainsOf(payload) {
  return listOf(payload).map(normalizeDomain);
}

/** @param {Record<string, unknown>} value */
export function normalizeAlias(value) {
  const source = value && typeof value === 'object' ? value : {};
  return {
    id: num(pick(source, ['id', 'alias_id'], 0), 0),
    localPart: String(pick(source, ['local_part'], '')),
    target: String(pick(source, ['target', 'destination', 'forward_to'], '')),
    domainId: num(pick(source, ['domain_id'], 0), 0),
    enabled: bool(pick(source, ['enabled', 'active'], true)),
    raw: source,
  };
}

/** @param {unknown} payload */
export function aliasesOf(payload) {
  return listOf(payload).map(normalizeAlias);
}

/** One DNS check row from `GET /api/v1/domains/:id/dns`. */
export function normalizeDnsRecord(value) {
  const source = value && typeof value === 'object' ? value : {};
  const found = pick(source, ['found', 'values', 'actual'], []);
  const status = String(pick(source, ['status', 'state', 'result'], 'skip')).toLowerCase();
  return {
    kind: String(pick(source, ['kind', 'type', 'record_type'], '?')).toUpperCase(),
    status: ['ok', 'warn', 'fail', 'skip'].includes(status) ? status : 'skip',
    expected: pick(source, ['expected', 'want'], null),
    found: Array.isArray(found) ? found.map(String) : found === null ? [] : [String(found)],
    hint: pick(source, ['hint', 'advice'], null),
    raw: source,
  };
}

/** The whole DNS health report. */
export function normalizeDnsReport(value) {
  const source = value && typeof value === 'object' ? value : {};
  const records = listOf(pick(source, ['records', 'checks'], [])).map(normalizeDnsRecord);
  return {
    domain: String(pick(source, ['domain', 'name'], '')),
    checkedAt: pick(source, ['checked_at', 'checked'], null),
    records,
    score: num(pick(source, ['score'], records.filter((r) => r.status === 'ok').length), 0),
    maxScore: num(pick(source, ['max_score', 'total_score'], records.length), records.length),
  };
}

/** The DKIM record to publish. */
export function normalizeDkim(value) {
  const source = value && typeof value === 'object' ? value : {};
  return {
    selector: String(pick(source, ['selector'], 'default')),
    recordName: String(pick(source, ['record_name', 'name'], '')),
    recordType: String(pick(source, ['record_type', 'type'], 'TXT')),
    recordValue: String(pick(source, ['record_value', 'value'], '')),
  };
}

/** One audit-log row. */
export function normalizeAuditEntry(value) {
  const source = value && typeof value === 'object' ? value : {};
  return {
    id: num(pick(source, ['id'], 0), 0),
    at: pick(source, ['at', 'created_at', 'timestamp'], null),
    actorUserId: pick(source, ['actor_user_id', 'actor_id', 'user_id'], null),
    actor: String(pick(source, ['actor', 'actor_email', 'actor_name'], '')),
    action: String(pick(source, ['action', 'event'], '')),
    target: String(pick(source, ['target', 'resource', 'subject'], '')),
    detail: pick(source, ['detail', 'details', 'metadata'], null),
    ip: String(pick(source, ['ip', 'ip_address', 'remote_addr'], '')),
    raw: source,
  };
}

/* ----------------------------------------------------------------- dashboard */

/**
 * The numbers behind the Admin dashboard's stat grid.
 *
 * They come from three responses and, crucially, from **nested** keys inside them:
 * `/health` carries today's traffic under `queue` and the live session count under
 * `clients`, while `/storage` carries the user and domain counts. Reading one of
 * those one level too high yields `undefined`, and the grid renders `undefined` as
 * “—”, which reads as “this API does not report it” — how three cards stayed blank
 * on servers that were reporting every one of them.
 *
 * The lookups live here, beside the other payload normalisers, so a test can prove
 * them without a browser — see the `data:dashboard` rule in `tools/check.mjs`.
 *
 * @param {unknown} health `GET /api/v1/health`
 * @param {unknown} queueStats `GET /api/v1/queue/stats`
 * @param {unknown} storage `GET /api/v1/storage`
 */
export function dashboardStats(health, queueStats, storage) {
  const h = health && typeof health === 'object' ? health : {};
  const q = queueStats && typeof queueStats === 'object' ? queueStats : {};
  const s = storage && typeof storage === 'object' ? storage : {};
  const healthQueue = h.queue && typeof h.queue === 'object' ? h.queue : {};
  const counts = q.counts && typeof q.counts === 'object' ? q.counts : q;

  return {
    // `/storage` is the only endpoint that counts accounts and domains.
    users: pick(s, ['users', 'user_count'], undefined),
    domains: pick(s, ['domains', 'domain_count'], undefined),
    // `/queue/stats` has no notion of "today"; `/health` reports it under `queue`.
    receivedToday: pick(q, ['received_today', 'today_received'], pick(healthQueue, ['received_today'], undefined)),
    sentToday: pick(q, ['sent_today', 'today_sent'], pick(healthQueue, ['sent_today'], undefined)),
    queuePending: pick(counts, ['pending'], pick(healthQueue, ['pending'], undefined)),
    queueRetry: pick(counts, ['retry'], pick(healthQueue, ['retry'], undefined)),
    failedDeliveries: pick(counts, ['failed'], pick(healthQueue, ['failed'], undefined)),
    // Sizes and counts from `/storage`.
    maildirBytes: pick(s, ['maildir_bytes'], undefined),
    attachmentBytes: pick(s, ['attachment_bytes'], undefined),
    databaseBytes: pick(s, ['database_bytes'], undefined),
    // Non-revoked, non-expired `sessions` rows, under `clients`.
    activeClientSessions: pick(h.clients && typeof h.clients === 'object' ? h.clients : {}, ['active_sessions', 'active_client_sessions'], undefined),
    uptimeSecs: pick(h, ['uptime_secs'], undefined),
  };
}
