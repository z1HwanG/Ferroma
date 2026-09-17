#!/usr/bin/env node
/**
 * Insert `server.cleanup().await;` before the closing brace of every async test in
 * `server/tests/e2e.rs` that starts a `Server`.
 *
 * Written as a small parser rather than a regex because brace tracking inside string
 * literals — and this file is full of `{` and `}` in format strings — is exactly the
 * kind of thing a regex gets wrong.
 *
 *   node tools/add-e2e-cleanup.mjs          # dry run
 *   node tools/add-e2e-cleanup.mjs --write
 */
import fs from 'node:fs';

const path = 'server/tests/e2e.rs';
const write = process.argv.includes('--write');
const source = fs.readFileSync(path, 'utf8');
const lines = source.split('\n');

/** Find the byte index of the `{` that opens a function body at column 0. */
function functionBodyStart(line) {
  if (!/^(pub )?(async )?fn /.test(line)) return -1;
  return line.lastIndexOf('{');
}

let insertions = [];

for (let i = 0; i < lines.length; i++) {
  if (functionBodyStart(lines[i]) === -1) continue;

  // Find the matching close brace, ignoring braces inside strings and comments.
  let depth = 0;
  let inString = false;
  let inLineComment = false;
  let end = -1;

  outer: for (let j = i; j < lines.length; j++) {
    const line = lines[j];
    inLineComment = false;
    for (let k = 0; k < line.length; k++) {
      const ch = line[k];

      if (inLineComment) break;
      if (inString) {
        if (ch === '\\') {
          k++;
          continue;
        }
        if (ch === '"') inString = false;
        continue;
      }
      if (ch === '"') {
        inString = true;
        continue;
      }
      if (ch === '/' && line[k + 1] === '/') {
        inLineComment = true;
        continue;
      }
      if (ch === '{') depth++;
      if (ch === '}') {
        depth--;
        if (depth === 0 && j > i) {
          end = j;
          break outer;
        }
      }
    }
  }

  if (end === -1) continue;

  const body = lines.slice(i, end + 1).join('\n');
  if (!body.includes('Server::start()')) continue;
  if (body.includes('cleanup().await')) continue;

  insertions.push({ start: i, at: end, name: lines[i].trim().slice(0, 60) });
}

for (const ins of insertions) {
  console.log(`will insert before line ${ins.at + 1}: ${ins.name}`);
}

if (!write) {
  console.log(`\n${insertions.length} insertion(s); pass --write to apply`);
  process.exit(0);
}

// Apply from the bottom so earlier line numbers stay valid.
for (const ins of insertions.reverse()) {
  lines.splice(ins.at, 0, '');
  lines.splice(ins.at, 0, '    server.cleanup().await;');
}

fs.writeFileSync(path, lines.join('\n'));
console.log(`\napplied ${insertions.length} insertion(s)`);
