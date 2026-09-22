// 快速校验生成的站点：内部链接有效性、锚点对应、markdown 残留
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';
const OUT = '.site';
let bad = 0;
const check = (cond, msg) => { if (!cond) { console.log('BAD: ' + msg); bad++; } };

// 目标文件存在则返回其内容，否则返回 null。目录（如 "zh/"）不算命中。
function readTarget(path) {
  try {
    return statSync(path).isFile() ? readFileSync(path, 'utf8') : null;
  } catch { return null; }
}

for (const lang of ['en', 'zh']) {
  for (const f of readdirSync(join(OUT, lang))) {
    const p = join(OUT, lang, f);
    const html = readFileSync(p, 'utf8');

    // 1. 内部链接存在；2. 锚点（含跨页锚点）在目标文档里真的有对应 id。
    //    锚点可能含非 ASCII 字符——中文标题的 slug 就是中文——所以这里不能
    //    用 \w 去匹配，否则 "2. DNS 记录" 的锚点会被截断而漏检。
    for (const m of html.matchAll(/href="([^"#]*)(#[^"]*)?"/g)) {
      const href = m[1].split("?")[0];   // 资源指纹挂在查询串上，文件路径只到 ? 之前
      const anchor = (m[2] || '').slice(1);
      if (/^(https?:|mailto:|data:)/.test(href)) continue;
      const targetHtml = href === '' ? html : readTarget(join(OUT, lang, href));
      if (targetHtml === null) { check(false, `${lang}/${f}: broken link ${href}`); continue; }
      if (anchor) {
        let id;
        try { id = decodeURIComponent(anchor); } catch { id = anchor; }
        check(targetHtml.includes(`id="${id}"`), `${lang}/${f}: anchor #${anchor} has no target in ${href || f}`);
      }
    }

    // 3. markdown 残留（排除 pre/code 内的示例文本）
    const bare = html.replace(/<(pre|code)[\s\S]*?<\/\1>/g, '');
    check(!/\*\*[^*]+\*\*/.test(bare), `${lang}/${f}: bold residue`);
    check(!/\]\([^)]+\.md\)/.test(bare), `${lang}/${f}: unrewritten .md link`);

    // 4. 基本结构
    check(html.includes('</html>'), `${lang}/${f}: truncated`);

    // 5. 有搜索框就必须加载索引。少一行 <script> 不会报任何错——输入什么都
    //    只得到"没有匹配结果"，看起来像没收录，而不是像坏了。
    if (html.includes('id="searchInput"')) {
      check(html.includes('assets/search.js'), `${lang}/${f}: has a search box but does not load assets/search.js`);
    }
  }
}

// 首页两份：`/` 是英文，`/zh/` 是中文。zh/index.html 已在上面按 zh/ 目录检查过链接，
// 这里只管根目录那一份，以及两份各自的首页断言。
const idx = readFileSync(join(OUT, 'index.html'), 'utf8');
const zhIdx = readFileSync(join(OUT, 'zh', 'index.html'), 'utf8');
check(idx.includes('home-hero'), 'index.html: hero missing');
check(zhIdx.includes('home-hero'), 'zh/index.html: hero missing');
check(idx.includes('lang="en"'), 'index.html: is not the English page');
check(zhIdx.includes('lang="zh-CN"'), 'zh/index.html: is not the Chinese page');
check(idx.includes('Glossary'), 'index.html: English doc card missing');
check(zhIdx.includes('术语表'), 'zh/index.html: Chinese doc card missing');
// 语言切换必须落在另一份首页上，而不是某个文档页。
check(/href="zh\/index\.html"[^>]*hreflang="zh-CN"/.test(idx), 'index.html: 中文 switch does not point at zh/index.html');
check(/href="\.\.\/index\.html"[^>]*hreflang="en"/.test(zhIdx), 'zh/index.html: EN switch does not point at /index.html');
check(idx.includes('assets/search.js'), 'index.html: has a search box but does not load assets/search.js');
check(zhIdx.includes('assets/search.js'), 'zh/index.html: has a search box but does not load assets/search.js');
for (const m of idx.matchAll(/href="([^"#]*)(#[^"]*)?"/g)) {
  const href = m[1].split('?')[0], anchor = (m[2] || '').slice(1);
  if (/^(https?:|mailto:|data:)/.test(href)) continue;
  const targetHtml = href === '' ? idx : readTarget(join(OUT, href));
  if (targetHtml === null) { check(false, `index.html: broken link ${href}`); continue; }
  if (anchor) {
    let id;
    try { id = decodeURIComponent(anchor); } catch { id = anchor; }
    check(targetHtml.includes(`id="${id}"`), `index.html: anchor #${anchor} has no target in ${href || 'index.html'}`);
  }
}

// 4. 双语覆盖：任一语言里存在的页面，另一种语言也必须存在。
//    这是这套站点最容易悄悄缺的一处——新增一篇文档却只写了一边时，链接检查照样全绿（页面集内部
//    自洽），而读者切换语言会撞上 404，或者发现某页只属于另一种语言。规则以"页面的并集"为准，
//    因此单语页面无论落在哪一边都会被抓出来；顺带检查每页的语言切换链接确实指向同名的另一语言页，
//    避免出现"切换过去却不是同一页"的情况。
const pages = new Map();
const note = (lang, file) => {
  if (!pages.has(file)) pages.set(file, new Set());
  pages.get(file).add(lang);
};
for (const lang of ['en', 'zh']) {
  for (const f of readdirSync(join(OUT, lang))) if (f.endsWith('.html')) note(lang, f);
}
// 根目录的 index.html 就是英文首页——`/` 即 en 的 index——所以它算在 en 这一边，
// 而不是一个只存在于 zh/ 里的孤儿页面。
note('en', 'index.html');

for (const [file, langs] of [...pages].sort()) {
  for (const lang of ['en', 'zh']) {
    if (langs.has(lang)) continue;
    check(false, `${file}: no ${lang} page — a published page has to exist in both languages`);
  }
  if (langs.size !== 2) continue;
  for (const lang of langs) {
    // 英文首页在站点根，其余页面都在 <lang>/ 下。
    const at = file === 'index.html' && lang === 'en' ? join(OUT, file) : join(OUT, lang, file);
    const html = readFileSync(at, 'utf8');
    // 两份首页互指 `/` 与 `/zh/`；文档页互指 ../<另一种语言>/<同名文件>。
    const other = lang === 'en' ? 'zh' : 'en';
    const want = file === 'index.html'
      ? (lang === 'en' ? '"zh/index.html"' : '"../index.html"')
      : `"../${other}/${file}"`;
    check(html.includes(want), `${lang}/${file}: does not link to its ${other} counterpart (${want})`);
  }
}

console.log(bad === 0 ? 'ALL OK' : `${bad} problem(s)`);
process.exit(bad === 0 ? 0 : 1);
