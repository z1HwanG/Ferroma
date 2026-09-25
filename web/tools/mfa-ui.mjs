#!/usr/bin/env node
/**
 * Regression check for the Webmail second-factor settings.
 *
 * The dialog itself needs a browser; the two pieces of it that carry rules rather
 * than markup do not. Both are asserted here:
 *
 *   * the status line must never describe a `pending` enrollment as protection —
 *     a secret the user never proved is not a second factor, and a screen that said
 *     otherwise would be the most dangerous kind of wrong;
 *   * a secret must be grouped the way an authenticator's manual-entry field wants
 *     it, spaces and all, because it is read off the screen and typed elsewhere.
 *
 * Run from the repository root: `node web/tools/mfa-ui.mjs`.
 */

import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');

const failures = [];
let checks = 0;

function check(condition, message) {
  checks += 1;
  if (!condition) failures.push(message);
}

const source = readFileSync(resolve(ROOT, 'web', 'settings.js'), 'utf8');

/* ------------------------------------------------------------------ source */

// The status line must distinguish all three states, and the pending one must not
// claim the account is protected.
check(
  /state === 'enabled'/.test(source) && /state === 'pending'/.test(source),
  'securityStatusLine must handle enabled and pending explicitly',
);
check(
  /Waiting for a code to confirm the new secret\./.test(source),
  'the pending state needs its own wording',
);
// The recovery codes and a new application password are shown once; the UI has to
// say so, and it must not hide the pane in the same breath as showing the codes.
check(
  /Save these recovery codes now\. Each one works once, and they are not shown again\./.test(source),
  'the one-time recovery codes need to say they are shown once',
);
check(
  /Copy this password now\. It is not shown again/.test(source),
  'a new application password needs to say it is shown once',
);
check(
  /The codes stay on screen[\s\S]{0,120}paneSticky = true;/.test(source),
  'confirming must keep the pane — and therefore the codes — on screen',
);

// Disabling costs the account password, and the request must not be replayed
// through the refresh path: a wrong password is a 401 too.
check(
  /retryOn401: false/.test(source),
  'the disable request must not treat a wrong password as an expired session',
);
check(
  /auth\/totp\/disable[\s\S]{0,400}password/.test(source),
  'disabling must send the account password',
);

/* ------------------------------------------------------------------ helpers */

// Reproduce the two pure helpers by loading the module with a minimal `t`.
const module = await import(
  `data:text/javascript,${encodeURIComponent(
    source
      .replace(/^import .*$/gm, '')
      .replace(/t\(\s*'([^']*)'\s*(?:,\s*\{[^}]*\})?\s*\)/g, (_match, key) => JSON.stringify(key)),
  )}`
);

const pending = module.securityStatusLine('pending');
const enabled = module.securityStatusLine('enabled');
const disabled = module.securityStatusLine('disabled');
check(pending !== enabled && enabled !== disabled && pending !== disabled,
  'the three states must read differently');
check(!/protecting/.test(pending), `a pending enrollment must not read as protection: ${pending}`);
check(pending !== disabled, 'a pending enrollment must not read like "off"');
check(/application password/.test(enabled), `enabled must point clients at an app password: ${enabled}`);

check(module.groupSecret('MZXW6YTBOI') === 'MZXW 6YTB OI', 'a secret is grouped in fours');
check(module.groupSecret('  mzxw6ytboi  ') === 'mzxw 6ytb oi', 'grouping trims and keeps case');
check(module.groupSecret('') === '', 'an empty secret stays empty');
check(module.groupSecret(null) === '', 'a missing secret does not throw');

/* ------------------------------------------------------------- the QR code */

// A code that cannot be drawn must disappear rather than leave a stale picture
// from the previous attempt on screen, and the secret text stays either way.
check(
  /function showQr\(/.test(source) && /setHidden\(node, true\)/.test(source),
  'a URI that cannot be drawn hides the picture instead of leaving the previous one',
);
check(
  /showQr\(qr, payload\.uri\)/.test(source),
  'enrollment renders the otpauth URI as a QR code',
);
check(
  /Scan this code with your authenticator app\./.test(source),
  'the QR code needs a caption saying what to do with it',
);
check(
  /Or enter this secret by hand\./.test(source),
  'the manual secret stays, for a camera that cannot read the picture',
);

/* ------------------------------------------------------------------ report */

if (failures.length) {
  process.stdout.write(`FAIL — second-factor settings (${checks} assertions)\n`);
  for (const failure of failures) process.stdout.write(`  ${failure}\n`);
  process.exit(1);
}
process.stdout.write(`PASS — second-factor settings (${checks} assertions)\n`);
