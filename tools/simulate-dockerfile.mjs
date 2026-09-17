#!/usr/bin/env node
/**
 * Replay the Dockerfile's build stages on the host, to test what can be tested
 * without Docker.
 *
 * The machine this was written on has no Docker, so `docker build` is the one part of
 * the delivery that cannot be exercised. Most of it *can* be: the multi-stage build
 * spends its first half proving that the dependency graph resolves from manifests
 * plus placeholder sources, and that half is operating-system independent. If a
 * workspace member is missing from the COPY list, or the placeholder loop forgot a
 * crate, this fails here in a minute instead of on a server in ten.
 *
 * The COPY instructions are **parsed out of the Dockerfile** rather than transcribed.
 * A simulation that copies from a hand-written list would keep passing after the
 * Dockerfile changed, which is the opposite of useful.
 *
 *   node tools/simulate-dockerfile.mjs --stage=placeholder   # manifests + placeholders
 *   node tools/simulate-dockerfile.mjs --stage=real          # overlay the real sources
 *   node tools/simulate-dockerfile.mjs --stage=clean
 */
import fs from 'node:fs';
import path from 'node:path';

const root = process.cwd();
const sim = path.join(root, '.cache', 'dockerfile-sim');
const stage = (process.argv.find((a) => a.startsWith('--stage=')) || '--stage=placeholder').split('=')[1];

const dockerfile = fs.readFileSync(path.join(root, 'Dockerfile'), 'utf8');

/** COPY instructions in the builder stage, in order. */
function builderCopies() {
  const lines = dockerfile.split('\n');
  const copies = [];
  let inBuilder = false;
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i].trim();
    if (/^FROM\s+\S+\s+AS\s+builder/i.test(line)) {
      inBuilder = true;
      continue;
    }
    if (/^FROM\s+/i.test(line)) {
      inBuilder = false;
      continue;
    }
    if (!inBuilder) continue;
    const match = line.match(/^COPY\s+(.*)$/i);
    if (!match) continue;
    const parts = match[1].split(/\s+/).filter((p) => !p.startsWith('--'));
    copies.push({ line: i + 1, sources: parts.slice(0, -1), dest: parts.at(-1) });
  }
  return copies;
}

/** The crate names the placeholder loop creates `src/lib.rs` for. */
function placeholderCrates() {
  const match = dockerfile.match(/for crate in([\s\S]*?);\s*do/);
  if (!match) throw new Error('Dockerfile has no `for crate in` loop');
  return match[1].replace(/\\/g, ' ').split(/\s+/).filter(Boolean);
}

function copyInto(source, dest) {
  const from = path.join(root, source);
  const to = path.join(sim, dest);
  if (!fs.existsSync(from)) throw new Error(`COPY source does not exist: ${source}`);

  const stat = fs.statSync(from);
  if (stat.isDirectory()) {
    fs.mkdirSync(to, { recursive: true });
    fs.cpSync(from, to, { recursive: true });
  } else {
    // `COPY a/b/c.toml a/b/` — the destination is a directory.
    const target = dest.endsWith('/') ? path.join(sim, dest, path.basename(source)) : to;
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.copyFileSync(from, target);
  }
}

// -----------------------------------------------------------------------------
if (stage === 'clean') {
  fs.rmSync(sim, { recursive: true, force: true });
  console.log(`removed ${path.relative(root, sim)}`);
  process.exit(0);
}

if (stage === 'placeholder') {
  fs.rmSync(sim, { recursive: true, force: true });
  fs.mkdirSync(sim, { recursive: true });

  // --- the COPYs that come before the placeholder build ----------------------
  const copies = builderCopies();
  const placeholderRunAt = dockerfile.split('\n').findIndex((l) => /cargo build --release --bin ferroma/.test(l));

  let done = 0;
  for (const copy of copies) {
    const lineText = dockerfile.split('\n')[copy.line - 1];
    if (copy.line - 1 > placeholderRunAt) continue; // after the placeholder build
    for (const source of copy.sources) {
      copyInto(source, copy.dest);
      done++;
    }
  }
  console.log(`replayed ${done} COPY source(s) from the Dockerfile`);

  // --- the placeholder sources ----------------------------------------------
  for (const crate of placeholderCrates()) {
    const dir = path.join(sim, 'crates', crate, 'src');
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, 'lib.rs'), '\n');
  }
  console.log(`created placeholder lib.rs for ${placeholderCrates().length} crate(s)`);

  for (const member of ['server', 'client']) {
    const dir = path.join(sim, member, 'src');
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, 'main.rs'), 'fn main() {}\n');
    // The client crate also has a library target; without it, `cargo` sees only the
    // binary here and the shape differs from the real tree. The Dockerfile relies on
    // `cargo build --bin ferroma` not needing it.
  }

  fs.mkdirSync(path.join(sim, 'migrations'), { recursive: true });
  fs.writeFileSync(path.join(sim, 'migrations', '0001_initial.sql'), '-- placeholder\n');
  fs.mkdirSync(path.join(sim, 'config'), { recursive: true });
  fs.writeFileSync(path.join(sim, 'config', 'ferroma.toml'), '\n');

  console.log(`\nplaceholder tree ready at ${path.relative(root, sim)}`);
  console.log('next: cargo build --bin ferroma --manifest-path .cache/dockerfile-sim/Cargo.toml');
  process.exit(0);
}

if (stage === 'real') {
  if (!fs.existsSync(sim)) throw new Error('run --stage=placeholder first');
  const copies = builderCopies();
  const placeholderRunAt = dockerfile.split('\n').findIndex((l) => /cargo build --release --bin ferroma/.test(l));

  let done = 0;
  for (const copy of copies) {
    if (copy.line - 1 < placeholderRunAt) continue; // already done
    for (const source of copy.sources) {
      copyInto(source, copy.dest);
      done++;
    }
  }
  console.log(`overlaid ${done} real source path(s)`);
  console.log('next: cargo build --bin ferroma --manifest-path .cache/dockerfile-sim/Cargo.toml');
  process.exit(0);
}

console.error(`unknown stage: ${stage}`);
process.exit(2);
