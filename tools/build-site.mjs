#!/usr/bin/env node
// build-site.mjs — 把 docs/ 下的中英文档构建为静态网站，输出到 .site/
// 零依赖，Node >= 18。运行：node tools/build-site.mjs
import { readFileSync, writeFileSync, mkdirSync, rmSync, readdirSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = dirname(dirname(fileURLToPath(import.meta.url)));
const DOCS = join(ROOT, 'docs');
const OUT = join(ROOT, '.site');

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
  { slug: 'deployment', group: 'operations',
    en: { title: 'Deployment', desc: 'DNS, TLS, backups, upgrades, troubleshooting.' },
    zh: { title: '部署', desc: 'DNS、TLS、备份、升级与故障排查。' } },
  { slug: 'glossary', group: 'overview',
    en: { title: 'Glossary', desc: 'The canonical terminology of the English documents.' },
    zh: { title: '术语表', desc: 'Ferroma 中英术语对照。' } },
];

const GROUPS = {
  overview:   { en: 'Overview',   zh: '总览' },
  protocols:  { en: 'Protocols',  zh: '协议' },
  internals:  { en: 'Internals',  zh: '内部实现' },
  operations: { en: 'Operations', zh: '运维' },
};

const PAGE_ORDER = PAGES.map(p => p.slug);
const LANGS = ['en', 'zh'];

// 术语表是唯一一个文件名全大写的文档：两个语言各有一份，而不是翻译对。
function docPath(lang, slug) {
  if (slug === 'glossary') return join(DOCS, lang === 'en' ? 'GLOSSARY.md' : 'zh/GLOSSARY.md');
  return lang === 'en' ? join(DOCS, slug + '.md') : join(DOCS, 'zh', slug + '.md');
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

function rewriteHref(href) {
  // 已知文档互链 → 当前语言页面；仓库内其他文件 → 降级为纯文本
  const m = href.match(/^(?:(?:\.\.\/)+|\.\/)?(?:zh\/)?([\w-]+)\.md$/);
  if (m && PAGE_ORDER.includes(m[1])) return m[1] + '.html';
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
      const hi = highlight(buf.join('\n'), lang);
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
  const file = docPath(lang, page.slug);
  if (!existsSync(file)) return null;
  const src = readFileSync(file, 'utf8');
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

function navGroupHtml(lang, activeSlug, root) {
  let h = '';
  for (const g of Object.keys(GROUPS)) {
    const pages = PAGES.filter(p => p.group === g && !(lang === 'en' && p.zhOnly));
    if (!pages.length) continue;
    h += `<div class="nav-group"><div class="nav-group-title">${GROUPS[g][lang]}</div>`;
    for (const p of pages) {
      const active = p.slug === activeSlug ? ' active' : '';
      h += `<a class="nav-link${active}" href="${root}${lang}/${p.slug}.html">${p[lang].title}</a>`;
    }
    h += '</div>';
  }
  return h;
}

function altLangUrl(lang, slug, root) {
  if (slug === 'home') return `${root}en/architecture.html`;
  const page = PAGES.find(p => p.slug === slug);
  // 仅中文存在的页面（如术语表）切英文时落到英文文档首篇
  if (lang === 'zh' && page && page.zhOnly) return `${root}en/architecture.html`;
  return `${root}${lang === 'en' ? 'zh' : 'en'}/${slug}.html`;
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
<link rel="stylesheet" href="${root}assets/style.css">
${themeBootstrap()}
</head>
<body>
<header class="topbar">
  <button class="iconbtn menu-btn" id="menuBtn" aria-label="menu">☰</button>
  <a class="brand" href="${root}index.html"><svg class="brand-env" viewBox="0 0 32 32" aria-hidden="true"><rect x="3" y="6" width="26" height="20" rx="3"/><path d="M4 8l12 9L28 8"/></svg><span>Ferroma</span></a>
  <div class="search" id="search">
    <svg class="search-icon" viewBox="0 0 20 20"><circle cx="9" cy="9" r="6"/><path d="M13.5 13.5L18 18"/></svg>
    <input id="searchInput" type="search" placeholder="${ui.searchPlaceholder}" autocomplete="off" spellcheck="false">
    <div class="search-results" id="searchResults" hidden></div>
  </div>
  <nav class="top-actions">
    <a class="lang-switch" href="${altLangUrl(lang, slug, root)}" title="${ui.switchLang}"><span class="${lang === 'en' ? 'on' : ''}">EN</span><span class="sep">/</span><span class="${lang === 'zh' ? 'on' : ''}">中文</span></a>
    <button class="iconbtn theme-btn" id="themeBtn" aria-label="theme">
      <svg class="i-sun" viewBox="0 0 20 20"><circle cx="10" cy="10" r="4"/><path d="M10 1v2M10 17v2M1 10h2M17 10h2M3.5 3.5l1.5 1.5M15 15l1.5 1.5M16.5 3.5L15 5M5 15l-1.5 1.5"/></svg>
      <svg class="i-moon" viewBox="0 0 20 20"><path d="M16 12.5A7 7 0 0 1 7.5 4a7 7 0 1 0 8.5 8.5z"/></svg>
    </button>
  </nav>
</header>
<div class="shell">
  <aside class="sidebar" id="sidebar">
    <div class="sidebar-inner">
      <a class="nav-link nav-home${slug === 'home' ? ' active' : ''}" href="${root}index.html">${ui.home}</a>
      ${navGroupHtml(lang, slug, root)}
      <div class="sidebar-foot">AGPL-3.0</div>
    </div>
  </aside>
  <div class="scrim" id="scrim"></div>
  <main class="content">
    ${kicker ? `<div class="kicker">${kicker}</div>` : ''}
    ${content}
    <nav class="pager">
      ${prev ? `<a class="pager-prev" href="${root}${lang}/${prev.slug}.html"><span>←</span><span><small>${ui.previous}</small><b>${prev[lang].title}</b></span></a>` : '<span></span>'}
      ${next ? `<a class="pager-next" href="${root}${lang}/${next.slug}.html"><span><small>${ui.next}</small><b>${next[lang].title}</b></span><span>→</span></a>` : '<span></span>'}
    </nav>
    <footer class="footer">Ferroma · AGPL-3.0 · <span>${ui.builtFrom}</span></footer>
  </main>
  ${tocHtml ? `<aside class="tocpane">${tocHtml}</aside>` : ''}
</div>
<script src="${root}assets/search.js"><\/script>
<script src="${root}assets/site.js"><\/script>
</body>
</html>`;
}

const UI = {
  en: {
    searchPlaceholder: 'Search docs…  ( / )', onThisPage: 'On this page', home: 'Home',
    switchLang: 'Switch to Chinese', previous: 'Previous', next: 'Next',
    builtFrom: 'built from docs/', noResults: 'No results', searching: 'Type to search…',
  },
  zh: {
    searchPlaceholder: '搜索文档…（按 / ）', onThisPage: '本页目录', home: '首页',
    switchLang: 'Switch to English', previous: '上一篇', next: '下一篇',
    builtFrom: '由 docs/ 生成', noResults: '没有匹配结果', searching: '输入以搜索…',
  },
};

// ---------------------------------------------------------------------------
// 首页
// ---------------------------------------------------------------------------
const ARCH_ASCII = `                              FERROMA
                                 │
      ┌──────────────────────────┼──────────────────────────┐
      │                          │                          │
   Webmail                Official Clients               Admin
      │                          │                          │
      │                    ┌─────┼─────┐                    │
      │                    ▼     ▼     ▼                    │
      │                  Win   Linux  macOS                 │
      │                                                      │
      └──────────────────────────┼──────────────────────────┘
                                 │
                        Client API (FCP) / HTTP API
                                 │
                         ┌────────▼────────┐
                         │  Ferroma Core   │
                         └────────┬────────┘
                                  │
       ┌───────────┬─────────────┼─────────────┬───────────┐
       ▼           ▼             ▼             ▼           ▼
     SMTP        IMAP         Storage        Queue        DNS
       │           │             │             │           │
       └───────────┴─────────────┼─────────────┴───────────┘
                                 │
                            Event Bus ── WebSocket ── Push
                                 │
                             PostgreSQL`;

function buildIndexHtml(root) {
  const quickstart = [
    'git clone https://github.com/ferroma/ferroma && cd ferroma',
    'cp .env.example .env    # POSTGRES_PASSWORD / FERROMA_HOSTNAME / FERROMA_JWT_SECRET',
    'docker compose up -d',
    'docker compose logs -f ferroma',
  ].join('\n');
  const feats = [
    ['One mail core', '单一邮件核心', 'SMTP, IMAP, Webmail and the Client API all go through ferroma-mail — one implementation of every operation.', 'SMTP、IMAP、Webmail 与客户端 API 全部经由 ferroma-mail——每个操作只有一份实现。'],
    ['Never an open relay', '永不开式中继', 'Unauthenticated peers deliver only to local domains, enforced at the protocol edge.', '未认证的对端只能投递到本地域，这条规则在协议边缘强制执行。'],
    ['Server = source of truth', '服务器即真相', 'Clients keep a cache and a cursor; the change log and its sequence numbers keep the truth.', '客户端只保留缓存与游标；变更日志及其序列号保存真相。'],
    ['Nothing is lost', '数据不丢失', 'Client operations carry an idempotency key and survive a crash mid-request.', '客户端操作携带幂等键，请求中途崩溃也能安全重放。'],
    ['rustls everywhere', '全线 rustls', 'One TLS implementation, in Rust. No OpenSSL, no schannel.', '只使用一种 Rust 实现的 TLS。不用 OpenSSL，也不用 schannel。'],
    ['Realtime event bus', '实时事件总线', 'mail.received and friends replay for reconnecting clients over WebSocket.', 'mail.received 等事件可对重连客户端重放，经 WebSocket 推送。'],
  ];
  const cards = PAGES.filter(p => p.slug !== 'glossary').map(p =>
    `<a class="doc-card" href="${root}en/${p.slug}.html"><span class="doc-card-en">${p.en.title}</span><span class="doc-card-zh">${p.zh.title}</span><span class="doc-card-desc">${escapeHtml(p.zh.desc)}</span><span class="doc-card-glyph">→</span></a>`).join('');

  return `<!doctype html>
<html lang="zh-CN" data-theme="dark">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Ferroma · A Rust-native, self-hosted mail platform · Rust 原生自托管邮件平台</title>
<meta name="description" content="Ferroma 文档站 — 双语文档：架构、SMTP、IMAP、存储、API、FCP、同步、安全与部署。">
<link rel="icon" href="${favicon}">
<link rel="stylesheet" href="${root}assets/style.css">
${themeBootstrap()}
</head>
<body class="home">
<header class="topbar">
  <span class="menu-btn-ghost"></span>
  <a class="brand" href="index.html"><svg class="brand-env" viewBox="0 0 32 32" aria-hidden="true"><rect x="3" y="6" width="26" height="20" rx="3"/><path d="M4 8l12 9L28 8"/></svg><span>Ferroma</span></a>
  <span class="topbar-tag">docs</span>
  <nav class="top-actions">
    <a class="lang-switch" href="en/architecture.html"><span>EN</span><span class="sep">/</span><span class="on">中文</span></a>
    <button class="iconbtn theme-btn" id="themeBtn" aria-label="theme">
      <svg class="i-sun" viewBox="0 0 20 20"><circle cx="10" cy="10" r="4"/><path d="M10 1v2M10 17v2M1 10h2M17 10h2M3.5 3.5l1.5 1.5M15 15l1.5 1.5M16.5 3.5L15 5M5 15l-1.5 1.5"/></svg>
      <svg class="i-moon" viewBox="0 0 20 20"><path d="M16 12.5A7 7 0 0 1 7.5 4a7 7 0 1 0 8.5 8.5z"/></svg>
    </button>
  </nav>
</header>

<section class="hero">
  <div class="hero-inner">
    <div class="hero-copy">
      <div class="hero-badges"><span>Rust-native</span><span>Self-hosted</span><span>AGPL-3.0</span></div>
      <h1>Ferroma</h1>
      <p class="hero-line-en">A Rust-native, self-hosted mail platform — its own SMTP and IMAP servers, its own MIME core, its own storage engine. It does not wrap Postfix or Dovecot. The point is to own the whole stack.</p>
      <p class="hero-line-zh">一个 Rust 原生、可自托管的邮件平台——自研 SMTP 与 IMAP 服务器、自研 MIME 邮件核心与存储引擎，不封装 Postfix、Dovecot 等任何现有邮件服务器，目标是掌握全部技术栈。</p>
      <div class="hero-cta">
        <a class="btn btn-primary" href="en/architecture.html">Read the docs · 阅读文档</a>
        <a class="btn" href="#quickstart">Quick start · 快速开始</a>
      </div>
    </div>
    <figure class="hero-ascii" aria-label="architecture diagram"><pre>${escapeHtml(ARCH_ASCII)}</pre></figure>
  </div>
</section>

<section class="section">
  <h2 class="section-title">What it is · 它是什么</h2>
  <div class="feature-grid">
    ${feats.map(f => `<div class="feature"><h3>${f[0]}</h3><div class="feature-zh">${f[1]}</div><p>${f[2]}</p><p class="feat-zh">${f[3]}</p></div>`).join('\n    ')}
  </div>
</section>

<section class="section" id="docs">
  <h2 class="section-title">Documentation · 文档</h2>
  <div class="doc-grid">
    ${cards}
    <a class="doc-card" href="zh/glossary.html"><span class="doc-card-en">Glossary</span><span class="doc-card-zh">术语表</span><span class="doc-card-desc">Ferroma 中英术语对照。</span><span class="doc-card-glyph">→</span></a>
  </div>
</section>

<section class="section" id="quickstart">
  <h2 class="section-title">Quick start · 快速开始</h2>
  <figure class="codeblock hero-code" data-lang="bash"><pre><code>${highlight(quickstart, 'bash')}</code></pre><button class="copy-btn" type="button" data-copy aria-label="copy code">⧉</button></figure>
  <p class="section-note">完整部署说明（DNS、TLS、DKIM、备份与升级）见 <a href="en/deployment.html">Deployment</a> 与 <a href="zh/deployment.html">中文版部署文档</a>。</p>
</section>

<footer class="footer footer-home">Ferroma · AGPL-3.0 · <span>built from docs/ · 由 docs/ 生成</span></footer>
<script src="assets/site.js"><\/script>
</body>
</html>`;
}

// ---------------------------------------------------------------------------
// 主流程
// ---------------------------------------------------------------------------
rmSync(OUT, { recursive: true, force: true });
for (const d of ['', 'assets', 'en', 'zh']) mkdirSync(join(OUT, d), { recursive: true });

const rendered = [];
for (const lang of LANGS) {
  for (const page of PAGES) {
    if (lang === 'en' && page.zhOnly) continue;
    const doc = renderDoc(lang, page);
    if (!doc) { console.error(`missing source: ${docPath(lang, page.slug)}`); process.exit(1); }
    rendered.push(doc);
    const html = layout({ lang, slug: page.slug, title: doc.title, desc: doc.desc, kicker: doc.kicker, content: doc.body, toc: doc.toc, root: '../' });
    writeFileSync(join(OUT, lang, page.slug + '.html'), html);
  }
}

// 搜索索引（两份：按语言）→ search.js
function searchEntry(doc) {
  return {
    u: `${doc.lang}/${doc.slug}.html`,
    t: doc.title,
    s: doc.segments.map(seg => ({ id: seg.id, h: seg.title, x: seg.text.replace(/\s+/g, ' ').slice(0, 2400) })),
  };
}
const idx = {
  en: rendered.filter(d => d.lang === 'en').map(searchEntry),
  zh: rendered.filter(d => d.lang === 'zh').map(searchEntry),
};
writeFileSync(join(OUT, 'assets', 'search.js'), 'window.FERROMA_SEARCH=' + JSON.stringify(idx) + ';');

// 样式与脚本
copyStatic('style.css');
copyStatic('site.js');
writeFileSync(join(OUT, 'index.html'), buildIndexHtml(''));

console.log(`site built: ${rendered.length} doc pages + index → ${OUT}`);

function copyStatic(name) {
  writeFileSync(join(OUT, 'assets', name), readFileSync(join(ROOT, 'tools', 'site-src', name), 'utf8'));
}