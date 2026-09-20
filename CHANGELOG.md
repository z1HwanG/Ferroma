# Changelog

All notable changes to Ferroma, newest first.

Every released version is a `vX.Y.Z` tag whose value must equal `Cargo.toml`'s, because
`.github/workflows/docker-publish.yml` refuses a tag that disagrees with the manifest —
so a tag cannot label a tree it did not build. Until 1.0 a minor bump may change
anything and a patch bump fixes it.

The headings follow [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) in spirit:
a section a release has nothing for is left out rather than written empty.

The Chinese translation is at [`CHANGELOG_zh.md`](CHANGELOG_zh.md).

## [Unreleased]

## [0.1.5] — 2026-09-20

### Changed

- **The interface follows the system's light or dark setting until someone chooses otherwise.**
  The theme default was light; it is now `match system`, so a desktop that has been dark all day
  does not get a white mail client. That reverses the decision recorded in 0.1.1 — "a UI that
  turns black because somebody's laptop is set up that way" — which is a fair argument about a
  decision nobody made, and this is about the first visit, where the system's setting is the best
  evidence there is. The toggle and the picker's three choices are unchanged, and an explicit
  choice still wins and is remembered.

- **The first-run wizard applies what it collected, by itself.** The hostname, the public URL,
  the HTTP listen address and the TLS material are read once at boot — a running process cannot
  move its own socket or re-read a PEM — so the wizard stores them and the server now comes back
  up to adopt them instead of printing "restart the container and they take effect". It replaces
  its own process image (`exec`, so a container keeps its ports, its volumes and the same PID 1),
  and the console waits for it to answer and reloads into it; if the platform cannot do that, the
  page says so and the container's restart policy finishes the job. A wizard that ends by asking
  the person who just filled it in to go and restart something is a wizard they cannot tell
  worked.

- **Setting up happens at one address: `/`.** While an instance still owes an administrator —
  no database, or a database with no administrator in it — `/` serves the Admin console, and
  the console shows whichever step is outstanding: the form that asks for the connection and
  the one-time code, then the first-run wizard. Before this, the database step was at `/admin/`
  and, once the database was accepted, a half-set-up instance left `/` to the Webmail: its
  sign-in box cannot work yet (it answers its own login request `405`), so an operator who
  opened the mail hostname met a page that looked broken while the step they owed sat one path
  away — and the two steps of initialisation were two addresses. `/admin/` serves the same
  console throughout (earlier releases printed that URL) and `/shared` is unmoved, so this is
  one application mounted twice; once an administrator exists, `/` is the Webmail that a mail
  hostname is for. `crates/ferroma-api/src/router.rs` now names the choice (`RootApp`) and why
  the app mounted at `/` must be the one whose *directory* is mounted there, and the console's
  own pages finish by navigating to `/admin/` rather than reloading `/`.

### Fixed

- **The panel that reports a front-end which did not start now speaks the chosen language.**
  `shared/diag.js` held its text in English on purpose — it is a classic script loaded before
  the apps, so it cannot read the language catalog — which meant a console set to Chinese
  reported its worst failure in English, the one message an operator is least able to interpret.
  It carries a two-entry table instead and applies the catalog's own rule (the stored choice
  wins, English is the default), while staying dependency-free.

- **The Webmail no longer keeps an address that names a different application.** The hash is
  how both front-ends say what is on screen, and they use different shapes: the Webmail's are
  `#/f/<folder>` and `#/search/…`, the console's are `#/<section>` under `/admin/`. A URL that
  mixed them — `/#/admin/`, which nothing in the product produces but a person can type — loaded
  the Webmail, which showed the inbox while the address bar named the console: `web/router.js`
  promises the opposite in its own opening paragraph. An unrecognised hash is now replaced, in
  the address bar, with the route that is actually being shown.

- **A fresh installation could land on the sign-in page instead of the first-run wizard.**
  The console asks `GET /api/v1/setup` to decide which page to show, and the moment the
  database address is accepted — when the page reloads — the process is still switching from
  the bootstrap router to the real API. In that window the request was answered by the
  front-end's SPA fallback: the Webmail's HTML with a `200`. The console reported "a response
  that is not JSON" and read it as *no wizard*, so the operator was shown a sign-in box
  claiming an administrator already existed, on an installation that had just been told to
  create its first one; only a hard reload got past it, which is not something to ask of an
  operator. The bootstrap router now answers every unknown API path with the same JSON `404`
  the running server uses, so an API request can never receive a page, and the console's two
  boot probes retry an answer they cannot parse instead of treating it as a decision.

- **The image shipped six static files that the service user could not read, and the
  Webmail and Admin rendered a blank page.** `COPY` preserves the mode of the file it
  copies, and a checkout can legitimately hold a source file at `0600` — some editors and
  agent tools write that way, and git records only the exec bit, so nothing in review
  shows it. The service runs as uid `10001`, so `shared/i18n.js`, `shared/diag.js`,
  `shared/locales/zh-CN.js`, `admin/icons.js`, `admin/views/bootstrap.js` and
  `admin/views/first-run.js` answered **404** through the static file server; the
  front-end's ES module graph failed to load and the page stayed empty, with nothing in
  the server log to connect the blank screen to a file mode. The `Dockerfile` now
  normalises what it ships (`chmod -R a+rX /usr/share/ferroma /etc/ferroma`) instead of
  trusting the umask of whichever machine built it, and `tools/check-deploy.mjs` fails on
  a static file that is not world-readable — a bind-mounted checkout gets no protection
  from the Dockerfile. 0.1.3 shipped the same defect.

## [0.1.4] — 2026-09-20

### Added

- **A custom folder can be dragged somewhere else.** Dragging one folder onto the *middle* of
  another moves it inside that one — a real move: `PATCH /folders/:id` now takes a
  `parent_id` (`null` for the top level), and the server re-paths the folder and every
  descendant, moves the Maildir directories, and refuses a move that would put a folder
  inside its own subtree. Dragging onto a row's *edge* keeps the existing reorder, and
  dragging onto the tree's empty space lifts a folder back to the top level. Until now the
  gestures were judged by which parent the target had, so two folders at the same level could
  only ever swap places — nesting one under the other was impossible from the UI.


- **A server with no database now serves the page that asks for one.** It used to exit with
  `cannot connect to PostgreSQL` — no page, no console, nothing to click. It now binds its web
  port anyway, prints a one-time setup code to the log, and serves one page: the database
  address and that code. On submit the connection is tested, the schema applied, the address
  written to `<data_dir>/database.json` (0600) and **the boot continues in the same process on
  the same port** — no restart — so the page reloads straight into the first-run wizard. The
  database must already exist; Ferroma never runs `CREATE DATABASE`.
  Leaving `DATABASE_URL` unset is what asks for this; stating it, as everywhere else, wins —
  and a *stated* address that fails is still a loud startup error, while a *remembered* one
  that fails brings the page back with the reason, so a mistyped password is fixable in the
  browser.


- **The first-run wizard is the installation's configuration surface.** It collects what an
  operator used to pass as flags or environment variables — the mail domain, the server
  hostname, the public URL, the API listen address and port, and TLS (switch, certificate,
  private key) — and stores them; the server adopts whatever the deployment left at its
  default on the next start. `GET /api/v1/setup` reports every one of those fields so the
  form prefills itself, and `POST /api/v1/setup` answers with exactly what it stored.
- **`scripts/deploy.sh --wizard` asks for the web port and nothing else.** The mail domain,
  MX hostname, first administrator and TLS are collected on the wizard page once the stack
  is up. The script writes none of `FERROMA_HOSTNAME`, `FERROMA_PUBLIC_URL` or
  `FERROMA_JWT_SECRET` in that mode — stating them is what would make the wizard's answers
  a no-op — and prints the URL to open instead of creating an administrator.
- **The shipped compose files publish the web port.** `docker-compose.prod.yml` now maps
  `${WEB_PORT:-8080}:8080`, and neither it nor `docker-compose.external-db.yml` demands a
  hostname or a public URL any more. A fresh install needs the web port (plus the database
  password `deploy.sh` generates) and nothing else; the mail ports keep their fixed
  defaults because Docker cannot publish one after a container is created.


- **A stored draft can be continued.** Clicking a message in the Drafts folder opens the
  composer with its recipients, subject, body and attachment list already in it, and the
  window is titled "Edit draft"; saving updates that draft (`PATCH /drafts/:id`) and
  sending goes through `POST /drafts/:id/send`, so the record is not left behind as a copy
  of what was sent. It used to open in the reading pane, showing a half-written message
  with no way to finish it.
- **Custom folders keep the order the reader gives them.** They sort by creation instead
  of by name — a new folder no longer jumps into the middle of the tree — and a folder can
  be dragged onto a sibling to change that order, which is remembered per address. IMAP has
  no folder order, so the preference lives where the other client preferences do.
- **A link in a message opens a new tab.** The reading pane injects
  `<base target="_blank">` into the frame and the frame's sandbox allows the popup (and
  still denies scripts and same-origin access), so following a link no longer replaces the
  message with the destination and leaves no way back.


- **A folder rename moves everything under it.** Renaming `Projects` to `Work` now
  re-paths its descendants in the database *and* the Maildir, the way IMAP `RENAME` does.
  It also no longer fails with a confusing `404` when the folder's directory happens to be
  missing: the database is the authority on which folders exist, so a missing directory is
  created rather than turned into a refusal the operator could neither see nor fix.
- **The tree shows a folder's own name, not its whole path.** A nested folder's IMAP name
  is `Projects/2026`, and the tree printed that in full, so every child repeated its
  parent. The row now shows the last segment; `folder.name` stays the full path for every
  operation that has to send it.


- **The two front-ends keep their house style, and now have a design system behind it.**
  A shared token set (surfaces, elevation, radii, a duration/easing scale, a focus ring)
  drives both apps; the polish that used to be ad hoc — a hover here, a shadow there —
  is now one visual layer per stylesheet, and `prefers-reduced-motion` switches all of it
  off.
- **Multi-select in the Webmail is something you can see, and it is always there.** Every
  row carries a checkbox, the list header has a select-all with an indeterminate state,
  and the selection bar above the list is permanent — 43px of icon buttons that are simply
  disabled until something is selected. Ctrl/Shift-click and Space still work; they were
  the *only* way to select more than one message, and a control nobody can see is not a
  feature.
- **An animated theme switch in both apps.** The Webmail's top bar gained a sun/moon
  toggle, the Admin console's was rebuilt, and the Webmail's Settings picker is now a
  three-way segmented control with a glyph per choice (follow the system / light / dark).
  Changing the theme cross-fades every surface for ~400ms; the icon rotates through the
  change rather than blinking.
- **Icons where they carry meaning.** The Webmail's reading-pane actions, folder toolbar
  and compose/search bars have glyphs; the Admin console's stat tiles have icon
  medallions, its sortable headers show a direction chevron, its status pills have a
  tone dot, and its table row actions (details / edit / password / enable / delete …)
  are labelled *and* drawn.


- **The first-run wizard is reachable without a session, and it is the entry point to a
  fresh install.** It used to live behind the Admin console's own login panel, which
  cannot be passed until an administrator exists — so the wizard that creates one could
  never be opened, and the only way to finish an install was to hand-edit the
  environment. The Admin console now renders `#/setup` for an unauthenticated visitor
  when `GET /setup` reports `{required:true}` and offers no other section; the Webmail
  sends a fresh install there instead of showing a login panel whose credentials
  cannot exist yet.
- **The wizard covers the hostname and the public URL, and the server adopts what it
  stored.** Submitting a hostname (`server.hostname`) or public URL (`api.public_url`)
  that differs from the running configuration writes it to the `settings` table instead
  of answering `400`, and `server/src/serve.rs` reads both back at startup. Precedence is
  `ferroma.toml` / environment, then the stored row, then the built-in default: a
  deployment that states its hostname explicitly is never overridden. `POST /setup`
  reports what it stored in a new `applied` object.
- **`api.jwt_secret` is provisioned and kept when it is not configured.** An unset secret
  was generated at boot and discarded, which logged every session out on every restart
  and made `FERROMA_JWT_SECRET` a startup item nobody could skip; it is now written to
  `<data_dir>/jwt_secret` (owner-only) and reused. A configured secret still wins.
- **`POST /users` creates the account's primary address, its Maildir and its standard
  folders** when the address's domain already exists — what `ferroma user create` has
  always done. The response carries the created addresses in `mailboxes`; an empty list
  means the domain is missing, and the console says so instead of reporting success. An
  account made in the console used to be able to log in to the Webmail and find no
  folder to open, no address to send from and nothing that worked.
- **Nested folders are real.** `POST /mailboxes/:id/folders` with a `Parent/Child` name
  creates the missing parents and records each row's `parent_id` (IMAP `CREATE` does the
  same), and the Webmail renders the tree as nested lists. The name has always been the
  full IMAP path; the console had no way to draw the hierarchy it implied.


- **The tab title names the open view.** The Admin console's was `Ferroma Admin` on every
  section, and the Webmail's was always `Ferroma Webmail`: the sidebar and the folder pane
  said where you were, and the browser tab — the one surface that answers the question
  across ten open tabs — did not. Both now read `<section> · <app>`, translated, from the
  `SECTIONS[].title` the console had been carrying unused.

### Changed

- **Both interfaces start in English and in light.** The default locale no longer follows
  `navigator.languages` and the default theme no longer follows `prefers-color-scheme`: one
  instance is reached by several people, and a UI that changes language or turns black
  because somebody's laptop is set up that way is a UI nobody can give instructions for. The
  language picker (on the login card and in Settings) and the theme toggle are one click
  away, and "match system" is still one of the theme choices.
- **The explanatory lines under every section title were rewritten in a technical register.**
  "Who did what, and when" is now "Administrative actions, with actor and timestamp";
  "Mail domains hosted by this server" is "Domains this server accepts and delivers mail for";
  and the rest of the eleven were brought in line with them.


- **Setup is no longer a permanent sidebar entry.** The console offered "Setup" to an
  installation that had already been set up, where the page could only ever answer "an
  administrator already exists". It appears while no administrator exists (that is how the
  wizard is reached) and disappears afterwards; `#/setup` still resolves for anyone following
  an old link.


- **First-run setup is a page of its own.** No sidebar, no top bar: during setup the
  console has one screen and no session, and it now says so by dropping the chrome, the way
  the login card does. It carries the two controls a first-time reader actually needs
  before they have an account — a language picker and the light/dark switch — and its own
  line of copy is short enough not to strand a word on a second line.
- **`toggleTheme()` flips the scheme you can see.** Every switch used to flip `currentTheme()`,
  which returns the *mode*: from `auto` that meant the first click chose an explicit `dark`,
  a click that visibly did nothing whenever the system was already dark. `resolvedTheme()`
  and `toggleTheme()` now live in `shared/theme.js` and all four switches use them.
- **The DNS verdicts and the wizard's icons come from the icon set.** `admin/icons.js` grew
  the `sun`/`moon` glyphs the wizard's switch needed.


- **The light switch sweeps across the window instead of cross-fading.** Switching theme
  now snapshots the page with the View Transitions API and clips the incoming scheme to a
  circle that grows out of the toggle you clicked — the light *arrives* rather than the
  colours quietly exchanging places. The toggle also throws a ring as it happens, and its
  sun/moon turns for exactly as long as the reveal takes. Browsers without the API keep the
  older cross-fade, and `prefers-reduced-motion` skips both: the switch is simply instant.


- **A domain's DNS verdicts read as cards, not as a five-column table.** A DNS record is
  the wrong shape for a narrow column: a single SPF or DMARC value is longer than the cell
  that held it, so the drawer showed `a:localho / st -all` and `p=quarant / ine` broken
  mid-word beside a horizontal scrollbar. One card per record — badge, verdict, then
  expected / found / hint stacked and allowed to wrap.


- **The loading skeleton is gone.** Three shimmering boxes stood in for messages that did
  not exist yet, which in an empty folder read as three empty messages. The pane keeps its
  own empty text while a page is in flight instead.
- **Clicking the folder that is already open refreshes it.** The hash does not change, so
  no route event fires and nothing reloaded; a second click now re-reads the folder tree
  and the message list.
- **One address is a label, not a picker.** The send-from picker in the folder pane was a
  disabled `<select>` showing "address (primary)" — truncated by the pane, and reading as a
  broken widget rather than as "these folders belong to this address". With a single
  address it is now a plain line; the picker returns, as a control, the moment the account
  owns a second address.
- **The login card has a theme switch, and its top rule no longer runs past the card.**
  The coloured rule is clipped by the card's rounded corners, and the sun/moon toggle sits
  in the corner the top bar's occupies once logged in.
- **The prompt dialog is a dialog.** `promptDialog` was a 560px card around one input with
  a destructive-looking confirm button; it is now a narrow card with a proper label, a
  placeholder that shows the shape of the answer, a line saying what will happen, and a
  primary confirm — with `danger: true` reserved for the two prompts that really delete
  something.


- **The Admin sidebar highlights the section you are actually on.** The highlight was
  refreshed only by the 30-second health poll, so after a click the sidebar pointed at the
  section you had left while the pane showed the one you asked for.
- **The Webmail's top bar no longer duplicates "Settings".** It had its own settings button
  next to the account menu's own entry; the menu entry is the one that stayed, and it now
  also carries an **Admin console** link for administrators.
- **The login card has a language picker.** The only way to change the interface language
  was behind the login, which is no help to a reader who cannot read the login page.
- **HTML mail is legible in dark mode.** A message is authored against white paper, and a
  white rectangle in the middle of a dark client was the one place the theme visibly
  stopped. The reading pane composes the frame's `srcdoc`, so the body is inverted there —
  media is inverted back, keeping photographs and logos in their own colours. A message
  that was authored dark will come out light; that is the trade every client that inverts
  makes.


- **The Webmail's top-left button is gone.** It only ever opened the folder drawer below
  640px, which is what a wide window never needed and what mobile — explicitly out of
  scope — no longer gets. The folder pane is part of the layout at every width, and the
  slot it occupied is now the theme toggle.
- **A message is marked read the moment it is opened.** The two-second delay, and the
  visibility test that came with it, are gone: opening a message *is* reading it, and the
  timer only left rows sitting unread behind a message the reader could plainly see. The
  Settings switch that turns the behaviour off is now labelled "Mark messages as read when
  I open them".


- **The Admin sidebar is always docked.** The collapse toggle and the narrow-screen
  drawer are gone: mobile is out of scope, and a navigation that can disappear makes the
  operator hunt for the button that brings it back.
- **The interface language is applied only when the operator confirms it.** Both pickers
  used to reload the page on `change`, so moving the arrow keys over the list swapped the
  whole interface — in the Webmail before its own Save button was pressed, and in the
  Admin console with no confirmation at all. The Admin picker gained an explicit
  **Apply** button.
- **`GET /setup` also reports `public_url`**, so the wizard can prefill both identity
  fields with the values the server actually advertises.


### Removed

- **The backup and restore sidecars are gone from the Docker deployment.** Neither compose
  file defines a `backup` or a `restore` service any more, the `ferroma-backups` volume is
  gone, and `docker compose up -d` now starts Ferroma and nothing else. `scripts/backup.sh`
  and `scripts/restore.sh` are deleted, and `scripts/deploy.sh` no longer has the `backup`
  or `restore` subcommands (nor does `upgrade` back anything up first). Backups are the
  operator's job now, with the host's own tooling: dump the database and copy the
  `ferroma-data` volume — Maildir, attachments and the DKIM private key — and restore the
  two together, because either half alone loses messages. `docs/deployment.md` §8 lists what
  a backup must contain and the commands to take one. The deployment also loses its second
  PostgreSQL client image pull and the `BACKUP_INTERVAL_SECONDS` / `BACKUP_RETENTION_DAYS`
  settings.

- **A release now publishes `X.Y.Z` and `latest`, and nothing else.** The rolling minor tag
  (`0.1`, `0.2`, …) is no longer created: it was a second name for the same image, and a
  patch release silently rewrote a name an operator may have pinned — `:0.1` moved forward
  on its own. The registry-backed layer cache is gone with it: it published a ~2 GB
  `buildcache` tag into the image repository, visible to every operator and counted against
  that repository's storage while only ever helping the machine that built it. Caching is
  local now (`.cache/buildx`, gitignored) and `--no-cache` skips it.


### Fixed

- **Dragging a folder onto a top-level row asked the server for "folder 0".** `Number(null)`
  is `0`, and the helper that read a folder's parent treated that as a parent id, so an edge
  drop onto a folder at the top level sent `parent_id: 0` and the move answered
  `no such folder` — a message about a folder that does not exist, for a folder that does.
  A parent is now `null` unless it is a positive id, and the API refuses `parent_id: 0` with
  an explanation instead of a `404`, because a client bug should not read like missing data.
- **A drag carries its folder id under its own type, and only that.** A browser fills the
  generic `text/plain` payload with the *selected text* when a drag begins at a selection, so
  a folder named `451` arrived as the string "451", was read as a folder id, and the move
  answered `no such folder`. The id travels as `text/x-ferroma-folder`, is checked against the
  folders actually on screen, and folder labels are no longer selectable drag sources.


- **"Move to the top level" is a button now, not only a drag.** Dragging a nested folder out
  of its parent worked, but it had nowhere obvious to aim at; the folder toolbar gained an
  explicit action (enabled only for a folder that actually has a parent) that does the same
  thing. The drop target that appears while dragging is also shown for every drag, not only
  nested ones.
- **A drop reads which folder was dragged from the drag event itself.** The module kept the id
  in a variable, and `dragend` does not always arrive — a drop outside the window, a cancelled
  drag — so a stale id made the *next* drop move the wrong folder. The id travels in the drag
  data, with the variable as a fallback.


- **A nested folder can be dragged back out.** The move-to-top-level gesture used to be "drop
  it on the empty space below the tree", which is neither visible nor always there; a dashed
  **Move to the top level** target now appears at the end of the tree while a nested folder is
  being dragged. (The target itself was also broken on arrival: it asked for its own element
  with the module's strict `byId`, which throws when the element is absent — this one is absent
  by design until a drag starts, and the exception aborted the drag handler.)
- **Front-end assets are served `Cache-Control: no-cache`.** They are files on disk edited in
  place, and a browser with no caching header is free to keep them heuristically — which is how
  a page keeps running last week's module and a fixed bug appears to still be there. They are
  revalidated on every request now (a 304 on a loopback or LAN connection), and a reload is
  enough.


- **A folder's message count is now maintained by the insert itself.** `messages.insert` only
  bumped `uid_next`; keeping `message_count` / `unseen_count` / `total_bytes` true was each
  caller's job, and delivery was the only caller that remembered. The result was a Drafts
  folder whose row said `0` while five drafts sat in it — visible as a sidebar with no
  counts over folders that opened full of mail. The counters now move in the same
  transaction as the row, and expunging moves them back. (The first attempt put both
  updates in one statement as two data-modifying CTEs; PostgreSQL applied only one of them,
  because a row may be updated once per statement, and the counters *looked* maintained
  while staying wrong — which is why this is two statements in one transaction.)


- **A stored certificate path can no longer stop the server from starting.** TLS material
  saved from the wizard is now adopted only when the files are actually on disk — a volume
  that was not mounted, a typo, rotated material — and a configured PEM that will not open
  falls back to the self-signed certificate with a loud error instead of aborting the boot.
  A refusal to start locked the operator out of the console that would have let them fix
  the path, and the wizard will not store one: it checks that the file exists on the server
  and explains that the path is read by the process, not by the browser.
- **The wizard no longer prints the endpoint it calls** (`GET /api/v1/setup`), and no longer
  repeats a card title above its own heading.


- **The reading pane has one scrollbar, not two.** The pane scrolled as a whole while the
  message frame inside it was pinned at `max(360px, 62vh)`, so a message with an attachment
  strip pushed the total past the pane's height: the pane grew a scrollbar *and* the frame
  grew another one whenever the message was longer than its box. The header and the
  attachment strip are now fixed (the strip bounded and scrollable past 30vh), and the body
  hands the frame exactly the height that is left. A short message with an attachment now
  scrolls nowhere at all; a long message scrolls inside its own frame, and a long
  plain-text message scrolls the body — never both.


- **A dialog opened from a drawer is no longer hidden behind it.** The drawer host and the
  dialog host are both children of `<body>` and both sat at `z-index: 60`, so the drawer —
  appended last — covered the confirmation it had opened: "generate a new DKIM key?" asked
  its question underneath the panel that asked it. Dialogs are now above drawers, and the
  stacking order is written down in one place.


- **A domain the server accepts can be created from the console.** The create-domain
  dialog required at least one dot, while `ferroma_core::address::validate_domain` accepts
  a single label — so `demo`, `localhost` or any internal-only name could not be created
  from the UI at all, and the only report was "enter a valid domain". The dialog now shares
  one validator with the first-run wizard, mirroring the server's rule, and says so
  explicitly when an internationalised name needs its punycode form.


- **"Add address" works again.** The dialog called `table(...)` without importing it from
  `ui.js`, so opening it threw `table is not defined`, the catch printed that as the form's
  error, and the domain list stayed empty — which made adding a *second* address to an
  account impossible from the console. (The first address never needed the dialog: creating
  an account provisions it.)
- **The address picker shows plain addresses.** Appending "(primary)" truncated in a 244px
  folder pane; the account drawer's own address table already marks it.


- **The compose editor shows its lists.** A global reset stripped markers from every
  `ul`/`ol`, which is right for the navigation lists and wrong for a message: a bulleted
  line came out as plain indented text, so the toolbar button looked like it had done
  nothing.
- **The sandbox check asserted the wrong thing.** The front-end check required the message
  frame's `sandbox` attribute to be literally empty, which contradicts `docs/security.md`
  §10.2 — the documented value is `allow-popups`, and it is what lets a link open in a new
  tab. The rule now asserts what the sandbox must *deny* (scripts and same-origin access).


- **Compose's formatting buttons mean what they say.** Bold, italic and underline applied
  to whatever selection the browser was left with — at worst the whole message; the list
  commands did nothing; "Clear" left a bulleted line bulleted; and the link button used
  `window.prompt`, whose dialog cannot be styled and returns focus to a page state where
  the selection is gone, so the link was never applied. The selection is now saved and
  restored around every command, the editor starts with a block so list/quote have
  something to act on, Clear also undoes lists, quotes and links, and the link dialog is
  the app's own.
- **The message list no longer shows three empty skeleton boxes in an empty folder.** The
  loading skeleton was toggled from inside the loader, so a call that returned early — a
  second folder click while the first page was still in flight — left it shimmering for
  good.
- **The global stylesheet no longer reaches markup the app did not create.** The `*`
  scrollbar rules, the focus ring and the theme cross-fade are scoped to this app's own
  containers; a browser extension that injects a panel into the page was inheriting them.


- **The message list no longer scrolls sideways.** A row's snippet was an inline `<span>`,
  and `text-overflow` does nothing to an inline box, so a long preview grew the row to its
  full text width and put a horizontal scrollbar across the bottom of the pane.
- **Moving the selection between two rows repaints them.** The list's repaint signature
  counted the selection but not *which* rows were in it, so a selection of one message
  swapped for another kept the old rows ticked on screen.
- **Chrome's autofill no longer paints a pale-yellow stripe** across a themed form field;
  both apps override `:-webkit-autofill` to match the surface.
- **"Settings" no longer looks like a second theme button.** The gear — a ring with eight
  spokes — was indistinguishable from the sun icon at 18px, and in the Webmail's top bar
  the two sat next to each other. Settings is now the sliders glyph in both apps, and the
  theme toggle is the only sun on screen.


- **Reading a message marks it read in the interface.** `renderList` skipped its repaint
  unless the list's *shape* changed — length, total, selection — and a flag change alters
  none of those, so the `PATCH /seen` succeeded while the row kept its unread dot and the
  sidebar badge kept its count. Starring a message from the reading pane had the same
  problem.
- **The sidebar's unread badge is no longer permanently stale.** `folders.unseen_count`
  is a denormalised counter that delivery and move recomputed but a flag change did not,
  so reading a message left the badge claiming unread mail until the next delivery.
- **An HTML message that omits `</head>` renders.** The sanitiser treated a missing
  closing tag as "drop the rest of the document", which emptied the body of every
  message written that way — HTML permits the omission — and the reader reported "this
  message has no body".
- **A stylesheet no longer becomes the message preview.** The snippet builder stripped
  tags but not the contents of `head`/`style`/`script`/`title`, so an HTML mail with its
  CSS in a `<style>` block previewed as `#outlook a { padding:0; } body { margin:0; …`.
  Entities are decoded too, so `Fish &amp; chips` previews as `Fish & chips`.
- **The HTML reading pane shows the whole message.** A sandboxed frame has an opaque
  origin, so the parent cannot read its content height; the code that tried anyway was a
  no-op and the frame stayed at its 240 px minimum. It now takes a fixed, generous height
  from CSS and scrolls inside itself.
- **The message list loads after logging in.** The route was applied once while there
  was no session, and the second application short-circuited on "same route", so the
  folder named in the URL stayed empty until a *different* folder was clicked.
- **An account with no address says so** instead of showing "No folders yet.", and its
  Compose button (and the `c` shortcut) is disabled rather than opening a form that
  cannot send.
- **The Admin console cannot go permanently blank.** An uncaught error, a rejected
  module import or a boot that never finishes now reveals the login panel with the
  failure printed on it, plus a boot watchdog; the previous behaviour was a white page
  indistinguishable from a server that never answered.


### Known issues carried into 0.1.6

Both items found while cutting 0.1.3 are still open: a published image cannot say which
build it is, and eight documents still carry `_(planned)_` claims written before those
crates existed. They are now tracked in [`TODO.md`](TODO.md) under "Next release
(0.1.6)"; the fix for the first is described in
[README — Carried into 0.1.6](README.md#carried-into-016).

## [0.1.3] — 2026-09-19

### Added

- **Simplified Chinese, next to English, in both front-ends.** English is the source
  language and its text *is* the lookup key, so a string with no translation renders in
  English instead of breaking; `shared/locales/zh-CN.js` holds 881 entries. The
  language picker is in Settings, the choice is remembered per browser, and the API is
  asked for error messages in it.
- **The API localises its error messages** from `Accept-Language` (RFC 9110 q-values).
  `code` stays language-neutral. The message is translated in two halves — the error
  kind and its detail — which is what covers 126 `format!` call sites without threading
  a message key through each of them.
- **`shared/`, one copy of the modules both front-ends use.** The server mounts it at
  `/shared`, so `../shared/api.js` resolves to the same URL from `/main.js` and from
  `/admin/main.js`; `api.shared_dir` names the directory and the image bakes it in.
- `tools/serve-frontends.mjs` — serves `web/`, `admin/` and `shared/` with the same
  three mounts the router creates, for front-end work with no server and no database.
- `ferroma user create --disabled`, matching `POST /users` with `enabled: false`.

### Changed

- `/auth/me` returns full mailbox records rather than the reduced address brief, so a
  client can read each address's quota and current size.
- `GET /users` carries each account's addresses; `GET /audit` resolves the acting
  account's address instead of sending only its id.
- `GET /setup` answers `404` when the wizard is disabled, rather than `200` with
  `required: false` — a client could not tell "disabled" from "an administrator already
  exists". A submitted `hostname` that disagrees with `server.hostname` is `400` naming
  the setting, and is validated before anything is written.
- An unknown `/api/v1` path, and a known path with the wrong method, answer the error
  envelope `docs/api.md` §1.3 promises. An unknown path previously fell through to the
  Webmail's SPA fallback and returned `index.html` with a `200`.
- The Admin console gained a filter bar, a sortable and selectable table, a detail
  drawer and a real section layout; `hidden` now hides what it marks.
- The Webmail's compose dialog can send and save again.

### Fixed

- **DKIM signing runs on outbound mail.** `[dkim]` was fully implemented — key parsing,
  canonicalisation, signing — with no production caller, so a deployment with DKIM
  enabled, a published selector and a matching key still sent every message unsigned.
- The Webmail showed every message as unread and hid every star: the API sends the
  canonical lower-case flag string (`seen flagged`, frozen by `docs/fcp.md` §4) and the
  client matched `\Seen`. The flags are also sent as booleans now.
- `INBOX` was never recognised, because it carries no `special_use` — RFC 6154 defines
  no `\Inbox`. It is identified by name, and standard folders render under a display
  name while `folder.name` stays IMAP data and is never translated.
- `POST /users` silently dropped `enabled`, so the console's "Account enabled" box did
  nothing; clearing a display name reported success and kept the old name.
- Four of the Admin dashboard's five cards rendered the string `undefined`.
- A wrong current password was retried as an expired session; `304 Not Modified` was
  treated as a failure by attachment downloads; badge colours were lost when their
  label was translated before the colour was derived from it.
- `doctor`'s loopback-shadow test asserted Windows' `SO_REUSEADDR` semantics and failed
  on every Linux host.
- `ferroma-api`'s integration harness created its shared test database with a
  check-then-create that raced against every other test in the binary.
- `scripts/docker-publish.sh` is executable, which its own usage examples assume.

### Removed

- `backup/` and `.probe/` — local scratch that was never tracked: a bundle of a commit
  already in history, and a previous audit's throwaway proofs of concept.

## [0.1.2] — 2026-09-17

### Added

- The Admin app is served at `/admin/`, with the directory redirect that makes its
  relative asset URLs resolve inside the Admin directory rather than the Webmail's.

### Fixed

- Both front-ends read the field names the API actually sends, and fill the sections
  that had been shipping empty.
- The front-end static checks run where they are documented to run.

## [0.1.1] — 2026-09-17

### Fixed

- An undefined `renderChrome` reference that stopped the Webmail from starting at all.

## [0.1.0] — 2026-09-17

The first release: SMTP, IMAP and the HTTP API in one process, over PostgreSQL
metadata, a Maildir for message bytes and a content-addressed store for attachments,
with the Webmail and Admin apps and the official client's core and CLI.

### Added

- An outbound relay, for hosts whose IP has no PTR record.
- Docker Hub publishing, driven by a `v*` tag.

### Changed

- The operator scripts are executable.

### Fixed

- `scripts/deploy.sh` fails fast when PostgreSQL is unreachable, can provision a
  containerised PostgreSQL, survives a cluster with no `postgres` database, and asks
  the container who its superuser is instead of assuming.
- `ferroma database init` creates a missing database without assuming a `postgres`
  database exists.
- TLS settings are carried into the IMAP configuration, and `FERROMA_PUBLIC_URL`
  reaches the server under the name the server actually reads.
