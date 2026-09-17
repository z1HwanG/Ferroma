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
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

/**
 * Modules duplicated verbatim into both apps by design (the server serves `web/`
 * and `admin/` independently). Their exports are shared between the two apps, so
 * the dead-export rule cannot judge them per directory and skips them.
 */
const SHARED_MODULES = new Set(['api.js', 'data.js', 'dom.js', 'format.js', 'net.js', 'theme.js', 'toast.js', 'modal.js']);

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
      INNERHTML_ALLOWED_TARGET.test(target) && INNERHTML_ALLOWED_SOURCE.test(value),
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
    check(
      /sandbox=""/.test(source),
      index,
      'html:sandbox',
      'the message HTML frame must carry an empty sandbox attribute',
    );
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
const data = await import('../data.js');

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
