#!/usr/bin/env node
/**
 * Check that the ten Chinese documents read as one document set.
 *
 * Ten translators working in parallel produce ten internally-consistent documents
 * and, without a pass like this, a set that contradicts itself: three renderings of
 * "specification §N", some files translating the `_(planned)_` marker and others not,
 * and two different conventions for the space between Chinese and Latin text. None of
 * those is a translation error, and all of them are visible to a reader.
 *
 *   node tools/check-zh.mjs
 */
import fs from 'node:fs';
import path from 'node:path';

const root = process.cwd();
const zhDir = path.join(root, 'docs', 'zh');
const enDir = path.join(root, 'docs');

const read = (p) => fs.readFileSync(p, 'utf8');

/** Strip fenced blocks and inline code, leaving prose. */
function prose(text) {
  return text.replace(/```[\s\S]*?```/g, '').replace(/`[^`\n]*`/g, '');
}

const CJK = '\u3400-\u4dbf\u4e00-\u9fff\u3000-\u303f\uff01-\uff60';

function spacing(text) {
  const p = prose(text);
  const spaced =
    (p.match(new RegExp(`[${CJK}] +[A-Za-z0-9]`, 'g')) || []).length +
    (p.match(new RegExp(`[A-Za-z0-9] +[${CJK}]`, 'g')) || []).length;
  const tight =
    (p.match(new RegExp(`[${CJK}][A-Za-z0-9]`, 'g')) || []).length +
    (p.match(new RegExp(`[A-Za-z0-9][${CJK}]`, 'g')) || []).length;
  return { spaced, tight };
}

const problems = [];
const notes = [];

// --- 1. what the project book does ------------------------------------------
// Optional: the project book is no longer part of the tree, so this reports the
// convention only when a copy is sitting next to the sources. Everything below runs
// either way; the step was never a check, only a note.
const specPath = path.join(root, 'Ferroma-完整项目书.md');
if (fs.existsSync(specPath)) {
  const spec = read(specPath);
  const specSpacing = spacing(spec);
  const specRatio = specSpacing.spaced / Math.max(1, specSpacing.spaced + specSpacing.tight);
  notes.push(
    `project book prose: ${specSpacing.spaced} spaced vs ${specSpacing.tight} tight ` +
      `(${(specRatio * 100).toFixed(0)}% spaced) -> convention is ` +
      `${specRatio > 0.5 ? 'SPACE' : 'NO SPACE'}`,
  );
}

// --- 2. per-file structure parity and terminology ---------------------------
// The two glossaries are the terminology source for each language. They are not a
// translated pair — they record the same decisions in different words, and the
// Chinese one is the authority — so the parity rows below skip them. Their
// existence in both directories is still checked, through `zhFiles`.
const zhFiles = fs.readdirSync(zhDir).filter((f) => f.endsWith('.md'));
const files = zhFiles.filter((f) => f !== 'GLOSSARY.md');

// A single bilingual artifact rather than a translated pair: its Chinese half lives
// inside the same file, because it is pasted into a page with no language switch
// (the Docker Hub repository description). There is nothing in docs/zh/ to compare
// it against, so it is checked for holding both halves in the right order instead
// of being reported as untranslated below.
const bilingual = new Set(['dockerhub.md']);

const missing = fs
  .readdirSync(enDir)
  .filter((f) => f.endsWith('.md') && !zhFiles.includes(f) && !bilingual.has(f));

let totalLines = 0;
let totalEnLines = 0;

console.log('file                 lines(en/zh)  fences(en/zh)  headings(en/zh)  spaced/tight');
for (const file of files.sort()) {
  const en = read(path.join(enDir, file));
  const zh = read(path.join(zhDir, file));

  const count = (t, re) => (t.match(re) || []).length;
  const enLines = en.split('\n').length;
  const zhLines = zh.split('\n').length;
  totalLines += zhLines;
  totalEnLines += enLines;

  const enFences = count(en, /^```/gm);
  const zhFences = count(zh, /^```/gm);
  const enHeads = count(en, /^#{2,3} /gm);
  const zhHeads = count(zh, /^#{2,3} /gm);
  const s = spacing(zh);

  console.log(
    `${file.padEnd(20)} ${String(enLines).padStart(4)}/${String(zhLines).padEnd(6)} ` +
      `${String(enFences).padStart(4)}/${String(zhFences).padEnd(7)} ` +
      `${String(enHeads).padStart(4)}/${String(zhHeads).padEnd(8)} ` +
      `${s.spaced}/${s.tight}`,
  );

  if (enFences !== zhFences) {
    problems.push(`${file}: ${enFences} fences in English, ${zhFences} in Chinese — a block was dropped or added`);
  }
  if (enHeads !== zhHeads) {
    problems.push(`${file}: ${enHeads} headings in English, ${zhHeads} in Chinese — a section was dropped or added`);
  }
  if (Math.abs(zhLines - enLines) / enLines > 0.15) {
    problems.push(
      `${file}: ${zhLines} lines against ${enLines} — outside the 15% band, which usually means a section was compressed`,
    );
  }
}

console.log('');
if (missing.length) {
  notes.push(`no Chinese version yet: ${missing.join(', ')}`);
}

// --- 2b. bilingual artifacts hold both halves, English first -----------------
for (const file of bilingual) {
  const full = path.join(enDir, file);
  if (!fs.existsSync(full)) {
    problems.push(`${file}: listed as a bilingual artifact but the file is missing`);
    continue;
  }
  const text = read(full);
  const cjk = (text.match(/[\u3400-\u4dbf\u4e00-\u9fff]/g) || []).length;
  const latin = (text.match(/[A-Za-z]/g) || []).length;
  const firstCjk = text.search(/[\u4e00-\u9fff]/);
  const firstHead = text.search(/^## /m);
  if (latin < 400) {
    problems.push(`${file}: only ${latin} Latin letters — the English half must stay complete and first`);
  }
  if (cjk < 200) {
    problems.push(`${file}: only ${cjk} Chinese characters — the Chinese half is missing or a stub`);
  }
  if (firstHead === -1 || firstCjk === -1 || firstCjk < firstHead) {
    problems.push(
      `${file}: the Chinese half must be appended after the English text — English is ` +
        `the default a reader sees first`,
    );
  }
  notes.push(`${file}: bilingual artifact (English first, Chinese appended)`);
}

// --- 3. cross-document terminology ------------------------------------------
for (const [label, patterns, preferred] of [
  ['"specification §N"', ['规格说明 §', '规格书 §', '规范 §'], '项目书 §'],
  ['the planned marker', ['_(planned)_', '_(计划中)_'], '_(planned)_'],
]) {
  const counts = new Map();
  for (const file of files) {
    const text = read(path.join(zhDir, file));
    for (const pattern of patterns) {
      const n = (text.match(new RegExp(pattern.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'g')) || []).length;
      if (n) counts.set(pattern, (counts.get(pattern) || 0) + n);
    }
  }
  const found = [...counts.entries()];
  if (found.length > 1) {
    problems.push(
      `${label} is rendered ${found.length} different ways: ` +
        found.map(([p, n]) => `"${p}" x${n}`).join(', ') +
        ` — pick one (preferred: "${preferred}")`,
    );
  } else if (found.length === 1) {
    notes.push(`${label}: uniformly "${found[0][0]}" (x${found[0][1]})`);
  }
}

// --- 4. links resolve --------------------------------------------------------
for (const file of files) {
  const text = read(path.join(zhDir, file));
  for (const [, target] of text.matchAll(/\]\(([^)]+)\)/g)) {
    if (target.startsWith('http') || target.startsWith('#')) continue;
    const resolved = path.resolve(zhDir, target.split('#')[0]);
    if (!fs.existsSync(resolved)) {
      problems.push(`${file}: link ${target} does not resolve`);
    }
  }
}

// --- 5. spacing consistency --------------------------------------------------
const tightFiles = files.filter((f) => {
  const s = spacing(read(path.join(zhDir, f)));
  return s.tight > s.spaced;
});
if (tightFiles.length && tightFiles.length !== files.length) {
  problems.push(
    `spacing convention differs between files: ${tightFiles.length} of ${files.length} ` +
      `write Chinese directly against Latin text (${tightFiles.join(', ')}) while the rest use a space`,
  );
}

// --- report ------------------------------------------------------------------
console.log(`Chinese docs: ${files.length} files, ${totalLines} lines (English: ${totalEnLines})`);
console.log('');
for (const note of notes) console.log(`  note   ${note}`);
if (!problems.length) {
  console.log('  result PASS — the set is internally consistent');
  process.exit(0);
}
for (const problem of problems) console.log(`  FAIL   ${problem}`);
console.log(`  result FAIL — ${problems.length} problem(s)`);
process.exit(1);
