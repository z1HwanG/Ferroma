#!/usr/bin/env node
// Build Ferroma documentation static website
// Usage: node scripts/build-static-site.mjs

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(__dirname, '..');
const OUT = path.resolve(ROOT, '.site');

// ── Doc list ─────────────────────────────────────
const DOCS = [
  { file: 'docs/architecture.md', title: 'Architecture', lang: 'en' },
  { file: 'docs/api.md', title: 'HTTP API', lang: 'en' },
  { file: 'docs/fcp.md', title: 'Client Protocol (FCP)', lang: 'en' },
  { file: 'docs/smtp.md', title: 'SMTP', lang: 'en' },
  { file: 'docs/imap.md', title: 'IMAP', lang: 'en' },
  { file: 'docs/storage.md', title: 'Storage', lang: 'en' },
  { file: 'docs/sync.md', title: 'Synchronisation', lang: 'en' },
  { file: 'docs/security.md', title: 'Security', lang: 'en' },
  { file: 'docs/deployment.md', title: 'Deployment', lang: 'en' },
  { file: 'docs/client.md', title: 'Official Client', lang: 'en' },
  { file: 'docs/zh/architecture.md', title: '架构', lang: 'zh' },
  { file: 'docs/zh/api.md', title: 'HTTP API', lang: 'zh' },
  { file: 'docs/zh/fcp.md', title: '客户端协议 (FCP)', lang: 'zh' },
  { file: 'docs/zh/smtp.md', title: 'SMTP', lang: 'zh' },
  { file: 'docs/zh/imap.md', title: 'IMAP', lang: 'zh' },
  { file: 'docs/zh/storage.md', title: '存储', lang: 'zh' },
  { file: 'docs/zh/sync.md', title: '同步', lang: 'zh' },
  { file: 'docs/zh/security.md', title: '安全', lang: 'zh' },
  { file: 'docs/zh/deployment.md', title: '部署', lang: 'zh' },
  { file: 'docs/zh/client.md', title: '官方客户端', lang: 'zh' },
];

// ── Markdown → HTML ──────────────────────────────
function esc(s) {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

function inlineMd(text) {
  let s = text;
  s = s.replace(/`([^`]+)`/g, '<code>$1</code>');
  s = s.replace(/\*\*\*(.+?)\*\*\*/g, '<strong><em>$1</em></strong>');
  s = s.replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>');
  s = s.replace(/\*(.+?)\*/g, '<em>$1</em>');
  s = s.replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="$2">$1</a>');
  return s;
}

function mdToHtml(md) {
  const out = [];
  const lines = md.split('\n');
  let i = 0;
  let inCode = false;
  let codeLang = '';
  let codeLines = [];

  function flushCode() {
    if (codeLines.length) {
      const lang = codeLang || 'text';
      const html = codeLines.map(l => esc(l)).join('\n');
      out.push(`<pre><code class="language-${lang}">${html}</code></pre>`);
      codeLines = [];
      codeLang = '';
    }
  }

  while (i < lines.length) {
    const line = lines[i];

    // Code fence
    const fenceMatch = line.match(/^```(\w*)/);
    if (fenceMatch) {
      if (inCode) {
        flushCode();
        inCode = false;
      } else {
        inCode = true;
        codeLang = fenceMatch[1] || '';
        codeLines = [];
      }
      i++;
      continue;
    }
    if (inCode) { codeLines.push(line); i++; continue; }

    // Horizontal rule
    if (/^---+$/.test(line.trim()) || /^\*\*\*+$/.test(line.trim())) {
      out.push('<hr>'); i++; continue;
    }

    // Table
    if (line.startsWith('|') && line.includes('|')) {
      const tableRows = [];
      while (i < lines.length && lines[i].startsWith('|') && lines[i].includes('|')) {
        tableRows.push(lines[i]);
        i++;
      }
      const headerRow = tableRows[0];
      const sepRow = tableRows[1];
      const isSep = sepRow && /^[\|:\-\s]+$/.test(sepRow);
      const dataRows = isSep ? tableRows.slice(2) : tableRows.slice(1);
      const cells = headerRow.split('|').map(c => c.trim()).filter(Boolean);
      out.push('<table><thead><tr>' + cells.map(c => `<th>${inlineMd(c)}</th>`).join('') + '</tr></thead>');
      if (dataRows.length) {
        out.push('<tbody>');
        for (const row of dataRows) {
          const rc = row.split('|').map(c => c.trim()).filter(Boolean);
          out.push('<tr>' + rc.map(c => `<td>${inlineMd(c)}</td>`).join('') + '</tr>');
        }
        out.push('</tbody>');
      }
      out.push('</table>');
      continue;
    }

    // Headers
    const hMatch = line.match(/^(#{1,6})\s+(.+)$/);
    if (hMatch) {
      const level = hMatch[1].length;
      const tag = 'h' + level;
      out.push(`<${tag}>${inlineMd(hMatch[2])}</${tag}>`);
      i++; continue;
    }

    // Blockquote
    if (line.startsWith('> ')) {
      const qlines = [];
      while (i < lines.length && (lines[i].startsWith('> ') || lines[i] === '>')) {
        qlines.push(lines[i].replace(/^> ?/, ''));
        i++;
      }
      out.push('<blockquote><p>' + qlines.map(l => inlineMd(l)).join('<br>') + '</p></blockquote>');
      continue;
    }

    // Unordered list
    if (/^[-*+] .+/.test(line)) {
      const items = [];
      while (i < lines.length && /^[-*+] .+/.test(lines[i])) {
        items.push(lines[i].replace(/^[-*+] /, ''));
        i++;
      }
      out.push('<ul>' + items.map(it => `<li>${inlineMd(it)}</li>`).join('') + '</ul>');
      continue;
    }

    // Ordered list
    if (/^\d+\. .+/.test(line)) {
      const items = [];
      while (i < lines.length && /^\d+\. .+/.test(lines[i])) {
        items.push(lines[i].replace(/^\d+\. /, ''));
        i++;
      }
      out.push('<ol>' + items.map(it => `<li>${inlineMd(it)}</li>`).join('') + '</ol>');
      continue;
    }

    // Empty line
    if (line.trim() === '') { i++; continue; }

    // Paragraph
    const para = [];
    while (i < lines.length && lines[i].trim() !== '' && !lines[i].match(/^#{1,6}\s/) && !lines[i].match(/^[-*+] /) && !lines[i].match(/^\d+\. /) && !(lines[i].startsWith('|') && lines[i].includes('|')) && !lines[i].startsWith('>') && !lines[i].match(/^---+$/)) {
      para.push(lines[i]);
      i++;
    }
    if (para.length) {
      out.push('<p>' + inlineMd(para.join(' ')) + '</p>');
    }
  }

  flushCode();
  return out.join('\n');
}

// ── TOC ───────────────────────────────────────────
function buildToc(lang) {
  const items = lang === 'zh' ? [
    { title: '概述', href: 'index.html' },
    { title: '架构', href: 'architecture.html' },
    { title: 'HTTP API', href: 'api.html' },
    { title: '客户端协议 (FCP)', href: 'fcp.html' },
    { title: 'SMTP', href: 'smtp.html' },
    { title: 'IMAP', href: 'imap.html' },
    { title: '存储', href: 'storage.html' },
    { title: '同步', href: 'sync.html' },
    { title: '安全', href: 'security.html' },
    { title: '部署', href: 'deployment.html' },
    { title: '官方客户端', href: 'client.html' },
  ] : [
    { title: 'Overview', href: 'index.html' },
    { title: 'Architecture', href: 'architecture.html' },
    { title: 'HTTP API', href: 'api.html' },
    { title: 'Client Protocol (FCP)', href: 'fcp.html' },
    { title: 'SMTP', href: 'smtp.html' },
    { title: 'IMAP', href: 'imap.html' },
    { title: 'Storage', href: 'storage.html' },
    { title: 'Synchronisation', href: 'sync.html' },
    { title: 'Security', href: 'security.html' },
    { title: 'Deployment', href: 'deployment.html' },
    { title: 'Official Client', href: 'client.html' },
  ];
  return items.map(it => `<a href="${it.href}" class="toc-link">${it.title}</a>`).join('');
}

// ── HTML template ─────────────────────────────────
function template(title, body, lang) {
  const toc = buildToc(lang);
  const assetsPrefix = lang === 'zh' ? '../' : '';
  const otherLangHref = lang === 'zh' ? '../' : 'zh/';
  const otherLangLabel = lang === 'zh' ? 'English' : '中文';

  return `<!DOCTYPE html>
<html lang="${lang}">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>${title} — Ferroma</title>
<link rel="stylesheet" href="${assetsPrefix}assets/style.css">
</head>
<body>
<header class="topbar">
  <div class="topbar-inner">
    <a href="${assetsPrefix}index.html" class="brand">⚡ Ferroma</a>
    <nav class="topbar-nav">
      <a href="${assetsPrefix}index.html" class="nav-link">Overview</a>
      <a href="${assetsPrefix}architecture.html" class="nav-link">Architecture</a>
      <a href="${assetsPrefix}api.html" class="nav-link">API</a>
      <a href="${assetsPrefix}fcp.html" class="nav-link">FCP</a>
      <a href="${assetsPrefix}smtp.html" class="nav-link">SMTP</a>
      <a href="${assetsPrefix}imap.html" class="nav-link">IMAP</a>
      <a href="${assetsPrefix}storage.html" class="nav-link">Storage</a>
      <a href="${assetsPrefix}sync.html" class="nav-link">Sync</a>
      <a href="${assetsPrefix}security.html" class="nav-link">Security</a>
      <a href="${assetsPrefix}deployment.html" class="nav-link">Deploy</a>
      <a href="${assetsPrefix}client.html" class="nav-link">Client</a>
    </nav>
    <div class="topbar-actions">
      <button id="searchBtn" class="icon-btn" title="Search">🔍</button>
      <a href="${otherLangHref}" class="lang-btn">${otherLangLabel}</a>
    </div>
  </div>
</header>

<div class="layout">
  <aside class="sidebar">
    <div class="sidebar-inner">
      <div class="sidebar-title">Contents</div>
      ${toc}
    </div>
  </aside>

  <main class="content">
    <article class="doc-article">
      ${body}
    </article>
  </main>
</div>

<div id="searchOverlay" class="search-overlay" hidden>
  <div class="search-box">
    <button id="searchClose" class="icon-btn">✕</button>
    <input id="searchInput" type="text" placeholder="Search docs…" autocomplete="off">
    <div id="searchResults" class="search-results"></div>
  </div>
</div>

<script src="${assetsPrefix}assets/search.js"></script>
</body>
</html>`;
}

// ── Build ─────────────────────────────────────────
function build() {
  if (fs.existsSync(OUT)) fs.rmSync(OUT, { recursive: true });
  fs.mkdirSync(path.join(OUT, 'assets'), { recursive: true });
  fs.mkdirSync(path.join(OUT, 'zh'), { recursive: true });
  fs.mkdirSync(path.join(OUT, 'zh', 'assets'), { recursive: true });

  // ── CSS ──
  const css = `/* ── Ferroma Docs — Static Site Theme ─────────────── */
:root {
  --bg: #0f1117;
  --surface: #1a1d27;
  --surface2: #242736;
  --border: #2d3148;
  --text: #c9d1d9;
  --text-dim: #8b949e;
  --accent: #58a6ff;
  --accent2: #f0883e;
  --green: #3fb950;
  --red: #f85149;
  --purple: #a371f7;
  --code-bg: #161b22;
  --max-w: 860px;
  --sidebar-w: 240px;
}
*,*::before,*::after{box-sizing:border-box;margin:0;padding:0}
body{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,'Helvetica Neue',Arial,sans-serif;background:var(--bg);color:var(--text);line-height:1.7;font-size:15px}

.topbar{position:sticky;top:0;z-index:100;background:var(--surface);border-bottom:1px solid var(--border);height:56px}
.topbar-inner{max-width:calc(var(--max-w)+var(--sidebar-w)+48px);margin:0 auto;display:flex;align-items:center;height:100%;padding:0 24px;gap:32px}
.brand{font-size:18px;font-weight:700;color:var(--accent);text-decoration:none;letter-spacing:-0.5px}
.topbar-nav{display:flex;gap:4px;flex-wrap:nowrap;overflow-x:auto}
.nav-link{color:var(--text-dim);text-decoration:none;font-size:13px;padding:6px 10px;border-radius:6px;white-space:nowrap;transition:color .15s,background .15s}
.nav-link:hover{color:var(--text);background:var(--surface2)}
.topbar-actions{margin-left:auto;display:flex;align-items:center;gap:8px}
.lang-btn{color:var(--accent);text-decoration:none;font-size:13px;padding:4px 10px;border:1px solid var(--border);border-radius:6px}
.icon-btn{background:none;border:1px solid var(--border);color:var(--text-dim);border-radius:6px;padding:6px 10px;cursor:pointer;font-size:14px}
.icon-btn:hover{color:var(--text);background:var(--surface2)}

.layout{display:flex;max-width:calc(var(--max-w)+var(--sidebar-w)+48px);margin:0 auto;min-height:calc(100vh - 56px)}
.sidebar{width:var(--sidebar-w);flex-shrink:0;border-right:1px solid var(--border);padding:24px 16px;position:sticky;top:56px;height:calc(100vh - 56px);overflow-y:auto;background:var(--surface)}
.sidebar-inner{display:flex;flex-direction:column;gap:2px}
.sidebar-title{font-size:11px;text-transform:uppercase;letter-spacing:1px;color:var(--text-dim);padding:8px 10px;margin-top:8px}
.toc-link{color:var(--text-dim);text-decoration:none;font-size:13px;padding:6px 10px;border-radius:6px;transition:color .15s,background .15s}
.toc-link:hover{color:var(--text);background:var(--surface2)}

.content{flex:1;padding:32px 40px 80px;max-width:var(--max-w)}
.doc-article{font-size:15px}
.doc-article h1{font-size:28px;margin-bottom:8px;color:#e6edf3;letter-spacing:-0.5px}
.doc-article h2{font-size:22px;margin:36px 0 12px;color:#e6edf3;border-bottom:1px solid var(--border);padding-bottom:8px}
.doc-article h3{font-size:18px;margin:28px 0 8px;color:#e6edf3}
.doc-article h4{font-size:16px;margin:20px 0 6px;color:#e6edf3}
.doc-article p{margin-bottom:16px}
.doc-article a{color:var(--accent);text-decoration:none}
.doc-article a:hover{text-decoration:underline}
.doc-article ul,.doc-article ol{margin:0 0 16px 24px}
.doc-article li{margin-bottom:6px}
.doc-article hr{border:none;border-top:1px solid var(--border);margin:24px 0}
.doc-article strong{color:#e6edf3}
.doc-article code{background:var(--code-bg);padding:2px 6px;border-radius:4px;font-size:13px;font-family:'SF Mono','Fira Code','Consolas',monospace;color:var(--accent2)}
.doc-article pre{background:var(--code-bg);border:1px solid var(--border);border-radius:8px;padding:16px;overflow-x:auto;margin:16px 0;font-size:13px;line-height:1.6}
.doc-article pre code{background:none;padding:0;color:var(--text)}
.doc-article table{width:100%;border-collapse:collapse;margin:16px 0;font-size:14px}
.doc-article th,.doc-article td{border:1px solid var(--border);padding:8px 12px;text-align:left}
.doc-article th{background:var(--surface2);color:var(--accent);font-weight:600}
.doc-article tr:nth-child(even){background:var(--surface)}
.doc-article .lead{font-size:17px;color:var(--text-dim);margin-bottom:24px}
.doc-article .doc-list{list-style:none;margin-left:0}
.doc-article .doc-list li{margin-bottom:8px}
.doc-article blockquote{border-left:3px solid var(--accent);padding:8px 16px;margin:16px 0;background:var(--surface);border-radius:0 6px 6px 0;color:var(--text-dim)}

.search-overlay{position:fixed;inset:0;z-index:200;background:rgba(0,0,0,0.6);display:flex;align-items:flex-start;justify-content:center;padding-top:80px}
.search-box{position:relative;background:var(--surface);border:1px solid var(--border);border-radius:12px;width:100%;max-width:600px;padding:16px;box-shadow:0 16px 64px rgba(0,0,0,0.5)}
#searchClose{position:absolute;top:8px;right:8px;background:none;border:none;color:var(--text-dim);cursor:pointer;font-size:16px;padding:4px 8px;border-radius:6px;line-height:1}
#searchClose:hover{color:var(--text);background:var(--surface2)}
.search-box input{width:100%;background:var(--bg);border:1px solid var(--border);border-radius:8px;padding:10px 14px;color:var(--text);font-size:16px;outline:none}
.search-box input:focus{border-color:var(--accent)}
.search-results{margin-top:12px;max-height:400px;overflow-y:auto}
.search-results a{display:block;padding:10px 12px;border-radius:6px;color:var(--text);text-decoration:none;font-size:14px}
.search-results a:hover{background:var(--surface2)}
.search-results .search-title{color:var(--accent);font-weight:600}
.search-results .search-lang{color:var(--text-dim);font-size:12px;margin-left:8px}

@media(max-width:900px){.sidebar{display:none}.content{padding:24px 20px 60px}.topbar-nav{gap:0}.nav-link{font-size:12px;padding:6px 6px}}
@media(prefers-reduce-motion:reduce){*{transition:none!important}}
`;

  // ── Search JS ──
  const js = `// ── Search ──────────────────────────────────────────────
let idx = [];
async function buildIndex() {
  try {
    const res = await fetch('search-index.json');
    idx = await res.json();
  } catch(e) { idx = []; }
}

document.getElementById('searchBtn').addEventListener('click', () => {
  document.getElementById('searchOverlay').hidden = false;
  document.getElementById('searchInput').focus();
});
document.getElementById('searchClose').addEventListener('click', () => {
  document.getElementById('searchOverlay').hidden = true;
});
document.getElementById('searchOverlay').addEventListener('click', (e) => {
  if (e.target === document.getElementById('searchOverlay')) {
    document.getElementById('searchOverlay').hidden = true;
  }
});
document.getElementById('searchInput').addEventListener('input', (e) => {
  const q = e.target.value.trim().toLowerCase();
  const box = document.getElementById('searchResults');
  if (!q || !idx.length) { box.innerHTML = ''; return; }
  const hits = idx.filter(i => i.title.toLowerCase().includes(q) || (i.body||'').toLowerCase().includes(q));
  box.innerHTML = hits.slice(0, 20).map(h => {
    return '<a href=\"' + h.href + '\"><span class=\"search-title\">' + h.title + '</span><span class=\"search-lang\">' + (h.lang === 'zh' ? '中文' : 'EN') + '</span></a>';
  }).join('');
});

document.addEventListener('keydown', (e) => {
  if ((e.metaKey || e.ctrlKey) && e.key === 'k') { e.preventDefault(); document.getElementById('searchBtn').click(); }
  if (e.key === 'Escape') document.getElementById('searchOverlay').hidden = true;
});

buildIndex();
`;

  fs.writeFileSync(path.join(OUT, 'assets', 'style.css'), css);
  fs.writeFileSync(path.join(OUT, 'assets', 'search.js'), js);
  fs.writeFileSync(path.join(OUT, 'zh', 'assets', 'style.css'), css);
  fs.writeFileSync(path.join(OUT, 'zh', 'assets', 'search.js'), js);

  // ── Process docs ──
  const searchIndex = [];

  for (const doc of DOCS) {
    const mdPath = path.join(ROOT, doc.file);
    if (!fs.existsSync(mdPath)) {
      console.warn('Missing:', doc.file);
      continue;
    }
    const md = fs.readFileSync(mdPath, 'utf-8');
    const body = mdToHtml(md);
    const html = template(doc.title, body, doc.lang);
    const outName = path.basename(doc.file, '.md') + '.html';
    const outDir = doc.lang === 'zh' ? path.join(OUT, 'zh') : OUT;
    fs.mkdirSync(outDir, { recursive: true });
    fs.writeFileSync(path.join(outDir, outName), html);

    // Strip HTML tags for search index body
    const bodyText = body.replace(/<[^>]*>/g, ' ').replace(/\s+/g, ' ').trim();
    searchIndex.push({
      title: doc.title,
      href: (doc.lang === 'zh' ? 'zh/' : '') + outName,
      lang: doc.lang,
      body: bodyText.substring(0, 500),
    });

    console.log('Built:', path.join(outDir, outName));
  }

  // ── Index pages ──
  const indexBodyEn = `
<h1>Ferroma Documentation</h1>
<p class="lead">A from-scratch, Rust-native self-hosted mail platform with a cross-platform official client.</p>
<h2>Documents</h2>
<ul class="doc-list">
  <li><a href="architecture.html">Architecture</a> — Crate graph, layering rule, request lifecycle, event bus</li>
  <li><a href="api.html">HTTP API</a> — Management API, Client API (FCP), endpoints, error envelope</li>
  <li><a href="fcp.html">Client Protocol (FCP)</a> — Sync cursor, realtime events, chunked uploads</li>
  <li><a href="smtp.html">SMTP</a> — Inbound, submission, outbound delivery, reply codes</li>
  <li><a href="imap.html">IMAP</a> — Commands, UIDs, flags, Maildir++ mapping, compatibility</li>
  <li><a href="storage.html">Storage</a> — PostgreSQL schema, Maildir, attachments, backup</li>
  <li><a href="sync.html">Synchronisation</a> — change_log, tombstones, conflict resolution</li>
  <li><a href="security.html">Security</a> — Threat model, password hashing, TLS, limits</li>
  <li><a href="deployment.html">Deployment</a> — Docker Compose, DNS, TLS, Let's Encrypt</li>
  <li><a href="client.html">Official Client</a> — Shared core, SQLite cache, Outbox, sync engine</li>
</ul>
<h2>Quick links</h2>
<ul>
  <li><a href="https://github.com" target="_blank">GitHub Repository</a></li>
</ul>
`;

  const indexBodyZh = `
<h1>Ferroma 文档</h1>
<p class="lead">从零构建的 Rust 原生自建邮件平台，附跨平台官方客户端。</p>
<h2>文档</h2>
<ul class="doc-list">
  <li><a href="architecture.html">架构</a> — Crate 图、分层规则、请求生命周期、事件总线</li>
  <li><a href="api.html">HTTP API</a> — 管理 API、客户端 API (FCP)、端点、错误格式</li>
  <li><a href="fcp.html">客户端协议 (FCP)</a> — 同步游标、实时事件、分块上传</li>
  <li><a href="smtp.html">SMTP</a> — 入站、提交、出站投递、回复码</li>
  <li><a href="imap.html">IMAP</a> — 命令、UID、标志、Maildir++ 映射</li>
  <li><a href="storage.html">存储</a> — PostgreSQL 模式、Maildir、附件、备份</li>
  <li><a href="sync.html">同步</a> — change_log、墓碑、冲突解决</li>
  <li><a href="security.html">安全</a> — 威胁模型、密码哈希、TLS、限制</li>
  <li><a href="deployment.html">部署</a> — Docker Compose、DNS、TLS、Let's Encrypt</li>
  <li><a href="client.html">官方客户端</a> — 共享核心、SQLite 缓存、Outbox、同步引擎</li>
</ul>
<h2>快速链接</h2>
<ul>
  <li><a href="https://github.com" target="_blank">GitHub 仓库</a></li>
</ul>
`;

  fs.writeFileSync(path.join(OUT, 'index.html'), template('Ferroma Documentation', indexBodyEn, 'en'));
  fs.writeFileSync(path.join(OUT, 'zh', 'index.html'), template('Ferroma 文档', indexBodyZh, 'zh'));

  // ── Search index ──
  // English: relative from root
  fs.writeFileSync(path.join(OUT, 'search-index.json'), JSON.stringify(searchIndex.filter(i => i.lang === 'en'), null, 2));
  // Chinese: relative from zh/
  fs.writeFileSync(path.join(OUT, 'zh', 'search-index.json'), JSON.stringify(searchIndex.filter(i => i.lang === 'zh'), null, 2));

  console.log('\n✅ Done. Output:', OUT);
}

build();
