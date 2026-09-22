#!/usr/bin/env node
// build-site.mjs — 把 docs/ 下的中英文档构建为静态网站，输出到 .site/
// 零依赖，Node >= 18。运行：node tools/build-site.mjs
import { readFileSync, writeFileSync, mkdirSync, rmSync, readdirSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';

const ROOT = dirname(dirname(fileURLToPath(import.meta.url)));
const DOCS = join(ROOT, 'docs');
const OUT = join(ROOT, '.site');

// The release the site is describing. Read from the manifest so a version bump
// never leaves a stale number on the home page.
const VERSION = (readFileSync(join(ROOT, 'Cargo.toml'), 'utf8').match(/^version\s*=\s*"([^"]+)"/m) || [null, 'dev'])[1];
const REPO_URL = 'https://github.com/z1HwanG/Ferroma';
const IMAGE = 'wesukilaye/ferroma';

// ---------------------------------------------------------------------------
// 页面元数据
// ---------------------------------------------------------------------------
const PAGES = [
  { slug: 'architecture', group: 'overview',
    en: { title: 'Architecture', desc: 'The system, the crate graph, and why it is shaped this way.' },
    zh: { title: '架构总览', desc: '系统组成、crate 依赖图，以及它为何如此设计。' } },
  { slug: 'smtp', group: 'protocols',
    en: { title: 'SMTP', desc: 'Inbound and outbound SMTP, reply codes, the open-relay policy.' },
    zh: { title: 'SMTP', desc: '入站与出站 SMTP、应答码、开放中继策略。' } },
  { slug: 'imap', group: 'protocols',
    en: { title: 'IMAP', desc: 'IMAP4rev1, folders, UIDs, flags, client compatibility.' },
    zh: { title: 'IMAP', desc: 'IMAP4rev1、文件夹、UID、标志与客户端兼容性。' } },
  { slug: 'api', group: 'protocols',
    en: { title: 'HTTP API', desc: 'Every HTTP endpoint, with examples.' },
    zh: { title: 'HTTP API', desc: '全部 HTTP 端点及示例。' } },
  { slug: 'fcp', group: 'protocols',
    en: { title: 'FCP', desc: 'The Ferroma Client Protocol: sync cursor, realtime, devices.' },
    zh: { title: 'FCP', desc: 'Ferroma 客户端协议：同步游标、实时事件、设备。' } },
  { slug: 'sync', group: 'protocols',
    en: { title: 'Sync Model', desc: 'The synchronisation model in depth.' },
    zh: { title: '同步模型', desc: '深入解析同步机制。' } },
  { slug: 'storage', group: 'internals',
    en: { title: 'Storage', desc: 'The schema, the Maildir, quotas, attachments, integrity.' },
    zh: { title: '存储', desc: '数据库模式、Maildir、配额、附件与完整性。' } },
  { slug: 'security', group: 'operations',
    en: { title: 'Security', desc: 'The threat model and each control, plus known gaps.' },
    zh: { title: '安全', desc: '威胁模型、每项安全控制及已知缺口。' } },
  { slug: 'deploy', group: 'operations', builtin: true,
    en: { title: 'Deployment guide', desc: 'Pick a shape, then the exact commands: Compose or plain Docker.' },
    zh: { title: '部署指南', desc: '先选部署形态，再照着命令走：Compose 或纯 Docker。' } },
  { slug: 'deployment', group: 'operations',
    en: { title: 'Deployment reference', desc: 'DNS, TLS, backups, upgrades, troubleshooting.' },
    zh: { title: '部署参考', desc: 'DNS、TLS、备份、升级与故障排查。' } },
  { slug: 'glossary', group: 'reference',
    en: { title: 'Glossary', desc: 'The canonical terminology of the English documents.' },
    zh: { title: '术语表', desc: 'Ferroma 中英术语对照。' } },
  { slug: 'contributing', group: 'contributing',
    en: { title: 'Contributing', desc: 'The working agreement, the checks that must pass, DCO.' },
    zh: { title: '参与开发', desc: '工作约定、必须通过的检查、DCO 签署。' } },
];

const GROUPS = {
  overview:   { en: 'Overview',   zh: '总览' },
  protocols:  { en: 'Protocols',  zh: '协议' },
  internals:  { en: 'Internals',  zh: '内部实现' },
  operations: { en: 'Operations', zh: '运维' },
  reference:  { en: 'Reference',  zh: '参考' },
  contributing: { en: 'Project', zh: '项目' },
};

const PAGE_ORDER = PAGES.map(p => p.slug);
const LANGS = ['en', 'zh'];

// 两个文档不在 docs/ 下：术语表文件名全大写，贡献指南在仓库根目录，
// 而且中文版是 CONTRIBUTING_zh.md 而不是 docs/zh/ 里的配对。
function docPath(lang, slug) {
  if (slug === 'glossary') return join(DOCS, lang === 'en' ? 'GLOSSARY.md' : 'zh/GLOSSARY.md');
  if (slug === 'contributing') return join(ROOT, lang === 'en' ? 'CONTRIBUTING.md' : 'CONTRIBUTING_zh.md');
  return lang === 'en' ? join(DOCS, slug + '.md') : join(DOCS, 'zh', slug + '.md');
}

// 该文档在仓库中的目录，用于把文档里的相对链接还原成仓库路径。
function docBase(lang, slug) {
  if (slug === 'contributing') return '';
  if (slug === 'glossary') return lang === 'en' ? 'docs' : 'docs/zh';
  return lang === 'en' ? 'docs' : 'docs/zh';
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------
const escapeHtml = s => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

function slugify(text) {
  return text.toLowerCase().trim()
    .replace(/[\u0300-\u036f]/g, '')
    .replace(/[^\p{L}\p{N}\-_ ]/gu, '')
    .replace(/ /g, '-');
}

// ---------------------------------------------------------------------------
// 极简语法高亮
// ---------------------------------------------------------------------------
const KW = {
  rust: `as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while`,
  bash: `if then else elif fi for while until do done case esac function return export local readonly set shift trap exit source alias sudo echo cd printf read test eval exec`,
  sql: `SELECT FROM WHERE INSERT INTO VALUES UPDATE SET DELETE CREATE TABLE ALTER DROP INDEX PRIMARY KEY FOREIGN REFERENCES NOT NULL DEFAULT UNIQUE CHECK JOIN LEFT RIGHT INNER OUTER ON AS AND OR LIMIT OFFSET ORDER BY GROUP BY HAVING DISTINCT COUNT SUM COALESCE NOW BEGIN COMMIT ROLLBACK BOOLEAN SMALLINT INTEGER BIGINT TEXT VARCHAR TIMESTAMPTZ TIMESTAMP UUID JSONB SERIAL INT8 BOOL CASE WHEN THEN END`,
};
KW.rust = KW.rust.split(' ');
KW.bash = KW.bash.split(' ');
KW.sql = KW.sql.split(' ');

const LANGDEF = {
  rust: {
    re: String.raw`(?<com>//[^\n]*|/\*[\s\S]*?\*/) | (?<str>"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|b?"[^"]*"|r#*") | (?<kw>\b(?:${KW.rust.join('|')})\b) | (?<typ>\b[A-Z][A-Za-z0-9_]*\b) | (?<num>\b\d[\d_]*(?:\.\d+)?(?:[uif](?:8|16|32|64|size))?\b)`,
    flags: 'g', classes: { com: 'c-com', str: 'c-str', kw: 'c-kw', typ: 'c-typ', num: 'c-num' } },
  bash: {
    re: String.raw`(?<com>#[^\n]*) | (?<str>"(?:\\.|[^"\\])*"|'[^']*') | (?<var>\$\{[^}]*\}|\$[A-Za-z_][A-Za-z0-9_]*) | (?<kw>\b(?:${KW.bash.join('|')})\b) | (?<flag>(?:^|\s)--?[A-Za-z][\w-]*)`,
    flags: 'gm', classes: { com: 'c-com', str: 'c-str', var: 'c-var', kw: 'c-kw', flag: 'c-flag' } },
  json: {
    re: String.raw`(?<key>"(?:\\.|[^"\\])*(?=\s*:)) | (?<str>"(?:\\.|[^"\\])*") | (?<kw>\b(?:true|false|null)\b) | (?<num>-?\b\d+(?:\.\d+)?(?:[eE][+-]?\d+)?\b)`,
    flags: 'g', classes: { key: 'c-key', str: 'c-str', kw: 'c-kw', num: 'c-num' } },
  sql: {
    re: String.raw`(?<com>--[^\n]*) | (?<str>'(?:''|[^'])*') | (?<kw>\b(?:${KW.sql.join('|')})\b) | (?<num>\b\d+\b)`,
    flags: 'gi', classes: { com: 'c-com', str: 'c-str', kw: 'c-kw', num: 'c-num' } },
  yaml: {
    re: String.raw`(?<com>#[^\n]*) | (?<key>^[ \t]*-?[ \t]*[\w.\-/]+(?=\s*:)) | (?<str>"(?:\\.|[^"\\])*"|'[^']*') | (?<kw>\b(?:true|false|null|yes|no|on|off)\b) | (?<num>-?\b\d+(?:\.\d+)?\b)`,
    flags: 'gm', classes: { com: 'c-com', key: 'c-key', str: 'c-str', kw: 'c-kw', num: 'c-num' } },
  toml: {
    re: String.raw`(?<sec>^\[[^\]]+\]) | (?<com>#[^\n]*) | (?<key>^[ \t]*[\w.\-"]+(?=\s*=)) | (?<str>"(?:\\.|[^"\\])*"|'[^']*') | (?<kw>\b(?:true|false)\b) | (?<num>-?\b\d+(?:\.\d+)?\b)`,
    flags: 'gm', classes: { sec: 'c-typ', com: 'c-com', key: 'c-key', str: 'c-str', kw: 'c-kw', num: 'c-num' } },
  http: {
    re: String.raw`(?<kw>^(?:GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS)\b) | (?<stat>^HTTP/[\d.]+\s+\d+\b) | (?<key>^[A-Za-z][\w-]*(?=\s*:)) | (?<str>"(?:\\.|[^"\\])*") | (?<num>\b\d{3}\b)`,
    flags: 'gm', classes: { kw: 'c-kw', stat: 'c-typ', key: 'c-key', str: 'c-str', num: 'c-num' } },
};
const LANG_ALIAS = { bind: null, nginx: null, markdown: null, text: null, txt: null, sh: 'bash', shell: 'bash', yml: 'yaml', js: 'rust' };

// 框线字符（U+2500 系列）是"歧义宽度"字符：同一个字符，拉丁等宽字体里占 1 列，CJK
// 等宽字体里占 2 列。这些图全是按 1 列排的，于是读者机器上回退到 CJK 字体时整张图错位——
// 实测本机 `─` 宽 9.36px、`|` 宽 4.61px，而且中英文 lang 下一样（决定它的是字体，不是
// lang）。所以渲染时换成 ASCII 等价物：对齐由字符本身保证，不再取决于读者装了什么字体。
const BOX_TO_ASCII = {
  '─': '-', '━': '-', '│': '|', '┃': '|', '═': '=', '║': '|',
  '┌': '+', '┐': '+', '└': '+', '┘': '+', '├': '+', '┤': '+',
  '┬': '+', '┴': '+', '┼': '+', '╭': '+', '╮': '+', '╰': '+', '╯': '+',
  '╔': '+', '╗': '+', '╚': '+', '╝': '+', '╠': '+', '╣': '+',
  '╦': '+', '╩': '+', '╬': '+',
  '▼': 'v', '▲': '^', '◄': '<', '►': '>', '·': '.',
};
const BOX_RE = new RegExp('[' + Object.keys(BOX_TO_ASCII).join('') + ']', 'g');
const asciiSafe = (code) => code.replace(BOX_RE, ch => BOX_TO_ASCII[ch]);

function highlight(code, lang) {
  const def = LANG_ALIAS[lang] !== undefined ? LANG_ALIAS[lang] : lang;
  if (!def || !LANGDEF[def]) return escapeHtml(code);
  const d = LANGDEF[def];
  const re = new RegExp(d.re, d.flags);
  let out = '', last = 0, m;
  while ((m = re.exec(code))) {
    out += escapeHtml(code.slice(last, m.index));
    const cls = Object.keys(d.classes).find(k => m.groups[k] !== undefined);
    if (cls && m[0] !== undefined) {
      let frag = m[0];
      // flag 规则会把前导空白也吞进来，拆开处理
      if (cls === 'flag') {
        const lead = frag.match(/^\s*/)[0];
        out += escapeHtml(lead) + `<span class="c-flag">${escapeHtml(frag.slice(lead.length))}</span>`;
      } else {
        out += `<span class="${d.classes[cls]}">${escapeHtml(frag)}</span>`;
      }
    } else {
      out += escapeHtml(m[0]);
    }
    last = m.index + m[0].length;
    if (m[0].length === 0) re.lastIndex++;
  }
  return out + escapeHtml(code.slice(last));
}

// ---------------------------------------------------------------------------
// 行内 Markdown
// ---------------------------------------------------------------------------
function inline(raw) {
  const codes = [];
  const links = [];
  let t = raw;
  // code spans（先保护）
  t = t.replace(/(`+)([\s\S]*?)\1/g, (m, ticks, body) => {
    codes.push(body.trim());
    return `\x00C${codes.length - 1}\x00`;
  });
  // 自动链接 <https://…>
  t = t.replace(/<((?:https?|mailto:)[^>\s]+)>/g, (m, u) => {
    links.push({ href: u, text: u });
    return `\x00L${links.length - 1}\x00`;
  });
  // md 链接
  t = t.replace(/\[([^\]]+)\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g, (m, text, href) => {
    links.push({ href, text });
    return `\x00L${links.length - 1}\x00`;
  });
  t = escapeHtml(t);
  // 粗体 / 斜体
  t = t.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  t = t.replace(/(^|[\s(>])\*([^*\s][^*]*)\*/g, '$1<em>$2</em>');
  t = t.replace(/(^|[\s(])_([^_\s][^_]*)_(?=$|[\s).,;:!?<])/g, '$1<em>$2</em>');
  // 恢复链接与 code
  t = t.replace(/\x00L(\d+)\x00/g, (m, i) => renderLink(links[i]));
  t = t.replace(/\x00C(\d+)\x00/g, (m, i) => `<code>${escapeHtml(codes[i])}</code>`);
  return t;
}

function renderLink({ href, text }) {
  let label = inlineLight(text);
  if (/^https?:/i.test(href) || /^mailto:/i.test(href)) {
    return `<a href="${escapeHtml(href)}" target="_blank" rel="noopener">${label}</a>`;
  }
  const rewritten = rewriteHref(href);
  if (!rewritten) return label;
  return `<a href="${escapeHtml(rewritten)}">${label}</a>`;
}

// 简化版行内解析，用于链接文字（避免嵌套链接）
function inlineLight(text) {
  return escapeHtml(text).replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>').replace(/\*([^*]+)\*/g, '<em>$1</em>').replace(/`([^`]+)`/g, '<code>$1</code>');
}

// 正在渲染的文档在仓库中的目录（仓库相对），renderDoc 在每篇之前设置。
let CURRENT_BASE = '';

/** 把文档里的相对链接还原成仓库根相对路径；越界返回 null。 */
function repoPath(pathPart) {
  const out = [];
  for (const seg of (CURRENT_BASE + '/' + pathPart).split('/')) {
    if (seg === '' || seg === '.') continue;
    if (seg === '..') { if (!out.length) return null; out.pop(); continue; }
    out.push(seg);
  }
  return out.join('/');
}

function rewriteHref(href) {
  const hashAt = href.indexOf('#');
  const pathPart = hashAt < 0 ? href : href.slice(0, hashAt);
  const hash = hashAt < 0 ? '' : href.slice(hashAt);
  if (!pathPart) return null;

  // 站内文档互链：security.md、../security.md、docs/zh/security.md、GLOSSARY.md
  // 指的是同一篇，一律落到当前语言的页面。
  const m = pathPart.match(/^(?:\.\.?\/)*(?:docs\/)?(?:zh\/)?([\w-]+)\.md$/i);
  if (m && PAGE_ORDER.includes(m[1].toLowerCase())) return m[1].toLowerCase() + '.html' + hash;

  // 站点不渲染的文件——AGENTS.md、LICENSE、TODO.md、config/ferroma.toml——指向
  // GitHub，但只在它真的存在于这个仓库时；否则仍旧降级为纯文本。
  const rel = repoPath(pathPart);
  if (rel && existsSync(join(ROOT, rel))) return `${REPO_URL}/blob/main/${rel}${hash}`;
  return null;
}

// ---------------------------------------------------------------------------
// 块级 Markdown
// ---------------------------------------------------------------------------
function renderBlocks(src, state) {
  const lines = src.split('\n');
  const out = [];
  let i = 0;
  const isTableSep = l => /^\s*\|?\s*:?-{2,}:?\s*(\|\s*:?-{2,}:?\s*)*\|?\s*$/.test(l) && l.includes('-') && (l.includes('|') || false);
  const listRe = /^(\s*)([-*+]|\d+[.)])\s+(.*)$/;

  while (i < lines.length) {
    const line = lines[i];

    if (/^\s*$/.test(line)) { i++; continue; }

    // 围栏代码块
    const fence = line.match(/^\s*(```+|~~~+)\s*([\w+-]*)/);
    if (fence) {
      const marker = fence[1][0].repeat(3);
      const lang = fence[2].toLowerCase();
      const buf = [];
      i++;
      while (i < lines.length && !new RegExp(`^\\s*${marker[0] === '`' ? '`' + '{3,}' : '~' + '{3,}'}\\s*$`).test(lines[i])) { buf.push(lines[i]); i++; }
      i++;
      const code = buf.join('\n');
      const hi = highlight(asciiSafe(code), lang);
      // 代码块另存一份供搜索：环境变量名、命令、路径几乎只出现在这里，
      // 而搜不到它们正是这套索引最容易让人踩空的地方。
      if (state.segments.length) {
        const seg = state.segments[state.segments.length - 1];
        seg.code = ((seg.code || '') + ' ' + code).slice(0, 1200);
      }
      out.push(`<figure class="codeblock"${lang ? ` data-lang="${lang}"` : ''}><pre><code>${hi}</code></pre><button class="copy-btn" type="button" data-copy aria-label="copy code">⧉</button></figure>`);
      continue;
    }

    // 标题
    const head = line.match(/^(#{1,6})\s+(.*)$/);
    if (head) {
      const level = head[1].length;
      const html = inline(head[2]);
      const plain = html.replace(/<[^>]+>/g, '');
      let id = slugify(plain) || 'section';
      const seen = state.ids;
      if (seen.has(id)) { let n = 2; while (seen.has(`${id}-${n}`)) n++; id = `${id}-${n}`; }
      seen.add(id);
      if (level >= 2 && level <= 3) state.toc.push({ level, id, text: plain });
      if (level === 2) state.segments.push({ id, title: plain, text: '' });
      state.headings = state.headings || [];
      out.push(`<h${level} id="${id}">${html}<a class="anchor" href="#${id}" aria-hidden="true">#</a></h${level}>`);
      i++;
      continue;
    }

    // 水平线
    if (/^\s*(?:-{3,}|\*{3,}|_{3,})\s*$/.test(line)) { out.push('<hr>'); i++; continue; }

    // 表格
    if (line.trimStart().startsWith('|') && i + 1 < lines.length && isTableSep(lines[i + 1])) {
      const parseCells = l => {
        let s = l.trim().replace(/^\|/, '').replace(/\|$/, '');
        s = s.replace(/\\\|/g, '\x01');
        return s.split('|').map(c => c.replace(/\x01/g, '|').trim());
      };
      const header = parseCells(line);
      const sepcells = parseCells(lines[i + 1]);
      const align = sepcells.map(c => c.startsWith(':') && c.endsWith(':') ? 'center' : c.endsWith(':') ? 'right' : 'left');
      i += 2;
      const rows = [];
      while (i < lines.length && lines[i].trimStart().startsWith('|')) { rows.push(parseCells(lines[i])); i++; }
      let h = '<div class="tablewrap"><table><thead><tr>';
      header.forEach((c, ci) => h += `<th${align[ci] ? ` style="text-align:${align[ci]}"` : ''}>${inline(c)}</th>`);
      h += '</tr></thead><tbody>';
      for (const r of rows) {
        h += '<tr>';
        r.forEach((c, ci) => h += `<td${align[ci] ? ` style="text-align:${align[ci]}"` : ''}>${inline(c)}</td>`);
        h += '</tr>';
      }
      h += '</tbody></table></div>';
      out.push(h);
      continue;
    }

    // 引用块
    if (/^\s*>/.test(line)) {
      const buf = [];
      while (i < lines.length && (/^\s*>/.test(lines[i]) || (!/^\s*$/.test(lines[i]) && buf.length && /^\s*\S/.test(lines[i]) && !/^(#{1,6}\s|```|\||[-*+]\s|\d+[.)]\s)/.test(lines[i].trimStart()) && /^\s*>/.test(buf[buf.length - 1] ?? '')))) {
        if (/^\s*>/.test(lines[i])) buf.push(lines[i].replace(/^\s*>\s?/, ''));
        else buf[buf.length - 1] += ' ' + lines[i].trim();
        i++;
      }
      out.push(`<blockquote>${renderBlocks(buf.join('\n'), state)}</blockquote>`);
      continue;
    }

    // 列表
    if (listRe.test(line)) {
      const res = collectList(lines, i);
      out.push(renderList(res.node, state));
      i = res.next;
      continue;
    }

    // 段落
    const pbuf = [line];
    i++;
    while (i < lines.length && !/^\s*$/.test(lines[i]) && !/^(#{1,6}\s|```|~~~|\s*>|\s*(?:[-*+]|\d+[.)])\s|\s*(?:-{3,}|\*{3,}|_{3,})\s*$)/.test(lines[i]) && !(lines[i].trimStart().startsWith('|') && i + 1 < lines.length && isTableSep(lines[i + 1]))) {
      pbuf.push(lines[i]);
      i++;
    }
    const ptext = pbuf.map(l => l.trim()).join(' ');
    if (state.segments.length) state.segments[state.segments.length - 1].text += ' ' + ptext.replace(/<[^>]+>/g, '').replace(/[`*_]/g, '');
    else state.segments.push({ id: '', title: 'intro', text: ptext.replace(/<[^>]+>/g, '').replace(/[`*_]/g, '') });
    out.push(`<p>${inline(ptext)}</p>`);
  }
  return out.join('\n');
}

function collectList(lines, start) {
  const stack = [{ indent: -1, type: null, items: [], sublist: null }];
  const top = () => stack[stack.length - 1];
  let outer = null;
  let i = start;
  const listRe = /^(\s*)([-*+]|\d+[.)])\s+(.*)$/;
  while (i < lines.length) {
    const line = lines[i];
    if (/^\s*$/.test(line)) {
      // 空行：若后续仍是列表内容则继续
      const nxt = lines[i + 1];
      if (nxt !== undefined && (listRe.test(nxt) || /^\s{2,}\S/.test(nxt))) { i++; continue; }
      break;
    }
    const m = line.match(listRe);
    const indent = m ? m[1].replace(/\t/g, '    ').length : line.match(/^\s*/)[0].replace(/\t/g, '    ').length;
    if (m) {
      const type = /^\d/.test(m[2]) ? 'ol' : 'ul';
      while (stack.length > 1 && (indent < top().indent || (indent === top().indent && top().type && top().type !== type))) stack.pop();
      if (indent > top().indent || top().type === null) {
        const node = { indent, type, items: [], parent: top() };
        if (top().type !== null) top().items[top().items.length - 1].sublist = node;
        stack.push(node);
        if (!outer) outer = node;
      }
      top().items.push({ text: m[3], sublist: null });
      i++;
    } else if (stack.length > 1 && indent >= top().indent + 2 && top().items.length) {
      const it = top().items[top().items.length - 1];
      it.text += '\n' + line.trim();
      i++;
    } else break;
  }
  return { node: outer || stack[0], next: i };
}

function renderList(root, state) {
  const walk = node => {
    const tag = node.type || 'ul';
    let h = `<${tag}>`;
    for (const it of node.items) {
      const text = it.text.split('\n').map(l => inline(l)).join('<br>');
      if (state.segments.length) state.segments[state.segments.length - 1].text += ' ' + it.text.replace(/[`*_]/g, '');
      h += `<li>${text}${it.sublist ? walk(it.sublist) : ''}</li>`;
    }
    return h + `</${tag}>`;
  };
  return walk(root);
}

// ---------------------------------------------------------------------------
// 渲染单个文档
// ---------------------------------------------------------------------------
function renderDoc(lang, page) {
  // 站点内置页面（部署指南）没有对应的 docs/ 源文件
  if (page.builtin) return buildDeployDoc(lang);
  const file = docPath(lang, page.slug);
  if (!existsSync(file)) return null;
  const src = readFileSync(file, 'utf8');
  CURRENT_BASE = docBase(lang, page.slug);
  const state = { ids: new Set(), toc: [], segments: [{ id: '', title: page[lang].title, text: '' }] };
  const body = renderBlocks(src, state);
  // 首个 h1 作为文档题头已包含在 body 里；补充页头（面包屑式 kicker）
  const kicker = `${GROUPS[page.group][lang]} · ${lang === 'zh' ? '文档' : 'Documentation'}`;
  return {
    slug: page.slug, lang, title: page[lang].title, desc: page[lang].desc,
    kicker, body, toc: state.toc, segments: state.segments,
  };
}

// ---------------------------------------------------------------------------
// 模板
// ---------------------------------------------------------------------------
const favicon = 'data:image/svg+xml,' + encodeURIComponent(`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect x="3" y="6" width="26" height="20" rx="3" fill="none" stroke="#F2683C" stroke-width="2.4"/><path d="M4 8l12 9L28 8" fill="none" stroke="#F2683C" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/></svg>`);

function themeBootstrap() {
  return `<script>(function(){try{var t=localStorage.getItem('ferroma-theme');if(!t){t=matchMedia('(prefers-color-scheme: light)').matches?'light':'dark';}document.documentElement.dataset.theme=t;}catch(e){}})();</script>`;
}

// 侧栏按分组渲染；每条前面的 01/02… 是 PAGES 的全局序号，两语言一致。
function navGroupHtml(lang, activeSlug, root) {
  const flat = PAGES.filter(p => !(lang === 'en' && p.zhOnly));
  let h = '';
  for (const g of Object.keys(GROUPS)) {
    const pages = flat.filter(p => p.group === g);
    if (!pages.length) continue;
    h += `<div class="nav-group"><div class="nav-group-title">${GROUPS[g][lang]}</div>`;
    for (const p of pages) {
      const n = String(flat.indexOf(p) + 1).padStart(2, '0');
      const active = p.slug === activeSlug ? ' active' : '';
      h += `<a class="nav-link${active}" href="${root}${lang}/${p.slug}.html">` +
           `<span class="nav-num">${n}</span><span>${p[lang].title}</span></a>`;
    }
    h += '</div>';
  }
  return h;
}

function altLangUrl(lang, slug, root) {
  if (slug === 'home') return `${root}${lang === 'en' ? 'zh/index.html' : 'index.html'}`;
  const page = PAGES.find(p => p.slug === slug);
  // 仅中文存在的页面（如术语表）切英文时落到英文文档首篇
  if (lang === 'zh' && page && page.zhOnly) return `${root}en/architecture.html`;
  return `${root}${lang === 'en' ? 'zh' : 'en'}/${slug}.html`;
}

// 三个资源的 URL（带内容指纹）。主流程在写 HTML 之前填上真实值；
// 这里的默认值只是让单独调用模板函数时也有东西可用。
let ASSETS = { style: 'assets/style.css', site: 'assets/site.js', search: 'assets/search.js' };

const ICON = {
  menu: '<svg viewBox="0 0 20 20" aria-hidden="true"><path d="M3 6h14M3 10h14M3 14h14"/></svg>',
  sun: '<svg class="seg-ico i-sun" viewBox="0 0 20 20" aria-hidden="true"><circle cx="10" cy="10" r="4"/><path d="M10 1.5v2M10 16.5v2M1.5 10h2M16.5 10h2M4 4l1.4 1.4M14.6 14.6L16 16M16 4l-1.4 1.4M5.4 14.6L4 16"/></svg>',
  moon: '<svg class="seg-ico i-moon" viewBox="0 0 20 20" aria-hidden="true"><path d="M16 12.5A7 7 0 0 1 7.5 4a7 7 0 1 0 8.5 8.5z"/></svg>',
  arrow: '<svg class="btn-arrow" viewBox="0 0 20 20" aria-hidden="true"><path d="M4 10h11M10.5 5.5L15 10l-4.5 4.5"/></svg>',
};

// 顶栏在文档页与首页共用。`home` 为真时语言切换指向英文部署指南。
function topbarHtml({ lang, slug, root, ui, home }) {
  const zh = lang === 'zh';
  // 当前语言的首页，以及另一种语言的首页——两者都是真实存在的页面。
  const homeHref = `${root}${zh ? 'zh/index.html' : 'index.html'}`;
  const otherHref = home
    ? `${root}${zh ? 'index.html' : 'zh/index.html'}`
    : altLangUrl(lang, slug, root);
  return `<header class="topbar">
  <button class="iconbtn menu-btn" id="menuBtn" aria-label="${ui.menu}" aria-expanded="false">${ICON.menu}</button>
  <a class="brand" href="${homeHref}"><svg class="brand-mark" viewBox="0 0 32 32" aria-hidden="true"><rect x="3" y="6" width="26" height="20" rx="3"/><path d="M4 8l12 9L28 8"/></svg><span>Ferroma</span></a>
  <span class="topbar-tag">v${VERSION}</span>
  <div class="search" id="search">
    <svg class="search-icon" viewBox="0 0 20 20" aria-hidden="true"><circle cx="9" cy="9" r="6"/><path d="M13.5 13.5L18 18"/></svg>
    <input id="searchInput" type="search" placeholder="${ui.searchPlaceholder}" autocomplete="off" spellcheck="false" aria-label="${ui.search}">
    <kbd>/</kbd>
    <div class="search-results" id="searchResults" hidden></div>
  </div>
  <nav class="top-actions">
    <div class="seg seg-lang" role="group" aria-label="${ui.switchLang}">
      ${zh ? `<a href="${otherHref}" hreflang="en">EN</a>` : `<span class="on" aria-current="true">EN</span>`}
      ${zh ? `<span class="on" aria-current="true">中文</span>` : `<a href="${otherHref}" hreflang="zh-CN">中文</a>`}
    </div>
    <button class="seg seg-theme" id="themeBtn" type="button" aria-label="${ui.theme}" title="${ui.theme}"><span class="on">${ICON.sun}${ICON.moon}</span></button>
  </nav>
</header>`;
}

function sidebarHtml({ lang, slug, root, ui, home }) {
  return `<aside class="sidebar" id="sidebar">
    <nav class="sidebar-inner">
      <a class="nav-link nav-home${home ? ' active' : ''}" href="${root}${lang === 'zh' ? 'zh/index.html' : 'index.html'}">${ui.home}</a>
      ${navGroupHtml(lang, slug, root)}
      <div class="sidebar-foot">v${VERSION} · AGPL-3.0-only<br><a href="${REPO_URL}" target="_blank" rel="noopener">${ui.source}</a> · ${ui.builtFrom}</div>
    </nav>
  </aside>`;
}

function layout({ lang, slug, title, desc, kicker, content, toc, root }) {
  const ui = UI[lang];
  const flat = PAGES.filter(p => !(lang === 'en' && p.zhOnly));
  const idx = flat.findIndex(p => p.slug === slug);
  const prev = idx > 0 ? flat[idx - 1] : null;
  const next = idx >= 0 && idx < flat.length - 1 ? flat[idx + 1] : null;
  const tocHtml = toc.length
    ? `<nav class="toc"><div class="toc-title">${ui.onThisPage}</div>` +
      toc.map(t => `<a class="toc-l${t.level}" href="#${t.id}">${t.text}</a>`).join('') + '</nav>'
    : '';
  return `<!doctype html>
<html lang="${lang === 'zh' ? 'zh-CN' : 'en'}" data-theme="dark">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${title} · Ferroma</title>
<meta name="description" content="${escapeHtml(desc)}">
<link rel="icon" href="${favicon}">
<link rel="stylesheet" href="${root}${ASSETS.style}">
${themeBootstrap()}
</head>
<body data-root="${root}">
${topbarHtml({ lang, slug, root, ui, home: false })}
<div class="frame">
<div class="shell">
  ${sidebarHtml({ lang, slug, root, ui, home: false })}
  <div class="scrim" id="scrim"></div>
  <main class="content">
    ${kicker ? `<div class="kicker">${kicker}</div>` : ''}
    <article class="prose">${content}</article>
    <nav class="pager">
      ${prev ? `<a class="pager-prev" href="${root}${lang}/${prev.slug}.html"><small>← ${ui.previous}</small><b>${prev[lang].title}</b></a>` : '<span></span>'}
      ${next ? `<a class="pager-next" href="${root}${lang}/${next.slug}.html"><small>${ui.next} →</small><b>${next[lang].title}</b></a>` : '<span></span>'}
    </nav>
    <footer class="footer">Ferroma · AGPL-3.0-only · <a href="${REPO_URL}" target="_blank" rel="noopener">${ui.source}</a> · ${ui.builtFrom}</footer>
  </main>
  ${tocHtml ? `<aside class="tocpane">${tocHtml}</aside>` : ''}
</div>
</div>
<script src="${root}${ASSETS.search}"><\/script>
<script src="${root}${ASSETS.site}"><\/script>
</body>
</html>`;
}

const UI = {
  en: {
    searchPlaceholder: 'Search the docs…', onThisPage: 'On this page', home: 'Home',
    switchLang: 'Switch language', previous: 'Previous', next: 'Next', menu: 'Menu',
    search: 'Search', theme: 'Toggle light / dark',
    builtFrom: 'built from <code>docs/</code>', source: 'Source', noResults: 'No results',
  },
  zh: {
    searchPlaceholder: '搜索文档…', onThisPage: '本页目录', home: '首页',
    switchLang: '切换语言', previous: '上一篇', next: '下一篇', menu: '菜单',
    search: '搜索', theme: '切换浅色 / 深色',
    builtFrom: '由 <code>docs/</code> 生成', source: '源码仓库', noResults: '没有匹配结果',
  },
};

// ---------------------------------------------------------------------------
// 首页
// ---------------------------------------------------------------------------
// 按列画：主干固定在第 30 列，框边在 6 和 54，五条分支落在 6/18/30/42/54。
// 每个 │、+、v 的连接点都在这些列上，标签以各自的分支为中心——改一行就要重新数一遍。
const ARCH_ASCII = `                           FERROMA
                              │
      ┌───────────────────────┼───────────────────────┐
      │                       │                       │
               Webmail     Clients      Admin
      │                       │                       │
      └───────────────────────┼───────────────────────┘
                              │
                 HTTP API · FCP · WebSocket
                              │
                     ┌────────▼────────┐
                     │    Mail core    │
                     └────────┬────────┘
      ┌───────────┬───────────┼───────────┬───────────┐
      ▼           ▼           ▼           ▼           ▼
    SMTP        IMAP       Storage      Queue        DNS
      └───────────┴───────────┼───────────┴───────────┘
                              │
                      Event Bus ── Push
                              │
                         PostgreSQL`;

// 代码块与表格的构造器，供内置页面复用
function codeBlock(code, lang) {
  return `<figure class="codeblock"${lang ? ` data-lang="${lang}"` : ''}><pre><code>${highlight(asciiSafe(code), lang || 'text')}</code></pre>` +
         `<button class="copy-btn" type="button" data-copy aria-label="copy code">⧉</button></figure>`;
}

function table(head, rows) {
  return '<div class="tablewrap"><table><thead><tr>' + head.map(h => `<th>${h}</th>`).join('') +
         '</tr></thead><tbody>' + rows.map(r => '<tr>' + r.map(c => `<td>${c}</td>`).join('') + '</tr>').join('') +
         '</tbody></table></div>';
}

const LEDE = {
  en: 'A Rust-native, self-hosted mail platform — its own SMTP and IMAP servers, its own MIME core, its own storage engine. It does not wrap Postfix or Dovecot. The point is to own the whole stack.',
  zh: '一个 Rust 原生、可自托管的邮件平台——自研 SMTP 与 IMAP 服务器、自研 MIME 邮件核心与存储引擎，不封装 Postfix、Dovecot 等任何现有邮件服务器，目标是掌握全部技术栈。',
};

// 首页两种语言各一份：`/` 是英文，`/zh/` 是中文。刻意只放三样东西——
// 定位、一条最快路径、文档索引。能力清单和部署细节都在文档页里，
// 首页不是它们的摘要。
function buildIndexHtml(lang) {
  const zh = lang === 'zh';
  const root = zh ? '../' : '';
  const ui = UI[lang];
  // 每张卡片只说当前语言：英文页上不出现中文标题，中文页上不出现英文标题。
  const cards = PAGES.map(p =>
    `<a class="doc-card" href="${root}${lang}/${p.slug}.html">` +
    `<span class="doc-card-top"><span class="doc-card-num">${String(PAGES.indexOf(p) + 1).padStart(2, '0')}</span>` +
    `<span class="doc-card-title">${p[lang].title}</span></span>` +
    `<span class="doc-card-desc">${escapeHtml(p[lang].desc)}</span><span class="doc-card-go">→</span></a>`).join('');

  return `<!doctype html>
<html lang="${zh ? 'zh-CN' : 'en'}" data-theme="dark">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${zh ? 'Ferroma · Rust 原生自托管邮件平台' : 'Ferroma · A Rust-native, self-hosted mail platform'}</title>
<meta name="description" content="${zh
    ? 'Ferroma —— Rust 原生、可自托管的邮件平台：自研 SMTP 与 IMAP、自研 MIME 核心与存储引擎，含 Docker 与 Docker Compose 完整部署流程。'
    : 'Ferroma — a Rust-native, self-hosted mail platform with its own SMTP and IMAP servers, MIME core and storage engine. Docker and Docker Compose deployment included.'}">
<link rel="icon" href="${favicon}">
<link rel="stylesheet" href="${root}${ASSETS.style}">
${themeBootstrap()}
</head>
<body class="home" data-root="${root}">
${topbarHtml({ lang, slug: 'home', root, ui, home: true })}
<div class="frame">

<section class="home-hero">
  <div class="home-hero-inner">
    <div>
      <div class="home-badges">
        <span class="chip chip-brand">Rust-native</span>
        <span class="chip">Self-hosted</span>
        <span class="chip">AGPL-3.0-only</span>
        <span class="chip chip-mono">v${VERSION}</span>
      </div>
      <h1>Ferroma</h1>
      <p class="home-lede">${zh ? LEDE.zh : LEDE.en}</p>
      <div class="home-cta">
        <a class="btn btn-primary" href="${root}${lang}/deploy.html">${zh ? '部署指南' : 'Deployment guide'} ${ICON.arrow}</a>
        <a class="btn" href="${root}${lang}/architecture.html">${zh ? '阅读文档' : 'Read the docs'}</a>
      </div>
    </div>
    <div class="panel">
      <pre class="panel-pre"><code>${escapeHtml(asciiSafe(ARCH_ASCII))}</code></pre>
      <div class="panel-cap"><span>RFC 5321 · 3501 · 9051</span><span>${IMAGE}:${VERSION}</span></div>
    </div>
  </div>
</section>

<section class="home-section" id="quickstart">
  <div class="home-section-head">
    <h2>${zh ? '快速开始' : 'Quick start'}</h2>
    <p class="home-section-sub">${zh
      ? '一个容器、一个数据卷，没有 <code>.env</code>，也没有构建。数据库连接在浏览器里填。'
      : 'One container, one volume, no <code>.env</code> and no build. The database connection is entered in a browser.'}</p>
  </div>
  ${codeBlock(COMPOSE_QUICK[lang], 'yaml')}
  ${codeBlock(CMD_QUICK[lang], 'bash')}
  <div class="note">
    <span class="note-ico">i</span>
    <div>
      <p>${zh
        ? `<strong>一个地址走完两步。</strong>没有声明数据库的实例不会退出——它在 <code>/</code> 上要连接串和日志里的 setup code；验证通过后同一个进程继续启动，页面直接变成引导界面，在那里建第一个邮件域和管理员账号。前提是这台主机上已经有一个 PostgreSQL，Ferroma 只连接、不建库。完整说明（含纯 <code>docker run</code> 写法）见<a href="${root}${lang}/deploy.html#quick">部署指南</a>。`
        : `<strong>One address, two steps.</strong> An instance with no stated database does not exit — it asks for the connection and the setup code from its log at <code>/</code>, then continues booting in the same process and the page turns into the wizard that creates the first mail domain and administrator. It assumes a PostgreSQL already runs on this host; Ferroma connects to it but never creates it. The full version, including the plain <code>docker run</code> form, is in the <a href="${root}${lang}/deploy.html#quick">deployment guide</a>.`}</p>
    </div>
  </div>
</section>

<section class="home-section" id="docs">
  <div class="home-section-head">
    <h2>${zh ? '文档' : 'Documentation'}</h2>
    <p class="home-section-sub">${zh
      ? '全部文档中英各一份，可整站搜索。'
      : 'Every document exists in both languages, and the whole site is searchable.'}</p>
  </div>
  <div class="doc-grid">
    ${cards}
  </div>
</section>

<footer class="footer">
  Ferroma · v${VERSION} · AGPL-3.0-only ·
  <a href="${REPO_URL}" target="_blank" rel="noopener">${zh ? '源码仓库' : 'Source'}</a> ·
  ${ui.builtFrom}
</footer>
</div>
<script src="${root}${ASSETS.search}"><\/script>
<script src="${root}${ASSETS.site}"><\/script>
</body>
</html>`;
}

// ---------------------------------------------------------------------------
// 内置页面：部署指南
//
// 内容不来自 docs/ —— 它是站点自己的入口页，把三条部署路径压在一页里，
// 每一条命令都与仓库里的 Dockerfile / docker-compose*.yml / scripts/deploy.sh
// 一一对应。深水区（DNS、TLS、DKIM、备份、排障）留在 docs/deployment.md。
// ---------------------------------------------------------------------------
const DEPLOY_META = {
  en: { title: 'Deployment guide', desc: 'Three supported shapes, and the exact commands for each: Compose or plain Docker.' },
  zh: { title: '部署指南', desc: '三种受支持的部署形态，以及每一种的确切命令：Compose 或纯 Docker。' },
};

// 最短的一条路：一个容器、一个数据卷、没有任何数据库配置。
// 没声明连接时服务器进入 bootstrap 模式，在 web 端口上要连接串和 setup code，
// 连上之后同一个进程继续启动，页面直接变成引导界面（docs/deployment.md §3.5）。
// 这些块按语言分开：命令是同一份，注释不是——中文页面上不该出现英文说明。
const COMPOSE_QUICK = {
  en: [
    'services:',
    '  ferroma:',
    `    image: ${IMAGE}:${VERSION}`,
    '    container_name: ferroma',
    '    restart: always',
    '    # Binds 25/587/143/8080 on the host directly, so a PostgreSQL running',
    '    # on this host is simply 127.0.0.1. Linux only.',
    '    network_mode: host',
    '    volumes:',
    '      - ./data:/var/lib/ferroma',
    '      - ./tls:/etc/ferroma/tls:ro',
  ].join('\n'),
  zh: [
    'services:',
    '  ferroma:',
    `    image: ${IMAGE}:${VERSION}`,
    '    container_name: ferroma',
    '    restart: always',
    '    # 直接绑定宿主的 25/587/143/8080，于是宿主上的 PostgreSQL 就是',
    '    # 127.0.0.1。仅限 Linux。',
    '    network_mode: host',
    '    volumes:',
    '      - ./data:/var/lib/ferroma',
    '      - ./tls:/etc/ferroma/tls:ro',
  ].join('\n'),
};

const CMD_QUICK = {
  en: [
    'docker compose up -d',
    'docker compose logs ferroma        # read the setup code it prints',
    '',
    '# then open http://<host>:8080/ and enter the database address and that code',
  ].join('\n'),
  zh: [
    'docker compose up -d',
    'docker compose logs ferroma        # 读它打印出来的 setup code',
    '',
    '# 然后打开 http://<host>:8080/，填入数据库地址和那个 code',
  ].join('\n'),
};

const CMD_QUICK_RUN = {
  en: [
    'mkdir -p data tls',
    'sudo chown -R 10001:10001 data     # the container runs as uid 10001',
    '',
    'docker run -d --name ferroma --restart always \\',
    '  --network host \\',
    '  -v "$PWD/data:/var/lib/ferroma" \\',
    '  -v "$PWD/tls:/etc/ferroma/tls:ro" \\',
    `  ${IMAGE}:${VERSION}`,
    '',
    'docker logs ferroma                # read the setup code it prints',
  ].join('\n'),
  zh: [
    'mkdir -p data tls',
    'sudo chown -R 10001:10001 data     # 容器以 uid 10001 运行',
    '',
    'docker run -d --name ferroma --restart always \\',
    '  --network host \\',
    '  -v "$PWD/data:/var/lib/ferroma" \\',
    '  -v "$PWD/tls:/etc/ferroma/tls:ro" \\',
    `  ${IMAGE}:${VERSION}`,
    '',
    'docker logs ferroma                # 读它打印出来的 setup code',
  ].join('\n'),
};

// 服务器进入 bootstrap 模式时打印的东西，逐字取自 server/src/bootstrap.rs。
const BOOTSTRAP_BANNER = [
  'No database is connected yet. Open http://0.0.0.0:8080/ and enter:',
  '',
  '    address   postgres://user:password@host:5432/ferroma',
  '    code      7JVTQAHO',
].join('\n');

const CMD_PATH_A = {
  en: [
    `git clone ${REPO_URL} && cd Ferroma`,
    'cp .env.example .env',
    '',
    '# .env — two values are required before the first pull',
    'POSTGRES_PASSWORD=<a long random password>',
    `FERROMA_VERSION=${VERSION}`,
    '',
    'docker compose -f docker-compose.yml pull',
    'docker compose -f docker-compose.yml up -d',
    'docker compose -f docker-compose.yml ps',
  ].join('\n'),
  zh: [
    `git clone ${REPO_URL} && cd Ferroma`,
    'cp .env.example .env',
    '',
    '# .env —— 首次拉取之前，这两行是必填的',
    'POSTGRES_PASSWORD=<一串够长的随机密码>',
    `FERROMA_VERSION=${VERSION}`,
    '',
    'docker compose -f docker-compose.yml pull',
    'docker compose -f docker-compose.yml up -d',
    'docker compose -f docker-compose.yml ps',
  ].join('\n'),
};

const CMD_PATH_B = [
  `git clone ${REPO_URL} && cd Ferroma`,
  './scripts/deploy.sh',
].join('\n');

const CMD_PATH_B_SUBS = {
  en: [
    './scripts/deploy.sh                 # first deployment, or re-apply .env and restart',
    './scripts/deploy.sh status          # containers / health / database',
    './scripts/deploy.sh logs            # follow the Ferroma log',
    './scripts/deploy.sh upgrade         # rebuild the image, restart, wait for health',
    './scripts/deploy.sh dkim --enable   # turn signing on after the TXT record is published',
    './scripts/deploy.sh certs           # re-install a renewed certificate and restart',
    './scripts/deploy.sh doctor          # run `ferroma doctor` inside the container',
    './scripts/deploy.sh down [--volumes]',
  ].join('\n'),
  zh: [
    './scripts/deploy.sh                 # 首次部署，或重新应用 .env 并重启',
    './scripts/deploy.sh status          # 容器 / 健康 / 数据库',
    './scripts/deploy.sh logs            # 跟随 Ferroma 日志',
    './scripts/deploy.sh upgrade         # 重建镜像、重启、等健康检查',
    './scripts/deploy.sh dkim --enable   # TXT 记录发布之后再打开签名',
    './scripts/deploy.sh certs           # 装入续期后的证书并重启',
    './scripts/deploy.sh doctor          # 在容器里跑 ferroma doctor',
    './scripts/deploy.sh down [--volumes]',
  ].join('\n'),
};

const CMD_PATH_B_CI = [
  './scripts/deploy.sh --yes --domain example.com --admin admin@example.com \\',
  '  --db-password "$DB_PW" \\',
  '  --tls-cert /etc/letsencrypt/live/mail.example.com/fullchain.pem \\',
  '  --tls-key  /etc/letsencrypt/live/mail.example.com/privkey.pem',
].join('\n');

const CMD_PATH_C = {
  en: [
    'DB_PW=$(openssl rand -hex 24)',
    'DATABASE_URL="postgres://ferroma:$DB_PW@ferroma-postgres:5432/ferroma"',
    '',
    '# 1. one private network for the two containers',
    'docker network create ferroma',
    '',
    '# 2. the database — never published to the host',
    'docker run -d --name ferroma-postgres --restart always --network ferroma \\',
    '  -e POSTGRES_USER=ferroma \\',
    '  -e POSTGRES_PASSWORD="$DB_PW" \\',
    '  -e POSTGRES_DB=ferroma \\',
    "  -e POSTGRES_INITDB_ARGS='--encoding=UTF8 --locale=C' \\",
    '  -v ferroma-postgres-data:/var/lib/postgresql/data \\',
    '  postgres:16-alpine',
    '',
    '# 3. create the schema, then migrate (idempotent; safe to re-run)',
    `docker run --rm --network ferroma -e DATABASE_URL="$DATABASE_URL" \\`,
    `  -v ferroma-data:/var/lib/ferroma ${IMAGE}:${VERSION} database init`,
    '',
    '# 4. start the server',
    'docker run -d --name ferroma --restart always --network ferroma \\',
    '  -e DATABASE_URL="$DATABASE_URL" \\',
    '  -e FERROMA_DATA_DIR=/var/lib/ferroma \\',
    '  -p 25:25 -p 587:587 -p 143:143 -p 8080:8080 \\',
    '  -v ferroma-data:/var/lib/ferroma \\',
    `  ${IMAGE}:${VERSION}`,
  ].join('\n'),
  zh: [
    'DB_PW=$(openssl rand -hex 24)',
    'DATABASE_URL="postgres://ferroma:$DB_PW@ferroma-postgres:5432/ferroma"',
    '',
    '# 1. 两个容器共用的一条私有网络',
    'docker network create ferroma',
    '',
    '# 2. 数据库 —— 永远不发布到宿主',
    'docker run -d --name ferroma-postgres --restart always --network ferroma \\',
    '  -e POSTGRES_USER=ferroma \\',
    '  -e POSTGRES_PASSWORD="$DB_PW" \\',
    '  -e POSTGRES_DB=ferroma \\',
    "  -e POSTGRES_INITDB_ARGS='--encoding=UTF8 --locale=C' \\",
    '  -v ferroma-postgres-data:/var/lib/postgresql/data \\',
    '  postgres:16-alpine',
    '',
    '# 3. 建表并迁移（幂等，重复执行是安全的）',
    `docker run --rm --network ferroma -e DATABASE_URL="$DATABASE_URL" \\`,
    `  -v ferroma-data:/var/lib/ferroma ${IMAGE}:${VERSION} database init`,
    '',
    '# 4. 启动服务',
    'docker run -d --name ferroma --restart always --network ferroma \\',
    '  -e DATABASE_URL="$DATABASE_URL" \\',
    '  -e FERROMA_DATA_DIR=/var/lib/ferroma \\',
    '  -p 25:25 -p 587:587 -p 143:143 -p 8080:8080 \\',
    '  -v ferroma-data:/var/lib/ferroma \\',
    `  ${IMAGE}:${VERSION}`,
  ].join('\n'),
};

const CMD_PATH_C_TLS = {
  en: [
    '# Once ./tls holds fullchain.pem and privkey.pem, recreate the container',
    '# with the implicit-TLS listeners on and the certificate mounted.',
    'docker rm -f ferroma',
    'docker run -d --name ferroma --restart always --network ferroma \\',
    '  -e DATABASE_URL="$DATABASE_URL" \\',
    '  -e FERROMA_DATA_DIR=/var/lib/ferroma \\',
    "  -e FERROMA_TLS_ENABLED=true \\",
    "  -e FERROMA_TLS_CERT=/etc/ferroma/tls/fullchain.pem \\",
    '  -e FERROMA_TLS_KEY=/etc/ferroma/tls/privkey.pem \\',
    "  -e FERROMA__SMTP__SMTPS_PORT=465 \\",
    "  -e FERROMA__IMAP__IMAPS_PORT=993 \\",
    '  -p 25:25 -p 587:587 -p 465:465 -p 143:143 -p 993:993 -p 8080:8080 \\',
    '  -v ferroma-data:/var/lib/ferroma -v "$PWD/tls:/etc/ferroma/tls:ro" \\',
    `  ${IMAGE}:${VERSION}`,
  ].join('\n'),
  zh: [
    '# ./tls 里有了 fullchain.pem 与 privkey.pem 之后，带着隐式 TLS 监听',
    '# 和挂载好的证书，重新创建这个容器。',
    'docker rm -f ferroma',
    'docker run -d --name ferroma --restart always --network ferroma \\',
    '  -e DATABASE_URL="$DATABASE_URL" \\',
    '  -e FERROMA_DATA_DIR=/var/lib/ferroma \\',
    "  -e FERROMA_TLS_ENABLED=true \\",
    "  -e FERROMA_TLS_CERT=/etc/ferroma/tls/fullchain.pem \\",
    '  -e FERROMA_TLS_KEY=/etc/ferroma/tls/privkey.pem \\',
    "  -e FERROMA__SMTP__SMTPS_PORT=465 \\",
    "  -e FERROMA__IMAP__IMAPS_PORT=993 \\",
    '  -p 25:25 -p 587:587 -p 465:465 -p 143:143 -p 993:993 -p 8080:8080 \\',
    '  -v ferroma-data:/var/lib/ferroma -v "$PWD/tls:/etc/ferroma/tls:ro" \\',
    `  ${IMAGE}:${VERSION}`,
  ].join('\n'),
};

// 服务商不给 PTR 时的出站中继。字段名与语义取自 config/ferroma.toml 的 [queue] 段。
const CMD_RELAY = {
  en: [
    'services:',
    '  ferroma:',
    `    image: ${IMAGE}:${VERSION}`,
    '    environment:',
    '      FERROMA__QUEUE__RELAY_HOST: smtp.example-relay.com',
    '      FERROMA__QUEUE__RELAY_PORT: "587"',
    '      FERROMA__QUEUE__RELAY_TLS: starttls',
    '      FERROMA__QUEUE__RELAY_USERNAME: your-username',
    '      FERROMA__QUEUE__RELAY_PASSWORD: your-password',
    '      # Empty means every outbound message goes through the relay.',
    '      # FERROMA__QUEUE__RELAY_FROM_DOMAINS: \'["example.com"]\'',
  ].join('\n'),
  zh: [
    'services:',
    '  ferroma:',
    `    image: ${IMAGE}:${VERSION}`,
    '    environment:',
    '      FERROMA__QUEUE__RELAY_HOST: smtp.example-relay.com',
    '      FERROMA__QUEUE__RELAY_PORT: "587"',
    '      FERROMA__QUEUE__RELAY_TLS: starttls',
    '      FERROMA__QUEUE__RELAY_USERNAME: 你的用户名',
    '      FERROMA__QUEUE__RELAY_PASSWORD: 你的口令',
    '      # 留空 = 所有外发邮件都走中继',
    '      # FERROMA__QUEUE__RELAY_FROM_DOMAINS: \'["example.com"]\'',
  ].join('\n'),
};

const CMD_OPS = {
  en: [
    '# the container is the only thing to look at on this host',
    'docker logs -f --tail=200 ferroma',
    '',
    '# a shell inside it, as the ferroma service user',
    'docker exec -it ferroma sh',
    '',
    '# the preflight: what serve needs, and what would go wrong',
    'docker exec ferroma ferroma doctor',
    '',
    '# which ports are actually bound',
    'docker exec ferroma ferroma config show',
  ].join('\n'),
  zh: [
    '# 这台主机上只有这一个容器需要看',
    'docker logs -f --tail=200 ferroma',
    '',
    '# 进容器拿一个 shell，身份是 ferroma 服务用户',
    'docker exec -it ferroma sh',
    '',
    '# 预检：serve 需要什么，以及哪一步会出问题',
    'docker exec ferroma ferroma doctor',
    '',
    '# 实际绑定了哪些端口',
    'docker exec ferroma ferroma config show',
  ].join('\n'),
};

const CMD_HEALTH = [
  'docker exec ferroma ferroma healthcheck',
  'curl -s localhost:8080/api/v1/health',
  'dig +short MX example.com',
  'dig +short TXT default._domainkey.example.com',
].join('\n');

function deploySections(lang) {
  const zh = lang === 'zh';
  const S = [];
  // Anchors into docs/deployment.md are language-specific: the Chinese headings
  // slugify to Chinese ids ("2. DNS 记录" → #2-dns-记录), so an English anchor
  // pointed at the Chinese page silently lands on top of the document.
  const doc = zh ? '../zh' : '../en';
  const A = zh
    ? { dns: '2-dns-记录', tls: '5-端口与-tls', backup: '8-备份与恢复' }
    : { dns: '2-dns-records', tls: '5-ports-and-tls', backup: '8-backup-and-restore' };

  S.push({ id: 'shape', title: zh ? '先选形态' : 'Pick a shape', html:
    `<p>${zh
      ? '三种形态都是受支持的部署方式，区别只在于“这台主机上已经有什么”。选一种走到底，不要混着来。'
      : 'All three are supported. The only question is what the host already has. Pick one and stay with it.'}</p>` +
    table(
      zh ? ['形态', '适合的主机', '数据库', 'TLS', '用到的文件']
         : ['Shape', 'The host', 'Database', 'TLS', 'Files'],
      zh ? [
        ['<b>Q</b> · 最快的一条路 <span class="chip chip-brand">先看这个</span>', '主机上已经跑着 PostgreSQL', '你已有的那一个（Ferroma 不建库）', '全明文，仅限本机', '你自己的一个 compose 文件，或一条 <code>docker run</code>'],
        ['<b>A</b> · Compose，自带数据库', '只有 Docker 的干净主机', '<code>postgres:16-alpine</code> 容器', 'Ferroma 终止 465/993；HTTPS 交给反向代理', '<code>docker-compose.yml</code>'],
        ['<b>B</b> · Compose，复用现有 PostgreSQL <span class="chip chip-green">推荐</span>', '已经在跑 PostgreSQL 和反向代理', '你自己的服务器，经 <code>127.0.0.1</code>', '反向代理终止 HTTPS；Ferroma 终止 465/993', '<code>docker-compose.yml</code> + <code>scripts/deploy.sh</code>'],
        ['<b>C</b> · 纯 Docker', '只有 Docker 的干净主机', '你自己起的容器', '你自己安排', '<code>docker run</code>'],
      ] : [
        ['<b>Q</b> · the fastest path <span class="chip chip-brand">start here</span>', 'PostgreSQL already runs on this host', 'the one you have (Ferroma never creates it)', 'all plaintext, this host only', 'your own compose file, or one <code>docker run</code>'],
        ['<b>A</b> · Compose, its own database', 'anything with Docker', '<code>postgres:16-alpine</code> container', 'Ferroma terminates 465/993; a reverse proxy terminates HTTPS', '<code>docker-compose.yml</code>'],
        ['<b>B</b> · Compose, existing PostgreSQL <span class="chip chip-green">recommended</span>', 'already runs PostgreSQL and a reverse proxy', 'your own server, over <code>127.0.0.1</code>', 'your proxy terminates HTTPS; Ferroma terminates 465/993', '<code>docker-compose.yml</code> + <code>scripts/deploy.sh</code>'],
        ['<b>C</b> · Plain Docker', 'anything with Docker', 'a container you run yourself', 'your own arrangement', '<code>docker run</code>'],
      ]) +
    `<div class="note note-warn"><span class="note-ico">!</span><div><p>${
      zh ? '<code>docker-compose.yml</code>（没有后缀的那个）是开发栈：IMAP 与 API 明文、没有资源限制、143 端口未加密地暴露。不要把它放到公网上。'
         : '<code>docker-compose.yml</code> (no suffix) is the development stack: plaintext IMAP and API, no resource limits, port 143 bound unencrypted. Do not put it on the internet.'
    }</p></div></div>` });

  S.push({ id: 'quick', title: zh ? '最快的一条路：一个容器，数据库在浏览器里填' : 'The fastest path: one container, the database in the browser', html:
    `<p>${zh
      ? '这条路上没有 <code>.env</code>、没有构建、没有预先配好的连接串：一个发布镜像、一个数据卷，剩下的两步都在浏览器里完成。前提是这台主机上已经有一个 PostgreSQL（1Panel 装的、发行版装的都算），因为 Ferroma 只连接、只应用 schema，<b>从不执行 <code>CREATE DATABASE</code></b>。'
      : 'No <code>.env</code>, no build, no pre-configured connection string: one released image, one data volume, and the remaining two steps happen in a browser. It assumes a PostgreSQL already runs on this host (1Panel\'s, or the distribution\'s) — Ferroma only connects and applies its schema, it <b>never runs <code>CREATE DATABASE</code></b>.'}</p>` +
    `<h3>${zh ? '用 Docker Compose' : 'With Docker Compose'}</h3>` +
    codeBlock(COMPOSE_QUICK[lang], 'yaml') +
    codeBlock(CMD_QUICK[lang], 'bash') +
    `<h3>${zh ? '或者，纯 docker run' : 'Or with plain docker run'}</h3>` +
    codeBlock(CMD_QUICK_RUN[lang], 'bash') +
    `<p>${zh ? '两种写法起的是同一个容器。它的日志会打印这一段：' : 'Both start the same container, and its log prints this:'}</p>` +
    codeBlock(BOOTSTRAP_BANNER, 'text') +
    `<div class="note"><span class="note-ico">i</span><div><p>${
      zh ? '<b>一个地址走完两步。</b>没有声明数据库的实例不会退出——它照常绑定 web 端口，在 <code>/</code> 上要连接串和 code。连接被验证、schema 被应用之后，<b>同一个进程、同一个端口继续启动</b>，没有重启，页面直接变成引导界面，在那里建第一个邮件域和管理员账号。'
         : '<b>One address, two steps.</b> An instance with no stated database does not exit — it binds the web port anyway and asks for the connection and the code at <code>/</code>. Once the connection is verified and the schema applied, <b>the same process continues booting on the same port</b>: no restart, and the page turns straight into the wizard that creates the first mail domain and administrator.'
    }</p></div></div>` +
    `<p>${zh ? '四件必须知道的事：' : 'Four things worth knowing:'}</p>` +
    table(
      zh ? ['', ''] : ['', ''],
      zh ? [
        ['<b><code>setup code</code> 是必需的</b>', '每次启动生成、只打印在日志里。否则任何能访问这个 web 端口的人，都能把你的实例指向他自己的数据库'],
        ['<b>数据库必须已经存在</b>', '角色只需要那个库的使用权限；建库是操作员的事，不是 Ferroma 的事'],
        ['<b>地址会被记住</b>', '写进 <code>&lt;data_dir&gt;/database.json</code>（0600，里面有密码），之后每次启动从磁盘读，不再问'],
        ['<b>没有重启</b>', '连接在同一个进程里生效。想让部署来指定连接串，就在 <code>.env</code> 里写 <code>DATABASE_URL</code>——写了就以部署为准，启动失败也会是响亮的那种'],
      ] : [
        ['<b>The <code>setup code</code> is required</b>', 'generated every start, printed only to the log. Without it, anyone who can reach the published web port could point this instance at a database of their choosing'],
        ['<b>The database must already exist</b>', 'the role you name only needs the rights to use it; creating it is the operator\'s job, not Ferroma\'s'],
        ['<b>The address is remembered</b>', 'written to <code>&lt;data_dir&gt;/database.json</code> (mode 0600 — it holds a password) and read from disk on every later start'],
        ['<b>There is no restart</b>', 'the connection takes effect inside the same process. To have the deployment state it instead, set <code>DATABASE_URL</code> in <code>.env</code> — stated, it wins, and a wrong one is a loud failure at startup'],
      ]) +
    `<p>${zh
      ? '数据库连上之后，<code>/</code> 仍然是控制台，直到第一个管理员存在；再之后它才变成 Webmail 的登录页。'
      : 'After the database is connected, <code>/</code> is still the console until the first administrator exists — only then does it become the Webmail sign-in.'}</p>` +
    `<div class="note note-warn"><span class="note-ico">!</span><div><p>${
      zh ? '<b>这条路是明文，而且只限本机试跑。</b>容器以 <code>network_mode: host</code> 把 25/587/143/8080 原样绑在主机上，143 未加密，没有 TLS，没有资源上限，也没有重启策略之外的保护。把界面点一遍、给自己发第一封信，然后按下面的路径 A 或 B 上公网。'
         : '<b>This path is plaintext, and meant for a local trial.</b> With <code>network_mode: host</code> the container binds 25/587/143/8080 straight onto the host — 143 unencrypted, no TLS, no resource limits, nothing beyond a restart policy. Click through the console, send yourself a first message, then take path A or B below to put it on the internet.'
    }</p></div></div>` });

  S.push({ id: 'before', title: zh ? '开工之前' : 'Before the first boot', html:
    table(
      zh ? ['前提', '为什么'] : ['Requirement', 'Why it matters'],
      zh ? [
        ['一个静态公网 IPv4', 'MX 需要一个稳定地址，而且 PTR 记录必须与它一致。<b>服务商不给设 PTR 的话，看下一节</b>'],
        ['25 端口<b>入站</b>可达', '接收别的服务器投来的邮件。很多 VPS 厂商默认封禁，开工前先让厂商放开'],
        ['25 端口<b>出站</b>可达', '把邮件投递出去'],
        ['一个你控制的域名', '下文一律用 <code>example.com</code>'],
        ['Docker Engine 24+ 与 Compose 插件', '<code>docker compose</code>，不是 <code>docker-compose</code>'],
        ['1 vCPU、1 GB 内存、20 GB 磁盘', '小型部署足够。Ferroma 自身只占几十 MB，内存是给 PostgreSQL 和页缓存的；邮件存储会一直长'],
      ] : [
        ['A static public IPv4 address', 'an MX needs a stable address, and the PTR record must match it. <b>If your provider will not set a PTR, read the next section</b>'],
        ['Port 25 reachable <b>inbound</b>', 'receiving mail from other servers. Many VPS providers block it by default — ask them to open it first'],
        ['Port 25 reachable <b>outbound</b>', 'delivering mail'],
        ['A domain you control', '<code>example.com</code> in every example below'],
        ['Docker Engine 24+ with the Compose plugin', '<code>docker compose</code>, not <code>docker-compose</code>'],
        ['1 vCPU, 1 GB RAM, 20 GB disk', 'a small deployment. Ferroma itself stays in the tens of megabytes — the memory belongs to PostgreSQL and the page cache — and the mail store grows'],
      ]) +
    `<div class="note note-warn"><span class="note-ico">!</span><div><p><strong>${
      zh ? 'DNS 先行。' : 'DNS first.'
    }</strong>${
      zh ? 'MX / A / PTR / SPF / DKIM / DMARC 没有就位之前，无论 Ferroma 配得多正确，发出的邮件都会被拒收，也收不到任何邮件。完整记录表在'
         : 'Until MX / A / PTR / SPF / DKIM / DMARC are in place, a perfectly configured Ferroma will have its outbound mail rejected and will receive nothing. The full record table is in'
    } <a href="${doc}/deployment.html#${A.dns}">${
      zh ? '部署参考 §2' : 'Deployment reference §2'
    }</a>。</p></div></div>` });

  S.push({ id: 'relay', title: zh ? '服务商不给 PTR：让出站走中继' : 'No PTR from your provider: relay the outbound path', html:
    `<p>${zh
      ? 'PTR（反向 DNS）由 IP 段的持有者设置 —— 通常是 VPS 服务商的控制面板。不少服务商根本不提供这个入口，或者要开工单才给设。而没有 PTR、或 PTR 与 <code>EHLO</code> 名不符的 IP，是"合法邮件被丢弃或拒收"最常见的原因：Gmail 丢进垃圾箱，Microsoft 名下各个域直接拒收。'
      : 'Reverse DNS is set by whoever owns the IP block — usually your VPS provider\'s control panel. Plenty of providers do not offer the setting at all, or want a support ticket first. And an IP with no PTR, or a PTR that disagrees with the name in the <code>EHLO</code>, is the single most common reason legitimate mail is junked or refused: Gmail files it as spam, Microsoft\'s properties refuse it outright.'}</p>` +
    `<p>${zh
      ? '<b>出路是让出站走中继（smarthost）。</b>把外发交给一个已经做好 PTR 的中继 —— 你自己的另一台机器，或商业 SMTP relay 服务。收件方看到的是中继的 IP 和它的 PTR，你的 IP 不再出现在出站路径上。'
      : '<b>The way out is an outbound relay (a smarthost).</b> Hand delivery to something that already has its PTR right — another machine of yours, or a commercial SMTP relay. What the receiver sees is the relay\'s address and the relay\'s PTR; your IP stops appearing in the outbound path at all.'}</p>` +
    `<div class="note"><span class="note-ico">i</span><div><p>${
      zh ? '<b>接收完全不受影响。</b>只有出站需要中继：MX 仍然指向你的服务器，25 端口仍然要开，别人发来的邮件照收不误。'
         : '<b>Receiving is untouched.</b> Only outbound needs the relay: the MX still points at your server, port 25 still has to be open, and mail from everyone else still arrives.'
    }</p></div></div>` +
    codeBlock(CMD_RELAY[lang], 'yaml') +
    `<p>${zh ? '三个 <code>relay_tls</code> 取值：' : 'The three <code>relay_tls</code> values:'}</p>` +
    table(
      zh ? ['取值', '含义'] : ['Value', 'Meaning'],
      zh ? [
        ['<code>starttls</code>', '587：先明文连接，再升级到 TLS'],
        ['<code>implicit</code>', '465：连上就是 TLS'],
        ['<code>none</code>', '内部中继、可信网络。<b>绝不要和凭据一起用</b>'],
      ] : [
        ['<code>starttls</code>', '587: connect in the clear, then upgrade'],
        ['<code>implicit</code>', '465: TLS from the first byte'],
        ['<code>none</code>', 'an internal relay on a trusted network. <b>Never combined with credentials</b>'],
      ]) +
    `<p>${zh
      ? '<code>relay_from_domains</code> 留空 = 每一封外发邮件都走中继；填了域名列表 = 只有列出的域走中继，其余仍然按 MX 直发。'
      : '<code>relay_from_domains</code> empty means every outbound message goes through the relay; a list of domains sends only those through it and leaves the rest resolving by MX as usual.'}</p>` +
    `<p>${zh ? '配了中继之后，DNS 里有两行要跟着变：' : 'Two DNS records change when you relay:'}</p>` +
    table(
      zh ? ['记录', '怎么变'] : ['Record', 'What changes'],
      zh ? [
        ['<b>SPF</b>', '投递方现在是中继的 IP，所以记录要 <code>include</code> 中继服务商自己的域，例如 <code>v=spf1 include:_spf.relay.example -all</code>。那个名字无法从中继主机名推出来，得问服务商要'],
        ['<b>PTR</b>', '不再需要。收件方反查的是中继的地址，不是你的'],
      ] : [
        ['<b>SPF</b>', 'the sender is now the relay\'s IP, so the record has to <code>include</code> the provider\'s own domain — <code>v=spf1 include:_spf.relay.example -all</code>. Nothing can guess that name from the relay hostname; ask the provider for it'],
        ['<b>PTR</b>', 'no longer needed. The reverse lookup a receiver performs is on the relay\'s address, not yours'],
      ]) +
    `<p>${zh
      ? '管理面板的 DNS 面板就是按 <code>[queue] relay_host</code> 判断这两行的：配了中继之后，缺失的 PTR 从"缺陷"降级为"仅供参考"，而 SPF 会提示你补上那个 include。'
      : 'The DNS panel in the console reads <code>[queue] relay_host</code> to decide both rows: with a relay configured, a missing PTR drops from a defect to a note, and the SPF row asks for that include instead.'}</p>` });

  S.push({ id: 'path-a', title: zh ? '路径 A · Compose，自带数据库' : 'Path A · Compose with its own database', html:
    `<p>${zh
      ? 'Compose 起两个容器：<code>postgres</code> 只在 <code>ferroma-internal</code> 网络里可达，<code>ferroma</code> 发布 25/587/465/143/993 与一个 HTTP 端口。数据库带真实负载的调优参数，两个容器都有资源上限、<code>restart: always</code> 和有界的日志。'
      : 'Compose brings up two containers: <code>postgres</code>, reachable only on the <code>ferroma-internal</code> network, and <code>ferroma</code>, which publishes 25/587/465/143/993 plus one HTTP port. The database is tuned for a real workload, and both containers carry resource limits, <code>restart: always</code> and bounded logs.'}</p>` +
    codeBlock(CMD_PATH_A[lang], 'bash') +
    `<p>${zh
      ? '随后浏览器打开 <code>http://&lt;host&gt;:8080</code> 走向导。生产环境要在这个端口前面放反向代理做 HTTPS —— Ferroma 自己没有 HTTPS 监听器，Webmail / Admin / API 都走这一个明文端口。'
      : 'Then open <code>http://&lt;host&gt;:8080</code> in a browser and walk the wizard. In production put a reverse proxy in front of that port for HTTPS — Ferroma has no HTTPS listener of its own; Webmail, Admin and the API all come out of this one plaintext port.'}</p>` });

  S.push({ id: 'path-b', title: zh ? '路径 B · Compose，复用现有 PostgreSQL' : 'Path B · Compose on a host that already runs PostgreSQL', html:
    `<p>${zh
      ? '主机已经在跑 PostgreSQL，也已经有反向代理占着 443 —— 不要再起第二个数据库容器。<code>scripts/deploy.sh</code> 一次走完首次部署，并且<b>不改动你数据库的任何配置</b>：容器用 <code>network_mode: host</code>，本地数据库就是 <code>127.0.0.1:5432</code>，<code>listen_addresses</code> 与 <code>pg_hba.conf</code> 都不用碰。'
      : 'The host already runs PostgreSQL and a reverse proxy owns 443 — do not start a second database container. <code>scripts/deploy.sh</code> walks the whole first run and <b>never edits your database server\'s configuration</b>: the container uses <code>network_mode: host</code>, so the local database is simply <code>127.0.0.1:5432</code>, and neither <code>listen_addresses</code> nor <code>pg_hba.conf</code> is touched.'}</p>` +
    codeBlock(CMD_PATH_B, 'bash') +
    `<p>${zh ? '它按顺序做这些事，任一步失败都会打印出修复它的确切命令：' : 'It does the following, in order; any failure prints the exact command that fixes it:'}</p>` +
    table(
      zh ? ['步骤', '做什么'] : ['Step', 'What it does'],
      zh ? [
        ['1. 预检', '<code>docker</code>、Compose 插件、compose 文件是否齐备'],
        ['2. 收集配置', '交互询问：邮件域、MX 主机名、管理员地址、数据库地址、API 端口（默认 <code>127.0.0.1:18080</code>）'],
        ['3. 写 <code>.env</code>', '生成随机数据库口令与 <code>FERROMA_JWT_SECRET</code>，权限 600 —— 这是唯一的配置文件'],
        ['4. 建角色与数据库', '依次尝试 <code>sudo -u postgres</code>、宿主机上 PostgreSQL 容器里的 <code>psql</code>、<code>--pg-password</code> 指定的超级用户；都不行就打印可直接粘贴的 SQL'],
        ['5. 构建镜像', '本地 <code>docker build</code>（首次 10–30 分钟）。加 <code>--image ' + IMAGE + ':' + VERSION + '</code> 则改为直接拉取发布镜像，跳过构建'],
        ['6. 建表', '在容器里执行 <code>ferroma database init</code>（数据库不存在时一并创建）'],
        ['7. 安装证书', '把证书按 uid 10001 装进 <code>./tls</code> 供 465/993 使用，并检查 SAN 是否覆盖 MX 主机名'],
        ['8. 启动', '<code>docker compose up -d</code>，最多等 3 分钟健康检查，超时则打印日志'],
        ['9. 首次初始化', '创建域与管理员账号（口令只打印一次）、生成 DKIM 密钥并打印待发布的 TXT 记录'],
        ['10. 汇总', '反向代理片段、仍缺失的 DNS 记录，以及日常命令'],
      ] : [
        ['1. Preflight', 'are <code>docker</code>, the compose plugin and the compose files all present'],
        ['2. Collect configuration', 'asks interactively: mail domain, MX hostname, admin address, database address, API port (default <code>127.0.0.1:18080</code>)'],
        ['3. Write <code>.env</code>', 'generates a random database password and <code>FERROMA_JWT_SECRET</code>, mode 600 — it is the only configuration file'],
        ['4. Create the role and the database', 'tries <code>sudo -u postgres</code>, then the <code>psql</code> inside a PostgreSQL container on this host, then the superuser named by <code>--pg-password</code>; prints pasteable SQL if none works'],
        ['5. Build the image', 'a local <code>docker build</code> (10–30 minutes the first time). Pass <code>--image ' + IMAGE + ':' + VERSION + '</code> to pull the release instead and skip the build'],
        ['6. Create the schema', 'runs <code>ferroma database init</code> in the container (which also creates the database when it is missing)'],
        ['7. Install the certificate', 'installs it into <code>./tls</code> as uid 10001 for 465/993, and checks that the SAN covers the MX hostname'],
        ['8. Start', '<code>docker compose up -d</code>, waiting up to 3 minutes for the health check and printing the log on timeout'],
        ['9. First-run initialisation', 'creates the domain and the admin account (the password is printed once), generates the DKIM key and prints the TXT record to publish'],
        ['10. Summary', 'the reverse-proxy snippet, the DNS records still missing, and the everyday commands'],
      ]) +
    `<p>${zh ? '之后你只会用到这几个子命令：' : 'After that these are the only sub-commands you will type:'}</p>` +
    codeBlock(CMD_PATH_B_SUBS[lang], 'bash') +
    `<p>${zh ? '无人值守（cloud-init、CI）时，每个提问都有对应的旗标：' : 'For an unattended run (cloud-init, CI), every prompt has a flag:'}</p>` +
    codeBlock(CMD_PATH_B_CI, 'bash') +
    `<div class="note"><span class="note-ico">i</span><div><p>${
      zh ? '没有证书时脚本会关掉 TLS 并明确告警：那时 587 上没有 STARTTLS，邮件客户端无法认证，只能用来把数据库和 API 先跑起来。'
         : 'With no certificate the script turns TLS off and warns explicitly: there is then no STARTTLS on 587, mail clients cannot authenticate, and it is only good for getting the database and the API up.'
    }</p></div></div>` });

  S.push({ id: 'path-c', title: zh ? '路径 C · 纯 Docker，不用 Compose' : 'Path C · Plain Docker, no Compose', html:
    `<p>${zh
      ? 'Compose 只是把下面这些接线写成了声明。不想引入 Compose 时，就手工做同样的事：一个私有网络、一个数据库容器、一次建库迁移、一次启动。'
      : 'Compose is only a declaration of the wiring below. If you would rather not introduce Compose, do the same thing by hand: one private network, one database container, one schema creation, one start.'}</p>` +
    codeBlock(CMD_PATH_C[lang], 'bash') +
    `<p>${zh
      ? '<code>465</code> 与 <code>993</code> 在默认配置里是 <code>0</code>（关闭）。拿到证书之后再带上 TLS 重新起容器：'
      : '<code>465</code> and <code>993</code> are <code>0</code> (off) in the shipped default configuration. Once you have a certificate, recreate the container with TLS on:'}</p>` +
    codeBlock(CMD_PATH_C_TLS[lang], 'bash') +
    `<div class="note note-warn"><span class="note-ico">!</span><div><p>${
      zh ? '这条路少了 Compose 给你的三样东西：<code>restart: always</code> 之外的崩溃恢复编排、资源上限、以及有界日志。生产环境请用 <code>--log-opt max-size=20m --log-opt max-file=10</code> 之类的手段自行补上，否则 json-file 日志会把磁盘吃满。'
         : 'This path gives up three things Compose was doing for you: resource limits, bounded logs, and orchestration beyond <code>restart: always</code>. In production add <code>--log-opt max-size=20m --log-opt max-file=10</code> and <code>--memory 2g</code> yourself, or a json-file log will fill the disk.'
    }</p></div></div>` });

  S.push({ id: 'env', title: zh ? '真正要设的环境变量' : 'The environment variables that matter', html:
    `<p>${zh
      ? '变量名是双下划线形式：<code>FERROMA__API__PORT</code> 对应 <code>ferroma.toml</code> 里的 <code>[api] port</code>。除了下面这些，其余全部有可用默认值。'
      : 'Names use the double-underscore form: <code>FERROMA__API__PORT</code> is <code>[api] port</code> in <code>ferroma.toml</code>. Everything not listed here already has a working default.'}</p>` +
    table(
      zh ? ['变量', '必填', '作用'] : ['Variable', 'Required', 'What it does'],
      zh ? [
        ['<code>DATABASE_URL</code>', '是', 'PostgreSQL 连接串。路径 A/C 必填；路径 B 由 <code>deploy.sh</code> 写入 <code>.env</code>'],
        ['<code>POSTGRES_PASSWORD</code>', '路径 A', '只有 <code>docker-compose.yml</code> 读它，用来起数据库容器'],
        ['<code>FERROMA_VERSION</code>', '路径 A', '要拉取的发布 tag（如 <code>' + VERSION + '</code>）。可复现部署请钉死版本，不要用 <code>latest</code>'],
        ['<code>FERROMA_DATA_DIR</code>', '否', 'Maildir、附件与 DKIM 私钥的位置。容器内默认 <code>/var/lib/ferroma</code>'],
        ['<code>FERROMA_JWT_SECRET</code>', '否', '不设则进程重启会让所有会话失效；镜像会在数据卷里生成一次并复用'],
        ['<code>FERROMA_TLS_ENABLED</code> / <code>FERROMA_TLS_CERT</code> / <code>FERROMA_TLS_KEY</code>', '否', '由本进程终止 SMTP/IMAP 的 TLS。证书文件不存在时会明确拒绝启动，而不是起完再失败'],
        ['<code>FERROMA__SMTP__SMTPS_PORT</code> / <code>FERROMA__IMAP__IMAPS_PORT</code>', '否', '隐式 TLS 监听（465/993）。默认 <code>0</code>，不打开则发布出去的端口没人监听'],
        ['<code>FERROMA__API__SECURE_COOKIES</code>', '否', '反向代理上 HTTPS 之后设为 <code>true</code>；明文 HTTP 下开着会让第一次登录失败'],
        ['<code>FERROMA__API__TRUST_PROXY_HEADERS</code>', '否', '在反向代理后面时设为 <code>true</code>，否则日志与限流拿到的是代理的地址'],
        ['<code>FERROMA__QUEUE__RELAY_HOST</code> 及同组的 <code>_PORT</code> / <code>_TLS</code> / <code>_USERNAME</code> / <code>_PASSWORD</code> / <code>_FROM_DOMAINS</code>', '否', '出站中继（smarthost）。服务商不给设 PTR 时用它，见第 4 节'],
        ['<code>FERROMA_LOG_LEVEL</code> / <code>FERROMA_LOG_FORMAT</code>', '否', '默认 <code>info</code> / <code>text</code>；容器里通常用 <code>json</code>'],
      ] : [
        ['<code>DATABASE_URL</code>', 'yes', 'PostgreSQL connection string. Required on paths A and C; path B writes it into <code>.env</code> for you'],
        ['<code>POSTGRES_PASSWORD</code>', 'path A', 'read only by <code>docker-compose.yml</code>, to start the database container'],
        ['<code>FERROMA_VERSION</code>', 'path A', 'the release tag to pull (e.g. <code>' + VERSION + '</code>). Pin the exact release for a reproducible deployment; never <code>latest</code>'],
        ['<code>FERROMA_DATA_DIR</code>', 'no', 'where the Maildir, attachments and the DKIM private key live. <code>/var/lib/ferroma</code> in the image'],
        ['<code>FERROMA_JWT_SECRET</code>', 'no', 'unset, every session is invalidated when the process restarts; the image generates one into the data volume and reuses it'],
        ['<code>FERROMA_TLS_ENABLED</code> / <code>FERROMA_TLS_CERT</code> / <code>FERROMA_TLS_KEY</code>', 'no', 'SMTP/IMAP TLS terminated by this process. Enabling it before the files exist is refused with an explanation rather than applied and failing later'],
        ['<code>FERROMA__SMTP__SMTPS_PORT</code> / <code>FERROMA__IMAP__IMAPS_PORT</code>', 'no', 'the implicit-TLS listeners (465/993). Default <code>0</code>: without them the published ports map to nothing'],
        ['<code>FERROMA__API__SECURE_COOKIES</code>', 'no', 'set <code>true</code> once a reverse proxy serves HTTPS; over plain HTTP it makes the first sign-in fail'],
        ['<code>FERROMA__API__TRUST_PROXY_HEADERS</code>', 'no', 'set <code>true</code> behind a reverse proxy, or the logs and the login throttle see the proxy'],
        ['<code>FERROMA__QUEUE__RELAY_HOST</code> and the rest of its group: <code>_PORT</code> / <code>_TLS</code> / <code>_USERNAME</code> / <code>_PASSWORD</code> / <code>_FROM_DOMAINS</code>', 'no', 'the outbound relay (smarthost). Use it when your provider will not set a PTR — see section 4'],
        ['<code>FERROMA_LOG_LEVEL</code> / <code>FERROMA_LOG_FORMAT</code>', 'no', '<code>info</code> / <code>text</code> by default; containers usually want <code>json</code>'],
      ]) });

  S.push({ id: 'first-run', title: zh ? '首次启动与设置向导' : 'First boot and the setup wizard', html:
    `<p>${zh
      ? '服务器第一次启动时，配置里还缺邮件域、主机名、公网地址和 API 监听 —— 这些由引导界面收集，存进数据库，服务下次启动时采用。这就是生产 compose 文件里没有 <code>FERROMA_HOSTNAME</code> 的原因：留着不设，向导的答案才是权威。'
      : 'On the very first boot the configuration still lacks the mail domain, the hostname, the public URL and the API listener. The wizard collects them, stores them in the database, and the server adopts them on its next start. That is why the production compose file sets no <code>FERROMA_HOSTNAME</code>: leaving it unset is what makes the wizard\'s answers authoritative.'}</p>` +
    `<ol class="deploy-steps">
      <li><strong>${zh ? '打开引导界面' : 'Open the wizard'}</strong><p>${zh
        ? '设置完成之前它就在根路径：<code>http://&lt;host&gt;:8080</code>。控制台本身始终在 <code>/admin/</code>。数据库不可达时服务不会退出，而是留在同一个控制台上，让你先把数据库接上。'
        : 'Before setup finishes it is the root path: <code>http://&lt;host&gt;:8080</code>. The console itself is always at <code>/admin/</code>. A server that cannot reach PostgreSQL does not exit; it stays on that console and lets you fix the database first.'}</p></li>
      <li><strong>${zh ? '填写四件事' : 'Answer four things'}</strong><p>${zh
        ? '邮件域、MX 主机名、公网地址（<code>https://mail.example.com</code>）、API 监听地址与端口。'
        : 'The mail domain, the MX hostname, the public URL (<code>https://mail.example.com</code>), and the API bind address and port.'}</p></li>
      <li><strong>${zh ? '建域与管理员' : 'Create the domain and the administrator'}</strong><p>${zh
        ? '向导直接建好；也可以在容器里用命令行做同样的事：'
        : 'The wizard does it for you; the same thing from the shell inside the container:'}</p>` +
      codeBlock([
        'docker exec ferroma ferroma domain create example.com',
        'docker exec ferroma ferroma user create you@example.com --admin',
      ].join('\n'), 'bash') + `</li>
      <li><strong>${zh ? '生成 DKIM 密钥并发布 TXT' : 'Generate a DKIM key and publish the TXT record'}</strong><p>${zh
        ? '签发之前，先把公钥发到 DNS 上。'
        : 'Publish the public key in DNS before you turn signing on.'}</p>` +
      codeBlock([
        'docker exec ferroma ferroma dkim generate --domain example.com',
        'docker exec ferroma ferroma dkim show --domain example.com',
      ].join('\n'), 'bash') + `</li>
      <li><strong>${zh ? '端到端自检' : 'An end-to-end check'}</strong><p>${zh
        ? '从外网往本域地址发一封信，再确认它落进了 Maildir。'
        : 'Send a message from the outside to a local address, then confirm it landed.'}</p></li>
    </ol>` });

  S.push({ id: 'operate', title: zh ? '日常命令' : 'Everyday commands', html:
    codeBlock(CMD_OPS[lang], 'bash') +
    `<p>${zh
      ? 'Compose 部署则把 <code>docker</code> 换成 <code>docker compose -f docker-compose.yml</code>：'
      : 'On a Compose deployment, replace <code>docker</code> with <code>docker compose -f docker-compose.yml</code>:'}</p>` +
    codeBlock([
      'docker compose -f docker-compose.yml logs -f --tail=200 ferroma',
      'docker compose -f docker-compose.yml restart ferroma',
      'docker compose -f docker-compose.yml exec ferroma sh',
      `docker compose -f docker-compose.yml down        # ${zh ? '保留数据卷' : 'keeps the volumes'}`,
      `# docker compose -f docker-compose.yml down -v   # ${zh ? '连同所有邮件与用户一起删除' : 'destroys all mail and users'}`,
    ].join('\n'), 'bash') });

  S.push({ id: 'upgrade', title: zh ? '升级与回滚' : 'Upgrades and rollback', html:
    `<p>${zh
      ? '升级就是换一个 tag 再重建容器；迁移在启动时自动执行。回滚就是把 tag 换回去。数据卷与数据库都不动。'
      : 'An upgrade is a new tag and a recreated container; migrations run at startup. A rollback is the old tag. Neither touches the volumes or the database.'}</p>` +
    codeBlock([
      '# .env',
      `FERROMA_VERSION=${VERSION}`,
      '',
      'docker compose -f docker-compose.yml pull',
      'docker compose -f docker-compose.yml up -d',
      '',
      `# ${zh ? '回滚：把上一个 tag 写回去，再重复一次' : 'rollback: put the previous tag back and repeat'}`,
      'docker compose -f docker-compose.yml up -d',
    ].join('\n'), 'bash') +
    `<div class="note note-warn"><span class="note-ico">!</span><div><p>${
      zh ? '迁移只向前。回滚镜像之前先看 <code>CHANGELOG.md</code> 里那一版是否带 schema 变更，并先做一次数据库转储。'
         : 'Migrations only move forward. Before rolling an image back, check <code>CHANGELOG.md</code> for a schema change in that release, and take a database dump first.'
    }</p></div></div>` });

  S.push({ id: 'backup', title: zh ? '备份' : 'Backups', html:
    `<p>${zh
      ? '部署里没有任何备份组件，这是有意的：用主机自己的工具去做。要备份的是<b>两半</b>，而且必须成对恢复 —— 只恢复数据库会丢邮件，只恢复 Maildir 会丢元数据。'
      : 'Nothing in the deployment backs anything up, on purpose: use the host\'s own tooling. There are <b>two halves</b> and they must be restored together — a database restored without its Maildir loses messages, and vice versa.'}</p>` +
    table(
      zh ? ['要备份的东西', '在哪里', '怎么备'] : ['What', 'Where', 'How'],
      zh ? [
        ['关系数据', 'PostgreSQL 的 <code>ferroma</code> 库（路径 A 是 <code>ferroma-postgres-data</code> 卷）', '<code>pg_dump -Fc</code>，或数据库层的快照'],
        ['邮件与附件', '卷 <code>ferroma-data</code>：<code>mail/</code>、<code>attachments/</code>', '卷级快照，或 <code>restic</code> / <code>borg</code> / <code>rsync</code>'],
        ['DKIM 私钥', '同一个卷的 <code>dkim/</code> 下', '同上。丢了就要重新签发并重新发布 TXT'],
        ['证书', '<code>./tls</code>', '可重新签发，不必备份'],
      ] : [
        ['Relational state', 'the <code>ferroma</code> database (the <code>ferroma-postgres-data</code> volume on path A)', '<code>pg_dump -Fc</code>, or a storage-level snapshot'],
        ['Mail and attachments', 'the <code>ferroma-data</code> volume: <code>mail/</code>, <code>attachments/</code>', 'a volume-level snapshot, or <code>restic</code> / <code>borg</code> / <code>rsync</code>'],
        ['The DKIM private key', '<code>dkim/</code> in the same volume', 'the same. Lose it and you re-issue and re-publish the TXT record'],
        ['Certificates', '<code>./tls</code>', 're-issuable; no need to back up'],
      ]) +
    `<p>${zh ? `完整命令与恢复顺序见 <a href="${doc}/deployment.html#${A.backup}">部署参考 §8</a>。` : `The commands and the restore order are in <a href="${doc}/deployment.html#${A.backup}">Deployment reference §8</a>.`}</p>` });

  S.push({ id: 'trouble', title: zh ? '排障速查' : 'Troubleshooting', html:
    table(
      zh ? ['症状', '先跑这个'] : ['Symptom', 'Run this first'],
      zh ? [
        ['收不到任何外部邮件', '<code>dig +short MX example.com</code>；确认 25 入站没有被厂商封禁'],
        ['发出的邮件进垃圾箱 / 被拒', '<code>dig +short TXT example.com</code> 与 <code>dig +short TXT default._domainkey.example.com</code>；确认 PTR 与 MX 主机名一致'],
        ['服务商不给设 PTR', '别硬发：让出站走中继，见上面「服务商不给 PTR」一节。SPF 记得 <code>include</code> 中继服务商的域'],
        ['邮件客户端连不上 587', '容器里 <code>ferroma config show</code>；STARTTLS 需要证书，没有证书就只能明文'],
        ['465/993 连不上', '隐式 TLS 监听默认关闭。设 <code>FERROMA__SMTP__SMTPS_PORT=465</code> 与 <code>FERROMA__IMAP__IMAPS_PORT=993</code>'],
        ['登录后立刻掉线', '反向代理已上 HTTPS 时设 <code>FERROMA__API__SECURE_COOKIES=true</code>'],
        ['容器起来但 <code>/</code> 是 404', '检查前端目录的环境变量是否被覆盖（镜像里已指向 <code>/usr/share/ferroma</code>）'],
        ['一切都慢', '<code>ferroma config check --dns-domain example.com</code>：解析器不响应会按超时全额计费，且在 SMTP 应答之前'],
      ] : [
        ['No inbound mail at all', '<code>dig +short MX example.com</code>; confirm inbound 25 is not blocked by the provider'],
        ['Outbound mail lands in spam / is rejected', '<code>dig +short TXT example.com</code> and <code>dig +short TXT default._domainkey.example.com</code>; confirm the PTR matches the MX hostname'],
        ['Your provider will not set a PTR', 'do not deliver directly — relay the outbound path, see "No PTR from your provider" above. The SPF record has to <code>include</code> the relay provider\'s domain'],
        ['A mail client cannot connect on 587', '<code>ferroma config show</code> inside the container; STARTTLS needs a certificate, and without one there is only plaintext'],
        ['465 / 993 refuse the connection', 'the implicit-TLS listeners are off by default. Set <code>FERROMA__SMTP__SMTPS_PORT=465</code> and <code>FERROMA__IMAP__IMAPS_PORT=993</code>'],
        ['Sessions drop right after sign-in', 'set <code>FERROMA__API__SECURE_COOKIES=true</code> once the reverse proxy serves HTTPS'],
        ['The container is up but <code>/</code> is a 404', 'check that the front-end directory variables were not overridden (the image points them at <code>/usr/share/ferroma</code>)'],
        ['Everything is slow', '<code>ferroma config check --dns-domain example.com</code>: a resolver that does not answer is paid for in full timeout, before the SMTP reply'],
      ]) +
    codeBlock(CMD_HEALTH, 'bash') });

  S.push({ id: 'more', title: zh ? '深水区在哪里' : 'Where the detail lives', html:
    `<p>${zh ? '这一页只负责把服务跑起来。下面这些是同一件事的完整版本：' : 'This page stops at a running server. The complete version of the same material:'}</p>` +
    `<ul>
      <li><a href="${doc}/deployment.html#${A.dns}">${zh ? '部署参考 §2 — DNS 记录' : 'Deployment reference §2 — DNS records'}</a>${zh ? '：MX、A、PTR、SPF、MTA-STS、DMARC 的完整表格与 BIND 片段。' : ': the full record table, a BIND-style zone snippet and how to verify the zone.'}</li>
      <li><a href="${doc}/deployment.html#${A.tls}">${zh ? '部署参考 §5 — 端口与 TLS' : 'Deployment reference §5 — ports and TLS'}</a>${zh ? '：端口表、两种 TLS 终止方式、nginx 配置与 Let\'s Encrypt。' : ': the port table, both TLS arrangements, the nginx server block, and Let\'s Encrypt.'}</li>
      <li><a href="${doc}/deployment.html#${A.backup}">${zh ? '部署参考 §8 — 备份与恢复' : 'Deployment reference §8 — backup and restore'}</a>${zh ? '：命令，以及一份备份必须包含什么。' : ': the commands, and what a backup must contain.'}</li>
      <li><a href="${doc}/security.html">${zh ? '安全' : 'Security'}</a>${zh ? '：威胁模型与每一项控制。' : ': the threat model and every control.'}</li>
      <li><a href="${REPO_URL}/blob/main/CHANGELOG.md" target="_blank" rel="noopener">CHANGELOG</a>${zh ? '：升级之前先读它。' : ': read it before an upgrade.'}</li>
    </ul>` });

  return S;
}

// 把内置页面的一段 HTML 拆成搜索用的两份文本：正文，以及代码块里的内容。
// 代码块必须能搜到——`FERROMA__QUEUE__RELAY_HOST` 这样的标识符只出现在代码里，
// 而"搜一个环境变量名却什么也搜不到"正是这套索引最初的样子。它单独成一份而不是
// 混进正文，是为了让只命中代码的结果排在后面，而不是和正文命中平起平坐。
function indexText(html) {
  const plain = (s) => s
    .replace(/<[^>]+>/g, ' ')
    .replace(/&lt;/g, '<').replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&amp;/g, '&')
    .replace(/\s+/g, ' ')
    .trim();
  const blocks = [...html.matchAll(/<figure class="codeblock"[\s\S]*?<\/figure>/g)].map(m => m[0]);
  let body = html;
  for (const b of blocks) body = body.replace(b, ' ');
  return { body: plain(body), code: plain(blocks.join(' ')).slice(0, 1200) };
}

function buildDeployDoc(lang) {
  const meta = DEPLOY_META[lang];
  const sections = deploySections(lang);
  const lede = lang === 'zh'
    ? '三种受支持的部署形态，选一种照着命令走完，再走一遍设置向导。下面每一条命令都对应仓库里的真实产物：<code>Dockerfile</code>、三个 <code>docker-compose*.yml</code>、<code>.env.example</code> 与 <code>scripts/deploy.sh</code>。'
    : 'Three supported shapes. Pick the one that matches the host you have, run the commands, then walk the setup wizard. Every command below corresponds to a real artefact in the repository: <code>Dockerfile</code>, the three <code>docker-compose*.yml</code> files, <code>.env.example</code> and <code>scripts/deploy.sh</code>.';
  // 编号在这里生成，不写进标题：插一节不用回头改后面所有节。
  const numbered = sections.map((s, i) => ({ ...s, text: `${i + 1}. ${s.title}` }));
  const body = `<h1>${meta.title}</h1>\n<p>${lede}</p>\n` +
    numbered.map(s => `<h2 id="${s.id}">${s.text}</h2>\n${s.html}`).join('\n');
  const toc = numbered.map(s => ({ level: 2, id: s.id, text: s.text }));
  return {
    slug: 'deploy', lang, title: meta.title, desc: meta.desc,
    kicker: `${GROUPS.operations[lang]} · ${lang === 'zh' ? '文档' : 'Documentation'}`,
    body, toc,
    // 每一节都要进搜索索引，否则这个页面上只有标题能被搜到；代码块另存一份，
    // 否则变量名和命令搜不到。
    segments: [
      { id: '', title: meta.title, text: `${meta.desc} ${indexText(lede).body}`, code: '' },
      ...numbered.map(s => {
        const t = indexText(s.html);
        return { id: s.id, title: s.text, text: t.body, code: t.code };
      }),
    ],
  };
}

// ---------------------------------------------------------------------------
// 主流程
// ---------------------------------------------------------------------------
rmSync(OUT, { recursive: true, force: true });
for (const d of ['', 'assets', 'en', 'zh']) mkdirSync(join(OUT, d), { recursive: true });

// 先渲染、后写文件：资源指纹要在 HTML 之前算出来。
const rendered = [];
const pending = [];
for (const lang of LANGS) {
  for (const page of PAGES) {
    if (lang === 'en' && page.zhOnly) continue;
    const doc = renderDoc(lang, page);
    if (!doc) { console.error(`missing source: ${docPath(lang, page.slug)}`); process.exit(1); }
    rendered.push(doc);
    pending.push({ lang, page, doc });
  }
}

// 搜索索引（两份：按语言）→ search.js
function searchEntry(doc) {
  return {
    u: `${doc.lang}/${doc.slug}.html`,
    t: doc.title,
    s: doc.segments.map(seg => ({
      id: seg.id,
      h: seg.title,
      x: seg.text.replace(/\s+/g, ' ').slice(0, 2400),
      // 代码块文本单独一份：命中它的结果排在有正文命中的结果之后。
      c: (seg.code || '').replace(/\s+/g, ' ').trim().slice(0, 1200),
    })),
  };
}
const idx = {
  en: rendered.filter(d => d.lang === 'en').map(searchEntry),
  zh: rendered.filter(d => d.lang === 'zh').map(searchEntry),
};

// 资源指纹。assets 会被 CDN 按扩展名缓存若干小时（这台服务器上是 4 小时），
// 于是"改了内容但 URL 没变"就等于"读者几个小时内看到的还是旧的"——一次搜索框
// 的交互修复就是这样推上去却看不见的。内容哈希挂在查询串上：URL 一变，CDN 与
// 浏览器都会当成新资源去取。
const hash = (text) => createHash('sha256').update(text).digest('hex').slice(0, 10);
const styleCss = readFileSync(join(ROOT, 'tools', 'site-src', 'style.css'), 'utf8');
const siteJs = readFileSync(join(ROOT, 'tools', 'site-src', 'site.js'), 'utf8');
const searchJs = 'window.FERROMA_SEARCH=' + JSON.stringify(idx) + ';';
ASSETS = {
  style: `assets/style.css?v=${hash(styleCss)}`,
  site: `assets/site.js?v=${hash(siteJs)}`,
  search: `assets/search.js?v=${hash(searchJs)}`,
};

for (const { lang, page, doc } of pending) {
  const html = layout({ lang, slug: page.slug, title: doc.title, desc: doc.desc, kicker: doc.kicker, content: doc.body, toc: doc.toc, root: '../' });
  writeFileSync(join(OUT, lang, page.slug + '.html'), html);
}

writeFileSync(join(OUT, 'assets', 'search.js'), searchJs);
writeFileSync(join(OUT, 'assets', 'style.css'), styleCss);
writeFileSync(join(OUT, 'assets', 'site.js'), siteJs);
// 两份首页：`/` 是英文，`/zh/` 是中文。语言切换与 brand 都按语言指向对应的一份。
writeFileSync(join(OUT, 'index.html'), buildIndexHtml('en'));
writeFileSync(join(OUT, 'zh', 'index.html'), buildIndexHtml('zh'));

console.log(`site built: ${rendered.length} doc pages + 2 indexes → ${OUT}`);