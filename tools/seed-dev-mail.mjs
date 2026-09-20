/**
 * Deliver a handful of representative messages to the local Ferroma SMTP port, so the
 * Webmail can be exercised against real MIME instead of a hand-built fixture.
 *
 * Development helper. `node tools/seed-dev-mail.mjs [port] [recipient]`
 */

import net from 'node:net';

const port = Number(process.argv[2] || 2527);
const recipient = process.argv[3] || 'alice@example.test';

/** A full HTML message carrying its CSS in `<head>`, the shape most bulk mail has. */
const HEAD_STYLED_HTML = [
  '<!DOCTYPE html>',
  '<html><head>',
  '<meta charset="utf-8">',
  '<style type="text/css">#outlook a { padding:0; } body { margin:0; padding:0; -webkit-text-size-adjust:100%; }</style>',
  '</head><body>',
  '<p>Hi,</p>',
  '<p>Your mail-tester.com score is <strong>10/10</strong>.</p>',
  '<p><a href="https://www.mail-tester.com/report.pdf">Click here to download your PDF report</a></p>',
  '<p>This link regenerates a fresh copy of your report every time it is opened.</p>',
  '</body></html>',
].join('\r\n');

/** A malformed HTML-only message: `<head>` is opened and never closed. */
const UNCLOSED_HEAD_HTML = [
  '<html><head><style>.x{color:red}</style>',
  '<body><p>Body after an unclosed head.</p></body></html>',
].join('');

const MESSAGES = [
  {
    from: 'Score Report <mail-tester@example.net>',
    subject: 'Your Score Report PDF export is ready (10/10)',
    body: [
      'MIME-Version: 1.0',
      'Content-Type: multipart/alternative; boundary="alt-boundary"',
      '',
      '--alt-boundary',
      'Content-Type: text/plain; charset=utf-8',
      '',
      'Hi,',
      '',
      'Your mail-tester.com score is 10/10.',
      '',
      'Click here to download your PDF report: https://www.mail-tester.com/report.pdf',
      '',
      '--alt-boundary',
      'Content-Type: text/html; charset=utf-8',
      '',
      HEAD_STYLED_HTML,
      '',
      '--alt-boundary--',
      '',
    ].join('\r\n'),
  },
  {
    from: 'DMARC Report <reports@example.org>',
    subject: 'EasyDMARC Download ready',
    body: [
      'MIME-Version: 1.0',
      'Content-Type: text/html; charset=utf-8',
      '',
      UNCLOSED_HEAD_HTML,
      '',
    ].join('\r\n'),
  },
  {
    from: 'Billing <billing@example.com>',
    subject: 'Invoice 2043',
    body: [
      'MIME-Version: 1.0',
      'Content-Type: text/plain; charset=utf-8',
      '',
      'Your invoice is attached to this message. Please pay it before the 30th.',
      '',
    ].join('\r\n'),
  },
];

/** One SMTP transaction. */
function deliver(message, index) {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection({ host: '127.0.0.1', port });
    let buffer = '';
    let step = 0;
    const stamp = new Date().toUTCString();
    const messageId = `<seed-${index}-${Date.now()}@example.test>`;
    const headers = [
      `From: ${message.from}`,
      `To: <${recipient}>`,
      `Subject: ${message.subject}`,
      `Date: ${stamp}`,
      `Message-ID: ${messageId}`,
    ].join('\r\n');
    const data = `${headers}\r\n${message.body}`;

    const script = [
      { send: 'EHLO seed.example.test', expect: '250' },
      { send: 'MAIL FROM:<sender@example.net>', expect: '250' },
      { send: `RCPT TO:<${recipient}>`, expect: '250' },
      { send: 'DATA', expect: '354' },
      { send: `${data.replace(/\r\n\./g, '\r\n..')}\r\n.`, expect: '250' },
      { send: 'QUIT', expect: '221' },
    ];

    socket.setEncoding('utf8');
    // `step` is the command whose reply is being awaited. The banner is unsolicited,
    // so step 0 is "await 220", and each later step sends on entry.
    socket.on('data', (chunk) => {
      buffer += chunk;
      const lines = buffer.split('\r\n');
      buffer = lines.pop() || '';
      const last = lines.filter(Boolean).pop() || '';
      const current = script[step];
      if (!current) return;
      if (!last.startsWith(current.expect)) return;
      step += 1;
      const next = script[step];
      if (!next) {
        socket.end();
        resolve();
        return;
      }
      socket.write(`${next.send}\r\n`);
    });
    script.unshift({ send: '', expect: '220' });
    socket.on('error', reject);
    socket.on('close', resolve);
    setTimeout(() => {
      socket.destroy();
      reject(new Error('smtp timeout'));
    }, 15000);
  });
}

for (const [index, message] of MESSAGES.entries()) {
  await deliver(message, index);
  console.log(`delivered: ${message.subject}`);
}
