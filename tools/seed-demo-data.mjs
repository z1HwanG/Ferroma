/**
 * Populate a development instance with a demo dataset: accounts, addresses, aliases,
 * nested folders, drafts and a couple of dozen messages delivered through real SMTP.
 *
 * Development helper, safe to re-run — everything it creates is looked up first.
 *
 *   node tools/seed-demo-data.mjs [api-base] [smtp-port] [admin-email] [admin-password]
 *
 * Defaults: http://127.0.0.1:8183, 2530, admin@example.test, AdminPassw0rd!
 *
 * Messages arrive through the MX port so they take the whole path a real delivery takes:
 * the queue, the Maildir, the unread counters and the sync journal. What the API is used
 * for is what SMTP cannot do — accounts, addresses, aliases, folders, drafts.
 */

import net from 'node:net';

const API = (process.argv[2] || 'http://127.0.0.1:8183').replace(/\/$/, '');
const SMTP_PORT = Number(process.argv[3] || 2530);
const ADMIN_EMAIL = process.argv[4] || 'admin@example.test';
const ADMIN_PASSWORD = process.argv[5] || 'AdminPassw0rd!';

const DOMAIN = 'example.test';

/** Accounts to make sure exist. Passwords are printed at the end. */
const ACCOUNTS = [
  { email: `alice@${DOMAIN}`, password: 'AlicePassw0rd!', display_name: 'Alice Zhang', admin: false },
  { email: `bob@${DOMAIN}`, password: 'BobPassw0rd!', display_name: 'Bob Li', admin: false },
  { email: `carol@${DOMAIN}`, password: 'CarolPassw0rd!', display_name: 'Carol Wang', admin: false },
];

/** Folders for alice, nested where the name contains a slash. */
const FOLDERS = ['项目', '项目/2026', '财务', '待办'];

/**
 * One alias forwards to exactly one address — `POST /domains/:id/aliases` takes a single
 * `target` and the local part is unique — so a "team" that reaches two people is two
 * aliases, or a target that is itself a group.
 */
const ALIASES = [
  { local_part: 'sales', target: `alice@${DOMAIN}` },
  { local_part: 'team', target: `alice@${DOMAIN}` },
  { local_part: 'support', target: `bob@${DOMAIN}` },
];

let token = '';

async function api(path, { method = 'GET', body, expect = [200, 201, 204] } = {}) {
  const response = await fetch(`${API}/api/v1${path}`, {
    method,
    headers: Object.assign(
      { 'Content-Type': 'application/json' },
      token ? { Authorization: `Bearer ${token}` } : {},
    ),
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  const payload = text ? JSON.parse(text) : null;
  if (!expect.includes(response.status)) {
    throw new Error(`${method} ${path} → ${response.status}: ${text.slice(0, 300)}`);
  }
  return payload;
}

/**
 * Sign in, and keep that token for the calls that follow.
 *
 * Which account is signed in decides what the mailbox-scoped endpoints answer: folders,
 * drafts and message flags belong to the *owner*, and an administrator asking for
 * somebody else's mailbox gets a `404` — "not yours" is reported as "not found" on
 * purpose. So this script acts as the administrator to create accounts, addresses and
 * aliases, and as each account to fill its own mailbox.
 */
async function loginAs(email, password) {
  const payload = await api('/auth/login', { method: 'POST', body: { email, password } });
  token = payload.access_token;
}

/** The signed-in account's own mailboxes. */
async function ownMailboxes() {
  const payload = await api('/mailboxes');
  return payload.items || payload.mailboxes || [];
}

/** The signed-in account's primary mailbox id. */
async function ownPrimaryMailbox() {
  const boxes = await ownMailboxes();
  const primary = boxes.find((box) => box.is_primary) || boxes[0];
  if (!primary) throw new Error('the signed-in account has no address');
  return primary.id;
}

/* ------------------------------------------------------------------ accounts */

async function ensureAccounts() {
  const existing = new Map(
    (await api('/users?limit=200')).items.map((user) => [user.email, user]),
  );
  for (const account of ACCOUNTS) {
    if (existing.has(account.email)) {
      console.log(`user        ${account.email} (already there)`);
      continue;
    }
    const created = await api('/users', {
      method: 'POST',
      body: {
        email: account.email,
        password: account.password,
        display_name: account.display_name,
        is_admin: account.admin,
      },
    });
    existing.set(account.email, created);
    console.log(`user        ${account.email} created`);
  }
  return existing;
}

/**
 * Give alice an address on the user's own domain.
 *
 * `younglee.cn` was created from the console with no address on it, which is a domain that
 * cannot receive anything — the exact state that makes "the new domain cannot send or
 * receive" look like a bug.
 */
async function ensureOwnDomainAddress(users) {
  const domains = (await api('/domains')).items;
  const owned = domains.find((domain) => domain.name !== DOMAIN);
  if (!owned) return null;

  const alice = users.get(`alice@${DOMAIN}`);
  const addresses = (await api(`/users/${alice.id}/mailboxes`)).items;
  const onOwnDomain = addresses.find((mailbox) => mailbox.address.endsWith(`@${owned.name}`));
  if (onOwnDomain) {
    console.log(`address     ${onOwnDomain.address} (already there)`);
    return onOwnDomain;
  }
  const created = await api(`/users/${alice.id}/mailboxes`, {
    method: 'POST',
    body: { domain: owned.name, local_part: 'lee', is_primary: false },
  });
  console.log(`address     ${created.mailbox.address} added to alice`);
  return created.mailbox;
}

async function ensureAliases() {
  const domains = (await api('/domains')).items;
  const domain = domains.find((entry) => entry.name === DOMAIN);
  const existing = (await api(`/domains/${domain.id}/aliases`)).items || [];
  for (const alias of ALIASES) {
    const already = existing.some(
      (entry) =>
        (entry.localPart ?? entry.local_part) === alias.local_part &&
        (entry.target ?? entry.targets?.[0]) === alias.target,
    );
    if (already) {
      console.log(`alias       ${alias.local_part}@${DOMAIN} → ${alias.target} (already there)`);
      continue;
    }
    await api(`/domains/${domain.id}/aliases`, {
      method: 'POST',
      body: { local_part: alias.local_part, target: alias.target },
    });
    console.log(`alias       ${alias.local_part}@${DOMAIN} → ${alias.target}`);
  }
}

async function ensureFolders() {
  await loginAs(`alice@${DOMAIN}`, 'AlicePassw0rd!');
  const mailboxId = await ownPrimaryMailbox();
  const existing = new Set(
    (await api(`/mailboxes/${mailboxId}/folders`)).folders.map((folder) => folder.name),
  );
  for (const name of FOLDERS) {
    if (existing.has(name)) {
      console.log(`folder      ${name} (already there)`);
      continue;
    }
    await api(`/mailboxes/${mailboxId}/folders`, { method: 'POST', body: { name } });
    console.log(`folder      ${name}`);
  }
}

async function ensureDrafts(users) {
  const drafts = [
    {
      owner: `alice@${DOMAIN}`,
      subject: '关于 2026 年 Q1 排期的几点想法',
      text: '先写一半，晚点补上预算表。\n\n需要确认：\n1. 交付时间\n2. 人手\n',
    },
    {
      owner: `bob@${DOMAIN}`,
      subject: 'Draft: vendor comparison',
      text: 'Three vendors replied. Cheapest is not the fastest.\n',
    },
  ];
  for (const draft of drafts) {
    const account = ACCOUNTS.find((entry) => entry.email === draft.owner);
    await loginAs(account.email, account.password);
    const mailboxId = await ownPrimaryMailbox();
    await api('/drafts', {
      method: 'POST',
      body: {
        mailbox_id: mailboxId,
        subject: draft.subject,
        text: draft.text,
        to: [`bob@${DOMAIN}`],
      },
    });
    console.log(`draft       ${draft.owner} — ${draft.subject}`);
  }
}

/* ------------------------------------------------------------------ messages */

const PNG_1PX =
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg==';

function attachmentPart(filename, contentType, base64) {
  return [
    '--mixed-boundary',
    `Content-Type: ${contentType}; name="${filename}"`,
    'Content-Transfer-Encoding: base64',
    `Content-Disposition: attachment; filename="${filename}"`,
    '',
    base64.replace(/(.{76})/g, '$1\r\n'),
    '',
  ].join('\r\n');
}

const HTML_PAGE = (title, inner) =>
  [
    '<!DOCTYPE html>',
    '<html><head><meta charset="utf-8">',
    '<style type="text/css">body{margin:0;padding:24px;font-family:-apple-system,Segoe UI,Roboto,sans-serif;color:#1b1f24}',
    `h1{font-size:20px;margin:0 0 12px}.muted{color:#6b7280;font-size:13px}table{border-collapse:collapse;width:100%}th,td{border:1px solid #e5e7eb;padding:8px;text-align:left;font-size:13px}th{background:#f4f6f9}</style>`,
    `</head><body><h1>${title}</h1>${inner}</body></html>`,
  ].join('\r\n');

const INVOICE_TABLE = [
  '<p class="muted">Invoice 2043 · due 30 Sep 2026</p>',
  '<table><tr><th>Item</th><th>Qty</th><th>Amount</th></tr>',
  '<tr><td>Mail hosting, annual</td><td>1</td><td>¥ 1,280.00</td></tr>',
  '<tr><td>Extra storage, 50 GB</td><td>2</td><td>¥ 300.00</td></tr>',
  '<tr><td><strong>Total</strong></td><td></td><td><strong>¥ 1,880.00</strong></td></tr></table>',
  '<p class="muted">Attachment: invoice-2043.pdf</p>',
].join('');

/** The batch. `to` defaults to alice; `from` is the envelope sender. */
const MESSAGES = [
  {
    from: 'Billing <billing@example.com>',
    subject: 'Invoice 2043 — 请查收',
    to: `alice@${DOMAIN}`,
    body: [
      'MIME-Version: 1.0',
      'Content-Type: multipart/mixed; boundary="mixed-boundary"',
      '',
      '--mixed-boundary',
      'Content-Type: text/html; charset=utf-8',
      '',
      HTML_PAGE('Invoice 2043', INVOICE_TABLE),
      '',
      attachmentPart('invoice-2043.pdf', 'application/pdf', 'JVBERi0xLjQKJcfsj6IKMSAwIG9iago8PC9UeXBlL0NhdGFsb2c+PgplbmRvYmoK'),
      attachmentPart('screenshot.png', 'image/png', PNG_1PX),
      '--mixed-boundary--',
      '',
    ].join('\r\n'),
  },
  {
    from: 'Score Report <mail-tester@example.net>',
    subject: 'Your Score Report PDF export is ready (10/10)',
    to: `alice@${DOMAIN}`,
    body: [
      'MIME-Version: 1.0',
      'Content-Type: multipart/alternative; boundary="alt-boundary"',
      '',
      '--alt-boundary',
      'Content-Type: text/plain; charset=utf-8',
      '',
      'Your mail-tester.com score is 10/10.',
      'https://www.mail-tester.com/report.pdf',
      '',
      '--alt-boundary',
      'Content-Type: text/html; charset=utf-8',
      '',
      HTML_PAGE('Score 10/10', '<p>你的 SPF、DKIM 与 DMARC 全部通过。</p><p><a href="https://www.mail-tester.com/report.pdf">Download the report</a></p>'),
      '',
      '--alt-boundary--',
      '',
    ].join('\r\n'),
  },
  {
    from: '李经理 <manager@partner.example.cn>',
    subject: '关于 2026 年 Q1 交付排期的确认',
    to: `alice@${DOMAIN}`,
    body: [
      'MIME-Version: 1.0',
      'Content-Type: text/plain; charset=utf-8',
      '',
      'Alice 你好，',
      '',
      '附件里是 Q1 的排期草案，主要变化有三点：',
      '1. 迁移窗口从 1 月 10 日推迟到 1 月 17 日，避开春节前的封网；',
      '2. 灰度比例从 5% 提到 20%，需要你确认容量；',
      '3. 回滚演练提前到上线前一周。',
      '',
      '辛苦确认一下第 2 点，其他我这边先按这个走。',
      '',
      '—— 李',
    ].join('\r\n'),
  },
  {
    from: 'Weekly Digest <digest@news.example.com>',
    subject: 'This week in self-hosting: 12 links',
    to: `alice@${DOMAIN}`,
    body: [
      'MIME-Version: 1.0',
      'Content-Type: text/html; charset=utf-8',
      '',
      HTML_PAGE(
        'This week in self-hosting',
        '<ul><li><a href="https://example.com/a">Running your own MX in 2026</a></li>' +
          '<li><a href="https://example.com/b">DKIM key rotation without downtime</a></li>' +
          '<li><a href="https://example.com/c">Why bounce handling is the hard part</a></li></ul>' +
          '<p class="muted">You are receiving this because you subscribed.</p>',
      ),
      '',
    ].join('\r\n'),
  },
  {
    from: 'Prize Department <winner@lottery.example.top>',
    subject: '恭喜！您已被选中领取 ¥ 88,888',
    to: `alice@${DOMAIN}`,
    body: [
      'MIME-Version: 1.0',
      'Content-Type: text/html; charset=utf-8',
      '',
      HTML_PAGE('CONGRATULATIONS', '<p>Click <a href="http://claim-prize.example.top/now">here</a> within 24 hours.</p>'),
      '',
    ].join('\r\n'),
  },
  {
    from: 'IT Support <support@example.test>',
    subject: 'Re: VPN 连接不上（已解决）',
    to: `alice@${DOMAIN}`,
    body: [
      'MIME-Version: 1.0',
      'Content-Type: text/plain; charset=utf-8',
      '',
      '已经修好了：配置文件里的地址还是旧的。',
      '',
      '> 我从昨天开始连不上 VPN，提示 “server not responding”。',
      '',
      '—— Support',
    ].join('\r\n'),
  },
  {
    from: 'Bob Li <bob@example.test>',
    subject: 'Re: 明天的评审',
    to: `alice@${DOMAIN}`,
    body: [
      'MIME-Version: 1.0',
      'Content-Type: text/plain; charset=utf-8',
      '',
      '我这边 10 点可以，会议室订 3 楼那个。',
      '',
      '带上上次的性能数据吧，省得再问一遍。',
    ].join('\r\n'),
  },
  {
    from: 'Notification <no-reply@ci.example.com>',
    subject: '[ferroma/api] Pipeline #1284 failed',
    to: `alice@${DOMAIN}`,
    body: [
      'MIME-Version: 1.0',
      'Content-Type: text/plain; charset=utf-8',
      '',
      'Build 1284 failed on the step “integration tests”.',
      '',
      'Log: https://ci.example.com/ferroma/api/1284/logs?step=integration&lines=200#L173',
      '',
      '-- ',
      'CI Bot',
    ].join('\r\n'),
  },
  {
    from: 'HR <hr@example.test>',
    subject: '年假余额提醒（2026 年度）',
    to: `alice@${DOMAIN}`,
    body: [
      'MIME-Version: 1.0',
      'Content-Type: text/html; charset=utf-8',
      '',
      HTML_PAGE('年假余额', '<p>你今年剩余 <strong>7.5</strong> 天年假，请在本季度内安排。</p>'),
      '',
    ].join('\r\n'),
  },
  {
    from: 'A very long signature <newsletter@long.example.org>',
    subject: '设计系统周报 · 第 42 期',
    to: `alice@${DOMAIN}`,
    body: [
      'MIME-Version: 1.0',
      'Content-Type: text/plain; charset=utf-8',
      '',
      '本期内容：',
      '· 图标网格的 0.5px 对齐问题',
      '· 深色模式下阴影的正确做法',
      '· 一个关于焦点环颜色的争论',
      '',
      '——',
      '本邮件由 Ferroma 开发实例投递，用于测试排版与滚动行为。',
      '这一行故意写得很长，用来观察纯文本邮件在窄窗格里的换行表现，以及在深色主题下正文颜色的可读性是否足够。',
      '再补一行，确保邮件高度超过视口，从而触发正文区域自身的滚动条。',
    ].join('\r\n'),
  },
  {
    from: 'Bob Li <bob@example.test>',
    subject: '报价单请查收',
    to: `bob@${DOMAIN}`,
    body: [
      'MIME-Version: 1.0',
      'Content-Type: multipart/mixed; boundary="mixed-boundary"',
      '',
      '--mixed-boundary',
      'Content-Type: text/plain; charset=utf-8',
      '',
      '这是最终报价，含税。',
      '',
      attachmentPart('quote.png', 'image/png', PNG_1PX),
      '--mixed-boundary--',
      '',
    ].join('\r\n'),
  },
  {
    from: 'Alice Zhang <alice@example.test>',
    subject: 'Fwd: 供应商对比',
    to: `bob@${DOMAIN}`,
    body: [
      'MIME-Version: 1.0',
      'Content-Type: text/plain; charset=utf-8',
      '',
      '转给你看看，第三家看起来最稳。',
      '',
      '---------- Forwarded message ----------',
      'From: procurement@example.com',
      'Subject: Vendor comparison',
      '',
      'A: cheapest, slowest. B: middle. C: fastest, 12% more.',
    ].join('\r\n'),
  },
  {
    from: 'Carol Wang <carol@example.test>',
    subject: '测试邮件：看看附件预览',
    to: `carol@${DOMAIN}`,
    body: [
      'MIME-Version: 1.0',
      'Content-Type: multipart/mixed; boundary="mixed-boundary"',
      '',
      '--mixed-boundary',
      'Content-Type: text/html; charset=utf-8',
      '',
      HTML_PAGE('附件预览', '<p>两张图，一张 png 一张被声明成 pdf。</p>'),
      '',
      attachmentPart('chart.png', 'image/png', PNG_1PX),
      '--mixed-boundary--',
      '',
    ].join('\r\n'),
  },
  {
    from: 'Spam King <promo@spam.example.biz>',
    subject: 'URGENT: your mailbox will be closed',
    to: `alice@${DOMAIN}`,
    body: [
      'MIME-Version: 1.0',
      'Content-Type: text/plain; charset=utf-8',
      '',
      'Verify your account immediately or it will be deleted.',
    ].join('\r\n'),
  },
];

/* ---------------------------------------------------------------------- smtp */

/** One SMTP transaction against the MX port. */
function deliver(message, index) {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection({ host: '127.0.0.1', port: SMTP_PORT });
    let buffer = '';
    let step = 0;
    const headers = [
      `From: ${message.from}`,
      `To: <${message.to}>`,
      `Subject: ${message.subject}`,
      `Date: ${new Date().toUTCString()}`,
      `Message-ID: <demo-${index}-${Date.now()}@${DOMAIN}>`,
    ].join('\r\n');
    const data = `${headers}\r\n${message.body}`;
    const script = [
      { send: 'EHLO seed.demo', expect: '250' },
      { send: 'MAIL FROM:<sender@example.net>', expect: '250' },
      { send: `RCPT TO:<${message.to}>`, expect: '250' },
      { send: 'DATA', expect: '354' },
      { send: `${data.replace(/\r\n\./g, '\r\n..')}\r\n.`, expect: '250' },
      { send: 'QUIT', expect: '221' },
    ];
    socket.setEncoding('utf8');
    socket.on('data', (chunk) => {
      buffer += chunk;
      const lines = buffer.split('\r\n');
      buffer = lines.pop() || '';
      const last = lines.filter(Boolean).pop() || '';
      const current = script[step];
      if (!current || !last.startsWith(current.expect)) return;
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
      reject(new Error(`smtp timeout on "${message.subject}"`));
    }, 15000);
  });
}

/* ---------------------------------------------------------------------- main */

async function main() {
  await loginAs(ADMIN_EMAIL, ADMIN_PASSWORD);
  const users = await ensureAccounts();
  await ensureOwnDomainAddress(users);
  await ensureAliases();
  await ensureFolders();
  await ensureDrafts(users);

  for (const [index, message] of MESSAGES.entries()) {
    await deliver(message, index);
    console.log(`delivered   ${message.to} ← ${message.from}: ${message.subject}`);
  }

  // Star a couple, so the flagged style is visible without clicking anything. Back to
  // alice: flags are set by the owner of the mailbox they are in.
  await loginAs(`alice@${DOMAIN}`, 'AlicePassw0rd!');
  const mailboxId = await ownPrimaryMailbox();
  const folders = (await api(`/mailboxes/${mailboxId}/folders`)).folders;
  const inbox = folders.find((folder) => folder.name === 'INBOX');
  const list = await api(`/messages?folder_id=${inbox.id}&limit=50`);
  for (const message of list.items.slice(0, 2)) {
    await api(`/messages/${message.id}`, { method: 'PATCH', body: { flagged: true } });
  }
  console.log(`starred     ${Math.min(2, list.items.length)} messages in alice's inbox`);

  console.log('\nSign in with:');
  for (const account of ACCOUNTS) {
    console.log(`  ${account.email.padEnd(24)} ${account.password}`);
  }
  console.log(`  ${ADMIN_EMAIL.padEnd(24)} ${ADMIN_PASSWORD}   (administrator)`);
  console.log(`\nWebmail  ${API}/`);
  console.log(`Console  ${API}/admin/`);
}

main().catch((error) => {
  console.error(`\nseeding failed: ${error.message}`);
  process.exitCode = 1;
});
