# TODO — what is not done

The Chinese translation is at [`TODO_zh.md`](TODO_zh.md).

This queue holds only what the repository already admits is not done. It is seeded from
the open items in [`CHANGELOG.md`](CHANGELOG.md), the `_(planned)_` claims the documents
still carry, and the gaps in the documentation set itself. Nothing here is a new idea;
what has shipped is in [`CHANGELOG.md`](CHANGELOG.md), and the reasoning behind the
retired pieces is in its `## [Unreleased]` section.

## Next release (0.1.9)

- Seven documents still describe skeletons: they carry `_(planned)_` claims written before those crates existed, and four of them — `imap.md`, `security.md`, `smtp.md`, `sync.md` — still open with a status banner calling implemented crates unimplemented — [README — Carried into 0.1.9](README.md#carried-into-019), [CHANGELOG — Known issues carried into 0.1.9](CHANGELOG.md#known-issues-carried-into-019).

## Documented but not delivered

Each line below is one document and the number of `_(planned)_` claims it still carries.
The counts are measured from the tree as it stands, so they move as the claims are
checked; `architecture.md` also holds the sentence that explains the marker, which is
one of its two.

- `docs/architecture.md` carries 2 `_(planned)_` claims, one of which is the sentence explaining the marker itself — [architecture.md](docs/architecture.md).
- `docs/deployment.md` carries 1 `_(planned)_` claim — [deployment.md](docs/deployment.md).
- `docs/imap.md` carries 19 `_(planned)_` claims — [imap.md](docs/imap.md).
- `docs/security.md` carries 25 `_(planned)_` claims — [security.md](docs/security.md).
- `docs/smtp.md` carries 24 `_(planned)_` claims — [smtp.md](docs/smtp.md).
- `docs/storage.md` carries 1 `_(planned)_` claim — [storage.md](docs/storage.md).
- `docs/sync.md` carries 2 `_(planned)_` claims — [sync.md](docs/sync.md).

## Documentation

- `docs/dockerhub.md` is a single bilingual artifact rather than a translated pair: its Chinese half lives in the same file because the page it feeds has no language switch, so there is no `docs/zh/dockerhub.md` — [dockerhub.md](docs/dockerhub.md).
- `AGENTS.md` has no Chinese mirror, and nothing in the repository requires one — [AGENTS.md](AGENTS.md).

## Testing

- The bootstrap server — the mode that serves the setup page when no database is reachable — has no acceptance test: `server/tests/e2e.rs` covers the running server only, so the root mount and `serve`'s choice of which app answers at `/` (which turns on there being an administrator) are held by unit tests in `crates/ferroma-api/src/router.rs`, and nothing drives either over a socket — [router.rs](crates/ferroma-api/src/router.rs).

## Decided against

- The backup and restore sidecars stay retired: neither compose file defines a `backup` or `restore` service, `scripts/backup.sh` and `scripts/restore.sh` are gone, and backing up the database and the `ferroma-data` volume together is the operator's job — [CHANGELOG — Removed](CHANGELOG.md#removed).
- The rolling minor tag (`0.1`, `0.2`, …) and the registry-backed `buildcache` layer cache stay retired: a release publishes `X.Y.Z` and `latest` only, and caching is local in `.cache/buildx` — [CHANGELOG — Removed](CHANGELOG.md#removed).
