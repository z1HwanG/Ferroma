# Changelog

All notable changes to Ferroma, newest first.

Every released version is a `vX.Y.Z` tag whose value must equal `Cargo.toml`'s.
The maintainer builds and publishes the image separately with `scripts/docker-publish.sh`;
pushing the GitHub tag does not publish to Docker Hub. Until 1.0 a minor bump may change
anything and a patch bump fixes it.

The headings follow [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) in spirit:
a section a release has nothing for is left out rather than written empty.

The Chinese translation is at [`CHANGELOG_zh.md`](CHANGELOG_zh.md).

## [Unreleased]

### Added

- **Certificates can be obtained and renewed automatically.** `[tls.acme]` orders a
  certificate over ACME (RFC 8555) with an HTTP-01 challenge, writes the chain and key
  where the listener already reads them, and renews before expiry — at startup and every
  six hours — so a deployment needs no certbot and no renewal cron. Off by default, and
  refused unless `agree_tos` is set. `ferroma tls acme-renew [--dry-run]` runs it on
  demand. `docs/deployment.md` §5.5.1.

- **Search looks inside message bodies.** The search box matched the subject, the
  sender and the stored preview, so a word a few lines into a message found nothing.
  Bodies are extracted as mail arrives (MIME walk, charset decode, HTML reduced to
  text) into a generated `tsvector` with a GIN index, and `?query=` uses it. The
  substring match is kept beside the word match, so a prefix search still works and
  mail stored before body indexing stays findable by subject, sender and snippet until
  `ferroma storage reindex-search` fills in its text. `docs/deployment.md` §6.7.

- **An administrator can see an account's second factor, and cannot clear it.**
  `GET /api/v1/users/:id/security` reports the enrollment state, the recovery codes
  left and the application passwords; `DELETE /api/v1/users/:id/app-passwords/:app_id`
  revokes one. The Admin user drawer shows both. Clearing a second factor is
  deliberately absent from the HTTP surface — one stolen administrator session would
  otherwise bypass every account — so the recovery route for a user who has lost both
  their authenticator and their recovery codes is `ferroma user totp-disable` on the
  host, with `ferroma user totp` to inspect and `ferroma user app-password-revoke` for
  a lost device (`docs/deployment.md` §6.6).

- **The Webmail can set up a second factor.** Settings grew a Security section:
  enroll an authenticator (the secret and its `otpauth://` URI are shown for manual
  entry — no QR image, because rendering one would mean a QR library or a request to
  somebody else's service from a page holding the secret), confirm with a code,
  save the one-time recovery codes, and manage application passwords. Turning the
  factor off asks for the account password.

- **TOTP second factors and application passwords.** An account can enroll a TOTP
  authenticator (`/api/v1/auth/totp/enroll`, confirmed with a code) and receives ten
  single-use recovery codes. Every password login then answers
  `401 totp_required` until the code arrives; a wrong code counts as a failed login,
  so the lockout covers code guessing. Disabling the factor costs the account
  password rather than merely a live session. Because IMAP and SMTP cannot carry a
  code, and an authenticated submission cannot either, those surfaces — and JMAP
  `Basic` — accept an **application password** instead
  (`/api/v1/auth/app-passwords`), so enabling the factor does not lock every mail
  client out. `docs/security.md` §15.3 states what a deployment may claim.

- **Greylisting.** `[policy.greylist]` defers an unauthenticated peer whose
  `(address, sender, recipient)` triplet has never been seen, once, with
  `451 4.7.1`. Off by default. Authenticated sessions, a null reverse-path and
  peers listed in `whitelist` are never deferred, and a database error accepts the
  message rather than refusing it. Triplets are pruned by `ferroma storage gc`.

- **Delivery-report tasks are visible.** `GET /api/v1/health` and
  `GET /api/v1/queue/stats` now count bounce tasks by state. A failed delivery whose
  report has not reached the sender was previously indistinguishable from an ordinary
  failure, so the one queue condition an operator has to act on could not be seen.

### Changed

- **The send path commits through `store_submission`.** The API's `MailSender`
  seam queued recipients as a second step after storing the Sent copy, so a
  failed enqueue left a Sent message the user had been told had not sent — and a
  retry produced a duplicate. Authenticated SMTP submission had the same shape.
  Both now stage the body, then commit the Sent row, its attachment rows, the
  usage and every outbound queue row in one transaction through
  `ferroma_storage::store_submission`. `Submission` carries attachment rows, and
  an uploaded attachment's placeholder row is replaced in the same transaction.
  The `MailSender` trait and `QueueMailSender` are gone with the seam.

### Fixed

- **Outbound delivery survives worker crashes.** Claims a dead worker left in
  `delivering` are requeued on startup and during polling; a stale worker's late
  result can no longer overwrite a newer claim. Delivery report bounces became a
  durable, separately claimed task with backoff and crash recovery instead of a
  fire-and-forget send after `failed`.
- **Inbound multi-recipient delivery is all-or-nothing.** Every local copy of one
  SMTP transaction commits in a single database transaction; a full mailbox or a
  second-recipient write failure refuses the whole `DATA` with `452`/`451` and
  leaves no partial copies behind.
- **Outbound mail cannot be deleted mid-flight.** Deleting or expunging a message
  referenced by a pending, retrying or delivering queue entry is refused with a
  conflict; a database guard covers folder and account cascades. Queueing is
  batched per submission, and cancelling a claimed entry is honestly refused.
- **IMAP and REST folder renames keep message bodies readable.** Both entries now
  share one coordinator that stages the destination Maildir tree, then renames
  folders, rewrites message paths and records the sync change in a single
  transaction. Flag updates stage the renamed body before committing, so a crash
  between the rename and the commit cannot strand the old path.
- **FCP cursors follow commit order.** A trigger reallocates each user's
  `change_log.seq` under a per-user lock, so a client can no longer permanently
  skip a change committed after a higher-numbered one.
- **SMTP delivery and copying enforce quotas transactionally.** Inbound batch,
  single delivery, copy and submission paths check live usage under the owner's
  row lock instead of an advisory pre-check.
- **Webmail blocks remote content by default.** The reader frame carries a CSP
  that forbids remote images until the reader opts in; reply/forward no longer
  mounts sender HTML into the same-origin editor; list loads ignore stale
  responses; sending waits for pending uploads.

## [0.1.12] — 2026-09-23

### Fixed

- **Authenticated SMTP submission reaches the outbound queue.** A remote recipient now
  produces one Sent copy per message and a queue row per recipient, attributed to the
  authenticated user; the server returns `250` only after a queue row exists. Unknown
  addresses in a local domain are not relayed. Inbound SPF/DMARC enforcement no longer
  applies to authenticated submissions. IMAP protocol tests remain unchanged.
- **JMAP URLs use `api.public_url`** instead of assuming the advertised SMTP hostname
  is the HTTPS origin; this supports reverse-proxy hostnames and ports.
- **S3-compatible archive transfers can use a private endpoint.** The Admin Storage
  page accepts the HTTPS origin, SigV4 region and access/secret keys. Keys are saved
  in a mode-0600 file under the data directory, never returned by the read API or
  included in the export. The bucket remains in `s3://bucket/key`. AWS S3 region
  redirects are retried with a newly signed request.

## [0.1.11] — 2026-09-23

### Fixed

- **A mail client can send after it authenticates.** `AUTH PLAIN` carrying its
  payload on the same line left the session waiting for a further SASL response, so
  the `MAIL FROM` a client pipelined behind it was read as that response and
  discarded. The client then disconnected, and the message never reached the queue.
  Authentication now ends the exchange, and the next command is a command.

- **The JMAP session document is valid.** `eventSourceUrl` was `null`. RFC 8620
  requires a string, and a client stopped parsing at that field. Ferroma has no
  JMAP push, so the field is an empty string.

- **An export uses a `pg_dump` of the server's major version.** The image shipped
  PostgreSQL 16's client, which aborts against a PostgreSQL 18 server before
  anything is written. The export reads the server version and runs the matching
  client; the image ships `postgresql-client-18`.

### Added

- **Contacts.** An address is remembered when the account sends to it or receives
  mail that names it. The Webmail lists them, searches by address, name or note,
  and edits the name, the note, a favorite mark and a block. Mail from a blocked
  address is delivered to Junk. Deleting a contact forgets it until a later message
  names it again. `GET`, `POST`, `PATCH` and `DELETE /api/v1/contacts`.

- **An existing address can change its quota and which address is primary.**
  `PATCH /api/v1/users/{id}/mailboxes/{mailbox_id}`. The address itself is unchanged.

- **Archive destinations are kept.** The Storage page saves more than one, on the
  server, and exports the ones that are selected. `GET` and
  `PUT /api/v1/storage/destinations`.

## [0.1.10] — 2026-09-23

### Fixed

- **A JMAP client can sign in with the mailbox password.** `GET /.well-known/jmap`
  accepted only a bearer token minted by a Ferroma-specific call, so a client that
  follows RFC 8620 and sends `Authorization: Basic` received `401` and stopped. The
  same address now accepts Basic — the full address and its password — and a `401`
  names `Basic` in `WWW-Authenticate`. A repeated Basic login reuses the JMAP session
  already open for that address instead of writing one row per request. Bearer tokens
  still work, and a browser cookie still does not reach this surface.

- **Autodiscovery no longer tells a client to open 587 as implicit TLS.**
  `GET /.well-known/ferroma` said `tls: true` and nothing else. A client that reads
  that as "TLS from the first byte" opens the submission port that way and the
  handshake fails; the same client opening a closed 465 as STARTTLS waits until it
  times out. Each endpoint now carries `security`: `implicit` on 465 and 993,
  `starttls` on 587 and 143. An implicit port that is not listening is not
  advertised — the document names the plaintext port and `starttls` instead.

- **The Admin console is usable below 860px.** The sidebar stayed docked at 248px, so
  the section it opened was what got clipped. Below that width it is a drawer, opened
  from the top bar and closed by the section it leads to, the backdrop, or Escape.
  Wide tables still scroll inside their own box.

- **A quoted reply keeps its lines.** The quoted HTML is the other client's own
  tables and paragraphs. The editor's reset gave those no layout, so the older
  message collapsed into one line. Inside the quote, paragraphs keep their margin and
  a table stays a table.

- **A signature card is no longer a black slab in dark mode.** The reading frame
  inverts the message and then inverts images back. A card that states its own light
  background came out near-black with the avatar still on it. Anything that brought
  its own background is inverted back once, and the images inside it are not inverted
  a second time.

- **An inline image with only a Content-ID is stored.** Delivery kept a part when it
  looked like an attachment. A logo that carries a `Content-ID` and neither a filename
  nor `Content-Disposition: attachment` was dropped, and the `cid:` in the HTML had
  nothing to resolve to. Such an image is now an attachment. SVG stays out. Remote
  `http:` and `https:` images are still not loaded until the reader asks: the reading
  pane has a per-message control for that, because fetching them tells the sender the
  message was opened.

### Changed

- **One deployment, and it does not bring a database.** `docker compose up -d`
  reads `docker-compose.yml`, which starts Ferroma only, on the host's network.
  The host is assumed to already run PostgreSQL. `docker-compose.demo.yml` is a
  demonstration: it builds the checkout and starts its own database, in plaintext,
  and the command has to name it. The setup page is one form: its first step asks for that server's
  host, user name and password, and the rest of the form — the administrator, the
  mail domain, the hostname, TLS, and whether 465 and 993 listen — is filled in
  before anything is submitted. Connecting the database happens behind that one
  click; the form is not replaced and the address bar does not move. The implicit
  ports and the "password only after TLS" switches are stored by the wizard and
  adopted on the next start, so the compose file no longer states them.
  `docker-compose.yml` is the plaintext checkout stack, not an install.
  `docker-compose.external-db.yml` remains only so an older `scripts/deploy.sh`
  still finds it.

### Added

- **`POST /api/v1/storage/export` — the move, from the console.** The Storage page
  takes a path inside the container, or an `s3://` or `webdav://` URL, and writes one
  archive of both halves while the server stays up. The archive is marked live. Import
  is not offered there: it refuses while a server is listening, because a restore
  writes both halves underneath that process. The page prints the command to run on
  the new host once that host's server is stopped. S3 and WebDAV credentials stay in
  the environment and are not stored by the page.

## [0.1.9] — 2026-09-22

### Fixed

- **The documents no longer describe skeletons.** Seven documents — `architecture.md`, `deployment.md`,
  `imap.md`, `security.md`, `smtp.md`, `storage.md` and `sync.md`, each with its Chinese pair — carried
  `_(planned)_` claims written before the crates existed, and four of them opened with a status banner
  calling implemented crates unimplemented skeletons. Every marker was checked against the code and
  rewritten as a statement about what the server actually does; where a claim was stale the correction
  is substantive, not cosmetic. What the read found, among the rest: `SEARCH BODY`/`TEXT`/`HEADER` are
  implemented (reading the Maildir only when a key needs it), `BODYSTRUCTURE` returns the extensible
  form, `NAMESPACE`/`UIDPLUS`/`UNSELECT` are always advertised, `AUTH=LOGIN` never was, SPF and DKIM
  verdicts feed DMARC rather than rejecting on their own, MIME depth and part budgets map to
  `552 5.3.4`, and HTML sanitisation exists behind `security.sanitize_html`. CONDSTORE stays honestly
  marked not implemented — the storage side is ready and the protocol is not wired.

- **A test that only passed on machines with stale artifacts.** The acceptance run still contained
  `the_official_client_syncs_and_reads_from_the_server`, which locates a `ferroma-client` binary and
  builds the removed crate if it is missing. The desktop client left this repository in 0.1.8, so on
  a clean checkout the build fails; on machines holding an older `target/` the test passed against a
  binary nothing in the tree produced. It is gone; the FCP sync path it covered runs against the API
  directly in `the_sync_cursor_sees_the_delivery`, and a client speaking FCP end to end belongs to the
  client's own repository.

- **A narrow window shows the message, not a strip of header.** Below 900px the folder and list
  panes are hidden, but the grid kept their columns, so the reading pane — the only box left — was
  placed in the 200px first column and the rest of the window stayed empty. Subject, addresses and
  the action row stacked into a vertical strip and the body sat below the fold. Opening a message
  at that width is now a single column that takes the window.

- **An inline image in a message is the image, not a broken box.** An `<img src="cid:…">` names an
  attachment by its Content-ID, and a browser cannot fetch that. The reading frame is an opaque
  origin, so it also cannot fetch `/api/v1/attachments/…` with the page's session. The parent page
  now reads the matching image part and rewrites the `cid:` source to a `data:` URL before the
  frame is composed. Remote `http:`/`https:` images are still not loaded — that would confirm the
  message was opened — and SVG stays out, because a `data:image/svg+xml` document can carry script.

- **The account menu's Admin console looks like the other items.** It is the only link in a menu of
  buttons, and the page-wide link colour painted it blue between Settings and Sign out. Menu items
  now share the body's colour and carry no underline.

### Added

- **`ferroma storage export` / `ferroma storage import` — one archive, both halves.** A manual backup
  and a move to a new server are the same operation, and neither half alone is a backup. `export --to`
  writes one tar: a PostgreSQL custom-format dump, the Maildir, the attachment blobs, `dkim/`,
  `<data_dir>/database.json`, the generated `jwt_secret`, and a `manifest.json` (Ferroma version, time,
  `pg_dump` major version, file counts, a SHA-256 of every member). `--to` is a local path or an
  `s3://` / `webdav://` URL; S3 and WebDAV are destinations for that same archive, not a second backup
  system, and their credentials come from the environment. Export refuses while `ferroma serve` is up
  unless `--live`, which the manifest records. Import verifies the manifest, refuses a target that
  already has accounts or files unless `--replace`, refuses a dump whose `pg_dump` major version is not
  the server's, and runs `ferroma storage verify` afterwards — a mismatch exits non-zero. The runtime
  image now ships `postgresql-client-16`, which is what the command shells out to. Scheduling and
  off-site retention stay the operator's job: the retired backup sidecar is not back.

- **The bootstrap setup page has the acceptance test it lacked.** A new e2e run starts a real server
  with no database configured and walks the whole first-run path over real sockets: the setup code is
  printed and then enforced (`403` on a wrong one), `/` serves the wizard as HTML, an unknown API path
  is the JSON envelope rather than the front-end fallback, health answers `503 setup` honestly, a
  wrong code, a non-postgres address and an unreachable database are each refused with `invalid_input`,
  an accepted POST migrates the database it is given, writes `database.json` into the data directory,
  and the same process comes up as a healthy `ferroma serve` on the same port — where the bootstrap
  endpoint no longer exists. Previously this mode was held only by in-memory unit tests of the router.

- **JMAP is a session kind an existing database can grow into.** `sessions.kind` is a
  check constraint, and a database created before JMAP does not list `jmap` in it, so
  minting a JMAP token failed the insert. `migrations/0004_sessions_allow_jmap.sql`
  drops that check and adds it back with `jmap` included. The kind is deliberately
  not added to `0001_initial.sql`: sqlx checksums a migration's bytes, and editing
  migration 1 makes every database that already applied it refuse to start with
  `VersionMismatch`. A fresh database runs 0001 and then 0004, and ends at the same
  constraint.

- **A front-end file the image cannot read.** `admin/views/services.js` was mode
  `0600`. `COPY` keeps the mode it is given, and the service runs as uid 10001, so
  the file is a 404 and an Admin page that imports it never boots. A checkout
  bind-mounted as `FERROMA__API__WEBMAIL_DIR` has the same problem, because that
  path never passes through the Dockerfile's `chmod`. The file is `0644`, which is
  what `tools/check-deploy.mjs` requires of everything under `web/`, `admin/`,
  `shared/` and `config/`.

- **`cargo clippy --workspace` is clean on 1.98, not merely free of errors.** The
  two `manual_is_multiple_of` notes in the SMTP connection limiter, the needless
  `as_deref_mut` on the IMAP shutdown watch, the nested `if` and the slice-from-clone
  in the DNS report, the five-vector return of the JMAP address splitter, and the
  SigV4 helper's ten arguments are all gone. The e2e bootstrap wait no longer
  assigns a status it immediately overwrites.

### Changed

- **The storage document names the migrations that actually exist.** It claimed a
  single 440-line file. There are four: 0001 creates the schema, 0002 comments it,
  0003 lets a folder tombstone outlive its row, and 0004 admits `jmap`. It also
  says why 0001 cannot be edited after it has been applied.

## [0.1.8] — 2026-09-21

### Changed

- **This repository is the server, and only the server.** The desktop client crate is gone: it is a
  separate project now, and a server repository that also ships a GUI carries a build matrix, a
  release cadence and a dependency tree that the server does not need. What stays is everything a
  client talks to — the FCP protocol, `docs/fcp.md`, and the `x-ferroma-client` header the API reads
  to identify a caller. What leaves is `client/`, `docs/client.md` and its Chinese pair, and the
  Dockerfile's build-cache stubs for it.

- **A reply's quoted text is visibly the older message, in both places you meet it.** In the
  reader, every message frame now carries a stylesheet for the wrappers clients actually use —
  `<blockquote>`, Gmail's `div.gmail_quote`, Yahoo's `.yahoo_quoted`, Outlook's `#appendonly` — so
  a wall of quoted text has a muted left edge and a colour of its own instead of looking like what
  the sender wrote today. In the reply editor the quote is dimmed (through the whole block, a
  copied signature card included), which is the only thing that can separate it from the text
  being typed: both are editable, so only the styling can tell them apart.

- **One licence, and it is the server one: AGPL-3.0-only.** The project was dual-licensed
  (`MIT OR Apache-2.0`); it is now a single licence, chosen so that a *modified* version offered
  to others as a network service has to come with its source. `LICENSE` carries the verbatim
  text, `Cargo.toml` and the image's OCI label carry the SPDX id, and the image now contains the
  licence file itself.

- **The documentation site is rebuilt on Beautiful UI's design language, and it now carries the
  deployment path end to end.** `tools/build-site.mjs` and `tools/site-src/` were rewritten
  around the oklch token set, the ring-first elevation model, the dashed section rules and the
  Inter + JetBrains Mono pairing that [beautifului.dev](https://www.beautifului.dev) publishes,
  in both themes. The home page states what the project is, then how to get at it: the
  four-command path that ends at the setup wizard — `docker compose up -d`, then the root path,
  which is where the wizard lives until the instance has its first administrator — and the three
  supported ways to put it on the internet (Compose with its own PostgreSQL, Compose on a host
  that already runs one, plain `docker run`), each with the commands that exist in this tree
  rather than a sketch of them. A new page, `deploy.html` / `部署指南`, walks all four, down to
  the environment variables and what the wizard's answers actually govern; `docs/deployment.md`
  stays the reference it was, and the two now link to each other.

  The site also covers what lives outside `docs/`: the root `CONTRIBUTING.md` /
  `CONTRIBUTING_zh.md` pair is a page of its own, and a link to a file the site does not render
  (`AGENTS.md`, `LICENSE`, `TODO.md`, `config/ferroma.toml`) resolves to GitHub rather than
  degrading to plain text — which is what those twenty-odd links used to do.

  The home page is now two pages — `/` is English and `/zh/` is Chinese, so switching language
  from the front door lands on the other front door instead of on a document — and it carries
  three things rather than five: the positioning, the fastest path, and the document index. The
  capability grid and the three-path deployment summary were saying at length what
  `docs/architecture.md` and the deployment guide already say.

- **The documented memory requirement was four times what it needs to be.** Every "~4 GB RAM"
  traced back to one line in `docker-compose.prod.yml` claiming its PostgreSQL tuning "suits a
  4 GB host" — but `shared_buffers=512MB` is 25% of 2 GB, which is the usual rule of thumb, so
  those numbers describe a 2 GB host and were only ever conservative on a bigger one. The
  process itself is far smaller than either figure: it stays in the tens of megabytes, and what
  a deployment spends memory on is PostgreSQL and the page cache. The requirement now reads
  1 vCPU / 1 GB, and the compose comment says 2 GB.

- **The search index now covers code blocks.** A variable name like
  `FERROMA__QUEUE__RELAY_HOST` appears only inside a YAML snippet, so searching for it returned
  nothing — which is how it was reported. Each section's code is indexed separately from its
  prose, so a hit that occurs only in code ranks below one in the text instead of competing with
  it. The relay variables are also in the environment-variable table now, where they belong.

  Site assets now carry a content hash in their URL. The host serves
  `Cache-Control: max-age=14400` on `.js` and `.css`, and Cloudflare caches them by extension,
  so an asset whose contents changed but whose URL did not stayed stale for up to four hours —
  which is what happened to the search-box fix below: it was pushed, and the browser went on
  running the previous script. A hash of the contents now goes into the query string, so a
  changed file is a different URL and both caches fetch it. (`site-check.mjs` strips the query
  string before resolving a link, so the fingerprint does not read as a broken path.)

  A reader-reported interaction bug: pressing the mouse in the search box and moving it dragged
  the page upward. The search box sits in the fixed top bar, so a press that leaves the box
  turns into a cross-document selection, and the browser auto-scrolls toward wherever the
  pointer went — all the way to the top. Two changes: the interface shell (top bar, sidebar,
  table of contents, pager, chips) no longer takes part in text selection, which is what it
  should have been from the start, and while the mouse is held down inside the search box the
  scroll position is pinned, so dragging still selects text and places the caret but no longer
  moves the page. `user-select: none` does not reach an `<input>` — it has its own selection
  model — and `preventDefault` would have taken caret placement with it.

  One deployment gap the site was missing: what to do when the provider will not set a PTR.
  `[queue] relay_host` has been in `config/ferroma.toml` from the start — it hands the outbound
  path to a smarthost that already has its reverse DNS right, and leaves receiving untouched —
  but neither the deployment guide nor §2.3 of the reference mentioned it, which leaves the
  reader stuck at the one precondition they cannot satisfy. Both say it now, together with the
  two DNS records that move with it: `SPF` has to `include` the relay provider's own domain,
  and `PTR` stops mattering.

  Two rendering fixes came out of reading the Chinese pages. Every diagram on the site was
  drawn with U+2500 box characters, which are *ambiguous width* — one column in a Latin
  monospace face, two in a CJK one — so a reader whose monospace falls back to a CJK font saw
  every diagram misaligned (measured here: `─` at 9.36px against `|` at 4.61px, and identical
  under both `lang` values, because the font decides it, not the language). Diagrams are now
  emitted as their ASCII equivalents, which makes alignment a property of the characters
  instead of a property of the reader's font stack. The home page's diagram was also redrawn
  on a strict column grid and centred in its panel, and code-block comments are now
  per-language: a Chinese page no longer explains itself in English.

  The fastest path itself was wrong in the first cut of this page. It cloned the repository and
  built the workspace in the container — ten to thirty minutes — and asked for a database
  password up front. It does not need any of that: with no database *stated*, the server binds
  its web port anyway and asks for the connection in the browser, alongside a setup code it
  prints to the log, and then continues booting in the same process. One released image, one
  data volume, no `.env`, no build — `docker compose up -d` or a single `docker run`, then the
  wizard. `docs/deployment.md` §3.5 had this right all along; the site now says it too, in both
  forms.

  Two things this fixes along the way: the quick-start block on the home page cloned a
  repository URL that does not exist, and the site carried a second, unreferenced generator
  (`scripts/build-static-site.mjs`) that wrote the same directory with a partial document list
  and no glossary — it now says at the top that it is not the one CI runs.

## [0.1.7] — 2026-09-20

### Changed

- **The first-run wizard ends at the sign-in page.** It used to reload into `/admin/`, and the
  session it minted meant the Webmail at `/` opened straight into the inbox. Finishing setup is a
  handoff, not a session: it waits for the restarted server to answer twice — the first answer can
  still come from the process on its way out, and landing in that gap is what put a blank shell on
  screen — then drops the session it created and opens the domain root, which is the Webmail's
  sign-in card, with the console one address away at `/admin/`.
- **The DNS panel's `PTR` and `SPF` rows read the deployment's actual outbound path.** Both used
  to assume this host sends its own mail. A `PTR` record is what a *direct* sender needs, so a
  relay turns a missing one from a defect into a note; and an `SPF` record for a relayed instance
  has to `include` the provider's own domain, a name nothing can guess from the relay's hostname —
  so a record that delegates sending is accepted with a hint naming that condition instead of a
  warning. Both rows now say what they mean, given `[queue] relay_host`.


### Fixed
- **A published image now names the build it is.** The builder stage re-declares
  `FERROMA_REVISION` and `FERROMA_CREATED` and exports them as `FERROMA_GIT_SHA` and
  `FERROMA_BUILD_TIMESTAMP` immediately before the real `cargo build` — after the
  dependency-cache layer, which a per-release value placed any earlier would invalidate on
  every release. `ferroma version` inside the container and the Admin sidebar footer now
  print the commit and the date instead of `unknown`. `docker inspect` had the revision all
  along, which is the wrong direction for a bug report to travel: the person holding the
  container can read the labels, the person reading the report cannot.


- **A front-end document is revalidated like the scripts it loads.** `/` and `/admin/` have no
  file extension, so the revalidation layer skipped them and their responses carried no
  `Cache-Control` at all — a browser's cue to cache heuristically. After an upgrade, or after the
  root switched from the console to the Webmail, a browser could therefore keep the previous
  shell while its scripts moved on: the new bundle running inside the old app's HTML, which
  reports a missing element and leaves a sign-in that worked sitting on the sign-in card.

## [0.1.6] — 2026-09-20

### Changed

- **Setting up is one page.** A server with no database serves the console at `/`; the
  database step hands that page to the first-run wizard instead of sending the operator to
  another address; and finishing lands on the domain root rather than on the console's own path.

### Fixed

- **The domain DNS panel reads `nslookup`, and two of its answer shapes were not understood.**
  A TXT answer arrives as `name  text = "…"` — the record type is spelled `text`, never `TXT` —
  so SPF, DKIM and DMARC were reported empty on domains whose records were published and
  correct; an address answer arrives as `Name:` plus `Address:`, so the A and AAAA rows carried
  no value either, and the PTR row could not run at all: its expected address was a field
  nothing ever filled in, and it now comes from the A record the domain itself publishes —
  which is the address a PTR record has to answer for. A DKIM key printed as several quoted
  pieces is joined into the one string an operator has to paste.

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


### Known issues carried into 0.1.9

One item found while cutting 0.1.3 is still open: seven documents still carry
`_(planned)_` claims written before those crates existed. It is tracked in
[`TODO.md`](TODO.md) under "Next release (0.1.9)". That work closed in 0.1.9, and
the README no longer restates it.

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
