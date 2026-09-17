#!/usr/bin/env node
/**
 * Ferroma dev-time crates.io proxy.
 *
 * Why this exists: on this Windows host the system TLS stack (schannel) is broken
 * (`SEC_E_NO_CREDENTIALS`), so cargo/curl/git cannot negotiate HTTPS. Node ships its
 * own OpenSSL, which works. This server therefore exposes the crates.io *sparse index*
 * and *crate downloads* over plain HTTP on localhost, fetching upstream with Node.
 *
 * cargo is pointed here through the workspace `.cargo/config.toml` source replacement,
 * so no schannel is involved anywhere in the toolchain.
 *
 * Routes
 *   GET /health                      -> "ok"
 *   GET /index/config.json           -> upstream config.json with `dl` rewritten here
 *   GET /index/<prefix>/<name>       -> sparse index entry (newline-delimited JSON)
 *   GET /dl/<name>/<version>         -> static.crates.io crates/<name>/<name>-<version>.crate
 *   GET /proxy?url=<urlencoded>      -> generic HTTPS fetch (used for toolchain blobs)
 *
 * Everything is cached on disk under .cache/crates-proxy.
 */
import http from 'node:http';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import crypto from 'node:crypto';

const PORT = Number(process.env.FERROMA_PROXY_PORT || 8931);
const HOST = process.env.FERROMA_PROXY_HOST || '127.0.0.1';
const ROOT = process.env.FERROMA_PROXY_ROOT || process.cwd();
const CACHE = process.env.FERROMA_PROXY_CACHE || path.join(ROOT, '.cache', 'crates-proxy');
const INDEX_UP = 'https://index.crates.io';
const DL_UP = 'https://static.crates.io';
const UPSTREAM_TIMEOUT_MS = Number(process.env.FERROMA_PROXY_TIMEOUT || 120000);

await fsp.mkdir(CACHE, { recursive: true });

const log = (...a) => console.error(`[crates-proxy]`, ...a);

function cachePathFor(key) {
  const h = crypto.createHash('sha256').update(key).digest('hex');
  return path.join(CACHE, h.slice(0, 2), h);
}

/** Fetch upstream with retries. Node's fetch uses OpenSSL, which works on this host. */
async function fetchUpstream(url, attempt = 0) {
  try {
    const res = await fetch(url, {
      redirect: 'follow',
      signal: AbortSignal.timeout(UPSTREAM_TIMEOUT_MS),
      headers: { 'user-agent': 'ferroma-crates-proxy/0.1' },
    });
    if (!res.ok) throw new Error(`upstream ${res.status} ${res.statusText}`);
    return Buffer.from(await res.arrayBuffer());
  } catch (err) {
    if (attempt >= 4) throw err;
    const delay = 400 * 2 ** attempt;
    log(`retry ${attempt + 1} in ${delay}ms: ${url} (${err.message})`);
    await new Promise((r) => setTimeout(r, delay));
    return fetchUpstream(url, attempt + 1);
  }
}

/** Read-through disk cache. */
async function cached(key, produce) {
  const file = cachePathFor(key);
  try {
    return await fsp.readFile(file);
  } catch {
    /* miss */
  }
  const body = await produce();
  await fsp.mkdir(path.dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  await fsp.writeFile(tmp, body);
  await fsp.rename(tmp, file);
  return body;
}

const stats = { index: 0, dl: 0, proxy: 0, bytes: 0 };

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, `http://${HOST}:${PORT}`);
  const send = (code, body, type = 'application/octet-stream') => {
    res.writeHead(code, {
      'content-type': type,
      'content-length': Buffer.byteLength(body),
      'cache-control': 'no-store',
    });
    res.end(body);
  };

  try {
    if (url.pathname === '/health') return send(200, 'ok', 'text/plain');

    // Sparse index root config. `dl` must be an absolute template cargo can fill in.
    if (url.pathname === '/index/config.json') {
      const body = await cached('index:config.json', async () => {
        const up = await fetchUpstream(`${INDEX_UP}/config.json`);
        const cfg = JSON.parse(up.toString('utf8'));
        cfg.dl = `http://${HOST}:${PORT}/dl/{crate}/{version}`;
        return Buffer.from(JSON.stringify(cfg));
      });
      return send(200, body, 'application/json');
    }

    // Sparse index entries: /index/<prefix>/<name>
    if (url.pathname.startsWith('/index/')) {
      const rel = url.pathname.slice('/index'.length);
      const body = await cached(`index:${rel}`, () => fetchUpstream(`${INDEX_UP}${rel}`));
      stats.index++;
      stats.bytes += body.length;
      return send(200, body, 'text/plain');
    }

    // Crate archive downloads: /dl/<name>/<version>
    const dl = url.pathname.match(/^\/dl\/([^/]+)\/([^/]+)$/);
    if (dl) {
      const [, name, version] = dl.map(decodeURIComponent);
      const body = await cached(`crate:${name}-${version}`, () =>
        fetchUpstream(`${DL_UP}/crates/${name}/${name}-${version}.crate`),
      );
      stats.dl++;
      stats.bytes += body.length;
      log(`dl ${name}-${version} (${(body.length / 1024).toFixed(0)} KiB)`);
      return send(200, body, 'application/gzip');
    }

    // Generic HTTPS fetcher for toolchain blobs (PostgreSQL zip, etc).
    if (url.pathname === '/proxy') {
      const target = url.searchParams.get('url');
      if (!target) return send(400, 'missing url', 'text/plain');
      const body = await cached(`proxy:${target}`, () => fetchUpstream(target));
      stats.proxy++;
      stats.bytes += body.length;
      log(`proxy ${target} (${(body.length / 1024 / 1024).toFixed(1)} MiB)`);
      return send(200, body);
    }

    return send(404, 'not found', 'text/plain');
  } catch (err) {
    log(`ERROR ${req.method} ${req.url}: ${err.message}`);
    return send(502, `proxy error: ${err.message}`, 'text/plain');
  }
});

server.listen(PORT, HOST, () => {
  log(`listening on http://${HOST}:${PORT}  cache=${CACHE}`);
  log(`stats: ${JSON.stringify(stats)}`);
});

process.on('SIGINT', () => {
  log(`shutting down. stats: ${JSON.stringify(stats)}`);
  server.close(() => process.exit(0));
});
