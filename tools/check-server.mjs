#!/usr/bin/env node
/** Confirm a running Ferroma serves both front-ends. */
const base = process.argv[2] || 'http://127.0.0.1:8180';
const checks = [
  ['/', 'webmail'],
  ['/admin', 'admin'],
  ['/api/v1/health', null],
  ['/.well-known/ferroma', null],
];
for (const [path, app] of checks) {
  try {
    const res = await fetch(base + path);
    const body = await res.text();
    const ok = app ? body.includes(`data-app="${app}"`) : res.ok;
    console.log(
      `${path.padEnd(22)} ${String(res.status).padEnd(4)} ${ok ? 'ok' : 'WRONG BODY'}${
        app ? ` (data-app=${app})` : ''
      }`,
    );
  } catch (err) {
    console.log(`${path.padEnd(22)} ERR  ${err.message}`);
  }
}
