#!/usr/bin/env node
/**
 * What is the real minimum supported Rust version of this workspace?
 *
 * The workspace manifest declares `rust-version`, but that is a claim about *our*
 * code. What actually decides whether a toolchain can build the project is the
 * highest `rust-version` among the dependencies, and the Dockerfile pins an exact
 * toolchain — so a stale pin fails the build on the server, hours after it passed
 * every test locally.
 *
 * Cargo does not expose dependency MSRVs through `cargo metadata`. It *does* cache
 * the registry index, and every index entry carries `rust_version`, so this reads the
 * locked version of every dependency straight out of that cache. No network, no child
 * process, no compile.
 *
 *   node tools/msrv.mjs
 */
import fs from 'node:fs';
import path from 'node:path';

const root = process.cwd();

/** Index cache path for a crate name, per the sparse-index layout. */
function indexPath(cacheRoot, name) {
  const lower = name.toLowerCase();
  let prefix;
  if (lower.length === 1) prefix = '1';
  else if (lower.length === 2) prefix = '2';
  else if (lower.length === 3) prefix = `3/${lower[0]}`;
  else prefix = `${lower.slice(0, 2)}/${lower.slice(2, 4)}`;
  return path.join(cacheRoot, prefix, lower);
}

/**
 * Parse an index cache file.
 *
 * The format is a version byte, a NUL, then a sequence of `version\0json\0` records;
 * in practice splitting on NUL and keeping the pieces that start with `{` is enough
 * and does not depend on the exact header layout.
 */
function readIndexCache(file) {
  const raw = fs.readFileSync(file, 'utf8');
  const entries = new Map();
  for (const piece of raw.split('\u0000')) {
    const start = piece.indexOf('{');
    if (start === -1) continue;
    let parsed;
    try {
      parsed = JSON.parse(piece.slice(start));
    } catch {
      continue;
    }
    if (parsed && parsed.vers) entries.set(parsed.vers, parsed);
  }
  return entries;
}

const cmp = (a, b) => {
  const A = a.split('.').map(Number);
  const B = b.split('.').map(Number);
  for (let i = 0; i < 3; i++) {
    const x = A[i] || 0;
    const y = B[i] || 0;
    if (x !== y) return x - y;
  }
  return 0;
};

// --- locate the registry index cache -----------------------------------------
const registryRoot = path.join(root, '.cargo-home', 'registry', 'index');
if (!fs.existsSync(registryRoot)) {
  console.error(`no cargo index cache at ${registryRoot}`);
  console.error('run a build first, or point CARGO_HOME at the right place');
  process.exit(2);
}
const cacheRoot = path.join(
  registryRoot,
  fs.readdirSync(registryRoot)[0],
  '.cache',
);

// --- read the lockfile -------------------------------------------------------
const lock = fs.readFileSync(path.join(root, 'Cargo.lock'), 'utf8');
const locked = [...lock.matchAll(/\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"/g)].map(
  (m) => ({ name: m[1], version: m[2] }),
);

// --- resolve each locked version's MSRV --------------------------------------
const findings = [];
let unknown = 0;

for (const { name, version } of locked) {
  const file = indexPath(cacheRoot, name);
  if (!fs.existsSync(file)) {
    unknown++;
    continue;
  }
  const entries = readIndexCache(file);
  const entry = entries.get(version);
  if (!entry) {
    unknown++;
    continue;
  }
  if (entry.rust_version) findings.push({ name, version, msrv: entry.rust_version });
}

findings.sort((a, b) => cmp(b.msrv, a.msrv));

// --- what the project claims and pins ----------------------------------------
const manifest = fs.readFileSync(path.join(root, 'Cargo.toml'), 'utf8');
const claimed = manifest.match(/rust-version\s*=\s*"([^"]+)"/)?.[1];
const dockerfile = fs.readFileSync(path.join(root, 'Dockerfile'), 'utf8');
const pinned = dockerfile.match(/^FROM\s+rust:([0-9.]+)/m)?.[1];

const highest = findings[0]?.msrv;

console.log(`locked packages:                 ${locked.length}`);
console.log(`with a known MSRV:               ${findings.length}`);
console.log(`without an index entry:          ${unknown}`);
console.log('');
console.log('highest dependency MSRVs:');
for (const f of findings.slice(0, 10)) {
  console.log(`  ${f.msrv.padEnd(8)} ${f.name} ${f.version}`);
}
console.log('');
console.log(`workspace rust-version claims:   ${claimed ?? '(none)'}`);
console.log(`Dockerfile pins:                 rust:${pinned ?? '(none)'}`);
console.log('');

const problems = [];
if (!highest) {
  problems.push('could not determine any dependency MSRV');
} else {
  if (pinned && cmp(pinned, highest) < 0) {
    problems.push(
      `Dockerfile pins rust:${pinned}, but ${findings[0].name} ${findings[0].version} ` +
        `requires ${highest} — the image build fails`,
    );
  }
  if (claimed && cmp(claimed, highest) < 0) {
    problems.push(
      `Cargo.toml claims rust-version ${claimed}, but a dependency requires ${highest} — ` +
        `the claim is false and a user on ${claimed} gets a confusing failure`,
    );
  }
}

if (problems.length) {
  for (const problem of problems) console.log(`FAIL  ${problem}`);
  process.exit(1);
}
console.log(`PASS  rust:${pinned} and rust-version ${claimed} both satisfy the dependency graph`);
