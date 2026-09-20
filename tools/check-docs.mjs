#!/usr/bin/env node
/**
 * Check that the documents read as one set, and keep doing so after a push.
 *
 * Everything a reader follows is a reference: a relative link to a sibling document,
 * an anchor into a section, the language switch between a document and its mirror,
 * the index table that claims to list the documents. All of them are strings, none of
 * them is compiled, and a rename or a moved section breaks them silently — the
 * document still renders, the link just 404s.
 *
 *   node tools/check-docs.mjs
 *
 * What is checked:
 *   1. every relative link resolves, and every `#anchor` names a heading that exists;
 *   2. a Chinese document links to its Chinese sibling, never to the English file it
 *      mirrors, and never climbs out of `docs/zh/` for a file that also lives there;
 *   3. an English document links to the English file, not into `docs/zh/`;
 *   4. `docs/dockerhub.md` is a bilingual artifact with no relative links, because it
 *      is pasted into a page outside the repository (English first, Chinese appended);
 *   5. each root pair — README, CHANGELOG, TODO — exists in both languages and links
 *      to its counterpart;
 *   6. every document under `docs/` is referenced from somewhere, so a new one cannot
 *      be added and then forgotten.
 *
 * Markdown inside fenced blocks and inline code spans is skipped: the glossary quotes
 * links on purpose, including wrong ones, and those are examples, not references.
 */
import fs from 'node:fs';
import path from 'node:path';

const root = process.cwd();
const problems = [];
const notes = [];

const read = (p) => fs.readFileSync(p, 'utf8');

/** Document files: the root pairs plus docs/ and docs/zh/. */
function docFiles() {
  const rootMd = fs
    .readdirSync(root)
    .filter((f) => f.endsWith('.md'))
    .map((f) => f);
  const docs = fs.readdirSync(path.join(root, 'docs')).filter((f) => f.endsWith('.md'));
  const zhDir = path.join(root, 'docs', 'zh');
  const zh = fs.existsSync(zhDir)
    ? fs.readdirSync(zhDir).filter((f) => f.endsWith('.md')).map((f) => path.join('docs', 'zh', f))
    : [];
  return [...rootMd, ...docs.map((f) => path.join('docs', f)), ...zh].sort();
}

/** Prose only: fenced blocks and inline code spans removed. */
function prose(text) {
  return text.replace(/```[\s\S]*?```/g, '').replace(/`[^`\n]*`/g, '');
}

/** GitHub's heading anchor for a heading's text. */
function slug(heading) {
  return heading
    .toLowerCase()
    .replace(/[^\w\u3400-\u4dbf\u4e00-\u9fff -]/g, '')
    .trim()
    .replace(/ +/g, '-');
}

function anchorsOf(text) {
  const out = new Set();
  for (const line of text.split('\n')) {
    const m = line.match(/^#{1,6} +(.+?)\s*$/);
    if (m) out.add(slug(m[1]));
  }
  return out;
}

const files = docFiles();
// Chinese means the mirrored set: everything under docs/zh/, and the root files that
// are the Chinese half of a pair (README_zh.md, CHANGELOG_zh.md, TODO_zh.md).
const isZh = (f) => f.startsWith('docs/zh/') || /_zh\.md$/.test(path.basename(f));
const isRootPair = (f) => /^(README|CHANGELOG|TODO)(_zh)?\.md$/.test(f);
// The two glossaries are complementary rather than translated, so they link to each
// other on purpose — the one rule about linking through a mirror does not apply.
const isGlossary = (f) => path.basename(f) === 'GLOSSARY.md';

let linkCount = 0;
const referenced = new Set();

for (const file of files) {
  const full = path.join(root, file);
  const text = read(full);
  const body = prose(text);
  const dir = path.dirname(full);

  if (!/^# /m.test(text)) {
    problems.push(`${file}: no level-1 heading — a document needs a title`);
  }

  for (const m of body.matchAll(/\[[^\]]*\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g)) {
    const href = m[1];
    linkCount++;
    if (/^(https?:|mailto:|data:)/.test(href)) continue;
    if (href.startsWith('#')) {
      const a = href.slice(1);
      if (a && !anchorsOf(text).has(a)) {
        problems.push(`${file}: anchor ${href} has no target in this file`);
      }
      continue;
    }

    const [target, anchor] = href.split('#');
    if (!target) continue;
    const resolved = path.resolve(dir, target);
    const rel = path.relative(root, resolved);

    if (!fs.existsSync(resolved)) {
      problems.push(`${file}: link ${href} does not resolve`);
      continue;
    }
    referenced.add(rel);

    // A file that has a Chinese mirror must be linked through the mirror from the
    // Chinese side, and never from the English side into `docs/zh/`.
    const base = path.basename(target);
    const zhTwin = path.join(root, 'docs', 'zh', base);
    const zhTwinRel = path.relative(root, zhTwin);
    if (
      isZh(file) &&
      !isGlossary(file) &&
      /\.md$/.test(target) &&
      fs.existsSync(zhTwin) &&
      rel !== zhTwinRel
    ) {
      problems.push(
        `${file}: link ${href} points at the English file — the Chinese documents link ` +
          `to their Chinese siblings (see docs/zh/GLOSSARY.md §四)`,
      );
    }
    if (!isZh(file) && !isGlossary(file) && /\.md$/.test(target) && rel.startsWith('docs/zh/')) {
      problems.push(`${file}: link ${href} points into docs/zh/ — an English document links to the English file`);
    }

    if (anchor && resolved.endsWith('.md') && !anchorsOf(read(resolved)).has(anchor)) {
      problems.push(`${file}: link ${href} — ${rel} has no heading with that anchor`);
    }
  }
}

// --- 4. the bilingual artifact that leaves the repository -------------------
const hub = 'docs/dockerhub.md';
if (!fs.existsSync(path.join(root, hub))) {
  problems.push(`${hub}: missing — it is the text of the published Docker Hub page`);
} else {
  const text = read(hub);
  const body = prose(text);
  const relative = [...body.matchAll(/\[[^\]]*\]\(([^)\s#]+)\)/g)]
    .map((m) => m[1])
    .filter((h) => !/^(https?:|mailto:)/.test(h));
  if (relative.length) {
    problems.push(
      `${hub}: ${relative.length} relative link(s) (${relative.join(', ')}) — this file is ` +
        `pasted outside the repository, where only absolute URLs resolve`,
    );
  }
}

// --- 5. every root pair exists and cross-links ------------------------------
for (const [en, zh] of [
  ['README.md', 'README_zh.md'],
  ['CHANGELOG.md', 'CHANGELOG_zh.md'],
  ['TODO.md', 'TODO_zh.md'],
]) {
  const hasEn = fs.existsSync(path.join(root, en));
  const hasZh = fs.existsSync(path.join(root, zh));
  if (hasEn !== hasZh) {
    problems.push(`${en} / ${zh}: one half exists without the other`);
    continue;
  }
  if (!hasEn) continue;
  const enText = prose(read(path.join(root, en)));
  const zhText = prose(read(path.join(root, zh)));
  if (!enText.includes(`](${zh})`)) {
    problems.push(`${en}: does not link to its Chinese counterpart ${zh}`);
  }
  if (!zhText.includes(`](${en})`)) {
    problems.push(`${zh}: does not link to its English counterpart ${en}`);
  }
}

// --- 6. every document is referenced from somewhere -------------------------
for (const file of files) {
  if (isRootPair(file)) continue;
  if (!file.startsWith('docs/')) continue;
  if (referenced.has(file)) continue;
  const self = file;
  problems.push(
    `${self}: no document or README links to it — a document nothing points at is a ` +
      `document nobody finds`,
  );
}

// --- report -----------------------------------------------------------------
console.log('documentation check');
console.log(`  documents: ${files.length}   links checked: ${linkCount}`);
for (const note of notes) console.log(`  note   ${note}`);
if (problems.length === 0) {
  console.log('  result PASS — every reference resolves');
  process.exit(0);
}
for (const problem of problems) console.log(`  FAIL   ${problem}`);
console.log(`  result FAIL — ${problems.length} problem(s)`);
process.exit(1);
