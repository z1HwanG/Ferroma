#!/usr/bin/env node
/**
 * Static regression check for a Ferroma front-end app.
 *
 * There is no browser and no bundler in CI, so this script is the whole test
 * suite. Run it from the app directory (`web/` or `admin/`): `node tools/check.mjs`.
 *
 * It asserts:
 *
 *   1. no inline `<script>` bodies, no `on*=` handler attributes, no absolute
 *      `/…` asset paths (a plain `<a href="/…">` navigation link is allowed);
 *   2. every `getElementById('x')` / `querySelector('#x')` target exists in the
 *      app's HTML;
 *   3. every `fetch(` call site lives in `api.js` and uses a path starting with
 *      `/api/v1`;
 *   4. every local file referenced from HTML (`src`, `<link href>`) or CSS
 *      (`url(...)`) exists on disk;
 *   5. every ES-module import resolves to a file on disk;
 *   6. no `console.log`, no `eval`, no `innerHTML` outside the compose editor;
 *   7. every `import` binding is used somewhere else in its module;
 *   8. (Webmail only) the message HTML part is rendered through a sandboxed
 *      `srcdoc` frame;
 *   9. every import names a real export, and every export is used;
 *  10. every payload envelope the server actually sends survives the app's own
 *      normalisers — a collection key `listOf()` does not know turns a response
 *      that has rows into an empty screen while every rule above stays green.
 *
 * The app profile is read from the `<html data-app="…">` attribute.
 */

import { readdirSync, readFileSync, statSync, existsSync } from 'node:fs';
import { dirname, join, resolve, relative, extname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

/**
 * Modules living in the sibling `shared/` directory, which both apps import. Their
 * exports are consumed by two different apps, so the dead-export rule cannot judge
 * them against one app's import graph and skips them.
 */
const SHARED_MODULES = new Set(['api.js', 'data.js', 'dom.js', 'format.js', 'net.js', 'theme.js', 'toast.js', 'modal.js', 'i18n.js']);

/** The one directory both apps take their common modules from. */
const SHARED_ROOT = resolve(ROOT, '..', 'shared');

const failures = [];
const notes = [];
let checks = 0;

function fail(file, rule, message) {
  failures.push({ file: relative(ROOT, file).replace(/\\/g, '/'), rule, message });
}

function check(condition, file, rule, message) {
  checks += 1;
  if (!condition) fail(file, rule, message);
}

function walk(dir, out = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === 'node_modules' || entry.name.startsWith('.')) continue;
    const full = join(dir, entry.name);
    // `tools/` holds this script; scanning it would let its own examples be
    // mistaken for real references.
    if (entry.isDirectory()) {
      if (full === join(ROOT, 'tools')) continue;
      walk(full, out);
    } else {
      out.push(full);
    }
  }
  return out;
}

const files = walk(ROOT);
// The shared modules belong to every app's module graph, so they are checked
// alongside the app that imports them: imports must resolve, exports must exist,
// and no `console.log` may hide in them.
if (existsSync(SHARED_ROOT)) files.push(...walk(SHARED_ROOT));
const htmlFiles = files.filter((file) => extname(file) === '.html');
const jsFiles = files.filter((file) => extname(file) === '.js');
const cssFiles = files.filter((file) => extname(file) === '.css');

/* ------------------------------------------------------------------- rule 1 */

const declaredIds = new Set();
let appProfile = 'unknown';

for (const file of htmlFiles) {
  const source = readFileSync(file, 'utf8');
  const withoutComments = source.replace(/<!--[\s\S]*?-->/g, '');

  const profile = /<html[^>]*\bdata-app\s*=\s*"([^"]+)"/i.exec(withoutComments);
  if (profile) appProfile = profile[1];

  for (const match of withoutComments.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/gi)) {
    const attrs = match[1];
    const body = match[2].trim();
    const hasSrc = /\bsrc\s*=/i.test(attrs);
    check(
      hasSrc,
      file,
      'html:inline-script',
      `a <script> tag without src carries ${body.length} characters of inline code`,
    );
    check(body === '', file, 'html:inline-script', 'a <script> tag has an inline body');
  }

  for (const match of withoutComments.matchAll(/\son[a-z]+\s*=/gi)) {
    fail(file, 'html:inline-handler', `inline handler attribute ${match[0].trim()} is not allowed`);
  }

  // Asset references must be relative; a document-level link may be absolute
  // (the Admin console links back to the Webmail root).
  for (const match of withoutComments.matchAll(/<(script|img|iframe|source|video|audio|embed)\b[^>]*\b(?:src|poster|data-src)\s*=\s*"([^"]*)"/gi)) {
    if (match[2].trim().startsWith('/')) {
      fail(file, 'html:absolute-path', `asset path "${match[2]}" must be relative (./…)`);
    }
  }
  for (const match of withoutComments.matchAll(/<link\b[^>]*\bhref\s*=\s*"([^"]*)"/gi)) {
    if (match[1].trim().startsWith('/')) {
      fail(file, 'html:absolute-path', `stylesheet path "${match[1]}" must be relative (./…)`);
    }
  }

  for (const match of withoutComments.matchAll(/\bid\s*=\s*"([^"]+)"/gi)) {
    declaredIds.add(match[1]);
  }
}

/* ------------------------------------------------------------------- rule 2 */

const SELECTOR_RE = /(?:querySelector|querySelectorAll|closest|matches)\(\s*'([^']*)'\s*\)/g;
const GET_BY_ID_RE = /getElementById\(\s*'([^']*)'\s*\)/g;
/* `byId()` is the app's own lookup helper; it throws on a missing id, so a typo
   here is a real regression. Ids a module creates itself are collected first so a
   view-rendered element is not mistaken for a typo. */
const BY_ID_RE = /\bbyId\(\s*'([^']*)'\s*\)/g;
const CREATED_ID_RE = /\bid\s*:\s*'([^']+)'/g;

for (const file of jsFiles) {
  const source = readFileSync(file, 'utf8');
  const createdHere = new Set();
  for (const match of source.matchAll(CREATED_ID_RE)) createdHere.add(match[1]);
  for (const pattern of [GET_BY_ID_RE, BY_ID_RE]) {
    for (const match of source.matchAll(pattern)) {
      check(
        declaredIds.has(match[1]) || createdHere.has(match[1]),
        file,
        'js:missing-id',
        `byId('${match[1]}') has no matching id in the HTML or in this module`,
      );
    }
  }
  for (const match of source.matchAll(SELECTOR_RE)) {
    const selector = match[1];
    for (const candidate of selector.matchAll(/#([A-Za-z_][\w-]*)/g)) {
      check(
        declaredIds.has(candidate[1]),
        file,
        'js:missing-id',
        `selector "${selector}" targets #${candidate[1]}, which the HTML does not declare`,
      );
    }
  }
  for (const match of source.matchAll(/querySelector\(\s*`([^`]*)`\s*\)/g)) {
    if (match[1].includes('${')) {
      notes.push(`dynamic selector in ${relative(ROOT, file)}: ${match[1]}`);
      continue;
    }
    for (const candidate of match[1].matchAll(/#([A-Za-z_][\w-]*)/g)) {
      check(declaredIds.has(candidate[1]), file, 'js:missing-id', `selector targets #${candidate[1]}`);
    }
  }
}

/* ------------------------------------------------------------------- rule 3 */

/* Every network call goes through `api.js`, so this rule both pins the paths it
   uses and stops a feature module from reaching for `fetch` behind its back. */
const FETCH_RE = /\bfetch\(\s*(?:new URL\(\s*)?(?:'([^']*)'|"([^"]*)")?/g;
const API_MODULE = 'api.js';

for (const file of jsFiles) {
  const source = readFileSync(file, 'utf8');
  const callSites = Array.from(source.matchAll(FETCH_RE)).map((match) => match[1] ?? match[2] ?? '');
  const isApiModule = file.endsWith(API_MODULE);

  if (isApiModule) {
    check(callSites.length >= 1, file, 'js:no-fetch', 'api.js must contain a fetch call site');
    check(
      /fetch\(\s*new URL\(path,/.test(source),
      file,
      'js:no-fetch',
      'api.js must keep the single variable-path fetch that all requests go through',
    );
  } else {
    check(callSites.length === 0, file, 'js:fetch-outside-api', 'only api.js may call fetch');
  }

  for (const target of callSites) {
    if (target === '') continue; // variable path: covered by the check below
    check(
      target.startsWith('/api/v1') || target.includes('API_BASE'),
      file,
      'js:fetch-path',
      `fetch("${target}") must be an /api/v1 path`,
    );
  }
}

/* The feature modules never call fetch directly — they go through the two
   helpers in `api.js`, so their paths are pinned here as well. */
const REQUEST_RE = /\b(?:request|download)\(\s*(?:'([^']*)'|"([^"]*)"|`([^`]*)`)/g;

for (const file of jsFiles) {
  const source = readFileSync(file, 'utf8');
  for (const match of source.matchAll(REQUEST_RE)) {
    const target = match[1] ?? match[2] ?? match[3] ?? '';
    // A template literal starting with `${API_BASE}` is the canonical form.
    const startsWithBase = target.startsWith('/api/v1') || target.startsWith('${API_BASE}');
    check(
      startsWithBase,
      file,
      'js:api-path',
      `api call "${target}" must start with \${API_BASE} or /api/v1`,
    );
  }
}

/* ------------------------------------------------------------------- rule 4 */

const EXTERNAL = /^(?:[a-z][a-z0-9+.-]*:|#|\/\/)/i;

function checkLocalReference(file, reference, rule) {
  const clean = reference.split('#')[0].split('?')[0].trim();
  if (clean === '' || EXTERNAL.test(clean)) return;
  if (clean.startsWith('/')) {
    fail(file, rule, `"${reference}" is an absolute path and will not resolve under /admin or /`);
    return;
  }
  const target = resolve(dirname(file), clean);
  const exists = existsSync(target) && statSync(target).isFile();
  const isDirectoryIndex = existsSync(target) && statSync(target).isDirectory() && existsSync(join(target, 'index.html'));
  check(exists || isDirectoryIndex, file, rule, `"${reference}" does not exist on disk`);
}

for (const file of htmlFiles) {
  const source = readFileSync(file, 'utf8');
  for (const match of source.matchAll(/<script\b[^>]*\bsrc\s*=\s*"([^"]+)"/gi)) {
    checkLocalReference(file, match[1], 'html:missing-asset');
  }
  for (const match of source.matchAll(/<link\b[^>]*\bhref\s*=\s*"([^"]+)"/gi)) {
    checkLocalReference(file, match[1], 'html:missing-asset');
  }
  for (const match of source.matchAll(/<(?:img|iframe|source|video|audio)\b[^>]*\bsrc\s*=\s*"([^"]+)"/gi)) {
    checkLocalReference(file, match[1], 'html:missing-asset');
  }
}

for (const file of cssFiles) {
  const source = readFileSync(file, 'utf8');
  for (const match of source.matchAll(/url\(\s*(['"]?)([^'")]+)\1\s*\)/gi)) {
    checkLocalReference(file, match[2], 'css:missing-asset');
  }
}

/* ------------------------------------------------------------------- rule 5 */

const IMPORT_RE = /(?:^|\n)\s*(?:import|export)[\s\S]*?from\s+['"]([^'"]+)['"]/g;

for (const file of jsFiles) {
  const source = readFileSync(file, 'utf8');
  for (const match of source.matchAll(IMPORT_RE)) {
    const specifier = match[1];
    if (!specifier.startsWith('.')) {
      fail(file, 'js:bare-import', `"${specifier}" is not a relative import`);
      continue;
    }
    const target = resolve(dirname(file), specifier);
    check(existsSync(target), file, 'js:missing-module', `import "${specifier}" does not exist`);
  }
}

/* ------------------------------------------------------------------- rule 6 */

/* Assignments to innerHTML are only tolerated on the compose editor, and only
   from the app's own escaping builders, from the value the editor stored itself,
   or from the caller-supplied seed of a draft. The reading pane renders
   attacker-controlled HTML in a sandboxed frame instead. */
const INNERHTML_ALLOWED_TARGET = /^(?:this\.)?(editor|richHtml)$/;
const INNERHTML_ALLOWED_SOURCE =
  /^(?:richHtml|textToHtml\(|quoteHtml\(|seed\.[a-z]+|source\.[a-z]+|`\$\{richHtml\}|`\$\{editor\.innerHTML\})/;

for (const file of jsFiles) {
  const source = readFileSync(file, 'utf8');
  if (!file.endsWith('check.mjs')) {
    for (const match of source.matchAll(/console\.(log|debug|info)\(/g)) {
      fail(file, 'js:console', `leftover console.${match[1]} call`);
    }
  }  check(!/\beval\s*\(/.test(source), file, 'js:eval', 'eval() is not allowed');

  for (const line of source.split('\n')) {
    const assignment = /([A-Za-z_$][\w$.]*)\.innerHTML\s*=\s*(.+?);\s*$/.exec(line.trim());
    if (!assignment) continue;
    const target = assignment[1];
    const value = assignment[2].trim();
    check(
      (INNERHTML_ALLOWED_TARGET.test(target) && INNERHTML_ALLOWED_SOURCE.test(value)) ||
        // Template content is inert and never inserted into the live document: used
        // only to extract quoted text from an HTML-only message before composing.
        (/[\\/]web[\\/]compose\.js$/.test(file) && target === 'template' && value === "String(source.html || '')"),
      file,
      'js:innerhtml',
      `innerHTML assigned on "${target}" from "${value.slice(0, 48)}"`,
    );
  }
}

/* The only place untrusted HTML is rendered: a script-less sandboxed frame.
   Webmail-specific, so it is skipped for a console-only app. */
if (appProfile === 'webmail') {
  const reader = join(ROOT, 'reader.js');
  if (existsSync(reader)) {
    const source = readFileSync(reader, 'utf8');
    check(
      /setAttribute\(\s*'srcdoc'/.test(source),
      reader,
      'js:html-body',
      'the reading pane must render the HTML part through srcdoc',
    );
  }
  const index = join(ROOT, 'index.html');
  if (existsSync(index)) {
    const source = readFileSync(index, 'utf8');
    // The rule is what the sandbox must *deny*, not which tokens it may carry.
    // `docs/security.md` §10.2 asks for `allow-popups` so a link in a message can open in
    // a new tab; asserting the attribute was literally empty made that impossible and
    // failed the documented design. Scripts and same-origin access stay forbidden.
    const frame = /<iframe[^>]*\bsandbox="([^"]*)"/i.exec(source);
    check(
      frame !== null,
      index,
      'html:sandbox',
      'the message HTML frame must carry a sandbox attribute',
    );
    if (frame) {
      const tokens = frame[1].split(/\s+/).filter(Boolean);
      check(
        !tokens.includes('allow-scripts') && !tokens.includes('allow-same-origin'),
        index,
        'html:sandbox',
        `the message frame may not allow scripts or same-origin access: sandbox="${frame[1]}"`,
      );
    }
  }
}

/* ------------------------------------------------------------------- rule 7 */

for (const file of jsFiles) {
  const source = readFileSync(file, 'utf8');
  const importBlocks = Array.from(source.matchAll(/(?:^|\n)\s*import\s+([\s\S]*?)\s+from\s+['"][^'"]+['"]/g));
  for (const block of importBlocks) {
    const clause = block[1];
    const named = clause.match(/\{([\s\S]*)\}/);
    if (!named) continue;
    for (const piece of named[1].split(',')) {
      const name = piece.trim().split(/\s+as\s+/).pop().trim();
      if (name === '') continue;
      const uses = source.split(new RegExp(`\\b${name}\\b`)).length - 1;
      check(uses > 1, file, 'js:unused-import', `"${name}" is imported but never used`);
    }
  }
}

/* ------------------------------------------------------------------- rule 9 */

/* Cross-module wiring: every named export must be used somewhere (inside its own
   module or by an importer), which also catches a renamed export that an import
   list still refers to under the old name. */

/** module path -> exported names */
const exportMap = new Map();

for (const file of jsFiles) {
  const source = readFileSync(file, 'utf8');
  const names = new Set();
  for (const match of source.matchAll(/(?:^|\n)\s*export\s+(?:async\s+)?(?:function|const|let|class)\s+([A-Za-z_$][\w$]*)/g)) {
    names.add(match[1]);
  }
  for (const match of source.matchAll(/(?:^|\n)\s*export\s*\{([\s\S]*?)\}/g)) {
    for (const piece of match[1].split(',')) {
      const parts = piece.trim().split(/\s+as\s+/);
      const name = (parts[1] || parts[0]).trim();
      if (name !== '') names.add(name);
    }
  }
  exportMap.set(resolve(file), names);
}

/** Exported names that are referenced from another module. */
const importedNames = new Map();

for (const file of jsFiles) {
  const source = readFileSync(file, 'utf8');
  const importTargets = [];
  for (const match of source.matchAll(/(?:^|\n)\s*import\s+([\s\S]*?)\s+from\s+['"]([^'"]+)['"]/g)) {
    importTargets.push({ clause: match[1], specifier: match[2], dynamic: false });
  }
  // Dynamic `import('./views/x.js')` counts as an importer too.
  for (const match of source.matchAll(/\bimport\(\s*["']([^"']+)["']\s*\)/g)) {
    importTargets.push({ clause: '', specifier: match[1], dynamic: true });
  }

  for (const entry of importTargets) {
    if (!entry.specifier.startsWith('.')) continue;
    const target = resolve(dirname(file), entry.specifier);
    if (!importedNames.has(target)) importedNames.set(target, new Set());
    if (entry.dynamic) {
      // The property touched on the imported namespace is not statically known,
      // so the module counts as fully used.
      importedNames.get(target).add('*');
    }
    const clause = entry.clause;
    const defaultMatch = /^\s*([A-Za-z_$][\w$]*)\s*(?:,|$)/.exec(clause);
    if (defaultMatch) importedNames.get(target).add(defaultMatch[1]);
    const named = clause.match(/\{([\s\S]*)\}/);
    if (!named) continue;
    for (const piece of named[1].split(',')) {
      const parts = piece.trim().split(/\s+as\s+/);
      const original = parts[0].trim();
      if (original === '') continue;
      importedNames.get(target).add(original);
      check(
        exportMap.has(target) && exportMap.get(target).has(original),
        file,
        'js:unknown-import',
        `"${original}" is not exported by ${relative(ROOT, target).replace(/\\/g, '/')}`,
      );
    }
  }
}

for (const [file, names] of exportMap) {
  // A leaf module nothing imports cannot have a stale export; a shared module has
  // a second consumer in the sibling app.
  const base = relative(ROOT, file).replace(/\\/g, '/').split('/').pop();
  if (SHARED_MODULES.has(base)) continue;
  if (!importedNames.has(file)) continue;
  const source = readFileSync(file, 'utf8');
  const otherModulesUse = importedNames.get(file) || new Set();
  if (otherModulesUse.has('*')) continue;
  for (const name of names) {
    const localUses = source.split(new RegExp(`\\b${name}\\b`)).length - 1;
    check(
      localUses > 1 || otherModulesUse.has(name),
      file,
      'js:unused-export',
      `"${name}" is exported but never used`,
    );
  }
}

/* ----------------------------------------------------------------- rule 10 */

/* The payload envelopes the server really sends.
 *
 * Every list in this app funnels through `data.js`, and `listOf()` is the only thing
 * standing between a response and the screen. The static rules above cannot see a
 * mismatched key: they prove the modules link, not that the JSON matches. That gap
 * shipped a webmail whose folder tree was empty for every account — the server sent
 * `{mailbox_id, folders: […]}` and `listOf()` looked for `items`, so `foldersOf()`
 * returned `[]`, the shell could not resolve a folder, and the message list never
 * loaded.
 *
 * Each case below is the exact envelope a Rust handler serialises. It asserts the
 * app's own normaliser turns it into rows, so renaming a collection on either side
 * fails here instead of in a browser. */
// `import()` resolves against *this* file, not the app directory, so the shared
// normalisers are loaded by absolute file URL.
const data = await import(pathToFileURL(join(SHARED_ROOT, 'data.js')).href);

const ENVELOPES = [
  ['`{items, total}`, the paged lists', { items: [{ id: 1 }], total: 1 }, (payload) => data.listOf(payload)],
  ['`{items, total}` domains', { items: [{ id: 1, name: 'example.test' }], total: 1 }, (payload) => data.domainsOf(payload)],
  ['`{items, total}` users', { items: [{ id: 1, email: 'a@example.test' }], total: 1 }, (payload) => data.usersOf(payload)],
  ['`{items, total}` aliases', { items: [{ id: 1, local_part: 'sales' }], total: 1 }, (payload) => data.aliasesOf(payload)],
  ['`{items, total}` queue entries', { items: [{ id: 1, recipient: 'a@example.test' }], total: 1 }, (payload) => data.queueEntriesOf(payload)],
  ['`{items, total}` messages', { items: [{ id: 1, subject: 'hi' }], total: 1 }, (payload) => data.messagesOf(payload)],
  ['`{mailboxes}` from GET /mailboxes', { mailboxes: [{ id: 1, address: 'a@example.test' }] }, (payload) => data.mailboxesOf(payload)],
  [
    '`{mailbox_id, folders}` from GET /mailboxes/:id/folders',
    { mailbox_id: 1, folders: [{ id: 2, name: 'INBOX' }] },
    (payload) => data.foldersOf(payload),
  ],
  ['a bare array, for a handler that does not page', [{ id: 1 }], (payload) => data.listOf(payload)],
];

for (const [name, payload, parse] of ENVELOPES) {
  check(
    parse(payload).length === 1,
    join(ROOT, 'data.js'),
    'data:envelope',
    `${name} normalises to no rows; the app would render an empty screen for a response that has rows`,
  );
}

/* The dashboard stat grid. Three of its twelve cards read a *nested* key — today's
   traffic under `health.queue`, the live session count under `health.clients` — and
   reading any of them one level too high leaves the card stuck on “—”, which reads
   as “this API does not report it”. These are the shapes the Rust handlers emit. */
const DASHBOARD_HEALTH = {
  status: 'ok',
  uptime_secs: 3600,
  database: { ok: true, server_version: '16.15', pool: { size: 3, idle: 2, max: 20 } },
  clients: { active_sessions: 4, active_devices: 2 },
  queue: { pending: 5, delivering: 1, retry: 2, failed: 3, received_today: 128, sent_today: 41 },
};
const DASHBOARD_QUEUE_STATS = {
  pending: 5,
  delivering: 1,
  retry: 2,
  delivered: 9,
  failed: 3,
  cancelled: 0,
  outstanding: 8,
  next_due_at: null,
};
const DASHBOARD_STORAGE = {
  maildir_bytes: 1024,
  attachment_bytes: 2048,
  database_bytes: 4096,
  mailboxes: 3,
  messages: 12,
  users: 7,
  domains: 2,
};

const dashboard = data.dashboardStats(DASHBOARD_HEALTH, DASHBOARD_QUEUE_STATS, DASHBOARD_STORAGE);

const DASHBOARD_CARDS = [
  ['Users', dashboard.users, 7],
  ['Domains', dashboard.domains, 2],
  ['Received today', dashboard.receivedToday, 128],
  ['Sent today', dashboard.sentToday, 41],
  ['Queue pending', dashboard.queuePending, 5],
  ['Queue retry', dashboard.queueRetry, 2],
  ['Failed deliveries', dashboard.failedDeliveries, 3],
  ['Mailbox storage', dashboard.maildirBytes, 1024],
  ['Attachments', dashboard.attachmentBytes, 2048],
  ['Database size', dashboard.databaseBytes, 4096],
  ['Active client sessions', dashboard.activeClientSessions, 4],
  ['Uptime', dashboard.uptimeSecs, 3600],
];

for (const [label, value, want] of DASHBOARD_CARDS) {
  check(
    value === want,
    join(ROOT, 'data.js'),
    'data:dashboard',
    `the “${label}” card reads ${String(value)} where the API reports ${String(want)}`,
  );
}

/* The two normaliser contracts the API and the app disagreed about.
 *
 * `GET /mailboxes/:id/folders` sends `special_use: null` for INBOX — RFC 6154 defines
 * no `\Inbox` attribute and `docs/fcp.md` §4 freezes the null — so the inbox has to be
 * recognised by name. Failing to do so sorted it last, after Trash, and left it without
 * an `/inbox` slug. In the other direction, `flags` is the canonical space-separated
 * lower-case string (`seen flagged`), so matching only the IMAP `\Seen` spelling left
 * every row unread and every star hidden. Both are asserted here because neither is
 * visible to the rules above. */
const INBOX_FOLDER = data.normalizeFolder({ id: 3, name: 'INBOX', special_use: null, message_count: 4 });
check(
  INBOX_FOLDER.specialUse === 'inbox',
  join(SHARED_ROOT, 'data.js'),
  'data:folder',
  `INBOX normalises to specialUse ${String(INBOX_FOLDER.specialUse)}; the tree orders and links by that slug`,
);
check(
  INBOX_FOLDER.sortKey === 0,
  join(SHARED_ROOT, 'data.js'),
  'data:folder',
  `INBOX sorts at ${INBOX_FOLDER.sortKey} instead of first`,
);
const SENT_FOLDER = data.normalizeFolder({ id: 4, name: 'Sent', special_use: '\\Sent' });
check(
  SENT_FOLDER.specialUse === 'sent',
  join(SHARED_ROOT, 'data.js'),
  'data:folder',
  `the IMAP spelling \\Sent normalises to ${String(SENT_FOLDER.specialUse)}`,
);

const CANONICAL_FLAGS = data.normalizeMessage({ id: 1, flags: 'seen flagged' });
check(
  CANONICAL_FLAGS.seen === true && CANONICAL_FLAGS.flagged === true && CANONICAL_FLAGS.answered === false,
  join(SHARED_ROOT, 'data.js'),
  'data:flags',
  'the canonical `seen flagged` string must set seen and flagged and leave answered alone',
);
const IMAP_FLAGS = data.normalizeMessage({ id: 1, flags: ['\\Seen', '\\Flagged'] });
check(
  IMAP_FLAGS.seen === true && IMAP_FLAGS.flagged === true,
  join(SHARED_ROOT, 'data.js'),
  'data:flags',
  'the IMAP spelling must keep working for a source that spells flags that way',
);

/* ----------------------------------------------------------------- rule 11 */

/* Localisation coverage.
 *
 * Every `t('…')` key must exist in the Simplified Chinese catalog. A missing key is not
 * a crash — `t()` falls back to the English text — which is exactly why it needs a rule:
 * a screen nobody translated looks finished to anyone who reads only English, and the
 * gap only surfaces to the reader it was meant for. */
const i18n = await import(pathToFileURL(join(SHARED_ROOT, 'i18n.js')).href);

/** Undo the escaping in a matched literal, so a key holding a newline still matches. */
function unescapeLiteral(text) {
  return text.replace(/\\(u[0-9a-fA-F]{4}|x[0-9a-fA-F]{2}|[\s\S])/g, (whole, escape) => {
    switch (escape) {
      case 'n': return '\n';
      case 'r': return '\r';
      case 't': return '\t';
      case 'b': return '\b';
      case 'f': return '\f';
      case '0': return '\0';
      default:
        if (escape[0] === 'u' || escape[0] === 'x') {
          return String.fromCharCode(parseInt(escape.slice(1), 16));
        }
        // `\\`, `\'`, `\"` and every other escaped character stand for themselves.
        return escape;
    }
  });
}

/**
 * Remove comments, so an example inside one is not mistaken for a call.
 *
 * A `t('…')` in a doc comment is documentation, not a string the app can show, and
 * demanding a catalog entry for it would punish writing the example down. String
 * literals are copied through untouched, which is what keeps the `//` in a URL from
 * truncating the rest of a line.
 */
function stripComments(source) {
  let out = '';
  let i = 0;
  while (i < source.length) {
    const char = source[i];
    const next = source[i + 1];
    if (char === '/' && next === '/') {
      while (i < source.length && source[i] !== '\n') i += 1;
      continue;
    }
    if (char === '/' && next === '*') {
      i += 2;
      while (i < source.length && !(source[i] === '*' && source[i + 1] === '/')) i += 1;
      i += 2;
      continue;
    }
    if (char === "'" || char === '"' || char === '`') {
      out += char;
      i += 1;
      while (i < source.length) {
        if (source[i] === '\\') {
          out += source[i] + (source[i + 1] || '');
          i += 2;
          continue;
        }
        out += source[i];
        if (source[i] === char) {
          i += 1;
          break;
        }
        i += 1;
      }
      continue;
    }
    out += char;
    i += 1;
  }
  return out;
}

/** The literal keys this module calls `t(...)` and `tn(...)` with. */
function i18nKeys(source) {
  const keys = new Set();
  for (const match of source.matchAll(/\bt\(\s*'((?:[^'\\]|\\.)*)'/g)) keys.add(unescapeLiteral(match[1]));
  for (const match of source.matchAll(/\bt\(\s*"((?:[^"\\]|\\.)*)"/g)) keys.add(unescapeLiteral(match[1]));
  for (const match of source.matchAll(/\btn\([^,]+,\s*'((?:[^'\\]|\\.)*)'\s*,\s*'((?:[^'\\]|\\.)*)'/g)) {
    keys.add(unescapeLiteral(match[1]));
    keys.add(unescapeLiteral(match[2]));
  }
  return keys;
}

const CATALOG = i18n.catalogs()['zh-CN'] || {};

/* The app shell, in both directions. The marks are explicit, so unlike the JS above
 * there is no guessing: a marked node must have a translation, and a static text node
 * the catalog already knows must carry a mark. Without the second half a new string
 * could be added to `index.html` and stay English with every check green. */
for (const file of htmlFiles) {
  const source = readFileSync(file, 'utf8').replace(/<!--[\s\S]*?-->/g, '');
  const known = (key) => key !== '' && Object.prototype.hasOwnProperty.call(CATALOG, key);

  for (const match of source.matchAll(
    /<([a-zA-Z][\w-]*)((?:[^>"']|"[^"]*"|'[^']*')*?)data-i18n(?=[\s>])((?:[^>"']|"[^"]*"|'[^']*')*)>([^<]*)</g,
  )) {
    const key = match[4].trim();
    check(
      known(key),
      file,
      'i18n:missing',
      `a data-i18n element holds ${JSON.stringify(key)}, which is not in the catalog`,
    );
  }

  // These markers are flags, not values: `data-i18n-title title="Compose (c)"` marks
  // the value held in the real attribute, which is what the runtime reads.
  for (const [marker, attribute] of [
    ['data-i18n-placeholder', 'placeholder'],
    ['data-i18n-title', 'title'],
    ['data-i18n-aria-label', 'aria-label'],
  ]) {
    const marked = new RegExp(`(?:^|\\s)${marker}(?:\\s|$)`);
    const valued = new RegExp(`(?:^|\\s)${attribute}\\s*=\\s*"([^"]*)"`);
    for (const element of source.matchAll(/<([a-zA-Z][\w-]*)((?:[^>"']|"[^"]*"|'[^']*')*)>/g)) {
      if (!marked.test(element[2])) continue;
      const value = valued.exec(element[2]);
      const key = value ? value[1].trim() : '';
      check(
        known(key),
        file,
        'i18n:missing',
        `${marker} marks ${attribute}=${JSON.stringify(key)}, which is not in the catalog`,
      );
    }
  }

  for (const match of source.matchAll(
    /<([a-zA-Z][\w-]*)((?:[^>"']|"[^"]*"|'[^']*')*)>([^<]+)<\/\1>/g,
  )) {
    const key = match[3].trim();
    if (!known(key)) continue;
    check(
      // The attribute group stops just before the `>`, so the marker sits at the end of
      // the string with nothing after it — a lookahead for whitespace would never fire.
      /(?:^|\s)data-i18n(?:\s|$)/.test(match[2]),
      file,
      'i18n:unmarked',
      `the text ${JSON.stringify(key)} has a translation but no data-i18n marker, so it stays English`,
    );
  }
}


for (const file of jsFiles) {
  for (const key of i18nKeys(stripComments(readFileSync(file, 'utf8')))) {
    check(
      Object.prototype.hasOwnProperty.call(CATALOG, key),
      file,
      'i18n:missing',
      `t(${JSON.stringify(key)}) has no Simplified Chinese translation`,
    );
  }
}

/* ------------------------------------------------------------------ report */

const summary = [
  `static check — ${relative(process.cwd(), ROOT).replace(/\\/g, '/') || '.'} (profile: ${appProfile})`,
  `  files      : ${files.length} (${htmlFiles.length} html, ${jsFiles.length} js, ${cssFiles.length} css)`,
  `  assertions : ${checks}`,
  `  ids known  : ${declaredIds.size}`,
];

if (notes.length) {
  summary.push('  notes      :');
  for (const note of notes) summary.push(`    - ${note}`);
}

if (failures.length === 0) {
  summary.push('  result     : PASS — no violations');
  process.stdout.write(`${summary.join('\n')}\n`);
  process.exit(0);
}

summary.push(`  result     : FAIL — ${failures.length} violation(s)`);
for (const item of failures) {
  summary.push(`    [${item.rule}] ${item.file}: ${item.message}`);
}
process.stdout.write(`${summary.join('\n')}\n`);
process.exit(1);
