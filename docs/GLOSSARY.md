# Glossary — English terminology

This file is the single source of terminology for the English documents, exactly as
[`zh/GLOSSARY.md`](zh/GLOSSARY.md) is for the Chinese ones. Two documents written a
month apart should not disagree about whether the thing is a *mailbox* or an *account*,
a *folder* or a *directory*; this is where those choices are written down, so a later
edit can be measured against them instead of guessed at.

The Chinese file is **not** a translation of this one: it records the same decisions and
adds the Chinese rendering of each term, and it is the authority for anything a
translator has to decide. A term changed here has to change in both files —
`tools/check-docs.mjs` and `tools/check-zh.mjs` are what notice when the pair drifts.

## 1. Literals that are never spelled differently

Keep these exactly as the code spells them, in backticks, including case and
punctuation: configuration keys (`server.hostname`, `api.jwt_secret`), environment
variables (`DATABASE_URL`, `FERROMA__API__WEBMAIL_DIR`), commands (`ferroma database
init`), HTTP methods and paths (`POST /api/v1/storage/gc`), protocol keywords (`EHLO`,
`MAIL FROM`, `IDLE`), reply and status codes (`550 5.1.1`, `426 Upgrade Required`),
Rust types (`FerromaError::MailboxFull`), database objects (`change_log`,
`rfc_message_id`, `TIMESTAMPTZ`), JSON field names, paths (`/var/lib/ferroma`), and RFC
numbers (`RFC 5322`, never "the RFC 5322 standard"). The exhaustive list, with the
Chinese renderings that go with it, is [`zh/GLOSSARY.md`](zh/GLOSSARY.md) §一.

## 2. Canonical terms

"Not" is what to avoid; it is not always wrong English, only a second name for one
thing, which is what this table exists to prevent.

| Write | Not | Why |
|---|---|---|
| Ferroma | ferroma, FERROMA (in prose) | the product; `ferroma` is the binary and the command |
| Webmail | webmail, web mail | the surface served from `web/` |
| Admin console | Admin UI, Admin panel, admin console | one name for the surface served from `admin/`; the Chinese files may write "Admin 管理后台" at first mention |
| mail core | mail engine, mail layer | the `ferroma-mail` layer |
| repository layer | repo, store | `ferroma-storage`'s data-access layer; "repository" alone means git |
| mailbox | account, e-mail account | the container a single address owns |
| folder | directory | an IMAP folder; "directory" only for the Maildir on disk |
| message | mail, e-mail, item | one stored RFC 5322 message |
| envelope | — | the SMTP envelope (`MAIL FROM` / `RCPT TO`), distinct from the header |
| header field | heading | `Received:` is a header field; "heading" is a document section |
| body | content | the message body |
| attachment | file | an attachment blob |
| blob store | binary store | the content-addressed attachment storage |
| content-addressed | hash-named | addressed by the SHA-256 of the content |
| quota | limit | a mailbox's byte quota |
| cursor | offset, position | an incremental-sync position |
| change log | changelog | the `change_log` table; "changelog" is `CHANGELOG.md` only |
| idempotency key | dedupe key | the client-supplied key on a mutating request |
| outbox | send queue | the client-side queue of unsent mail |
| submission | sending | authenticated mail on 587 |
| inbound, outbound | incoming, outgoing | mail direction, as nouns |
| delivery, queue, retry, bounce, quarantine | | see `zh/GLOSSARY.md` §二 for the renderings |
| resolver, lookup | | DNS |
| preflight | pre-flight | the checks `ferroma doctor` runs |
| health check | healthcheck, health-check | prose; `ferroma healthcheck` is the literal command |
| migration | schema change | a file under `migrations/` |
| Maildir | maildir, MailDir | prose; lowercase only inside a path or a code span |
| front-end | frontend, front end | noun and adjective, for `web/` and `admin/` |
| real time, real-time (attributive) | realtime | prose; `Realtime` stays the feature's name in `fcp.md` |
| setup (noun, attributive) | set-up | `setup` for the thing, `set up` for the verb |
| login (noun), log in (verb) | signin, sign in, sign-in | one pair for authentication |
| backup (noun, attributive), back up (verb) | back-up | `back up the database`, `a database backup` |
| first-run wizard, the wizard | setup wizard | the browser page that collects identity and TLS |
| operator | admin | the human running the host; "admin" is a role and an account, never the person |
| named volume, bind mount | | Docker storage, as Compose names them |
| reverse proxy | proxy | when the distinction matters; "proxy" alone after first mention |
| release, tag, digest | version (for a tag), hash | a release is `X.Y.Z`; a tag names it; a digest is the immutable manifest |
| provenance attestation | provenance | the statement BuildKit attaches to a published image |
| layer cache | build cache | the local buildx cache — `.cache/buildx`, or `$HOME/.cache/ferroma-buildx` when the checkout path has non-ASCII characters; the retired registry cache was a `buildcache` tag |
| sidecar | | **retired**: the backup and restore containers 0.1.3 shipped. Historical notes only |
| TLS, STARTTLS, SMTPS, IMAPS, SMTP, IMAP, MTA-STS, DKIM, SPF, DMARC, MX | | protocol names, always in this case |
| RFC 5322, RFC 3501, RFC 7489 | the RFC 5322 standard | cite the number, not a paraphrase of it |

## 3. Style

* **English is the default.** `README.md`, `CHANGELOG.md`, `TODO.md` and `docs/*.md` are
  the primary text; the `*_zh.md` and `docs/zh/*` files mirror them and say so.
* Sentence-case headings, one `#` per file. Headings are **not** renamed to fix
  terminology: other documents link to their anchors, and `tools/check-docs.mjs` fails
  on a link whose anchor is gone.
* Identifiers, paths, keys and commands go in backticks; prose does not.
* Em dashes are spaced: `text — like this`.
* Serial comma in a list of three or more.
* Quantities take a space and a unit: `10 MB`, `14 days`, `86400 s`; time is UTC.
* A term is introduced once as "Chinese (English)" only in the Chinese files; English
  text uses the English term alone.

## 4. Cross-references

* An English document links to a sibling by file name: `[smtp.md](smtp.md)`. A Chinese
  document links to its *Chinese* sibling; repository paths climb one level further
  (`../../crates/`), because the mirrored file is one directory deeper.
* Every pair keeps the same headings and the same number of code fences
  (`tools/check-zh.mjs`), and every link and anchor resolves (`tools/check-docs.mjs`).
* Text that leaves the repository — [`dockerhub.md`](dockerhub.md), pasted into the
  Docker Hub page — uses **absolute** `https://github.com/z1HwanG/Ferroma/…` URLs, because
  a relative link cannot resolve outside a checkout.

## 5. Keeping the pair in step

```bash
node tools/check-docs.mjs              # links, anchors, the bilingual Docker Hub artifact
node tools/check-zh.mjs                # heading/fence parity and terminology across the pair
node tools/build-site.mjs              # render docs/ the way a reader will see it
node tools/site-check.mjs              # no unrendered markdown, no link without a target
node tools/check-deploy.mjs            # the deployment path the docs describe
```

All of them run in CI before a release image is built
(`.github/workflows/docker-publish.yml`), so a document that drifts fails the release
rather than the reader.
