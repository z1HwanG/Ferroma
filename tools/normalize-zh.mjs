#!/usr/bin/env node
/**
 * Normalise the ten Chinese documents into one consistent set.
 *
 * Three things went wrong by construction: ten translators each made a defensible
 * local choice. None of them is a translation error and all of them are visible to a
 * reader, which is exactly the class of problem a parallel translation produces and a
 * mechanical pass fixes.
 *
 *   1. Relative links out of `docs/` were copied verbatim. `../AGENTS.md` is correct
 *      from `docs/` and wrong from `docs/zh/` — it resolves to `docs/AGENTS.md`, which
 *      does not exist. Only links whose target actually lives at the repository root
 *      are rewritten, so a genuine `../` link is left alone.
 *   2. "specification §N" became 规范 §N in some files and 规格说明 §N in others. The
 *      Chinese documents settled on 项目书 §N, which is the form this script enforces.
 *   3. The `_(planned)_` status marker was translated in some files and kept in
 *      others. It is prose about documentation status, not a contract literal, so the
 *      Chinese documents should say 计划中.
 *
 *   node tools/normalize-zh.mjs [--check]
 */
import fs from 'node:fs';
import path from 'node:path';

const root = process.cwd();
const zhDir = path.join(root, 'docs', 'zh');
const checkOnly = process.argv.includes('--check');

const files = fs
  .readdirSync(zhDir)
  .filter((f) => f.endsWith('.md') && f !== 'GLOSSARY.md');

const report = [];

for (const file of files) {
  const full = path.join(zhDir, file);
  const before = fs.readFileSync(full, 'utf8');
  let text = before;
  const changes = [];

  // --- 1. links that must climb one directory further ------------------------
  text = text.replace(/\]\(\.\.\/([^)#]+)(#[^)]*)?\)/g, (match, target, anchor = '') => {
    // Already correct from docs/zh/?
    if (fs.existsSync(path.resolve(zhDir, '..', target))) return match;
    // Does it exist at the repository root instead?
    if (fs.existsSync(path.resolve(root, target))) {
      changes.push(`link ../${target} -> ../../${target}`);
      return `](../../${target}${anchor})`;
    }
    return match;
  });

  // --- 2. the specification's name -------------------------------------------
  const specCount = (text.match(/规范 §/g) || []).length;
  const specCount2 = (text.match(/规格说明 §/g) || []).length;
  if (specCount || specCount2) {
    text = text.replace(/规范 §/g, '项目书 §').replace(/规格说明 §/g, '项目书 §');
    changes.push(`specification §N -> 项目书 §N (x${specCount + specCount2})`);
  }

  // --- 3. the planned marker --------------------------------------------------
  const planned = (text.match(/\(planned\)_/g) || []).length;
  if (planned) {
    text = text.replace(/_\(planned\)_/g, '_(计划中)_');
    changes.push(`_(planned)_ -> _(计划中)_ (x${planned})`);
  }

  if (text !== before) {
    if (!checkOnly) fs.writeFileSync(full, text);
    report.push({ file, changes });
  }
}

for (const { file, changes } of report) {
  console.log(`${file}`);
  for (const change of changes) console.log(`    ${change}`);
}

if (!report.length) {
  console.log('nothing to normalise — the set is already consistent');
} else {
  console.log(
    `\n${report.length} file(s) ${checkOnly ? 'would change' : 'changed'}` +
      (checkOnly ? ' (dry run)' : ''),
  );
}
