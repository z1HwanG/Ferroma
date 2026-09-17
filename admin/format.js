/**
 * Formatting helpers: dates, sizes and the two body renderings.
 *
 * Date handling parses the API's RFC 3339 UTC strings and renders them in the
 * reader's own timezone. Anything unparseable renders as an empty string rather
 * than "Invalid Date".
 */

const TIME_FORMAT = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit' });
const SHORT_FORMAT = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' });
const FULL_FORMAT = new Intl.DateTimeFormat(undefined, {
  dateStyle: 'full',
  timeStyle: 'short',
});

/**
 * @param {unknown} value
 * @returns {Date|null}
 */
export function parseDate(value) {
  if (value === null || value === undefined || value === '') return null;
  if (value instanceof Date) return Number.isNaN(value.getTime()) ? null : value;
  if (typeof value === 'number') {
    const fromEpoch = new Date(value < 1e12 ? value * 1000 : value);
    return Number.isNaN(fromEpoch.getTime()) ? null : fromEpoch;
  }
  const text = String(value).trim();
  const candidates = [text];
  if (/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}/.test(text)) candidates.push(`${text.replace(' ', 'T')}Z`);
  if (/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2})?(\.\d+)?$/.test(text)) candidates.push(`${text}Z`);
  for (const candidate of candidates) {
    const parsed = new Date(candidate);
    if (!Number.isNaN(parsed.getTime())) return parsed;
  }
  return null;
}

/**
 * Short stamp for the message list: a time for today, a weekday for this week,
 * a month and day for the current year, and a full date beyond that.
 * @param {unknown} value
 */
export function shortStamp(value) {
  const date = parseDate(value);
  if (!date) return '';
  const now = new Date();
  const startOfToday = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  const startOfDay = new Date(date.getFullYear(), date.getMonth(), date.getDate());
  const dayDelta = Math.round((startOfToday.getTime() - startOfDay.getTime()) / 86400000);

  if (dayDelta === 0) return TIME_FORMAT.format(date);
  if (dayDelta === 1) return 'Yesterday';
  if (dayDelta > 1 && dayDelta < 7) {
    return new Intl.DateTimeFormat(undefined, { weekday: 'short' }).format(date);
  }
  if (date.getFullYear() === now.getFullYear()) return SHORT_FORMAT.format(date);
  return new Intl.DateTimeFormat(undefined, { year: 'numeric', month: 'short', day: 'numeric' }).format(date);
}

/** Full local timestamp, used in the reading pane and as a `title`. */
export function fullStamp(value) {
  const date = parseDate(value);
  return date ? FULL_FORMAT.format(date) : '';
}

/** Compact relative age, e.g. `3 min ago`. */
export function relativeStamp(value) {
  const date = parseDate(value);
  if (!date) return '';
  const seconds = Math.round((Date.now() - date.getTime()) / 1000);
  if (seconds < 60) return 'just now';
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} h ago`;
  const days = Math.round(hours / 24);
  if (days < 30) return `${days} d ago`;
  return shortStamp(value);
}

/**
 * A `<time>` element carrying both the local rendering and the machine-readable
 * value.
 * @param {unknown} value
 * @param {{relative?: boolean}} [options]
 */
export function timeElement(value, options = {}) {
  const date = parseDate(value);
  const time = document.createElement('time');
  if (!date) {
    time.textContent = '';
    return time;
  }
  time.dateTime = date.toISOString();
  time.textContent = options.relative ? relativeStamp(value) : shortStamp(value);
  time.title = fullStamp(value);
  return time;
}

/**
 * `1536` → `1.5 KB`.
 * @param {number} bytes
 */
export function formatBytes(bytes) {
  const value = Number(bytes);
  if (!Number.isFinite(value) || value <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let index = 0;
  let scaled = value;
  while (scaled >= 1024 && index < units.length - 1) {
    scaled /= 1024;
    index += 1;
  }
  const digits = scaled >= 10 || index === 0 ? 0 : 1;
  return `${scaled.toFixed(digits)} ${units[index]}`;
}

/** `2026-09-16T12:00:00Z` → `2026-09-16 12:00:00` in local time. */
export function formatLogStamp(value) {
  const date = parseDate(value);
  if (!date) return '';
  const pad = (input) => String(input).padStart(2, '0');
  return (
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ` +
    `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
  );
}

/**
 * Turn a plain-text body into the minimal HTML the compose editor can show,
 * escaping first so a body can never introduce markup.
 * @param {string} text
 */
export function textToHtml(text) {
  const escaped = String(text || '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
  return escaped
    .split(/\n{2,}/)
    .map((block) => `<p>${block.replace(/\n/g, '<br>')}</p>`)
    .join('\n');
}

/**
 * Derive a plain-text body from the rich-text editor's HTML. Runs against a
 * detached document, so nothing here is ever rendered.
 * @param {string} html
 */
export function htmlToText(html) {
  const source = String(html || '');
  if (source === '') return '';
  const doc = new DOMParser().parseFromString(source, 'text/html');
  doc.querySelectorAll('br').forEach((node) => node.replaceWith('\n'));
  doc.querySelectorAll('p, div, li, tr, blockquote, h1, h2, h3, h4').forEach((node) => {
    node.append('\n');
  });
  const text = doc.body ? doc.body.textContent || '' : '';
  return text
    .replace(/\u00a0/g, ' ')
    .replace(/[ \t]+\n/g, '\n')
    .replace(/\n{3,}/g, '\n\n')
    .trim();
}

/** The quoted reply block, in plain text. */
export function quoteText(message) {
  const stamp = fullStamp(message.date);
  const attribution = `On ${stamp || 'an unknown date'}, ${message.from || 'the sender'} wrote:`;
  const body = String(message.text || '').trim() || htmlToText(message.html) || '(no text body)';
  const quoted = body
    .split('\n')
    .map((line) => `> ${line}`)
    .join('\n');
  return `${attribution}\n${quoted}`;
}

/** The quoted reply block, in HTML (escaped, then wrapped in a blockquote). */
export function quoteHtml(message) {
  const stamp = fullStamp(message.date);
  const attribution = `On ${stamp || 'an unknown date'}, ${message.from || 'the sender'} wrote:`;
  const source = String(message.html || '').trim();
  const inner = source !== '' ? source : textToHtml(message.text || '');
  return (
    `<p><br></p><p>${escapeHtml(attribution)}</p>` +
    `<blockquote style="margin:0 0 0 12px;padding-left:12px;border-left:2px solid #cccccc">${inner}</blockquote>`
  );
}

/** @param {string} value */
export function escapeHtml(value) {
  return String(value || '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

/** `Re:`, `Fwd:` prefixes without stacking duplicates. */
export function replySubject(subject) {
  const text = String(subject || '').trim();
  if (/^re:/i.test(text)) return text;
  return `Re: ${text}`.trim();
}

export function forwardSubject(subject) {
  const text = String(subject || '').trim();
  if (/^fwd?:/i.test(text)) return text;
  return `Fwd: ${text}`.trim();
}

/**
 * Recipients of a reply-all: the original sender plus every original To/Cc, minus
 * the user's own addresses.
 * @param {{from: string, to: string[], cc: string[]}} message
 * @param {string[]} selfAddresses
 */
export function replyAllRecipients(message, selfAddresses) {
  const mine = new Set(selfAddresses.map((address) => address.toLowerCase()));
  const to = [];
  const cc = [];
  const candidates = [message.from].concat(message.to);
  for (const candidate of candidates) {
    const address = String(candidate || '');
    const bare = (address.match(/<([^>]+)>/) ? address.match(/<([^>]+)>/)[1] : address).trim().toLowerCase();
    if (bare === '' || mine.has(bare) || to.includes(address)) continue;
    to.push(address);
  }
  for (const candidate of message.cc) {
    const address = String(candidate || '');
    const bare = (address.match(/<([^>]+)>/) ? address.match(/<([^>]+)>/)[1] : address).trim().toLowerCase();
    if (bare === '' || mine.has(bare) || to.includes(address) || cc.includes(address)) continue;
    cc.push(address);
  }
  return { to, cc };
}

/** A short label for the attachment chip. */
export function fileKind(filename) {
  const match = /\.([A-Za-z0-9]{1,8})$/.exec(String(filename || ''));
  return match ? match[1].toUpperCase() : 'FILE';
}
