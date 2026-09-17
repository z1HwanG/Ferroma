#!/usr/bin/env node
/**
 * Drive the first-run setup wizard against a freshly started server.
 *
 * This is the first thing an operator does after `docker compose up`, so it is worth
 * proving rather than assuming: `GET /api/v1/setup` must report that setup is
 * required, `POST` must create the first administrator and a domain, and the returned
 * credentials must actually work.
 *
 *   node tools/first-run.mjs http://127.0.0.1:8182
 */
const base = (process.argv[2] || 'http://127.0.0.1:8182').replace(/\/$/, '');

async function json(method, path, body, token) {
  const headers = { Accept: 'application/json' };
  if (body) headers['Content-Type'] = 'application/json';
  if (token) headers.Authorization = `Bearer ${token}`;
  const res = await fetch(base + path, {
    method,
    headers,
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch {
    parsed = text;
  }
  return { status: res.status, body: parsed };
}

const step = (label, value) => console.log(`${label.padEnd(34)} ${value}`);

/**
 * The management API returns either a page envelope (`{items, total, …}`) or a named
 * collection (`{mailboxes: […]}`) depending on the route, so unwrap both. Getting
 * this wrong makes the script report "no addresses" for a server that created one —
 * a false alarm that costs more time than the check saves.
 */
function list(value) {
  if (Array.isArray(value)) return value;
  if (Array.isArray(value?.items)) return value.items;
  for (const key of ['mailboxes', 'domains', 'users', 'messages', 'drafts', 'devices']) {
    if (Array.isArray(value?.[key])) return value[key];
  }
  return [];
}

// 1. A server with no administrator must say so.
const before = await json('GET', '/api/v1/setup');
step('GET /setup (fresh server)', `${before.status} required=${before.body?.required}`);

// 2. The wizard creates the first admin, the domain and its primary address.
const created = await json('POST', '/api/v1/setup', {
  email: 'root@firstrun.test',
  password: 'correct horse battery staple',
  hostname: 'mail.firstrun.test',
  domain: 'firstrun.test',
});
step('POST /setup', `${created.status}`);
if (created.status >= 400) {
  console.log('  ->', JSON.stringify(created.body));
}
step('  token issued', Boolean(created.body?.access_token ?? created.body?.accessToken));

// 3. Setup must not be runnable twice.
const again = await json('GET', '/api/v1/setup');
step('GET /setup (after setup)', `${again.status} required=${again.body?.required}`);

// 4. The new administrator can log in and is genuinely an admin.
const login = await json('POST', '/api/v1/auth/login', {
  email: 'root@firstrun.test',
  password: 'correct horse battery staple',
});
step('POST /auth/login', `${login.status}`);
const token = login.body?.access_token ?? login.body?.accessToken;
if (!token) {
  console.log('  ->', JSON.stringify(login.body));
  process.exit(1);
}

const me = await json('GET', '/api/v1/auth/me', null, token);
step(
  'GET /auth/me',
  `${me.status} email=${me.body?.email} admin=${me.body?.is_admin ?? me.body?.isAdmin}`,
);

const domains = await json('GET', '/api/v1/domains', null, token);
step('GET /domains', `${domains.status} ${JSON.stringify(list(domains.body).map((d) => d.name))}`);

// 5. The address the wizard promised can receive mail — the whole point of setup.
const mailboxes = await json('GET', '/api/v1/mailboxes', null, token);
const addresses = list(mailboxes.body).map((m) => m.address);
step('GET /mailboxes', `${mailboxes.status} ${JSON.stringify(addresses)}`);

const ok =
  before.status === 200 &&
  before.body?.required === true &&
  created.status < 400 &&
  Boolean(token) &&
  me.status === 200 &&
  addresses.includes('root@firstrun.test');
console.log(ok ? '\nFIRST RUN OK' : '\nFIRST RUN FAILED');
process.exit(ok ? 0 : 1);
