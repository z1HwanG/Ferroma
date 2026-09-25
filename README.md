<div align="center">

# Ferroma

**English** · [简体中文](README_zh.md)

**A self-hosted mail server, written in Rust.**

It receives and sends mail, and reads and manages it in the browser. Mail is stored on the host that runs it.

[![Rust](https://img.shields.io/badge/Rust-stable-black?logo=rust)](https://www.rust-lang.org/)
[![PostgreSQL](https://img.shields.io/badge/PostgreSQL-18-4169E1?logo=postgresql&logoColor=white)](https://www.postgresql.org/)
[![Docker](https://img.shields.io/badge/Docker-Compose-2496ED?logo=docker&logoColor=white)](https://hub.docker.com/r/wesukilaye/ferroma)
[![License](https://img.shields.io/badge/License-AGPL--3.0-blue)](LICENSE)

Documentation: <https://ferroma.z1hwang.cn/>

</div>

## Install

The deployment requires PostgreSQL. `docker compose up -d` starts Ferroma only; the
setup page collects the database connection.

```bash
git clone https://github.com/z1HwanG/Ferroma && cd Ferroma
cp .env.example .env          # set FERROMA_VERSION to the release you want
docker compose up -d
```

Open `http://localhost:8080`. Pin the image with `FERROMA_VERSION=0.1.13`; `latest`
follows the newest release. `docker-compose.demo.yml` is a plaintext demonstration
and is not part of the command above.

DNS, TLS, DKIM and backups: [the deployment guide](docs/deployment.md).

## What works

| Component | State |
|---|---|
| `ferroma-core` — configuration, errors, typed ids, addresses, limits, logging | done |
| `ferroma-mail` — RFC 5322 + MIME parsing and building, headers, flags, envelopes | done |
| `ferroma-storage` — PostgreSQL schema, repositories, Maildir, content-addressed attachments | done |
| `ferroma-events` — typed event bus with replay for reconnecting clients | done |
| `ferroma-auth` — Argon2id, HS256 access tokens, rotating refresh tokens, devices, throttling | done |
| `ferroma-smtp` — server, outbound client, MX resolution, DKIM/SPF/DMARC, inbound policy | done |
| `ferroma-imap` — IMAP4rev1 server, including IDLE, APPEND, MOVE and EXPUNGE | done |
| `ferroma-sync` — change log, cursors, idempotent client operations | done |
| `ferroma-api` — REST API, Ferroma Client Protocol, WebSocket, front-end hosting | done |
| `server` — the `ferroma` binary and its operator commands | done |
| Webmail, Admin console | done |

## Contributing

[`CONTRIBUTING.md`](CONTRIBUTING.md) is the agreement: the checks, the test a change
has to carry, and the DCO sign-off. [`AGENTS.md`](AGENTS.md) is the longer version,
including the quirks of one development machine.

## Licence

**AGPL-3.0-only.** Run it, modify it, self-host it — the condition that matters for a server is
§13: if you let other people use a *modified* version over a network, you have to offer them that
version's source. This repository is the upstream source, and the full text is in
[`LICENSE`](LICENSE).
