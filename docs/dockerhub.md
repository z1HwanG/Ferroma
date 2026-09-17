# Ferroma — Docker Hub repository description

Paste-ready text for the Docker Hub repository page. The **Overview** field below is
deliberately shorter than [`README.md`](../README.md): the repository README covers the
crate graph, the test suite and building from source, none of which an image consumer
needs. Keep this file in step with the image contract — the ports, volumes and the
running user are what the [Dockerfile](../Dockerfile) actually produces.

## Short description (the one-line field)

> A Rust-native self-hosted mail platform: its own SMTP and IMAP servers, Maildir
> storage, Webmail, admin console and client API — one binary, no Postfix or Dovecot.

## Overview

**Ferroma is a complete mail system built from the protocols up.** Its own SMTP and
IMAP servers, its own MIME and mail core, its own storage engine — plus Webmail, an
admin console, and official clients that speak a purpose-built synchronisation
protocol. It does not wrap Postfix, Dovecot or Stalwart: the point is to own the whole
stack.

This image is the server binary, `ferroma`, on a minimal Debian base.

### What is in the image

| | |
|---|---|
| Entry point | `ferroma`, default command `serve --config /etc/ferroma/ferroma.toml` |
| Runs as | uid/gid `10001` (`ferroma`), never root — `NET_BIND_SERVICE` is granted as a file capability so it can still bind 25/587/143 |
| Ports | `25` SMTP (inbound MX), `587` submission, `465` SMTPS, `143` IMAP, `993` IMAPS, `8080` HTTP API + Webmail + Admin |
| Volumes | `/var/lib/ferroma` — Maildir, attachments, TLS material, backups. **This is the volume to back up.** A configuration file is read from `/etc/ferroma/ferroma.toml` |
| Health check | `ferroma healthcheck --url http://127.0.0.1:8080/api/v1/health` |
| Frontends | Webmail and Admin are baked in at `/usr/share/ferroma/{web,admin}` |
| Shutdown | `SIGTERM` drains in-flight SMTP transactions and queue deliveries before exiting |

### Run it

The supported deployment is Docker Compose, and the compose files live in the
[repository](https://github.com/z1HwanG/Ferroma) — they wire up PostgreSQL, the
backup sidecar, TLS and the reverse-proxy assumptions this image cannot make for you:

```bash
git clone https://github.com/z1HwanG/Ferroma && cd Ferroma
cp .env.example .env      # set POSTGRES_PASSWORD, FERROMA_HOSTNAME, FERROMA_JWT_SECRET
FERROMA_VERSION=0.1.0 docker compose -f docker-compose.prod.yml up -d
```

A bare `docker run` works, but PostgreSQL has to be reachable from the container and
the schema has to exist. The implicit-TLS ports are left out on purpose: `465` and
`993` are **off in the shipped configuration** (`smtps_port = 0`, `imaps_port = 0`), so
publishing them here would map ports nothing listens on — a connection that is refused
rather than an error anyone can read. `docker-compose.prod.yml` turns them on together
with the certificates they need:

```bash
docker run -d --name ferroma \
  -p 25:25 -p 587:587 -p 143:143 -p 8080:8080 \
  -e DATABASE_URL=postgres://ferroma:secret@db:5432/ferroma \
  -e FERROMA_HOSTNAME=mail.example.com \
  -e FERROMA_JWT_SECRET="$(openssl rand -hex 32)" \
  -v ferroma-data:/var/lib/ferroma \
  wesukilaye/ferroma:0.1.0
```

The full first-run path — migrations, the first domain, the first administrator, DKIM,
the DNS records you must publish — is in
[`docs/deployment.md`](https://github.com/z1HwanG/Ferroma/blob/main/docs/deployment.md).
The DNS section is not optional: a correct Ferroma with wrong SPF, DKIM, DMARC or PTR
records is a mail server whose mail lands in Junk.

### Tags

| Tag | Meaning |
|---|---|
| `0.1.0` | an exact release — pin this in production |
| `0.1` | the newest patch of that minor |
| `latest` | the newest release |

Built for `linux/amd64` and `linux/arm64`. Pre-releases publish only their exact tag:
`latest` never points at a release candidate.

### Before you deploy it on port 25

You need a host with a public IP, a domain whose `MX` record points at it, a matching
`PTR` record, and outbound port 25 open. Without them mail is rejected regardless of
how correct the server is — see
[`docs/deployment.md`](https://github.com/z1HwanG/Ferroma/blob/main/docs/deployment.md) §2.

## Licence

MIT OR Apache-2.0, at your option.
