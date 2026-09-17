// 快速校验生成的站点：内部链接有效性、锚点对应、markdown 残留
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
const OUT = '.site';
let bad = 0;
const check = (cond, msg) => { if (!cond) { console.log('BAD: ' + msg); bad++; } };

for (const lang of ['en', 'zh']) {
  for (const f of readdirSync(join(OUT, lang))) {
    const p = join(OUT, lang, f);
    const html = readFileSync(p, 'utf8');
    // 1. 内部链接存在
    for (const m of html.matchAll(/href="([^"#]+?)(#[\w-]+)?"/g)) {
      const href = m[1];
      if (/^(https?:|mailto:|data:)/.test(href)) continue;
      if (href === '') continue;
      const target = join(OUT, lang, href);
      try { readFileSync(target); } catch { check(false, `${lang}/${f}: broken link ${href}`); }
    }
    // 2. 锚点链接都有对应 id
    const ids = new Set([...html.matchAll(/id="([\w-]+)"/g)].map(m => m[1]));
    for (const m of html.matchAll(/href="#([\w-]+)"/g)) {
      check(ids.has(m[1]), `${lang}/${f}: anchor #${m[1]} has no target`);
    }
    // 3. markdown 残留（排除 pre/code 内的示例文本）
    const bare = html.replace(/<(pre|code)[\s\S]*?<\/\1>/g, '');
    check(!/\*\*[^*]+\*\*/.test(bare), `${lang}/${f}: bold residue`);
    check(!/\]\([^)]+\.md\)/.test(bare), `${lang}/${f}: unrewritten .md link`);
    // 4. TOC 锚点都存在（toc 链接已在 2 中覆盖）
    // 5. 基本结构
    check(html.includes('</html>'), `${lang}/${f}: truncated`);
  }
}
// 首页
const idx = readFileSync(join(OUT, 'index.html'), 'utf8');
check(idx.includes('hero'), 'index: hero missing');
check(idx.includes('术语表'), 'index: zh card missing');
console.log(bad === 0 ? 'ALL OK' : `${bad} problem(s)`);