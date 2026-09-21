# 官方 Ferroma 客户端

**读者对象：**实现`client/`的所有人、需要判断某个功能该属于桌面客户端还是服务端
的所有人，以及所有测试多设备行为的人。

Ferroma Client 是面向 Windows、Linux 与 macOS 的官方桌面应用（Android 与 iOS
属于第二期）。它由平台无关的**共享核心**加一层薄薄的外壳组成，因为同一套账户、
同步引擎、缓存、发件箱与搜索必须在三个平台上表现一致，此后还要再加两个。本文档
规定项目书 §24 与 §25 的共享核心架构、§25 的目录职责、§26 的 SQLite 缓存模式与
`Server = Source of Truth`规则、同步引擎及其待处理操作队列、离线模式（§27）、
含八个状态的发件箱状态机（§28）、附件缓存与流式传输（§29）、本地搜索与服务端
兜底（§30）、多账户（§31）、基于`.well-known/ferroma`的自动发现（§32）、设备
管理（§33）、通知（§34，含 APNs/FCM 预留）、设置界面（§52）以及 UI 方案（§51）。

> **状态：**设计规范。`ferroma-client` crate 仍是骨架：`client/src/lib.rs`只声明了
> 模块清单，`client/src/main.rs`是个占位实现，而`Cargo.toml`已经引入了设计所需的
> 依赖（带`sqlite` feature 的`sqlx`、`reqwest`、`tokio-tungstenite`、`clap`、
> `dirs`）。**因此下面每个模块都标注了_（计划中）_**，§3 中的 SQLite 模式是一份
> 规范，不是已有的迁移。它使用的协议冻结在[fcp.md](fcp.md)与[api.md](api.md)，而
> 契约的服务端只在`ferroma-api`已实现的范围内实现了这两者，而它同样尚未实现。

---

## 1. 范围与同 IMAP 的关系

项目书 §53 划定了这条线，[fcp.md](fcp.md) §1 重复了一遍：

```text
                    Ferroma
                       │
          ┌────────────┼────────────┐
          ▼            ▼            ▼
      Client API      IMAP         SMTP
          │            │            │
          ▼            ▼            ▼
     官方客户端    Thunderbird   Outlook
                   Apple Mail
```

| 客户端 | 协议 | 原因 |
|---|---|---|
| **官方 Ferroma 客户端** | 基于 HTTPS + WebSocket 的 FCP | 需要带游标的增量同步、服务端草稿、设备管理、分块附件与推送，这些 IMAP 一概无法表达 |
| Thunderbird、Apple Mail、Outlook、iPhone Mail、Android | IMAP4rev1 + SMTP | 它们已经存在、能正常工作，Ferroma 不能把它们弄坏（[imap.md](imap.md) §11） |

官方客户端**从不使用 IMAP 或 SMTP**。它没有 IMAP 解析器，不打开 143 端口，也不
实现 SMTP 提交。一切都经`/api/v1/client`。

这是一次刻意的收窄：它意味着桌面客户端只需要一种传输、一套认证方案、一套错误词汇
和一种同步模型，也意味着 IMAP 层里的服务端缺陷影响不到它。代价是 FCP 接口必须
完整：只存在于 IMAP 之上的功能，官方客户端就不可能拥有。

---

## 2. 架构：共享核心 + UI

项目书 §24.2：

```text
                    Ferroma Client
                          │
             ┌────────────┴────────────┐
             │                         │
        Shared Core                    UI
             │                         │
      ┌──────┼──────┐          ┌───────┼───────┐
      ▼      ▼      ▼          ▼       ▼       ▼
    Sync    API     DB        Windows  Linux   macOS
```

值得专门写下来的那条规则：**UI 是核心的消费者，绝不参与核心的内部。** UI 不打开
套接字、不持有游标，也不写缓存。它调用核心，并对核心发出的事件作出反应。

```text
   ┌──────────────────────────── UI shell (per platform) ─────────────────────┐
   │  window, folder pane, list, reader, composer, notifications, settings    │
   └────────────────────────────────┬─────────────────────────────────────────┘
                                    │  core API + a stream of state changes
   ┌────────────────────────────────▼─────────────────────────────────────────┐
   │                        ferroma-client core (Rust)                        │
   │                                                                          │
   │   account ── api ── sync ── database ── mail ── draft ── outbox          │
   │                    │                     │                               │
   │                    └── attachment ── search ── notification ── device    │
   │                                                                          │
   │             settings (local + server-mirrored)                           │
   └────────────────────────────────┬─────────────────────────────────────────┘
                                    │
                       ┌────────────┴────────────┐
                       ▼                         ▼
                SQLite cache               HTTPS + WebSocket
                       │                         │
                       └────────────┬────────────┘
                                    ▼
                          Ferroma Server (FCP)
```

为什么用 Rust 核心，而不是每个平台各写一套原生代码：

| 性质 | 后果 |
|---|---|
| 同步、缓存与发件箱只有一份实现 | 客户端里最难的逻辑只写一次、只测一次，而不是三次 |
| 各处 SQLite 模式相同 | 缓存里的 bug 只有一个，迁移路径也只有一条 |
| 各处的 FCP 客户端相同 | 协议变更只改一处 |
| 平台 UI 可以自由选择 | Windows、Linux 与 macOS 各得原生外壳；增加移动端外壳不必碰核心 |
| `ferroma-core`与服务端共用 | `FerromaError`、类型化 id、`Cursor`与地址解析就是同一批类型（[architecture.md](architecture.md) §2） |

核心是一个库（`client/src/lib.rs`）；平台外壳是链接它的独立二进制或独立进程。
项目书 §6.3 的建议是：若项目偏重 Rust 原生客户端就用 **Slint**，否则用
**Tauri + Web UI**。这个选择属于 UI 而不属于核心，核心绝不能依赖它。注意当前的
清单：`client/Cargo.toml`只依赖`ferroma-core`，不依赖`ferroma-storage`，也不依赖
`sqlx`的 PostgreSQL feature。桌面客户端绝不能链接 PostgreSQL 驱动或 Maildir。

**状态：核心已经建成，外壳还没有。** §2 以及 §3 到 §13 的全部内容都已交付并通过
测试：330 个测试函数覆盖同步、SQLite 缓存、发件箱、离线队列、本地搜索、附件、
多账户、自动发现与设置，由真实 CLI（`ferroma-client`）驱动，并在验收运行中对着
真实服务端跑。不存在的只有三栏窗口（§14）。它将要建立其上的接缝是`ui.rs`：
`ClientHandle`（`open`、`folders`、`messages`、`open_message`、`send`、`sync`、
`search`、`outbox_counts`、`load_settings`/`save_settings`），外加一个
`subscribe()`对`ClientEvent`的广播，用于同步进度与新邮件，外壳驱动这些，此外什么
都不拥有。

两条候选工具链都在把这项工作推迟之前**在开发主机上实测通过编译**，因此可以按优劣
来做选择，而不是按机器支持什么来做选择：

| 工具链 | 探测结果 |
|---|---|
| **Tauri 2.11**（`tauri` + `tauri-build`，WebView2 后端） | `cargo check`约 2 分 25 秒通过；`wry`、`tao`、`webview2-com`均可构建 |
| **Slint 1.18**（`slint`，软件渲染器与 femtovg 渲染器） | `cargo check`约 2 分 18 秒通过；不需要系统 webview |

Windows 工具链具备所需的 C++ 生成工具与 Windows SDK，本机操作系统也有 WebView2，
所以 Tauri 可行；但 Slint 完全不依赖系统 webview，这是 §6.3 更倾向的、更 Rust
原生的答案。

---

## 3. SQLite 缓存（§26）

### 3.1 表

项目书 §26 列出了这些表。每张表在服务端都有对应物，而客户端这一份是*缓存*，这一点
决定了哪些列存在、哪些不存在：

| 表 | 服务端对应物 | 是否缓存？ |
|---|---|---|
| `accounts` | —（客户端独有） | 本机已配置账户的列表；服务端没有对应物，因为它就是服务端 |
| `mailboxes` | `mailboxes` + `folders` | 已扁平化：客户端希望「文件夹 id → 名称、计数、uid_validity」在一行里 |
| `messages` | `messages` | 仅元数据：id、flags、subject、sender、日期、大小、`has_attachments` |
| `message_headers` | `message_recipients` + 解析出的头部 | 供阅读器使用的完整头字段列表，按需获取 |
| `attachments` | `attachments` | 元数据，加上字节下载完成后的本地缓存路径 |
| `drafts` | `drafts` | 完整内容：草稿是唯一必须挺过离线的东西 |
| `outbox` | `mail_queue`（只读镜像） | 待处理操作与待发送项，各带其`operation_id` |
| `sync_state` | `client_sync_states` | 每账户/邮箱/文件夹的游标 |
| `devices` | `devices` | 供设置界面使用的设备列表 |
| `settings` | `settings` | 本地 UI 设置，外加服务端取值的一份镜像 |

另有一张表是项目书 §26 没有列出、但设计需要的：

| 表 | 用途 |
|---|---|
| `search_index` | §30 的本地搜索索引；一张覆盖 subject、sender、recipients 与正文文本的 FTS5 虚拟表 |

### 3.2 一份具体模式

_（计划中）_，这是缓存应有的形态，好让同步引擎的操作都是单行 upsert，UI 的查询
都走索引。

```sql
-- 每个已配置账户一行。
CREATE TABLE accounts (
    id              INTEGER PRIMARY KEY,
    server_id       INTEGER NOT NULL,              -- users.id on the server
    email           TEXT    NOT NULL UNIQUE,
    display_name    TEXT,
    api_base        TEXT    NOT NULL,              -- https://mail.example.com/api/v1
    access_token    TEXT,                          -- 见 §3.4
    refresh_token   TEXT,
    token_expires_at INTEGER,
    device_uid      TEXT    NOT NULL,              -- 每次安装保持稳定
    device_id       INTEGER,                       -- 服务端 devices.id
    paused          INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    last_sync_at    INTEGER
);

-- 每个账户的每个文件夹一行。镜像 folders 以及来自
-- GET /api/v1/client/mailboxes 的计数。
CREATE TABLE mailboxes (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    server_id       INTEGER NOT NULL,              -- folders.id
    address_id      INTEGER NOT NULL,              -- 服务端的 mailboxes.id
    name            TEXT    NOT NULL,              -- "INBOX", "Archive/2026"
    special_use     TEXT,                          -- "\Sent", "\Drafts", …
    message_count   INTEGER NOT NULL DEFAULT 0,
    unseen_count    INTEGER NOT NULL DEFAULT 0,
    uid_validity    INTEGER NOT NULL DEFAULT 0,
    uid_next        INTEGER NOT NULL DEFAULT 0,
    subscribed      INTEGER NOT NULL DEFAULT 1,
    UNIQUE (account_id, server_id)
);

-- 邮件元数据。不是正文：正文按需获取（§5）。
CREATE TABLE messages (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    server_id       INTEGER NOT NULL,              -- messages.id，即 API 的 message_id
    folder_id       INTEGER NOT NULL,              -- 上面的 mailboxes.id
    uid             INTEGER NOT NULL,
    uid_validity    INTEGER NOT NULL,              -- uid 所属的代次
    rfc_message_id  TEXT,
    thread_id       TEXT,
    subject         TEXT,
    sender          TEXT,
    sender_name     TEXT,
    snippet         TEXT,
    flags           TEXT NOT NULL DEFAULT '',      -- "seen,flagged"，Flags::to_db_string 形式
    size_bytes      INTEGER NOT NULL DEFAULT 0,
    has_attachments INTEGER NOT NULL DEFAULT 0,
    attachment_count INTEGER NOT NULL DEFAULT 0,
    internal_date   INTEGER NOT NULL,
    sent_at         INTEGER,
    body_cached     INTEGER NOT NULL DEFAULT 0,
    UNIQUE (account_id, server_id)
);

CREATE INDEX messages_folder_date_idx ON messages (folder_id, internal_date DESC);
CREATE INDEX messages_account_idx     ON messages (account_id, internal_date DESC);
CREATE INDEX messages_rfc_id_idx      ON messages (account_id, rfc_message_id)
    WHERE rfc_message_id IS NOT NULL;
CREATE UNIQUE INDEX messages_folder_uid_key ON messages (folder_id, uid);

-- 供阅读器使用的完整头字段列表。随正文一起获取，不随列表获取。
CREATE TABLE message_headers (
    id          INTEGER PRIMARY KEY,
    message_id  INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL DEFAULT 0,
    name        TEXT    NOT NULL,
    value       TEXT    NOT NULL
);
CREATE INDEX message_headers_message_idx ON message_headers (message_id, ordinal);

-- 附件元数据，加上本地缓存位置。
CREATE TABLE attachments (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    server_id       INTEGER NOT NULL,              -- attachments.id
    message_id      INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    filename        TEXT,
    content_type    TEXT,
    size_bytes      INTEGER NOT NULL DEFAULT 0,
    sha256          TEXT,                          -- 服务端的内容寻址地址
    content_id      TEXT,
    is_inline       INTEGER NOT NULL DEFAULT 0,
    cached_path     TEXT,                          -- NULL = 尚未下载
    cached_bytes    INTEGER NOT NULL DEFAULT 0,
    cached_at       INTEGER
);
CREATE INDEX attachments_message_idx ON attachments (message_id);
CREATE INDEX attachments_sha_idx     ON attachments (account_id, sha256);

-- 草稿：完整内容，因为草稿必须挺过离线。
CREATE TABLE drafts (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    server_id       INTEGER,                       -- 首次同步前为 NULL
    subject         TEXT,
    body_text       TEXT,
    body_html       TEXT,
    recipients_json TEXT NOT NULL DEFAULT '[]',
    attachments_json TEXT NOT NULL DEFAULT '[]',
    in_reply_to     TEXT,
    references_json TEXT NOT NULL DEFAULT '[]',
    updated_at      INTEGER NOT NULL,
    server_updated_at INTEGER,
    dirty           INTEGER NOT NULL DEFAULT 0    -- 1 = 本地编辑尚未推送
);

-- 发件箱：待处理操作与待发送项（§6、§8）。
CREATE TABLE outbox (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    operation_id    TEXT    NOT NULL UNIQUE,       -- 用户操作时生成
    kind            TEXT    NOT NULL,              -- send | mark_read | move | flag | delete | draft_save …
    payload_json    TEXT    NOT NULL,
    state           TEXT    NOT NULL DEFAULT 'pending',
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    next_attempt_at INTEGER,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
CREATE INDEX outbox_due_idx ON outbox (account_id, next_attempt_at)
    WHERE state IN ('pending', 'retrying');

-- 游标。每账户/邮箱/文件夹一行；folder_id = 0 是账户级，
-- 镜像 client_sync_states 的 COALESCE(folder_id, 0)。
CREATE TABLE sync_state (
    id          INTEGER PRIMARY KEY,
    account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    mailbox_id  INTEGER NOT NULL,
    folder_id   INTEGER NOT NULL DEFAULT 0,
    cursor      INTEGER NOT NULL DEFAULT 0,
    last_full_sync_at INTEGER,
    UNIQUE (account_id, mailbox_id, folder_id)
);

CREATE TABLE devices (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    server_id       INTEGER NOT NULL,
    device_uid      TEXT    NOT NULL,
    name            TEXT,
    platform        TEXT,
    client_version  TEXT,
    last_seen_at    INTEGER,
    revoked         INTEGER NOT NULL DEFAULT 0,
    UNIQUE (account_id, server_id)
);

CREATE TABLE settings (
    account_id  INTEGER NOT NULL DEFAULT 0,         -- 0 = 应用级
    key         TEXT    NOT NULL,
    value       TEXT    NOT NULL,
    PRIMARY KEY (account_id, key)
);

-- 本地搜索（§9）。对用户会搜索的字段建 FTS5。
CREATE VIRTUAL TABLE search_index USING fts5(
    subject, sender, recipients, body,
    content='',                                     -- 无内容：文本由我们自己保存
    tokenize='unicode61'
);
```

### 3.3 `Server = Source of Truth`

项目书 §26 与 §55，在[sync.md](sync.md) §2 中进一步展开。把它应用到上面的模式上，
得到四条规则：

1. **每一行都是服务端所拥有数据的缓存。** `server_id`才是身份；本地的`id`是实现
   细节，绝不能当成服务端 id 发给服务端。
2. **`sync_state.cursor`是客户端唯一权威拥有的东西。** 它记录*本次安装*读到了
   哪里，对其他任何设备都没有意义。
3. **`outbox`行是意图，不是事实。** 在服务端确认某个操作之前，这一行只是一个
   愿望。UI 可以展示乐观结果，但绝不能把它呈现为已确认。
4. **`drafts.dirty`是唯一能在冲突中胜出的本地写入**，见 §7。

实际含义是：客户端总能从服务端重建整个数据库。不存在丢失后不可接受的客户端数据，
只有两个例外：游标，丢了要重新同步；发件箱，丢了就丢了用户敲进去的内容。这就是
`outbox`是最受悉心对待的表的原因。

### 3.4 令牌存储

项目书 §38 把「Token 安全存储」（secure token storage）列入客户端层的控制项。规则
如下：

| 平台 | 位置 |
|---|---|
| Windows | 凭据管理器（`windows-credentials` / DPAPI） |
| macOS | 钥匙串（`security`框架） |
| Linux | 通过平台的 keyring crate 使用 Secret Service（libsecret / gnome-keyring） |

**不放在 SQLite 里明文保存，也不放在配置文件中。** 上面的`accounts`表有
`access_token` / `refresh_token`列，是因为设计需要一个引用它们的位置；在有 keyring
的平台上，它们保存的是一个不透明句柄；在没有 keyring 的平台上，设计必须拒绝保存
刷新令牌，而不是未加保护地写进磁盘。

`device_uid`不是密钥。它是本次安装的稳定标识，生成一次后一直保留，正是它让设备
吊销能对准正确的安装（[fcp.md](fcp.md) §9）。

---

## 4. 目录职责（§25）

项目书 §25 给出了目录清单与各核心职责。

| 模块 | 职责 | 绝不能做的事 |
|---|---|---|
| `app/` | 进程生命周期：单实例锁、启动、关闭，以及把核心与 UI 串起来的顶层事件循环 | 包含业务逻辑 |
| `account/` | 添加/移除/暂停/重新认证账户；持有每账户的核心上下文 | 直接讲 HTTP，那是`api/`的职责 |
| `api/` | FCP HTTP 客户端：请求构造、`X-Ferroma-*`头字段、`401`时的令牌刷新、WebSocket 连接与重连、重试策略 | 知道邮件是什么 |
| `sync/` | 同步引擎：读取`sync_state.cursor`、分页`GET /client/sync`、在事务中应用变更、推进游标；检测空洞与`uid_validity`变化 | 把协议负载解析成本地模型以外的任何东西 |
| `mail/` | 邮件模型与对已缓存邮件的操作：列表查询、标志变更、移动、会话聚合 | 实现同步或 HTTP |
| `draft/` | 草稿生命周期：创建、编辑、自动保存、删除；镜像到服务端与 Drafts 文件夹 | 发送 |
| `outbox/` | 发件箱状态机（§8）：入队、尝试、退避、重试、失败，并与服务端的`mail_queue`状态对账 | 上传附件，那是`attachment/`的职责 |
| `attachment/` | 内容寻址的本地缓存、带进度的流式下载、分块可续传上传（§7） | 决定何时需要某个文件，那是`mail/`或 UI 的职责 |
| `search/` | 本地 FTS 索引与查询解析；服务端兜底（§9） | 成为唯一的搜索路径，兜底存在就是为此 |
| `notification/` | 桌面 toast 与角标计数；APNs/FCM 预留（§11） | 渲染邮件 |
| `database/` | SQLite 连接池、迁移与类型化访问器 | 包含本属于功能模块的查询 |
| `device/` | 设备列表、吊销，以及「退出此设备」（§12） | — |
| `settings/` | §13 的设置界面，读写`settings` | — |
| `ui/` | 平台外壳（§14）：窗口、面板、撰写器、对话框 | 打开套接字、持有游标，或直接写缓存 |

由这张表得出两条不变式：

* **`api/`是唯一知道 HTTP 的模块。** 其他一切都调用它上面的类型化函数。这正是
  重试策略、令牌刷新与版本头字段只存在于一处的原因。
* **`sync/`是唯一推进游标的模块。** 如果任何其他模块能写`sync_state.cursor`，就
  可能跳过一页，而且这种丢失是无声的。

---

## 5. 同步引擎

机制与理由见[sync.md](sync.md)；这里讲客户端这一侧的形状。

```text
   ┌────────────────────── sync engine (one task per account) ─────────────────┐
   │                                                                           │
   │   loop:                                                                   │
   │     for each folder of the account:                                       │
   │        cursor = SELECT cursor FROM sync_state WHERE …                     │
   │        page   = api.get_sync(mailbox_id, folder_id, cursor, limit)        │
   │        BEGIN;                                                             │
   │          for change in page.changes: apply(change)     # idempotent       │
   │          UPDATE sync_state SET cursor = page.next_cursor                  │
   │        COMMIT;                                                            │
   │     until not page.has_more                                               │
   │                                                                           │
   │   then: drain the outbox (§8)                                             │
   │   then: wait for a WebSocket frame, a timer, or a UI request to sync      │
   └───────────────────────────────────────────────────────────────────────────┘
```

客户端义务，照抄自[sync.md](sync.md) §4，因为它们很容易做错：

| 义务 | 被忽略时的后果 |
|---|---|
| 在一个事务里按顺序应用每条变更，然后存入游标 | 崩溃后会重新拉取那一页；这无害，*正因为*处理器是幂等的 |
| 每个处理器都是幂等的（`INSERT … ON CONFLICT`、标志用绝对值而不是切换） | 重放一页会产生错误状态 |
| 一直分页直到`has_more`为 false | 大账户只同步了一部分，看起来像被截断 |
| 检测`seq`空洞并从`0`重新同步 | 无声的永久数据丢失 |
| 检测`uid_validity`变化并丢弃该文件夹的 UID 索引 | 服务端重建后会打开错误的邮件 |
| 绝不把游标推进到某条应用失败的变更之后 | 那条变更再也不会被投递 |
| 遇到`/sync`返回`409 conflict`时，丢弃该文件夹缓存并从`0`同步 | 该文件夹永远无法恢复 |

**邮件正文不在变更流里。** `message_created`只带 id 与 flags（[fcp.md](fcp.md) §3
第 4 项）；正文按需用`GET /api/v1/client/messages/:id`或`…/raw`获取。正是这一点让
一个 5 万封邮件邮箱的首次同步成为元数据操作，而不是下载若干 GB。

**首次同步会显示进度**，办法是把已应用的变更数与`GET /api/v1/client/mailboxes`
返回的文件夹`message_count`相比较；这是 UI 渲染出有意义进度条的唯一办法，因为
服务端不会事先报告变更总数。

### 5.1 待处理操作

离线操作不在同步引擎里排队；它们进入`outbox`，同步引擎在每次成功同步之后将其排空
（[sync.md](sync.md) §10）。这个分离很重要：

* **同步引擎**消费服务端真相。
* **发件箱**持有客户端意图。

把两者混在同一个队列里，会让某一条目到底是「服务端告诉我的」还是「我想让服务端
做的」变得含糊，而这两者的重试策略正好相反。

---

## 6. 离线模式（§27）

项目书 §27 列出了离线可用的功能：

```text
查看已同步邮件       view synced mail
本地搜索             local search
查看已缓存附件       view cached attachments
写邮件               compose
保存草稿             save a draft
回复                 reply
删除                 delete
标记已读             mark read
```

| 能力 | 离线行为 | 重连之后 |
|---|---|---|
| 查看已同步邮件 | 完全由`messages` + `message_headers` + `body_cached`提供 | 无事可做 |
| 查看未缓存的邮件 | 「此邮件离线不可用」，客户端必须这么说，而不是显示一个空的阅读器 | 按需获取 |
| 本地搜索 | `search_index`，不走服务端 | 不变 |
| 查看已缓存附件 | 来自`attachment.cached_path` | 不变 |
| 查看未缓存附件 | 不提供；下载进入队列 | 下载 |
| 撰写/回复 | 全本地；附件引用本地文件 | 由发件箱发送 |
| 保存草稿 | 写入`drafts`且`dirty = 1` | 推送，并适用 §7 的冲突策略 |
| 删除/标记已读/加标志/移动 | 乐观地应用到缓存，每个动作一行`outbox`记录，带其`operation_id` | 在服务端应用；相应变更返回并确认它 |
| 发送 | 该邮件是`outbox`中的一行`send`；它出现在发件箱视图，而不是已发送 | 上传、接受、入队 |

不可协商的规则：

* **乐观的本地变更标记为未确认。** UI 必须能显示「回到在线后会把它发出去」；用户
  以为删除已经发生、后来却发现邮件还在，那是 bug 报告，不是体验上的小瑕疵。
* **用户敲进去的东西永远不会丢。** 草稿、发件箱行与附件引用都留在 SQLite 里，直到
  服务端确认它们（[fcp.md](fcp.md) §11）。
* **已缓存的正文能挺过失败的发送。** 因`413`失败的发送会保留草稿与附件引用，好让
  用户把附件改小后重试。
* **缓存不是真相。** 本机离线期间在服务端被删除的邮件，一旦墓碑被应用就在本地删除，
  即使正文已经缓存。已缓存的正文从来不是保留一封服务端说已经不存在的邮件的理由。

---

## 7. 冲突

完整论述见[sync.md](sync.md) §9。客户端这一侧的看法：

| 实体 | 策略 | 客户端做什么 |
|---|---|---|
| 标志 | 后写者胜 | 应用传入的`message_updated`，若自己的待处理标志变更已被确认则丢弃它 |
| 文件夹归属 | 后写者胜 | 双向应用`message_moved` |
| 邮件是否存在 | 服务端绝对优先 | 墓碑即删除，即使正文已经缓存 |
| 文件夹列表 | 服务端优先 | 把`folder_created` / `folder_deleted`应用到缓存 |
| **草稿内容** | 后写者胜，**并且告知客户端** | 收到`conflict.detected`时，把本地版本保留为副本，或者提示用户；绝不静默重试 |
| 本地设置 | 从不同步 | 窗口大小、主题、缓存预算都留在本地 |
| 服务端设置 | 服务端优先 | 从`GET /client/account`重新同步 |

草稿这一种是用户唯一可能丢失已输入文字的场合，这也是[fcp.md](fcp.md) §7 规定服务端
要报告它覆盖了什么的原因：

```json
{ "id": 44, "updated_at": "2026-09-16T12:00:01Z",
  "conflict": { "detected": true, "server_updated_at": "2026-09-16T11:59:58Z" } }
```

忽略该字段的客户端，正在静默丢弃用户写的一段话。最低可接受的行为是把落败的版本
另存为一份本地草稿，并告诉用户发生了这件事；更好的行为是把两者都显示出来，让用户
自己选。

---

## 8. 发件箱（§28）

项目书 §28 给出了流程与八个状态。

```text
Compose
   ↓
Local Outbox
   ↓
Uploading
   ↓
Server
   ↓
Mail Queue
   ↓
SMTP Delivery
```

### 8.1 八个状态

```text
Draft   Pending   Uploading   Queued   Sending   Sent   Failed   Retrying
```

| 状态 | 由谁设置 | 含义 | 转移到 |
|---|---|---|---|
| `Draft` | 撰写器 | 已撰写，尚未入队发送 | `Pending`（用户按下发送），或者草稿被删除 |
| `Pending` | 用户 | 已在本地排队，等待同步引擎 | `Uploading`、`Failed` |
| `Uploading` | 发件箱 | 附件正在传输（分块、可续传） | `Queued`、`Retrying`、`Failed` |
| `Queued` | 服务端对`POST /client/messages`返回的`202`/`200` | 已接受：每个收件人一行`mail_queue` | `Sending`、`Retrying`、`Failed` |
| `Sending` | 服务端状态 | 服务端正在投递给远端 MX（`mail_queue.status = 'delivering'`） | `Sent`、`Retrying`、`Failed` |
| `Sent` | 服务端状态 | 每个收件人都已投递（`delivered`） | 终态 |
| `Retrying` | 服务端状态 | 暂时性失败；服务端会在`next_attempt_at`再试 | `Sending`、`Failed` |
| `Failed` | 服务端状态，或本地校验 | 永久失败或尝试次数耗尽（`failed`），或者请求被拒（`413`、`403`） | 终态（用户可以编辑后重发，这会创建新的发件箱行） |

前四个由客户端驱动，后四个由服务端驱动。客户端通过应用同步里的`delivery.updated`
变更，**以及在套接字可用时的相应 WebSocket 帧**，达到`Sent`、`Retrying`与
`Failed`。因为套接字只是优化、游标才是真相来源（[fcp.md](fcp.md) §8），只听套接字
的客户端会让邮件永远卡在`Sending`。两条路径必须喂给同一个状态机。

### 8.2 转移规则

```text
  Draft ──send──► Pending ──pick──► Uploading ──accepted──► Queued
                     │                  │                     │
                     │ reject           │ reject              │ server
                     ▼                  ▼                     ▼
                  Failed            Retrying ◄──────────── Sending
                                       │                     │
                                       └──attempt──────────► │
                                                             ▼
                                                           Sent
                                       │
                                       └──exhausted──► Failed
```

| 规则 | 理由 |
|---|---|
| `operation_id`在行创建时生成，绝不在发送时生成 | 重试必须出示同一个 id，否则它什么也保护不了（[sync.md](sync.md) §8.1） |
| 附件在发送**之前**上传 | 引用服务端不存在的附件 id 的`send`会失败 |
| 已上传的附件 id 存在发件箱行里 | 重启后接着传，而不是重新上传（[fcp.md](fcp.md) §6） |
| `429`以外的`4xx`对该行是终态 | `413 limit_exceeded`不会变得不再成立 |
| `429`严格遵守`Retry-After` | 服务端最清楚自己的限额 |
| `5xx`与网络错误按指数退避并加抖动 | 而且绝不丢弃该行 |
| 超时永远不算确认 | 请求可能已经成功；重试带同一个`operation_id`，会拿到缓存的响应 |
| 只有在确认`Sent`之后，或者用户删除时，才移除该行 | 「大概发出去了」不能变成「没了」 |

### 8.3 发件箱视图

项目书 §28 展示了用户看到的内容：

```text
发件箱

正在发送    2
发送失败    1
已发送    152
```

它对应`Sending`/`Uploading`、`Failed`与`Sent`。`Failed`行必须可操作：显示原因
（`last_error`，有服务端增强状态码时一并显示），提供「编辑并重发」，并且绝不静默
过期。用户以为已经发出、却无声消失的邮件，是这个客户端可能有的最严重故障。

---

## 9. 附件（§29）

项目书 §29 列出了这些能力：

```text
流式上传       streaming upload
分块上传       chunked upload
断点续传       resumable transfer
流式下载       streaming download
下载进度       download progress
本地缓存       local cache
缓存清理       cache eviction
文件大小限制   size limit
MIME 类型      MIME type
文件校验       integrity check
```

### 9.1 上传

小文件走简单端点；大文件与可续传传输走分块端点。两者都在[fcp.md](fcp.md) §6 中
规定。

| 路径 | 何时使用 | 端点 |
|---|---|---|
| 简单 | 文件 ≤ `client.attachment_chunk_size`（默认 1 MiB，如`GET /client/account`的`limits`所报告） | `POST /api/v1/client/attachments`（multipart） |
| 分块 | 更大的文件，或者续传一次被中断的上传 | `POST …/attachments/init` → `PUT …/attachments/:id/chunk?index=N` → `POST …/attachments/:id/complete` |

分块路径的客户端义务：

* **分块大小来自服务端**，在`init`响应里（`chunk_size`），不是客户端里的一个常量。
  服务端可以更改`client.attachment_chunk_size`。
* **除最后一块外，每一块都恰好是`chunk_size`。** 服务端会强制执行。
* **分块可以重试，也可以乱序到达。** 服务端维护一个位图。
* **续传靠询问。** `GET …/attachments/:id/status`报告服务端持有哪些分块，于是崩溃过
  的客户端只上传缺口。靠猜要付出整份重传的代价，更糟的是可能造出一个客户端自以为
  补上了、实际存在的空洞。
* **`complete`必须发送 SHA-256。** 不匹配就是`409 conflict`，该上传被丢弃：客户端
  应当重新上传，而不是用同一个摘要重试`complete`。

附件在服务端是内容寻址的，所以共享同一个文件的两封邮件共享同一个二进制对象
（[storage.md](storage.md) §6）。客户端可以在本地沿用同样的思路：按`sha256`缓存，
于是附在三份草稿上的同一个 PDF 在磁盘上只存一份。

### 9.2 下载与缓存

| 方面 | 行为 |
|---|---|
| 流式 | `GET /api/v1/client/attachments/:id`是流式；支持`Range`，所以中断的下载可以续传 |
| 校验 | `ETag`就是该二进制对象的 SHA-256（[fcp.md](fcp.md) §6）；客户端校验内容，不匹配就重新下载 |
| 进度 | 来自`Content-Length`与已接收的字节数；`Range`续传从已知偏移开始 |
| 缓存键 | `(account_id, sha256)`，所以相同的附件在本地自动去重 |
| `cached_path` | 客户端数据目录下的文件，除非用户导出过，否则绝不放用户的下载文件夹 |
| 淘汰 | 按`cached_at`做 LRU，受最大缓存大小设置约束（§13）。绝不淘汰`dirty`草稿的附件，也绝不淘汰发件箱行的附件 |
| 离线 | 已缓存的附件可以打开；未缓存的提供「在线时下载」 |
| 内联图片 | 同受缓存约束，也同受[security.md](security.md) §10.2 的「阻止远程内容」规则约束：带远程 URL 的内联部分不会被获取 |

大小限制是`limits.max_attachment_size`（25 MiB）与每封邮件的
`limits.max_attachments`（50），两者都由服务端在`GET /client/account`的 limits 块
中报告。撰写器必须在本地、在上传之前就拒绝，好让用户在还在写这封邮件的时候就发现
问题。

---

## 10. 本地搜索（§30）

项目书 §30：

```text
from:alice@example.com
subject:invoice
attachment:pdf
after:2026-01-01
```

策略：

```text
先搜索本地缓存      search the local cache first
       ↓
没有结果            no results
       ↓
请求服务器搜索      ask the server
```

### 10.1 运算符集合

客户端与服务端必须就查询语言达成一致，否则同一个查询会因为缓存当时有没有答案而
返回不同结果。服务端的集合冻结在[fcp.md](fcp.md) §10：`from:`、`to:`、`subject:`、
`body:`、`has:attachment`、`is:unread`、`is:flagged`、`before:`、`after:`、
`folder:`。

| 运算符 | 本地支持 | 说明 |
|---|---|---|
| `from:` | 是 | `messages.sender` |
| `to:` | 是 | 需要收件人进索引；缓存为搜索保存了它们 |
| `subject:` | 是 | FTS 列 |
| `body:` | 仅对已缓存的正文为是 | 正文未缓存的邮件在本地无法匹配，这正是兜底存在的主要原因 |
| `has:attachment` | 是 | `messages.has_attachments` |
| `is:unread` | 是 | `flags`不含`seen` |
| `is:flagged` | 是 | `flags`含`flagged` |
| `before:` / `after:` | 是 | `internal_date` |
| `folder:` | 是 | `folder_id` |

### 10.2 兜底

```text
   user types a query
        │
        ▼
   parse locally (reject an unknown operator with a clear message)
        │
        ▼
   search_index over the cached rows
        │
        ├── results ──► show them, marked "from this device's cache"
        │
        └── no results ──► GET /api/v1/client/search?q=…&mailbox_id=…&limit=50
                          │
                          ├── results ──► show them, marked "from the server"
                          │               (and offer to cache the bodies)
                          └── offline ──► say so explicitly
```

让兜底保持诚实的两条规则：

* **绝不把本地结果集呈现为完整的。** 本地搜索没匹配到，可能是因为正文没有缓存。
  UI 必须区分「没有结果」与「本地缓存中没有结果」，否则用户会断定这封邮件不存在。
* **离线时绝不静默兜底。** 正确的提示是「当前处于离线状态；只显示本机的结果」。

既然服务端能搜索，为什么还要本地优先？因为本地搜索离线可用，也因为对已经同步过的
邮箱它是即时的。服务端搜索存在的意义，是覆盖本地搜索服务不了的情况：从未下载过的
正文，以及已同步窗口之外的邮件。

客户端的索引是无内容的 FTS5，所以客户端还必须同时维护源文本（subject、sender、
recipients、已缓存的正文）。在正文被获取时把它加进索引，在正文缓存被淘汰时把它移出
索引，这正是让索引与用户离线时实际能读到的东西保持一致的办法。

---

## 11. 多账户（§31）

项目书 §31：

```text
Accounts
├── Personal   → alice@example.com
├── Work       → alice@company.com
└── Other      → test@example.org
```

| 能力 | 实现 |
|---|---|
| 添加账户 | `account/`，由自动发现驱动（§12）；一行`accounts`记录 |
| 移除账户 | 删除该行；级联删除它的邮件、草稿、发件箱行与已缓存附件。先确认：此操作不可逆，而缓存里可能存着某次`failed`发送的唯一副本 |
| 暂停同步 | `accounts.paused = 1`；同步任务停止分页。**发件箱仍在排空**，因为用户暂停同步并不意味着「别把我已经让你发的信发出去」 |
| 重新认证 | 熬过一次刷新仍然存在的`401`；提示输入密码，并保留缓存与发件箱 |
| 编辑账户 | 显示名称、服务端 URL、设备名称 |
| 查看同步状态 | 每个文件夹的`sync_state.last_full_sync_at`，加上发件箱计数 |
| 每账户隔离 | 每张表都带`account_id`，每个查询都按它过滤。一个账户绝不能看到另一个账户的邮件 |

多账户模型强制带来的两条设计规则：

* **每账户一个同步任务**，而不是一个全局任务。账户可能位于可用性不同的服务端上，
  慢的或者连不上的那一个绝不能拖住其他账户。
* **UI 以账户为作用域，并提供统一视图。** 统一收件箱是一次跨账户查询
  （`messages WHERE account_id IN (…) ORDER BY internal_date DESC`），而不是一张
  合并表。合并会让每个按账户进行的操作（删除、移动、加标志）都变得含糊。

---

## 12. 自动发现（§32）

项目书 §32：`https://example.com/.well-known/ferroma`。响应结构冻结在
[api.md](api.md) §2：

```json
{
  "api": "https://mail.example.com/api/v1",
  "imap": { "host": "mail.example.com", "port": 993, "tls": true },
  "smtp": { "host": "mail.example.com", "port": 587, "tls": true },
  "web": "https://mail.example.com",
  "protocol_version": 1
}
```

用户输入一个地址时客户端的流程：

```text
   user types alice@example.com
        │
        ▼
   1. GET https://example.com/.well-known/ferroma        （只看地址里的域名，
        │                                                  端口固定 443，只用 HTTPS）
        ├── 200 + JSON ──► 用 `api` 当 FCP 基址，记录 `protocol_version`
        │
        ├── 404 ──► Discovery::guessed：按惯例主机名给出
        │           api  = https://mail.example.com/api/v1
        │           imap = imap.example.com:993 (tls)
        │           smtp = mail.example.com:587 (tls)
        │           并标记为「猜测」，由界面请用户确认——不会拿它当发现结果。
        │
        └── 其他（5xx、TLS/传输失败、内容不是 JSON、缺少可用的 `api`）
                    ──► 报错，绝不猜测：猜错就等于把账号指向别人的服务器。
                        此时由手工面板接手（`Discovery::candidate_hosts` 会依次给出
                        mail.<domain>、imap.<domain>、<domain> 供选择）。
        ▼
   2. 手工配置：用户填服务器 URL，用 GET <base>/health 或
      GET <base>/client/account 校验（CLI：`account add … --server <URL>`）。
```

只有第 1 步会真的发起网络请求，而且只请求**裸域**一次：它不会再自动去试
`mail.<domain>`。要把部署放在非 443 端口上（例如 §5.4 末尾那种容器反代），让裸域的 443
把这份 JSON 吐出来即可——文档里的 `api` 字段本来就会指向真实端口。

规则：

| 规则 | 理由 |
|---|---|
| 从**地址的域名**获取，而不是从区域获取 | 记录就在那里，而且那是用户唯一输入过的部分 |
| 只用 HTTPS；纯 HTTP 的发现响应被忽略 | 否则网络上的攻击者可以把每个账户都重定向到自己的服务端 |
| 正常校验证书 | 理由同上 |
| 未知字段忽略，缺失字段走兜底 | 响应是带版本的，还会继续增长 |
| 官方客户端只使用`api` | `imap`/`smtp`条目的存在，是为了让同一条记录也能服务第三方客户端与将来的导入流程 |
| 客户端从`GET /client/account`记录`protocol_version` | 决定协商版本的是服务端（[fcp.md](fcp.md) §1） |
| 地址域名与发现到的服务器不一致时要显示出来，而不是藏起来 | 「你输入的是 example.com，但这台服务器是 mail.other.example」值得一次确认 |

项目书 §32 还为将来的兼容性预留了 **autoconfig** 与 **autodiscover**（Thunderbird 与
Microsoft 的惯例）_（计划中）_；今天两者都没有提供。

---

## 13. 设备（§33）

项目书 §33 列出了 API 与字段。端点冻结在[fcp.md](fcp.md) §9。

| 操作 | 端点 | 效果 |
|---|---|---|
| 列出 | `GET /api/v1/client/devices` | 该账户的每一次安装，含`last_seen_at`、`last_ip`、`platform`、`client_version`、`protocol_version`、`revoked` |
| 吊销 | `POST /api/v1/client/devices/:id/revoke` | 标记为已吊销，吊销它的会话，发布`device.revoked` |
| 删除 | `DELETE /api/v1/client/devices/:id` | 移除该记录 |

客户端行为：

* **登录时注册。** 在 FCP 登录体里发送`device`
  （`{ device_uid, name, platform, client_version }`），其中`device_uid`每次安装保持
  稳定且只生成一次（[fcp.md](fcp.md) §2）。
* **启动时更新。** 每次启动刷新一次，让`last_seen_at`与`client_version`保持最新，
  这正是设备列表能帮用户发现陌生安装的原因。
* **对`device.revoked`作出反应。** 针对*本*设备的 WebSocket 帧意味着：清除令牌、
  停止同步、保留缓存，并显示「此设备已被退出登录」。下一个请求会拿到`401`
  （[fcp.md](fcp.md) §9）。
* **允许吊销自己的设备**，而且立即生效。即将卖掉笔记本电脑的用户，必须能用手机把
  它切断。
* **退出登录与吊销不是一回事。** 登出丢弃本地令牌；吊销在服务端使它们失效。设置
  界面必须两者都提供，并说明各是哪一个。

---

## 14. 通知（§34）

项目书 §34：

```text
Mail Received
      ↓
 Event Bus
      ↓
Notification Service
      ↓
┌─────┴─────┐
▼           ▼
APNs       FCM
```

### 14.1 桌面端

| 平台 | 机制 |
|---|---|
| Windows | 通过 WinRT 通知 API 发送 toast 通知 |
| Linux | freedesktop 通知规范（libnotify / DBus） |
| macOS | `UNUserNotificationCenter` |

由`Event::mail_received`（`mail.received`）触发，它携带的正好是横幅所需的，不多
不少：

```json
{ "seq": 1842, "type": "mail.received", "mailbox_id": 3, "message_id": 4822,
  "from": "bob@example.net", "subject": "Re: Invoice", "snippet": "Thanks, got it." }
```

**事件携带摘要片段，绝不携带正文**（[architecture.md](architecture.md) §6），因此
通知无法把邮件内容泄漏进操作系统的通知存储，而那在 Windows 与 macOS 上是持久化且
可搜索的。

规则：

* **每封邮件只通知一次。** 同一个事件可能到达两次（一次套接字帧、一次同步变更）；
  客户端按`message_id`去重。
* **用户正在看该邮箱时抑制通知。** 为当前打开且获得焦点的文件夹弹通知是噪音。
* **尊重每账户与每文件夹的开关**，以及免打扰时段。
* **角标计数来自同步，而不是套接字。** 套接字可能漏事件；该显示的数字是
  `GET /client/mailboxes`的`folders.unseen_count`。
* **摘要片段里绝不含正文。** 服务端为此截断`MailReceived.snippet`；客户端也不得
  为了做出更丰富的横幅而去获取正文。

### 14.2 移动端预留：APNs 与 FCM

项目书 §34 把 APNs 与 FCM 放在「移动端预留」之下。这些设计预留让将来加入移动端不必
改模式：

| 预留 | 位置 |
|---|---|
| `client.push_enabled` | 配置键，默认`false`；服务端宣告推送能力但不启用它 |
| 一个设备令牌列 | 移动端上线时`devices`增加`push_token TEXT`与`push_platform TEXT`；该表已经存在，主键为`(user_id, device_uid)` |
| 推送负载 | 与`MailReceived`相同的字段：id、`from`、`subject`、`snippet`、计数。没有正文 |
| 投递路径 | `Event::MailReceived` → 通知服务 → APNs/FCM。事件已经是集成点，所以通知服务是唯一的新组件 |
| 隐私规则 | 锁屏通知显示发件人与主题。要看到正文需要解锁应用并经 FCP 获取 |

这些都没有实现。把它写下来的要点在于：`Event::MailReceived`已经携带了正确的字段，
`devices`也已经有稳定的身份，所以增加推送不需要协议变更，只需要一个服务端发送方与
一个移动客户端。

---

## 15. 设置（§52）

项目书 §52 列出了各个分区，以及同步方面的选项。

| 分区 | 内容 |
|---|---|
| **账户** | 添加/移除/暂停/重新认证；每账户的服务端 URL 与设备名称 |
| **同步** | 同步什么、同步多少（见下） |
| **通知** | 每账户与每文件夹的开关；免打扰；声音 |
| **外观** | 主题（浅色/深色/跟随系统）、密度、字号 |
| **阅读** | 默认用 HTML 还是纯文本；阻止远程内容；标记已读的延迟；会话视图 |
| **撰写** | 每账户签名；回复引用样式；发送延迟/撤销窗口 |
| **附件** | 自动下载策略；下载目录；自动下载的最大附件大小 |
| **搜索** | 是否索引正文；是否兜底到服务端 |
| **存储** | 缓存预算与缓存位置（见下） |
| **安全** | 令牌存储状态；「退出所有设备」；自动锁定 |
| **设备** | §13 的设备列表，带吊销 |
| **关于** | 版本、`protocol_version`、服务端版本、构建、日志位置 |

同步设置，来自项目书 §52：

```text
同步全部邮件             sync all mail
仅同步最近 30 天         sync the last 30 days only
仅同步最近 90 天         sync the last 90 days only
附件自动下载             auto-download attachments
仅 Wi-Fi 下载            download over Wi-Fi only
最大缓存大小             maximum cache size
```

每一项如何落到设计上：

| 设置 | 实现 |
|---|---|
| 同步窗口（全部/30/90 天） | 分页`GET /client/sync`时施加的谓词：`internal_date`落在窗口之外的变更记为已见（因此游标照常推进），但不存为邮件。被跳过邮件的正文永远不会被获取 |
| 自动下载附件 | 覆盖「打开时下载」的默认值 |
| 仅 Wi-Fi | 桌面端：连接是否按流量计费；由平台 API 报告 |
| 最大缓存大小 | `attachments.cached_path`与已缓存正文的淘汰预算（§9.2），按`cached_at`做 LRU |

设置存储：应用级的取值用`settings`且`account_id = 0`，每账户的取值用真实的 id。
服务端上也存在的取值（`client.tombstone_retention_days`会影响客户端的行为，但它是
服务端的策略）从服务端读取，并以只读方式显示；客户端若自己发明一套保留策略，就会
无声地偏离。

---

## 16. UI 方案（§51）

项目书 §51：

```text
┌─────────────────────────────────────────────────────┐
│ Ferroma                         🔍   ⚙   👤         │
├───────────────┬─────────────────────┬───────────────┤
│               │                     │               │
│ 收件箱        │ 邮件列表            │ 邮件阅读      │
│ 已发送        │                     │               │
│ 草稿          │ ┌─────────────────┐ │               │
│ 垃圾箱        │ │ Alice           │ │               │
│ 回收站        │ │ Invoice         │ │               │
│               │ ├─────────────────┤ │               │
│ 文件夹        │ │ Bob             │ │               │
│               │ │ Meeting         │ │               │
│               │ └─────────────────┘ │               │
│               │                     │               │
└───────────────┴─────────────────────┴───────────────┘
```

三栏外壳：一条账户与文件夹侧栏、一份邮件列表、一个阅读器，顶部横贯一条工具栏
（搜索、设置、账户）。

| 面板 | 内容 | 数据来源 |
|---|---|---|
| 侧栏 | 账户；每个账户下是它的文件夹，`INBOX`在最前，其余按名称排序；`special_use`标记驱动图标，因此名为`Sent Items`的文件夹仍被读作已发送 | `mailboxes`表（[imap.md](imap.md) §4.3） |
| 列表 | 每封邮件一行：发件人、主题、摘要片段、日期、附件回形针、标志；虚拟化，因为一个文件夹可以有 5 万行 | `messages`，按`internal_date DESC`分页 |
| 阅读器 | 头字段、纯文本或净化后的 HTML 正文、附件 | `message_headers`加上获取到的正文；HTML 经过净化器（[security.md](security.md) §10.2） |
| 撰写器 | 独立窗口或全宽浮层；收件人字段带本地收件人自动补全、主题、正文、附件芯片、发送/存草稿 | `drafts`、`attachments` |
| 发件箱 | §8 的状态机，带项目书 §28 的计数 | `outbox` |
| 搜索 | 覆盖在列表之上的浮层，带 §10 的运算符语法 | `search_index`，然后是服务端 |
| 设置 | §15 的十二个分区 | `settings` |

由核心的设计推出的 UI 规则：

* **UI 绝不在网络上阻塞。** 每个面板都从 SQLite 读取。正在进行的同步是一条状态行，
  而不是盖在列表上的转圈。
* **离线是一种可见状态，不是错误对话框。** 一个状态指示器，外加给尚未确认的操作
  逐项加上的标记。
* **发件箱始终可达。** 用户必须能看到某封邮件卡住了，以及为什么。
* **未读数来自文件夹行**，由同步刷新，绝不通过数已加载的列表来计算，因为列表是
  分页的，算出来必然是错的。
* **正文未缓存的邮件要说明这一点。** 绝不显示空的阅读器。

平台目标与阶段，来自项目书 §24.1 与 §50：

| 阶段 | 平台 |
|---|---|
| 第一期（v0.7–v0.9） | Windows、Linux、macOS |
| 第二期 | Android、iOS |

项目书 §50 的客户端里程碑：v0.7 是最小可用版本（登录、账户、收件箱、阅读、撰写、
回复、转发、删除、已读/未读、附件、基础同步），v0.8 离线、本地缓存、增量同步、
本地搜索、WebSocket，v0.9 多账户、设备管理、推送通知、草稿同步、发件箱。

---

## 17. 设计已经隐含的依赖

`client/Cargo.toml`已经声明了它们，这是关于预期形态最有力的证据：

| Crate | 用途 |
|---|---|
| `ferroma-core` | `FerromaError`、类型化 id、`Cursor`、地址解析，与服务端所用的是同一批类型 |
| `tokio` | 异步运行时；每账户一个同步任务 |
| `sqlx`（features `sqlite`、`runtime-tokio-rustls`、`migrate`、`chrono`） | 本地缓存，带真正的迁移 |
| `reqwest`（`rustls-tls`、`json`、`stream`、`multipart`） | FCP HTTP 客户端，含流式下载与 multipart 上传 |
| `tokio-tungstenite` | 实时 WebSocket（`GET /api/v1/client/events`） |
| `serde` / `serde_json` | 线上类型，与[fcp.md](fcp.md)一致 |
| `chrono` | 时间戳；一切都是 UTC |
| `clap` | 客户端二进制自己的 CLI（账户管理、供测试用的无界面同步模式） |
| `dirs` | 缓存与附件存储在平台上的数据目录 |
| `sha2` | 校验附件摘要，并为本地缓存去重 |
| `uuid` | 生成`device_uid`与`operation_id` |
| `tracing` / `tracing-subscriber` | 与服务端相同的结构化日志，好让 bug 报告能附上客户端日志 |
| `base64`、`hmac` | 当某个平台需要构造请求签名时的令牌处理 |

注意**不在**其中的东西：没有`ferroma-storage`，没有`ferroma-imap`，没有
`ferroma-smtp`，`sqlx`里也没有 PostgreSQL feature。客户端只链接共享核心 crate，
服务端那一侧别的什么都不链接（[architecture.md](architecture.md) §2）。

---

## 18. 相关文档

| 主题 | 文档 |
|---|---|
| FCP 线上格式：端点、游标、实时分帧、分块上传 | [fcp.md](fcp.md) |
| 管理 API、自动发现、健康检查、错误信封 | [api.md](api.md) |
| 同步模型：变更日志、先应用后推进、墓碑、失败矩阵 | [sync.md](sync.md) |
| 同一批账户上第三方客户端的 IMAP 行为 | [imap.md](imap.md) |
| 服务端上的附件内容寻址、配额、缓存淘汰 | [storage.md](storage.md) §6 |
| 令牌盗用检测、HTML 净化器、已知缺口 | [security.md](security.md) |
| 部署客户端所对话的服务端 | [deployment.md](deployment.md) |
| Crate 依赖图与分层规则 | [architecture.md](architecture.md) |
