#!/usr/bin/env node
/**
 * Take the serial guard at the top of every acceptance test that starts a server.
 *
 * Those tests bind fixed ports, so they must not overlap; the guard is a
 * `tokio::sync::Mutex` held for the test's duration.
 *
 *   node tools/add-e2e-serial.mjs          # dry run
 *   node tools/add-e2e-serial.mjs --write
 */
import fs from 'node:fs';

const path = 'server/tests/e2e.rs';
const write = process.argv.includes('--write');
const source = fs.readFileSync(path, 'utf8');
const lines = source.split('\n');

const ANCHOR = 'let server = Server::start().await;';
let applied = 0;

for (let i = 0; i < lines.length; i++) {
  if (!lines[i].includes(ANCHOR)) continue;
  // Already guarded?
  const window = lines.slice(Math.max(0, i - 8), i + 1).join('\n');
  if (window.includes('serial_guard()')) continue;

  console.log(`will guard line ${i + 1}: ${lines[i].trim()}`);
  if (write) {
    lines.splice(
      i,
      0,
      '    // Held for the whole test: these tests bind fixed ports and start a real server.',
      '    let _serial = serial_guard().lock().await;',
      '',
    );
    applied++;
    i += 3;
  }
}

if (write) {
  fs.writeFileSync(path, lines.join('\n'));
  console.log(`\napplied ${applied} guard(s)`);
} else {
  console.log('\ndry run; pass --write to apply');
}
