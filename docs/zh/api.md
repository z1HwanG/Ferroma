# Ferroma HTTP API

两类使用者共用一个路由：

| 接口面 | 基础路径 | 使用者 |
|---|---|---|
| **管理 API** | `/api/v1` | Webmail、Admin 面板、脚本 |
| **客户端 API（FCP）** | `/api/v1/client` | 官方桌面客户端 |

两者都是基于 HTTP/1.1 与 HTTP/2 的纯 JSON。TLS 由 Ferroma 自身终止
（`api.tls_port`），或由前置的反向代理终止。

本页的所有内容都由`ferroma-api`实现；客户端接口面的协议级细节
（版本、游标、实时帧格式）见
[`fcp.md`](fcp.md)。

---

## 1. 约定

### 1.1 内容类型与编码

除非另有说明，请求与响应都是`application/json; charset=utf-8`。附件以
`application/octet-stream`流式传输，其真实`Content-Type`放在元数据中。字段名是
`snake_case`。

### 1.2 认证

| 接口面 | 机制 |
|---|---|
| 管理 API | `Authorization: Bearer <access_token>`**或**`ferroma_session` cookie（Webmail）|
| 客户端 API | 仅`Authorization: Bearer <access_token>` |

访问令牌是由`POST /auth/login`签发的 HS256 JWT。它们是短时效的
（`api.access_token_ttl_secs`，默认一小时）；同时获得的刷新令牌用来签发新的访问令牌。
刷新令牌是一次性的：刷新会轮换它们并使先前的值失效。

Admin 端点额外要求用户具有`is_admin`。

### 1.3 错误

每一次失败都返回同一个信封：

```json
{
  "error": {
    "code": "invalid_input",
    "message": "recipient address has no domain: bob",
    "details": { "field": "to" }
  }
}
```

`code`稳定且机器可读（它字面上就是
`FerromaError::code()`）；`message`供人阅读，可能变化。`details`是可选的，
仅当确有结构化内容要表达时才出现。

`message`遵循`Accept-Language`（RFC 9110）：任何地区的`zh`都选择简体中文
词表，其余情况（包括完全没有该请求头）一律返回英文；尚未提供译文的
信息按英文原样返回，而不是被丢掉。`code`不随语言变化，因此客户端的
分支逻辑与语言无关：

```http
GET /api/v1/domains/9999 HTTP/1.1
Authorization: Bearer …
Accept-Language: zh-CN,zh;q=0.9,en;q=0.8
```

```json
{ "error": { "code": "not_found", "message": "未找到：域名 9999" } }
```

路径匹配不到任何路由、或路径存在但不接受该方法的请求，同样返回这个
信封，而不是空响应体。

| HTTP | `code` | 含义 |
|---|---|---|
| 400 | `invalid_input`, `parse_error`, `protocol_error` | 请求格式错误 |
| 401 | `unauthorized` | 凭据缺失、过期或已吊销 |
| 403 | `forbidden` | 已认证但无权（例如不是管理员）|
| 404 | `not_found` | 没有该邮件、邮箱、草稿或设备 |
| 409 | `conflict` | 唯一性或状态冲突；原始请求**仍在处理中**的操作；比保留历史更旧的同步游标 |
| 413 | `limit_exceeded`, `mailbox_full` | 邮件过大、收件人过多，或收件人邮箱已满 |
| 426 | `unsupported` | 客户端的`X-Ferroma-Protocol`低于`client.min_protocol_version`；需要升级 |
| 429 | `rate_limited` | 放慢速度；遵循`Retry-After` |
| 500 | `storage_error`, `internal_error` | 程序缺陷或数据库故障 |
| 501 | `unsupported` | 已规定但尚未实现的能力，且协议升级也无济于事 |
| 502 | `dns_error`, `network_error`, `timeout` | 上游失败 |

对于客户端已经完成的操作，它看到的**不是**`409`。见 §1.5。

### 1.4 分页

列表端点接受`limit`（默认 50，最大 500）与`offset`，并返回：

```json
{ "items": [ … ], "total": 1234, "limit": 50, "offset": 0 }
```

增量同步**不**使用分页；它使用游标（§5）。

### 1.5 幂等

改变状态的请求接受幂等键：管理接口面上是`Idempotency-Key`头字段，
客户端 API 上是请求体中的`operation_id`。`POST`、`PATCH`
与`DELETE`都接受它。

服务器记录该操作，并在重复请求时**重放原始响应**
（相同的状态码、相同的响应体），因此超时后重试的客户端不会重复发送
或重复删除。幂等键保留时长为`client.tombstone_retention_days`。

```text
首次  POST /api/v1/client/messages  {"operation_id":"op_9f2c…", …}  -> 201 {"message_id":4821,…}
重试  POST /api/v1/client/messages  {"operation_id":"op_9f2c…", …}  -> 201 {"message_id":4821,…}
```

有三种结果值得区分，因为客户端必须对它们作出不同的反应：

| 情形 | 响应 | 客户端应做什么 |
|---|---|---|
| 幂等键是新的 | 操作照常执行 | 正常处理 |
| 幂等键已经**完成** | 记录下来的响应，原样重放 | 视为成功；**不要**重试 |
| 幂等键的原始请求**仍在运行**（或其进程在请求中途退出）| `409 conflict`，`"operation … has not finished; retry later"` | 稍后用同一个幂等键重试 |

资源已经不存在之后才重试的`DELETE`会应答`404`；正在清空排队删除的客户端
必须把它视为成功，因为期望的终态已经成立。正是这一点让`DELETE`
无需单独的墓碑 API 也安全。

### 1.6 限流

`429`响应携带以秒为单位的`Retry-After`。提交与登录按账号限流；
API 按令牌限流。

### 1.7 时间戳

UTC 下的 RFC 3339 / ISO 8601，例如`2026-09-16T12:00:00Z`。除
`operation_id`（`op_…`）与`device_uid`（客户端生成的字符串）之外，ID 都是整数。

---

## 2. 健康检查与发现

### `GET /api/v1/health`

无需认证。驱动容器健康检查。

```json
{
  "status": "ok",
  "version": "0.1.0",
  "protocol_version": 1,
  "uptime_secs": 84213,
  "database": { "ok": true, "server_version": "PostgreSQL 16.15", "pool": { "size": 4, "idle": 3, "max": 20 } },
  "smtp": { "enabled": true, "connections": 3 },
  "imap": { "enabled": true, "connections": 1 },
  "clients": { "active_sessions": 4, "active_devices": 2 },
  "queue": { "pending": 0, "delivering": 0, "retry": 2, "failed": 1, "cancelled": 0, "bounce_pending": 1, "bounce_processing": 0, "received_today": 128, "sent_today": 41 }
}
```

`clients.active_sessions`统计存活的 Webmail/API/客户端会话（`sessions`中
既未吊销也未过期的行），`active_devices`统计未吊销的
`devices`行。`queue.received_today`与`sent_today`覆盖当前 UTC 日。
`queue.bounce_pending`与`bounce_processing`统计仍欠发件人的退信：一封投递失败、
退信却还没发出的邮件，否则与普通失败无从区分，而它恰恰是运维必须处理的那个队列
状态。以上各项都供 Admin 看板使用，看板必须把缺失的数字渲染为`—`，而不是
编造出来的零。

数据库不可达时返回`503`与`"status": "degraded"`；此时
`queue`与`clients`块被省略，而不是报告为零。

### `GET /api/v1/version`

`{ "version": "0.1.0", "protocol_version": 1, "git_sha": "abc1234", "built": "…" }`

### `GET /.well-known/ferroma`

无需认证的自动发现（项目书 §32）。客户端从用户所输入地址的
域名处获取它。

```json
{
  "api": "https://mail.example.com/api/v1",
  "imap": { "host": "mail.example.com", "port": 993, "tls": true, "security": "implicit" },
  "smtp": { "host": "mail.example.com", "port": 587, "tls": true, "security": "starttls" },
  "web": "https://mail.example.com",
  "protocol_version": 1
}
```

`tls` 只说明连接是否加密。`security` 说明方式：465 与 993 是 `implicit`（从第一个字节起就是 TLS），587 与 143 是 `starttls`（先说 SMTP 或 IMAP，再升级）。把每个 `tls: true` 都当成隐式 TLS 的客户端会用这种方式打开 587，握手随即失败。隐式端口没有在监听时，文档给出明文端口和 `starttls`，而不是通告一个没人听的 465 或 993。没有配置 TLS 时省略 `security`。

### `GET /.well-known/mta-sts.txt`与`GET /api/v1/domains/:id/dns`

见 §4.4。

---

## 3. 认证（管理接口面）

### `POST /api/v1/auth/login`

```json
{ "email": "alice@example.com", "password": "…", "device_name": "Firefox on Linux", "totp": "123456" }
```

```json
{
  "access_token": "eyJ…",
  "refresh_token": "rt_…",
  "token_type": "Bearer",
  "expires_in": 3600,
  "user": { "id": 7, "email": "alice@example.com", "display_name": "Alice", "is_admin": false, "quota_bytes": 1073741824, "used_bytes": 52428800 }
}
```

`device_name`缺失（浏览器流程）时设置`ferroma_session`cookie。
凭据错误返回`401 unauthorized`；失败达到
`limits.max_failed_logins`次后返回`429 rate_limited`，账号被锁定
`limits.login_lockout_secs`。

`totp`是六位验证码或一次性恢复码，仅当账号启用了第二因子时才需要。此时省略它
返回`401 totp_required`——刻意**不是**`unauthorized`：它表示「请把验证码发来」，
而不是「密码错了」，把两者混为一谈的客户端会永远重试密码。验证码错误同样计入
失败次数，因此锁定策略也覆盖验证码的暴力猜测。

在任何登录入口（含客户端与 JMAP 入口），`password`都可以是**应用专用密码**
（`ap_…`）。这正是让无法被索取验证码的邮件客户端，在第二因子启用后仍能继续工作
的方式。

### `POST /api/v1/auth/refresh`

`{ "refresh_token": "rt_…" }` → 一对全新的令牌。被出示的刷新令牌
随即失效。重复使用刷新令牌会吊销整个令牌族并返回`401`。

### `POST /api/v1/auth/logout`

吊销当前会话。`204 No Content`。

### `GET /api/v1/auth/me`

已认证的用户本身，外加其地址：

```json
{
  "id": 7, "email": "alice@example.com", "display_name": "Alice",
  "is_admin": false, "quota_bytes": 1073741824, "used_bytes": 52428800,
  "mailboxes": [
    { "id": 3, "address": "alice@example.com", "user_id": 7, "display_name": "Alice",
      "is_primary": true, "enabled": true, "quota_bytes": null,
      "used_bytes": 4096, "created_at": "2026-01-01T00:00:00Z" }
  ]
}
```

### 第二因子与应用专用密码

| 方法 | 路径 | 用途 |
|---|---|---|
| `GET` | `/api/v1/auth/totp` | `{ "status": "disabled\|pending\|enabled", "recovery_codes_left": 0 }` |
| `POST` | `/api/v1/auth/totp/enroll` | 签发`{ "secret", "uri" }`——处于`pending`，**尚未**强制执行。Webmail 在页面内把 `uri` 画成二维码 |
| `POST` | `/api/v1/auth/totp/confirm` | `{ "code": "123456" }`证明验证器持有该密钥；返回`{ "enabled": true, "recovery_codes": ["…"] }` |
| `POST` | `/api/v1/auth/totp/disable` | `{ "password": "…" }` |
| `GET` | `/api/v1/auth/app-passwords` | 全部应用专用密码，含已吊销的 |
| `POST` | `/api/v1/auth/app-passwords` | `{ "label": "Thunderbird" }` → 密钥**只返回一次** |
| `DELETE` | `/api/v1/auth/app-passwords/:id` | 吊销一个；再次调用返回`404` |

三条性质是刻意的：注册在确认之前**不强制执行**，因此扫错的二维码不会把账号锁在
门外；`recovery_codes`以明文只返回一次（只存摘要），且每个只能使用一次；关闭第二
因子**需要账号密码**，而不只是有效会话——被盗的会话恰恰是该因子存在的理由，它不
应该能移除这个因子。

### `POST /api/v1/auth/password`

`{ "current_password": "…", "new_password": "…" }`。吊销其它所有会话。

---

## 4. 管理

仅限 Admin。普通用户得到`403 forbidden`。

### 4.1 用户

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/api/v1/users` | `?query=&limit=&offset=` |
| `POST` | `/api/v1/users` | `{email, password, display_name?, is_admin?, quota_bytes?}`；当邮箱域名已存在时同时创建主地址（响应中的 `mailboxes`；域名不存在时为空数组） |
| `GET` | `/api/v1/users/:id` | |
| `PATCH` | `/api/v1/users/:id` | `display_name`、`enabled`、`is_admin`、`quota_bytes`、`password`中的任意一项 |
| `DELETE` | `/api/v1/users/:id` | 级联：地址、文件夹、邮件、队列行 |
| `GET` | `/api/v1/users/:id/mailboxes` | 该用户拥有的地址 |
| `POST` | `/api/v1/users/:id/mailboxes` | `{domain, local_part, is_primary?, quota_bytes?}`，创建 Maildir 与标准文件夹 |
| `PATCH` | `/api/v1/users/:id/mailboxes/:mailbox_id` | `{is_primary?, quota_bytes?}`。`quota_bytes: 0` 表示继承账号配额。地址本身不可编辑。 |
| `GET` | `/api/v1/users/:id/security` | `{totp_status, recovery_codes_left, app_passwords[]}` |
| `DELETE` | `/api/v1/users/:id/app-passwords/:app_id` | 吊销一枚应用专用密码 |

`GET /users/:id/security` 是**只读**的。这里刻意没有任何清除第二因子的端点：一个能做
这件事的管理员会话，等于绕过服务器上每一个账号的第二因子。对于同时丢失验证器与恢复码
的用户，运维路径是在主机上执行 `ferroma user totp-disable`
（见 [deployment.md](deployment.md) §6.6）。吊销应用专用密码**是**提供的，因为丢失的
设备必须能够停止工作，而这不会削弱任何东西——用户仍然保有第二因子。

### 4.2 域名

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/api/v1/domains` | |
| `POST` | `/api/v1/domains` | `{name, description?}` |
| `GET` | `/api/v1/domains/:id` | |
| `PATCH` | `/api/v1/domains/:id` | `enabled`、`description`、`catch_all` |
| `DELETE` | `/api/v1/domains/:id` | 在仍有地址存在时拒绝执行，除非`?force=true` |

### 4.3 别名

| 方法 | 路径 |
|---|---|
| `GET` | `/api/v1/domains/:id/aliases` |
| `POST` | `/api/v1/domains/:id/aliases` — `{local_part, target}` |
| `PATCH` | `/api/v1/aliases/:id` |
| `DELETE` | `/api/v1/aliases/:id` |

### 4.4 DNS 诊断

`GET /api/v1/domains/:id/dns`执行 Admin「DNS Health」
面板背后的实时检查（项目书 §16）：

```json
{
  "domain": "example.com",
  "checked_at": "2026-09-16T12:00:00Z",
  "records": [
    { "kind": "MX",    "status": "ok",   "expected": "mail.example.com", "found": ["10 mail.example.com."] },
    { "kind": "A",     "status": "ok",   "expected": "203.0.113.10",     "found": ["203.0.113.10"] },
    { "kind": "AAAA",  "status": "skip", "found": [] },
    { "kind": "PTR",   "status": "ok",   "expected": "mail.example.com", "found": ["mail.example.com."] },
    { "kind": "SPF",   "status": "ok",   "found": ["v=spf1 mx -all"] },
    { "kind": "DKIM",  "status": "warn", "expected": "default._domainkey.example.com",
      "found": [], "hint": "publish the TXT record shown by GET /api/v1/domains/:id/dkim" },
    { "kind": "DMARC", "status": "ok",   "found": ["v=DMARC1; p=quarantine; rua=mailto:dmarc@example.com"] }
  ],
  "score": 5,
  "max_score": 7
}
```

`status`取值为`ok`、`warn`、`fail`或`skip`。

有两行取决于这台实例**怎么把信发出去**。`PTR` 是**直发**者需要的——线上那个地址才是收信方反查的
对象——所以设了 `[queue] relay_host` 之后，该行以 `skip` 报告查到什么并附说明，而不再对它收信方根本
不会去查的记录报警。`SPF` 同理：`expected` 是直发者应发布的记录，而经中继发送的实例必须改为
`include` 其服务商自己的域——那个名字只有服务商知道——因此"委托发送"的记录会被接受并附上说明该前提的
提示，而不是警告。

`GET /api/v1/domains/:id/dkim`返回要发布的 DNS 记录：

```json
{ "selector": "default", "record_name": "default._domainkey.example.com", "record_type": "TXT", "record_value": "v=DKIM1; k=rsa; p=MIIBIjANBg…" }
```

`POST /api/v1/domains/:id/dkim`在不存在密钥对时生成一对。

### 4.5 邮件队列与投递日志

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/api/v1/queue` | `?status=pending\|delivering\|delivered\|retry\|failed\|cancelled&limit=&offset=` |
| `GET` | `/api/v1/queue/:id` | 一条记录外加它的尝试历史 |
| `POST` | `/api/v1/queue/:id/retry` | 立即重新入队一条失败的记录 |
| `DELETE` | `/api/v1/queue/:id` | 取消 |
| `GET` | `/api/v1/queue/stats` | 各状态的计数、尚未发出的退信任务数，外加`next_due_at` |

### 4.6 存储、审计与设置

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/api/v1/storage` | 见下面的结构 |
| `POST` | `/api/v1/storage/gc` | 清除无人引用的附件二进制对象与陈旧的`tmp/`文件 |
| `GET` | `/api/v1/storage/destinations` | 服务器记住的归档地址 |
| `PUT` | `/api/v1/storage/destinations` | `{items: [{id, to}]}`，替换整份列表 |
| `GET` | `/api/v1/storage/transfer-settings` | S3 地址、区域和密钥是否存在；绝不返回密钥 |
| `PUT` | `/api/v1/storage/transfer-settings` | `{endpoint, region, access_key?, secret_key?}`；密钥留空则保留已有值，`clear_credentials: true` 清除两者 |
| `POST` | `/api/v1/storage/export` | `{to, live?}`，服务器保持运行时写出一份归档 |
| `GET` | `/api/v1/audit` | `?actor_user_id=&action=&since=&limit=&offset=` |
| `GET` | `/api/v1/settings` | 由数据库支撑的设置 |
| `PUT` | `/api/v1/settings/:key` | `{ "value": … }` |
| `GET` | `/api/v1/services` | SMTP 与 IMAP 监听器的实时状态 |
| `PUT` | `/api/v1/services/:service` | `service`为`smtp`、`imap`或`jmap`；`{ "enabled": true\|false }` |

`GET /api/v1/services`返回`{ "smtp": { "available": true, "enabled": true },
"imap": { "available": true, "enabled": true }, "jmap": { "available": true, "enabled": true } }`。当进程没有随该子系统启动
（`--only`或静态配置）时，`available`为 false；这类监听器不能通过 API 打开。`PUT`
仅限管理员，会把选择作为专门的运行时设置持久化，并在返回前应用。关闭 SMTP 会停止其
MX、submission 与已配置的 SMTPS 监听器；关闭 IMAP 会停止其明文与已配置的 IMAPS
监听器。空闲会话会收到协议的关闭响应并断开，而已存储的邮件不受影响。关闭 JMAP 会撤下
`/.well-known/jmap`以及 JMAP API、上传和下载，但 Webmail 与管理 API 继续在线。TLS 端口仍由
`smtps_port`、`imaps_port`和`tls.enabled`决定；此端点不增加 TLS 专用开关。

`GET /api/v1/storage`服务 Admin 的「Storage」界面与看板卡片：

```json
{
  "maildir_bytes": 8123456789,
  "attachment_bytes": 1234567890,
  "database_bytes": 234567890,
  "mailboxes": 42,
  "messages": 128431,
  "users": 17,
  "domains": 3,
  "disk_total_bytes": 107374182400,
  "disk_free_bytes": 64424509440
}
```

`users`、`domains`、`mailboxes`与`messages`是计数而非容量，而且
`mailboxes`统计的是**地址**，与 schema 一致。某部署无法报告的
数字会从对象中省略，而不是发送`0`。

### 4.7 首次运行设置

`GET /api/v1/setup` → `{ "required": true, "hostname": "mail.example.com",
"public_url": "https://mail.example.com" }`（在尚无管理员存在期间）。
`hostname`与`public_url`是运行中的配置所对外声明的值，客户端可以直接拿它们给
运维者确认，而不必猜测。

`POST /api/v1/setup`创建第一个管理员、域名及其主地址，然后返回一对普通令牌
（平铺在顶层）以及一个`applied`对象：

```json
{
  "access_token": "…", "refresh_token": "…", "token_type": "Bearer", "expires_in": 3600,
  "user": { "…": "…" },
  "applied": {
    "hostname": "mail.example.com",
    "public_url": "https://mail.example.com",
    "api_host": "0.0.0.0",
    "api_port": 8080,
    "tls_enabled": true,
    "tls_cert": "/etc/ferroma/tls/fullchain.pem",
    "tls_key": "/etc/ferroma/tls/privkey.pem",
    "restart_required": true
  }
}
```

请求体为`{email, password, domain, domain_description?, hostname?, public_url?,
api_host?, api_port?, tls_enabled?, tls_cert?, tls_key?}`；`GET /api/v1/setup`会返回
同样字段的当前值，供首次运行向导预填。

除管理员与域名外全部可选，并且全部是**「存下来」而不是「当场生效」**：运行中的进程
既不能挪动自己的监听套接字，也不能重新读取 PEM 文件，因此这些值写入`settings`表，
由服务器在启动时读回（见服务器端的`apply_stored_settings`）。`applied`列出实际写入的项，
`restart_required`说明这是否意味着需要重启。

需要重启时不会把这件事丢给运维：响应一发出，服务器就替换自己的进程镜像——容器保留端口、卷与
同一个 PID（这一点在容器里很关键，因为那个进程就是 PID 1）——控制台则等它重新应答后自动回到
控制台。`ferroma.toml`与环境变量仍然优先于存下来的行，所以显式声明了 hostname 的部署永远不会
被一次过期的向导提交覆盖。`tls_cert` / `tls_key`若在**服务器上**不是真实文件会直接以`400`
拒绝——该路径由进程读取，而不是浏览器。

管理员一旦存在，`POST /api/v1/setup`返回`409 conflict`。把
`api.enable_setup_wizard = false`则两个端点都返回`404 not_found`：被停用的
向导就是一个不存在的端点，客户端也正靠这一点把「已停用」与「已完成」区分
开来。

### 4.8 系统日志与设备

Admin 面板的「System Logs」与「Devices」界面（项目书 §36）需要
一个管理侧的视图；客户端 API 的设备路由只接受 Bearer 认证，且范围限于一个
账号。

`GET /api/v1/logs`

```json
{
  "items": [
    { "at": "2026-09-16T12:00:00Z", "level": "warn", "target": "ferroma_smtp::client",
      "message": "delivery deferred: 421 too many connections", "fields": { "queue_id": 91, "remote_mx": "mx1.example.net" } }
  ],
  "total": 1, "limit": 100, "offset": 0
}
```

它由进程内有界的环形缓冲区支撑，`tracing`层把事件写入其中，
级别为`WARN`及以上（可用`?level=info`下调到`INFO`），因此该面板
无需把日志运出主机即可工作。缓冲区保存最近 1000
条记录，并且**在重启时丢失**：这是诚实的取舍，响应通过
`"buffer_entries"`、`"buffer_capacity"`与`"oldest_at"`如实说明。过滤器：
`?level=`、`?target=`、`?query=`、`?since=`、`?limit=`、`?offset=`。

邮件正文、凭据与令牌永远不会写入该缓冲区；日志
层在存储之前会擦除看起来像不透明令牌的值（`rt_…`、`st_…`）。

`GET /api/v1/devices`：服务器上的每一台设备，最近活动在前：

```json
{
  "items": [
    { "id": 12, "user_id": 7, "email": "alice@example.com", "device_uid": "3f2c…",
      "name": "Alice's laptop", "platform": "windows", "client_version": "0.7.0",
      "protocol_version": 1, "last_seen_at": "2026-09-16T12:00:00Z",
      "last_ip": "203.0.113.44", "created_at": "2026-08-01T10:00:00Z", "revoked": false }
  ],
  "total": 1, "limit": 50, "offset": 0
}
```

过滤器：`?user_id=`、`?include_revoked=`、`?platform=`。
`POST /api/v1/devices/:id/revoke`与`DELETE /api/v1/devices/:id`的行为与
客户端 API 中的对应端点完全一致：设备被标记为已吊销，它持有的每个会话都被
吊销，并发布`device.revoked`。

### 4.9 TLS

`GET /api/v1/tls`支撑 Admin 的“TLS”界面：配置了什么 TLS、进程能否真正读到
PEM 文件，以及哪些端口提供 TLS。

```json
{
  "enabled": true,
  "min_version": "1.2",
  "self_signed_fallback": false,
  "use_platform_roots": true,
  "allow_insecure_dev_mode": false,
  "certificate": {
    "path": "/etc/ferroma/tls/fullchain.pem",
    "present": true, "readable": true, "size_bytes": 4312,
    "modified_at": "2026-09-01T09:12:44Z",
    "sha256": "9f2c…", "error": null
  },
  "private_key": {
    "path": "/etc/ferroma/tls/privkey.pem",
    "present": true, "readable": false, "size_bytes": 2412,
    "modified_at": "2026-09-01T09:12:44Z",
    "sha256": null, "error": "Permission denied (os error 13)"
  },
  "listeners": {
    "smtps_port": 465, "imaps_port": 993, "https_port": 0,
    "public_url": "https://mail.example.com", "public_url_is_tls": true
  }
}
```

两处刻意的省略：

* **私钥永不做指纹**——对密钥求哈希仍然是对密钥的一条持久事实。存在性、大小、
  修改时间与`readable`已足以捕捉该界面要防的故障：服务器自己的用户读不到密钥。
* **不解析证书的`notBefore`/`notAfter`。** 本构建未链接任何 X.509 解析器，而由文件
  修改时间推导出的到期时间比不回答更糟。请在主机上把`sha256`与
  `openssl x509 -fingerprint -sha256 -noout`的输出对照，并在那里盯到期。

`readable`是最有用的字段：`enabled: true`而`readable: false`，意味着配置看起来
正确、而每一次 TLS 握手都会失败。

---

## 5. 邮箱、邮件与附件

### 5.1 邮箱与文件夹

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/api/v1/mailboxes` | 调用者的地址 |
| `GET` | `/api/v1/mailboxes/:id/folders` | 带`message_count`、`unseen_count`、`special_use`的 IMAP 文件夹。每个文件夹在应答前都会按现存行重算，因此移动或删除留下的旧角标会在打开列表时修好 |
| `POST` | `/api/v1/mailboxes/:id/folders` | `{name, parent?}` |
| `PATCH` | `/api/v1/folders/:id` | `{name?, parent_id?, subscribed?}`。`parent_id: null` 表示移到顶层，数字表示移入该文件夹，省略该字段则父目录不变。移动会同时改写自身与所有子文件夹的路径（名字即路径）；若会形成「文件夹放进自己内部」的环则被拒绝 |
| `DELETE` | `/api/v1/folders/:id` | 拒绝`INBOX` |

### 5.2 邮件

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/api/v1/messages` | `?mailbox_id=&folder_id=&query=&unread=&flagged=&has_attachments=&since=&before=&limit=&offset=` |
| `GET` | `/api/v1/messages/:id` | 完整邮件：头字段、纯文本正文、HTML 正文、附件元数据 |
| `GET` | `/api/v1/messages/:id/raw` | RFC 5322 字节，`message/rfc822` |
| `POST` | `/api/v1/messages` | 发送。`{from, to[], cc[]?, bcc[]?, subject, text?, html?, attachments[]?, in_reply_to?, references[], draft_id?}` |
| `PATCH` | `/api/v1/messages/:id` | `{seen?, flagged?, answered?, deleted?}` |
| `POST` | `/api/v1/messages/:id/move` | `{folder_id}` |
| `POST` | `/api/v1/messages/:id/copy` | `{folder_id}` |
| `DELETE` | `/api/v1/messages/:id` | 移入 Trash；带`?permanent=true`则彻底删除 |
| `POST` | `/api/v1/messages/batch` | `{operation: "read"\|"unread"\|"flag"\|"unflag"\|"move"\|"delete", ids: [ … ], folder_id?}` |

`query=`是对主题、发件人以及**整封正文**的**词**搜索，同时保留对主题、发件人与摘要的
子串匹配——因此`invoi`仍能找到`invoice`，而埋在已存预览之外的词也能找到它所在的邮件。
引号以及`or`/`-`按`websearch_to_tsquery`的读法接受，用户输入任何内容都不会让查询失败。
正文在邮件到达时建立索引；在正文索引存在之前存储的邮件，在
`ferroma storage reindex-search` 补齐其正文之前，按主题、发件人与摘要匹配
（见 [deployment.md](deployment.md) §6.7）。

发送会为每个收件人排队一条`mail_queue`行，并立即返回：

```json
{ "message_id": 4821, "queued": 2, "recipients": ["bob@example.net", "carol@example.org"] }
```

邮件超过`limits.max_message_size`时返回`413 limit_exceeded`，
每小时超过`submission_rate_limit`或每天超过`daily_send_limit`时
返回`429 rate_limited`。

`POST /api/v1/messages`还接受`draft: true`，这会把邮件归档到
发件人的 Drafts 文件夹而不是入队。草稿不需要`to`，
这正是「先保存、回头再处理」的常见情形。

#### 单封邮件的结构

`GET /api/v1/messages/:id`返回阅读者需要的一切，包括
回复必须携带的头字段：

```json
{
  "id": 4821,
  "uid": 117,
  "folder_id": 5,
  "mailbox_id": 3,
  "subject": "Invoice for September",
  "from": { "address": "bob@example.net", "name": "Bob" },
  "to": [ { "address": "alice@example.com", "name": "Alice" } ],
  "cc": [],
  "reply_to": [],
  "flags": "seen",
  "size_bytes": 24831,
  "snippet": "Hi Alice, attached is the invoice…",
  "text_body": "Hi Alice,\n\nattached is the invoice for September.\n",
  "html_body": "<p>Hi Alice,</p><p>attached is the invoice for September.</p>",
  "message_id_header": "<20260916091231.7f3a@example.net>",
  "in_reply_to": "<20260915101100.4b21@example.com>",
  "references": ["<20260910120000.11ab@example.com>", "<20260915101100.4b21@example.com>"],
  "internal_date": "2026-09-16T09:12:44Z",
  "sent_at": "2026-09-16T09:12:31Z",
  "is_draft": false,
  "has_attachments": true,
  "attachment_count": 1,
  "attachments": [
    { "id": 991, "filename": "invoice-2026-09.pdf", "content_type": "application/pdf", "size_bytes": 24831, "is_inline": false, "content_id": null }
  ]
}
```

* `message_id_header`是本邮件的 RFC 5322 `Message-ID`。回复需要把它
  作为`in_reply_to`，而`references`是本邮件的`references`
  追加`message_id_header`后的结果。没有收到
  `message_id_header`的客户端发送回复时必须**不带**串接头字段，而不是
  自己编造一个值。
* `html_body`在服务端**无条件**净化：`<script>`、`<style>`与
  `<iframe>`的内容体、每一个`on*`处理器、`javascript:`/`vbscript:`/`file:` URL 以及
  非图片的`data:` URL 都会在正文返回之前被移除。这里
  刻意不提供配置开关：一个能关闭 HTML 净化的设置，
  就是一个把邮件客户端变成远程代码执行载体的设置。
  净化器只做*移除*，从不改写，因此它无法引入标记；它是
  过滤器而不是完整的解析器，所以客户端仍必须在
  沙箱中渲染结果。
* 调用者不拥有的邮件是`404`，绝不是`403`：API 不会
  确认他人的邮件是否存在。

### 5.3 草稿

草稿保存在服务端，因此同一份草稿会出现在用户登录的每一台设备上。
管理接口面与客户端接口面（`fcp.md` §7）彼此对应，两者操作的是同一批
记录。

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/api/v1/drafts` | `?limit=&offset=` |
| `POST` | `/api/v1/drafts` | `{mailbox_id?, subject?, text?, html?, to?, cc?, bcc?, in_reply_to?, references?, attachment_ids?}` |
| `GET` | `/api/v1/drafts/:id` | |
| `PATCH` | `/api/v1/drafts/:id` | 创建字段的任意子集 |
| `DELETE` | `/api/v1/drafts/:id` | |

草稿也会作为一封携带`\Draft`的真实邮件镜像到邮箱的`Drafts`文件夹，
因此 IMAP 客户端也能看到它；从任一面删除它都会同时从两者中移除。

### 5.4 联系人

账号发信给一个地址，或收到提到该地址的邮件时，会记住它。除地址外，其余字段由所有者编辑。

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/api/v1/contacts` | `?q=` 匹配地址、名称或备注。收藏排在前面。 |
| `POST` | `/api/v1/contacts` | `{address, display_name?}` |
| `PATCH` | `/api/v1/contacts/:id` | `{display_name?, note?, favorite?, blocked?}`。空字符串清除名称或备注。 |
| `DELETE` | `/api/v1/contacts/:id` | 忘掉它。之后的邮件再次提到该地址时会重新记住，且不再拉黑。 |

被拉黑地址发来的邮件进入垃圾邮件。

### 5.5 附件

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/api/v1/attachments` | `multipart/form-data`，字段名**`file`**；流入二进制存储，返回`{id, filename, content_type, size_bytes, sha256}` |
| `GET` | `/api/v1/attachments/:id` | 流式返回字节，支持`Range`与`ETag` |
| `DELETE` | `/api/v1/attachments/:id` | 仅在仍无人引用时可用 |
| `GET` | `/api/v1/attachments/:id/meta` | 不含字节的元数据 |

超过`client.attachment_chunk_size`的上传应使用
[`fcp.md`](fcp.md) §6 中的分块端点。`GET /api/v1/client/attachments/:id/status`（续传
客户端轮询的端点）应答：

```json
{ "attachment_id": 991, "size_bytes": 4194304, "chunk_size": 1048576,
  "chunk_count": 4, "received": [0, 1, 3], "complete": false }
```

`received`是服务器持有的分块索引列表，因此在上传中途
崩溃的客户端只需补发缺口部分。

---

## 6. 客户端 API（FCP）

官方客户端只使用这个接口面。完整的协议语义
（包括同步游标、实时帧格式与分块上传）见
[`fcp.md`](fcp.md)；这里是项目书 §19 给出的端点索引。

| 方法 | 路径 |
|---|---|
| `POST` | `/api/v1/client/auth/login` |
| `POST` | `/api/v1/client/auth/refresh` |
| `POST` | `/api/v1/client/auth/logout` |
| `GET` | `/api/v1/client/account` |
| `GET` | `/api/v1/client/mailboxes` |
| `GET` | `/api/v1/client/sync` |
| `GET` | `/api/v1/client/messages` |
| `GET` | `/api/v1/client/messages/:id` |
| `POST` | `/api/v1/client/messages` |
| `PATCH` | `/api/v1/client/messages/:id` |
| `DELETE` | `/api/v1/client/messages/:id` |
| `POST` | `/api/v1/client/messages/:id/read` |
| `POST` | `/api/v1/client/messages/:id/unread` |
| `POST` | `/api/v1/client/messages/:id/star` |
| `POST` | `/api/v1/client/messages/:id/archive` |
| `POST` | `/api/v1/client/messages/:id/move` |
| `POST` | `/api/v1/client/messages/:id/trash` |
| `GET`/`POST` | `/api/v1/client/drafts` |
| `PATCH`/`DELETE` | `/api/v1/client/drafts/:id` |
| `GET`/`POST` | `/api/v1/client/attachments`, `/api/v1/client/attachments/:id` |
| `GET` | `/api/v1/client/devices` |
| `DELETE` | `/api/v1/client/devices/:id` |
| `POST` | `/api/v1/client/devices/:id/revoke` |
| `GET` | `/api/v1/client/events`（WebSocket 升级）|

客户端请求会表明自己的身份，服务器据此门控兼容性
（项目书 §56）：

```http
X-Ferroma-Client: FerromaClient/0.7.0
X-Ferroma-Protocol: 1
X-Ferroma-Platform: windows
User-Agent: FerromaClient/0.7.0 (Windows 11; x86_64)
```

`X-Ferroma-Protocol`低于`client.min_protocol_version`的客户端会收到
`426 Upgrade Required`，并附带`{ "error": { "code": "unsupported", "message": "client protocol 0 is no longer supported; upgrade to FCP/1" } }`。

---

## 7. 端到端发送一封邮件

Webmail UI 与官方客户端都遵循的流程：

1. `POST /api/v1/auth/login`（或客户端中的对应端点）→ 令牌。
2. `GET /api/v1/mailboxes` → 用户可以用来发信的地址。
3. 对每个文件执行`POST /api/v1/attachments`；收集返回的 id。
4. 带`from`、`to`、`subject`、`text`/`html`与附件 id 调用
   `POST /api/v1/messages`。服务器把邮件写入发件人的 Sent 文件夹，
   为每个收件人入队一条`mail_queue`行，发布`mail.sent`，并返回
   `{message_id, queued, recipients}`。
5. 用`GET /api/v1/queue?status=retry,failed`（或客户端自己的发件箱（Outbox）视图）
   观察投递；每次尝试完成时，服务器通过 socket 推送`delivery.updated`事件。


---

## 8. JMAP

Ferroma 也提供 RFC 8620 与 RFC 8621 的首批 JMAP 邮件接口。它复用既有的邮箱、
Maildir 文件、仓储、事件和变更日志；不会另建第二套邮箱或邮件存储。JMAP 与 IMAP、
FCP 并存，并不取代其中任何一个。

标准客户端用 `Authorization: Basic`（完整邮箱地址和它的密码）访问
`GET /.well-known/jmap`，得到 RFC 8620 Session 对象。`401` 的
`WWW-Authenticate` 会写明 `Basic`，客户端据此重发密码。这个 `401` 的
`Content-Type` 是 `application/problem+json`，正文带 `type`（RFC 7807）；
JMAP 客户端库需要这个字段，才会把状态当成认证失败并改试 Basic。同一份正文里
仍有 Ferroma 的错误信封。同一次请求也接受
`POST /api/jmap/auth/token`（`email`、`password`、`device_name`）签发的
JMAP Bearer 令牌。该令牌只被 JMAP 端点接受；浏览器 cookie、普通 REST 令牌和
FCP 令牌都会被拒绝。Basic 登录会复用该地址已经打开的 JMAP 会话，而不是每次请求都写一行。

| 方法 | 路径 | 用途 |
|---|---|---|
| `GET` | `/.well-known/jmap` | 已认证的 JMAP Session 发现 |
| `POST` | `/api/jmap/` | JMAP 方法调用端点 |
| `POST` | `/api/jmap/auth/token` | 创建独立的 JMAP 令牌会话 |
| `POST` | `/api/jmap/upload/:accountId` | 上传一个原始二进制对象 |
| `GET` | `/api/jmap/download/:accountId/:blobId` | 下载当前账户拥有的对象 |

Session 同时在服务器能力与账户能力中声明 `urn:ietf:params:jmap:submission`，
并把该账户写进它的 `primaryAccounts`。一个能读邮件、却在建立时被
`server does not advertise JMAP EmailSubmission` 拒绝的客户端，找的就是这个 URI。
账户能力把 `maxDelayedSend` 设为 `0`，`submissionExtensions` 设为空对象：
不提供延迟发送，也不接受 SMTP 扩展参数。

方法端点支持 `Mailbox/get`、`Mailbox/set`、`Mailbox/changes`、`Email/query`、
`Email/get`、`Email/set`、`Email/changes`、`Email/import`、`Identity/get`、
`EmailSubmission/get` 和 `EmailSubmission/set`。同一次请求可以用 `#` 结果引用
取前一次调用的返回值（RFC 8620 §3.7），创建 id（`#id`）指向同一次请求里更早创建的对象。
`Mailbox/set` 经由与 Webmail 相同的 Maildir 路径创建、改名、改父级、改订阅和删除文件夹。
标准文件夹（`INBOX` 以及任何带 role 的文件夹）不能改名或删除，`myRights` 也如此声明。
删除文件夹会永久删除其中的邮件；它还有子文件夹时会被拒绝。

一个账户可以拥有多个地址，每个地址都有自己的标准文件夹。RFC 8621 §2 规定一个账户
里每个角色最多出现一次，因此 `Mailbox/get` 只给主地址的标准文件夹标注角色。其余
地址的文件夹仍在列表中，`role` 为 null。以 `mailboxes N and M both advertise the
inbox role` 停止的客户端，读到的是一个把该角色声明了两次的服务器。

`Email/get` 返回邮件元数据，并在客户端要求时返回 `textBody`、`htmlBody`、
`bodyValues`、`bodyStructure` 和 `attachments`。`properties` 限制返回的字段；
`fetchTextBodyValues`、`fetchHTMLBodyValues`、`fetchAllBodyValues` 和
`maxBodyValueBytes` 决定正文是否内联、内联多少。`Email/query` 可按 `inMailbox`、
`from`、`to`、`subject`、`text`、`body`、`hasKeyword`、`notKeyword`、
`hasAttachment`、`after` 和 `before` 过滤，并按 `receivedAt`、`sentAt`、`size`、
`from`、`subject` 其中之一排序。`hasKeyword: "$seen"` 匹配已读邮件。过滤运算符、
第二个排序条件和 `collapseThreads` 会被拒绝，而不是被静默忽略。查询不计算增量
（`canCalculateChanges` 为 false），所以文件夹视图要重新执行查询。

`Email/set` 用结构化属性创建草稿（`mailboxIds`、`keywords`、地址字段、`subject`、
`textBody`、`htmlBody`），整体替换或逐个补丁关键字（`$seen`、`$flagged`、
`$answered`、`$draft` 以及私有关键字），通过设置唯一的 `mailboxIds` 移动邮件，
或删除邮件。账户的 `maxMailboxesPerEmail` 为 `1`，因此一封邮件不能同时属于两个文件夹。
`Email/import` 仍把上传的 RFC 5322 对象存入文件夹。`Email/changes` 和
`Mailbox/changes` 读的是 FCP 所用的同一条变更日志，state 字符串就是这条日志的游标。
一页填满 `maxObjectsInGet` 时会置 `hasMoreChanges`，并返回该页最后一行的游标，
下一次调用从这里继续，而不是跳过。

`Identity/get` 列出该账户已启用的地址；一个 Identity 就是那个邮箱，而不是另存的对象。
`EmailSubmission/set` 把一封已经存储的 Email 交给与 Webmail 相同的 Sent 副本与队列事务。
入队的是存储下来的 RFC 5322 字节。`Bcc` 头会在存入这份副本之前去掉，
地址仍留在信封上。`onSuccessDestroyEmail` 可以用 `#id` 指向源邮件，
客户端借此删掉刚刚发出的草稿。提交对象本身不存储。

这个接口足够一个 JMAP 邮件客户端列出文件夹、阅读、归档、移动和发送。它不宣称
支持推送、日历、联系人、Sieve、休假回复、配额、共享，或一封邮件同时属于多个邮箱。
客户端轮询 Session 和 `/changes` 方法返回的 state。上述未实现部分不构成完整
RFC 8621 支持。
