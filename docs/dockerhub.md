# Ferroma — Docker Hub repository description

Paste-ready text for the Docker Hub repository page. The **Overview** field below is
deliberately shorter than [`README.md`](https://github.com/z1HwanG/Ferroma/blob/main/README.md):
the repository README covers the crate graph, the test suite and building from source,
none of which an image consumer needs. Keep this file in step with the image contract —
the ports, the volume and the running user are what the
[`Dockerfile`](https://github.com/z1HwanG/Ferroma/blob/main/Dockerfile) actually
produces. The Chinese translation is appended at the end.

## Short description (the one-line field)

> A Rust-native self-hosted mail platform: its own SMTP and IMAP servers, Maildir
> storage, Webmail, Admin console and client API — one binary, no Postfix or Dovecot.

## Overview

**Ferroma is a complete mail system built from the protocols up.** Its own SMTP and
IMAP servers, its own MIME and mail core, its own storage engine — plus Webmail, an
Admin console, and official clients that speak a purpose-built synchronisation
protocol. It does not wrap Postfix, Dovecot or Stalwart: the point is to own the whole
stack.

This image is the server binary, `ferroma`, on a minimal Debian base.

### What is in the image

| | |
|---|---|
| Entry point | `ferroma`, default command `serve --config /etc/ferroma/ferroma.toml` |
| Runs as | uid/gid `10001` (`ferroma`), never root — `NET_BIND_SERVICE` is granted as a file capability so it can still bind 25/587/143 |
| Ports | `25` SMTP (inbound MX), `587` submission, `465` SMTPS, `143` IMAP, `993` IMAPS, `8080` HTTP API + Webmail + Admin — `465` and `993` are off in the shipped configuration |
| Volume | `/var/lib/ferroma` — the Maildir, the attachment blobs, the DKIM private key at `/var/lib/ferroma/dkim/<selector>.private`, and `<data_dir>/database.json` (the remembered database address, mode 0600; it holds the database password). **This is the volume to back up.** `ferroma storage export` writes it and the database into one archive; scheduling and keeping that archive off the machine stay the operator's job |
| Config | read from `/etc/ferroma/ferroma.toml`; the image sets `FERROMA_CONFIG` and `FERROMA_DATA_DIR` |
| Health check | `ferroma healthcheck --url http://127.0.0.1:8080/api/v1/health` |
| Frontends | Webmail and Admin are baked in at `/usr/share/ferroma/{web,admin}`, with the modules both import at `/usr/share/ferroma/shared` |
| Shutdown | `SIGTERM` stops the listeners, lets in-flight SMTP transactions and queue deliveries finish, then exits |

### Run it

The only supported deployment is one Compose file,
[`docker-compose.yml`](https://github.com/z1HwanG/Ferroma/blob/main/docker-compose.yml).
It starts Ferroma only; PostgreSQL must be available separately. Enter its connection
information in the first-run setup page. Back up both PostgreSQL and the Ferroma data
volume (see [`docs/deployment.md`](https://github.com/z1HwanG/Ferroma/blob/main/docs/deployment.md) §8). The file is in the
[repository](https://github.com/z1HwanG/Ferroma):

```bash
git clone https://github.com/z1HwanG/Ferroma && cd Ferroma
cp .env.example .env      # set FERROMA_VERSION to the desired image tag
FERROMA_VERSION=0.1.14 docker compose -f docker-compose.yml up -d
```

A single container is the same deployment with the database elsewhere. PostgreSQL has to be reachable from the container and the
schema has to exist. Leave `DATABASE_URL` unset: the container starts and serves a page at `/` that
asks for the address, printing a one-time code to the log, and remembers the answer in the volume.
Everything else the first-run wizard
collects — the mail domain, the hostname, the public URL — is that wizard's to own, and
the JWT secret is generated into the volume (`<data_dir>/jwt_secret`) when
`FERROMA_JWT_SECRET` is unset. The `docker run` example does not mount certificate files; 465 and 993 remain off
until TLS is configured in the setup wizard and the required ports are available.
The production Compose file uses host networking, so its mail listeners bind host ports directly:

```bash
docker run -d --name ferroma \
  -p 25:25 -p 587:587 -p 143:143 -p 8080:8080 \
  -v ferroma-data:/var/lib/ferroma \
  wesukilaye/ferroma:0.1.14
```

The full first-run path — migrations, the first domain, the first administrator, DKIM,
the DNS records you must publish — is in
[`docs/deployment.md`](https://github.com/z1HwanG/Ferroma/blob/main/docs/deployment.md),
and the rest of the documents are in the
[`docs/`](https://github.com/z1HwanG/Ferroma/tree/main/docs) directory.
The DNS section is not optional: a correct Ferroma with wrong SPF, DKIM, DMARC or PTR
records is a mail server whose mail lands in Junk.

### Tags

| Tag | Meaning |
|---|---|
| `0.1.14` | an exact release — pin this in production |
| `latest` | the newest release |

Built for `linux/amd64` and `linux/arm64`. A release publishes `X.Y.Z` and `latest` and
nothing else: there is deliberately no rolling `X.Y` tag and no `buildcache` tag.
Pre-releases publish only their exact tag, and `latest` never points at a release
candidate.

### Before you deploy it on port 25

You need a host with a public IP, a domain whose `MX` record points at it, a matching
`PTR` record, and outbound port 25 open. Without them mail is rejected regardless of
how correct the server is — see
[`docs/deployment.md`](https://github.com/z1HwanG/Ferroma/blob/main/docs/deployment.md) §2.

## Licence

AGPL-3.0-only — free to run, modify and self-host. The condition that matters for a server: a modified version offered to others over a network has to come with its source.

---

## 中文版（Chinese）

本文件是 Docker Hub 仓库页面的粘贴文本。下面的 **Overview** 字段刻意比 [`README.md`](https://github.com/z1HwanG/Ferroma/blob/main/README.md) 短：README 讲的是 crate 图谱、测试套件和从源码构建，这些都不是镜像使用者需要的内容。请让本文件与镜像契约保持一致，端口、数据卷和运行身份以 [`Dockerfile`](https://github.com/z1HwanG/Ferroma/blob/main/Dockerfile) 实际产出的为准。

### 一句话描述（单行字段）

> 一个 Rust 原生自托管邮件平台：自带 SMTP 与 IMAP 服务器、Maildir 存储、Webmail、Admin 管理后台和客户端 API，单一二进制，不需要 Postfix 或 Dovecot。

### 概述

**Ferroma 是一套从协议层自下而上构建的完整邮件系统。** 自带的 SMTP 与 IMAP 服务器、自有的 MIME 与邮件核心、自有的存储引擎，外加 Webmail、Admin 管理后台，以及使用专有同步协议的官方客户端。它没有包裹 Postfix、Dovecot 或 Stalwart：重点就是掌握整条技术栈。

本镜像是在最小化 Debian 基础镜像上运行的服务器二进制 `ferroma`。

#### 镜像里有什么

| | |
|---|---|
| 入口 | `ferroma`，默认命令 `serve --config /etc/ferroma/ferroma.toml` |
| 运行身份 | uid/gid `10001`（`ferroma`），绝不以 root 运行。`NET_BIND_SERVICE` 以文件能力授予，因此仍能绑定 25/587/143 |
| 端口 | `25` SMTP（收信 MX）、`587` 提交、`465` SMTPS、`143` IMAP、`993` IMAPS、`8080` HTTP API + Webmail + Admin。`465` 与 `993` 在随附配置中处于关闭状态 |
| 数据卷 | `/var/lib/ferroma`：Maildir、附件的二进制存储、DKIM 私钥 `/var/lib/ferroma/dkim/<selector>.private`，以及 `<data_dir>/database.json`（记住的数据库地址，权限 0600，内含数据库密码）。**这是需要备份的数据卷。** `ferroma storage export` 把它和数据库写成一份归档；定时，以及把这份归档带离这台机器，仍由运维自己负责 |
| 配置 | 从 `/etc/ferroma/ferroma.toml` 读取；镜像设置了 `FERROMA_CONFIG` 与 `FERROMA_DATA_DIR` |
| 健康检查 | `ferroma healthcheck --url http://127.0.0.1:8080/api/v1/health` |
| 前端 | Webmail 与 Admin 已内置在 `/usr/share/ferroma/{web,admin}`，两者共用的模块在 `/usr/share/ferroma/shared` |
| 关闭 | `SIGTERM` 会停止监听，等待进行中的 SMTP 事务与队列投递完成后再退出 |

#### 运行它

唯一受支持的部署方式是 [`docker-compose.yml`](https://github.com/z1HwanG/Ferroma/blob/main/docker-compose.yml)。该文件只启动 Ferroma，PostgreSQL 须单独提供。首次运行引导页收集数据库连接信息。需要同时备份 PostgreSQL 和 Ferroma 数据卷（见 [`docs/deployment.md`](https://github.com/z1HwanG/Ferroma/blob/main/docs/deployment.md) §8）。文件位于 [仓库](https://github.com/z1HwanG/Ferroma)：

```bash
git clone https://github.com/z1HwanG/Ferroma && cd Ferroma
cp .env.example .env      # 将 FERROMA_VERSION 设为要部署的镜像标签
FERROMA_VERSION=0.1.14 docker compose -f docker-compose.yml up -d
```

单个容器是同一种部署，只是数据库在别处。PostgreSQL 必须能从容器内访问，并且 schema 已经存在。不要设置 `DATABASE_URL`：容器会启动，并在 `/` 提供一个询问地址的页面，一次性代码打印在日志里，答案记在数据卷中。其余由首次运行向导收集，邮件域、主机名、公开 URL 都归向导所有；未设置 `FERROMA_JWT_SECRET` 时，JWT 密钥会生成到数据卷（`<data_dir>/jwt_secret`）。以下 `docker run` 示例不挂载证书；完成 TLS 配置并确保端口可用之前，465 和 993 保持关闭。生产 Compose 使用宿主机网络，邮件监听器直接绑定宿主机端口：

```bash
docker run -d --name ferroma \
  -p 25:25 -p 587:587 -p 143:143 -p 8080:8080 \
  -v ferroma-data:/var/lib/ferroma \
  wesukilaye/ferroma:0.1.14
```

完整的首次运行流程（迁移、第一个域、第一位管理员、DKIM，以及你必须发布的 DNS 记录）见 [`docs/deployment.md`](https://github.com/z1HwanG/Ferroma/blob/main/docs/deployment.md)，其余文档在 [`docs/`](https://github.com/z1HwanG/Ferroma/tree/main/docs) 目录。DNS 部分不是可选项：SPF、DKIM、DMARC 或 PTR 记录写错的 Ferroma 再正确，邮件也会落进 Junk。

#### 标签

| 标签 | 含义 |
|---|---|
| `0.1.14` | 精确版本，生产环境请固定此标签 |
| `latest` | 最新发布版 |

为 `linux/amd64` 与 `linux/arm64` 构建。一次发布会推送 `X.Y.Z` 与 `latest` 两个标签，别无其他：刻意没有滚动的 `X.Y` 标签，也没有 `buildcache` 标签。预发布版只推送自己的精确标签，`latest` 永远不会指向候选发布版。

#### 在公网 25 端口部署之前

你需要一台有公网 IP 的主机、一个 `MX` 记录指向它的域名、匹配的 `PTR` 记录，以及出站 25 端口开放。缺少这些，无论服务器本身多正确，邮件都会被拒绝，见 [`docs/deployment.md`](https://github.com/z1HwanG/Ferroma/blob/main/docs/deployment.md) §2。

### 许可证

AGPL-3.0-only —— 自由运行、修改、自托管。对服务端软件要紧的那一条：修改版若作为网络服务提供给别人，必须一并提供其源码。
