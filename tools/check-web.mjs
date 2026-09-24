#!/usr/bin/env node
/**
 * The one link error the front-ends' own checks cannot see.
 *
 * `web/tools/check.mjs` and `admin/tools/check.mjs` are the suites — 584 and 601
 * assertions per app, over element ids, asset paths, module imports and exports. Run
 * those first; this script exists only for the rule they do not have.
 *
 * They validate imports that *exist*. Neither notices a **call** to a name the module
 * never introduced, because such a call imports nothing to check. That is exactly how
 * `renderChrome` — defined but not exported in `web/reader.js`, called six times in
 * `web/main.js` with no import — reached a published image: in an ES module it is a
 * ReferenceError on the first call, and the first call sits inside `start()`, so the
 * webmail never initialised while every other check stayed green. Measured on that
 * revision, the per-app check reported "PASS — no violations".
 *
 * Two fatal, invisible-until-runtime link errors are reported here:
 *
 *   * a call to a name the file neither declares nor imports — `ReferenceError` at
 *     the first call, which in a module with a top-level start() kills the whole app;
 *   * an `import { x } from './y.js'` where `y.js` does not export `x` — a
 *     `SyntaxError` at link time, which stops the module graph from evaluating at all.
 *
 * This script also runs the app-local regression suites under `web/tools/`. They
 * carry rules the per-app check has no place for — they assert *behaviour* of pure
 * helpers, which is what a static id/import sweep cannot do. Nothing ran them
 * before, so a regression they were written to catch would have reached a release
 * with every documented check still green.
 *
 *   node tools/check-web.mjs
 */
import fs from 'node:fs';
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

/** The repository root, so the app-local suites run from anywhere. */
const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

const ROOTS = ['web', 'admin'];

/** Words that look like a call but are syntax, or are simply always available. */
const KEYWORDS = new Set([
  'if', 'for', 'while', 'switch', 'catch', 'return', 'typeof', 'function', 'new',
  'do', 'else', 'case', 'delete', 'void', 'in', 'of', 'instanceof', 'await', 'yield',
  'constructor', 'super', 'async', 'import', 'export', 'get', 'set',
]);

const GLOBALS = new Set([
  'window', 'document', 'console', 'Math', 'JSON', 'Object', 'Array', 'String', 'Number',
  'Boolean', 'Promise', 'Set', 'Map', 'WeakMap', 'WeakSet', 'Date', 'Error', 'TypeError',
  'RangeError', 'SyntaxError', 'ReferenceError', 'EvalError', 'AggregateError',
  'fetch', 'parseInt', 'parseFloat', 'isNaN', 'isFinite', 'setTimeout', 'clearTimeout',
  'setInterval', 'clearInterval', 'requestAnimationFrame', 'cancelAnimationFrame',
  'encodeURIComponent', 'decodeURIComponent', 'encodeURI', 'decodeURI', 'btoa', 'atob',
  'structuredClone', 'queueMicrotask', 'URL', 'URLSearchParams', 'Intl', 'RegExp',
  'Symbol', 'Proxy', 'Reflect', 'BigInt', 'FormData', 'Blob', 'File', 'FileReader',
  'XMLHttpRequest', 'Headers', 'Request', 'Response', 'AbortController', 'AbortSignal',
  'DOMParser', 'XMLSerializer', 'IntersectionObserver', 'ResizeObserver',
  'MutationObserver', 'localStorage', 'sessionStorage', 'CustomEvent', 'Event', 'Image',
  'Notification', 'crypto', 'performance', 'history', 'location', 'navigator',
  'matchMedia', 'getComputedStyle', 'alert', 'confirm', 'prompt', 'TextEncoder',
  'TextDecoder', 'atob', 'btoa', 'define', 'require',
  // Typed arrays are language built-ins. `new Uint8Array(buffer)` is how an
  // inline image becomes base64; without these names the call looks undeclared.
  'Uint8Array', 'Uint8ClampedArray', 'Uint16Array', 'Uint32Array', 'BigUint64Array',
  'Int8Array', 'Int16Array', 'Int32Array', 'BigInt64Array', 'Float32Array',
  'Float64Array', 'ArrayBuffer', 'DataView', 'SharedArrayBuffer',
]);

const problems = [];
const notes = [];

/** Blank out comments so prose can never look like code. Length is preserved. */
function withoutComments(source) {
  return source
    .replace(/\/\*[\s\S]*?\*\//g, (m) => ' '.repeat(m.length))
    .replace(/(^|[^:'"`\\])\/\/[^\n]*/g, (m, p1) => p1 + ' '.repeat(m.length - p1.length));
}

/**
 * Blank out string and template literals, keeping every offset and newline.
 *
 * Message text reaches this code a lot — `\`Request failed (HTTP ${status})\`` contains
 * `failed (`, which looks exactly like a call to a function named `failed`. Nothing
 * inside a literal can be a call, so it is removed rather than argued about. The one
 * thing this gives up is a call written inside a `${…}` interpolation, which is rare
 * enough to trade for a check that reports nothing but the truth.
 */
function withoutLiterals(source) {
  let out = '';
  let i = 0;
  while (i < source.length) {
    const ch = source[i];
    if (ch === "'" || ch === '"' || ch === '`') {
      const quote = ch;
      out += ' ';
      i++;
      while (i < source.length) {
        if (source[i] === '\\') {
          out += '  ';
          i += 2;
          continue;
        }
        if (source[i] === '\n' && quote !== '`') break; // unterminated on this line
        if (source[i] === quote) {
          out += ' ';
          i++;
          break;
        }
        out += source[i] === '\n' ? '\n' : ' ';
        i++;
      }
      continue;
    }
    out += ch;
    i++;
  }
  return out;
}

/** Names a module introduces: what it imports, and what it declares at any level. */
function introducedNames(source) {
  const names = new Set();

  for (const m of source.matchAll(/import\s+([\s\S]*?)\s+from\s*['"]([^'"]+)['"]/g)) {
    const clause = m[1];
    const braced = clause.match(/\{([\s\S]*)\}/);
    if (braced) {
      for (const part of braced[1].split(',')) {
        const name = part.trim().split(/\s+as\s+/).pop().trim();
        if (/^[A-Za-z_$][\w$]*$/.test(name)) names.add(name);
      }
    }
    const fallback = clause.replace(/\{[\s\S]*\}/, '').replace(/,/g, '').trim();
    if (/^[A-Za-z_$][\w$]*$/.test(fallback)) names.add(fallback);
  }

  // Declarations are collected from the *raw* source on purpose: a name that only
  // appears inside a template literal still means this file mentions it, and being
  // over-generous here only makes the check quieter — never wrong in the direction
  // that matters, which is reporting a name that really is missing.
  for (const m of source.matchAll(/\bfunction\s+([A-Za-z_$][\w$]*)/g)) names.add(m[1]);
  for (const m of source.matchAll(/\bclass\s+([A-Za-z_$][\w$]*)/g)) names.add(m[1]);
  for (const m of source.matchAll(/\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)/g)) names.add(m[1]);
  for (const m of source.matchAll(/\b(?:const|let|var)\s*\{([^}]*)\}/g)) {
    for (const part of m[1].split(',')) {
      const name = part.split(':').pop().replace(/=.*/, '').trim();
      if (/^[A-Za-z_$][\w$]*$/.test(name)) names.add(name);
    }
  }
  // Parameters of arrow functions and of `function`/`catch` clauses.
  for (const m of source.matchAll(/\(([^()]*)\)\s*(?:=>|\{)/g)) {
    for (const part of m[1].split(',')) {
      const name = part.trim().replace(/^\.\.\./, '').replace(/=.*/, '').trim();
      if (/^[A-Za-z_$][\w$]*$/.test(name)) names.add(name);
    }
  }
  for (const m of source.matchAll(/(?:^|[^\w$.])([A-Za-z_$][\w$]*)\s*=>/g)) names.add(m[1]);

  return names;
}

/** Every name a module exports, for the import check. */
function exportedNames(source) {
  const names = new Set();
  for (const m of source.matchAll(/export\s+(?:async\s+)?function\s+([A-Za-z_$][\w$]*)/g)) names.add(m[1]);
  for (const m of source.matchAll(/export\s+class\s+([A-Za-z_$][\w$]*)/g)) names.add(m[1]);
  for (const m of source.matchAll(/export\s+(?:const|let|var)\s+([A-Za-z_$][\w$]*)/g)) names.add(m[1]);
  for (const m of source.matchAll(/export\s*\{([^}]*)\}/g)) {
    for (const part of m[1].split(',')) {
      const name = part.trim().split(/\s+as\s+/).shift().trim();
      if (/^[A-Za-z_$][\w$]*$/.test(name)) names.add(name);
    }
  }
  if (/export\s+default\b/.test(source)) names.add('default');
  return names;
}

function analyse(file) {
  const raw = fs.readFileSync(file, 'utf8');
  // Literals first: a quote inside a comment or a `/*` inside a string would
  // otherwise make the other pass eat real code.
  const code = withoutComments(withoutLiterals(raw));
  const introduced = introducedNames(raw);
  const lineOf = (index) => raw.slice(0, index).split('\n').length;

  const findings = [];

  // 1. Calls to a name this file never introduces.
  for (const m of code.matchAll(/(^|[^.\w$])([A-Za-z_$][\w$]*)\s*\(/g)) {
    const name = m[2];
    if (KEYWORDS.has(name) || GLOBALS.has(name) || introduced.has(name)) continue;
    if (findings.some((f) => f.name === name)) continue;

    // `{ report(x) { … } }` is a method *definition* in an object literal, not a
    // call. The two are identical until you follow the parentheses to what comes
    // after: a call continues with `;`/`,`/`)`, a definition opens its body.
    const open = m.index + m[0].length - 1;
    let depth = 0;
    let i = open;
    for (; i < code.length; i++) {
      if (code[i] === '(') depth++;
      else if (code[i] === ')') {
        depth--;
        if (depth === 0) break;
      }
    }
    if ((code.slice(i + 1).match(/^\s*([^\s])/) || [])[1] === '{') continue;

    findings.push({ name, line: lineOf(m.index), kind: 'call' });
  }

  // 2. Imports of a name the target module does not export — a link-time SyntaxError.
  for (const m of raw.matchAll(/import\s+([\s\S]*?)\s+from\s*['"](\.[^'"]+)['"]/g)) {
    const braced = m[1].match(/\{([\s\S]*)\}/);
    if (!braced) continue;
    const target = path.join(path.dirname(file), m[2]);
    if (!fs.existsSync(target)) {
      findings.push({ name: m[2], line: lineOf(m.index), kind: 'missing-module' });
      continue;
    }
    const available = exportedNames(fs.readFileSync(target, 'utf8'));
    for (const part of braced[1].split(',')) {
      const name = part.trim().split(/\s+as\s+/).shift().trim();
      if (!/^[A-Za-z_$][\w$]*$/.test(name)) continue;
      if (!available.has(name)) {
        findings.push({ name, line: lineOf(m.index), kind: 'import', target: m[2] });
      }
    }
  }

  return findings;
}

let modules = 0;
for (const root of ROOTS) {
  for (const entry of fs.readdirSync(root).filter((f) => f.endsWith('.js')).sort()) {
    const file = path.join(root, entry);
    modules++;
    const findings = analyse(file);
    if (findings.length === 0) {
      notes.push(`${file}`);
      continue;
    }
    for (const f of findings) {
      if (f.kind === 'call') {
        problems.push(
          `${file}:${f.line} calls ${f.name}(), which this module neither declares nor ` +
            `imports — a ReferenceError the moment that line runs`,
        );
      } else if (f.kind === 'import') {
        problems.push(
          `${file}:${f.line} imports ${f.name} from '${f.target}', which does not export it ` +
            `— a SyntaxError before any of this runs`,
        );
      } else {
        problems.push(`${file}:${f.line} imports from '${f.name}', which does not exist`);
      }
    }
  }
}

/* ------------------------------------------------- the app-local suites */

const LOCAL_SUITES = [
  ['web', 'tools/mfa-ui.mjs'],
  ['web', 'tools/list-race.mjs'],
  ['web', 'tools/security-regressions.mjs'],
];

const suiteProblems = [];
for (const [app, script] of LOCAL_SUITES) {
  const run = spawnSync(process.execPath, [script], {
    cwd: path.join(REPO_ROOT, app),
    encoding: 'utf8',
  });
  if (run.status !== 0) {
    const detail = `${run.stdout || ''}${run.stderr || ''}`.trim().split('\n').slice(0, 4).join('; ');
    suiteProblems.push(`${app}/${script} failed: ${detail || `exit ${run.status}`}`);
  } else {
    console.log(`  suite  ${app}/${script} — ${(run.stdout || '').trim()}`);
  }
}

console.log('frontend check');
console.log(`  note   modules scanned: ${modules}`);
if (problems.length === 0 && suiteProblems.length === 0) {
  console.log('  result PASS — no link errors');
  process.exit(0);
}
for (const problem of problems) console.log(`  FAIL   ${problem}`);
for (const problem of suiteProblems) console.log(`  FAIL   ${problem}`);
const total = problems.length + suiteProblems.length;
console.log(`  result FAIL — ${total} problem(s)`);
process.exit(1);
