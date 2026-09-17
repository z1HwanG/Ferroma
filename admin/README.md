# Ferroma Admin (`admin/`)

The operator console (specification §36). A static single-page app served by the
Rust server as plain files: **no build step, no npm, no bundler, no framework, no
CDN**. Vanilla ES modules, hand-written CSS, relative asset paths only — which
matters here because this app is served from `/admin`, so `/styles.css` would miss.

---

## 1. File map

```text
admin/
  index.html             95 lines  shell: sidebar navigation, topbar, login card, modal + toast hosts
  styles.css            903 lines  tokens, light/dark themes, tables, stat grid, badges, responsive
  main.js               352 lines  boot, sign-in, health poll, section navigation, view dispatcher
  ui.js                 192 lines  view heads, state-driven cards, tables, pagers, badges, clipboard
  api.js                229 lines  fetch wrapper: bearer token, error envelope, 401 refresh, offline flag
  data.js               390 lines  normalisation of every API payload the console reads
  dom.js                172 lines  createElement/textContent helpers (never assigns innerHTML)
  format.js             225 lines  RFC 3339 → local time, byte sizes, log stamps
  modal.js              226 lines  dialogs: focus trap, Escape, click-outside, focus restore
  toast.js               36 lines  polite + assertive live regions
  theme.js               63 lines  prefers-color-scheme with a localStorage override
  net.js                 18 lines  AbortSignal timeout helper
  router.js              55 lines  `#/<section>?<params>` sections
  store.js               26 lines  signed-in operator, health snapshot, queue-depth history
  views/dashboard.js    337 lines  health / queue stats / storage + browser-sampled SVG sparkline
  views/domains.js      361 lines  domain CRUD + DNS Health panel + DKIM record with copy button
  views/users.js        410 lines  user list/search/paging, create/edit/delete, per-user addresses
  views/aliases.js      234 lines  aliases per domain
  views/queue.js        211 lines  queue filter, retry, cancel, per-attempt delivery log
  views/audit.js        138 lines  audit log with actor/action/since filters
  views/settings.js     189 lines  DB-backed settings
  views/setup.js        150 lines  first-run setup wizard
  tools/check.mjs       410 lines  the static regression check (see §4)
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
| `GET` | `/api/v1/storage` | dashboard storage figures |
| `GET` | `/api/v1/audit` | Audit log (`actor_user_id`, `action`, `since`, paging) |
| `GET` | `/api/v1/settings` | Settings |
| `PUT` | `/api/v1/settings/:key` | edit a setting |

### Payload shapes this app assumes

The console normalises through `data.js`, so a differently-shaped payload degrades
to a fallback instead of `undefined`. Assumptions worth confirming on the Rust side:

* **Queue entry** — `{id, recipient, sender, subject, message_id, status, attempts,
  next_attempt_at, last_error, created_at, updated_at}`.
* **Attempt log** — `GET /queue/:id` returns the entry plus its attempts under
  `attempts` (aliases accepted: `log`, `history`, `delivery_log`, `entries`), each
  `{at, status, smtp_code?, message, host?}`.
* **Settings** — a flat object, a `{settings: {…}}` wrapper, or a list of
  `{key, value}`; all three render. Values are written back through
  `PUT /settings/:key` with `{value}`.
* **Audit entry** — `{at, actor_user_id, actor?, action, target?, detail?, ip?}`.

### Endpoints `docs/api.md` does not define

Nothing in the console was invented, but three specification §36 line items have no
documented endpoint and are therefore **not** implemented as separate views. They
are named here rather than faked:

* **System logs** — only `GET /api/v1/audit` exists; there is no process/application
  log stream. The Audit log section is the closest documented thing.
* **Devices** — `docs/api.md` exposes devices only on the client surface
  (`GET /api/v1/client/devices`), which is bearer-only FCP. The console does not
  call it; a management-side device list would need `GET /api/v1/devices`.
* **Active client sessions / online clients** — not in the `GET /api/v1/health`
  payload of §2. The dashboard shows a card reading “—” with the note that the API
  does not report it, rather than printing a fake zero. `GET /api/v1/storage` also
  does not carry user/domain counts, so those two dashboard cards read “—” unless
  the server adds them.

## 4. The static check

There is no browser and no bundler here, so `tools/check.mjs` **is** the test
suite. Run it from this directory:

```powershell
node tools/check.mjs
```

Same rules as the Webmail copy (see `web/README.md` §4), minus the Webmail-only
`srcdoc` assertion; the app profile comes from `<html data-app="admin">` and this
run makes **601 assertions**. The checker also fails on an import that names a
non-existent export, on an unused export, on a `byId('…')` target that neither the
HTML nor the module itself declares, on any `fetch` outside `api.js`, and on any
`request(...)` path that does not start with `${API_BASE}` — which is how the
wiring in §3 stays honest.

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
* `POST /api/v1/storage/gc` is not wired to a button — it is a maintenance action
  with no documented progress reporting, so it was left out rather than shipped
  blind.
