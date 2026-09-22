# TODO — what is not done

The Chinese translation is at [`TODO_zh.md`](TODO_zh.md).

This list records only what the repository already marks as unfinished. The sources are
the open items in [`CHANGELOG.md`](CHANGELOG.md) and the gaps in the documentation set.
It does not record new proposals. What has shipped is in [`CHANGELOG.md`](CHANGELOG.md),
and the reasoning for the retired pieces is in its `## [Unreleased]` section.

## Documentation

- `docs/dockerhub.md` is a single bilingual artifact rather than a translated pair: its Chinese half lives in the same file because the page it feeds has no language switch, so there is no `docs/zh/dockerhub.md` — [dockerhub.md](docs/dockerhub.md).
- `AGENTS.md` has no Chinese mirror, and nothing in the repository requires one — [AGENTS.md](AGENTS.md).

## Tests deliberately not written

- The desktop client left the repository in 0.1.8, so the FCP acceptance path is driven
  against the API directly (`the_sync_cursor_sees_the_delivery`). A client speaking FCP
  end to end belongs to the client's own repository, where the protocol contract
  ([`docs/fcp.md`](docs/fcp.md)) is what both sides implement.

## Decided against

- The backup and restore sidecars stay retired: neither compose file defines a `backup` or `restore` service, `scripts/backup.sh` and `scripts/restore.sh` are gone, and backing up the database and the `ferroma-data` volume together is the operator's job — [CHANGELOG — Removed](CHANGELOG.md#removed).
- The rolling minor tag (`0.1`, `0.2`, …) and the registry-backed `buildcache` layer cache stay retired: a release publishes `X.Y.Z` and `latest` only, and caching is local in `.cache/buildx` — [CHANGELOG — Removed](CHANGELOG.md#removed).
