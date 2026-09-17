#!/usr/bin/env node
/**
 * Manual SMTP probe for the acceptance run.
 *
 * The acceptance test only sees "no reply within 60s"; this shows *which* command the
 * server stopped answering, and with what timing, so the hang can be located instead
 * of guessed at.
 *
 *   node tools/smtp-probe.mjs 2625 alice@acceptance.test bob@acceptance.test
 */
import net from 'node:net';

const [, , portArg, from, to] = process.argv;
const port = Number(portArg || 2625);
const started = Date.now();

const stamp = () => `+${String(Date.now() - started).padStart(6)}ms`;

function log(message) {
  console.log(`${stamp()}  ${message}`);
}

const socket = net.connect(port, '127.0.0.1');
socket.setEncoding('utf8');

let buffer = '';
let step = 0;
let pendingReply = null;

const message = [
  `From: ${from}`,
  `To: ${to}`,
  'Subject: Acceptance run',
  'Date: Thu, 16 Sep 2026 12:00:00 +0000',
  'Message-ID: <acceptance-1@acceptance.test>',
  '',
  'Hello Bob,',
  '',
  'this message is the acceptance run.',
  '',
].join('\r\n');

const steps = [
  ['EHLO', 'probe.acceptance.test'],
  ['MAIL', `FROM:<${from}>`],
  ['RCPT', `TO:<${to}>`],
  ['DATA', null],
  // The body and its terminator go in ONE write: the server is not obliged to answer
  // mid-`DATA`, so sending the body and then waiting for a reply before the `.` line
  // deadlocks the probe against a perfectly healthy server.
  ['BODYDOT', message],
  ['QUIT', null],
];

function send(name, payload) {
  const line = payload === null ? `${name}\r\n` : `${name} ${payload}\r\n`;
  log(`>>> ${name}${payload ? ' ' + payload.slice(0, 60) : ''}`);
  socket.write(line);
}

socket.on('connect', () => {
  log(`connected to 127.0.0.1:${port}`);
});

// A deliberately dumb reader: print every complete line as it arrives, and advance
// the conversation only when a reply is not a `NNN-` continuation.
socket.on('data', (chunk) => {
  buffer += chunk;
  let index;
  while ((index = buffer.indexOf('\r\n')) !== -1) {
    const line = buffer.slice(0, index);
    buffer = buffer.slice(index + 2);
    log(`<<< ${line}`);

    const isContinuation = line.length >= 4 && line[3] === '-';
    if (isContinuation) continue;

    // A complete reply arrived; send the next command.
    if (pendingReply) {
      const next = pendingReply;
      pendingReply = null;
      next();
    }
  }
});

function advance() {
  if (step >= steps.length) {
    log('conversation finished');
    socket.end();
    return;
  }
  const [name, payload] = steps[step++];
  if (name === 'BODYDOT') {
    const stuffed = message.replace(/(^|\r\n)\./g, '$1..');
    log(`>>> BODYDOT (${stuffed.length} bytes including the terminator)`);
    socket.write(`${stuffed}.\r\n`);
    pendingReply = advance;
    return;
  }
  send(name, payload);
  pendingReply = advance;
}

// The greeting arrives without a command being sent, so kick the chain off from the
// first reply rather than from `connect`.
pendingReply = advance;

socket.on('error', (err) => log(`ERROR ${err.message}`));
socket.on('close', () => {
  log('connection closed');
  process.exit(0);
});

setTimeout(() => {
  log('GIVING UP after 90s — the server never answered the last command');
  socket.destroy();
  process.exit(1);
}, 90_000);
