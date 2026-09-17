#!/usr/bin/env node
/**
 * Ferroma toolchain downloader.
 *
 * Windows schannel on this host is broken, so curl/Invoke-WebRequest cannot do
 * HTTPS. Node ships its own OpenSSL, so this is the one reliable way to pull
 * build and test tooling (PostgreSQL binaries, Rust dists, …).
 *
 *   node tools/fetch.mjs <url> <dest-file>
 *   node tools/fetch.mjs --find-version <metadata-url> <prefix>   # print newest matching version
 */
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';

const [, , first, second, third] = process.argv;

async function get(url, attempt = 0) {
  try {
    const res = await fetch(url, {
      redirect: 'follow',
      signal: AbortSignal.timeout(600000),
      headers: { 'user-agent': 'ferroma-fetch/0.1' },
    });
    if (!res.ok) throw new Error(`HTTP ${res.status} ${res.statusText}`);
    return res;
  } catch (err) {
    if (attempt >= 4) throw err;
    const delay = 500 * 2 ** attempt;
    console.error(`[fetch] retry ${attempt + 1} in ${delay}ms: ${url} (${err.message})`);
    await new Promise((r) => setTimeout(r, delay));
    return get(url, attempt + 1);
  }
}

if (first === '--find-version') {
  const [, , , metadataUrl, prefix] = process.argv;
  const res = await get(metadataUrl);
  const xml = await res.text();
  const versions = [...xml.matchAll(/<version>([^<]+)<\/version>/g)].map((m) => m[1]);
  const matching = versions.filter((v) => v.startsWith(prefix));
  if (matching.length === 0) {
    console.error(`[fetch] no version matching prefix ${prefix}`);
    process.exit(1);
  }
  // Compare numerically so 16.10 > 16.9
  const key = (v) => v.split(/[.-]/).map((p) => (/^\d+$/.test(p) ? Number(p) : 0));
  matching.sort((a, b) => {
    const ka = key(a);
    const kb = key(b);
    for (let i = 0; i < Math.max(ka.length, kb.length); i++) {
      const d = (kb[i] ?? 0) - (ka[i] ?? 0);
      if (d !== 0) return d;
    }
    return 0;
  });
  console.log(matching[0]);
  process.exit(0);
}

const url = first;
const dest = second;
if (!url || !dest) {
  console.error('usage: node tools/fetch.mjs <url> <dest-file>');
  process.exit(2);
}

await fsp.mkdir(path.dirname(path.resolve(dest)), { recursive: true });
const res = await get(url);
const total = Number(res.headers.get('content-length') || 0);
const out = fs.createWriteStream(dest);
let seen = 0;
let lastReport = 0;

for await (const chunk of res.body) {
  seen += chunk.length;
  if (!out.write(chunk)) await new Promise((r) => out.once('drain', r));
  const now = Date.now();
  if (now - lastReport > 3000) {
    lastReport = now;
    const pct = total ? ` (${((seen / total) * 100).toFixed(1)}%)` : '';
    console.error(`[fetch] ${(seen / 1048576).toFixed(1)} MiB${pct}`);
  }
}
await new Promise((r) => out.end(r));
console.error(`[fetch] wrote ${dest} (${(seen / 1048576).toFixed(1)} MiB)`);
