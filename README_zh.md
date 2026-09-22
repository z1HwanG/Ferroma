# Ferroma

[English](README.md) · **简体中文**

**基于 Rust 的自建邮件平台。**

Ferroma 是一套从协议层开始自建的完整邮件系统：自己的 SMTP 与 IMAP 服务、自己的 MIME 与邮件核心、自己的存储引擎，在此之上还有 Webmail 网页客户端、Admin 管理后台，以及一套通过专用同步协议与之通信的官方跨平台客户端。

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
      │                                                     │
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

唯一支持的部署方式是一个 Compose 文件，`docker-compose.yml`，它只启动 Ferroma。默认这台机器已经在运行 PostgreSQL。你还需要一台有公网 IP 的主机、一个 `MX` 记录指向它的域名，以及放行 25 端口。在宿主机上直接运行二进制不是受支持的安装方式。

```bash
git clone https://github.com/z1HwanG/Ferroma && cd Ferroma
cp .env.example .env          # 把 FERROMA_VERSION 设成要部署的版本
docker compose up -d          # 读 docker-compose.yml，也就是部署文件
docker compose logs -f ferroma
```

然后打开 `http://localhost:8080`。第一页要这台机器上已经在运行的 PostgreSQL 的主机、用户名和密码，之后的向导收其余各项。作为真正的 MX 使用所需的 TLS、DKIM、投递率见 [`docs/zh/deployment.md`](docs/zh/deployment.md)；其中 DNS 那一节不是可选项。

### 想直接拉镜像，不在本机构建？

每个发布版本都以 `wesukilaye/ferroma` 发布到 Docker Hub，覆盖 `linux/amd64` 与 `linux/arm64`。上面首次 `docker compose up -d` 会在容器里把整个 Rust 工作区编译一遍——10–30 分钟，外加数 GB 构建缓存——所以在服务器上直接拉取要快得多：

```bash
docker pull wesukilaye/ferroma:0.1.10
```

`docker-compose.yml` 的默认仓库已经是它，在 `.env` 里锁定版本即可。`docker-compose.demo.yml` 是演示，不是这条命令：

```bash
FERROMA_VERSION=0.1.10                     # docker-compose.yml：要拉取的标签
```

可用标签只有 `0.1.10`（一个精确版本）与 `latest`（最新发布）——每次发布只产出这两个，所以一个标签永远只对应一个具体版本。要可复现的部署请锁定精确版本，不要用 `latest`。

### 服务器上已经有 PostgreSQL 和反向代理？

那就不需要手工拼装了，一条命令跑完整条首次部署，而且**完全不碰你数据库服务的配置**：容器与宿主机共享网络命名空间，所以你已有的 PostgreSQL 就是 `127.0.0.1:5432`，反向代理则访问 `127.0.0.1:18080` 上的 API。

```bash
git clone https://github.com/z1HwanG/Ferroma && cd Ferroma
./scripts/deploy.sh
```

它会：生成 `.env`（随机密码）、建角色与库、构建镜像、执行迁移、把证书以 uid 10001 装进 `./tls` 供 SMTP/IMAP 的 TLS 使用、启动整套服务、创建第一个管理员、生成 DKIM 密钥，最后打印还需发布的 DNS 记录和可直接粘贴的反向代理片段。之后用 `./scripts/deploy.sh status | logs | upgrade | dkim | certs | doctor | down`。每一步的细节与取舍见 [`docs/zh/deployment.md`](docs/zh/deployment.md) §3.1。

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

上表最后一行以上的全部内容，都由下面的测试体系验证，其中包括一项验收测试：启动真实服务器、通过 SMTP 投递一封信、用 IMAP 读回来、再通过 API 找到它、跟着同步游标走一遍，并驱动 bootstrap 设置页——无数据库的服务器、setup code、向导的 POST、健康的 API——全程走真实 socket。

```text
cargo test --workspace    →  通过，0 失败，0 跳过（0.1.10，rustc 1.98）
cargo clippy --workspace  →  clippy 1.98 下 0 警告（钉住的 1.88 仍能构建；
                              `manual_is_multiple_of` 是 1.98 的 lint，且已修复）
```

### 带入 0.1.9 的待办

0.1.3 时的发现已经了结。七篇文档曾带着写于各 crate 尚不存在之时的 `_(planned)_`
断言；如今每一处都已对照代码核实，改写成关于服务器实际行为的陈述，四篇把已实现
crate 称作骨架的状态横幅也已摘掉。bootstrap 设置页也补上了它一直缺失的验收测试
（仍待办的事与缘由见 [`TODO_zh.md`](TODO_zh.md)）。

仓库约定见 [`AGENTS.md`](AGENTS.md)；本文件的英文原版见 [`README.md`](README.md)。

---

## 文档

整套文档另有一个中英双语的在线站点：<https://ferroma.z1hwang.cn/>。

| 文档 | 内容 |
|---|---|
| [`architecture.md`](docs/zh/architecture.md) | 整体架构、crate 依赖图，以及为什么长成这样 |
| [`GLOSSARY.md`](docs/zh/GLOSSARY.md) | 全部文档共用的术语来源：每样东西叫什么、不叫什么 |
| [`smtp.md`](docs/zh/smtp.md) | 收信与发信 SMTP、应答码、防开放中继策略 |
| [`imap.md`](docs/zh/imap.md) | IMAP4rev1、文件夹、UID、标志、客户端兼容性 |
| [`storage.md`](docs/zh/storage.md) | 数据库结构、Maildir、配额、附件、一致性 |
| [`api.md`](docs/zh/api.md) | 全部 HTTP 接口，含示例 |
| [`fcp.md`](docs/zh/fcp.md) | Ferroma 客户端协议：同步游标、实时事件、设备 |
| [`sync.md`](docs/zh/sync.md) | 同步模型深入说明 |
| [`security.md`](docs/zh/security.md) | 威胁模型、各项控制，以及已知缺口 |
| [`deployment.md`](docs/zh/deployment.md) | DNS、TLS、备份、升级、故障排查 |
| [`CHANGELOG_zh.md`](CHANGELOG_zh.md) | 每个版本改了什么 |
| [`TODO_zh.md`](TODO_zh.md) | 还没做完的事，以及已经决定不做的方向 |

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
web/, admin/         Webmail 与 Admin 单页应用（无构建步骤）
shared/              两个前端共用的 ES 模块，服务端挂在 /shared
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

## 参与贡献

先读 [`CONTRIBUTING_zh.md`](CONTRIBUTING_zh.md)：必须通过的检查、一个改动必须带的东西（没有它就会红的
测试），以及 DCO 签署方式。

## 许可

**AGPL-3.0-only。** 运行、修改、自托管都可以——对服务端软件真正要紧的是第 13 条：如果你把**修改过的**
版本作为网络服务提供给别人使用，就必须向他们提供该版本的源码。本仓库即是上游源码，完整正文见
[`LICENSE`](LICENSE)。
