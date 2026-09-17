# 部署

**谁应该读这一份：** 正在搭建 Ferroma 服务器的运维人员，以及当它停止投递邮件时被呼叫的那位。

本文是完整的运维路径：首次启动前需要创建的 DNS 记录、该用哪个 compose 文件以及何时用、
必须设置的环境变量、端口表、TLS 方案（由 Ferroma 终结还是交给反向代理）以及如何取得
Let's Encrypt 证书、首次运行设置、创建域与用户、生成并发布 DKIM 密钥、备份与恢复流程
以及为什么顺序很重要、监控与健康检查、升级与回滚，以及一节按症状编排的故障排查，每个
症状都配上用于诊断的命令。

> **状态：** 部署产物是真实且完整的 —
> `Dockerfile`、`docker-compose.yml`、`docker-compose.prod.yml`、
> `docker-compose.external-db.yml`、`.env.example`、`config/ferroma.toml`、
> `scripts/deploy.sh`、`scripts/backup.sh`、`scripts/restore.sh`。
> `ferroma` 二进制实现了本文用到的全部子命令：`serve`、`config check|show|default`、
> `database init|status`、`migrate`、`user`、`domain`、`dkim`、`storage`、`sync`、
> `healthcheck`、`doctor`、`version`。
>
> **例外只有一处：HTTPS 监听。** `api.tls_port`（默认 8443）会被配置读取，但**从未被绑定**
> —— HTTP API 只提供明文端口（`api.port`，默认 8080）。Webmail / Admin / API 的 HTTPS
> 必须由反向代理终结，见 §5.3 与 §5.4。SMTP 与 IMAP 的隐式 TLS（465 / 993）由 Ferroma
> 自己终结，但要注意 `smtps_port` / `imaps_port` 默认是 `0`（关闭），必须显式打开。

---

## 1. 一次部署由什么组成

```text
                       Internet
                           │
        ┌──────────────────┼───────────────────┬──────────────┐
        │                  │                   │              │
      :25                :587/:465           :143/:993      :443
    inbound MX         submission           IMAP           HTTPS (API,
        │                  │                   │            Webmail, Admin)
        └──────────────────┴───────────────────┘              │
                           │                                  │
                  ┌────────▼──────────────────────────────────▼────────┐
                  │                  ferroma container                 │
                  │  one process: SMTP, IMAP, HTTP API, queue workers, │
                  │  sync service, event bus                          │
                  └────────┬───────────────────────────────┬──────────┘
                           │                               │
                  ┌────────▼────────┐            ┌─────────▼──────────┐
                  │ postgres:16     │            │ volume ferroma-data│
                  │ (internal only) │            │  mail/ attachments/│
                  └─────────────────┘            │  tls/ backups/     │
                                                 └────────────────────┘
```

最少两个容器。PostgreSQL 从不发布到宿主机：它位于 `ferroma-internal` 上，只有
`ferroma` 服务能访问它。

> **如果这台服务器上已经有 PostgreSQL**（以及负责 443 的反向代理），就别再起第二个数据库
> 容器：用 §3.1 的 `docker-compose.external-db.yml` 与 `./scripts/deploy.sh`，那里只有
> Ferroma 自己、一个备份边车，数据库是本机已有的那一个。

开始之前的要求：

| 要求 | 原因 |
|---|---|
| 一台具有静态公网 IPv4 地址的宿主机 | MX 需要稳定地址，PTR 记录必须与之匹配 |
| 端口 25 **入站**可达 | 接收来自其它服务器的邮件。许多 VPS 供应商默认封锁它，开始之前先请他们解除封锁 |
| 端口 25 **出站**可达 | 投递邮件。有些供应商封锁出站 25，迫使你走中继 |
| 一个你控制的域 | 下文的 `example.com` |
| Docker Engine 24+ 与 Compose 插件 | `docker compose`，不是 `docker-compose` |
| 约 4 GB 内存、2 vCPU、20 GB 磁盘 | 够小型部署使用；邮件存储会持续增长 |

---

## 2. DNS 记录

**先做这件事。** 项目书 §42 与 §54：配置完美但没有 DNS 记录的 Ferroma，全部发信都会被
拒收，也收不到任何邮件。首次启动前，把下面每一条记录都创建好。

下文示例区是 `example.com`，服务器是 `203.0.113.10` 上的 `mail.example.com`。两者都要
替换成你自己的。

### 2.1 记录一览表

| 类型 | 名称 | 值 | TTL | 用途 |
|---|---|---|---|---|
| `A` | `mail.example.com` | `203.0.113.10` | 3600 | 服务器地址 |
| `AAAA` | `mail.example.com` | `2001:db8::10` | 3600 | 可选；没有可用的 IPv6 就省略，坏掉的 AAAA 会破坏投递 |
| `MX` | `example.com` | `10 mail.example.com.` | 3600 | 该域的邮件去往何处 |
| `PTR` | `10.113.0.203.in-addr.arpa` | `mail.example.com.` | 3600 | 反向 DNS，在托管服务商处设置，不在你自己的区里 |
| `TXT` | `example.com` | `"v=spf1 mx -all"` | 3600 | SPF：只有这台主机可以发信 |
| `TXT` | `default._domainkey.example.com` | `"v=DKIM1; k=rsa; p=…"` | 3600 | DKIM 公钥，来自 §7 |
| `TXT` | `_dmarc.example.com` | `"v=DMARC1; p=quarantine; rua=mailto:dmarc@example.com; adkim=r; aspf=r"` | 3600 | DMARC 策略与报告 |
| `TXT` | `_mta-sts.example.com` | `"v=STSv1; id=20260916000000"` | 3600 | MTA-STS 策略版本 |
| `CNAME` | `mta-sts.example.com` | `mail.example.com.` | 3600 | 策略文件的提供位置 |
| `TXT` | `_smtp._tls.example.com` | `"v=TLSRPTv1; rua=mailto:tlsrpt@example.com"` | 3600 | TLS 报告（可选但有用） |
| `CAA` | `example.com` | `0 issue "letsencrypt.org"` | 3600 | 只有这个 CA 可以签发证书 |

### 2.2 一段 BIND 风格的区片段

```bind
$TTL 3600
$ORIGIN example.com.

; --- address ---
@               IN  A       203.0.113.10
mail            IN  A       203.0.113.10
; 只有在 IPv6 端到端确实可用时才发布 AAAA。一台宣告了 AAAA
; 却无法在其上应答的主机，会丢掉双栈发信方的邮件。
; mail          IN  AAAA    2001:db8::10

; --- mail routing ---
@               IN  MX  10  mail.example.com.

; 「null MX」表示该域不接收任何邮件。不要在已有用户的域上
; 发布它：
; @             IN  MX  0   .

; --- sender authentication ---
@               IN  TXT     "v=spf1 mx -all"

; DKIM：粘贴来自 `ferroma dkim generate` / Admin 面板的 p= 值。
default._domainkey IN TXT  "v=DKIM1; k=rsa; p=MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA..."

; DMARC。先用 p=none 收集报告，再逐步收紧。
_dmarc          IN  TXT     "v=DMARC1; p=quarantine; rua=mailto:dmarc@example.com; ruf=mailto:dmarc@example.com; adkim=r; aspf=r; pct=100"

; --- transport security ---
_mta-sts        IN  TXT     "v=STSv1; id=20260916000000"
mta-sts         IN  CNAME   mail.example.com.
_smtp._tls      IN  TXT     "v=TLSRPTv1; rua=mailto:tlsrpt@example.com"

; --- issuance control ---
@               IN  CAA     0 issue "letsencrypt.org"
@               IN  CAA     0 iodef "mailto:security@example.com"

; --- optional service hostnames (specification §42) ---
; webmail       IN  CNAME   mail.example.com.
; admin         IN  CNAME   mail.example.com.
```

### 2.3 PTR 记录

反向 DNS 在 IP 段的持有方那里设置，通常是你 VPS 供应商的控制面板。MX 没有 PTR，或者
PTR 与 `EHLO` 中的名称不一致，是合法邮件被拒收或被判为垃圾邮件最常见的原因。

```bash
# 全世界为你的地址看到的内容。它必须等于 FERROMA_HOSTNAME。
dig +short -x 203.0.113.10
# 预期：mail.example.com.
```

设置 `FERROMA_HOSTNAME=mail.example.com`，并让 PTR 与它完全一致，包括该名称的正向
`A` 记录指回同一个地址。接收方检查的就是这个往返：

```bash
dig +short mail.example.com        # -> 203.0.113.10
dig +short -x 203.0.113.10         # -> mail.example.com.
```

### 2.4 MX 优先级与备用 MX

一台服务器用一条优先级为 10 的 MX 就是对的。如果以后要加第二台，给它一个更大的优先级
数字，数字越小越优先：

```bind
@   IN  MX  10  mail.example.com.
@   IN  MX  20  mail2.example.com.
```

不要添加一条没有相同邮箱数据的第二 MX。一台收下却投不出去的备用 MX，比没有备用 MX
更糟：发信方以为邮件已被接收。

### 2.5 SPF：把它配对

| 记录 | 含义 |
|---|---|
| `v=spf1 mx -all` | 该域 MX 记录中的主机可以发信。邮件服务器自己做 MX 时的常用选择 |
| `v=spf1 a:mail.example.com -all` | 显式指定一台主机，用于 MX 在别处的情况 |
| `v=spf1 mx ip4:203.0.113.10 -all` | 双保险 |
| `v=spf1 mx ~all` | softfail：标记为可疑而不是拒收。迁移期间使用 |
| `v=spf1 mx ?all` | neutral：SPF 什么也证明不了。不要上线这个 |
| `v=spf1 mx include:_spf.other.example -all` | 加上一个也为该域发信的第三方 |

真正重要的规则：

* **SPF 记录永远只有一条。** 同一个名称上有两条 `v=spf1` TXT 记录是永久性错误，接收方
  会按 `permerror` 处理。需要合并就合并成一个字符串。
* **末尾要有 `-all`。** 没有 `all` 机制时，每个未匹配的主机都得到 neutral 结果，SPF
  就不再保护你。
* **最多十次 DNS 查询**（RFC 7208 §4.6.4），这也是 `policy.spf_max_lookups`。超过之后
  接收方返回 `permerror`。数一数你的 `include:` 和 `mx` 机制。
* **子域不继承。** 如果你从 `news.example.com` 发信，它需要自己的记录，或者来自父域的
  `redirect=`/`include:`。

```bash
# 看看接收方会看到什么。
dig +short TXT example.com | grep spf1
```

### 2.6 MTA-STS

MTA-STS 告知发信方必须对你的域使用 TLS 并验证你的证书，从而堵上
[security.md](security.md) §7.4 描述的降级机会漏洞。它同时需要一条 TXT 记录和一个
HTTPS 文件。

```bind
_mta-sts    IN  TXT     "v=STSv1; id=20260916000000"
mta-sts     IN  CNAME   mail.example.com.
```

```text
# 在 https://mta-sts.example.com/.well-known/mta-sts.txt 提供
version: STSv1
mode: enforce
mx: mail.example.com
max_age: 604800
```

| 字段 | 取值 | 说明 |
|---|---|---|
| `mode` | `none`、`testing`、`enforce` | 先用 `testing` 跑一周并读 TLS 报告，再改 `enforce` |
| `mx` | 每个 MX 主机一行 | 必须列出每一台 MX，否则发信方会拒绝你漏掉的那些 |
| `max_age` | 秒 | 发信方可以缓存该策略多久；604800 是一周 |

修改策略就意味着修改 TXT 记录中的 `id`。发信方按 `id` 缓存，所以 `id` 未变而文件变了会
被忽略。

`GET /.well-known/mta-sts.txt` 由 `ferroma-api` 提供（[api.md](api.md) §2）。把
`mta-sts.<domain>` 的 `CNAME` 指向本机，再让反向代理把该路径转发给 Ferroma 即可：
§5.4 的 nginx 片段转发全部路径，因此不需要额外规则。证书要覆盖 `mta-sts.<domain>`
（§5.5）。

### 2.7 DMARC：逐步推进

| 阶段 | 记录 | 你了解到什么 |
|---|---|---|
| 1. 监控 | `v=DMARC1; p=none; rua=mailto:dmarc@example.com` | 谁在用你的域发信，包括你忘了的中继 |
| 2. 隔离 | `v=DMARC1; p=quarantine; pct=25; rua=…` | 缓慢收紧，漏掉一个发信方只影响它四分之一邮件 |
| 3. 强制执行 | `v=DMARC1; p=reject; rua=…` | 目标状态 |

Ferroma 自身的收信策略与此一致：`policy.dmarc_failure_action` 默认是 `"quarantine"`
而不是 `"reject"`，因为被转发的邮件经常通不过 SPF 与 DKIM，却仍然是合法的
（[security.md](security.md) §8.1）。

读 `rua` 报告。一份你从不打开的 DMARC 报告等于永远停在 `p=none`，那和没有策略却以为
自己有策略是一回事。

### 2.8 验证区

```bash
# 一次全查。
dig +short MX  example.com
dig +short A   mail.example.com
dig +short -x  203.0.113.10
dig +short TXT example.com              | grep spf1
dig +short TXT default._domainkey.example.com
dig +short TXT _dmarc.example.com
dig +short TXT _mta-sts.example.com
curl -s https://mta-sts.example.com/.well-known/mta-sts.txt

# 问一个 DNSBL，你的 IP 是否已被列入（用一个真实的，这里只是示例）。
# 203.0.113.10 是文档保留地址，永远不会被列入。
dig +short 10.113.0.203.zen.spamhaus.org
```

Admin 管理后台的 DNS Health 界面（`GET /api/v1/domains/:id/dns`，[api.md](api.md) §4.4）
执行的正是这些检查，并返回一个满分 7 分的评分。

---

## 3. 三个 compose 文件

| 文件 | 用途 | TLS | Postgres | 镜像 | 额外内容 |
|---|---|---|---|---|---|
| `docker-compose.external-db.yml` | **服务器已有 PostgreSQL 与反向代理**（推荐，见 §3.1） | 由反向代理终结 HTTPS；Ferroma 自己终结 465/993 | **不要**：连本机 Postgres | `ferroma:${FERROMA_IMAGE}`，本机构建或 `docker pull` | 每夜备份边车、`network_mode: host`、`.env` 即全部配置 |
| `docker-compose.yml` | 开发、单机、先看一眼 | 关闭；端口 25/587/143/8080 明文 | `postgres:16-alpine`，默认值 | 从 `Dockerfile` 本地构建，标记为 `ferroma:dev` | — |
| `docker-compose.prod.yml` | 真正的 MX，自带数据库 | 由 Ferroma 在 465/993 终结（HTTPS 交给反向代理） | 已调优（`shared_buffers=512MB`、`wal_compression=on` 等） | `ferroma:${FERROMA_VERSION}`，发布标签，从不构建 | 每夜备份边车、资源限制、`restart: always`、日志上限、`ulimit nofile 65536` |

不要在生产环境使用 `docker-compose.yml`。它以明文提供 IMAP 与 API，没有备份边车、没有
资源限制，并且把端口 143 未加密地绑定到宿主机。

### 3.1 服务器已经有 PostgreSQL：一条命令（推荐）

如果这台 Linux 服务器**已经跑着 PostgreSQL**、也**已经有反向代理**（nginx、Caddy…）负责
443，那就不该再起第二个数据库容器：用 `docker-compose.external-db.yml`，由
`scripts/deploy.sh` 驱动。这是步骤最少、对你现有环境改动最小的一条路。

```bash
git clone … && cd ferroma
./scripts/deploy.sh
```

它会依次做完下面这些事。任何一步失败，都会打印出修复它的确切命令，而不是让你去猜：

| 步骤 | 做什么 |
|---|---|
| 1. 预检 | `docker`、compose 插件、compose 文件是否齐全 |
| 2. 收集配置 | 交互式问：邮件域、MX 主机名、管理员邮箱、数据库地址、API 端口（默认 `127.0.0.1:18080`） |
| 3. 写 `.env` | 生成随机数据库密码与 `FERROMA_JWT_SECRET`，权限 600；**它是唯一的配置文件** |
| 4. 建角色与库 | 优先用 `sudo -u postgres`（peer 认证）执行幂等的 `CREATE ROLE` / `CREATE DATABASE`；做不到就打印可直接粘贴的 SQL 并停下 |
| 5. 构建镜像 | 本机 `docker build`（首次 10–30 分钟）；`FERROMA_IMAGE` 指向仓库地址时改为 `docker pull` |
| 6. 建表 | 在容器里跑 `ferroma database init`（库不存在时也会建） |
| 7. 装证书 | 把证书以 uid 10001 装进 `./tls` 供 465/993 使用，并检查 SAN 是否覆盖 MX 主机名 |
| 8. 启动 | `docker compose up -d`，最多等 3 分钟健康检查，超时自动打印日志 |
| 9. 首次初始化 | 建域 + 管理员账号（密码只打印一次），生成 DKIM 密钥并打印要发布的 TXT 记录 |
| 10. 汇总 | 反向代理片段、仍缺哪些 DNS 记录、日常命令 |

关键取舍：

* **不改你的 PostgreSQL 配置。** 容器用 `network_mode: host`，于是本机数据库就是
  `127.0.0.1:5432`：不需要动 `listen_addresses`，不需要往 `pg_hba.conf` 里加 Docker 网段，
  也不需要 `host-gateway`。副作用是好事——SMTP/IMAP 看到的是客户端真实源 IP，登录限流与
  日志都依赖它。
* **低端口需要一点权限。** 25、587、143 都小于 1024。Docker 的 bridge 网络里内核把
  `net.ipv4.ip_unprivileged_port_start` 设为 0，所以 uid 10001 绑得上；`network_mode: host`
  用的是宿主机的网络命名空间，那里通常是 1024。镜像给二进制加了 `NET_BIND_SERVICE`
  文件能力（`Dockerfile` 里的 `setcap`）来覆盖这种情况；万一该能力没能保留下来，脚本会在
  启动前发现，并告诉你那行命令：
  `sudo sysctl -w net.ipv4.ip_unprivileged_port_start=0`。
* **`.env` 是全部配置。** 这个栈不挂载 `ferroma.toml`，任何设置都用环境变量覆盖
  （`FERROMA__SMTP__PORT=2525` 这种双下划线形式），改完 `docker compose … up -d` 生效。
* **HTTPS 仍然归你的反向代理。** Ferroma 只在 `127.0.0.1:18080` 上提供明文 API，公网侧的
  TLS 由代理终结——就是 §5.3 / §5.4 描述的做法。代理本身跑在容器里、或者公网端口不是
  443（例如容器内 80/443、宿主机发布成 180/1443）时，看 §5.4 末尾那一节：API 要绑到
  Docker 网桥地址，`--public-port` 也要一起给。
* **备份是自带的长驻边车**：每 24 小时一次，保留 14 天，写进 `ferroma-backups` 卷。

常用子命令：

```bash
./scripts/deploy.sh status              # 容器 / 健康 / 数据库
./scripts/deploy.sh logs
./scripts/deploy.sh backup              # 立刻备份一次（每夜自动跑）
./scripts/deploy.sh upgrade             # 先备份 → 重建镜像 → 重启 → 等健康
./scripts/deploy.sh restore /backups/20260916T030000Z
./scripts/deploy.sh dkim --enable       # 发布 TXT 记录之后打开签名
./scripts/deploy.sh certs               # 续期后重装证书并重启监听器（certbot deploy hook 调它）
./scripts/deploy.sh doctor
./scripts/deploy.sh down [--volumes]
```

无人值守（cloud-init、CI）时每个提示都有对应开关：

```bash
./scripts/deploy.sh --yes --domain example.com --admin admin@example.com \
  --db-password "$DB_PW" \
  --tls-cert /etc/letsencrypt/live/mail.example.com/fullchain.pem \
  --tls-key  /etc/letsencrypt/live/mail.example.com/privkey.pem
```

没有给证书时脚本会把 TLS 关掉并明确警告：那样 587 上没有 STARTTLS，邮箱客户端无法认证，
只适合先把数据库与 API 跑通。

### 3.2 开发 / 单机

```bash
cp .env.example .env
# 编辑 .env：至少要改 POSTGRES_PASSWORD 和 FERROMA_JWT_SECRET。
docker compose up -d
docker compose logs -f ferroma
```

它启动的内容：`postgres`（仅内部网络，`expose: 5432`）与 `ferroma`（端口 25、587、
143、8080；卷 `ferroma-data`、只读挂载的 `./config/ferroma.toml`、只读挂载的 `./tls`）。

### 3.3 生产（自带数据库）

```bash
cp .env.example .env
# 编辑 .env 并设置每一个 REQUIRED 变量：见 §4。
docker compose -f docker-compose.prod.yml pull
docker compose -f docker-compose.prod.yml up -d
docker compose -f docker-compose.prod.yml ps
docker compose -f docker-compose.prod.yml logs -f ferroma
```

运维上要紧的差异：

```yaml
FERROMA_TLS_ENABLED: 'true'
FERROMA_TLS_CERT: /etc/ferroma/tls/fullchain.pem
FERROMA_TLS_KEY: /etc/ferroma/tls/privkey.pem
FERROMA__API__SECURE_COOKIES: 'true'
FERROMA__SMTP__REQUIRE_TLS_FOR_AUTH: 'true'
FERROMA__IMAP__REQUIRE_TLS_FOR_LOGIN: 'true'
# 隐式 TLS 监听默认是关的（0）。缺了这两行，compose 里发布的 465/993
# 映射到的是没人监听的端口——连接被拒绝，而不是报出任何错误。
FERROMA__SMTP__SMTPS_PORT: '465'
FERROMA__IMAP__IMAPS_PORT: '993'
FERROMA_DKIM_ENABLED: ${FERROMA_DKIM_ENABLED:-false}
FERROMA_DKIM_KEY: /etc/ferroma/dkim/${FERROMA_DKIM_SELECTOR:-default}.private
```

它不发布任何 HTTPS 端口：Ferroma 没有 HTTPS 监听器（见本文开头的状态说明），Webmail /
Admin / API 由反向代理访问 `127.0.0.1:8080`——把 `127.0.0.1:8080:8080` 加进 `ports`
即可，见 §5.3。

它还挂载备份边车的输入：

```yaml
backup:
  image: postgres:16-alpine
  entrypoint: ['/bin/sh', '/usr/local/bin/backup.sh']
  # 循环，而不是跑一次就退出：一次性脚本挂在 restart: always 下会在每次重启退避后
  # 再写一份完整备份，直到磁盘写满。
  command: ['--loop']
  restart: unless-stopped
  volumes:
    - ./scripts/backup.sh:/usr/local/bin/backup.sh:ro
    - ferroma-data:/mail:ro
    - backups:/backups
```

### 3.4 你真正会敲的运维命令

```bash
# 跟一个服务的日志。
docker compose -f docker-compose.prod.yml logs -f --tail=200 ferroma

# 只重启 Ferroma（PostgreSQL 继续运行）。
docker compose -f docker-compose.prod.yml restart ferroma

# 容器内的一个 shell，以 ferroma 用户身份。
docker compose -f docker-compose.prod.yml exec ferroma sh

# 针对数据库开一个 psql 会话。
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma

# 两个卷的磁盘占用。
docker system df -v | grep -E 'ferroma-data|ferroma-postgres-data'

# 停掉一切，保留卷。
docker compose -f docker-compose.prod.yml down

# 停掉一切并销毁卷。这会删除所有邮件与用户。
# docker compose -f docker-compose.prod.yml down -v
```

---

## 4. 环境变量

把 `.env.example` 复制为 `.env`。Compose 会对它做插值，几个 compose 文件在缺少必填项时
都拒绝启动。

### 4.1 生产必填

| 变量 | 示例 | 要求方 | 说明 |
|---|---|---|---|
| `POSTGRES_PASSWORD` | `openssl rand -base64 32` | 全部 | `${POSTGRES_PASSWORD:?…}`，缺它 compose 直接失败 |
| `FERROMA_JWT_SECRET` | `openssl rand -base64 48` | 全部 | 签发访问/刷新令牌。缺它每次重启都会让所有会话失效 |
| `FERROMA_HOSTNAME` | `mail.example.com` | prod（`:?`） | 必须与 PTR 记录一致 |
| `FERROMA_PUBLIC_URL` | `https://mail.example.com` | prod（`:?`） | 用于 `.well-known/ferroma` 和链接中 |
| `FERROMA_VERSION` | `0.1.0` | prod（`:?`） | 已发布的镜像标签；prod 从不构建 |

### 4.2 常设变量

| 变量 | 默认值 | 映射到 |
|---|---|---|
| `POSTGRES_USER` | `ferroma` | 数据库角色 |
| `POSTGRES_DB` | `ferroma` | 数据库名 |
| `FERROMA_LOG_LEVEL` | `info` | `server.log_level` |
| `FERROMA_LOG_FORMAT` | `text`（dev）/ `json`（prod） | `server.log_format` |
| `FERROMA_TLS_ENABLED` | `false` | `tls.enabled` |
| `FERROMA_TLS_CERT` | — | `tls.cert_path` |
| `FERROMA_TLS_KEY` | — | `tls.key_path` |
| `FERROMA_DKIM_ENABLED` | `false` | `dkim.enabled` |
| `FERROMA_DKIM_SELECTOR` | `default` | `dkim.selector` |
| `FERROMA_DKIM_KEY` | — | `dkim.private_key_path` |
| `TRUST_PROXY_HEADERS` | `false` | `api.trust_proxy_headers` |
| `BACKUP_RETENTION_DAYS` | `14` | `scripts/backup.sh` 的保留期 |
| `SMTP_PORT`、`SUBMISSION_PORT`、`IMAP_PORT`、`HTTP_PORT` | 25、587、143、8080 | **仅开发环境**，发布端口的宿主机侧 |
| `HTTPS_PORT` | 8443 | **已废弃**：`api.tls_port` 没有监听器，HTTPS 由反向代理终结，见 §5.3 |
| `FERROMA_API_PORT` | 8080（external-db 栈默认 18080） | `api.port`，明文 HTTP API 的宿主侧端口 |

### 4.3 通用覆盖形式

任何设置都能在不编辑 `ferroma.toml` 的情况下覆盖，用双下划线作为路径分隔符：

```bash
FERROMA__SMTP__PORT=2525
FERROMA__TLS__ENABLED=true
FERROMA__API__SECURE_COOKIES=true
FERROMA__SMTP__REQUIRE_TLS_FOR_AUTH=true
FERROMA__IMAP__REQUIRE_TLS_FOR_LOGIN=true
FERROMA__API__TRUST_PROXY_HEADERS=true
FERROMA__LIMITS__MAX_MESSAGE_SIZE=52428800
```

来自 `Config::load` 的优先级：内嵌默认值 < `ferroma.toml` < 环境变量。取值会按该路径
上的默认值做类型转换，所以 `2525` 变成整数、`true` 变成布尔值。**未知键会被拒绝**，
一个拼写错误会让服务器在启动时停下，而不是悄悄让某个限制保持关闭，这正是你想要的行为。

简写别名（`config.rs` 中的 `ALIASES` 表）：

```text
DATABASE_URL                  FERROMA_HOSTNAME        FERROMA_DATA_DIR
FERROMA_LOG_LEVEL             FERROMA_LOG_FORMAT      FERROMA_SMTP_HOST
FERROMA_SMTP_PORT             FERROMA_SMTP_SUBMISSION_PORT
FERROMA_IMAP_PORT             FERROMA_API_HOST        FERROMA_API_PORT
FERROMA_API_PUBLIC_URL        FERROMA_JWT_SECRET      FERROMA_TLS_ENABLED
FERROMA_TLS_CERT              FERROMA_TLS_KEY         FERROMA_DKIM_ENABLED
FERROMA_DKIM_KEY              FERROMA_DKIM_SELECTOR
```

`FERROMA_CONFIG` 指向配置文件；镜像把它设为 `/etc/ferroma/ferroma.toml`。

### 4.4 密钥

```bash
# 生成两者，然后放进 .env。永远不要提交 .env。
openssl rand -base64 32      # POSTGRES_PASSWORD
openssl rand -base64 48      # FERROMA_JWT_SECRET
```

`.env` 已在 gitignore 中。`scripts/backup.sh` 在配置归档中显式排除 `*.env` 与
`credentials*`，因为备份卷通常比密钥存储保护得更弱。见 [security.md](security.md) §13。

---

## 5. 端口与 TLS

### 5.1 端口表

| 端口 | 服务 | 配置键 | Dev | Prod | 说明 |
|---|---|---|---|---|---|
| 25 | SMTP 收信（MX） | `smtp.port` | 已发布 | 已发布 | **必须**可从互联网访问 |
| 587 | 提交，`STARTTLS` | `smtp.submission_port` | 已发布 | 已发布 | 给你的用户的邮件客户端用 |
| 465 | SMTPS（隐式 TLS） | `smtp.smtps_port` | 未发布 | 已发布 | 需要 `tls.enabled` **且** `smtps_port = 465`（默认是 0，即关闭） |
| 143 | IMAP，`STARTTLS` | `imap.port` | 已发布 | 已发布 | |
| 993 | IMAPS（隐式 TLS） | `imap.imaps_port` | 未发布 | 已发布 | 需要 `tls.enabled` **且** `imaps_port = 993`（默认是 0，即关闭） |
| 8080 | HTTP API + Webmail + Admin | `api.port` | 已发布 | 仅回环 | 健康检查针对它运行；公网侧由反向代理终结 HTTPS |
| 8443 | *（未实现）* | `api.tls_port` | — | — | 配置里有这个键，但**没有监听器**：API / Webmail / Admin 的 HTTPS 交给反向代理，见 §5.3 |
| 5432 | PostgreSQL | — | 仅 `expose` | 仅 `expose` | 永远不要发布它 |

`Dockerfile` 中有 `EXPOSE 25 587 465 143 993 8080`。`Config::validate()` 在两个活跃的
SMTP 端口冲突时、在 `smtp.port` 为 `0` 时，或在 `tls.enabled = false` 却配置了 TLS
端口时拒绝启动。

### 5.2 方案 A — Ferroma 终结 SMTP/IMAP 的 TLS

`docker-compose.prod.yml` 采用的做法：465 与 993 由 rustls 直接提供（`network_mode: host`
的 `docker-compose.external-db.yml` 同样如此），PEM 包与私钥只读挂载。HTTPS 不在其中——
见本文开头的状态说明。两个隐式 TLS 监听默认关闭，所以要显式打开：

```bash
FERROMA__SMTP__SMTPS_PORT=465
FERROMA__IMAP__IMAPS_PORT=993
```

```bash
mkdir -p tls dkim
# 证书与私钥，无论你用什么方式取得。
ls -l tls/fullchain.pem tls/privkey.pem
```

```bash
# 在 .env 中
FERROMA_TLS_ENABLED=true
FERROMA_TLS_CERT=/etc/ferroma/tls/fullchain.pem
FERROMA_TLS_KEY=/etc/ferroma/tls/privkey.pem
```

`tls.cert_path` 是一个证书包：先是叶证书，然后是中间证书。`tls.key_path` 是 PKCS#8 或
PKCS#1。如果只设了其中一个而没设另一个，`Config::validate()` 拒绝启动：

```text
tls.cert_path and tls.key_path must be set together (or enable self_signed_fallback)
```

`tls.self_signed_fallback = true` 会在启动时、未配置 PEM 的情况下用 `rcgen` 生成一张
证书。它只用于本地开发与 CI，并且被 `tls.allow_insecure_dev_mode = true` 门控。使用自签
证书的 MX 无法被任何发信服务器验证，因此它所有的出站 TLS 都会失败。

### 5.3 方案 B — 反向代理终结 HTTPS

当别的东西已经占着 443 并管理证书时使用它，或者当你希望 HTTP 安全头集中在一处时使用
它。SMTP 与 IMAP **不**走代理；Ferroma 仍然自己终结它们。§3.1 的
`docker-compose.external-db.yml` 就是这种形态：它用 `network_mode: host`，于是不必再映射
端口，API 直接听 `127.0.0.1:18080`（`FERROMA_API_HOST` / `FERROMA_API_PORT`）。

```yaml
# 添加到 docker-compose.prod.yml 的 ferroma 服务。
    ports:
      - '25:25'
      - '587:587'
      - '465:465'
      - '143:143'
      - '993:993'
      - '127.0.0.1:8080:8080'      # HTTP，绑定到回环：代理能访问它
    environment:
      FERROMA_TLS_ENABLED: 'true'  # 465 和 993 仍然需要它
      FERROMA__SMTP__SMTPS_PORT: '465'
      FERROMA__IMAP__IMAPS_PORT: '993'
      FERROMA__API__TRUST_PROXY_HEADERS: 'true'
      FERROMA__API__SECURE_COOKIES: 'true'
```

`api.trust_proxy_headers = true` 让 Ferroma 相信 `X-Forwarded-For` 与 `X-Real-IP`。
**只在你控制的代理会设置它们时才打开**：打开而没有代理时，客户端可以伪造自己的源 IP，
从而绕过按 IP 的登录限流与连接限制。代理必须覆盖该头字段，而不是追加。把 API 绑在回环
地址上（`FERROMA_API_HOST=127.0.0.1`）是同一件事的另一半：本机之外没人能直接伪造它。

### 5.4 前置 nginx

```nginx
server {
    listen 443 ssl http2;
    server_name mail.example.com mta-sts.example.com;

    ssl_certificate     /etc/letsencrypt/live/mail.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/mail.example.com/privkey.pem;
    ssl_protocols       TLSv1.2 TLSv1.3;
    ssl_prefer_server_ciphers off;

    add_header Strict-Transport-Security "max-age=31536000" always;
    add_header X-Content-Type-Options nosniff always;
    add_header X-Frame-Options DENY always;
    add_header Referrer-Policy no-referrer always;

    client_max_body_size 30m;      # >= api.max_request_size 与 limits.max_message_size

    # MTA-STS 策略由 ferroma-api 提供，转发即可（把 mta-sts.<域名> 的 CNAME
    # 指向本机，证书也要覆盖它）。
    location /.well-known/mta-sts.txt {
        proxy_pass http://127.0.0.1:18080;
        proxy_set_header Host $host;
    }

    location / {
        proxy_pass http://127.0.0.1:18080;   # prod 栈用 8080；§3.1 的栈默认 18080
        proxy_http_version 1.1;
        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $remote_addr;   # 覆盖，绝不追加
        proxy_set_header X-Forwarded-Proto $scheme;

        # 实时套接字需要 upgrade 握手和较长的读超时。
        proxy_set_header Upgrade    $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 3600s;
    }
}
```

用 `X-Forwarded-For $remote_addr` 而不是 `$proxy_add_x_forwarded_for` 是有意为之：
追加会让客户端把自己的值放在最前面，而 Ferroma 会读第一个。

#### 反向代理本身在容器里，或公网端口不是 443

这两种情况常常一起出现：nginx 是容器，容器内照旧听 80/443，宿主机把它们发布成 180/1443
（因为宿主机的 80/443 已经给了别的服务）。这种形态下有三处和上面不同：

1. **容器访问不到 `127.0.0.1`。** 每个容器有自己的网络命名空间，宿主机的回环地址对它不可见。
   把 API 绑到宿主机在 Docker 网桥上的地址（通常是 `172.17.0.1`；`docker network inspect bridge`
   可以查，脚本也会算），再让代理连它：

   ```bash
   ./scripts/deploy.sh --api-host 172.17.0.1 --api-port 18080 --public-port 1443
   ```

   ```nginx
   # nginx 容器内：listen 照旧写 80 / 443，那是容器内的端口
   location / { proxy_pass http://172.17.0.1:18080; … }
   # 或者给 nginx 容器加 --add-host=host.docker.internal:host-gateway，
   # 然后 proxy_pass http://host.docker.internal:18080;
   ```

   宿主机侧只发布 nginx 的端口：`-p 180:80 -p 1443:443`。

2. **`FERROMA_PUBLIC_URL` 必须带端口**（`--public-port 1443` 就是在做这件事），否则通知邮件
   里的链接、以及客户端拿到的地址都会指向宿主机的 443——那里是别的服务。

3. **证书只能走 DNS-01。** HTTP-01 固定连 80、TLS-ALPN-01 固定连 443，两个都不归 nginx。
   用 DNS 服务商的 API（certbot 的 DNS 插件，或 `acme.sh --dns`）签发，再按 §5.5 装进 `./tls`；
   续期同样用 §5.5 的 deploy hook。

代价是有两件事不能用，因为它们都写死在 443 上：

* **客户端自动发现** —— 客户端只在 **443** 上取 `https://<地址里的域名>/.well-known/ferroma`
  （`alice@example.com` 查的是**裸域** `example.com`，不是 `mail.example.com`），路径和端口
  都是固定的。但这份文档只是一个小 JSON，**谁提供都行**，而且文档里的 `api` 字段就是
  `FERROMA_PUBLIC_URL + /api/v1`——也就是说它自己会告诉客户端真身在 1443。所以：

  ```bash
  # 1. 从 Ferroma 取一份现成的（内容随 FERROMA_PUBLIC_URL 变）
  curl -s https://mail.example.com:1443/.well-known/ferroma
  curl -s http://172.17.0.1:18080/.well-known/ferroma   # 或从宿主机

  # 2. 让占用 443 的那个服务把它吐出来：放成静态文件，
  #    或只把这一条路径反代给 Ferroma（映射到裸域 example.com）
  sudo mkdir -p /var/www/html/.well-known
  curl -s http://127.0.0.1:18080/.well-known/ferroma | sudo tee /var/www/html/.well-known/ferroma

  # 3. 验证：期望 200，且 api 指向 1443
  curl -s https://example.com/.well-known/ferroma
  ```

  文档里同时给出了 `imap`（993）与 `smtp`（465/587）的地址，那些端口 Ferroma 直接绑在
  宿主机上，和 HTTPS 端口无关，所以静态副本也完全够用。

  做不到、或者那份文档返回 404 时，客户端不会乱猜成别人的服务器：它会退回「猜测」
  `mail.<域名>`（443/993/587）并标记为猜测，或者直接报错。那就改手工配置：在账号里填
  `https://mail.example.com:1443/api/v1`，IMAP 填 `mail.example.com:993`。

* **MTA-STS**（RFC 8461 规定从 443 取策略）——跳过它，或者让占用 443 的那个服务来提供这份
  策略文件（同样可以只放一份静态文件）。

如果你能在上游做端口转发（把公网的 443 转到这台机器的 1443），这两件事就都回来了：那时
`FERROMA_PUBLIC_URL` 用不带端口的 `https://mail.example.com`，也不要传 `--public-port`。

### 5.5 Let's Encrypt

证书必须覆盖你的用户与对端连接的名称：至少要有 `mail.example.com`（SMTP、IMAP 与
API），如果你从 `mta-sts.example.com` 提供策略，还要加上它。

```bash
# certbot，HTTP-01。端口 80 必须可达。
sudo certbot certonly --standalone \
  -d mail.example.com -d mta-sts.example.com \
  --agree-tos -m admin@example.com --no-eff-email

# 文件落在这里；把它们符号链接或复制到 ./tls/。
sudo ls -l /etc/letsencrypt/live/mail.example.com/
#   fullchain.pem  -> tls/fullchain.pem
#   privkey.pem    -> tls/privkey.pem
```

```bash
# 复制到挂载目录，并使用容器期望的属主
# （镜像以 uid 10001 运行）。
sudo install -o 10001 -g 10001 -m 0640 \
  /etc/letsencrypt/live/mail.example.com/fullchain.pem tls/fullchain.pem
sudo install -o 10001 -g 10001 -m 0600 \
  /etc/letsencrypt/live/mail.example.com/privkey.pem   tls/privkey.pem
docker compose -f docker-compose.prod.yml restart ferroma
```

注意：

* **端口 80 必须空闲且可达**，HTTP-01 才能用。如果你的反向代理已经占着 80（很常见），
  用 webroot 而不是 `--standalone`：让代理把 `/.well-known/acme-challenge/` 指到一个目录，
  签证书时用 `certbot certonly --webroot -w /var/www/html -d mail.example.com`。
* **`acme.sh` 配 DNS-01** 完全绕开端口问题，还覆盖通配符；在代理之后，或在端口 80 被
  占用的宿主机上，它是更好的选择。
* **续期要重新装载，不是重新签名。** Ferroma 只在启动时读 PEM、不监视文件，所以续期之后
  必须重新装一遍再重启。用 §3.1 的栈时这是一条命令，把它挂成 certbot 的 deploy hook
  即可全自动：

  ```bash
  # /etc/letsencrypt/renewal-hooks/deploy/ferroma.sh  (chmod +x)
  #!/bin/sh
  cd /path/to/ferroma && ./scripts/deploy.sh certs >> /var/log/ferroma-certs.log 2>&1
  ```

  没挂 hook 时，续期后手动执行 `sudo ./scripts/deploy.sh certs`（它做三件事：把 PEM 以
  uid 10001 装进 `./tls`、必要时更新 `.env`、重启监听器并等健康检查）。nginx 那边的证书
  由它自己 reload，两边互不干扰。
* **在这台开发机上**，记住 `AGENTS.md` §1.1：Windows 的 TLS 栈是坏的
  （`schannel` → `SEC_E_NO_CREDENTIALS`），所以 `curl.exe`、`git` 与 .NET 都无法走
  HTTPS。一次性下载请用 `node tools/fetch.mjs <url> <dest>`，并且永远不要添加
  `native-tls` 依赖。

```bash
# 续期演练：certbot 会真的申请一次（走 staging），并触发上面的 hook。
sudo certbot renew --dry-run
```

---

## 6. 首次运行设置

### 6.1 启动

```bash
docker compose -f docker-compose.prod.yml up -d
docker compose -f docker-compose.prod.yml ps
docker compose -f docker-compose.prod.yml logs ferroma | tail -50
```

一次健康的启动会记录解析后的配置摘要、每个监听器一行 `info`，以及
`database.run_migrations = true` 时的迁移结果。

### 6.2 设置向导

在还没有 admin 时，`GET /api/v1/setup` 返回 `{ "required": true }`（[api.md](api.md)
§4.7）。打开 `https://mail.example.com/`，Webmail 会重定向到 Admin 设置界面。

```bash
# 或者从 shell 里驱动它。
curl -s https://mail.example.com/api/v1/setup
curl -s -X POST https://mail.example.com/api/v1/setup \
  -H 'Content-Type: application/json' \
  -d '{"email":"admin@example.com","password":"…","hostname":"mail.example.com","domain":"example.com"}'
```

`POST /setup` 创建第一个 admin、该域及其主地址，并返回一对普通令牌。此后两个端点都返回
`409 conflict`。用 `api.enable_setup_wizard = false` 可以完全禁用该向导；如果你更愿意在
带外创建第一个 admin，就在 `ferroma.toml` 里设它，并记住这意味着那些端点返回 404 而不是
失败。

### 6.3 不用向导创建域

```bash
curl -s -X POST https://mail.example.com/api/v1/domains \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"example.com","description":"primary"}'
```

或者，如果 API 还没起来，直接在数据库里做：

```sql
-- 域会被转成小写；模式强制要求这一点。
INSERT INTO domains (name, description) VALUES ('example.com', 'primary')
RETURNING id, name, enabled;
```

### 6.4 创建用户与地址

```bash
# 密码由服务器用 Argon2id 哈希；永远不要手工插入哈希值。
curl -s -X POST https://mail.example.com/api/v1/users \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"email":"alice@example.com","password":"…","display_name":"Alice","quota_bytes":1073741824}'

# 给该用户挂一个地址。这会创建 Maildir 与标准文件夹。
curl -s -X POST https://mail.example.com/api/v1/users/7/mailboxes \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"domain":"example.com","local_part":"alice","is_primary":true}'
```

Maildir 与文件夹行由 `Maildir::ensure_mailbox` 与 `FoldersRepository::ensure_standard`
创建，它们产出带 `special_use` 标记的 `INBOX`、`Sent`、`Drafts`、`Trash`、`Junk`、
`Archive`（[imap.md](imap.md) §4.3）。

```bash
# 在容器里确认磁盘上的结果。
docker compose -f docker-compose.prod.yml exec ferroma \
  ls -la /var/lib/ferroma/mail/example.com/alice/Maildir
```

### 6.5 第一次端到端检查

```bash
# 手工向本地地址做本地投递。要做真实测试就用真实的 From。
printf 'EHLO test\r\nMAIL FROM:<admin@example.com>\r\nRCPT TO:<alice@example.com>\r\nDATA\r\nSubject: hello\r\n\r\nfirst\r\n.\r\nQUIT\r\n' \
  | nc 127.0.0.1 25

# 它落库了吗？
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT id, uid, subject, sender, size_bytes, storage_path FROM messages ORDER BY id DESC LIMIT 5;"
```

---

## 7. DKIM：生成并发布密钥

DKIM 签名是让你的发信不落进垃圾邮件文件夹、并让 DMARC 对齐成为可能的关键。

### 7.1 生成密钥对

```bash
# 首选：CLI 直接生成 2048 位密钥，写进域记录（也可以 --out 落一个文件）。
docker compose exec ferroma ferroma dkim generate --domain example.com

# 打印要发布的 TXT 记录：
docker compose exec ferroma ferroma dkim show --domain example.com
```

```bash
# 或者用 OpenSSL。2048 位 RSA 是接收方期望的长度；1024 太弱，4096 验证起来太慢。
openssl genrsa -out dkim/default.private 2048
openssl rsa -in dkim/default.private -pubout -out dkim/default.public

# TXT 记录的值，写在一行里：
printf 'v=DKIM1; k=rsa; p=%s\n' \
  "$(openssl rsa -in dkim/default.private -pubout 2>/dev/null \
     | grep -v '^-----' | tr -d '\n')"

# 私钥必须只对服务用户可读（uid 10001）。
sudo chown 10001:10001 dkim/default.private
chmod 0600 dkim/default.private
```

用 §3.1 的 `docker-compose.external-db.yml` 时不需要上面这些手工步骤：
`./scripts/deploy.sh` 会把密钥生成到 `ferroma-data` 卷里的
`/var/lib/ferroma/dkim/<selector>.private`（属主天然就是 uid 10001），并打印要发布的
TXT 记录；发布之后 `./scripts/deploy.sh dkim --enable` 打开签名。

`p=` 值是 base64，可能很长。多数 DNS 提供商接受 255 字符的 TXT 记录，有些要求你把更长的
值拆成带引号的片段；`dig` 会把它们重新拼起来。如果你的提供商拒绝整个字符串，就拆分它：

```bind
default._domainkey IN TXT (
    "v=DKIM1; k=rsa; p=MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA"
    "…base64 的其余部分…"
)
```

### 7.2 发布它

添加 §2.2 中的记录，等 TTL 过去，然后验证：

```bash
dig +short TXT default._domainkey.example.com
# 预期（示例）："v=DKIM1; k=rsa; p=MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8A..."
```

### 7.3 启用签名

```toml
# config/ferroma.toml
[dkim]
enabled = true
selector = "default"
private_key_path = "/etc/ferroma/dkim/default.private"
canonicalization = "relaxed"
verify_inbound = true
add_auth_results = true
```

```bash
# 或者通过环境变量，docker-compose.prod.yml 就是这么做的。
FERROMA_DKIM_ENABLED=true
FERROMA_DKIM_SELECTOR=default
FERROMA_DKIM_KEY=/etc/ferroma/dkim/default.private
docker compose -f docker-compose.prod.yml restart ferroma
```

在 `dkim.enabled = true` 而 `dkim.private_key_path` 未设置时、在 `dkim.selector` 为空
时，或在 `dkim.canonicalization` 既不是 `relaxed` 也不是 `simple` 时，
`Config::validate()` 拒绝启动。

签名域默认按域确定，也可以用 `dkim.domain` 固定。私钥同样可以放在
`domains.dkim_private_key`（`DomainsRepository::set_dkim`）里，选一个地方就留在那里，
并且备份你选的那一个。

### 7.4 端到端验证签名

```bash
# 发一封邮件给会报告 DKIM 结果的检查器。
swaks --server mail.example.com --port 587 --tls \
      --auth PLAIN --auth-user alice@example.com --auth-password '…' \
      --from alice@example.com --to check-auth@verifier.port25.com \
      --body "dkim test"

# 或者读你发给自己那封邮件里的 Authentication-Results 头字段。
```

因为 `policy.add_auth_results = true`，投递到你自己的某个邮箱的邮件会带上判定结果，这是
确认签名程序正在运行最快的方式：

```bash
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -Atc \
  "SELECT storage_path FROM messages ORDER BY id DESC LIMIT 1"
```

### 7.5 轮换 DKIM 密钥

1. 用一个**新的 selector**（`default2`）生成新密钥。
2. 发布 `default2._domainkey`，等 TTL 在各地都过期。
3. 切换到 `dkim.selector = "default2"` 并重启。
4. 旧记录至少再保留一个 DMARC 报告周期，一个月比较从容。用旧密钥签名的邮件仍在路上，
   并且仍会被验证。
5. 之后才移除旧记录与旧密钥文件。

---

## 8. 备份与恢复

### 8.1 备份包含什么

`scripts/backup.sh` 每次运行写一个带时间戳的目录：

```text
/backups/20260916T030000Z/
├── ferroma.dump      pg_dump --format=custom --compress=6
├── schema.sql        pg_dump --schema-only：一个空但正确的结构
├── maildir.tar.gz    邮件根目录的 tar -czf
├── config.tar.gz     ferroma.toml、DKIM 密钥、TLS 材料
├── MANIFEST          version、created_at、database、postgres_version、
│                     ferroma_version、hostname
└── SHA256SUMS        sha256sum ./*，这样恢复时可以证明归档完好
```

脚本自己的头部写明了支配其余一切的规则：

> 只包含前两项之一的备份不是备份：数据库说某封邮件存在，Maildir 存着它的字节，单独恢复
> 任何一个，你得到的都是一个满是悬空行的邮箱，或一个满是孤立文件的目录。

| 单独恢复 | 用户看到什么 |
|---|---|
| 只有数据库 | 一个只有主题没有正文的收件箱，每次读取都是 `StorageError::BodyMissing` |
| 只有 Maildir | 空的文件夹；字节在磁盘上，但没有任何东西知道它们 |

### 8.2 运行它

生产栈运行一个每夜的边车：

```yaml
backup:
  image: postgres:16-alpine
  environment:
    PGHOST: postgres
    PGUSER: ${POSTGRES_USER:-ferroma}
    PGPASSWORD: ${POSTGRES_PASSWORD}
    PGDATABASE: ${POSTGRES_DB:-ferroma}
    BACKUP_DIR: /backups
    RETENTION_DAYS: ${BACKUP_RETENTION_DAYS:-14}
    BACKUP_INTERVAL_SECONDS: ${BACKUP_INTERVAL_SECONDS:-86400}
  entrypoint: ['/bin/sh', '/usr/local/bin/backup.sh']
  command: ['--loop']
  restart: unless-stopped
  volumes:
    - ./scripts/backup.sh:/usr/local/bin/backup.sh:ro
    - ferroma-data:/mail:ro
    - backups:/backups
```

`command: ['--loop']` 不是装饰：一次性脚本挂在 `restart: always` 下会**每次重启都再写一份
完整备份**，退避到一分钟一轮，直到磁盘写满。

手工运行：

```bash
# 跑一遍就退出（--once 是显式的「只跑一次」）。
docker compose -f docker-compose.prod.yml run --rm backup --once

# 前台循环：每 24 小时一次，保留 14 天。
docker compose -f docker-compose.prod.yml run --rm \
  -e BACKUP_INTERVAL_SECONDS=86400 -e RETENTION_DAYS=14 \
  backup --loop

# 离机存放，这才让备份成为备份。从宿主机上运行它：
docker run --rm -v ferroma-backups:/backups -v "$PWD:/out" alpine \
  tar -czf /out/ferroma-backups-$(date -u +%Y%m%d).tar.gz -C /backups .
```

用 §3.1 的栈时更短：`./scripts/deploy.sh backup`。

边车以只读方式挂载邮件根目录（`ferroma-data:/mail:ro`），从不写它。把归档弄出这台机器：
与邮件存储放在同一块盘上的备份不是备份，放在同一个云账号里而没有版本控制的也不是。

### 8.3 运行期间的一致性

* **数据库那一半是一次单独的 `pg_dump`**，一个瞬间的一致性快照。
* **Maildir 那一半是一次运行中的 `tar`。** Maildir 写入是原子重命名
  （[storage.md](storage.md) §4.3），所以归档可能漏掉一次正在进行的投递，但绝不会包含
  写了一半的邮件。
* 坏的方向不可能发生：Maildir 文件是在行*之前*写入的，所以运行中的备份可能产生一个孤立
  文件（无害，可清扫），但不会产生一行没有字节的记录。

要得到完全一致的一对，就在此期间停掉服务：

```bash
docker compose -f docker-compose.prod.yml stop ferroma
docker compose -f docker-compose.prod.yml run --rm backup --once
docker compose -f docker-compose.prod.yml start ferroma
```

这是保证没有事务被拆到两半的唯一办法，在小型存储上只需要几秒钟。

### 8.4 恢复

`scripts/restore.sh` 遵循项目书 §47：**数据库、然后是邮件存储、最后是配置。** 数据库放第一
位，因为它定义了应该存在什么；Maildir 第二位，这样服务器启动前每一行都已经有它的文件；
配置放最后，这样一个做了一半的恢复不会留下一个运行中的服务器指向错误的证书。

恢复要**写**邮件存储，而每夜边车只以只读方式挂载它（这是有意的），所以恢复走一个单独的
一次性容器：`docker-compose.external-db.yml` 里 `profiles: ['tools']` 的 `restore` 服务
（prod 栈里可以用同样的方式，或者照下面这样覆盖 entrypoint）。

```bash
# 只验证：检查校验和并打印清单，不恢复任何东西。服务可以照常运行。
./scripts/deploy.sh restore /backups/20260916T030000Z --verify-only

# 完整恢复：脚本会先停 Ferroma，恢复完再拉起来并等健康检查。
./scripts/deploy.sh restore /backups/20260916T030000Z

# 等价的原始命令（external-db 栈）。
docker compose -f docker-compose.external-db.yml stop ferroma
docker compose -f docker-compose.external-db.yml --profile tools run --rm \
  restore /backups/20260916T030000Z
docker compose -f docker-compose.external-db.yml up -d ferroma
```

脚本拒绝覆盖一个非空的数据库：

```text
database ferroma is not empty (19 tables). Set FORCE_RESTORE=1 to overwrite,
or restore into a fresh database.
```

那道防线是文件里最有价值的一行。悄悄合并两个邮件存储，正是运维人员丢掉一周邮件的方式，
而没有任何工具能分辨「覆盖式恢复」和「恢复错库」。真要覆盖时：

```bash
# external-db 栈：加一个 -e FORCE_RESTORE=1 就够了。
docker compose -f docker-compose.external-db.yml stop ferroma
docker compose -f docker-compose.external-db.yml --profile tools run --rm \
  -e FORCE_RESTORE=1 restore /backups/20260916T030000Z
docker compose -f docker-compose.external-db.yml up -d ferroma
```

为灾难恢复准备了部分模式，两者都会打印警告：

```bash
./scripts/deploy.sh restore /backups/20260916T030000Z --db-only
./scripts/deploy.sh restore /backups/20260916T030000Z --mail-only
```

### 8.5 恢复之后

```bash
# 1. 先做检查：没有正文的行、没有对应行的文件、计数器、uid_next。
#    查询语句与 shell 循环见 docs/storage.md §9。
docker compose -f docker-compose.prod.yml exec ferroma sh -c '
  psql "$DATABASE_URL" -Atc "SELECT COUNT(*) FROM messages WHERE expunged_at IS NULL"'

# 2. 校正那些允许漂移的计数器。
#    FoldersRepository::recount 与 MailboxesRepository::recompute_usage，
#    通过 POST /api/v1/storage/gc（需要管理员令牌）与 Admin 存储界面触发。
#    curl -s -X POST https://mail.example.com/api/v1/storage/gc \
#      -H "Authorization: Bearer $TOKEN"

# 3. 启动 Ferroma 并观察头一分钟的日志。
docker compose -f docker-compose.prod.yml up -d ferroma
docker compose -f docker-compose.prod.yml logs -f --tail=100 ferroma
```

完整的完整性流程（孤立查询、校验和循环、计数器比对、`uid_next`/`uid_validity` 规则）
在 [storage.md](storage.md) §9。每次恢复之后都运行它；恢复是那种一旦哪里出错就必然产生
不一致的操作。

### 8.6 一次恢复演练

**测试恢复，而不是测试备份。** 一份从未被恢复过的备份只是一个假设（项目书 §46：
*"必须实际测试恢复"*，恢复必须真的被测过）。

```bash
# 恢复到同一台宿主机上的临时数据库，不碰生产。
docker compose -f docker-compose.prod.yml exec postgres createdb -U ferroma ferroma_drill
docker compose -f docker-compose.prod.yml exec postgres \
  pg_restore --no-owner --no-privileges --dbname=ferroma_drill /backups/…/ferroma.dump
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma_drill -c 'SELECT COUNT(*) FROM messages;'
docker compose -f docker-compose.prod.yml exec postgres dropdb -U ferroma ferroma_drill
```

每季度做一次，并在每次模式迁移之后做一次。

---

## 9. 升级与回滚

### 9.1 升级

```bash
# 1. 先备份。永远。升级是第二可能需要它的时刻。
docker compose -f docker-compose.prod.yml run --rm backup --once
# external-db 栈上一步到位：./scripts/deploy.sh upgrade（备份 → 重建 → 重启 → 等健康）

# 2. 阅读发布说明里的迁移与配置变更。
#    一个新的必填键，或一个被移除的键，会让新版本在启动时停下
#    （未知键会被拒绝）。

# 3. 更新镜像标签。Prod 从不从源码构建。
sed -i 's/^FERROMA_VERSION=.*/FERROMA_VERSION=0.2.0/' .env

# 4. 只拉取并重建 ferroma 服务。
docker compose -f docker-compose.prod.yml pull ferroma
docker compose -f docker-compose.prod.yml up -d ferroma

# 5. 看着它起来。
docker compose -f docker-compose.prod.yml logs -f --tail=100 ferroma
```

`database.run_migrations = true` 时迁移在启动时运行。它们只向前：`migrations/` 是一个
有序列表（目前一个文件，`0001_initial.sql`），按顺序应用，没有向下迁移。这就是第 1 步
之所以是第 1 步的原因。

### 9.2 回滚

```bash
# 把镜像回滚。
sed -i 's/^FERROMA_VERSION=.*/FERROMA_VERSION=0.1.0/' .env
docker compose -f docker-compose.prod.yml pull ferroma
docker compose -f docker-compose.prod.yml up -d ferroma
```

镜像回滚**只有在模式兼容时**才有效。如果新版本应用了旧版本读不了的迁移，只回滚二进制
文件不够，你必须从升级前的备份恢复数据库：

```bash
docker compose -f docker-compose.external-db.yml stop ferroma
docker compose -f docker-compose.external-db.yml --profile tools run --rm \
  -e FORCE_RESTORE=1 restore /backups/<pre-upgrade-stamp>
docker compose -f docker-compose.external-db.yml up -d ferroma
```

当只有数据库变了时，回滚邮件存储没有必要，而从升级前的备份恢复邮件存储会*丢掉*此后收到
的每一封邮件，这就是为什么存在 `--db-only`，也是为什么它会打印警告。

### 9.3 不支持零停机

一个进程，一条事件总线（[security.md](security.md) §15.4）。在负载均衡后面跑两个副本，
会让每个用户的实时体验取决于命中了哪个副本。先扩展数据库与存储，再考虑第二个 Ferroma
进程；两者都更可能是瓶颈。

一次重启的代价是 `server.shutdown_timeout_secs` 的时长（30 秒）加上启动时间：监听器停止
接受连接，在途的 SMTP 事务与队列投递完成，然后进程退出。`docker-compose.prod.yml` 中的
`stop_grace_period: 60s` 给了它余量。这期间收到的邮件会由发信 MTA 重试，因为 SMTP 按设计
就是存储转发。

---

## 10. 监控与健康检查

### 10.1 健康端点

`GET /api/v1/health`，无需认证，驱动容器健康检查（[api.md](api.md) §2）：

```json
{
  "status": "ok",
  "version": "0.1.0",
  "protocol_version": 1,
  "uptime_secs": 84213,
  "database": { "ok": true, "server_version": "PostgreSQL 16.15",
                "pool": { "size": 4, "idle": 3, "max": 20 } },
  "smtp": { "enabled": true, "connections": 3 },
  "imap": { "enabled": true, "connections": 1 },
  "queue": { "pending": 0, "delivering": 0, "retry": 2, "failed": 1 }
}
```

数据库不可达时返回 `503` 与 `"status": "degraded"`。

```bash
docker compose -f docker-compose.prod.yml exec ferroma \
  ferroma healthcheck --url http://127.0.0.1:8080/api/v1/health

# 或者不用 CLI。
docker compose -f docker-compose.prod.yml exec ferroma \
  sh -c 'wget -qO- http://127.0.0.1:8080/api/v1/health || echo unreachable'
```

几个 compose 文件与 `Dockerfile` 中的 Docker 健康检查每 30 秒运行一次
`ferroma healthcheck --url http://127.0.0.1:8080/api/v1/health`，启动期为 20 到 30 秒，
重试 3 次。`docker-compose.external-db.yml` 里的地址跟随 `FERROMA_API_HOST` 与
`FERROMA_API_PORT`（默认 `127.0.0.1:18080`）——API 绑在哪儿，探针就打哪儿。代理在容器里
因而 API 绑到 Docker 网桥地址时（§5.4 末尾），探针也跟着换过去，不会误报 unhealthy。

### 10.2 该盯什么

| 信号 | 在哪看 | 健康 | 何时动手 |
|---|---|---|---|
| 容器健康 | `docker compose ps` | `healthy` | 连续两次 `unhealthy` |
| 重启次数 | `docker inspect -f '{{.RestartCount}}' ferroma` | 稳定 | 它在攀升 |
| `mail_queue.status = 'failed'` | `GET /api/v1/queue/stats` | 接近零 | 任何持续的非零 |
| `mail_queue.status = 'retry'` | 同上 | 很少 | 持续增长数小时 |
| 磁盘可用 | 宿主机上的 `df -h` | > 20 % | < 15 %：邮件不再被接受 |
| `mail_queue_due_idx` 积压 | `GET /api/v1/queue/stats` → `next_due_at` | 在过去或就是现在 | 落后超过一分钟 |
| IMAP/SMTP 连接数 | `/api/v1/health` | 远低于 `limits.max_connections` | 顶在上限 |
| 登录失败 | `login_attempts` | 涓涓细流 | 一阵爆发，或某个邮箱/IP 反复出现 |
| 证书到期 | `openssl s_client`、`certbot certificates` | > 21 天 | < 21 天：在失效前续期 |
| 备份新鲜度 | `/backups` 中最新的目录 | 不到 26 小时 | 超过 48 小时 |

```bash
# 队列，按状态分组。
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT status, COUNT(*) FROM mail_queue GROUP BY status ORDER BY 2 DESC;"

# 等待重试中最久的那一条。
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT id, recipient, attempts, next_attempt_at, last_status_code, left(last_error,60)
     FROM mail_queue WHERE status IN ('pending','retry')
    ORDER BY next_attempt_at LIMIT 20;"

# 存储。
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT pg_size_pretty(pg_database_size('ferroma')) AS db,
          (SELECT COUNT(*) FROM messages WHERE expunged_at IS NULL) AS live_messages,
          (SELECT COUNT(*) FROM users) AS users,
          (SELECT COUNT(*) FROM domains) AS domains;"
du -sh "$(docker volume inspect -f '{{.Mountpoint}}' ferroma-data)"

# 过去一天的登录失败。
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT email, ip, COUNT(*) FROM login_attempts
    WHERE NOT success AND created_at > NOW() - INTERVAL '1 day'
    GROUP BY email, ip ORDER BY 3 DESC LIMIT 20;"
```

### 10.3 日志

生产环境用 `server.log_format = "json"`（`FERROMA_LOG_FORMAT=json`），这是 Loki/ELK
想要的格式。每个 SMTP 会话都带上 `connection_id`、`remote_ip`、`helo`、
`authenticated_user`、`sender`、`recipient`、`message_id`、`result`、`duration`
（[security.md](security.md) §12.2），而 `connection_id` 也出现在 Ferroma 前置的
`Received:` 头字段里，所以一行日志与一个邮件头字段可以关联起来。

```bash
# 只看错误。
docker compose -f docker-compose.prod.yml logs ferroma | grep -i '"level":"ERROR"'

# 关于某个 message id 的一切。
docker compose -f docker-compose.prod.yml logs ferroma | grep '4821'

# 一次失败的投递。
docker compose -f docker-compose.prod.yml logs ferroma | grep -i 'delivery'

# 实时到来的新日志行，已过滤。
docker compose -f docker-compose.prod.yml logs -f ferroma | grep -E 'WARN|ERROR'
```

几个 compose 文件都对日志轮转设了上限（prod 中是 `max-size: 20m`、`max-file: 10`），
所以日志洪水无法塞满磁盘。不要在生产环境提高 `database.log_statements`：它会打印邮件
主题。

### 10.4 指标与告警是 _(计划中)_

项目书 §40 列出了 Prometheus 指标（`smtp_connections_total`、
`smtp_messages_received_total`、`queue_pending_messages`、`mail_storage_bytes`、
`sync_operations_total` 等），§41 把 Prometheus 与 Grafana 放在更晚的版本。两者都还
不存在：没有 `/metrics` 端点。在它出现之前，用健康端点与上面的 `psql` 查询来监控，并对
以下情况告警：

* 容器健康检查失败，
* `mail_queue.status = 'failed'` 超过你选定的阈值，
* 磁盘占用超过 85 %，
* 证书到期在 21 天以内，
* 最新的备份目录超过 48 小时。

---

## 11. 按症状排查

### 11.1 「邮件被判为垃圾邮件」/ 落进收件人的 Junk

几乎总是 DNS，不是 Ferroma。

```bash
# 1. PTR 与 FERROMA_HOSTNAME 一致吗，它指回来吗？
dig +short -x 203.0.113.10            # 必须等于 FERROMA_HOSTNAME
dig +short mail.example.com           # 必须是同一个地址
grep FERROMA_HOSTNAME .env

# 2. SPF 存在吗，是唯一一条吗，以 -all 结尾吗？
dig +short TXT example.com | grep spf1

# 3. DKIM 记录发布了吗，与你用来签名的密钥匹配吗？
dig +short TXT default._domainkey.example.com
openssl rsa -in dkim/default.private -pubout 2>/dev/null | grep -v '^-----' | tr -d '\n'

# 4. DMARC 存在吗？
dig +short TXT _dmarc.example.com

# 5. 这个 IP 在黑名单上吗？
dig +short 10.113.0.203.zen.spamhaus.org

# 6. 队列在报告带远端状态码的失败吗？
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT recipient, last_status_code, last_status_text FROM mail_queue
    WHERE status = 'failed' ORDER BY updated_at DESC LIMIT 10;"
```

常见原因按出现频率排列：没有 PTR 或 PTR 不匹配；SPF 记录缺失或重复；SPF 的 DNS 查询超过
十次；DKIM 记录发布了但 `dkim.enabled = false`，所以什么都没签；DMARC 设成 `p=reject`
却没有对齐；一个全新 IP 没有任何发信历史（这只能靠时间和量来解决）；以及一个你继承了其
声誉的共享 IP。

### 11.2 「收不到 Gmail（或另一个大厂）的邮件」

大厂要求 TLS，并且行为严格。

```bash
# 1. 来自互联网的连接到底有没有到达端口 25？
#    从你网络之外的一台机器上：
nc -vz mail.example.com 25

# 2. MX 记录能解析吗，是你以为的那台主机吗？
dig +short MX example.com
dig +short A mail.example.com

# 3. Ferroma 在容器内监听 25 吗？
docker compose -f docker-compose.prod.yml exec ferroma \
  sh -c 'netstat -tlnp 2>/dev/null || ss -tlnp'

# 4. 连接到底有没有到？如果一行日志都没有，它就从未到达这里。
docker compose -f docker-compose.prod.yml logs ferroma | grep -i 'connection\|reject\|550\|554'

# 5. 从外部看证书有效吗？
openssl s_client -starttls smtp -connect mail.example.com:25 -crlf < /dev/null 2>&1 | head -30

# 6. 问候名与 PTR 一致吗？
printf 'EHLO test\r\nQUIT\r\n' | nc mail.example.com 25
```

常见原因：主机防火墙或供应商封锁端口 25（在新的 VPS 上非常常见，请他们开放）；把 25 映射
到错误主机的 NAT/端口转发；一条宣告了主机无法提供的 IPv6 的 AAAA 记录，导致双栈发信方
超时；以及一张已过期的证书，它会让要求 TLS 的发信方（MTA-STS，或某个供应商策略）用
`4xx` 推迟投递。

### 11.3 「邮件被接收但从未到达」/「队列在增长」

```bash
# 1. 队列真的在增长吗，第一次尝试是最近的吗？
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT status, COUNT(*), MIN(next_attempt_at), MAX(created_at)
     FROM mail_queue GROUP BY status;"

# 2. 主要的失败都说了什么？
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT recipient, attempts, last_status_code, last_error
     FROM mail_queue WHERE status IN ('retry','failed')
    ORDER BY attempts DESC LIMIT 20;"

# 3. 一次卡住的投递的尝试历史。
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT a.attempt, a.remote_mx, a.status_code, a.status_text, a.duration_ms, a.created_at
     FROM delivery_attempts a JOIN mail_queue q ON q.id = a.queue_id
    WHERE q.id = 1234 ORDER BY a.attempt;"

# 4. 这台主机到底能不能在端口 25 上连到远端的 MX？
docker compose -f docker-compose.prod.yml exec ferroma \
  sh -c 'nc -vz gmail-smtp-in.l.google.com 25'

# 5. 出站 25 被供应商封了吗？从宿主机上测。
nc -vz alt1.gmail-smtp-in.l.google.com 25

# 6. 调度程序到底在跑吗？
docker compose -f docker-compose.prod.yml logs ferroma | grep -i 'queue\|dispatch'
grep -A6 '^\[queue\]' config/ferroma.toml
```

读应答码里的诊断信息：

| 症状 | 含义 | 处理 |
|---|---|---|
| 来自大厂的 `421 4.7.0 Try again later` | 被限速或 IP 声誉问题 | 放慢速度、申请移出黑名单、检查是否有账号被攻陷 |
| 反复出现 `450`/`451` | 远端在做灰名单 | 正常；重试计划会处理它 |
| 来自远端的 `550 5.7.1` | 他们的策略拒绝了你 | SPF/DKIM/DMARC/PTR，或黑名单 |
| `550 5.1.1` | 收件人不存在 | 邮件会退信；修正地址 |
| `delivery_attempts` 里什么都没有 | 工作进程从未认领该行 | 检查 `queue.enabled`、`queue.workers` 与 `mail_queue_due_idx` |
| 尝试次数爬到 `max_attempts`（12） | 持续性失败 | 读 `last_status_text` |
| 一切都卡在 `pending`，`next_attempt_at` 在过去 | 调度程序没在跑 | 在日志里找 panic；重启 |

一阵不是用户发的发信爆发，暗示有账号被攻陷：检查 `login_attempts`、`sessions`（看
`ip`）与 `mail_queue.user_id`，找出某个账号占了大头。

```sql
-- 谁发得最多？
SELECT user_id, COUNT(*) FROM mail_queue
 WHERE created_at > NOW() - INTERVAL '1 hour'
 GROUP BY user_id ORDER BY 2 DESC LIMIT 10;
```

然后吊销该账号的会话（`POST /api/v1/client/devices/:id/revoke`）并修改密码。

### 11.4 「IMAP 登录失败」

```bash
# 1. IMAP 在监听吗？
docker compose -f docker-compose.prod.yml exec ferroma \
  sh -c 'netstat -tlnp 2>/dev/null | grep -E "143|993"'

# 2. 问候语与能力列表说了什么？
openssl s_client -crlf -connect mail.example.com:143 < /dev/null 2>&1 | head -20
# 隐式 TLS：
openssl s_client -connect mail.example.com:993 < /dev/null 2>&1 | head -20

# 3. 试一次真实登录。
printf 'a LOGIN alice@example.com "…"\r\nb LOGOUT\r\n' | \
  openssl s_client -quiet -crlf -connect mail.example.com:143

# 4. 是否要求 TLS，你的客户端在做吗？
grep -E 'require_tls_for_login|imaps_port' config/ferroma.toml
grep FERROMA__IMAP__REQUIRE_TLS_FOR_LOGIN .env

# 5. 账号被登录限流锁了吗？
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT id, email, enabled, failed_logins, locked_until FROM users WHERE email='alice@example.com';"

# 6. 最近的尝试说了什么？
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT email, ip, success, created_at FROM login_attempts
    ORDER BY created_at DESC LIMIT 20;"

# 7. 服务器对这次失败的看法。
docker compose -f docker-compose.prod.yml logs ferroma | grep -i 'imap\|login'
```

| 应答 | 原因 | 修复 |
|---|---|---|
| `NO [PRIVACYREQUIRED]` | `imap.require_tls_for_login = true` 而客户端在用明文 | 在客户端里配置 `STARTTLS`（端口 143）或隐式 TLS（993） |
| `NO [AUTHENTICATIONFAILED]` | 密码错误，或该地址不是登录名 | 登录名是完整地址，小写：`alice@example.com` |
| Connection refused | 监听器挂了，或端口未发布 | `grep -A4 '^\[imap\]' config/ferroma.toml`；检查 `docker compose ps` 的端口 |
| TLS handshake error | 证书过期或不匹配 | `openssl s_client -connect mail.example.com:993 -servername mail.example.com` |
| 立刻 `* BYE Autologout` | `imap.idle_timeout_secs` 太小，或时钟有问题 | 调大它；检查宿主机时钟 |
| 本地能用，远程不行 | 防火墙，或客户端配错了主机 | 从外面执行 `nc -vz mail.example.com 993` |

一个收得了却发不出的用户，是**提交**的问题，不是 IMAP 的问题：检查端口 587、
`smtp.require_auth_on_submission`，并确认他用来发信的地址确实是他自己的
（[security.md](security.md) §6.3）。

### 11.5 「API 挂了」/ Webmail 打不开

```bash
# 1. 健康检查，从容器内部发起。
docker compose -f docker-compose.prod.yml exec ferroma \
  sh -c 'wget -qO- http://127.0.0.1:8080/api/v1/health || echo unreachable'

# 2. 容器健康吗？
docker compose -f docker-compose.prod.yml ps
docker inspect -f '{{.State.Health.Status}} restarts={{.RestartCount}}' ferroma

# 3. 它为什么退出？
docker compose -f docker-compose.prod.yml logs --tail=200 ferroma | grep -iE 'error|panic|config'

# 4. 一个配置拼写错误会让服务器在启动时停下。这是有意设计的。
docker compose -f docker-compose.prod.yml run --rm ferroma \
  ferroma serve --config /etc/ferroma/ferroma.toml
```

每个端点都返回 `500 storage_error` 意味着数据库不可达：检查
`docker compose ps postgres`、`DATABASE_URL`，以及连接池大小与
`database.max_connections` 的关系。`/health` 返回 `503` 与 `"status": "degraded"` 是
同一种状况被优雅地报告出来。

### 11.6 「配额说满了但邮箱看起来是空的」

```bash
# 数据库相信的情况。
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT m.id, d.name||'@'||m.local_part AS addr, u.used_bytes, u.quota_bytes
     FROM mailboxes m JOIN domains d ON d.id=m.domain_id JOIN users u ON u.id=m.user_id
    ORDER BY u.used_bytes DESC LIMIT 10;"

# 磁盘上实际的情况。
docker compose -f docker-compose.prod.yml exec ferroma \
  du -sb /var/lib/ferroma/mail/example.com/alice

# 构成总数的那些文件。
docker compose -f docker-compose.prod.yml exec ferroma \
  sh -c 'find /var/lib/ferroma/mail/example.com/alice -type f -printf "%s %p\n" | sort -rn | head -20'
```

`used_bytes` 是一个缓存，`MailboxesRepository::recompute_usage` 会修正它
（[storage.md](storage.md) §5）。任何方向的巨大偏差都值得调查，而不只是重算一遍：绕过
Ferroma 被删掉的文件，意味着还有别人在写邮件根目录。

### 11.7 「磁盘在变满」

```bash
# 在哪。
du -sh /var/lib/docker/volumes/ferroma-data/_data/*
du -sh /var/lib/docker/volumes/ferroma-data/_data/mail/*

# 中断写入留下的垃圾。
docker compose -f docker-compose.prod.yml exec ferroma \
  sh -c 'find /var/lib/ferroma/mail -type d -name tmp -exec du -sh {} +'

# 未被引用的附件二进制对象。
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -Atc 'SELECT DISTINCT storage_path FROM attachments' | wc -l
docker compose -f docker-compose.prod.yml exec ferroma \
  sh -c 'find /var/lib/ferroma/attachments -type f ! -name "*.tmp" | wc -l'

# 已清除但仍占着文件的邮件。
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT COUNT(*), pg_size_pretty(SUM(size_bytes)) FROM messages WHERE expunged_at IS NOT NULL;"
```

可用的杠杆，按顺序：跑清扫（`Maildir::sweep_tmp`、`AttachmentStore::gc`）；按时间清理
`delivery_attempts` 与 `login_attempts`；调低 `queue.retention_days`；然后才去看配额。
二进制对象与文件数量不一致是正常的，完全相同的附件共享同一个二进制对象，所以要比较
*大小*，而不是数量。

---

## 12. 相关文档

| 主题 | 文档 |
|---|---|
| 上面每条命令用到的端点 | [api.md](api.md) |
| SMTP 应答码、重试计划、退信格式 | [smtp.md](smtp.md) |
| IMAP 能力列表、文件夹名称、客户端兼容性 | [imap.md](imap.md) |
| 模式、Maildir 布局、配额、GC、完整性流程 | [storage.md](storage.md) |
| 威胁模型、中继防御、TLS 决策、已知缺口 | [security.md](security.md) |
| 同步模型与客户端的失败矩阵 | [sync.md](sync.md) |
| 官方桌面客户端 | [client.md](client.md) |
| crate 图与请求生命周期 | [architecture.md](architecture.md) |
| 这台机器上的构建怪癖（代理、`CARGO_HOME`、PostgreSQL） | [../AGENTS.md](../../AGENTS.md) |
