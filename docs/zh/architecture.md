# Ferroma 架构

**谁应该读这份文档：**任何准备为 Ferroma 添加一个 crate、一个协议面或一个
后台任务的人。它是本目录其它文档视为理所当然的那张地图。

Ferroma 是一个单一的 Rust 进程，它讲 SMTP、IMAP 和 HTTPS，把
邮件元数据存进 PostgreSQL，把邮件字节存进同一台主机上的一个 Maildir。
所有不是协议解析器的东西都经过同一个邮件核心和同一套仓储层。这份文档
描述四个产品、crate 依赖图、分层规则，以及一封邮件从远端 MX 打开一条
TCP 连接的那一刻，到客户端的 socket 收到一个 `mail.received` 帧的那一刻，
中间会发生什么。

> **状态：**对当前仓库的架构描述。`ferroma-core`、`ferroma-mail`、
> `ferroma-storage`、`ferroma-auth` 和 `ferroma-events` 这几个 crate 已实现。
> `ferroma-smtp`、`ferroma-imap`、`ferroma-sync`、`ferroma-api`、`server` 和
> `client` 是 crate 骨架，有文档化的接口但尚无实现；下文关于它们内部的一切陈述
> 都标记为_(计划中)_，是设计规格，不是观察结论。HTTP 与 FCP 的线上契约另行冻结在
> [api.md](api.md) 和 [fcp.md](fcp.md) 中，本文档从不重述它们。

---

## 1. 四个产品

| 产品 | 所在位置 | 它是什么 | 状态 |
|---|---|---|---|
| **Ferroma Server** | `server/`（二进制 `ferroma`）、`crates/*` | 守护进程：SMTP、IMAP、HTTP API、队列工作进程、同步服务、事件总线 | 二进制是桩；库部分实现 |
| **Ferroma Webmail** | `web/` | 浏览器邮件客户端，一个由 API 提供服务的静态 SPA | SPA 源码已存在；尚未被服务 _(计划中)_ |
| **Ferroma Admin** | `admin/` | 域名/用户/队列/DNS/存储管理 SPA | SPA 源码已存在；尚未被服务 _(计划中)_ |
| **Ferroma Client** | `client/`（二进制 `ferroma-client`） | 官方桌面客户端（Windows、Linux、macOS），共享核心加 UI 外壳 | 骨架 |

Webmail 和 Admin 不是独立进程。它们是由 `ferroma-api` 在与
`/api/v1` 同源之下提供的静态资源，受 `api.serve_frontend` 门控，
Admin 路由还受 `is_admin` 门控。两者都消费 **Management API**；
官方客户端消费 **Client API (FCP)**，别的都不消费。

Webmail 与官方客户端刻意共享四个子系统：邮件核心、
API、事件总线和认证。这样服务端上「把这封邮件标为已读」就恰好只有
一个实现。项目书 §35。

---

## 2. Crate 依赖图

工作区成员，来自 `Cargo.toml`：

```text
                                 ┌───────────────┐
                                 │ ferroma-core  │  config, error, ids,
                                 │               │  address, limits, logging
                                 └───────┬───────┘
                    ┌────────────────────┼────────────────────┬──────────────┐
                    │                    │                    │              │
                    ▼                    ▼                    ▼              ▼
            ┌───────────────┐   ┌────────────────┐   ┌──────────────┐  ┌───────────┐
            │ ferroma-mail  │   │ferroma-storage │   │ferroma-auth  │  │ferroma-   │
            │ RFC 5322+MIME │   │ PG + Maildir + │   │ Argon2id,    │  │events     │
            │               │   │ blob store     │   │ tokens,      │  │ bus +     │
            └───────┬───────┘   └───────┬────────┘   │ devices      │  │ envelopes │
                    │                   │            └──────┬───────┘  └─────┬─────┘
                    │                   │                   │                │
      ┌─────────────┴──────────┬────────┴───────────┬───────┘                │
      │                        │                    │                        │
      ▼                        ▼                    ▼                        │
┌───────────┐           ┌─────────────┐      ┌──────────────┐                │
│ferroma-   │           │ferroma-imap │      │ferroma-sync  │◄───────────────┘
│smtp       │           │             │      │ changelog,   │
│server+    │           │ IMAP4rev1   │      │ cursors,     │
│client+mx+ │           │ server      │      │ operations   │
│dkim/spf/  │           │             │      │              │
│dmarc      │           │             │      │              │
└─────┬─────┘           └──────┬──────┘      └──────┬───────┘
      │                        │                    │
      └────────────┬───────────┴────────────────────┘
                   ▼
            ┌─────────────┐
            │ ferroma-api │  REST + FCP + WebSocket + frontends
            └──────┬──────┘
                   │
        ┌──────────┴──────────┐
        ▼                     ▼
  ┌───────────┐        ┌────────────┐
  │  server/  │        │  client/   │
  │  ferroma  │        │ ferroma-   │
  │  binary   │        │ client     │
  └───────────┘        └────────────┘
```

依赖边以 manifest 为准，不要从图上读：

| Crate | 依赖于 |
|---|---|
| `ferroma-core` | 内部什么都不依赖 |
| `ferroma-mail` | `ferroma-core` |
| `ferroma-storage` | `ferroma-core` |
| `ferroma-auth` | `ferroma-core`、`ferroma-storage` |
| `ferroma-events` | `ferroma-core` |
| `ferroma-smtp` | core、mail、storage、auth、events |
| `ferroma-imap` | core、mail、storage、auth、events |
| `ferroma-sync` | core、mail、storage、events |
| `ferroma-api` | core、mail、storage、auth、events、sync、smtp |
| `server` | 上面的每一个 crate |
| `client` | 只有 `ferroma-core` |

在你动手改一个 manifest 之前，有两点后果值得知道：

1. **`ferroma-core` 永不依赖数据库驱动。** `ferroma-storage`
   把 `sqlx::Error` 翻译成自己的 `StorageError`，然后才翻译成
   `FerromaError`（`crates/ferroma-storage/src/error.rs`），从而把 `sqlx` 挡在
   它之上所有东西的公开 API 之外。
2. **`client` 只共享 `ferroma-core`，别的都不共享。** Cargo 会很乐意让
   客户端链接 `ferroma-storage`，但桌面客户端绝不能把一个
   PostgreSQL 连接池或一个 Maildir 拖进要发布的二进制；它自己的 SQLite 缓存位于
   `client/src/database/` _(计划中)_。

---

## 3. 分层规则

不可协商，出自 [../AGENTS.md](../../AGENTS.md) §4.5 与项目书 §8：

```text
    Protocol layer            ferroma-smtp, ferroma-imap, ferroma-api, client
    (parse, authenticate, marshal, reply)
              │
              ▼
    Mail core + repositories  ferroma-mail, ferroma-storage,
                              ferroma-auth, ferroma-sync
              │
              ▼
    Storage / Queue /         PostgreSQL, Maildir, attachment blob store
    Delivery
```

**协议层不含业务逻辑。** SMTP 会话解析器不判断一个地址是否存在；
它把信封交给邮件核心，并把得到的 `FerromaError` 变成一个应答码。IMAP 的 `STORE`
处理函数不编辑标志字符串；它调用 `MessagesRepository::set_flags` 以及
maildir 的 `set_flags`。一条 Webmail 路由不构造 MIME；它调用
`ferroma-mail::MessageBuilder`。

原因不是纯粹性。原因是 SMTP、IMAP、Webmail 和客户端 API 各自
对「已读」「删除」和「大小」有不同理解，而让它们保持一致，并让
同步变更日志保持诚实的唯一办法，是让这四者都调用同一个实现。

今天这条边界由什么守住：

| 规则 | 由什么强制 |
|---|---|
| 错误处处都是 `ferroma_core::FerromaError` | `crates/ferroma-core/src/error.rs` 里的 `FerromaError`；`From<StorageError> for FerromaError` |
| 禁止 `sqlx::query!` 宏（编译期数据库） | [../AGENTS.md](../../AGENTS.md) §4.3 的约定；`ferroma-storage::models` 里的 `sqlx::query_as` + `#[derive(FromRow)]` |
| 禁止对来自对端的输入用 `unwrap()` | [../AGENTS.md](../../AGENTS.md) §4.4 的约定 |
| 服务端永不是开放中继 | `SmtpConfig::require_auth_on_submission`、`FerromaError::Forbidden`，见 [smtp.md](smtp.md) §6 |
| 时间戳处处是 `TIMESTAMPTZ`、UTC | `migrations/0001_initial.sql`；模型里的 `chrono::DateTime<Utc>` |

---

## 4. 请求生命周期：一封收信 SMTP 邮件

一封邮件从陌生人的 MX 走到一行存储记录和一个 `new/` 文件所经过的路径。
标记为_(计划中)_的步骤描述的是按规格实现的 `ferroma-smtp` 与 `ferroma-api`；
存储与事件这两段步骤已实现。

```text
 remote MX ──TCP:25──► ferroma-smtp listener
                            │  accept, spawn session task
                            ▼
                       SmtpSession { state: Connected, … }        (spec §9.2, §9.3)
                            │  220 banner (smtp.banner)
                            ▼
                       EHLO ──► 250-… capabilities, 250 SIZE <limits.max_message_size>
                            │  state = Greeted
                            ▼
                       MAIL FROM:<bob@example.net>
                            │  parse reverse-path (ferroma-core::EmailAddress)
                            │  state = MailFrom
                            ▼
                       RCPT TO:<alice@example.com>
                            │  resolve domain → domains.name
                            │  resolve local part → mailboxes(domain_id, local_part)
                            │  unknown domain or address ⇒ 550 5.1.1 / 5.1.2
                            │  recipient count > limits.max_recipients ⇒ 452 4.5.3
                            ▼
                       DATA ──► 354, read dot-terminated body with limits
                            │  abort with 552 5.3.4 when SIZE is exceeded
                            ▼
                 ┌──────────────────────────────────────────┐
                 │  Mail Core                               │
                 │  1. parse: ferroma_mail::ParsedMessage   │
                 │  2. auth verdicts: SPF/DKIM/DMARC        │
                 │  3. prepend Received: (server.hostname)  │
                 │  4. quota: MailboxesRepository::         │
                 │             check_quota(mailbox, size)   │
                 └──────────────┬───────────────────────────┘
                                ▼
                 ┌──────────────────────────────────────────┐
                 │  Maildir::store(domain, local_part,      │
                 │                 "INBOX", bytes, flags)   │
                 │  tmp/ → fsync → rename into new/         │
                 └──────────────┬───────────────────────────┘
                                ▼
                 ┌──────────────────────────────────────────┐
                 │  MessagesRepository::insert(NewMessage)  │
                 │  allocates the IMAP UID from             │
                 │  folders.uid_next under the folder's     │
                 │  row lock (one statement, no race)       │
                 │  then MessagesRepository::insert_        │
                 │  recipients() and AttachmentsRepository  │
                 │  ::insert() per part, blobs via          │
                 │  AttachmentStore::store()                │
                 └──────────────┬───────────────────────────┘
                                ▼
                 ┌──────────────────────────────────────────┐
                 │  ChangeLogRepository::append(NewChange { │
                 │      kind: "message_created", … })       │
                 │  → one change_log row per affected       │
                 │    mailbox, seq is the client cursor     │
                 └──────────────┬───────────────────────────┘
                                ▼
                 ┌──────────────────────────────────────────┐
                 │  EventBus::publish(                      │
                 │      EventScope::User(user_id),          │
                 │      Event::mail_received(…))            │
                 └──────────────┬───────────────────────────┘
                                ▼
                    250 2.0.0 Ok: queued as <id>

  Event subscribers:  WebSocket /api/v1/client/events  → official client
                      Webmail session (SSE or WS)      → unread badge
                      notification service             → desktop toast
                      webhook consumer                 → HTTP POST
```

几条重要的顺序规则：

* Maildir 写入发生在行被插入**之前**。两者之间崩溃会留下一个孤儿文件
  （稍后由 `Maildir::sweep_tmp` 清扫）；反过来则会给出一行正文并不存在的
  记录，这在用户面前表现为 `StorageError::BodyMissing`。
* `messages.id` 和 `messages.uid` 由数据库分配。UID 只在
  使用它的那条 `INSERT` 内部被分配，见
  `crates/ferroma-storage/src/repository/messages.rs` 中的
  `WITH next_uid AS (UPDATE folders SET uid_next = uid_next + 1 …)` 语句。
* 变更日志行与邮件行在同一个事务里追加。一条已提交却没有变更日志条目的
  邮件，对每个已同步的客户端都不可见，而这正是游标模型存在所要防止的
  那种失败。

每种失败的应答码列在 [smtp.md](smtp.md) §12。

---

## 5. 请求生命周期：一次 API 调用

`GET /api/v1/client/messages/4821`，每个已认证请求共有的形状。

```text
 HTTP/1.1 or HTTP/2 (axum) behind rustls or a proxy
      │
      ▼
 1. version gate                     X-Ferroma-Protocol < client.min_protocol_version
      │                              ⇒ 426 { "code": "unsupported" }
      ▼
 2. authentication                   Authorization: Bearer <access token>
      │                              AuthService::authenticate(&token)
      │                                TokenService::verify_access → AccessClaims
      │                                sessions row must exist and not be revoked
      │                              failure ⇒ FerromaError::Unauthorized ⇒ 401
      ▼
 3. authorisation                    the message's mailbox_id must belong to
      │                              claims.user_id, else FerromaError::Forbidden
      │                              ⇒ 403
      ▼
 4. idempotency (mutating verbs)     OperationsRepository::begin(operation_id, …)
      │                                Fresh  ⇒ do the work, then complete(result)
      │                                Replay ⇒ return the cached result verbatim
      ▼
 5. handler                          protocol layer only: read path params,
      │                              build a `MessageSearch` / `NewMessage`, call
      ▼                              the repository or the mail core
 6. repository / mail core           sqlx over the PgPool; Maildir or
      │                              AttachmentStore for bytes
      ▼
 7. response mapping                 FerromaError::code() + http_status()
      │                              ⇒ the error envelope in api.md §1.3
      ▼
 8. side effects                     change_log append, change_log-driven sync,
                                     EventBus::publish for the realtime socket
```

`FerromaError::code()` 字面上就是每个 API 错误体的 `code` 字段，
`FerromaError::http_status()` 就是状态码，见 `crates/ferroma-core/src/error.rs`。
别处没有第二张映射表，一条自己发明 code 字符串的 API 路由就是 bug。

---

## 6. 事件总线

`ferroma-events` 已实现，它是进程范围内唯一的一个对象。规范
§22 与 §23。

```text
  producer (Mail Core, queue worker, auth service)
      │ EventBus::publish(scope, event)          or publish_nowait(…)
      ▼
  ┌─ one mutex ──────────────────────────────────────────┐
  │ last_seq += 1        seq is gap-free and monotonic   │
  │ ring: VecDeque<EventEnvelope>  (history_capacity)    │
  └───────────────────────┬──────────────────────────────┘
                          │ tokio::sync::broadcast::send
                          ▼
        per-subscriber Subscription { EventFilter, … }
              EventFilter::matches(&scope)   ← the authorisation boundary
                          │
        ┌─────────────────┴──────────────────┐
        ▼                                    ▼
  live WebSocket frames              EventBus::replay_since(after)
  EventEnvelope::to_wire()           for a reconnecting client
```

词汇，`Event` 定义在 `crates/ferroma-events/src/event.rs`：

| Rust 变体 | `Event::name()` | 线上 `type` | 载荷结构体 |
|---|---|---|---|
| `Event::MailReceived` | `mail.received` | `mail.received` | `MailReceived` |
| `Event::MailSent` | `mail.sent` | `mail.sent` | `MailSent` |
| `Event::MailDeleted` | `mail.deleted` | `mail.deleted` | `MailDeleted` |
| `Event::MailRead` | `mail.read` | `mail.read` | `MailRead` |
| `Event::MailFlagChanged` | `mail.flag_changed` | `mail.flag_changed` | `MailFlagChanged` |
| `Event::MailMoved` | `mail.moved` | `mail.moved` | `MailMoved` |
| `Event::DraftCreated` | `draft.created` | `draft.created` | `DraftCreated` |
| `Event::DraftUpdated` | `draft.updated` | `draft.updated` | `DraftUpdated` |
| `Event::DeliveryUpdated` | `delivery.updated` | `delivery.updated` | `DeliveryUpdated` |
| `Event::DeviceRevoked` | `device.revoked` | `device.revoked` | `DeviceRevoked` |

注意与项目书 §23 的一处命名分歧，那里列的是
`mail.updated`：实现把它拆成 `mail.read` 和
`mail.flag_changed`，并新增了 `mail.moved`、`draft.created` 和
`device.revoked`。冻结的线上列表在 [fcp.md](fcp.md) §8。

作用域就是授权边界：`EventScope::User(UserId)`、
`EventScope::Mailbox(MailboxId)` 或 `EventScope::System`。一个被过滤到
`User(7)` 的订阅者观察不到别的用户的帧，尽管所有会话共享同一个总线。

保证及其边界：

* `seq` 严格单调且无空洞，即使在并发下也如此；计数器与
  重放环在同一把互斥锁下变更，广播也在持锁期间发生，因此订阅者
  也按 `seq` 顺序观察事件。
* 默认值：`history_capacity = 1024`、`channel_capacity = 512`、
  `drop_on_lag = true`。一个停止读取的订阅者会得到
  `SubscriptionError::Lagged`，而不是把邮件核心拖住。`drop_on_lag =
  false` 会被接受，但行为与 `true` 相同，总线没有发布者
  背压模式。
* **没有邮件正文穿过总线。** `MailReceived` 携带一个 `snippet`、
  一个主题和一个大小，从不携带字节。

**总线只在进程内。** 没有 Redis 或 NATS 后端，也没有
跨进程扇出 _(计划中)_。两个共享同一个数据库的 `ferroma` 进程
有两条独立的事件流；连到进程 A 的客户端，在跑一次同步之前看不到
通过进程 B 做的改动。这是水平扩展上的一条真实约束，在 [security.md](security.md) 和
[deployment.md](deployment.md) 中再次列出。

---

## 7. 同步模型

完整论述见 [sync.md](sync.md)；这里只给形状。

```text
   Server (source of truth)                         Client (cache)
   ─────────────────────────                        ──────────────
   messages, folders, drafts                        SQLite cache
        │                                                │
        │  every mutation appends one change_log row     │
        ▼                                                │
   change_log(seq BIGSERIAL, user_id,                    │
              mailbox_id, folder_id,                     │
              message_id, kind, payload)                 │
        │                                                │
        │  GET /api/v1/client/sync?cursor=N ────────────►│
        │◄──── { next_cursor, has_more, changes[] } ─────│
        │                                                │
        │                                     apply all changes durably
        │                                     then store next_cursor
        │                                                │
        │  POST /… { operation_id: "op_…" } ◄────────────│  Outbox
        │  OperationsRepository::begin()                 │
        │    Fresh  → execute, record result             │
        │    Replay → return the recorded response       │
```

三个由构造保证的性质：

1. **游标就是 `change_log.seq`。** `ChangeLogRepository::changes_since(user_id,
   after, limit)` 以升序选出 `seq > after`，所以传入最后应用过的
   `seq` 永不重复某一条。客户端把它当作不透明的值。
2. **删除是只追加的墓碑。** `change_log.message_id` 与 `change_log.folder_id`
   都刻意*不*是外键，因此一行 `message_deleted` 或 `folder_deleted`
   比它所描述的行活得更久。见 `migrations/0001_initial.sql` 与
   `migrations/0003_change_log_folder_tombstones.sql` 中的注释。
3. **重放是幂等的。** `OperationsRepository::begin` 是一条
   `INSERT … ON CONFLICT DO NOTHING RETURNING *`；对同一个 `operation_id` 的两次并发重试
   中，恰好一次得到 `OperationOutcome::Fresh`。

---

## 8. 为什么这样选择

**为什么元数据用 PostgreSQL，字节用文件系统。** 邮件正文是
只追加的大块二进制；元数据小、热且关系型。项目书 §6.2
说的正是这一点（「邮件正文和附件不建议全部直接存入数据库」）。把正文挡在
PostgreSQL 之外，`pg_dump` 才小到能每晚跑，`VACUUM` 才保持廉价，
你也才能用各自合适的工具分别备份这两半。

**为什么用 Maildir，而不是数据库的 blob 列或对象存储。** Maildir
是一棵任何邮件管理员都能用 `ls` 查看、用 `tar` 恢复、
并在 Ferroma 哪天被弃用时交给 Dovecot 的目录树。它还免费给投递
带来原子性：写入 `tmp/`，`rename(2)` 进 `new/`，读者
永远观察不到一封残缺的邮件。项目书 §13。

**为什么用 rustls，并且绝不用 native-tls。** 两个恰好互相印证的
理由。在这台开发主机上 Windows TLS 栈是坏的（`schannel` 以
`SEC_E_NO_CREDENTIALS` 失败），所以 `native-tls` 连编译或连接都做不到；而
对于一个自己终结 SMTP、IMAP 和 HTTPS 的服务端，一个内存安全、
密码套件列表显式的 TLS 实现才是正确的默认。每个
支持 TLS 的依赖都在工作区 `Cargo.toml` 里被钉到 rustls，并且
`AGENTS.md` §1.1 禁止添加会引入 `native-tls`、`openssl` 或
`schannel` 的 crate。

**为什么在 IMAP 之外另造一套 FCP。** IMAP 无法表达可续传的游标、
服务端草稿、设备管理、分块附件传输或实时推送。第三方客户端
继续使用 IMAP 和 SMTP，并保持一等身份
（项目书 §53）；官方客户端则得到一套合用的协议。理由
与线上格式见 [fcp.md](fcp.md) §1。

**为什么用有类型的 id 而不是 `i64`。** `UserId`、`MailboxId`、`MessageId` 以及
其余的都是 `crates/ferroma-core/src/ids.rs` 里的 `#[repr(transparent)]` newtype。
它们运行时零开销，并直接映射到 `BIGINT` 列，但一个想要 `MailboxId` 的函数
不会被误传一个 `MessageId`：这类 bug 在评审中
看不见，在生产中却是灾难性的。

**为什么事件总线外面只有一把互斥锁。** 发布必须廉价，而且绝不能
阻塞在一个慢消费者上。一把短暂持有的互斥锁，用来递增一个计数器并
推入一个有界环，对单进程服务端已经足够快，而且它是让
`seq` 无空洞的最简做法，而 `seq` 无空洞正是让客户端能把 `seq`
当作重连游标使用、无需另一套排序机制的原因。

**为什么客户端只编译 `ferroma-core`。** 桌面客户端绝不能链接
PostgreSQL 驱动或 Maildir。共享 `ferroma-core` 让它得到同一个
`FerromaError`、同一套有类型的 id 和同一个 `Cursor`，而不必
把服务端的存储层一起拖上。

---

## 9. 与项目书 §37 schema 草图的两处刻意偏离

`migrations/0001_initial.sql` 以这两条说明开头。它们是与规范
schema 仅有的两处有意偏离，而且两处都是因为
§37 的草图存在某种会破坏查询的歧义。

### 9.1 `mailboxes` 是一个地址；IMAP 文件夹住在 `folders` 里

项目书 §37 定义：

```sql
CREATE TABLE mailboxes (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id),
    domain_id BIGINT NOT NULL REFERENCES domains(id),
    local_part TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(domain_id, local_part)
);
```

然后给 `messages` 一个单独的 `mailbox_id`：

```sql
CREATE TABLE messages (
    id BIGSERIAL PRIMARY KEY,
    mailbox_id BIGINT NOT NULL REFERENCES mailboxes(id),
    ...
);
```

草图把表叫作 `mailboxes`，却给了它一个*地址*的形状：
`(domain_id, local_part)` 加上 `UNIQUE(domain_id, local_part)` 就是
`alice@example.com`，而不是「Inbox 文件夹」。`messages` 上只有这样一个列
就无法表达 INBOX 与 Sent 与 Archive 的区别，一个用户的每封邮件都会
落进同一个桶，IMAP 的 `SELECT INBOX` 也就与 `SELECT Sent`
无法区分。

实际发布的 schema 把 *mailbox* 一词留给地址，另加一张表表示
文件夹：

| 表 | 含义 | 关键列 |
|---|---|---|
| `mailboxes` | 一个域名中归属于某用户的地址，即 SMTP `RCPT TO` 的目标 | `domain_id`、`local_part`、`user_id`、`is_primary`、`quota_bytes` |
| `folders` | 某个地址的一个 IMAP 文件夹 | `mailbox_id`、`name`、`parent_id`、`special_use`、`uid_validity`、`uid_next`、`highest_modseq`、`message_count`、`unseen_count`、`total_bytes` |
| `messages` | 一封已存储的邮件 | `folder_id`（**权威父级**）、`mailbox_id`（反规范化副本）、`uid`、`storage_path` |

每个按文件夹作用域的查询用的都是 `messages.folder_id`。
`messages.mailbox_id` 是 `folders.mailbox_id` 的反规范化副本，保留它是为了让
按账户作用域的查询和配额核算保持单索引快速：

* `messages_live_idx ON messages (folder_id, internal_date DESC) WHERE expunged_at IS NULL`
  服务于单个文件夹的 IMAP `SELECT` 视图。
* `messages_mailbox_date_idx ON messages (mailbox_id, internal_date DESC)`
  服务于「该地址的全部邮件」，即 Webmail 的「All mail」视图和 API 的
  `?mailbox_id=` 过滤器。

`INBOX` 对每个地址都是真实的一行 `folders`，由
`FoldersRepository::ensure_standard` 创建，它同时创建 `Sent`、`Drafts`、
`Trash`、`Junk` 和 `Archive` 以及各自的 `special_use` 标记。`INBOX` 刻意
带 `special_use = NULL`：RFC 6154 把 `\Inbox` 保留给另一个
用途，客户端必须按名字对待 `INBOX`。细节见 [imap.md](imap.md) §3。

### 9.2 `messages.rfc_message_id` 保存 RFC 5322 `Message-ID` 头字段

项目书 §37 把这个头字段列命名为 `message_id`：

```sql
CREATE TABLE messages (
    id BIGSERIAL PRIMARY KEY,
    mailbox_id BIGINT NOT NULL REFERENCES mailboxes(id),
    message_id TEXT,
    ...
```

而行自身的身份是 `id`。于是每个查询、连接和临时的 `psql` 会话
都得去消歧两个都叫「message id」的东西：API 作为 `message_id`
返回的 `BIGSERIAL` 主键，以及客户端据以串接的 RFC 5322
头字段值。在实际发布的 schema 中，这个头字段列按它的实质命名：

| 项目书 §37 | 实际发布 | 含义 |
|---|---|---|
| `messages.id` | `messages.id` | 行自身的身份；`BIGSERIAL PRIMARY KEY`；类型是 `MessageId`；由 API 作为 `message_id` 返回 |
| `messages.message_id TEXT` | `messages.rfc_message_id TEXT` | RFC 5322 `Message-ID` 头字段的值，例如 `<20260916091231.7f3a@example.net>`；类型是 `RfcMessageId`；由 API 作为 `rfc_message_id` 返回 |

两者相关但相互独立。`messages.id` 由数据库分配，
除 IMAP UID 之外，所有内部引用都用它。`rfc_message_id` 是
*发件人*选定的值，可为空（并非每封邮件都有，格式错误的会被丢弃而不是
猜测），并且只在索引
`messages_rfc_id_idx ON messages (rfc_message_id) WHERE rfc_message_id IS NOT NULL`
中唯一，刻意*不*做唯一约束，因为一封邮件被复制进多个文件夹或被投递给
多个收件人时，同一个 `Message-ID` 会合法地出现多次。
用它做查询的是 `MessagesRepository::find_by_rfc_message_id(mailbox_id, …)`。

`crates/ferroma-core/src/ids.rs` 里的 `RfcMessageId` 是 Rust 类型；它的
`RfcMessageId::generate(domain)` 构造形如
`<{timestamp}.{random:016x}.{pid:08x}@{domain}>` 的值。

`thread_id` 又是另一回事：它保存 `References` 链的根 `Message-ID`，
用于会话分组。

---

## 10. 各类关注点记录在哪里

| 关注点 | 文档 |
|---|---|
| HTTP 端点与错误信封 | [api.md](api.md) |
| FCP 线上格式、游标、实时帧、分块上传 | [fcp.md](fcp.md) |
| SMTP 命令、状态机、应答码、发信投递 | [smtp.md](smtp.md) |
| IMAP 命令、状态、UID、标志、文件夹命名 | [imap.md](imap.md) |
| Schema、索引、Maildir、配额、GC、备份 | [storage.md](storage.md) |
| 同步模型、墓碑、冲突、发件箱、失败矩阵 | [sync.md](sync.md) |
| 威胁模型、控制措施、已知缺口 | [security.md](security.md) |
| DNS、compose 文件、TLS、备份/恢复、故障排查 | [deployment.md](deployment.md) |
| 官方桌面客户端 | [client.md](client.md) |
| 每条术语及其规范写法 | [GLOSSARY.md](GLOSSARY.md) |
| 还有哪些没做 | [../../TODO_zh.md](../../TODO_zh.md) |
| 这台机器上的构建怪癖 | [../../AGENTS.md](../../AGENTS.md) |
