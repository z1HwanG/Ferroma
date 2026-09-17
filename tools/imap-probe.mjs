#!/usr/bin/env node
/**
 * Manual IMAP probe: log in, select a folder, and print the subjects it holds.
 *
 * Written because the acceptance tests only tell you "the message was not found",
 * while this shows the tag-by-tag conversation and where it went wrong.
 *
 *   node tools/imap-probe.mjs 143 bob@example.com "correct horse battery staple" INBOX
 */
import net from 'net';

const [, , portArg, user, password, folder = 'INBOX'] = process.argv;
const port = Number(portArg || 143);
const started = Date.now();
const stamp = () => `+${String(Date.now() - started).padStart(6)}ms`;

const socket = net.connect(port, '127.0.0.1');
socket.setEncoding('utf8');

let buffer = '';
let pending = null;

socket.on('data', (chunk) => {
  buffer += chunk;
  let index;
  while ((index = buffer.indexOf('\r\n')) !== -1) {
    const line = buffer.slice(0, index);
    buffer = buffer.slice(index + 2);
    console.log(`${stamp()}  <<< ${line}`);
    // A tagged line completes a command; untagged `*` lines are data.
    if (/^a\d+ (OK|NO|BAD)/.test(line) && pending) {
      const next = pending;
      pending = null;
      next();
    }
  }
});

/** Send a tagged command and wait for its tagged completion. */
function command(tag, line) {
  return new Promise((resolve) => {
    console.log(`${stamp()}  >>> ${line}`);
    socket.write(`${line}\r\n`);
    pending = resolve;
  });
}

socket.on('error', (err) => {
  console.log(`${stamp()}  ERROR ${err.message}`);
  process.exit(1);
});
socket.on('close', () => {
  console.log(`${stamp()}  connection closed`);
  process.exit(0);
});

// The greeting arrives unprompted; wait a beat for it, then run the sequence.
setTimeout(async () => {
  try {
    const login = await command('a1', `a1 LOGIN ${user} "${password}"`);
    void login;
    await command('a2', `a2 SELECT ${folder}`);
    await command('a3', 'a3 FETCH 1:* (BODY.PEEK[HEADER.FIELDS (SUBJECT FROM DATE)])');
    await command('a4', 'a4 LOGOUT');
    socket.end();
  } catch (err) {
    console.log(`${stamp()}  probe failed: ${err.message}`);
    process.exit(1);
  }
}, 300);

setTimeout(() => {
  console.log(`${stamp()}  GIVING UP after 20s`);
  socket.destroy();
  process.exit(1);
}, 20_000);
