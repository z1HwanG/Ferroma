# Ferroma 中的 SMTP

**本文读者：** 所有实现、测试或排查 `ferroma-smtp` 的人，以及任何想弄清远端服务器
为何拒收自己邮件的运维者。

本文覆盖 SMTP 的两个方向。收信侧既包括从其它 MTA 接收邮件的监听器，也包括从你自己
用户的邮件客户端接收邮件的 submission 监听器。发信侧是解析 MX 记录、把你用户的邮件
投递到远端主机的队列工作器。本文规定命令集、会话状态机、每一种失败对应的应答码、
各项限制以及每项限制在哪一层强制执行、Ferroma 前置的 `Received:` 头字段、重试计划、
4xx 与 5xx 的分类规则，以及退信生成。

> **状态：** 设计规范。`ferroma-smtp` crate 是照着本文档实现的；标注 _(计划中)_ 的
> 小节描述的是已经规定但尚未交付的行为。就目前而言，本文件的全部内容都是 _(计划中)_：
> `crates/ferroma-smtp/src/lib.rs` 只是一个骨架，声明了 server/client/MX/policy 的
> 模块边界。它提到的配置键、限制值和错误变体**确实**已经存在——
> [`config/ferroma.toml`](../../config/ferroma.toml) 中的 `[smtp]` 与 `[limits]`、
> `crates/ferroma-core/src/limits.rs` 中的 `Limits`、
> `crates/ferroma-core/src/error.rs` 中的 `FerromaError`，以及
> `migrations/0001_initial.sql` 中的 `mail_queue` / `delivery_attempts` 两张表。

---

## 1. 端口与监听器角色

| 端口 | 配置键 | 角色 | TLS |
|---|---|---|---|
| 25 | `smtp.port` | 收信 MX。接收来自其它 MTA 的邮件。未认证的对等端**只能**投递到本地域。 | 明文，提供 `STARTTLS` |
| 587 | `smtp.submission_port` | Submission。供你用户的邮件客户端使用。`MAIL FROM` 之前必须先认证。 | 明文，策略要求 `STARTTLS` |
| 465 | `smtp.smtps_port` | 隐式 TLS。策略与 587 相同；握手在最前面。`0` 表示关闭该监听器。 | 从第一个字节起就是 TLS |

当 `smtp.smtps_port != 0` 而 `tls.enabled = false` 时，当两个启用的 SMTP 端口冲突时，
或者当 `smtp.port` 为 `0` 时，`Config::validate()` 会拒绝启动。见
`crates/ferroma-core/src/config.rs`。

连接到达的是哪个端口属于会话的一部分，而不是另一条代码路径：它选择的是*策略配置档*
（认证是否强制，以及未认证的事务是否可以向非本地域投递）。

---

## 2. 命令集（§9）

项目书 §9.1 把命令集拆成两个阶段。

| 阶段 | 命令 | 状态 |
|---|---|---|
| **MVP** | `EHLO`、`HELO`、`MAIL FROM`、`RCPT TO`、`DATA`、`RSET`、`NOOP`、`QUIT` | 第一个版本 |
| **第二阶段** | `AUTH`、`STARTTLS` | submission 在第一个版本即支持；25 端口上的 `STARTTLS` 在 `tls.enabled` 后立即支持 |

实现额外接受的命令，以及定义它们的 RFC：

| 命令 | RFC | 说明 |
|---|---|---|
| `VRFY` | 5321 §4.1.1.6 | _(计划中)_ 应答 `252 2.5.2 Cannot VRFY user`——绝不确认某个地址是否存在 |
| `HELP` | 5321 §4.1.1.4 | _(计划中)_ `214 2.0.0` 加一行摘要 |
| `EXPN` | 5321 §4.1.1.7 | _(计划中)_ `502 5.5.1 Command not implemented` |
| `STARTTLS` | 3207 | 仅在 `tls.enabled` 且尚未加密时可用 |
| `AUTH` | 4954 | 只支持 `PLAIN` 与 `LOGIN` |
| `BDAT` | 3030 | _(计划中)_ `502`——不宣告 `CHUNKING` |

其它任何输入都得到 `500 5.5.2 Command unrecognized`。

### 命令行

* 命令行上限：512 个八位组，含 `CRLF`（RFC 5321 §4.5.3.1.4）。更长
  ⇒ `500 5.5.2 Line too long`。该限制在解析之前检查，因此攻击者无法让解析器分配内存。
* 命令不区分大小写（`mail from:` 合法）。
* `MAIL FROM`、`RCPT TO` 和 `AUTH` 可以携带 ESMTP 参数
  （`SIZE=`、`BODY=8BITMIME`、`SMTPUTF8`、`AUTH=`），以空格分隔。按照 RFC 5321
  §4.1.1.11，未知参数被忽略。
* 未知动词被拒绝，未知*参数*不会。这个不对称正是 RFC 要求的，也正是它让新客户端
  能继续对着旧服务器工作。

---

## 3. 状态机

项目书 §9.2 给出该枚举；已交付的实现原样使用它，并补上 `STARTTLS` 需要的加密标志。

```rust
/// crates/ferroma-smtp/src/server/state.rs
pub enum SmtpState {
    Connected,      // 已发出问候语，尚未收到任何内容
    Greeted,        // EHLO/HELO 已被接受
    MailFrom,       // MAIL FROM 已被接受，尚无收件人
    RcptTo,         // 至少一个 RCPT TO 已被接受
    Data,           // 已发出 354，正在读取以点号终止的正文
    Authenticated,  // SASL 成功（与上面五个状态正交）
}
```

迁移：

```text
                    ┌──────────────┐
   connect ────────►│  Connected   │ 220 <smtp.banner>
                    └──────┬───────┘
        EHLO/HELO ─────────┤          250-… / 250 SIZE n
                           ▼
                    ┌──────────────┐
                    │   Greeted    │◄──── RSET (from any state)
                    └──────┬───────┘
      MAIL FROM:<…> ───────┤          250 2.1.0
                           ▼
                    ┌──────────────┐
                    │   MailFrom   │
                    └──────┬───────┘
      RCPT TO:<…> ─────────┤          250 2.1.5  (repeatable)
                           ▼
                    ┌──────────────┐
                    │    RcptTo    │
                    └──────┬───────┘
         DATA ─────────────┤          354 End data with <CR><LF>.<CR><LF>
                           ▼
                    ┌──────────────┐
                    │     Data     │  (body with limits.data_timeout_secs)
                    └──────┬───────┘
        end-of-data ───────┴─────────► 250 2.0.0 Ok: queued as <id>  → Greeted
                                   └──► 4xx / 5xx                    → Greeted
```

`Authenticated` 不是该序列中的一个位置；它是一个标志，能跨 `RSET` 保留，并改变会话
被允许做什么。AUTH 在 `Greeted` 及之后的状态合法，在 `Connected` 中永远不合法
（`503 5.5.1 Send HELO/EHLO first`），在 `Data` 期间也永远不合法。

各项守卫，以及守卫触发时的应答：

| 守卫 | 触发条件 | 应答 |
|---|---|---|
| `helo_required` | `MAIL FROM` 在未发 `EHLO`/`HELO` 时到达 | `503 5.5.1 Send HELO/EHLO first` |
| 顺序 | `RCPT TO` 出现在 `MAIL FROM` 之前 | `503 5.5.1 Need MAIL FROM before RCPT TO` |
| 顺序 | `DATA` 时没有任何已被接受的收件人 | `503 5.5.1 Need RCPT TO before DATA` |
| `require_auth_on_submission` | submission 端口上的 `MAIL FROM` 未经 AUTH | `530 5.7.0 Authentication required` |
| `require_tls_for_auth` | 未加密连接上的 `AUTH` | `538 5.7.11 Encryption required for requested authentication mechanism` |
| 嵌套 `MAIL` | 事务中出现第二条 `MAIL FROM` | `503 5.5.1 Sender already specified` |
| 嵌套 `DATA` | 已经处于 `Data` 时又来 `DATA` | 不可能：正文读取器会一直消费到终止符 |

---

## 4. 会话结构体

项目书 §9.3：

```rust
/// crates/ferroma-smtp/src/server/session.rs
pub struct SmtpSession {
    /// Unique per accepted connection; also the `connection_id` log field.
    pub connection_id: Uuid,
    /// TCP peer address. Taken from `X-Forwarded-For` only when
    /// `api.trust_proxy_headers` is set and the peer is a trusted proxy.
    pub remote_addr: SocketAddr,
    /// The handler the connection is talking to.
    pub port: SmtpPort,          // Mx | Submission | Smtps
    pub state: SmtpState,
    /// The name the peer announced. Used in `Received:`, never trusted.
    pub helo: Option<String>,
    /// `true` once STARTTLS completed. Gates `require_tls_for_auth`.
    pub encrypted: bool,
    /// Set by a successful `AUTH`; the account whose quota and rate limits apply.
    pub authenticated_user: Option<UserId>,
    /// The session row created by `AuthService::open_session(SessionKind::Smtp)`.
    pub session_id: Option<SessionId>,
    /// `MAIL FROM` reverse-path, empty for a null sender (`<>`, a bounce).
    pub envelope_from: Option<String>,
    /// Accepted recipients, in order, after aliases and catch-all expansion.
    pub recipients: Vec<String>,
    /// `SIZE=` announced on `MAIL FROM`, when the client sent one.
    pub declared_size: Option<u64>,
    /// `BODY=` / `SMTPUTF8` parameters seen on `MAIL FROM`.
    pub body_8bit: bool,
    pub smtp_utf8: bool,
}
```

每个可以被记录的字段都出现在项目书 §40 列出的结构化字段中（`connection_id`、
`remote_ip`、`helo`、`authenticated_user`、`sender`、`recipient`、`message_id`、
`result`、`duration`）。`ferroma-core` 的 `logging` 模块安装携带这些字段的订阅者。

会话是每连接一份的，并且活在一个 Tokio 任务上。它没有任何共享成分，因此状态机无需
加锁；*确实*共享的计数器（连接总数、按 IP 的速率窗口）放在监听器里。

---

## 5. 宣告的 `EHLO` 扩展

对 `EHLO` 的应答是每个能力一行，并且整组扩展只在 `smtp.advertise_extensions = true`
时才发送；当它为 `false` 时，会话以 `250-<hostname>` 加一个光秃秃的 `250 OK` 应答，
这正是那种会因扩展而噎住的远古客户端所需要的。

| 行 | 何时宣告 | 含义 |
|---|---|---|
| `250-<server.hostname>` | 总是 | 问候行 |
| `250-PIPELINING` | `smtp.advertise_extensions` | 客户端可以批量发送命令而不等待应答 |
| `250-SIZE <limits.max_message_size>` | `smtp.advertise_size` | 可接受的 `DATA` 最大字节数 |
| `250-8BITMIME` | `smtp.advertise_extensions` | 接受 `BODY=8BITMIME` |
| `250-ENHANCEDSTATUSCODES` | `smtp.advertise_extensions` | 应答携带 `x.y.z` 状态码（见 §11） |
| `250-SMTPUTF8` | `smtp.advertise_extensions` | 接受 UTF-8 的本地部分 |
| `250-DSN` | _(计划中)_ `policy` | 在 `MAIL FROM` 与 `RCPT TO` 上处理 `RET`/`ENVID` 参数 |
| `250-STARTTLS` | `tls.enabled` 且尚未加密 | 客户端可以原地升级 |
| `250-AUTH PLAIN LOGIN` | `tls.enabled` 或 `smtp.require_tls_for_auth = false` | SASL 机制 |
| `250-HELP` | _(计划中)_ | 已实现 `HELP` |
| `250 CHUNKING` | 从不 | 未实现 `BDAT`；不要宣告它 |

当 `smtp.require_tls_for_auth = true` 时，在未加密的连接上完全不给 `AUTH`。宣告一个
会话会拒绝的机制，比不宣告它更糟：那会让客户端在明文发出了它本不该发出的凭据之后
才失败。

---

## 6. 开放中继策略，以及它为何是默认值

项目书 §9.4 与 §54：

```text
recipient domain is a local domain   →  accept (subject to quota and limits)
recipient domain is anything else    →  require a successful AUTH first
```

具体到 `RCPT TO` 的处理中：

| 连接 | 收件域 | 结果 |
|---|---|---|
| 未认证，25 端口 | 本地（`domains.name` 匹配，`domains.enabled`） | 接受，本地投递 |
| 未认证，25 端口 | 非本地 | `550 5.7.1 Relaying denied` |
| 已认证，任意端口 | 本地 | 接受 |
| 已认证，587/465 端口 | 非本地 | 接受，入队到 `mail_queue` |
| 已认证，25 端口 | 非本地 | 接受并入队——账号已认证，因此这是提交，不是中继 |

**为什么默认值就是它。** 开放中继不是一处配置上的不便；它是一台洗白垃圾邮件的机器，
几小时内就会被列入黑名单，并把运维者自己的正常邮件一起拖下水。每一位邮件管理员都
见过它发生，损害是以数月的投递率来计量的，而不是以分钟的停机。因此，让安全的行为
成为默认值、让不安全的行为因打错字而不可达，值得多花那一点点配置：没有任何
`allow_relay = true` 键可以被偶然找到并设置，而 `[../AGENTS.md](../../AGENTS.md)` §4.6
把这条规则列为不可协商。

由同一套推理得出的两个相邻策略决定：

* **本地投递不要求认证。** 否则任何其它 MTA 都无法向你用户投递邮件，而运行一台 MX
  的全部意义正在于此。
* **AUTH 不会让你对任意 `MAIL FROM` 变得可信。** 已认证用户只能设置自己拥有的本地
  `From` 地址（`mailboxes` 表中 `user_id` 等于他的那些行）；在自己的提交中使用
  外来 `From` 会得到 `550 5.7.1 Sender address rejected: not owned by user`。

---

## 7. 各项限制及其强制执行位置

每一项限制都在协议边缘强制执行，**并且**在邮件核心中复查，因此改用 API 而不是 SMTP
也无法绕过它。`crates/ferroma-core/src/limits.rs` 是唯一定义处；
`config/ferroma.toml` 中的 `[limits]` 是唯一的取值来源。

| 限制 | 配置键 | 默认值 | 强制执行位置 | 应答或错误 |
|---|---|---|---|---|
| 邮件大小 | `limits.max_message_size` | 26214400 (25 MiB) | `DATA` 正文读取器，依据 `SIZE` 扩展并按八位组计数；Maildir 写入前由邮件核心复查 | `552 5.3.4 Message size exceeds fixed maximum message size` |
| 提前声明的大小 | `MAIL FROM SIZE=` | — | `MAIL FROM` 时，在 `354` 之前 | `552 5.3.4 Message size exceeds fixed maximum message size` |
| 每事务收件人数 | `limits.max_recipients` | 100 | `RCPT TO` 计数器 | `452 4.5.3 Too many recipients` |
| 全局同时连接数 | `limits.max_connections` | 100 | 监听器 accept 循环 | `421 4.3.2 Too many connections, try again later`，随后关闭 |
| 每 IP 同时连接数 | `limits.max_connections_per_ip` | 10 | 监听器 accept 循环，按源地址 | `421 4.3.2 Too many connections from your address` |
| 每 IP 每分钟收信命令数 | `limits.smtp_rate_limit` | 100 | 命令循环，按 IP 的滑动窗口 | `421 4.7.0 Too many commands, slow down` |
| 每账号每小时 submission 邮件数 | `limits.submission_rate_limit` | 50 | 接受已认证事务时 | `452 4.7.0 Submission rate limit exceeded` |
| 每账号每天邮件数 | `limits.daily_send_limit` | 500 | 接受时，从 `mail_queue` 统计，而非从内存 | `452 4.7.0 Daily send limit exceeded` |
| 邮箱配额 | `users.quota_bytes`、`mailboxes.quota_bytes`、`limits.mailbox_quota` | 1 GiB | Maildir 写入前的 `MailboxesRepository::check_quota(mailbox_id, needed)` | `452 4.2.2 Mailbox full`——临时性错误，因此发件人在用户腾出空间后会重试 |
| 命令超时 | `smtp.command_timeout_secs` | 300 | 命令循环，每次读取 | `421 4.4.2 Timeout waiting for command`，随后关闭 |
| `DATA` 超时 | `smtp.data_timeout_secs` | 600 | 正文读取器 | `421 4.4.2 Timeout waiting for data`，随后关闭 |
| MIME 嵌套深度 | `limits.max_mime_depth` | 20 | MIME 解析器（`ferroma-mail` 中的 `ParseLimits`） | 邮件被接受进 `Junk` 而非被拒绝 _(计划中)_ |
| 每封邮件附件数 | `limits.max_attachments` | 50 | 附件提取 | `552 5.3.4 Too many message parts` _(计划中)_ |
| 单个附件大小 | `limits.max_attachment_size` | 26214400 | 附件提取 | `552 5.3.4 Attachment too large` _(计划中)_ |

当 `max_connections_per_ip > max_connections` 时，当 `max_attachment_size >
max_message_size` 时，当 `max_message_size` 为 `0` 时，或者当 `max_mime_depth` 落在
1–100 之外时，`Limits::validate()` 会拒绝启动。因此 `[limits]` 里的一个错字会在启动时
让服务器停下，而不是悄悄禁用一项限制。

按账号的限制才是对滥用真正要紧的那些：否则一个被攻陷的已认证账号就能被当作垃圾邮件
加农炮，速率只受主机上行带宽制约。`submission_rate_limit` 和 `daily_send_limit` 在
*接受*时检查，并且每日计数取自 `mail_queue` 的行而不是内存计数器，因此重启不会把它
清零。

---

## 8. `AUTH PLAIN` 与 `AUTH LOGIN`

项目书 §15 规定口令存储使用 Argon2id，SMTP 使用 `AUTH PLAIN` / `AUTH LOGIN`。

| 机制 | 线上形式 | 说明 |
|---|---|---|
| `PLAIN` | `AUTH PLAIN <base64(\0authcid\0passwd)>`，或先 `AUTH PLAIN` 再一个 `334` 续行携带同一份数据块 | 一次往返；续行形式是客户端在收到 `334` 之后使用的形式 |
| `LOGIN` | `AUTH LOGIN` → `334 VXNlcm5hbWU6` → base64 用户名 → `334 UGFzc3dvcmQ6` → base64 口令 | 两次往返；提示语是 `Username:` 与 `Password:` 的常规 base64 |

初始应答 `=`（RFC 4954 §4）表示「空」，对两种机制都是解析错误：
`501 5.5.4 Invalid base64 data`。

认证成功：

1. 会话解码凭据并调用
   `AuthService::login(email, password, SessionKind::Smtp, ip, user_agent, None)`。
2. `AuthService` 对着存储的 Argon2id PHC 字符串校验
   （`PasswordHasher::verify`），当 `PasswordHasher::needs_rehash` 判断存储参数已过期时
   透明地重新哈希，并通过 `LoginAttemptsRepository` 记录本次尝试。
3. 成功后会话获得 `authenticated_user`，以及来自 `sessions` 表、`kind = 'smtp'` 的
   一个 `SessionId`。
4. 应答为 `235 2.7.0 Authentication successful`。

失败应答——在「无此用户」与「口令错误」之间刻意不可区分，正如 `AuthService::login`
所做的那样：

| 情况 | 应答 |
|---|---|
| 口令错误、账号不存在、账号已禁用 | `535 5.7.8 Authentication credentials invalid` |
| 达到 `limits.max_failed_logins`，或账号被锁定 | `454 4.7.0 Temporary authentication failure` |
| 窗口期内来自单一源 IP 的失败次数超过 `3 × limits.max_failed_logins` | `454 4.7.0 Temporary authentication failure` |
| base64 格式错误、未知机制 | `501 5.5.4 Invalid base64 data` / `504 5.5.4 Unrecognized authentication type` |
| 在一次成功的 `AUTH` 之后再来 `AUTH` | `503 5.5.1 Already authenticated` |
| 在明文连接上 `AUTH`，而 `smtp.require_tls_for_auth = true` | `538 5.7.11 Encryption required for requested authentication mechanism` |

### `require_tls_for_auth`

在 `config/ferroma.toml` 中默认为 `false`，这样一次裸 `cargo run` 无需证书即可工作。
`docker-compose.prod.yml` 设置 `FERROMA__SMTP__REQUIRE_TLS_FOR_AUTH: 'true'`，生产 MX
应当保持这一设置：`AUTH PLAIN` 与 `AUTH LOGIN` 都用 base64 发送口令，而那是编码，
不是加密。在明文 587 端口上，被动观察者能读到明文口令。开启该标志后，在 `STARTTLS`
完成之前，`AUTH` 既不被宣告也不被接受。

`Config::allows_plaintext_auth()` 的存在，是为了让调用方无需从 TLS 标志再推导一遍
就能问出这个问题。

---

## 9. `STARTTLS`、SMTPS 与 submission 角色

| 端口 | 发生什么 | 策略 |
|---|---|---|
| 25 | `EHLO` 宣告 `STARTTLS`；客户端发送 `STARTTLS`，得到 `220 2.0.0 Ready to start TLS`，双方重新协商。会话回到 `Connected`，客户端必须重新发送 `EHLO`。 | 来自其它 MTA 的邮件被机会性地接受。拒绝明文收信会丢掉所有不做 TLS 的主机发来的邮件。 |
| 587 | 同样的升级路径，但适用 submission 策略：`require_auth_on_submission` 意味着在 AUTH 成功之前 `MAIL FROM` 被拒绝，`require_tls_for_auth` 意味着在 TLS 成功之前 AUTH 被拒绝。 | Submission。不会 `STARTTLS` 的客户端无法发信。 |
| 465 | 从第一个八位组起就是 TLS（SMTPS）。不宣告 `STARTTLS`——没有东西可升级。 | Submission。 |

那些容易弄错、因此被明确规定的细节：

* **`STARTTLS` 之后状态被重置。** `helo`、`envelope_from`、`recipients`、
  `declared_size` 以及 AUTH 标志全部清空；只有 `connection_id` 和
  `remote_addr` 留存。在 `STARTTLS` 之前完成认证的客户端必须重新认证——否则攻击者
  可以向明文会话中注入命令，并让它们作用于加密会话。
* **任何命令都不得跨 `STARTTLS` 流水线化。** 按照 RFC 3207 §6，一个 `STARTTLS`
  命令行之后在同一 TCP 报文段里还有任何内容都是错误。应答：`554 5.5.1 Pipelining
  violated`，随后关闭。
* **`tls.enabled = false` 时的 `STARTTLS`** 应答为
  `502 5.5.1 Command not implemented`，且它也不会被宣告。
* **已经加密时的 `STARTTLS`** 是 `503 5.5.1 TLS already active`。
* **TLS 握手失败会关闭连接**，不给应答；没有明文回退，这正是阻止降级攻击的原因。
* **证书材料**来自 `tls.cert_path` / `tls.key_path`（一个 PEM 包：叶证书后跟中间
  证书，以及一把 PKCS#8 或 PKCS#1 私钥）。`tls.self_signed_fallback` 在启动时用
  `rcgen` 生成证书，仅供开发使用——还要求 `tls.allow_insecure_dev_mode` 为真，
  并由 `Config::validate()` 强制这一配对。
* **最低协议版本**是 `tls.min_version`，默认 `"1.2"`；`"1.3"` 也被接受。其它任何值
  都会拒绝启动。
* **只用 rustls。** 不用 `native-tls`，不用 OpenSSL，不用 schannel——见
  [architecture.md](architecture.md) §8 与 [../AGENTS.md](../../AGENTS.md) §1.1。

---

## 10. `Received:` 头字段

当 `smtp.add_received_header = true`（默认值）时，收信邮件会被前置一个 `Received:`
头字段。它是前置而不是后置：`Received:` 头字段按最新在前的顺序累积，最上面的那条
是 Ferroma 知道的第一跳。

```text
Received: from <helo> (<reverse-dns> [<remote-ip>])
        by <server.hostname> (Ferroma <version>)
        with ESMTPS id <connection_id>
        for <recipient>
        ; <date>
```

渲染示例（示意输出）：

```text
Received: from mail.example.net (mail.example.net [203.0.113.25])
        by mail.example.com (Ferroma 0.1.0)
        with ESMTPS id 0f4c9a12-6b1e-4d3f-9a77-2c1f0e5b8d41
        for <alice@example.com>
        ; Tue, 16 Sep 2026 09:12:31 +0000
```

逐字段规则：

| 字段 | 来源 | 规则 |
|---|---|---|
| `from <helo>` | `SmtpSession::helo` | 对等端宣告的名字——不可信，仅展示，绝不用于任何决策 |
| `(<reverse-dns> [<remote-ip>])` | 对 `remote_addr` 的 PTR 查询 | 查询失败时整段省略；方括号里始终是字面 IP |
| `by <host>` | `server.hostname` | 必须是合法 DNS 名，否则 `Config::validate()` 拒绝启动 |
| `with <protocol>` | 会话 | `SMTP`（明文）、`ESMTP`（EHLO，明文）、`ESMTPS`（EHLO + TLS）、`ESMTPSA`（EHLO + TLS + AUTH）、`ESMTPA`（EHLO + AUTH，无 TLS） |
| `id <connection_id>` | `SmtpSession::connection_id` | 与日志中出现的同一个 UUID，因此一行 `Received:` 与一行日志可以关联起来 |
| `for <recipient>` | 第一个被接受的收件人 | 有多个收件人时省略（否则会泄露其它收件人），这是 RFC 5321 §4.4 对多收件人邮件的做法 |
| `; <date>` | 接受时刻的时钟 | RFC 5322 `date-time`，始终为 UTC、时区为 `+0000` |

该头字段绝不包含两样东西：对等端的 IP 取自套接字，而不是对等端发来的任何头字段；
并且对等端发来的任何头字段都绝不会被复制进生成的那一行。

当设置了 `policy.add_auth_results` 或 `policy.add_auth_results` 时，
`Authentication-Results` 会被单独加入；见 [security.md](security.md) §8。

---

## 11. 发信投递

项目书 §10 给出流水线；项目书 §11 给出队列状态与重试计划。

```text
 SMTP submission / Webmail / Client API
                │
                ▼
            Mail Core                renders RFC 5322, signs DKIM,
                │                    writes the Sent copy
                ▼
            mail_queue                one row per recipient
                │
                ▼
        QueueRepository::claim_due()  status pending|retry → delivering
                │
                ▼
            DNS MX                    per recipient domain
                │
                ▼
       remote MX hosts, in preference order, :25
                │
                ▼
        SMTP client (EHLO → STARTTLS if offered → MAIL → RCPT → DATA)
                │
                ├── success  ──► delivered   + delivery_attempts row
                └── failure  ──► retry | failed, per §12
```

### 11.1 队列状态及承载它们的列

`mail_queue.status` 带有一个 `CHECK` 约束，恰好列出这些取值：
`pending`、`delivering`、`delivered`、`retry`、`failed`、`cancelled`。

```text
  pending ──► delivering ──┬──► delivered
     ▲                     │
     │                     ├──► retry ──► delivering ──► …
     └─────────────────────┘
                           └──► failed   (attempts exhausted, or a 5xx)
```

| 列 | 含义 |
|---|---|
| `mail_queue.message_id` | 本次投递所针对的存储副本；`ON DELETE CASCADE` |
| `mail_queue.sender` | 信封反向路径 |
| `mail_queue.recipient` | 信封正向路径——每个收件人一行，因此一个坏收件人不会拖慢其它收件人 |
| `mail_queue.attempts` / `max_attempts` | 迄今尝试次数 / 上限（`queue.max_attempts`，12） |
| `mail_queue.next_attempt_at` | 调度器何时可以取走该行；`mail_queue_due_idx` 恰好索引 `WHERE status IN ('pending','retry')` |
| `mail_queue.last_error`、`last_status_code`、`last_status_text` | 最近一次失败，供 Admin 队列界面使用 |
| `mail_queue.remote_mx` | 尝试过的主机 |
| `mail_queue.delivered_at` | 最终成功的时刻 |

每次尝试还会写入一行 `delivery_attempts`（`queue_id`、`attempt`、`remote_mx`、
`status_code`、`status_text`、`error`、`duration_ms`），Admin 的「Delivery Logs」
界面读的就是它。尝试是历史；队列行是状态。

### 11.2 MX 解析

_(计划中)_ `ferroma-smtp::mx::MxResolver` 使用 `hickory-resolver`（由 `[dns]`
配置）。

1. 查询收件域的 `MX`。
2. 按 preference 升序排序，相同 preference 内部用稳定的随机次序打破平局，这样重复
   尝试不会总是先打同一台主机——这也是 RFC 5321 §5.1 所推荐的，并且能防止一台死掉的
   MX 吸走所有重试。
3. **按顺序尝试主机。** 连接失败、`4xx` 问候语或 TLS 失败都会在同一次尝试内转向下一
   台主机。只有当每一台主机都失败时，这次尝试才被记为失败。
4. **Null MX。** 单条 `MX 0 .` 表示该域不接受任何邮件。这是永久性失败（`failed`，
   并且退信）：在本地等价于 `556 5.1.10`，即 `FerromaError::Invalid`。
5. **没有 MX，但有 `A`/`AAAA` 记录。** 按照 RFC 5321 §5.1 回退到地址本身。只有当
   回退也失败时，该域才不可投递。
6. **既没有 MX 也没有地址。** `FerromaError::Dns`——按 §12.5 分类，会被归为临时性：
   一个域可能正处于注册过程中。
7. **CNAME 链**由解析器跟进，受 `dns.attempts` 限制。
8. 超时取自 `[dns]`（`timeout_secs`、`attempts`、`tcp_fallback`）。

对单一主机的连接并发数由 `queue.max_connections_per_host`（4）封顶：用四百条并行连接
猛砸一台远端 MX，正是一台邮件服务器把自己搞进黑名单的方式。

### 11.3 重试计划（§11）

`queue.retry_schedule_secs = [60, 300, 900, 3600, 21600, 86400]`——一分钟、五分钟、
十五分钟、一小时、六小时、二十四小时。最后一项会一直重复，直到 `attempts` 达到
`queue.max_attempts`（12）。

`QueueConfig::backoff_for_attempt(attempt)` 就是该规则的实现：它把索引钳制到计划末尾，
因此第 7、8、… 次尝试都等待 86 400 秒，并在计划为空时返回 60。

| 尝试 | 之前的延迟 | 累计经过时间（约） |
|---|---|---|
| 1 | 立即（接受时） | 0 |
| 2 | 60 s | 1 min |
| 3 | 300 s | 6 min |
| 4 | 900 s | 21 min |
| 5 | 3600 s | 1 h 21 min |
| 6 | 21600 s | 7 h 21 min |
| 7 | 86400 s | 31 h 21 min |
| 8–12 | 各 86400 s | 最多约 5 天 7 小时 |

尝试四天半是 MTA 的常规姿态：长到远端服务器周末的一次故障不会退掉你用户的邮件，
又短到发件人最终会知道邮件没送到。`queue.retention_days`（30）管辖 `delivered` 行在
那之后保留多久；它与重试窗口无关。

调度器以 `queue.poll_interval_secs`（10）轮询，并发运行 `queue.workers`（4）条投递。

### 11.4 出站中继（smarthost）

有些主机没法直接投递：公网 IP 没有 PTR 记录，或者服务商拒绝设置它（工单都提了也不行）。
这类 IP 发出的邮件会被 Gmail 判进垃圾箱、被微软系直接拒收 —— 而**收信完全不受影响**，
只有出站需要绕道。

```toml
[queue]
# 服务商的提交服务、事务邮件 API，或另一台有正确 PTR 的主机
relay_host = "smtp.example-relay.com"
relay_port = 587
# starttls(587) | implicit(465) | none（内网中继，绝不与凭据同时使用）
relay_tls = "starttls"
relay_username = "…"
relay_password = "…"
# 留空 = 所有出站邮件都走中继；列出域则只有这些域的邮件走中继
relay_from_domains = ["example.com"]
```

行为要点：

* 队列 worker **跳过 MX 解析**，把信封直接交给中继的连接；`relay_from_domains` 之外的域
  仍按 §11.2 正常解析 MX 直投。中继主机名自身用 `A`/`AAAA` 解析（`MxResolver::addresses`）。
* **本地域投递本来就不经过队列**，所以站内邮件不受影响。
* **DKIM 签名在把邮件交给中继之前完成**，签名带你自己的域，DMARC 对齐不受影响；SPF 需要
  把中继的发送域 `include:` 进你的记录（[security.md](security.md) §8）。
* 需要 `AUTH` 时，凭据**只在加密信道上发送**：`relay_tls = "none"` 与凭据同时配置会在
  启动校验时被拒绝。优先 `AUTH PLAIN`（带初始响应），服务端只支持 `LOGIN` 时回退
  `AUTH LOGIN`。
* **认证失败算临时性失败，不会退信**：密码写错是配置问题，不该把已经入队的邮件全退掉。
* 退信（空信封发件人）同样走中继 —— 反向解析缺失的主机，最容易被拒的就是这类邮件。
* 启动横幅会明说这件事：`queue     4 worker(s), outbound via relay smtp.example-relay.com`。

---

## 12. `4xx` 与 `5xx`，以及 `FerromaError::is_temporary()`

分类不是在 SMTP 层决定的。它就是 `FerromaError::is_temporary()`——`crates/ferroma-core/src/error.rs`
中的一个函数——它是 SMTP 应答类别与队列「重试还是失败」决定背后的唯一真相来源。

```rust
pub fn is_temporary(&self) -> bool {
    matches!(
        self,
        FerromaError::Io(_)
            | FerromaError::Network(_)
            | FerromaError::Dns(_)
            | FerromaError::RateLimited
            | FerromaError::Timeout(_)
            | FerromaError::Storage(_)
            | FerromaError::Internal(_)
    )
}
```

| `FerromaError` 变体 | `code()` | 临时？ | 对一次投递的含义 |
|---|---|---|---|
| `Io` | `io_error` | 是 | 本地文件系统/套接字失败——再试一次 |
| `Network` | `network_error` | 是 | 出站连接失败 |
| `Dns` | `dns_error` | 是 | MX/A 查询失败或超时 |
| `RateLimited` | `rate_limited` | 是 | 被限流，退避 |
| `Timeout` | `timeout` | 是 | 超过超时上限 |
| `Storage` | `storage_error` | 是 | 数据库或邮件存储失败——**邮件本身没有问题** |
| `Internal` | `internal_error` | 是 | 一个 bug；重试是保守的选择，并且该失败会大声记入日志 |
| `Config` | `config_error` | 否 | 不可用的配置——绝不是单封邮件层面的情况 |
| `Parse` | `parse_error` | 否 | 输入格式错误 |
| `NotFound` | `not_found` | 否 | 被引用的实体不存在 |
| `Conflict` | `conflict` | 否 | 唯一性或状态违例 |
| `Invalid` | `invalid_input` | 否 | 校验失败 |
| `Unauthorized` | `unauthorized` | 否 | 凭据缺失或错误 |
| `Forbidden` | `forbidden` | 否 | 策略拒绝，例如中继被拒 |
| `LimitExceeded` | `limit_exceeded` | 否 | 大小、收件人数或配额 |
| `Protocol` | `protocol_error` | 否 | 对等端违反了协议 |
| `Tls` | `tls_error` | 否 | 证书校验失败 |
| `Unsupported` | `unsupported` | 否 | 已被规定但未实现 |

一句话规则：**`is_temporary() == true` ⇒ `4xx` 并重新入队；
`is_temporary() == false` ⇒ `5xx` 并失败。** 一个协议层如果靠字符串匹配错误消息来
自行分类错误，那就是 bug；请新增变体或修正 `is_temporary()`。

### 12.1 收信：Ferroma 对某对等端应答什么

| 情况 | 应答 | 类别 |
|---|---|---|
| 邮件已存储 | `250 2.0.0 Ok: queued as <id>` | 2xx |
| `DATA` 正文超过 `limits.max_message_size` | `552 5.3.4 Message size exceeds fixed maximum message size` | 5xx，永久——重试不会让它变小 |
| 收件人数超过 `limits.max_recipients` | `452 4.5.3 Too many recipients` | 4xx——RFC 5321 §4.5.3.1.10 规定这是临时性应答 |
| 未知本地域 | `550 5.1.2 Relay access denied` | 5xx |
| 未知本地地址 | `550 5.1.1 No such user here` | 5xx |
| 向非本地域中继，未认证 | `550 5.7.1 Relaying denied` | 5xx |
| 邮箱超出配额 | `452 4.2.2 Mailbox full` | 4xx——用户可以腾出空间 |
| 数据库或 Maildir 写入失败 | `451 4.3.0 Temporary local problem` | 4xx——`FerromaError::Storage`，重试 |
| 邮件根目录所在磁盘写满 | `452 4.3.1 Insufficient system storage` | 4xx |
| 存储时发生内部错误 | `451 4.3.0 Temporary local problem` | 4xx——`FerromaError::Internal` |
| 超出速率限制 | `421 4.7.0 Too many commands, slow down` | 4xx，随后关闭 |
| SPF 硬失败（`-all`）_(计划中)_ | `550 5.7.23 SPF validation failed` | 5xx |
| DMARC 失败且 `p=reject` _(计划中)_ | `550 5.7.1 DMARC policy violation` | 5xx |
| DKIM 校验失败且 `dkim.verify_inbound` _(计划中)_ | `550 5.7.20 DKIM signature validation failed` | 5xx |

对上文直觉的两处更正，都是刻意的：

* **超配额是 `452`，不是 `550`。** 邮箱满了是*收件人*的临时状况，不是该地址的永久
  属性。`550` 会让发件人的 MTA 立即退信，用户就会丢掉那些在他超配额期间到达的邮件。
  RFC 3463 把 `4.2.2` 编码为「mailbox full」正是出于这个原因。因此存储层的
  `StorageError::QuotaExceeded` 只在 SMTP 应答中映射为临时类别——在队列层面它仍然是
  `is_temporary() == false`，因为在那一层过错确实属于邮件本身。
* **任何存储失败都用 `451 4.3.0`**，包括数据库中断。收到 `451` 的远端服务器会重试
  好几天；而 `550` 会因为我们的磁盘满了十秒钟就永久退掉这封邮件。

### 12.2 发信：远端应答对队列意味着什么

| 远端应答 | 队列决定 | 理由 |
|---|---|---|
| 最后一个点号之后的 `2xx` | `delivered`，写入 `delivered_at`、`delivery_attempts` | 完成 |
| 任意时刻的 `4xx` | `retry`，`next_attempt_at = now + backoff_for_attempt(attempts)`，记录 `last_status_code`/`last_status_text` | 远端要求我们稍后再来 |
| 任意时刻的 `5xx` | 立即 `failed`，不再尝试，若 `queue.bounce_on_failure` 则退信 | 远端永久拒绝；继续重试就是滥用 |
| 连接被拒 / 超时 / 重置 | `retry`（`FerromaError::Network` / `Timeout`） | 试下一台 MX，然后退避 |
| TLS 握手失败 | `retry` | 远端那边常常是在轮换证书 |
| MX 查询失败 | `retry`（`FerromaError::Dns`） | DNS 抖动是暂时的 |
| 所有 MX 主机都试过且都以 4xx 失败 | `retry` | 一次尝试覆盖每一台主机；退避作用于整个域 |
| `attempts` 达到 `max_attempts` | `failed`，若 `queue.bounce_on_failure` 则退信 | 重试窗口已耗尽 |
| 多收件人邮件中某一位收件人收到远端 `5xx` | 那一行 `mail_queue` 失败；其它收件人不受影响 | 每个收件人一行，正是为了让这种情况按收件人处理 |

### 12.3 增强状态码

宣告了 `ENHANCEDSTATUSCODES`（RFC 3463），因此每个应答在基本码之后都带一个 `x.y.z`
类别。Ferroma 发出的类别：

| 增强码 | 含义 | 来源 |
|---|---|---|
| `2.0.0` | 其它/未定义状态，成功 | 邮件被接受 |
| `2.1.0` | 发件人正常 | `MAIL FROM` 被接受 |
| `2.1.5` | 收件人正常 | `RCPT TO` 被接受 |
| `2.5.2` | 无法 `VRFY` 用户 | `VRFY` |
| `2.7.0` | 安全策略正常 | `AUTH` 成功 |
| `4.2.2` | 邮箱已满 | 超出配额 |
| `4.3.0` | 其它邮件系统状态 | 本地存储/数据库失败 |
| `4.3.1` | 邮件系统已满 | 磁盘写满 |
| `4.3.2` | 系统不接受网络邮件 | 连接数封顶 |
| `4.4.2` | 连接不良 | 超时 |
| `4.5.3` | 收件人过多 | `limits.max_recipients` |
| `4.7.0` | 安全策略，临时性 | 速率限制、被限流的 AUTH |
| `5.1.1` | 目的邮箱地址错误 | 未知本地地址 |
| `5.1.2` | 目的系统地址错误 | 未知本地域 |
| `5.3.4` | 邮件对本系统过大 | 大小限制 |
| `5.5.1` | 无效命令 | 顺序违例 |
| `5.5.2` | 语法错误 | 无法解析的命令行 |
| `5.5.4` | 无效的命令参数 | base64 错误、参数错误 |
| `5.7.0` | 安全策略，永久性 | 要求认证 |
| `5.7.1` | 投递未获授权 | 中继被拒、发件人不属于该用户 |
| `5.7.8` | 认证凭据无效 | 口令错误 |
| `5.7.11` | 要求加密 | 明文 `AUTH` 被拒 |
| `5.7.20` | DKIM 签名校验失败 _(计划中)_ | 收信 DKIM |
| `5.7.23` | SPF 校验失败 _(计划中)_ | 收信 SPF |

当 `smtp.advertise_extensions = false` 时，增强码被省略，应答是光秃秃的三位数字码
加上没有类别的文本。

### 12.4 DSN

`DSN`（RFC 3461）会被宣告 _(计划中)_，并且 `MAIL FROM` 上的参数
（`RET=FULL|HDRS`、`ENVID=`）与 `RCPT TO` 上的参数（`NOTIFY=`、`ORCPT=`）会记录到
队列行上。对某个收件人设置 `NOTIFY=NEVER` 会抑制该收件人的退信。在它实现之前，
`DSN` **不会**被宣告，那些参数也会被忽略，这正是 RFC 5321 §4.1.1.11 对不支持它们的
服务器所要求的做法。

### 12.5 完整的收信应答表

| 应答 | 文本 | 触发条件 |
|---|---|---|
| `220` | `<smtp.banner>` | 连接被接受 |
| `220` | `2.0.0 Ready to start TLS` | `STARTTLS` |
| `221` | `2.0.0 Bye` | `QUIT` |
| `235` | `2.7.0 Authentication successful` | `AUTH` |
| `250` | `2.0.0 Ok` | `EHLO`/`HELO` 最后一行、`NOOP`、`RSET` |
| `250` | `2.1.0 Ok` | `MAIL FROM` |
| `250` | `2.1.5 Ok` | `RCPT TO` |
| `250` | `2.0.0 Ok: queued as <id>` | `DATA` 结束 |
| `252` | `2.5.2 Cannot VRFY user, but will accept message and attempt delivery` | `VRFY` |
| `334` | `<base64 prompt>` | `AUTH` 续行 |
| `354` | `End data with <CR><LF>.<CR><LF>` | `DATA` |
| `421` | `4.3.2 Too many connections, try again later` | `limits.max_connections` |
| `421` | `4.3.2 Too many connections from your address` | `limits.max_connections_per_ip` |
| `421` | `4.4.2 Timeout waiting for command` | `smtp.command_timeout_secs` |
| `421` | `4.4.2 Timeout waiting for data` | `smtp.data_timeout_secs` |
| `421` | `4.7.0 Too many commands, slow down` | `limits.smtp_rate_limit` |
| `450` | `4.2.0 Mailbox busy, try again later` | 临时锁竞争 _(计划中)_ |
| `451` | `4.3.0 Temporary local problem` | `FerromaError::Storage` / `Internal` |
| `452` | `4.2.2 Mailbox full` | 配额 |
| `452` | `4.3.1 Insufficient system storage` | 磁盘写满 |
| `452` | `4.5.3 Too many recipients` | `limits.max_recipients` |
| `452` | `4.7.0 Submission rate limit exceeded` | `limits.submission_rate_limit` |
| `452` | `4.7.0 Daily send limit exceeded` | `limits.daily_send_limit` |
| `454` | `4.7.0 Temporary authentication failure` | 锁定，或按 IP 的失败洪流 |
| `500` | `5.5.2 Command unrecognized` | 未知动词 |
| `500` | `5.5.2 Line too long` | 命令行超过 512 个八位组 |
| `501` | `5.5.4 Invalid base64 data` | SASL 数据块格式错误 |
| `501` | `5.5.4 Syntax error in parameters` | `MAIL`/`RCPT` 无法解析 |
| `502` | `5.5.1 Command not implemented` | `EXPN`、`BDAT`、无 TLS 时的 `STARTTLS` |
| `503` | `5.5.1 Send HELO/EHLO first` | `helo_required` |
| `503` | `5.5.1 Need MAIL FROM before RCPT TO` | 顺序 |
| `503` | `5.5.1 Need RCPT TO before DATA` | 顺序 |
| `503` | `5.5.1 Sender already specified` | 第二条 `MAIL FROM` |
| `503` | `5.5.1 Already authenticated` | 第二次 `AUTH` |
| `503` | `5.5.1 TLS already active` | `STARTTLS` 两次 |
| `504` | `5.5.4 Unrecognized authentication type` | `AUTH CRAM-MD5` |
| `530` | `5.7.0 Authentication required` | `require_auth_on_submission` |
| `535` | `5.7.8 Authentication credentials invalid` | 凭据错误 |
| `538` | `5.7.11 Encryption required for requested authentication mechanism` | `require_tls_for_auth` |
| `550` | `5.1.1 No such user here` | 未知本地地址 |
| `550` | `5.1.2 Relay access denied` | 未知本地域 |
| `550` | `5.7.1 Relaying denied` | 未认证的中继尝试 |
| `550` | `5.7.1 Sender address rejected: not owned by user` | 外来 `MAIL FROM` |
| `550` | `5.7.23 SPF validation failed` _(计划中)_ | SPF |
| `550` | `5.7.1 DMARC policy violation` _(计划中)_ | DMARC |
| `550` | `5.7.20 DKIM signature validation failed` _(计划中)_ | DKIM |
| `552` | `5.3.4 Message size exceeds fixed maximum message size` | 大小限制 |
| `552` | `5.3.4 Too many message parts` _(计划中)_ | `limits.max_attachments` |
| `554` | `5.5.1 Pipelining violated` | 命令跨 `STARTTLS` 流水线化 |
| `554` | `5.7.1 Message rejected` | catch-all 策略拒绝 _(计划中)_ |

---

## 13. 退信生成

当一次投递以 `failed` 结束且 `queue.bounce_on_failure = true` 时，Ferroma 会向信封
发件人回发一封投递状态通知。项目书 §11 没有写明格式，因此本节就是规范。

### 13.1 退给谁

| 信封发件人 | 行为 |
|---|---|
| 一个存在的本地地址 | 退信投递进该邮箱的 `INBOX`，`From: MAILER-DAEMON@<server.hostname>` |
| 一个已不存在的本地地址 | 退信被丢弃，以 `warn` 级别记入日志 |
| 一个远端地址 | 一行新的 `mail_queue`，发件人为空（`MAIL FROM:<>`），适用同样的重试策略；退信自身若退信则被丢弃 |
| 空（`<>`） | 永不退信——它本身已经是一封退信，而给它退信正是邮件环路开始的方式 |

空发件人规则很要紧：RFC 5321 §6.1 与 RFC 3464 都要求通知的反向路径为空，而一台
会给退信再退信的服务器，会乐于在两台配置错误的 MTA 之间制造无限环路。

### 13.2 退信是一封 DSN

`multipart/report; report-type=delivery-status`（RFC 3462/3464），包含：

| 部分 | 内容类型 | 内容 |
|---|---|---|
| 1 | `text/plain; charset=utf-8` | 人类可读的说明：哪一位收件人、为什么、以及尝试了多久 |
| 2 | `message/delivery-status` | 逐邮件字段（`Reporting-MTA`、`Arrival-Date`）与逐收件人字段（`Final-Recipient`、`Action: failed`、`Status: 5.1.1`、`Diagnostic-Code: smtp; 550 5.1.1 No such user here`） |
| 3 | `message/rfc822`（或 `text/rfc822-headers`） | 原邮件的头字段，或在它较小时给出整封邮件 |

用 `ferroma_mail::MessageBuilder` 构建；原始字节按 `messages.storage_path` 取自
Maildir。`Status:` 字段携带 §12.3 的*增强*码，这样发件人的客户端可以依据类别而不是
文字描述来行动。

### 13.3 何时生成退信

| 条件 | 退信？ |
|---|---|
| 最后一个点号处收到远端 `5xx` | 是，立即 |
| `RCPT TO` 处收到远端 `5xx` | 是，对该收件人 |
| `attempts` 达到 `queue.max_attempts` | 是 |
| Null MX | 是 |
| 收件人是一个不存在的本地地址 | 在 `RCPT TO` 时生成，不是由队列生成 |
| `queue.bounce_on_failure = false` | 不退信；失败记录在 `mail_queue` 与 `delivery_attempts` 中并在 Admin 中展示 |
| 邮件来自本地提交且发件人仍处于连接中 | submission 已经返回 `250`；退信是唯一的反馈途径 |

退信在 `In-Reply-To` 与 `References` 中携带原 `Message-ID`，因此客户端可以把
「Undelivered Mail Returned to Sender」与用户实际发出的那封邮件串在一起。

---

## 14. 手工诊断 SMTP

项目书 §44 列出了工具。以下全部都可对本地运行的服务器使用；25 端口是收信监听器，
587 是 submission 监听器。

```bash
# Greeting, capabilities and a full transaction, unencrypted.
swaks --server 127.0.0.1 --port 25 --from bob@example.net --to alice@example.com --body "test"

# The same, forcing STARTTLS.
swaks --server 127.0.0.1 --port 587 --tls --auth PLAIN --auth-user alice@example.com --auth-password '…'

# By hand: type EHLO, MAIL FROM, RCPT TO, DATA.
nc 127.0.0.1 25

# Which capabilities does the submission port advertise, and does it offer STARTTLS?
openssl s_client -starttls smtp -connect 127.0.0.1:587 -crlf

# Implicit TLS on 465.
openssl s_client -connect 127.0.0.1:465

# Is the MX record the one Ferroma will use?
dig +short MX example.com
```

问候语与能力交换的示意输出：

```text
220 mail.example.com Ferroma ESMTP ready
EHLO client.example.net
250-mail.example.com
250-PIPELINING
250-SIZE 26214400
250-8BITMIME
250-ENHANCEDSTATUSCODES
250-SMTPUTF8
250-STARTTLS
250-AUTH PLAIN LOGIN
250 HELP
MAIL FROM:<bob@example.net>
250 2.1.0 Ok
RCPT TO:<alice@example.com>
250 2.1.5 Ok
DATA
354 End data with <CR><LF>.<CR><LF>
Subject: test

hello
.
250 2.0.0 Ok: queued as 4821
QUIT
221 2.0.0 Bye
```

关于「邮件送不到」和「队列在不断增长」，见 [deployment.md](deployment.md) §11 中
按症状编排的命令清单。

---

## 15. 相关文档

| 主题 | 文档 |
|---|---|
| 端口、DNS 记录、TLS 终止、首次运行设置 | [deployment.md](deployment.md) |
| SPF、DKIM、DMARC、HTML 清洗、中继防御的理由 | [security.md](security.md) |
| 字节最终落在哪里，以及 `messages` / `mail_queue` 模式 | [storage.md](storage.md) |
| 从客户端视角看重的重试状态机，Outbox | [sync.md](sync.md)、[client.md](client.md) |
| 把邮件入队的 API：`POST /api/v1/messages` | [api.md](api.md) §5.2 |
| crate 分层与请求生命周期 | [architecture.md](architecture.md) |
