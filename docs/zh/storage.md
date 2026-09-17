# 存储

**谁该读这份文档：** 任何改`ferroma-storage`的人、任何针对 Ferroma 数据库写 SQL 的人，
以及任何需要思考配额、磁盘占用、垃圾回收或恢复的运维者。

Ferroma 使用两个存储。PostgreSQL 保存应用要查询的每一个事实，谁存在、哪封邮件在哪个
文件夹、一次投递处于什么状态。文件系统保存字节，Maildir 里的 RFC 5322 邮件与内容寻址
二进制存储里的附件。本文说明哪个存储对什么内容是权威的，逐表走一遍 schema 并给出服务
每条查询的索引，规定 Maildir 投递算法及其持久性保证，解释配额记账与执行位置，描述附件
的内容寻址与 GC，最后给出备份模型与完整性检查方案。

> **状态：** 描述已实现的代码。`ferroma-storage` 已完成：
> `crates/ferroma-storage/src/{database,maildir,attachment,error,models}.rs` 与
> `crates/ferroma-storage/src/repository/*.rs`。schema 是
> `migrations/0001_initial.sql`，它是仓库中唯一的迁移，也是下文每一个表与列的来源。
> 有两处是 _(计划中)_：`ferroma storage` CLI 子命令，以及驱动 GC 与完整性检查的
> API 端点。凡给出 `pg_dump`/`psql` 命令的地方，都是今天就能运行的真实命令。

---

## 1. 两个存储，一个真相

```text
                        ┌──────────────────────────────┐
                        │        PostgreSQL            │
   权威描述：           │  users, domains, mailboxes,  │
   *什么存在*           │  folders, messages,          │
                        │  message_recipients,         │
                        │  attachments, mail_queue,    │
                        │  delivery_attempts, devices, │
                        │  sessions, client_sync_      │
                        │  states, drafts, operations, │
                        │  change_log, audit_logs,     │
                        │  login_attempts, settings    │
                        └──────────────┬───────────────┘
                                       │  messages.storage_path
                                       │  attachments.storage_path
                                       ▼
                        ┌──────────────────────────────┐
   权威描述：           │        文件系统              │
   *字节本身*           │                              │
                        │  Maildir：                   │
                        │    <root>/<domain>/<local>/  │
                        │      Maildir/{cur,new,tmp}   │
                        │      Maildir/.Folder/{…}     │
                        │                              │
                        │  二进制存储：                │
                        │    <root>/ab/cd/<sha256>     │
                        └──────────────────────────────┘
```

这个分工写在 `migrations/0001_initial.sql` 的头部，并实现在
`crates/ferroma-storage/src/lib.rs` 中：

> 数据库对*什么存在*是权威的，文件系统对*字节本身*是权威的。有行无文件是
> `StorageError::BodyMissing`；有文件无行则由 `AttachmentStore::gc` 与
> `Maildir::sweep_tmp` 回收。

| 问题 | 答案来自 |
|---|---|
| `alice@example.com` 存在吗？ | `mailboxes`（+ `domains.enabled`） |
| 她有哪些文件夹？ | `folders` |
| 有多少封未读？ | `folders.unseen_count` |
| 邮件 4821 的主题是什么？ | `messages.subject` |
| 邮件 4821 的 UID 是什么？ | `messages.uid` |
| 邮件 4821 的字节是什么？ | Maildir 根下 `messages.storage_path` 指的文件 |
| 附件 9 的内容是什么？ | 二进制存储根下 `attachments.storage_path` 指的文件 |
| 它投递成功了吗？ | `mail_queue.status` |

根目录来自配置，不来自数据库：`Config::maildir_root()` 是 `storage.maildir_root` 或
`<server.data_dir>/mail`，`Config::attachment_root()` 是 `storage.attachment_root` 或
`<server.data_dir>/attachments`。

**每个路径列都是相对路径。** `messages.storage_path` 形如
`example.com/alice/Maildir/cur/1758012751.M4821P3210.mail:2,S`，相对于 Maildir 根，
且形式固定：始终使用正斜杠（`Maildir::relative` 用 `/` 拼接）。这正是数据目录可以迁移的
原因：移动根目录，更新 `server.data_dir`，所有路径依然能解析。

---

## 2. schema，逐表说明

只有一个迁移：`migrations/0001_initial.sql`，440 行，在 `database.run_migrations = true`
时于启动阶段应用。它面向 PostgreSQL 14+，只使用核心的 `gen_random_uuid()` 时代特性，
不需要安装任何扩展。

### 2.1 身份

#### `users`

| 列 | 类型 | 说明 |
|---|---|---|
| `id` | `BIGSERIAL PK` | |
| `email` | `TEXT NOT NULL` | 唯一，小写 |
| `password_hash` | `TEXT NOT NULL` | 一个 Argon2id PHC 字符串，见 [security.md](security.md) §2 |
| `display_name` | `TEXT` | |
| `enabled` | `BOOLEAN NOT NULL DEFAULT TRUE` | 被停用的账户无法登录 |
| `is_admin` | `BOOLEAN NOT NULL DEFAULT FALSE` | |
| `quota_bytes` | `BIGINT NOT NULL DEFAULT 1073741824` | 1 GiB |
| `used_bytes` | `BIGINT NOT NULL DEFAULT 0` | Maildir 总量的反规范化缓存 |
| `failed_logins` | `INTEGER NOT NULL DEFAULT 0` | 连续失败次数 |
| `locked_until` | `TIMESTAMPTZ` | 由 `record_login_failure` 设置 |
| `last_login_at` | `TIMESTAMPTZ` | |
| `created_at`, `updated_at` | `TIMESTAMPTZ NOT NULL DEFAULT NOW()` | |

约束：`users_email_lowercase CHECK (email = lower(email))`、
`users_email_not_blank CHECK (length(btrim(email)) > 3)`、
`users_quota_sane CHECK (quota_bytes >= 0)`、`users_used_sane CHECK (used_bytes >= 0)`。

类型：`crates/ferroma-storage/src/models.rs` 中的 `User`；辅助方法
`User::is_login_allowed(now)` 读取 `enabled` 与 `locked_until`。

#### `domains`

| 列 | 类型 | 说明 |
|---|---|---|
| `id` | `BIGSERIAL PK` | |
| `name` | `TEXT NOT NULL` | 唯一，小写 |
| `description` | `TEXT` | |
| `enabled` | `BOOLEAN NOT NULL DEFAULT TRUE` | 被停用的域不接收任何邮件 |
| `catch_all` | `TEXT` | 接收本域内发往不存在邮箱的邮件的本地部分 |
| `dkim_selector` | `TEXT` | 按域配置的选择器 |
| `dkim_private_key`, `dkim_public_key` | `TEXT` | PEM；见 [security.md](security.md) §9 |
| `created_at`, `updated_at` | `TIMESTAMPTZ` | |

`domains_name_lowercase`、`domains_name_not_blank` 与 `users` 的检查对应。

**DKIM 私钥既存在这里的数据库中，也存在于配置的 `[dkim] private_key_path` 中。**
两者都受支持；`GET /api/v1/domains/:id/dkim` 读取的是数据库列，而在设置
`dkim.enabled` 时签名器优先使用文件。两者都要备份，见 §8。

### 2.2 地址、别名、文件夹

#### `mailboxes` — 是一个地址，不是文件夹

| 列 | 类型 | 说明 |
|---|---|---|
| `id` | `BIGSERIAL PK` | |
| `user_id` | `BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE` | |
| `domain_id` | `BIGINT NOT NULL REFERENCES domains(id) ON DELETE CASCADE` | |
| `local_part` | `TEXT NOT NULL` | 小写 |
| `display_name` | `TEXT` | |
| `enabled` | `BOOLEAN NOT NULL DEFAULT TRUE` | |
| `is_primary` | `BOOLEAN NOT NULL DEFAULT FALSE` | |
| `quota_bytes` | `BIGINT` | `NULL` = 继承属主的配额 |
| `created_at`, `updated_at` | `TIMESTAMPTZ` | |

这是 SMTP `RCPT TO` 的目标，也是配额记账的单位。它*不是* IMAP 文件夹；项目书 §37 的草案
为何被拆分，见 [architecture.md](architecture.md) §9.1。

#### `aliases`

| 列 | 类型 | 说明 |
|---|---|---|
| `id` | `BIGSERIAL PK` | |
| `domain_id` | `BIGINT NOT NULL REFERENCES domains(id) ON DELETE CASCADE` | |
| `local_part` | `TEXT NOT NULL` | 小写 |
| `target` | `TEXT NOT NULL` | 完整目标地址；只写本地部分表示「同域」 |
| `enabled` | `BOOLEAN NOT NULL DEFAULT TRUE` | |
| `created_at` | `TIMESTAMPTZ` | |

#### `folders` — IMAP 文件夹

| 列 | 类型 | 说明 |
|---|---|---|
| `id` | `BIGSERIAL PK` | |
| `mailbox_id` | `BIGINT NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE` | 所属地址 |
| `name` | `TEXT NOT NULL` | IMAP 名称，例如 `Archive/2026` |
| `parent_id` | `BIGINT REFERENCES folders(id) ON DELETE CASCADE` | 层级关系 |
| `special_use` | `TEXT` | `\Sent`、`\Drafts`、`\Trash`、`\Junk`、`\Archive`、`\All`、`\Flagged` 或 `NULL` |
| `subscribed` | `BOOLEAN NOT NULL DEFAULT TRUE` | `LSUB` |
| `uid_validity` | `BIGINT NOT NULL DEFAULT 1` | IMAP UID 世代 |
| `uid_next` | `BIGINT NOT NULL DEFAULT 1` | UID 分配器 |
| `highest_modseq` | `BIGINT NOT NULL DEFAULT 1` | 为 `CONDSTORE` 预留 |
| `message_count` | `INTEGER NOT NULL DEFAULT 0` | 计数缓存，由 `recount` 维护 |
| `unseen_count` | `INTEGER NOT NULL DEFAULT 0` | 计数缓存 |
| `total_bytes` | `BIGINT NOT NULL DEFAULT 0` | 计数缓存 |
| `created_at`, `updated_at` | `TIMESTAMPTZ` | |

约束：`folders_name_not_blank`，以及把 `special_use` 限制为上述七个值的
`folders_special_use_known`。

### 2.3 邮件

#### `messages`

| 列 | 类型 | 说明 |
|---|---|---|
| `id` | `BIGSERIAL PK` | 类型为 `MessageId`；API 以 `message_id` 返回 |
| `folder_id` | `BIGINT NOT NULL REFERENCES folders(id) ON DELETE CASCADE` | **权威的父级** |
| `mailbox_id` | `BIGINT NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE` | `folders.mailbox_id` 的反规范化副本 |
| `uid` | `BIGINT NOT NULL` | IMAP UID，在文件夹内唯一 |
| `rfc_message_id` | `TEXT` | RFC 5322 的 `Message-ID` 头字段，见 [architecture.md](architecture.md) §9.2 |
| `thread_id` | `TEXT` | `References` 链的根 `Message-ID` |
| `subject` | `TEXT` | 已解码 |
| `sender` | `TEXT` | `From` 地址 |
| `sender_name` | `TEXT` | `From` 显示名 |
| `snippet` | `TEXT` | 供列表视图与通知使用的短预览，不含正文 |
| `size_bytes` | `BIGINT NOT NULL` | |
| `storage_path` | `TEXT NOT NULL` | 相对于 Maildir 根 |
| `checksum_sha256` | `TEXT` | 小写十六进制，当 `storage.checksum = true` 时写入 |
| `flags` | `TEXT NOT NULL DEFAULT ''` | `Flags::to_db_string()` 形式，例如 `seen,flagged` |
| `internal_date` | `TIMESTAMPTZ NOT NULL DEFAULT NOW()` | IMAP `INTERNALDATE` |
| `received_at` | `TIMESTAMPTZ NOT NULL DEFAULT NOW()` | Ferroma 接收它的时刻 |
| `sent_at` | `TIMESTAMPTZ` | `Date` 头字段，可解析时写入 |
| `has_attachments`, `attachment_count` | `BOOLEAN` / `INTEGER` | 为列表视图反规范化 |
| `is_draft` | `BOOLEAN NOT NULL DEFAULT FALSE` | |
| `modseq` | `BIGINT NOT NULL DEFAULT 1` | 为 `CONDSTORE` 预留 |
| `deleted_at` | `TIMESTAMPTZ` | 软删除（`\Deleted`），发生在 expunge 之前 |
| `expunged_at` | `TIMESTAMPTZ` | 已从客户端视图中消失；行与文件可能仍然存在 |
| `created_at`, `updated_at` | `TIMESTAMPTZ` | |

约束：`messages_size_sane CHECK (size_bytes >= 0)`、
`messages_uid_sane CHECK (uid > 0)`。

两阶段删除值得展开说明，因为它正是「用户把它标记为已删除」与「邮件已经消失」的区别：

```text
live            expunged_at IS NULL AND deleted_at IS NULL
\Deleted        deleted_at  IS NOT NULL   (IMAP STORE +FLAGS \Deleted, API DELETE)
expunged        expunged_at IS NOT NULL   (IMAP EXPUNGE, API DELETE ?permanent=true)
```

每条读取路径都过滤 `expunged_at IS NULL`：`find_by_uid`、`list_by_uids`、
`list_by_folder`、`list_unexpunged`、`count_by_folder`、`newest`、`search`。
行由 `hard_delete` 删除，且只在 Maildir 文件已 unlink 之后。

#### `message_recipients`

| 列 | 类型 | 说明 |
|---|---|---|
| `id` | `BIGSERIAL PK` | |
| `message_id` | `BIGINT NOT NULL REFERENCES messages(id) ON DELETE CASCADE` | |
| `kind` | `TEXT NOT NULL` | `to`、`cc`、`bcc`、`reply-to`、`sender` |
| `address` | `TEXT NOT NULL` | |
| `display_name` | `TEXT` | |
| `ordinal` | `INTEGER NOT NULL DEFAULT 0` | 保留头字段中的顺序 |

`message_recipients_kind_known` 限制 `kind`。这张表存在的意义，是让
`IMAP SEARCH TO/CC/BCC`、API 的收件人过滤和 Admin 搜索变成一次带索引的查询，而不是每封
邮件重新解析一遍头字段。

#### `attachments`

| 列 | 类型 | 说明 |
|---|---|---|
| `id` | `BIGSERIAL PK` | 类型为 `AttachmentId` |
| `message_id` | `BIGINT NOT NULL REFERENCES messages(id) ON DELETE CASCADE` | |
| `filename` | `TEXT` | |
| `content_type` | `TEXT NOT NULL DEFAULT 'application/octet-stream'` | |
| `size_bytes` | `BIGINT NOT NULL` | |
| `storage_path` | `TEXT NOT NULL` | `ab/cd/<sha256>`，相对于二进制存储根 |
| `content_id` | `TEXT` | 内联部分的 `Content-ID` |
| `is_inline` | `BOOLEAN NOT NULL DEFAULT FALSE` | |
| `checksum_sha256` | `TEXT` | 与路径相同的摘要 |
| `created_at` | `TIMESTAMPTZ` | |

**多行，一个 blob。** 一行附件是（邮件、部分、元数据）；它指向的文件与所有持有相同字节的
行共享。删除一封邮件会移除它的行，但未必移除 blob，原因见 §6。

### 2.4 发信队列

#### `mail_queue`

| 列 | 类型 | 说明 |
|---|---|---|
| `id` | `BIGSERIAL PK` | 类型为 `QueueId` |
| `message_id` | `BIGINT NOT NULL REFERENCES messages(id) ON DELETE CASCADE` | |
| `user_id` | `BIGINT REFERENCES users(id) ON DELETE SET NULL` | 系统邮件为 `NULL` |
| `sender` | `TEXT NOT NULL` | 信封反向路径 |
| `recipient` | `TEXT NOT NULL` | **每个收件人一行** |
| `status` | `TEXT NOT NULL DEFAULT 'pending'` | `pending`、`delivering`、`delivered`、`retry`、`failed`、`cancelled` |
| `attempts` | `INTEGER NOT NULL DEFAULT 0` | |
| `max_attempts` | `INTEGER NOT NULL DEFAULT 12` | `queue.max_attempts` |
| `next_attempt_at` | `TIMESTAMPTZ` | 调度器可以重试的时间 |
| `last_attempt_at` | `TIMESTAMPTZ` | |
| `delivered_at` | `TIMESTAMPTZ` | |
| `last_error` | `TEXT` | |
| `last_status_code` | `INTEGER` | 远端的 SMTP 应答码 |
| `last_status_text` | `TEXT` | 远端返回的文本 |
| `remote_mx` | `TEXT` | 尝试过的远端主机 |
| `created_at`, `updated_at` | `TIMESTAMPTZ` | |

`mail_queue_status_known` 把 `status` 限制为上述六个值，另有
`mail_queue_attempts_sane CHECK (attempts >= 0)`。重试时机见
[smtp.md](smtp.md) §11.3。

#### `delivery_attempts`

每次尝试一行：`queue_id`（外键，级联）、`attempt`、`remote_mx`、
`status_code`、`status_text`、`error`、`duration_ms`、`created_at`。这是 Admin
「Delivery Logs」界面读取的历史记录。它随每次重试增长，这正是 `queue.retention_days`
（30）存在的原因，也是索引取 `(queue_id, attempt)` 而不是 `(created_at)` 的原因。

### 2.5 会话、设备、客户端同步状态

#### `devices`

`id`、`user_id`（外键，级联）、`device_uid TEXT`（客户端生成，按安装保持稳定）、
`name`、`platform`、`client_version`、`protocol_version`、`last_seen_at`、`last_ip`、
`created_at`、`revoked_at`。

`devices_uid_key UNIQUE (user_id, device_uid)` 让设备注册具备幂等性：
`DevicesRepository::upsert` 可以在每次客户端启动时调用。

#### `sessions`

`id`、`user_id`、`kind`（`web`、`api`、`client`、`imap`、`smtp`，由
`sessions_kind_known` 限制）、`token_hash TEXT`、`device_id BIGINT REFERENCES
devices(id) ON DELETE SET NULL`、`ip`、`user_agent`、`created_at`、`last_seen_at`、
`expires_at`、`revoked_at`。

**原始令牌从不存储。** `sessions.token_hash` 是某个不透明刷新令牌的 SHA-256
（`TokenService::hash`），因此数据库转储不会把可用的会话直接交给攻击者。访问令牌是无状态
JWT，完全不在这张表里。

#### `client_sync_states`

`id`、`device_id`（外键，级联）、`mailbox_id`（外键，级联）、`folder_id`（外键，
级联；`NULL` = 账户级）、`cursor BIGINT NOT NULL DEFAULT 0`、`updated_at`。

`client_sync_states_key UNIQUE (device_id, mailbox_id, COALESCE(folder_id, 0))`。
这个 `COALESCE` 是承重的：PostgreSQL 在唯一索引里把 `NULL` 视为互不相同，没有它，一个
设备就能累积出无限多的账户级行。`SyncStatesRepository::get` 对不存在的行返回 `0`，
其含义恰好是「从未同步过」。

### 2.6 草稿

#### `drafts`

`id`、`user_id`、`mailbox_id`、`folder_id`、`message_id`（Drafts 文件夹中镜像的那份
副本）、`subject`、`body_text`、`body_html`、`recipients JSONB DEFAULT
'[]'`、`attachments JSONB DEFAULT '[]'`、`in_reply_to`、`reference_ids JSONB
DEFAULT '[]'`、`created_at`、`updated_at`。

草稿既是行里的 JSON，*也是* Drafts 文件夹中的一封真实邮件，因此 IMAP 客户端与官方客户端
看到的是同一份草稿，见 [fcp.md](fcp.md) §7。

### 2.7 操作、变更日志、审计

#### `operations` — 幂等性

`operation_id TEXT PRIMARY KEY`、`user_id`、`kind`、`status`（`applied` 或
`failed`）、`result JSONB`（缓存的响应）、`created_at`、`completed_at`。

主键*就是*客户端生成的那个 `op_…` 字符串。`begin()` 是一条
`INSERT … ON CONFLICT DO NOTHING RETURNING *`，因此这次认领是原子的。

#### `change_log` — 同步日志

`seq BIGSERIAL PK`、`user_id`、`mailbox_id`、`folder_id`、`message_id BIGINT`、
`kind`、`payload JSONB`、`created_at`。

**`message_id` 与 `folder_id` 都没有外键，这是刻意的。** schema 注释说明了原因：
*「墓碑必须比行存活得更久」*。一条 `message_deleted` 记录必须在 `messages` 行消失之后
仍然可读，`folder_deleted` 同理必须在 `folders` 行消失之后仍可读，否则离线客户端
永远无法得知它们已经不见。`folder_id` 是在
`migrations/0003_change_log_folder_tombstones.sql` 中去掉约束的：在有约束期间，记录
文件夹删除的那条插入会违反它，`DELETE /api/v1/folders/:id` 因此报 `500`。
`mailbox_id` 保留级联——并不存在 `mailbox_deleted` 这种 kind。
`seq` 就是同步游标，见 [sync.md](sync.md)。

#### `audit_logs`

`id`、`actor_user_id`（外键 `ON DELETE SET NULL`，审计行比它记录的账户活得更久）、
`action`、`target_type`、`target_id`、`ip`、`user_agent`、
`details JSONB DEFAULT '{}'`、`created_at`。

### 2.8 登录限流与设置

#### `login_attempts`

`id`、`email`、`ip`、`kind TEXT NOT NULL DEFAULT 'password'`、`success BOOLEAN
NOT NULL`、`created_at`。每次登录尝试都写一行，无论成功还是失败；
`AuthService::login` 在做任何 Argon2 工作*之前*，先通过
`LoginAttemptsRepository::count_failures_for_ip(ip, window)` 读取它们，因此凭据洪水无法
烧掉 CPU。

#### `settings`

`key TEXT PRIMARY KEY`、`value JSONB NOT NULL`、`updated_at`。由数据库支撑的设置，
Admin 面板可以修改而无需重启。它们**不**覆盖 `ferroma.toml`：这里的值是运行时旋钮，
不是配置，`Config` 中没有任何东西读这张表。

---

## 3. 索引，以及每个索引服务的查询

没有查询使用的索引就是纯粹的写入成本。以下是 `migrations/0001_initial.sql` 中的完整列表，
以及每个索引存在的理由。

| 索引 | 表 | 服务 |
|---|---|---|
| `users_email_key` | `users` | 登录：`UsersRepository::find_by_email` |
| `users_admin_idx` … `WHERE is_admin` | `users` | 部分索引：（很短的管理员列表） |
| `domains_name_key` | `domains` | `RCPT TO` 的域解析、`DomainsRepository::find_by_name` |
| `mailboxes_address_key` | `mailboxes` | **SMTP 投递查找**：`(domain_id, local_part)` — 唯一，因此地址无法重复 |
| `mailboxes_user_idx` | `mailboxes` | `list_by_user`，调用者的地址列表 |
| `mailboxes_primary_key` … `WHERE is_primary` | `mailboxes` | 部分唯一：每个用户至多一个主地址 |
| `aliases_key` | `aliases` | `(domain_id, local_part)` — `RCPT TO` 上的别名查找 |
| `folders_name_key` | `folders` | `(mailbox_id, name)` 唯一 — `find_by_name`，以及阻止重复文件夹的守卫 |
| `folders_mailbox_idx` | `folders` | `list(mailbox_id)` |
| `folders_special_use_key` … `WHERE special_use IS NOT NULL` | `folders` | 部分唯一：每个地址至多一个 `\Sent`（等） |
| `messages_folder_uid_key` | `messages` | **`(folder_id, uid)` 唯一** — `find_by_uid`、`list_by_uids`、按 UID 的 `FETCH`/`STORE` |
| `messages_live_idx` | `messages` | **`(folder_id, internal_date DESC) WHERE expunged_at IS NULL`** — IMAP `SELECT` 视图与文件夹邮件列表 |
| `messages_mailbox_date_idx` | `messages` | `(mailbox_id, internal_date DESC)` — 「本地址的全部邮件」，Webmail 的 All-Mail 视图 |
| `messages_folder_date_idx` | `messages` | `(folder_id, internal_date DESC)` — 含已 expunge 行的文件夹列表（完整性检查、Admin） |
| `messages_rfc_id_idx` … `WHERE rfc_message_id IS NOT NULL` | `messages` | 部分索引，**非唯一**：`find_by_rfc_message_id`，用于会话归组与去重 |
| `messages_thread_idx` … `WHERE thread_id IS NOT NULL` | `messages` | 部分索引：会话分组 |
| `messages_sender_idx` | `messages` | `SEARCH FROM`、API 的发件人过滤 |
| `messages_subject_fts_idx` | `messages` | 基于 `to_tsvector('simple', coalesce(subject, ''))` 的 GIN — `SEARCH SUBJECT`、API 的 `?query=` |
| `message_recipients_message_idx` | `message_recipients` | 一封邮件的收件人 |
| `message_recipients_address_idx` | `message_recipients` | `SEARCH TO/CC/BCC`、「发给这个地址的邮件」 |
| `attachments_message_idx` | `attachments` | 一封邮件的附件 |
| `mail_queue_due_idx` | `mail_queue` | **`(next_attempt_at) WHERE status IN ('pending','retry')`** — 调度器的「现在该发什么？」热路径 |
| `mail_queue_message_idx` | `mail_queue` | 一封邮件的队列行 |
| `mail_queue_status_idx` | `mail_queue` | Admin 按状态过滤队列 |
| `mail_queue_user_idx` | `mail_queue` | `(user_id, created_at DESC) WHERE user_id IS NOT NULL` — 用户自己的 Outbox 视图 |
| `delivery_attempts_queue_idx` | `delivery_attempts` | `(queue_id, attempt)` — 某条队列行的尝试历史 |
| `devices_uid_key` | `devices` | `(user_id, device_uid)` 唯一 — 幂等的设备 upsert |
| `devices_user_idx` | `devices` | 某用户的设备列表 |
| `sessions_token_key` | `sessions` | `(token_hash)` 唯一 — 刷新令牌查找 |
| `sessions_user_idx` | `sessions` | Admin 中的「活跃会话」 |
| `sessions_expiry_idx` … `WHERE revoked_at IS NULL` | `sessions` | 部分索引：过期清理器 |
| `client_sync_states_key` | `client_sync_states` | `(device_id, mailbox_id, COALESCE(folder_id, 0))` 唯一 — 游标读写 |
| `client_sync_states_device_idx` | `client_sync_states` | `(device_id, updated_at DESC)` — 「这个设备在同步什么？」 |
| `drafts_user_idx` | `drafts` | `(user_id, updated_at DESC)` — 草稿列表 |
| `operations_user_idx` | `operations` | `(user_id, created_at DESC)` — 操作历史 |
| `operations_created_idx` | `operations` | `(created_at)` — `purge_older_than` |
| `change_log_cursor_idx` | `change_log` | **`(user_id, seq)`** — `changes_since(user_id, after, limit)`，同步查询 |
| `change_log_mailbox_idx` | `change_log` | `(mailbox_id, seq)` — 按地址同步 |
| `change_log_folder_idx` | `change_log` | `(folder_id, seq)` — 按文件夹同步 |
| `change_log_created_idx` | `change_log` | `(created_at)` — 保留期清理 |
| `audit_logs_actor_idx`, `audit_logs_action_idx`, `audit_logs_created_idx` | `audit_logs` | Admin 审计的三个过滤器 |
| `login_attempts_email_idx`, `login_attempts_ip_idx` | `login_attempts` | `(email, created_at DESC)` 与 `(ip, created_at DESC)` — 限流窗口 |
| `login_attempts_created_idx` | `login_attempts` | `(created_at)` — 保留期清扫 |

加粗的五个位于关键路径上。如果某个查询计划在 `messages_live_idx` 或 `mail_queue_due_idx`
上出现顺序扫描，问题出在查询，而不是索引。

新增索引之前要知道两件事：`messages.flags` 上没有索引，因此 `SEARCH UNSEEN` 是文件夹内
的过滤扫描，对邮箱规模的文件夹没问题，对百万行的文件夹不行；正文也没有索引，所以
`SEARCH BODY` 是 _(计划中)_（[imap.md](imap.md) §8）。

---

## 4. Maildir 布局与投递算法

### 4.1 布局

`storage.layout = "maildir"`（默认值）产生项目书 §13 中的布局：

```text
<maildir_root>/
└── example.com/                       <- domains.name，小写、已净化
    └── alice/                         <- mailboxes.local_part，小写、已净化
        └── Maildir/                   <- INBOX
            ├── cur/                   邮件客户端已读；标志位在文件名里
            ├── new/                   已投递但尚未被客户端读取
            ├── tmp/                   写了一半的文件；永不读取
            ├── .Sent/{cur,new,tmp}
            ├── .Drafts/{cur,new,tmp}
            ├── .Trash/{cur,new,tmp}
            ├── .Junk/{cur,new,tmp}
            ├── .Archive/{cur,new,tmp}
            └── .Archive.2026/{cur,new,tmp}   <- IMAP 的 "Archive/2026"
```

`storage.layout = "maildirperfolder"` 则为每个文件夹生成一个 Maildir：
`<root>/example.com/alice/INBOX/{cur,new,tmp}`、`…/Sent/{cur,new,tmp}`，不使用带点的
目录名。在文件名规则特殊的文件系统上选择它，见 [imap.md](imap.md) §10。

`Maildir::SUBDIRS` 就是字面量 `["cur", "new", "tmp"]`；`Maildir::INBOX` 是 `"INBOX"`。

### 4.2 文件名

```text
1758012751.M4821P3210.mail:2,S
└────┬───┘ └──┬───┘ └─┬─┘ │ └┬┘
  unix 秒数   pid _    主机 │  标志位：S = \Seen
              计数器       版本标记 "2,"
```

`Maildir::unique_filename` 构造 `<secs>.<pid>_<counter>.<hostname>`，并通过 `with_info`
追加 `{sep}2,{flags}`。唯一性在进程内由 `AtomicU64` 计数器保证，跨进程由 pid 保证；
主机名先经过 `sanitize_component`，失败时回退为 `ferroma`。

分隔符在 Unix 上是 `:`，在 Windows 上是 `;`，读取时两者都接受。完整解释见
[imap.md](imap.md) §10。

### 4.3 投递算法

`Maildir::store(domain, local_part, folder, bytes, flags)`：

```text
 1. dir = folder_dir(domain, local_part, folder)
       └─ 对域与本地部分做 sanitize_component，
          对文件夹做 maildir_folder_name  （路径穿越防御）
 2. create dir/{cur,new,tmp} if missing
 3. maildir_flags = flags_to_maildir(flags)      // "seen" -> "S"
 4. filename = unique_filename(maildir_flags)
 5. tmp_path   = dir/tmp/<filename>.tmp
 6. final_sub  = if maildir_flags.is_empty() { "new" } else { "cur" }
    final_path = dir/<final_sub>/<filename>
 7. std::fs::write(tmp_path, bytes)                  ← 整封邮件
 8. if storage.fsync_on_write: sync_all() on tmp_path
 9. std::fs::rename(tmp_path, final_path)            ← 原子步骤
10. if storage.fsync_on_write: sync_all() on dir/<final_sub>
11. return StoredMessage { path (relative), size, sha256 }
```

三个性质，以及它们各自为什么重要：

**原子性。** 同一文件系统内的 `rename(2)` 是原子的：读取者要么看不到文件，要么看到完整
的文件。因此一封邮件永远不会被观察到写了一半。`tmp/` 这一步的全部理由就在这里，也正是
读取者绝不能查看 `tmp/` 的原因：`Maildir::iter_messages` 跳过任何以 `.tmp` 结尾的文件，
`Maildir::sweep_tmp(older_than_secs)` 删除残留。

**持久性。** 第 8 步和第 10 步冲刷数据*以及*目录项，因此断电不会留下一个指向未冲刷块的
rename。`fsync` 会牺牲吞吐；`storage.fsync_on_write = true` 是默认值，因为一台先应答
`DATA` 再丢失邮件的邮件服务器已经违背了它唯一的承诺。使用带电池保护存储的运维者可以关掉它。

**名字的幂等性。** `store` 从不覆盖：每次调用都分配一个新的计数器值。`set_flags` 才是
幂等的那个，它计算出目标文件名，在无需移动时原样返回原路径。

### 4.4 Maildir API 的其余部分

| 函数 | 行为 |
|---|---|
| `read(relative_path)` | 整封邮件；文件缺失是 `StorageError::BodyMissing`，不是通用 IO 错误 |
| `read_prefix(relative_path, limit)` | 前 `limit` 个字节，供只取头部的 `FETCH` 使用 |
| `delete(relative_path)` | unlink；已经不存在时返回 `Ok(())`，因此重试安全 |
| `set_flags(relative_path, flags)` | 重命名文件，并随标志位集合变为非空或空而在 `new/` 与 `cur/` 之间移动；返回可能是新值的路径 |
| `move_message(relative_path, domain, local_part, to_folder, flags)` | 读取、存入目标、删除源 |
| `iter_messages(domain, local_part, folder)` | `cur/` 与 `new/` 中的每个文件，跳过 `.tmp`；返回 `MaildirEntry { path, size, maildir_flags, modified_secs }` |
| `usage(domain, local_part)` | 邮箱根下的总字节数，用于配额核对 |
| `sweep_tmp(older_than_secs)` | 删除早于阈值的废弃 `tmp/` 文件 |
| `ensure_mailbox` / `create_folder` / `delete_folder` / `rename_folder` / `list_folders` / `folder_exists` | 文件夹生命周期；`INBOX` 受保护，不可删除与重命名 |
| `absolute(relative_path)` | 路径穿越闸门，见 §7 |

注意 `move_message` 的不对称：它是先复制再删除，而不是 `rename`。跨文件夹移动可能跨越
文件系统边界，而且复制路径也让目标能够为新标志位集合获得一个重新编码的文件名。两份副本
同时存在的窗口是无害的：数据库行随后在一条语句里更新，因此事务提交之后，没有任何东西
指向源文件。

---

## 5. 配额记账

两个数字：`users.quota_bytes`（默认 `limits.mailbox_quota` = 1073741824 = 1 GiB）与
`mailboxes.quota_bytes`（`NULL` = 继承属主的配额）。`FoldersRepository` 不参与；配额按
**地址**计算，因为那才是 SMTP 投递的单位。

| 步骤 | 位置 |
|---|---|
| 读取有效上限 | `MailboxesRepository::quota(mailbox_id)` |
| 读取当前用量 | `MailboxesRepository::used_bytes(mailbox_id)` — `SELECT used_bytes FROM users` |
| 判定 | `MailboxesRepository::check_quota(mailbox_id, needed)` — 返回 `StorageError::QuotaExceeded { mailbox_id, used, needed, limit }` |
| 写入后调整 | `MailboxesRepository::add_usage(mailbox_id, delta_bytes)` |
| 从磁盘核对 | `MailboxesRepository::recompute_usage(mailbox_id)` — 用 `Maildir::usage` 遍历 Maildir 并写回总量 |

执行位置，以及从外部看到的执行结果：

| 路径 | 检查时机 | 结果 |
|---|---|---|
| 收信 SMTP | Maildir 写入之前，`DATA` 完整之后 | `452 4.2.2 Mailbox full`，**临时**失败，因此发件人会重试（[smtp.md](smtp.md) §12.1） |
| `POST /api/v1/messages`（发信） | 写 Sent 副本之前、入队之前 | `413 limit_exceeded` |
| `POST /api/v1/attachments` | 附件被挂到目标邮箱时 | `413 limit_exceeded` |
| IMAP `APPEND` | literal 落盘之前 | `NO [OVERQUOTA]` |
| IMAP/API 的标志位变更与移动 | 不检查，它们不改变总量 | — |

`used_bytes` 是缓存，`recompute_usage` 是修正它的方式。它可能在文件写入与计数器更新之间
发生崩溃后漂移，也可能在运维者手工往 Maildir 里添加文件后漂移。核对方式如下：

```sql
-- 数据库认为的情况，按地址列出。
SELECT m.id, d.name || '@' || m.local_part AS address, u.used_bytes
  FROM mailboxes m
  JOIN domains d ON d.id = m.domain_id
  JOIN users   u ON u.id = m.user_id
 ORDER BY u.used_bytes DESC;
```

```bash
# 某个地址在磁盘上实际占用多少。
du -sb /var/lib/ferroma/mail/example.com/alice
```

如果两者不一致，`recompute_usage` 就是修复手段，而这个差值本身值得追查：明显为正的差值
意味着有文件在 Ferroma 背后被删除，明显为负的差值意味着一次被中断的写入。

`storage.enforce_quota = false` 完全关闭这项检查。它服务于迁移，也服务于宁可先全部投递、
之后再收拾的运维者；处于该模式的服务器会把磁盘写满。

---

## 6. 附件：内容寻址与 GC

`AttachmentStore` 位于 `crates/ferroma-storage/src/attachment.rs`。

### 6.1 寻址

```text
<attachment_root>/ab/cd/abcdef0123…      <- 内容的 SHA-256，十六进制、小写
                  └┬┘└┬┘└─────┬─────┘
               字节 0-1  2-3   完整摘要
```

`AttachmentStore::path_for_digest(digest_hex)` 构造该路径，并以 `StorageError::Invalid`
拒绝短于四个字符或含非十六进制字符的摘要。两级分片让任何一个目录都不会装下几十万个条目，
这正是遍历二进制存储根时 `readdir` 在 ext4 与 NTFS 上都便宜的原因。

按内容寻址带来的后果：

| 性质 | 效果 |
|---|---|
| 相同内容只存一次 | 一个在组织内被到处转发的 PDF 只占一个 blob，无论有多少 `attachments` 行指向它 |
| `store` 是幂等的 | 两次存入相同的字节会返回 `StoredBlob { deduplicated: true }` 而不写入 |
| `ETag` 就是 SHA-256 | `GET /api/v1/attachments/:id` 无需额外工作就能提供强校验器，重复下载也永不重新传输（[fcp.md](fcp.md) §6） |
| blob 无法被静默损坏 | 文件名*就是*校验和，对整个树跑 `sha256sum` 即可验证整个存储 |

### 6.2 写入一个 blob

```text
1. digest   = sha256(data)
2. relative = path_for_digest(digest)          // "ab/cd/<sha256>"
3. if <root>/<relative> is already a file: return { deduplicated: true }
4. create_dir_all(parent)
5. tmp = parent/".{digest[4..16]}.{pid}.tmp"
6. write(tmp, data)
7. if storage.fsync_on_write: sync_all(tmp)
8. rename(tmp, final)      // Unix：内容相同时是原子覆盖
                           // Windows：先写入者胜出
9. return { path, size, sha256, deduplicated: false }
```

第 8 步是一场结果良性的竞争：如果两个并发写入者产生相同的摘要，`rename` 要么用相同的字节
覆盖（Unix），要么因为目标已存在而失败（Windows），而 `Err(_) if final_path.is_file()`
分支会清理临时文件并报告成功。无论哪种情况，字节都与文件名相符。

### 6.3 垃圾回收

**删除一行 `attachments` 不会删除 blob。** 它做不到：另一封邮件可能引用同一个摘要，而
这个存储没有引用计数。因此 `AttachmentStore::gc(keep)` 接收一个集合，一趟完成工作：

```text
for every file under the blob root:
    if the name ends in .tmp          -> remove  (一次被中断的写入)
    if keep contains the file name    -> keep
    if keep contains the relative path-> keep    (两种形式都接受)
    otherwise                         -> remove
```

`keep` 集合由 `AttachmentsRepository::referenced_paths()` 产生
（`SELECT DISTINCT storage_path FROM attachments`），调用方是
`POST /api/v1/storage/gc` _(计划中)_，或今天的直接调用。给重新实现它的人两条规则：

* **先收集 keep 集合，再扫描，而不是边扫边收集。** 在遍历目录树的同时读取
  `attachments` 会与并发上传竞争，其失败模式是删掉一个一毫秒前才挂上的 blob。
* **`gc` 也会清扫 `.tmp` 文件**，因此上传中途崩溃不会永久泄漏临时文件。
  `AttachmentStore::total_size` 排除它们，理由与清扫器删除它们相同。

`Maildir::sweep_tmp(older_than_secs)` 对邮件是等价物，但带一个阈值：比阈值更年轻的
`tmp/` 文件可能属于另一个任务上仍在进行的投递，因此零阈值清扫只在已停止的服务器上安全。

### 6.4 验证二进制存储

```bash
# 每个 blob 的文件名都必须等于其内容的 SHA-256。
cd /var/lib/ferroma/attachments
find . -type f ! -name '*.tmp' -printf '%f %p\n' | while read digest path; do
    actual=$(sha256sum "$path" | cut -d' ' -f1)
    [ "$actual" = "$digest" ] || echo "CORRUPT: $path (name $digest, content $actual)"
done
```

一切正常时输出示意，命令不打印任何内容。附件**不**由 `messages.checksum_sha256` 覆盖；
文件名就是校验和，因此这项检查完全不需要数据库。

---

## 7. 路径穿越防御

有三处会接收外部字符串并把它变成文件系统路径：域名、本地部分、文件夹名，以及邮件的
`storage_path` 与附件的 `storage_path`。三处都经过两道闸门之一。

### 7.1 `sanitize_component`

```rust
/// 拒绝任何可被用来逃出邮件根目录的内容。
pub fn sanitize_component(component: &str) -> Result<String> {
    let trimmed = component.trim();
    if trimmed.is_empty() {
        return Err(StorageError::Invalid("empty path component".into()));
    }
    if trimmed == "." || trimmed == ".." {
        return Err(StorageError::Invalid(format!("invalid path component: {trimmed}")));
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains('\0') {
        return Err(StorageError::Invalid(format!(
            "path component contains a separator: {trimmed}"
        )));
    }
    if trimmed.contains(':') {
        return Err(StorageError::Invalid(format!(
            "path component contains a colon: {trimmed}"
        )));
    }
    Ok(trimmed.to_string())
}
```

它在文件夹路径拼接之前对每一段调用（`maildir_folder_name`），在 `Maildir::mailbox_dir`
中对域与本地部分调用，在 `Maildir::new` 中对主机名调用。它的测试覆盖了有意思的输入：

```rust
for bad in ["..", ".", "a/b", "a\\b", "", "  ", "a:b", "x\0y"] {
    assert!(sanitize_component(bad).is_err(), "should reject {bad:?}");
}
```

拒绝 `:` 是刻意的，尽管 `:` 在 Unix 上合法：它是 Maildir 的信息分隔符，名为 `a:b` 的
文件夹会产生一个与带标志位文件名有歧义的目录名。在 Windows 上它同时还是 NTFS 的备用
数据流。

名字是**按段**校验的，因此 IMAP 文件夹 `Archive/2026` 可以通过（每一段都干净），而
`Archive/../../etc` 会在第三段被拒绝，而不是靠对整个字符串做模式匹配。

### 7.2 `absolute` — 相对路径闸门

两个存储各有一个，行为相同：

```rust
/// 把相对路径变成绝对路径，拒绝逃出根目录。
pub fn absolute(&self, relative_path: &str) -> Result<PathBuf> {
    let candidate = Path::new(relative_path);
    if candidate.is_absolute() {
        return Err(StorageError::Invalid(format!(
            "storage path must be relative: {relative_path}"
        )));
    }
    for component in candidate.components() {
        match component {
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err(StorageError::Invalid(format!(
                    "storage path escapes the mail root: {relative_path}"
                )));
            }
            _ => {}
        }
    }
    Ok(self.root.join(candidate))
}
```

`Maildir::absolute`（邮件根）与 `AttachmentStore::absolute`（二进制存储根）是把存储的
`storage_path` 变成真实路径的唯一函数。`read`、`read_prefix`、`delete`、`set_flags` 与
`exists` 都先调用它，因此即使数据库被攻陷，也没有任何代码路径能打开根目录之外的文件：

```rust
assert!(m.absolute("../../etc/passwd").is_err());
assert!(m.absolute("/etc/passwd").is_err());
assert!(s.absolute("../../secret").is_err());
```

`Component::Prefix(_)` 在 Windows 上很要紧：没有它，`C:\Windows\...` 会被当成相对路径
拼接到根目录上。

互补的函数是 `Maildir::relative`，它剥掉根目录并回退到 `with_info` 风格的规范化；不以
根目录开头的路径会得到 `StorageError::Invalid("path outside the mail root")`，而不是被
静默存成一个绝对路径。

---

## 8. 备份与恢复

完整的运维流程在 [deployment.md](deployment.md) §8。原则部分属于这里。

### 8.1 两半一起恢复

`scripts/backup.sh` 产出一个带时间戳的目录，其中包含：

| 文件 | 内容 |
|---|---|
| `ferroma.dump` | `pg_dump --format=custom --compress=6`，整个数据库 |
| `schema.sql` | `pg_dump --schema-only`，空但正确的结构 |
| `maildir.tar.gz` | 邮件根的 `tar -czf` |
| `config.tar.gz` | `ferroma.toml`、DKIM 密钥、TLS 材料（排除 `*.env` 与 `credentials*`） |
| `MANIFEST` | `ferroma_backup_version=1`、`created_at`、`database`、`postgres_version`、`ferroma_version`、`hostname` |
| `SHA256SUMS` | `sha256sum ./*`，让恢复过程能证明归档在传输中完好 |

脚本自身的头部写明了规则：

> 只包含前两项之一的备份不是备份：数据库说某封邮件存在，Maildir 持有它的字节，单独恢复
> 任何一半，得到的要么是一个满是悬空行的邮箱，要么是一个满是孤儿文件的目录。

具体来说，两种失败模式是：

| 恢复了 | 结果 |
|---|---|
| 只有数据库 | 每条 `messages.storage_path` 都指向一个不存在的文件。每次读取都是 `StorageError::BodyMissing`；用户看到一个只有主题、没有正文的收件箱 |
| 只有 Maildir | 文件存在，但没有任何东西知道它们。文件夹显示为空；在运维者手工重新导入之前，这些字节是不可见的 |

两者都无法由应用恢复，这就是为什么 `scripts/restore.sh` 在你要求 `--db-only` 或
`--mail-only` 时会打印警告，也是为什么 compose 的备份 sidecar 同时挂载两者。

### 8.2 在线备份的一致性

* **数据库那一半是单次 `pg_dump`**，因此它是某一个时间点的一致快照。
* **Maildir 那一半是实时 `tar`。** Maildir 写入是原子 rename，因此归档可能漏掉一次正在
  进行的投递，但绝不会包含写了一半的文件。因此在 dump 期间投递的一封邮件，可能存在于
  Maildir 却不在数据库里（孤儿文件），或者（可能性小得多，也更糟）两边都不存在，后一种
  情况下投递它的那次 SMTP 事务还没有被应答，发件人会重试。
* **孤儿方向才是安全的那一边**，这正是投递算法先写文件再写行的原因。
  `Maildir::sweep_tmp` 与完整性报告就是发现孤儿的手段。

如果你需要完全一致的一对，停掉容器：

```bash
docker compose stop ferroma
docker compose run --rm backup
docker compose start ferroma
```

这是唯一能保证没有在途事务被拆到两半的方式。对大多数部署来说，在线备份已经足够正确，
因为孤儿文件是无害的，而漏掉的邮件会被发信方 MTA 重试。

### 8.3 恢复顺序

`scripts/restore.sh` 遵循项目书 §47，按依赖图要求的顺序执行：

```text
0. verify      sha256sum -c SHA256SUMS   (abort on mismatch)
               print MANIFEST
1. database    pg_restore --no-owner --no-privileges --exit-on-error
               refuses a non-empty database unless FORCE_RESTORE=1
2. mail store  tar -xzf maildir.tar.gz
3. config      tar -xzf config.tar.gz
```

数据库放第一位，因为它是定义「应该存在什么」的那一半；Maildir 第二位，这样到服务器启动时
每一行都已经有了自己的文件。配置放最后，因此一次中途失败的恢复不会留下一个指向错误 TLS
证书的运行中服务器。

`restore_db` 拒绝覆盖已有内容的数据库，给出的消息里包含它找到的表数量：

```text
database ferroma is not empty (19 tables). Set FORCE_RESTORE=1 to overwrite,
or restore into a fresh database.
```

（19 是 `0001_initial.sql` 创建的表数量；真实消息里的数字是 `information_schema.tables`
为 `public` 报告的数量。）

这道守卫是脚本里最有价值的一行。静默合并两个邮件存储，正是运维者丢掉一周邮件的方式，
而没有任何自动化工具能分辨「在现有库上恢复」和「糟糕，连错数据库了」。

### 8.4 还必须保留什么

| 项目 | 原因 | 存放位置 |
|---|---|---|
| `ferroma.toml` | 上限、端口、TLS 路径、`api.public_url`、`server.hostname` | `config/ferroma.toml`，以只读方式挂载 |
| `[api] jwt_secret` / `FERROMA_JWT_SECRET` | 没有它，每次重启都会让所有会话失效 | 环境变量，**不**在配置归档里 |
| DKIM 私钥 | 丢失意味着所有签名失效、DMARC 开始失败 | `domains.dkim_private_key` **和** `/etc/ferroma/dkim/*.private` |
| TLS 证书与私钥 | 丢失意味着在重新签发之前 TLS 一直中断 | `/etc/ferroma/tls/` |
| PostgreSQL 角色与数据库 | 恢复需要有地方可恢复进去 | `postgres` 服务 |

JWT 密钥刻意被排除在 `config.tar.gz` 之外
（`--exclude='*.env' --exclude='credentials*'`），因为备份卷的保护可能弱于密钥存储。
把它放在你用于密钥的任何地方，见 [security.md](security.md) §10 与
[deployment.md](deployment.md) §4。

---

## 9. 完整性检查与修复

要验证的不变量：**每条 live 的 `messages` 行在其 `storage_path` 处都有一个可读文件，
且邮件根下的每个文件都属于某一行。**

### 9.1 找出没有正文的行

```sql
-- 候选集合：live 邮件。与文件系统比对。
SELECT m.id, m.mailbox_id, m.uid, m.size_bytes, m.storage_path
  FROM messages m
 WHERE m.expunged_at IS NULL
  ORDER BY m.id;
```

```bash
# 文件缺失的行。
psql "$DATABASE_URL" -Atc \
  "SELECT storage_path FROM messages WHERE expunged_at IS NULL" |
while read -r p; do
    [ -f "/var/lib/ferroma/mail/$p" ] || echo "MISSING: $p"
done
```

正文缺失在生产中表现为 `StorageError::BodyMissing`，它在 IMAP 上变成
`NO [SERVERBUG] message body missing`，在 API 上变成 `404 not_found`。这意味着文件系统
在 Ferroma 背后被改动过，或者一次恢复放回了一个比 Maildir 更新的数据库。

### 9.2 找出没有行的文件

```bash
# 邮件根下的孤儿候选：相对路径不在任何行中的文件。
psql "$DATABASE_URL" -Atc \
  "SELECT storage_path FROM messages" | sort > /tmp/known.txt
cd /var/lib/ferroma/mail
find . -type f ! -path '*/tmp/*' | sed 's|^\./||' | sort > /tmp/on_disk.txt
comm -13 /tmp/known.txt /tmp/on_disk.txt        # 在磁盘上，但不在数据库里
```

孤儿是被中断的投递以及崩溃后 `hard_delete` 的预期残留，而且它们是安全的那种失败方向。
它们不会被自动删除：一个因为恢复只做了一半而看起来像孤儿的文件，可能是某人邮件唯一的副本。

### 9.3 检查校验和

`messages.checksum_sha256` 在 `storage.checksum = true` 时写入
（`StoredMessage.sha256`，小写十六进制）：

```bash
psql "$DATABASE_URL" -Atc \
  "SELECT storage_path, checksum_sha256 FROM messages
    WHERE expunged_at IS NULL AND checksum_sha256 IS NOT NULL" |
while IFS='|' read -r p want; do
    got=$(sha256sum "/var/lib/ferroma/mail/$p" | cut -d' ' -f1)
    [ "$got" = "$want" ] || echo "CORRUPT: $p (want $want, got $got)"
done
```

### 9.4 检查计数器

`folders.message_count`、`unseen_count`、`total_bytes`，以及 `users.used_bytes`，都是
缓存。它们可以由其所汇总的数据修正：

```sql
-- 每个文件夹的计数器应该是多少。
SELECT f.id, f.name, f.message_count,
       COUNT(m.id) FILTER (WHERE m.expunged_at IS NULL) AS actual,
       COUNT(m.id) FILTER (WHERE m.expunged_at IS NULL
                            AND m.flags NOT LIKE '%seen%') AS actual_unseen,
       COALESCE(SUM(m.size_bytes) FILTER (WHERE m.expunged_at IS NULL), 0) AS actual_bytes
  FROM folders f
  LEFT JOIN messages m ON m.folder_id = f.id
 GROUP BY f.id, f.name, f.message_count
HAVING f.message_count <> COUNT(m.id) FILTER (WHERE m.expunged_at IS NULL)
 ORDER BY f.id;
```

`FoldersRepository::recount(folder_id)` 是修复手段：它重算全部三个计数器并返回更新后的
`Folder`。`MailboxesRepository::recompute_usage` 对 `users.used_bytes` 做同样的事。

`message_count` 不对只是表面问题，文件夹列表会显示错误的数字。`uid_next` 不对则不是：
它是 UID 分配器，而 `MessagesRepository::max_uid`
（`SELECT COALESCE(MAX(uid), 0) FROM messages WHERE folder_id = $1`）是它至少应达到的
值。如果 `uid_next` 曾经落后于 `max_uid`，下一次投递会分配一个已经存在的 UID，而
`(folder_id, uid)` 上的唯一索引会拒绝它。由于 `messages_folder_uid_key`，这是一次响亮
的失败，而不是静默覆盖。

### 9.5 每种发现该如何处理

| 发现 | 处理 |
|---|---|
| 正文缺失 | 从包含它的备份中恢复，或硬删除该行并记录日志。没有任何办法从元数据重建 RFC 5322 字节 |
| 孤儿文件 | 放着不动，或移到隔离目录。在确认恢复完整之前不要删除 |
| 校验和不匹配 | 文件在磁盘上变了。恢复它；不要「修」数据库 |
| 计数器漂移 | `FoldersRepository::recount` / `MailboxesRepository::recompute_usage` |
| `uid_next` 落后于 `max_uid` | 设置 `uid_next = max_uid + 1` **并递增 `uid_validity`**，UID 空间已被篡改（[imap.md](imap.md) §5.3） |
| 无人引用的 blob | 用刚收集的 keep 集合跑 `AttachmentStore::gc` |
| 残留的 `tmp/` 文件 | 在线服务器用 `Maildir::sweep_tmp(3600)`，已停止的服务器用 `sweep_tmp(0)` |

脚本与 API 提到了一个 `ferroma storage verify` 子命令来包装上述全部检查，以及一个
`POST /api/v1/storage/gc` 端点来运行 blob 回收器。两者都是 _(计划中)_：`ferroma`
二进制还没有实现子命令接口（`server/src/main.rs` 打印构建横幅后退出）。在它们存在之前，
本节中的 `psql` 与 shell 片段就是操作流程。

---

## 10. 决定存储形态的配置

| 键 | 默认值 | 作用 |
|---|---|---|
| `server.data_dir` | `./data` | 两个根目录的基准 |
| `storage.maildir_root` | `<data_dir>/mail` | Maildir 根 |
| `storage.attachment_root` | `<data_dir>/attachments` | 二进制存储根 |
| `storage.fsync_on_write` | `true` | 在应答 `DATA` 之前 `fsync` 邮件与其目录项 |
| `storage.layout` | `"maildir"` | `"maildir"`（Maildir++ 点号文件夹）或 `"maildirperfolder"` |
| `storage.checksum` | `true` | 存储每封邮件与每个附件的 SHA-256 |
| `storage.enforce_quota` | `true` | 拒绝超出配额的写入 |
| `storage.soft_delete` | `true` | 移入 `Trash` 而不是立即 unlink |
| `database.url` | `postgres://ferroma:ferroma@localhost:5432/ferroma` | 连接字符串 |
| `database.max_connections` / `min_connections` | 20 / 2 | 连接池上限 |
| `database.run_migrations` | `true` | 启动时应用 `migrations/*.sql` |
| `database.log_statements` | `false` | **绝不要在生产环境启用，它会打印邮件主题** |
| `limits.mailbox_quota` | 1073741824 | 新用户的默认配额 |

`Config::validate()` 在以下情况拒绝启动：`storage.maildir_root` 被设为空路径，
`database.url` 不是 `postgres://`/`postgresql://` URL，或
`database.min_connections > database.max_connections`。

---

## 11. 相关文档

| 主题 | 文档 |
|---|---|
| 为什么 `mailboxes` 与 `folders` 是分开的，以及 `rfc_message_id` | [architecture.md](architecture.md) §9 |
| Maildir++ 文件夹命名、UID/UIDVALIDITY、标志位映射 | [imap.md](imap.md) §4、§5、§6 |
| 变更日志、游标、墓碑 | [sync.md](sync.md) |
| SMTP 上的配额应答、重试调度、退信 | [smtp.md](smtp.md) §7、§11 |
| 备份命令、DNS、TLS、恢复演练 | [deployment.md](deployment.md) §8 |
| 什么从不记入日志、密钥处理 | [security.md](security.md) §10 |
