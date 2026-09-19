#!/usr/bin/env node
/**
 * Serve the two front-ends exactly the way the server does, without building or running
 * the server.
 *
 * This exists because the layout is not a plain static directory any more. The three
 * mounts the router creates are reproduced here:
 *
 *   `/`        → `web/`, SPA fallback to `web/index.html`
 *   `/admin`   → `admin/`, SPA fallback to `admin/index.html`, `/admin` → `/admin/`
 *   `/shared`  → `shared/`
 *
 * `/shared` is the part a naive file server gets wrong: both apps import their common
 * modules as `../shared/…`, which resolves to `/shared/…` from `/main.js` and from
 * `/admin/main.js`, so serving `web/` alone leaves every shared import as a 404 and
 * neither app starts.
 *
 * Any `/api/v1` request is answered with a `503` envelope rather than an HTML page, so a
 * screen that needs the API fails visibly instead of rendering `index.html` as data.
 *
 * Usage:
 *   node tools/serve-frontends.mjs [--port 8099] [--host 127.0.0.1]
 */

import { createReadStream } from 'node:fs';
import { stat } from 'node:fs/promises';
import http from 'node:http';
import { dirname, extname, join, normalize, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const args = process.argv.slice(2);
const flag = (name, fallback) => {
  const at = args.indexOf(`--${name}`);
  return at === -1 ? fallback : args[at + 1];
};
const PORT = Number(flag('port', '8099'));
const HOST = flag('host', '127.0.0.1');

const TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.gif': 'image/gif',
  '.webp': 'image/webp',
  '.ico': 'image/x-icon',
  '.woff': 'font/woff',
  '.woff2': 'font/woff2',
  '.txt': 'text/plain; charset=utf-8',
  '.map': 'application/json; charset=utf-8',
};

/**
 * Resolve a URL path inside `root`, or `null` when it escapes the directory.
 * @param {string} root
 * @param {string} pathname
 */
function within(root, pathname) {
  const target = resolve(root, '.' + normalize(decodeURIComponent(pathname)));
  if (target !== root && !target.startsWith(root + sep)) return null;
  return target;
}

/** @param {string} file */
async function isFile(file) {
  try {
    return (await stat(file)).isFile();
  } catch {
    return false;
  }
}

/** @param {http.ServerResponse} response @param {string} file */
function sendFile(response, file) {
  response.writeHead(200, {
    'Content-Type': TYPES[extname(file).toLowerCase()] || 'application/octet-stream',
    // Never cache: an edit should show up on reload, which is the whole point of a
    // development server.
    'Cache-Control': 'no-store',
  });
  createReadStream(file).pipe(response);
}

const server = http.createServer(async (request, response) => {
  const url = new URL(request.url || '/', `http://${request.headers.host || HOST}`);
  const pathname = url.pathname;

  // The API is not here. Answering with the documented envelope shape makes a screen
  // that needs it say so, instead of parsing this server's `index.html` as JSON.
  if (pathname === '/api/v1' || pathname.startsWith('/api/v1/')) {
    const body = JSON.stringify({
      error: {
        code: 'network_error',
        message: `the API is not served here; run \`ferroma serve\` and use its port instead of ${PORT}`,
      },
    });
    response.writeHead(503, { 'Content-Type': 'application/json; charset=utf-8' });
    response.end(body);
    return;
  }

  if (pathname === '/admin') {
    response.writeHead(308, { Location: '/admin/' });
    response.end();
    return;
  }

  const mounts = [
    { prefix: '/shared/', root: join(ROOT, 'shared'), fallback: null },
    { prefix: '/admin/', root: join(ROOT, 'admin'), fallback: join(ROOT, 'admin', 'index.html') },
    { prefix: '/', root: join(ROOT, 'web'), fallback: join(ROOT, 'web', 'index.html') },
  ];

  for (const mount of mounts) {
    if (!pathname.startsWith(mount.prefix)) continue;
    const relative = pathname.slice(mount.prefix.length);
    const target = within(mount.root, '/' + relative);
    if (target && (await isFile(target))) {
      sendFile(response, target);
      return;
    }
    if (mount.fallback && (await isFile(mount.fallback))) {
      sendFile(response, mount.fallback);
      return;
    }
    // The mount matched and has no SPA to fall back to — `shared/` is imported, never
    // navigated to. Answering `404` here is what makes a mistyped or missing shared
    // module obvious: falling through to the Webmail's `index.html` would hand the
    // browser HTML with a `200`, which surfaces as an opaque MIME-type error instead.
    if (!mount.fallback) break;
  }

  response.writeHead(404, { 'Content-Type': 'text/plain; charset=utf-8' });
  response.end(`not found: ${pathname}\n`);
});

server.listen(PORT, HOST, () => {
  process.stdout.write(
    `Ferroma front-ends on http://${HOST}:${PORT}/\n` +
      `  webmail  http://${HOST}:${PORT}/\n` +
      `  admin    http://${HOST}:${PORT}/admin/\n` +
      `  shared   http://${HOST}:${PORT}/shared/i18n.js\n`,
  );
});
