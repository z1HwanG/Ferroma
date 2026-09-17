# Ferroma Admin (`admin/`)

The operator console (specification §36). A static single-page app served by the
Rust server as plain files: **no build step, no npm, no bundler, no framework, no
CDN**. Vanilla ES modules, hand-written CSS, relative asset paths only — which
matters here because this app is served from `/admin`, so `/styles.css` would miss.

---

## 1. File map

```text
admin/
  index.html             107 lines  shell: sidebar navigation, topbar, login card, modal + toast hosts
  styles.css             903 lines  tokens, light/dark themes, tables, stat grid, badges, responsive
  main.js                399 lines  boot, sign-in, health poll, section navigation, view dispatcher
  ui.js                  213 lines  view heads, state-driven cards, tables, pagers, badges, clipboard
  api.js                 262 lines  fetch wrapper: bearer token, error envelope, 401 refresh, offline flag
  data.js                492 lines  normalisation of every API payload the console reads
  dom.js                 187 lines  createElement/textContent helpers (never assigns innerHTML)
  format.js              243 lines  RFC 3339 → local time, byte sizes, log stamps
  modal.js               254 lines  dialogs: focus trap, Escape, click-outside, focus restore
  toast.js                45 lines  polite + assertive live regions
  theme.js                73 lines  prefers-color-scheme with a localStorage override
  net.js                  19 lines  AbortSignal timeout helper
  router.js               66 lines  `#/<section>?<params>` sections
  store.js                29 lines  signed-in operator, health snapshot, queue-depth history
  views/dashboard.js     368 lines  health / queue stats / storage + browser-sampled SVG sparkline
  views/domains.js       390 lines  domain CRUD + DNS Health panel + DKIM record with copy button
  views/users.js         449 lines  user list/search/paging, create/edit/delete, per-user addresses
  views/aliases.js       252 lines  aliases per domain
  views/queue.js         230 lines  queue filter, retry, cancel, per-attempt delivery log
  views/logs.js          188 lines  System Logs: the in-process ring, with level/target/message filters
  views/storage.js       203 lines  Storage: bytes on disk, row counts, garbage collection
  views/devices.js       229 lines  Devices: every installation, filter by owner, revoke
  views/tls.js           243 lines  TLS: configured PEM files as the host sees them, and the ports
  views/audit.js         153 lines  audit log with actor/action/since filters
  views/settings.js      207 lines  DB-backed settings
  views/setup.js         164 lines  first-run setup wizard
  tools/check.mjs        462 lines  the static regression check (see §4)
```

Eight modules — `api.js`, `data.js`, `dom.js`, `format.js`, `modal.js`, `net.js`,
`theme.js`, `toast.js` — are duplicated verbatim from `web/`. That is deliberate:
the two apps are separate directories served independently, so a shared parent
path would be reachable from neither.

## 2. Serving it

```powershell
# from the repository root, after `cargo build --bin ferroma`
.\target\debug\ferroma.exe serve --config .\config\ferroma.toml
```

With `config/ferroma.toml` defaults (`[api] host = "0.0.0.0"`, `port = 8080`):

```text
http://127.0.0.1:8080/admin       Admin console
http://127.0.0.1:8080/            Webmail
```

In the production image the files are baked in at `/usr/share/ferroma/admin` and
the command is `docker run … ferroma serve --config /etc/ferroma/ferroma.toml`
(the image's `CMD`).

To serve this working copy directly, set in `config/ferroma.toml`:

```toml
[api]
admin_dir = "./admin"
```

The console signs in at its own card (`POST /api/v1/auth/login`), stores the token
pair in `localStorage`, and sends `Authorization: Bearer …` on every request. A
`401` triggers a single `POST /api/v1/auth/refresh`; if that fails the sign-in card
returns.

## 3. Endpoints wired

| Method | Path | View |
|---|---|---|
| `POST` | `/api/v1/auth/login` | sign-in card |
| `POST` | `/api/v1/auth/logout` | account menu |
| `POST` | `/api/v1/auth/refresh` | retry after a `401`, once |
| `GET` | `/api/v1/auth/me` | identity + admin check (a non-admin is refused) |
| `GET` | `/api/v1/health` | dashboard, sidebar status, 30 s poll |
| `GET` | `/api/v1/version` | sidebar footer |
| `GET` | `/api/v1/setup` | wizard gate (`required: true`) |
| `POST` | `/api/v1/setup` | create the first admin |
| `GET` | `/api/v1/domains` | Domains, Aliases picker, address dialog |
| `POST` | `/api/v1/domains` | create domain |
| `PATCH` | `/api/v1/domains/:id` | enable/disable, description |
| `DELETE` | `/api/v1/domains/:id` | delete; `?force=true` after a `409` |
| `GET` | `/api/v1/domains/:id/dns` | DNS Health panel |
| `GET` | `/api/v1/domains/:id/dkim` | DKIM record + copy button |
| `POST` | `/api/v1/domains/:id/dkim` | generate the key pair |
| `GET` | `/api/v1/domains/:id/aliases` | Aliases |
| `POST` | `/api/v1/domains/:id/aliases` | create alias |
| `PATCH` | `/api/v1/aliases/:id` | retarget, enable/disable |
| `DELETE` | `/api/v1/aliases/:id` | delete alias |
| `GET` | `/api/v1/users` | Users (`query`, `limit`, `offset`) |
| `POST` | `/api/v1/users` | create user |
| `GET` | `/api/v1/users/:id/mailboxes` | per-user addresses |
| `POST` | `/api/v1/users/:id/mailboxes` | create address |
| `PATCH` | `/api/v1/users/:id` | edit, enable/disable, set password |
| `DELETE` | `/api/v1/users/:id` | delete (typed confirmation) |
| `GET` | `/api/v1/queue` | Mail queue (`status`, `limit`, `offset`) |
| `GET` | `/api/v1/queue/:id` | per-attempt delivery log |
| `GET` | `/api/v1/queue/stats` | dashboard counts + queue-depth sparkline |
| `POST` | `/api/v1/queue/:id/retry` | retry a failed entry |
| `DELETE` | `/api/v1/queue/:id` | cancel an entry |
| `GET` | `/api/v1/logs` | System Logs (`level`, `target`, `query`, `since`, paging) |
| `GET` | `/api/v1/storage` | Storage figures, and the dashboard cards |
| `POST` | `/api/v1/storage/gc` | "Collect garbage", behind a confirmation |
| `GET` | `/api/v1/devices` | Devices (`user_id`, `platform`, `include_revoked`, paging) |
| `POST` | `/api/v1/devices/:id/revoke` | revoke one device and its sessions |
| `GET` | `/api/v1/tls` | TLS: config, PEM files, listener ports |
| `GET` | `/api/v1/audit` | Audit log (`actor_user_id`, `action`, `since`, paging) |
| `GET` | `/api/v1/settings` | Settings |
| `PUT` | `/api/v1/settings/:key` | edit a setting |

### Payload shapes this app assumes

The console normalises through `data.js`, so a differently-shaped payload degrades
to a fallback instead of `undefined`. The collection key is the exception: the server
pages with `{items, total}` everywhere the console lists rows, and `listOf()` accepts
`{items}` / `{data}` / `{mailboxes}` / `{folders}` / a bare array. Rule 10 of
`tools/check.mjs` feeds each envelope to the app's own normalisers, so a renamed
collection fails the check rather than the browser.

* **Queue entry** — `{id, recipient, sender, subject, message_id, status, attempts,
  next_attempt_at, last_error, created_at, updated_at}`.
* **Attempt log** — `GET /queue/:id` returns the entry plus its attempts under
  `attempts` (aliases accepted: `log`, `history`, `delivery_log`, `entries`), each
  `{at, status, smtp_code?, message, host?}`.
* **Settings** — a flat object, a `{settings: {…}}` wrapper, or a list of
  `{key, value}`; all three render. Values are written back through
  `PUT /settings/:key` with `{value}`. The API answers `{items, total}`.
* **Audit entry** — `{at, actor_user_id, actor?, action, target?, detail?, ip?}`.
* **Log entry** — `{at, level, target, message, fields}`, where `fields` is a flat
  object of stringified values. The page also carries `buffer_entries`,
  `buffer_capacity` and `oldest_at`.
* **Device** — `{id, user_id, email, device_uid, name, platform, client_version,
  protocol_version, last_seen_at, last_ip, created_at, revoked}`.
* **Dashboard cards** — every lookup lives in `data.js#dashboardStats`, including the
  nested ones (`health.queue.received_today`, `health.queue.sent_today`,
  `health.clients.active_sessions`), so rule 10 can prove all twelve without a
  browser. Reading any of those a level too high leaves the card on “—”, which reads
  as “this API does not report it”.

### Sections the specification asks for

All twelve §36 areas now have a screen. Five of them are panels inside another
section rather than a top-level entry of their own, which is a deliberate choice and
not an omission:

| §36 area | Where it lives |
|---|---|
| Dashboard, Domains, Users, Aliases, Mail Queue, System Logs, Storage, Devices, Settings | their own section |
| Delivery Logs | per-attempt log inside a queue entry |
| DNS Diagnostics | the DNS Health panel inside a domain |
| Mailboxes | the per-user addresses dialog inside Users |

Specification §4.3 also names **TLS**; §36 does not. It has its own section here
because `GET /api/v1/tls` exists (see `docs/api.md` §4.9), and it reports the
configured PEM files, whether this process can read them, and which ports offer TLS.

Two things that section deliberately does not show, both documented in `docs/api.md`
§4.9: the private key is never fingerprinted, and the certificate's expiry is not
parsed — this build links no X.509 parser, and an expiry derived from a file mtime
would be worse than no answer.

## 4. The static check

There is no browser and no bundler here, so `tools/check.mjs` **is** the test
suite. Run it from this directory:

```powershell
node tools/check.mjs
```

Same rules as the Webmail copy (see `web/README.md` §4), minus the Webmail-only
`srcdoc` assertion; the app profile comes from `<html data-app="admin">` and this
run makes **795 assertions**. The checker also fails on an import that names a
non-existent export, on an unused export, on a `byId('…')` target that neither the
HTML nor the module itself declares, on any `fetch` outside `api.js`, on any
`request(...)` path that does not start with `${API_BASE}` — which is how the
wiring in §3 stays honest — and, in rule 10, on any collection envelope the server
sends that the app's own normalisers do not unwrap. The two `check.mjs` files are
byte-identical.

## 5. Accessibility

Sections are `<button>`s in a `<nav>` with `aria-current`; every table has a
header row with `scope="col"`; dialogs carry `role="dialog"` + `aria-modal`, trap
Tab, close on `Escape` and on a click outside, and restore focus; loading, empty
and error states are announced (`role="status"` / `role="alert"`); the topbar
status and every toast live in live regions. The sidebar collapses to a drawer
below 860 px. Animations are disabled under `prefers-reduced-motion: reduce`.

## 6. Known limitations

* The sparkline samples the queue depth **in the browser** (one point per 30 s poll,
  at most 40 points, held in memory) — a reload starts the line again. That is the
  intended behaviour: the API exposes no historical series.
* Destructive actions use the shared confirmation dialog. Deleting a user or an
  alias requires typing the exact address, and a domain delete that the server
  refuses with `409` offers a second, explicit `?force=true` confirmation.
* `POST /api/v1/storage/gc` runs behind a confirmation and then reports what it
  freed, from the response's own counters. It cannot report *progress*: the endpoint
  answers once, when the sweep is done.
* The TLS section does not parse the certificate, so it cannot show an expiry date.
  Compare the fingerprint it prints against `openssl x509 -fingerprint -sha256 -noout`
  on the host. Adding expiry means linking an X.509 parser, which this build does not
  do — see `docs/api.md` §4.9.
* The System Logs ring is in-process and **lost on restart**. It holds the most recent
  1000 events, and it **captures** at the level the server was configured with
  (`server.log_level`), so a deployment running at `info` shows `INFO` and above —
  which is what keeps the panel from being permanently blank on a healthy host. The
  `Severity` filter is a floor *within* what was captured; it cannot recover events the
  ring never held.
