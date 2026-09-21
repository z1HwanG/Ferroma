// 示意图对齐检查：同一代码块内，"框线字符个数相同"的行，最后一个框线字符必须落在同一显示列。
//
// 为什么是这个规则：并排框与嵌套框的框线个数天然不同，因此被排除在外；而"同一个框的右边框
// 漂了一格"必然是同个数、不同末列。早先试过两种更粗的规则，都在真实文件上误报——一种把连接线
// 当成边框（把孤立的 │ 从第 36 列"修"到第 78 列），另一种只看相邻行（并排框之间必然不同列）。
//
// 宽度按显示列算：CJK 与 emoji 占两列，其余占一列，制表符按四列。这与 GitHub 等宽渲染一致；
// 用字符下标去量含中文的行，会把完全对齐的图判成坏的。
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = dirname(dirname(fileURLToPath(import.meta.url)));
const BOX = new Set('│┃┌┐└┘├┤┬┴┼─═╔╗╚╝║┏┓┗┛┣┫┳┻╋╭╮╯╰');

/** 双宽：CJK、全角形式、以及常见 emoji 区块。 */
const WIDE =
  /[\u1100-\u115f\u2e80-\u303e\u3041-\u33ff\u3400-\u4dbf\u4e00-\u9fff\ua000-\ua4cf\uac00-\ud7a3\uf900-\ufaff\ufe30-\ufe6f\uff00-\uff60\uffe0-\uffe6\u2600-\u27bf\u2b00-\u2bff\u{1f000}-\u{1faff}]/u;

function displayColumns(line) {
  const out = [];
  let x = 0;
  for (const ch of line) {
    if (BOX.has(ch)) out.push(x);
    x += ch === '\t' ? 4 : WIDE.test(ch) ? 2 : 1;
  }
  return out;
}

/** 每个文档里含框线的代码块，按"框线个数 + 左沿"分组，组内末列必须唯一。 */
function problemsIn(path) {
  const lines = readFileSync(path, 'utf8').split('\n');
  const problems = [];
  let fence = false;
  let block = [];
  let start = 0;

  const inspect = () => {
    const groups = new Map();
    for (const i of block) {
      const line = lines[i];
      const trimmed = line.trimEnd();
      if (trimmed === '' || !BOX.has(trimmed[trimmed.length - 1])) continue;
      const cols = displayColumns(line);
      if (cols.length < 2) continue;
      const key = `${cols.length}:${cols[0]}`;
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push({ line: i + 1, last: cols[cols.length - 1] });
    }
    for (const [, rows] of groups) {
      if (rows.length < 3) continue;
      const tally = new Map();
      for (const r of rows) tally.set(r.last, (tally.get(r.last) ?? 0) + 1);
      const majority = [...tally.entries()].sort((a, b) => b[1] - a[1])[0];
      for (const r of rows) {
        if (r.last === majority[0]) continue;
        if (tally.get(r.last) > 1) continue; // 两个都偏离，无法判断谁是基准
        problems.push(
          `${path}:${r.line} 框线末列=${r.last}，同组其余 ${majority[1]} 行都在第 ${majority[0]} 列`,
        );
      }
    }
  };

  lines.forEach((line, i) => {
    if (line.startsWith('```')) {
      if (fence && block.length) inspect();
      fence = !fence;
      block = [];
      start = i;
      return;
    }
    if (fence) block.push(i);
  });
  if (fence && block.length) inspect();
  return problems;
}

const DOCS = join(ROOT, 'docs');
const targets = [
  join(ROOT, 'README.md'),
  join(ROOT, 'README_zh.md'),
  ...readdirSync(DOCS)
    .filter((f) => f.endsWith('.md'))
    .map((f) => join(DOCS, f)),
  ...readdirSync(join(DOCS, 'zh'))
    .filter((f) => f.endsWith('.md'))
    .map((f) => join(DOCS, 'zh', f)),
].filter((p) => statSync(p).isFile());

const problems = targets.flatMap(problemsIn);
for (const p of problems) console.log(`  ${p}`);
console.log(
  problems.length === 0
    ? `  result PASS — ${targets.length} documents, every figure's borders agree`
    : `  result FAIL — ${problems.length} misaligned row(s)`,
);
process.exit(problems.length === 0 ? 0 : 1);
