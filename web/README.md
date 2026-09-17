# Ferroma Webmail (`web/`)

The browser mail client (specification §35). A static single-page app served by the
Rust server as plain files: **no build step, no npm, no bundler, no framework, no
CDN**. Vanilla ES modules, hand-written CSS, relative asset paths only.

---

## 1. File map

```text
web/
  index.html          204 lines  app shell: three panes, login card, modal + toast hosts
  styles.css         1108 lines  tokens, light/dark themes, responsive panes, prefers-reduced-motion
  main.js             530 lines  boot, auth, routing, keyboard map, bulk-action bar
  api.js              229 lines  fetch wrapper: bearer token, error envelope, 401 refresh, offline flag
  data.js             390 lines  normalisation of every API payload the app reads
  dom.js              172 lines  createElement/textContent helpers (never assigns innerHTML)
  format.js           225 lines  RFC 3339 dates, relative stamps, sizes, quote/text↔HTML
  modal.js            226 lines  dialogs: focus trap, Escape, click-outside, focus restore
  toast.js             36 lines  polite + assertive live regions
  theme.js             63 lines  prefers-color-scheme with a localStorage override
  net.js               18 lines  AbortSignal timeout helper
  store.js             83 lines  observable state + localStorage preferences
  router.js           100 lines  `#/f/<folder>[/m/<id>]` and `#/search/<query>`
  login.js             87 lines  `POST /auth/login` panel
  folders.js          174 lines  address picker + folder tree with unread counts
  list.js             382 lines  paginated list, infinite scroll, selection, bulk bar glue
  reader.js           476 lines  reading pane, sandboxed HTML frame, 2 s auto-mark-read
  compose.js          533 lines  compose modal: chips, rich text, uploads, reply/forward
  settings.js         109 lines  settings dialog
  tools/check.mjs     410 lines  the static regression check (see §4)
```

`api.js`, `data.js`, `dom.js`, `format.js`, `modal.js`, `net.js`, `theme.js` and
`toast.js` are duplicated verbatim in `admin/`. That is deliberate: the server
serves `web/` at `/` and `admin/` at `/admin` as two independent directories, so a
shared parent path would not be reachable from either app.

## 2. Serving it

```powershell
# from the repository root, after `cargo build --bin ferroma`
.\target\debug\ferroma.exe serve --config .\config\ferroma.toml
```

`config/ferroma.toml` has `[api] host = "0.0.0.0"`, `port = 8080` and the built-in
Webmail assets. Open:

```text
http://127.0.0.1:8080/            Webmail
http://127.0.0.1:8080/admin       Admin console (see admin/README.md)
```

In the production image the same files are baked in at
`/usr/share/ferroma/web` and the command is
`docker run … ferroma serve --config /etc/ferroma/ferroma.toml` (the image's
`CMD`), reachable on port 8080.

To point the server at this working copy while developing, uncomment in
`config/ferroma.toml`:

```toml
[api]
webmail_dir = "./web"
admin_dir = "./admin"
```

## 3. Endpoints wired

Management API (`docs/api.md`). Webmail authenticates with the `ferroma_session`
cookie the browser flow sets; a bearer token from `POST /auth/login` is also sent
when one is in `localStorage`.

| Method | Path | Used for |
|---|---|---|
| `POST` | `/api/v1/auth/login` | sign in (no `device_name`, so the cookie flow is used) |
| `POST` | `/api/v1/auth/logout` | sign out |
| `POST` | `/api/v1/auth/refresh` | one retry after a `401` on any other request |
| `GET` | `/api/v1/auth/me` | the signed-in user and the addresses it may send from |
| `GET` | `/api/v1/mailboxes` | send-from addresses (read from `auth/me.mailboxes`) |
| `GET` | `/api/v1/mailboxes/:id/folders` | folder tree, `unseen_count`, `special_use` |
| `POST` | `/api/v1/mailboxes/:id/folders` | "New folder" |
| `GET` | `/api/v1/messages` | list page: `mailbox_id`, `folder_id`, `query`, `limit`, `offset` |
| `GET` | `/api/v1/messages/:id` | full message for the reading pane |
| `POST` | `/api/v1/messages` | send, and save a draft |
| `PATCH` | `/api/v1/messages/:id` | `{seen}` mark read/unread, `{flagged}` star |
| `POST` | `/api/v1/messages/:id/move` | move to folder, archive |
| `DELETE` | `/api/v1/messages/:id` | move to Trash; `?permanent=true` deletes |
| `POST` | `/api/v1/messages/batch` | bulk `read`/`unread`/`flag`/`unflag`/`move`/`delete` |
| `POST` | `/api/v1/attachments` | compose attachments (`multipart/form-data`, progress via XHR) |
| `GET` | `/api/v1/attachments/:id` | download from the reading pane |

### Payload shapes this app assumes

`docs/api.md` fixes the endpoints but not every field name inside a list item. The
UI reads through `data.js`, which normalises the plausible spellings, so a
differently-named field degrades to a sensible fallback instead of `undefined`.
These are the guesses the Rust side should confirm:

* **Folder** — `{id, name, parent?, subscribed?, message_count, unseen_count, special_use}`.
  `special_use` accepts IMAP form (`\Sent`) and plain (`Sent`); the folder *name* is
  never used to decide the icon.
* **Message (list item)** — `{id, subject, from, to, date, snippet, seen, flagged,
  has_attachments, size_bytes, folder_id}`. `flags` may be an IMAP flag list instead
  of the booleans, and `date` may arrive as `internal_date` / `received_at`.
* **Message (detail)** — adds `text`, `html`, `cc`, `bcc`, `attachments[]`. For
  correct reply threading the detail response must expose the RFC 5322 header as
  `message_id_header` (accepted aliases: `rfc_message_id`, `message_id_header_value`)
  plus `in_reply_to` and `references`; when they are absent the reply is sent
  without `in_reply_to`/`references` rather than with invented values.
* **Attachment** — `{id, filename, content_type, size_bytes, sha256}`.

### Endpoints `docs/api.md` does not define

* **Drafts.** `POST /api/v1/client/drafts` belongs to the desktop client and
  `docs/api.md` §5.2 has no draft route for the management API. "Save draft"
  therefore posts to `POST /api/v1/messages` with the composed message plus
  `draft: true` and no recipients, and expects the server to file it in the
  Drafts folder. **If `ferroma-api` rejects that flag the button needs a real
  endpoint.** Nothing else in the app depends on it: the Drafts folder is opened
  and listed through the ordinary message endpoints.

## 4. The static check

There is no browser and no bundler here, so `tools/check.mjs` **is** the test
suite. Run it from this directory:

```powershell
node tools/check.mjs
```

It parses every HTML and JS file and asserts, with **581 assertions** over this
app: no inline `<script>` bodies, no `on*=` attributes, no absolute `/…` asset
paths, every `byId('x')` / `getElementById('x')` / `querySelector('#x')` target
declared in `index.html` or created by that module, every `fetch(` confined to
`api.js`, every `request(...)` / `download(...)` call starting with `${API_BASE}`
or `/api/v1`, every local `src`/`href`/`url()` present on disk, every ES-module
import resolvable, no `console.log`/`eval`, `innerHTML` touched only on the
compose editor, every import naming a real export, and every export actually
used. It exits non-zero and prints a `[rule] file: message` list on violation. A
second copy lives in `admin/tools/check.mjs` (**601 assertions**) and takes the
app profile from `<html data-app="…">`.

## 5. Keyboard and accessibility

| Key | Action |
|---|---|
| `c` | compose |
| `/` | focus search |
| `j` | focus the message list |
| `↑` / `↓` | move the active row |
| `Space` | select the active row |
| `Enter` | open the active row |
| `Esc` | close the account menu, then leave the reading pane |

The list is a `role="listbox"` of `role="option"` rows, every action is a real
`<button>` or `<form>`, dialogs carry `role="dialog"` + `aria-modal`, trap Tab,
close on `Escape` and on a click outside, and restore focus. Toasts live in a
polite and an assertive `aria-live` region. Long subjects, senders and folder
names truncate with an ellipsis and carry the full value in `title` and
`aria-label`. Animations are disabled under `prefers-reduced-motion: reduce`.

## 6. Known limitations

* New mail is not pushed: the app has no WebSocket yet, so a folder refreshes when
  you open it or act on a message.
* Message bodies are never injected as markup. The text part goes into a `<pre>`
  via `textContent`; the HTML part is rendered only inside
  `<iframe sandbox="" srcdoc=…>` and sized from its own `scrollHeight`. If the
  server serves the app behind a CSP that forbids `srcdoc`, the HTML part must be
  fetched as text and set the same way instead.
* Attachment downloads go through `fetch` (so the credentials are attached) and are
  handed to the browser as a blob. Direct `<a href="/api/v1/attachments/:id">` links
  would also work **because the session is cookie-based**; the fetch path is used so
  a bearer-token session behaves identically.
* Compose allows attachments up to 25 MB in this UI
  (`client.attachment_chunk_size` chunked uploads in `docs/fcp.md` §6 are a client
  protocol feature and are not used by Webmail).
