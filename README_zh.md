# Ferroma

**基于 Rust 的自建邮件平台。**

Ferroma 是一套从协议层开始自建的完整邮件系统：自己的 SMTP 与 IMAP 服务、自己的 MIME 与邮件核心、自己的存储引擎，在此之上还有 Webmail 网页客户端、管理后台，以及一套通过专用同步协议与之通信的官方跨平台客户端。

它**不包装** Postfix、Dovecot、Stalwart 或任何其它邮件服务器。这个项目的意义就在于自己拥有整条技术栈。

```text
                              FERROMA
                                 │
      ┌──────────────────────────┼──────────────────────────┐
      │                          │                          │
   Webmail                 官方客户端                     Admin
      │                          │                          │
      │                    ┌─────┼─────┐                    │
      │                    ▼     ▼     ▼                    │
      │                  Win   Linux  macOS                 │
      │                                                      │
      └──────────────────────────┼──────────────────────────┘
                                 │
                        Client API (FCP) / HTTP API
                                 │
                        ┌────────▼────────┐
                        │  Ferroma Core   │
                        └────────┬────────┘
                                 │
       ┌───────────┬─────────────┼─────────────┬───────────┐
       ▼           ▼             ▼             ▼           ▼
     SMTP        IMAP          存储          队列         DNS
       │           │             │             │           │
       └───────────┴─────────────┼─────────────┴───────────┘
                                 │
                            事件总线 ── WebSocket ── 推送
                                 │
                            PostgreSQL
```

---

## 快速开始

官方支持的部署方式是 Docker Compose。你需要一台有公网 IP 的主机、一个 `MX` 记录指向它的域名，以及放行 25 端口。

```bash
git clone https://github.com/z1HwanG/Ferroma && cd Ferroma
cp .env.example .env          # 填写 POSTGRES_PASSWORD、FERROMA_HOSTNAME、FERROMA_JWT_SECRET
docker compose up -d
docker compose logs -f ferroma
```

然后打开 `http://localhost:8080`，跟着首次运行向导走一遍即可。要作为真正的 MX 使用——TLS、DKIM、投递率——请改 `docker-compose.prod.yml`，并按 [`docs/zh/deployment.md`](docs/zh/deployment.md) 操作；其中 DNS 那一节不是可选项。

### 想直接拉镜像，不在本机构建？

每个发布版本都以 `wesukilaye/ferroma` 发布到 Docker Hub，覆盖 `linux/amd64` 与 `linux/arm64`。上面首次 `docker compose up -d` 会在容器里把整个 Rust 工作区编译一遍——10–30 分钟，外加数 GB 构建缓存——所以在服务器上直接拉取要快得多：

```bash
docker pull wesukilaye/ferroma:0.1.0
```

`docker-compose.prod.yml` 与 `docker-compose.external-db.yml` 的默认仓库已经是它，在 `.env` 里锁定版本即可：

```bash
FERROMA_VERSION=0.1.0                      # docker-compose.prod.yml：要拉取的标签
# FERROMA_IMAGE=wesukilaye/ferroma:0.1.0   # docker-compose.external-db.yml：整串引用
```

可用标签：`0.1.0`（精确版本）、`0.1`（该 minor 的最新补丁）、`latest`（最新发布）。要可复现的部署请锁定精确版本，不要用 `latest`。

### 服务器上已经有 PostgreSQL 和反向代理？

那就不需要手工拼装了，一条命令跑完整条首次部署，而且**完全不碰你数据库服务的配置**：容器与宿主机共享网络命名空间，所以你已有的 PostgreSQL 就是 `127.0.0.1:5432`，反向代理则访问 `127.0.0.1:18080` 上的 API。

```bash
git clone https://github.com/z1HwanG/Ferroma && cd Ferroma
./scripts/deploy.sh
```

它会：生成 `.env`（随机密码）、建角色与库、构建镜像、执行迁移、把证书以 uid 10001 装进 `./tls` 供 SMTP/IMAP 的 TLS 使用、启动整套服务、创建第一个管理员、生成 DKIM 密钥，最后打印还需发布的 DNS 记录和可直接粘贴的反向代理片段。之后用 `./scripts/deploy.sh status | logs | backup | upgrade | restore | dkim | certs | doctor | down`。每一步的细节与取舍见 [`docs/zh/deployment.md`](docs/zh/deployment.md) §3.1。

### 不用 Docker，直接运行

一条命令就会建库、执行迁移，并打印下一步做什么——整条首次运行路径都不需要碰 `psql`：

```bash
cargo build --release --bin ferroma

# 指向你能连上的 PostgreSQL，其余全部有默认值。
export DATABASE_URL=postgres://ferroma:secret@localhost:5432/ferroma

./target/release/ferroma database init        # 建库，然后迁移
./target/release/ferroma domain create example.com
./target/release/ferroma user create you@example.com --admin
./target/release/ferroma serve
```

`ferroma serve` 会绑定 SMTP 的 25 与 587、IMAP 的 143，以及 8080 上的 API / Webmail / Admin——并且**打印实际绑定的地址**，所以「某个端口被你没料到的进程占着」这种情况立刻可见：

```text
smtp      0.0.0.0:25, 0.0.0.0:587
imap      0.0.0.0:143 (starttls), 0.0.0.0:0 (tls)
http      http://0.0.0.0:8080/api/v1
webmail   http://0.0.0.0:8080/
admin     http://0.0.0.0:8080/admin
```

在另一个终端验证：

```bash
ferroma healthcheck            # 容器 HEALTHCHECK 跑的就是这条
curl -s localhost:8080/api/v1/health
```

### 先做一次预检

`ferroma doctor` 会把 `serve` 需要的一切都检查一遍，在你自己撞上之前就告诉你哪里会出问题。只要有会导致服务器无法正常工作的项，它就返回非零退出码：

```text
data-dir    ok    /var/lib/ferroma 可写
database    ok    ferroma，已应用 2 个迁移
ports       warn  回环地址被其它进程占用：http 0.0.0.0:8080
                  绑定 0.0.0.0 会成功，但另一个进程占着同端口的 127.0.0.1，
                  于是本机上的客户端实际连到的是那个进程。
tls         skip  未启用：STARTTLS、SMTPS、IMAPS、HTTPS 均不可用
jwt-secret  warn  未设置 api.jwt_secret
                  进程重启会让所有会话失效。

没有阻断性问题，有 2 条警告值得一读。
```

这里的每一项检查都是因为**真的出过一次**才写下的。端口那一项尤其重要，它抓的是一种本来完全看不见的故障：即使另一个进程占着 `127.0.0.1:8080`，绑定 `0.0.0.0:8080` 依然会成功——服务器报告启动正常，而同一台主机上的所有客户端都在跟另一个程序说话。

### 在责怪服务器之前，先查 DNS

这三十秒很值，因为搞错了会看起来像 Ferroma 的 bug，而实际上是你解析器的事实。

收信时的 SPF/DKIM/DMARC 校验跑在 SMTP 应答**之前**，所以它的 DNS 查询位于每一封邮件的关键路径上。会应答的解析器只要约 7 毫秒，你根本不会注意到；但**完全不应答**的解析器——域名还没有任何记录、内网专用域名、或 `.test` 这类保留顶级域时就是这样——每一级查询都要等满超时，对端 MTA 才拿到那个 `250`：

```bash
ferroma config check --dns-domain example.com
# dns          ok (3 MX host(s) for example.com in 8 ms)
# ……或者……
# dns          FAILED for example.com: no answer within 5s
```

如果失败，要么把 `[dns] resolvers` 指向一个会应答的解析器，要么调低 `dns.timeout_secs`，要么接受「在该域名有记录之前，收信一直很慢」这个事实。每封邮件第一次查询的代价就是 `dns.timeout_secs × (attempts + 1)`，所以它是整个服务器上对延迟最敏感的一项设置。

还有一个坑值得知道：`example.com`、`example.net`、`example.org` 是**真实域名，而且 DNS 是「敌对」的**——IANA 为它们发布了 `v=spf1 -all` 和 `p=reject`。把其中一个当本地域来托管，意味着你自己用户发的信会 SPF 与 DMARC 失败、被隔离进垃圾邮件。请使用你自己控制的域名，或确实没有任何记录的保留顶级域（`.test`、`.invalid`、`.localhost`）。

---

## 完成度

| 组件 | 状态 |
|---|---|
| `ferroma-core` — 配置、错误、类型化 ID、地址、限制、日志 | 完成 |
| `ferroma-mail` — RFC 5322 + MIME 解析与构建、头部、标志、信封 | 完成 |
| `ferroma-storage` — PostgreSQL schema、仓储层、Maildir、内容寻址附件 | 完成 |
| `ferroma-events` — 类型化事件总线，支持断线重连回放 | 完成 |
| `ferroma-auth` — Argon2id、HS256 访问令牌、轮换式刷新令牌、设备、限流 | 完成 |
| `ferroma-smtp` — 服务端、出站客户端、MX 解析、DKIM/SPF/DMARC、收信策略 | 完成 |
| `ferroma-imap` — IMAP4rev1 服务端，含 IDLE、APPEND、MOVE、EXPUNGE | 完成 |
| `ferroma-sync` — 变更日志、游标、幂等客户端操作 | 完成 |
| `ferroma-api` — REST API、Ferroma 客户端协议、WebSocket、前端托管 | 完成 |
| `server` — `ferroma` 二进制及其运维命令 | 完成 |
| Webmail、Admin | 完成 |
| `client` — 官方桌面客户端**核心**（同步、缓存、发件箱、搜索、多账户） | 完成 |
| `client` — 桌面端**图形外壳** | 未构建 |

上表最后一行以上的全部内容，都由下面的测试体系验证，其中包括一项验收测试：启动真实服务器、通过 SMTP 投递一封信、用 IMAP 读回来、再通过 API 找到它，并用真实的桌面客户端完成同步。

```text
cargo test --workspace    →  2376 通过，0 失败，0 跳过
cargo clippy --workspace  →  0 告警，0 错误
```

官方客户端目前交付的是「经过测试的共享核心 + 命令行」。项目书 §51 描述的三栏式图形界面，是唯一尚未构建的部分。

完整规格见 [`Ferroma-完整项目书.md`](Ferroma-完整项目书.md)（中文，64 章）。仓库约定见 [`AGENTS.md`](AGENTS.md)。

---

## 文档

| 文档 | 内容 |
|---|---|
| [`architecture.md`](docs/zh/architecture.md) | 整体架构、crate 依赖图，以及为什么长成这样 |
| [`smtp.md`](docs/zh/smtp.md) | 收信与发信 SMTP、应答码、防开放中继策略 |
| [`imap.md`](docs/zh/imap.md) | IMAP4rev1、文件夹、UID、标志、客户端兼容性 |
| [`storage.md`](docs/zh/storage.md) | 数据库结构、Maildir、配额、附件、一致性 |
| [`api.md`](docs/zh/api.md) | 全部 HTTP 接口，含示例 |
| [`fcp.md`](docs/zh/fcp.md) | Ferroma 客户端协议：同步游标、实时事件、设备 |
| [`sync.md`](docs/zh/sync.md) | 同步模型深入说明 |
| [`security.md`](docs/zh/security.md) | 威胁模型、各项控制，以及已知缺口 |
| [`deployment.md`](docs/zh/deployment.md) | DNS、TLS、备份、升级、故障排查 |
| [`client.md`](docs/zh/client.md) | 官方客户端的架构与功能 |

英文原版位于 `docs/` 下的同名文件。两版内容一一对应，章节编号与代码块完全相同，
可以逐节对照阅读。

---

## 从源码构建

Ferroma 面向稳定版 Rust 与 PostgreSQL 14+。

```bash
cargo build --release --bin ferroma
cargo test --workspace
```

### 参与开发

开发环境的细节——包括构建这台机器时的两处怪癖——都写在 [`AGENTS.md`](AGENTS.md) 里，因为它们描述的是某一个工作副本，而不是 Ferroma 本身。

这里只重复一条与**代码**有关、与机器无关的测试规则：需要数据库的集成测试读取 `FERROMA_TEST_DATABASE_URL`，数据库不可达时**不会静默跳过**——被夹具计为「通过」的跳过是一种虚假的绿色，而本仓库已经吃过一次这个亏。数据库不可达会让测试失败；要跳过必须显式设置 `FERROMA_TEST_SKIP_WITHOUT_DATABASE=1`。

---

## 仓库结构

```text
crates/
  ferroma-core/      配置、错误、类型化 ID、地址、限制、日志
  ferroma-mail/      RFC 5322 + MIME：解析、构建、头部、标志、信封
  ferroma-storage/   PostgreSQL 仓储层 + Maildir + 附件内容寻址存储
  ferroma-auth/      Argon2id、会话、令牌、设备、登录限流
  ferroma-events/    事件总线（mail.received、mail.updated……）
  ferroma-smtp/      SMTP 服务端、SMTP 客户端、MX 解析、DKIM/SPF/DMARC
  ferroma-imap/      IMAP4rev1 服务端
  ferroma-sync/      变更日志、游标、幂等客户端操作
  ferroma-api/       REST API、Ferroma 客户端协议、WebSocket、前端
server/              `ferroma` 二进制
client/              官方桌面客户端
web/, admin/         Webmail 与 Admin 单页应用（无构建步骤）
migrations/          PostgreSQL DDL，编译期嵌入二进制
config/              ferroma.toml —— 同时作为内嵌默认配置
docs/                架构、协议与运维文档
scripts/             开发与部署辅助脚本
tools/               crates 代理与 HTTPS 下载器
```

---

## 设计准则

1. **协议正确性优先于功能数量。** SMTP 与 IMAP 的行为是针对真实命令序列测试出来的，不是假定的。
2. **只有一个邮件核心。** SMTP、IMAP、Webmail 与客户端 API 都不各自实现邮件处理，一律经过 `ferroma-mail` 与仓储层。
3. **服务器永远不会成为开放中继。** 未认证的对端只能投递到本地域。这在协议边缘强制执行，而不是「寄希望于配置正确」。
4. **服务器是唯一事实来源。** 客户端只持有缓存与游标，真相在服务端。
5. **用户输入不会因为网络故障而丢失。** 客户端操作带幂等键，请求中途崩溃也能安全重试。
6. **全面使用 rustls。** 不用 OpenSSL、不用 schannel——只有一份 Rust 实现的 TLS。
7. **绝不记录密钥或邮件正文。**

## 许可

MIT OR Apache-2.0。
