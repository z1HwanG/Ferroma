#!/usr/bin/env node
/**
 * Regression check for the QR encoder the Webmail draws TOTP enrollment with.
 *
 * `shared/qr.js` is a from-scratch encoder — there is no QR library, because the
 * front ends have no build step and the page it runs on is holding a shared
 * secret. An encoder that is merely plausible is worse than none: an authenticator
 * scans it and stores a secret the server will never confirm. So the modules are
 * compared, one for one, against the Project Nayuki encoder
 * (https://github.com/nayuki/QR-Code-generator, MIT), which is fetched into the
 * gitignored `.cache/` the first time this runs and never committed.
 *
 * The comparison forces byte mode and mask 0, which is exactly the combination
 * `qrSvg` emits, and it walks every version that encoder can draw.
 *
 * Run from the repository root: `node web/tools/qr.mjs`.
 */

import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const CACHE = resolve(ROOT, '.cache');
const REFERENCE = resolve(CACHE, 'qr-reference.mjs');

const failures = [];
let checks = 0;

function check(condition, message) {
  checks += 1;
  if (!condition) failures.push(message);
}

/**
 * The Nayuki encoder, bundled to one module. Cloned and bundled on demand so the
 * repository does not carry a third-party tree it only needs for this check.
 * @returns {Promise<{QrCode: Function, QrSegment: Function}>}
 */
async function reference() {
  if (!existsSync(REFERENCE)) {
    mkdirSync(CACHE, { recursive: true });
    const source = resolve(CACHE, 'qrgen');
    if (!existsSync(resolve(source, 'typescript-javascript', 'qrcodegen.ts'))) {
      const cloned = spawnSync(
        'git',
        ['clone', '--depth', '1', 'https://github.com/nayuki/QR-Code-generator.git', source],
        { stdio: 'inherit' },
      );
      if (cloned.status !== 0) throw new Error('could not fetch the QR reference encoder');
    }
    const bundled = spawnSync(
      'npx',
      ['--yes', 'esbuild', 'qrcodegen.ts', '--bundle', '--format=esm', `--outfile=${REFERENCE}`],
      { cwd: resolve(source, 'typescript-javascript'), stdio: 'inherit', env: { ...process.env, npm_config_cache: resolve(CACHE, 'npm') } },
    );
    if (bundled.status !== 0) throw new Error('could not bundle the QR reference encoder');
    writeFileSync(REFERENCE, `${readFileSync(REFERENCE, 'utf8')}\nexport { qrcodegen };\n`);
  }
  const loaded = await import(pathToFileURL(REFERENCE).href);
  return { QrCode: loaded.qrcodegen.QrCode, QrSegment: loaded.qrcodegen.QrSegment };
}

/** The modules `qrSvg` drew, without the quiet zone. */
function modulesOf(svg) {
  const extent = Number(svg.match(/viewBox="0 0 (\d+)/)[1]);
  const size = extent - 8;
  const grid = Array.from({ length: size }, () => Array(size).fill(false));
  for (const run of svg.match(/d="([^"]*)"/)[1].matchAll(/M(\d+) (\d+)h(\d+)/g)) {
    const row = Number(run[2]) - 4;
    if (row < 0 || row >= size) continue;
    for (let i = 0; i < Number(run[3]); i += 1) {
      const col = Number(run[1]) - 4 + i;
      if (col >= 0 && col < size) grid[row][col] = true;
    }
  }
  return grid;
}

const { qrSvg } = await import(pathToFileURL(resolve(ROOT, 'shared', 'qr.js')).href);
const { QrCode, QrSegment } = await reference();

/** How many modules differ between our drawing and the reference's. */
function differences(text) {
  const svg = qrSvg(text);
  const segment = QrSegment.makeBytes(new TextEncoder().encode(text));
  const code = QrCode.encodeSegments([segment], QrCode.Ecc.MEDIUM, 1, 40, 0, false);
  if (svg === null) return { version: code.version, count: Infinity };
  const grid = modulesOf(svg);
  if (grid.length !== code.size) return { version: code.version, count: Infinity };
  let count = 0;
  for (let row = 0; row < code.size; row += 1) {
    for (let col = 0; col < code.size; col += 1) {
      if (grid[row][col] !== code.getModule(col, row)) count += 1;
    }
  }
  return { version: code.version, count };
}

// Every length from one byte up to the longest version this encoder draws, which
// exercises each version boundary, both block groups and the 16-bit count field.
const versions = new Set();
for (let length = 1; length <= 213; length += 1) {
  const result = differences('x'.repeat(length));
  versions.add(result.version);
  if (result.count !== 0 && failures.length < 8) {
    failures.push(`a ${length}-byte payload at version ${result.version} differs in ${result.count} modules`);
  }
  checks += 1;
}

check(
  [...versions].sort((a, b) => a - b).join(',') === '1,2,3,4,5,6,7,8,9,10',
  `versions 1 to 10 were not all exercised: ${[...versions].join(',')}`,
);

// The shapes an enrollment actually produces: an ASCII URI, one with a non-ASCII
// issuer, and one long enough to cross into the 16-bit count.
const uri = 'otpauth://totp/Ferroma:alice%40example.com?secret=JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP&issuer=Ferroma&algorithm=SHA1&digits=6&period=30';
const unicode = 'otpauth://totp/Example%20Issuer:%E4%B8%AD%E6%96%87@example.com?secret=JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP&issuer=Example%20Issuer&algorithm=SHA1&digits=6&period=30';
for (const sample of [uri, unicode, '中文密钥测试 — ferroma']) {
  const result = differences(sample);
  check(result.count === 0, `${JSON.stringify(sample).slice(0, 40)} differs in ${result.count} modules`);
}

// Nothing past version 10, and nothing empty, is drawn — a wrong picture is worse
// than the text fallback the settings page already has.
check(qrSvg('x'.repeat(214)) === null, 'a payload past version 10 must not be drawn');
check(qrSvg('') === null, 'an empty string must not be drawn');
check(qrSvg(null) === null, 'a missing value must not throw');

if (failures.length) {
  process.stdout.write(`FAIL — QR encoder (${checks} assertions)\n`);
  for (const failure of failures) process.stdout.write(`  ${failure}\n`);
  process.exit(1);
}
process.stdout.write(`PASS — QR encoder (${checks} assertions)\n`);
