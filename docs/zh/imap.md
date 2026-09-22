# Ferroma 中的 IMAP

**适合谁读：** 正在实现 `ferroma-imap` 的人，正在针对 Thunderbird 或 Apple Mail
编写兼容性测试的人，以及用户无法让邮件客户端登录的任何运维者。

Ferroma 是面向第三方客户端的 IMAP4rev1 服务器（RFC 3501）：Thunderbird、
Apple Mail、Outlook、iPhone Mail 与 Android 客户端都是一等公民，不要求它们说 FCP
（项目书 §53）。本文档规定首个版本发布哪些命令、会话状态机、文件夹命名（包括 `INBOX`
特例与 Maildir++ 映射）、UID 与 UIDVALIDITY 语义、标志词汇表及其 Maildir 编码、
支持的 `FETCH` 数据项与 `SEARCH` 键、`IDLE`、`APPEND` 限制，以及兼容性目标清单。

> **状态：** 已实现，并由 `cargo test --workspace` 与走真实 socket 的验收测试覆盖。
> 下文描述的命令、会话状态机、文件夹模型、UID 语义、标志词汇表、`FETCH` 数据项与
> `SEARCH` 键，都是 `ferroma-imap` 今天的行为；标为「未实现」的行描述的是已规定但
> 尚未交付的行为。它所依赖的文件夹、标志、Maildir 与搜索行为位于
> `crates/ferroma-storage/src/maildir.rs`、
> `crates/ferroma-storage/src/repository/mailboxes.rs`、
> `crates/ferroma-storage/src/repository/messages.rs` 与
> `crates/ferroma-mail/src/flags.rs`。

---

## 1. 协议基线

| 属性 | 取值 |
|---|---|
| 协议 | IMAP4rev1，RFC 3501 |
| 问候 | `* OK [CAPABILITY …] <imap.banner>`，例如 `* OK [CAPABILITY IMAP4rev1 …] Ferroma IMAP4rev1 ready` |
| 端口 | `imap.port`，143，明文配合 `STARTTLS` |
| 隐式 TLS 端口 | `imap.imaps_port`，993，`0` 表示禁用 |
| 行终止符 | `CRLF` |
| 字面量 | `{n}` 同步字面量；也接受 `{n+}` 非同步字面量，因此 `APPEND` 可以流水线发送 |
| 认证 | `LOGIN`（用户名 + 密码）、`AUTHENTICATE PLAIN` |
| 加密 | 只用 rustls，见 [architecture.md](architecture.md) §8 |

`imap.require_tls_for_login`（默认 `false`）会在未加密的连接上以 `NO [PRIVACYREQUIRED]`
拒绝 `LOGIN` 与 `AUTHENTICATE`。`docker-compose.yml` 把
`FERROMA__IMAP__REQUIRE_TLS_FOR_LOGIN` 设为 `'true'`；生产部署应当保持这一设置。
`LOGIN` 以明文发送密码，而没有 `CRAM-MD5` 或 `SCRAM-*` 可以退而求其次（见 §8）。

**仅使用 rustls，不依赖 `LOGINDISABLED` 与 `AUTH=PLAIN` 的通告组合。** 当
`require_tls_for_login` 开启时，不会通告 `LOGIN` 在明文连接上可用；客户端在
`CAPABILITY` 中看到 `STARTTLS`，并应使用该命令。

---

## 2. 命令：实现与未实现

项目书 §12 把命令集一分为二。

### 2.1 已实现

```text
CAPABILITY   LOGIN    LOGOUT   NOOP
LIST         LSUB     STATUS   SUBSCRIBE
SELECT       EXAMINE  CLOSE    CHECK
FETCH        STORE    SEARCH   UID
APPEND       COPY     MOVE     EXPUNGE
IDLE         STARTTLS AUTHENTICATE
NAMESPACE    UNSELECT
```

再加上任何客户端要能用就离不开的几条，以及 §12 划为后续版本、此后已交付的那批：

| 命令 | 进入原因 / 门控 |
|---|---|
| `AUTHENTICATE` | Thunderbird 与 Apple Mail 默认用 `AUTHENTICATE PLAIN`，而不是 `LOGIN` |
| `STARTTLS` | 在 143 端口上加密通信的唯一途径 |
| `CLOSE` | 若干客户端在 `LOGOUT` 之前会发送它；没有它它们会记一条错误 |
| `CHECK` | 一个空操作的检查点，但它缺失会在某些客户端表现为协议错误 |
| `APPEND` | `imap.max_append_size` |
| `COPY` | — |
| `MOVE` | `imap.enable_move`，RFC 6851 |
| `EXPUNGE` | `storage.soft_delete` |
| `IDLE` | `imap.enable_idle`、`imap.max_idle_secs`，RFC 2177 |
| `UIDPLUS`（`UID EXPUNGE`） | RFC 4315，始终通告 |
| `NAMESPACE` | RFC 2342；Thunderbird 会请求它 |
| `UNSELECT` | RFC 3691；让客户端不必付出 `CLOSE` 的副作用就能退出选中的邮箱 |

### 2.2 v1 不做

| 命令 | 状态 |
|---|---|
| `SORT` / `THREAD` | 不在 v1 计划内；Thunderbird 会优雅回退 |
| `CONDSTORE` / `QRESYNC` | 存储侧已就绪（`folders.highest_modseq`、`messages.modseq`、`bump_modseq`），`HIGHESTMODSEQ` 应答码也存在，但没有任何命令发出它，也没有 `FETCH` 数据项返回 `MODSEQ` —— 未实现 |
| `COMPRESS=DEFLATE` | 不在 v1 计划内 |
| `NOTIFY` | 不在 v1 计划内；`IDLE` 一次一个文件夹地覆盖同样的需求 |
| `ACL`、`QUOTA`、`METADATA` | 不在 v1 计划内；配额在服务端强制执行，不需要 IMAP 扩展 |
| `CATENATE`、`BINARY`、`MULTIAPPEND` | 不在 v1 计划内 |

这些门控是真实的配置键，并在启动时校验：
`crates/ferroma-core/src/config.rs` 中的 `ImapConfig::enable_idle`、
`ImapConfig::enable_move`、`ImapConfig::max_idle_secs`、
`ImapConfig::max_append_size`。

### 2.3 `CAPABILITY`

通告的清单由配置生成，绝不硬编码：

```text
* CAPABILITY IMAP4rev1
             LOGINDISABLED        （仅当 imap.require_tls_for_login 且未加密）
             STARTTLS             （仅当 tls.enabled 且尚未加密）
             AUTH=PLAIN
             IDLE                 （仅当 imap.enable_idle）
             UIDPLUS              （始终通告——见 §2.1）
             MOVE                 （仅当 imap.enable_move）
             UNSELECT
             NAMESPACE
             LITERAL+
             CHILDREN             （Maildir++ 有真实的层级）
```

`LOGINDISABLED` 是 RFC 3501 §7.2.1 表达“`LOGIN` 在此连接上不可用”的方式；它是在
*保留* `LOGIN` 于清单的同时被通告的，因为两者都看不到的客户端会把它当成坏掉的服务器。

---

## 3. 会话状态

```text
   ┌──────────────────────────┐
   │          未认证          │◄── 连接，“* OK [CAPABILITY …]”
   └────────────┬─────────────┘
                │  LOGIN / AUTHENTICATE 成功
                │  （在此处 STARTTLS 会回到未认证）
                ▼
   ┌──────────────────────────┐
   │          已认证          │◄── CLOSE，或一次失败的 SELECT
   └────────────┬─────────────┘
                │  SELECT / EXAMINE
                ▼
   ┌──────────────────────────┐
   │          已选中          │  同一时刻只有一个邮箱
   └────────────┬─────────────┘
                │  LOGOUT（从任何状态）
                ▼
   ┌──────────────────────────┐
   │          已登出          │  “* BYE”，然后关闭 TCP
   └──────────────────────────┘
```

实现必须遵守的规则：

| 规则 | 说明 |
|---|---|
| 每个会话只有一个已选邮箱 | 已处于 `Selected` 时再 `SELECT` 会静默取消第一个并选中第二个。不存在隐式 `CLOSE`。 |
| `SELECT` 是读写，`EXAMINE` 是只读 | 该模式属于会话状态，并门控 `STORE` 与 `EXPUNGE`：`NO [READ-ONLY]` |
| 状态属于会话，而非全局 | 每个会话都有自己的已选邮箱、自己的标签命名空间和自己所处命令流水线的位置 |
| 在错误状态下发出的合法命令 | 带提示的 `BAD`，绝不静默空操作：`BAD Command not valid in this state` |
| `NOOP` 在任何状态下都合法 | 包括 `Selected`；待发的未标记更新也是在这里冲出的 |
| 未标记响应 | `SELECT` 时给出 `EXISTS`、`RECENT`、`FLAGS`、`UIDVALIDITY`、`UIDNEXT`、`PERMANENTFLAGS`；`Selected` 期间给出 `EXPUNGE`、`FETCH`、`EXISTS`、`RECENT` |
| 标签回显 | 每个带标签的应答都逐字重复客户端的标签，而 `*` 不会 |
| `imap.idle_timeout_secs` | 会话在此期间什么都没发就会收到 `* BYE Autologout; idle for too long`，随后套接字关闭。默认 1800。 |

会话结构体（`crates/ferroma-imap/src/session.rs`）：

```rust
pub struct ImapSession {
    context: SessionContext,       // 跨会话存活的共享状态
    parser: CommandParser,         // 解析器状态，包括在途的字面量
    config: SessionConfig,         // banner、tls、require_tls_for_login、enable_idle、
                                   // enable_move、max_append_size、max_literal_size、
                                   // max_idle_secs、starttls_available
    state: SessionState,           // NotAuthenticated | Authenticated | Selected | Logout
    user: Option<UserId>,          // 已认证的账号
    address: Option<String>,       // 该账号的主地址，local@domain
    local_part: Option<String>,    // 该地址对应的 Maildir 路径组件
    domain: Option<String>,
    selected: Option<Selected>,    // 文件夹行、邮箱 id、按 UID 排序的邮件列表，
                                   // 以及只读标志
    idle: Option<ferroma_events::Subscription>,   // 在 IDLE 被接受时创建
    pending: std::collections::VecDeque<Vec<u8>>, // 字面量的尾部，先于 socket 读取
}
```

---

## 4. 文件夹

### 4.1 其背后的表结构

文件夹是 `folders` 表中的行，而不是在请求时遍历的目录
（`migrations/0001_initial.sql`）：

| 列 | 含义 |
|---|---|
| `folders.mailbox_id` | 拥有该文件夹的地址 |
| `folders.name` | IMAP 名称，例如 `Archive/2026` |
| `folders.parent_id` | 层级结构的自引用；顶层文件夹为 `NULL` |
| `folders.special_use` | `\Sent`、`\Drafts`、`\Trash`、`\Junk`、`\Archive`、`\All`、`\Flagged` 或 `NULL` |
| `folders.subscribed` | `LSUB` 与 `LIST` 之别 |
| `folders.uid_validity`、`folders.uid_next` | 见 §5 |
| `folders.highest_modseq` | 由 `bump_modseq`（`crates/ferroma-storage/src/repository/mailboxes.rs`）为 `CONDSTORE` 递增；`HIGHESTMODSEQ` 应答码已存在，但还没有命令发出它 |
| `folders.message_count`、`unseen_count`、`total_bytes` | 由 `FoldersRepository::recount` 保持最新的计数 |

索引：`folders_name_key (mailbox_id, name)` 唯一：同一地址的两个文件夹不能重名；
`folders_special_use_key (mailbox_id, special_use) WHERE special_use IS NOT NULL`：每个
地址最多一个 `\Sent`。

表结构的 `CHECK` 把 `special_use` 限定在上述清单内。`INBOX` 以
`special_use = NULL` 存储。

### 4.2 `INBOX` 特例

RFC 3501 §5.1 让 `INBOX` 在三个方面特殊，三点都已实现：

1. **`INBOX` 不区分大小写。** `select inbox`、`SELECT Inbox` 与
   `SELECT INBOX` 是同一个文件夹。`FoldersRepository::find_by_name` 对 `INBOX` 做
   不区分大小写的比较，对其余一切做区分大小写的比较，而
   `crates/ferroma-storage/src/maildir.rs` 中的 `normalise_folder` 会在名称到达
   文件系统之前把拼写规范化：
   `normalise_folder("") == "INBOX"`、`normalise_folder("/") == "INBOX"`、
   `normalise_folder("/Sent/") == "Sent"`。
2. **`INBOX` 不能被创建、重命名或删除。** `CREATE INBOX` 是 `NO`
   （概念上它已经存在）；`RENAME INBOX` 是 `NO`；`DELETE INBOX` 是
   `NO`。`Maildir::delete_folder` 与 `Maildir::rename_folder` 也会拒绝它，因此
   即使某个协议层忘了这层保护，保护依然成立：
   `StorageError::Invalid("INBOX cannot be deleted")`。
3. **`INBOX` 总是排在第一位**，在 `LIST` 中以及在 `Maildir::list_folders` 中都是如此。
   客户端把第一个文件夹显示为收件箱；一个把 `Archive` 排在 `INBOX` 之前的 `LIST`
   在用户看来就是坏的。

`INBOX` 不携带 `special_use`。`FoldersRepository::ensure_standard` 故意用 `None`
创建它：RFC 6154 把 `\Inbox` 保留给包含全部邮件的文件夹，而仅靠 `special_use`
映射文件夹的客户端必须退回到匹配名称 `INBOX`。

### 4.3 标准文件夹集合

`FoldersRepository::ensure_standard`（以及 `Maildir::ensure_mailbox`）会创建：

| 文件夹 | `special_use` | Maildir 目录（`storage.layout = "maildir"`） |
|---|---|---|
| `INBOX` | `NULL` | `<root>/<domain>/<local>/Maildir/` |
| `Sent` | `\Sent` | `<root>/<domain>/<local>/Maildir/.Sent/` |
| `Drafts` | `\Drafts` | `<root>/<domain>/<local>/Maildir/.Drafts/` |
| `Trash` | `\Trash` | `<root>/<domain>/<local>/Maildir/.Trash/` |
| `Junk` | `\Junk` | `<root>/<domain>/<local>/Maildir/.Junk/` |
| `Archive` | `\Archive` | `<root>/<domain>/<local>/Maildir/.Archive/` |

幂等，且与自身竞争也是安全的：第二个调用者的 `INSERT` 输给唯一索引，随即被忽略。

### 4.4 Maildir++ 映射

嵌套的 IMAP 文件夹是有层级的，分隔符为 `/`。Maildir++ 把整条路径编码成一个目录名，
用点分隔：

```text
Archive/2026     ↔     .Archive.2026
Archive/2026/Q1  ↔     .Archive.2026.Q1
```

`maildir_folder_name(folder)` 实现其中一个方向
（`format!(".{}", folder.replace('/', "."))`），`Maildir::list_folders`
实现另一个方向（`name.trim_start_matches('.').replace('.', "/")`）。

这种嵌套*在磁盘上是扁平的*：`.Archive.2026` 是 `.Archive` 的兄弟，而不是它的子目录。
这就是 Maildir++ 的含义，也是任何邮件工具无需理解 IMAP 层级就能读取这棵树的原因。
它还意味着文件夹名中 `.` 出现的位置不能与分隔符产生歧义：一个字面名为 `a.b` 的
文件夹与嵌套的 `a/b` 会冲突。Ferroma 按每个 Maildir++ 实现的做法解决这个问题：
**IMAP 名称才是权威，它保存在 `folders.name` 中；目录名是推导出来的。** 含 `.` 的
名称会被忠实存储并原样提供，磁盘上的目录只是一种编码。`Maildir::folder_dir` 每次
访问都调用 `maildir_folder_name`，因此映射从不缓存，也就不可能漂移。

文件夹名在到达文件系统之前会被校验。
`sanitize_component` 应用于**每一个路径段**，因此 `Archive/../../etc`
是逐段被拒绝的，而不是靠对整个字符串做模式匹配：

```rust
// crates/ferroma-storage/src/maildir.rs
fn maildir_folder_name(folder: &str) -> Result<String> {
    for part in folder.split('/') {
        sanitize_component(part)?;
    }
    Ok(format!(".{}", folder.replace('/', ".")))
}
```

`sanitize_component` 拒绝 `""`、`"."`、`".."`、任何包含 `/`、`\`、
`\0` 或 `:` 的内容（见 §10），否则返回 `StorageError::Invalid`。

### 4.5 `LIST`、`LSUB`、`STATUS`、`SUBSCRIBE`

| 命令 | 行为 |
|---|---|
| `LIST "" "*"` | 所有文件夹，`INBOX` 在最前，其余按不区分大小写的字母序。`\HasChildren` / `\HasNoChildren` 来自 `parent_id` |
| `LIST` 属性 | 由 `parent_id` 推导出 `\HasChildren` / `\HasNoChildren`；根 `LIST "" ""` 应答给出 `\Noselect` |
| `LIST "" "Archive/%"` | `%` 匹配一层，`*` 匹配任意深度，依据 RFC 3501 §6.3.8 |
| `LSUB` | 满足 `folders.subscribed = true` 的文件夹。默认即为已订阅 |
| `SUBSCRIBE` / `UNSUBSCRIBE` | `FoldersRepository::set_subscribed`。绝不影响文件夹是否存在 |
| `STATUS mailbox (MESSAGES RECENT UIDNEXT UIDVALIDITY UNSEEN)` | 读取 `folders.message_count`、`unseen_count`、`uid_next`、`uid_validity`。在 `Authenticated` 状态下合法，对已选邮箱同样合法 |
| 带结尾分隔符的 `CREATE` | 依据 RFC 3501 §6.3.3，也会为父文件夹创建层级 |

---

## 5. UID 与 UIDVALIDITY

两个数值，两种职责。混淆二者是 IMAP 实现中的常见缺陷。

| | UID | UIDVALIDITY |
|---|---|---|
| 作用域 | 在**单个文件夹内**唯一且单调递增 | 标识某个文件夹 UID 空间的一个*代* |
| 存储 | `messages.uid`、`UNIQUE (folder_id, uid)` | `folders.uid_validity` |
| 分配者 | `folders.uid_next` | 在创建文件夹时分配，默认 `1` |
| 在以下情况保持稳定 | 标志变更、同一文件夹*内*的移动（并不存在这种移动） | 任何情况都不稳定，只有 UID 空间被重建时它才会变 |
| 绝不重用 | `MessagesRepository::move_to_folder` 在目标文件夹分配一个**全新** UID；旧 UID 永不重新发放 | — |

### 5.1 分配是原子的

存储邮件的 `INSERT` 在同一条语句里分配它的 UID，并且持有该文件夹的行锁：

```sql
WITH next_uid AS (
    UPDATE folders SET uid_next = uid_next + 1, updated_at = NOW()
     WHERE id = $1
     RETURNING uid_next - 1 AS uid
)
INSERT INTO messages (folder_id, mailbox_id, uid, …)
SELECT $2, $3, next_uid.uid, … FROM next_uid
```

`crates/ferroma-storage/src/repository/messages.rs`。因此同时投递进同一个文件夹的两封
邮件不可能拿到同一个 UID，崩溃也不会留下一个会被后续邮件填补的空洞。
`copy_to_folder` 与 `move_to_folder` 使用同一个 CTE。

### 5.2 UID 永不重用，被清除的 UID 永久消失

`MessagesRepository::find_by_uid` 与 `list_by_uids` 会过滤
`expunged_at IS NULL`：一旦客户端清除了某个 UID，再次引用它就是 `NO`，而不是把一个
被复活的邮件交出去。`expunge()` 设置 `expunged_at`，并按 `uid` 排序返回受影响的
行，因为 PostgreSQL 的 `UPDATE` 不保证顺序，而 IMAP 的 `EXPUNGE` 响应必须按 UID
升序（RFC 3501 对未标记 `EXPUNGE` 实际要求的是按下标降序；见 §6.3）。

### 5.3 什么会改变 UIDVALIDITY

`FoldersRepository::set_uid_validity(id, uid_validity)` 的存在，正是为了 UID 空间被
重建的那些情形：

| 事件 | UIDVALIDITY |
|---|---|
| 文件夹被创建 | 分配一次，默认 `1` |
| 正常运行 | **不变** |
| 文件夹被重命名 | 不变，它是同一个 UID 空间 |
| 邮件被清除 | 不变 |
| 数据丢失后从 Maildir 重建 | **递增** |
| 从重建之前取得的备份恢复 | **递增** |
| `ferroma storage verify --repair` 给文件夹重新编号 | **递增** |

实现遵循的规则是：只要任何操作可能让旧的 UID → 邮件映射出错，就递增 UIDVALIDITY。
保守地多递增一次，代价是客户端重新同步一个文件夹；漏掉一次递增，代价是客户端拿到
错误的邮件。

### 5.4 UIDVALIDITY 变化时客户端必须做什么

项目书 §55 的“服务器即唯一事实来源”让这一点毫无歧义，FCP 契约在
[fcp.md](fcp.md) §4 中如此陈述：

1. 发现 `SELECT` 应答中的 `UIDVALIDITY`（或
   `GET /api/v1/client/mailboxes` 中的值）与该文件夹缓存的值不同。
2. **丢弃该文件夹所有已缓存的 UID**，以及所有仅靠 UID 标识的缓存邮件。以
   `rfc_message_id` 或服务器的 `message_id` 为键的邮件正文可以保留。
3. 从游标 `0` 重新同步该文件夹。UID 会被重新发放。
4. 绝不假设变更之前的某个 UID 仍指向同一封邮件。

服务端的一条推论：因为客户端会丢弃它的缓存，Ferroma 不能随意递增 UIDVALIDITY。
每次投递都递增它，会让每个客户端在每封新邮件到来时重新下载整个文件夹。

---

## 6. 标志与关键字

### 6.1 词汇表

RFC 3501 §2.3.2 定义了六个系统标志：

```text
\Seen     \Answered     \Flagged     \Deleted     \Draft     \Recent
```

再加上**关键字**：客户端发来的其它一切：`$Junk`、`$label1`、
`$Forwarded`、`NonJunk`。`ferroma_mail::Flags` 把六个系统标志存为一个
`u8` 位集合（位 0 `\Seen`、1 `\Answered`、2 `\Flagged`、3 `\Deleted`、4 `\Draft`、
5 `\Recent`），把关键字按插入顺序存为一个 `Vec<String>`。关键字在入库时转小写并
去重，这正是 `Flags::to_db_string()` 具备确定性的原因。

`Flags::parse` 接受 `(\Seen \Flagged $Label1)`、不带括号的裸形式、重复空格以及
带引号的关键字。未知的 `\Foo` 名称会作为关键字存储，客户端的私有标志能在一个从未
听说过它们的服务器上往返而存活。

### 6.2 三种呈现，一套标志

| 呈现 | 方法 | 示例 | 使用位置 |
|---|---|---|---|
| IMAP | `Flags::to_imap_string()` | `(\Seen \Flagged "$Label1")` | `FETCH FLAGS`、`STORE` 应答、`PERMANENTFLAGS` |
| 数据库 | `Flags::to_db_string()` | `seen,flagged,$label1` | `messages.flags` |
| Maildir | `flags_to_maildir()` | `FS` | `cur/` 中的文件名 |

`messages.flags` 是 `TEXT NOT NULL DEFAULT ''`，表结构注释把它描述为空格分隔的列表。
`Flags::to_db_string()` 写入的是**逗号**分隔的值（`parts.join(",")`），转小写并去掉
反斜杠：`\Seen` → `seen`，`$Label1` → `$label1`。该列是不透明文本；逗号形式就是
已实现的写入方产出的形式，也是 `Flags::from_db_string()` 读回的形式。把
`Flags::to_db_string` / `from_db_string` 当作触碰该列的唯一正确方式；针对它手写
SQL 就是等着出缺陷。

`\Recent` 是棘手的那一个。它是一个系统标志，但它是*会话状态*，不是已存储的状态：
RFC 3501 说，如果这是第一个看到该邮件的会话，它就是 `\Recent`。Maildir 用目录来
表达它：一封尚未被邮件客户端看过的邮件在 `new/` 中，看过的在 `cur/` 中。因此
Ferroma 从目录推导 `\Recent`，而 `flags_to_maildir("recent")` 返回空字符串，因为
`\Recent` 绝不能出现在文件名里。

### 6.3 Maildir ↔ IMAP 标志映射

两套词汇表恰好在两个函数里相遇，即 `Maildir` 的 `flags_to_maildir` 与
`maildir_to_flags`，此外别无他处。如果哪个字母出了错，
`crates/ferroma-storage/src/maildir.rs` 是唯一需要改动的地方。

| IMAP 标志 | Maildir 字母 | 数据库形式 | 说明 |
|---|---|---|---|
| `\Draft` | `D` | `draft` | |
| `\Flagged` | `F` | `flagged` | |
| `\Answered` | `R` | `answered` | |
| `\Seen` | `S` | `seen` | |
| `\Deleted` | `T` | `deleted` | `T` 意为 trashed（已丢弃），这正是 `\Deleted` 在 Maildir 中的含义 |
| `\Recent` | *（无）* | `recent` | 由 `new/` 与 `cur/` 推导；无法存进文件名 |
| 关键字 `$Junk` | *（无）* | `$junk` | 关键字只存在于数据库中；Maildir 无处存放它们 |
| — | `P` | — | “passed”（已通过）；读取时接受，从不写入 |

写入顺序是 Maildir 约定的 `D F P R S T`，实现为
`D`、`F`、`R`、`S`、`T`：

```rust
pub fn flags_to_maildir(flags: &str) -> String {
    // … if has("draft") { out.push('D') } if has("flagged") { out.push('F') }
    //    if has("answered") { out.push('R') } if has("seen") { out.push('S') }
    //    if has("deleted") { out.push('T') }
}
```

`maildir_to_flags("FS") == "flagged seen"`；未知字母被忽略
（`maildir_to_flags("XYZ") == ""`），这正是另一个工具写入的存储仍然可读的原因。

**关键字按设计就是有损的那一部分。** 在文件夹之间移动邮件会根据标志集合重写它的
Maildir 文件名，而自定义关键字没有字母。它们留在 `messages.flags` 里；文件名只是
不提它们。因此重新读取时，关键字以数据库为准，字母以 Maildir 为准，这就是为什么
`iter_messages` 返回的 `maildir_flags` 是一份修复输入，而不是唯一事实来源。

`SELECT` 时的 `PERMANENTFLAGS` 通告 `(\Answered \Flagged \Deleted \Seen
\Draft \*)`；`\*` 是 RFC 3501 表达“接受任意关键字”的方式，而 Ferroma 确实接受它们。

### 6.4 `STORE` 语义

| 形式 | 行为 |
|---|---|
| `STORE 1:5 +FLAGS (\Seen)` | 添加 |
| `STORE 1:5 -FLAGS (\Seen)` | 移除 |
| `STORE 1:5 FLAGS (\Seen)` | 替换 |
| `STORE 1:5 +FLAGS.SILENT (…)` | 同上，但没有未标记的 `FETCH` 应答 |
| `UID STORE …` | 同上，按 UID 寻址 |
| `STORE` 中的 `\Recent` | 被忽略，不算错误：它不可设置 |

每一个真正改动了东西的 `STORE` 都按此顺序做三件事：

1. `MessagesRepository::set_flags` / `add_flags` / `remove_flags`：改数据库行，而
   那是所有其它界面读取的东西。
2. `Maildir::set_flags(relative_path, flags)`：重命名文件，当标志集合变为空或
   非空时在 `new/` 与 `cur/` 之间移动它。
   它返回可能是新的路径，如果发生了变化，调用方必须通过
   `MessagesRepository::set_storage_path` 把它持久化。
3. `EventBus::publish(EventScope::User(user_id), Event::mail_flag_changed(…))`，然后
   追加一条 `change_log`，让已同步的客户端无需轮询就能得知。

第 2 步是幂等的：计算出的名称未变时 `set_flags` 返回原路径，因此重复的 `STORE`
不会让文件系统反复折腾。

当 `BODY[…]` 在未带 `.PEEK` 的情况下被获取时，`FETCH` 会隐式写入 `\Seen`，而一次
`\Seen` 变迁同样会发布 `Event::mail_read`。想窥探的客户端（每个渲染列表的邮件
客户端）发送 `BODY.PEEK[]`。

### 6.5 Maildir 无法表达的东西，以及会发生什么

| 情形 | 行为 |
|---|---|
| 设置了一个关键字 | 存入 `messages.flags`；文件名不变 |
| 所有标志被清除 | 文件从 `cur/` 移回 `new/`，这正是客户端把邮件标为未读时所期望的 |
| 设置了一个标志 | 文件从 `new/` 移到 `cur/`，并追加 `2,<字母>` |
| 只读模式生效（`EXAMINE`） | `STORE` 为 `NO [READ-ONLY]` |

---

## 7. `FETCH` 数据项

| 数据项 | 支持 | 来源 |
|---|---|---|
| `FLAGS` | 是 | `messages.flags`，经 `Flags::to_imap_string()` |
| `UID` | 是 | `messages.uid` |
| `RFC822.SIZE` | 是 | `messages.size_bytes` |
| `INTERNALDATE` | 是 | `messages.internal_date` |
| `ENVELOPE` | 是 | 从已存储的头字段解析：`Date`、`Subject`、`From`、`Sender`、`Reply-To`、`To`、`Cc`、`Bcc`、`In-Reply-To`、`Message-ID` |
| `BODY` / `BODY[]` | 是 | 来自 Maildir 的完整 RFC 5322 字节 |
| `BODY[HEADER]` | 是 | 头字段块用 `Maildir::read_prefix` 读取，再用 `ferroma_mail::Headers` 解析 |
| `BODY[HEADER.FIELDS (…)]` | 是 | 选定的头字段，用 `fold_header_line` 重新折行 |
| `BODY[HEADER.FIELDS.NOT (…)]` | 是 | 上述的补集 |
| `BODY[TEXT]` | 是 | 空行之后的一切 |
| `BODY[<section>]` / `BODY[<section>]<partial>` | 是 | MIME 部件寻址，以及用于可续传下载的 `BODY[]<0.1024>` 部分获取 |
| `BODY.PEEK[…]` | 是 | 同上，但不设置 `\Seen` |
| `BODYSTRUCTURE` | 是 | 可扩展形式：每个部分都携带 `md5`、`disposition`、`language` 与 `location` 扩展字段（`crates/ferroma-imap/src/fetch.rs` 中的 `bodystructure`） |
| `BODY`（不可扩展的 `BODYSTRUCTURE`） | 是 | |
| `MODSEQ` | 否 | `messages.modseq` 已存储并建立索引供 `CONDSTORE` 使用，`HIGHESTMODSEQ` 应答码也存在，但没有任何 `FETCH` 数据项返回 `MODSEQ` |
| `BINARY[…]` | 否 | 不通告 `BINARY` |
| `X-GM-*` | 否 | 不模拟 Gmail 扩展 |

三条实用规则：

* **获取到的字节就是存储的字节。** `BODY[]` 返回的正是 Maildir 中的内容，即收到的
  那封邮件，最上面是 Ferroma 加的 `Received:` 头字段。没有任何东西被重新渲染，因此
  一次 `FETCH` 与一次 `curl` 请求
  `GET /api/v1/messages/:id/raw` 返回相同的字节。
* **`max_fetch_messages` 限制单条命令。** `limits.max_fetch_messages`（5000）
  为巨大文件夹上的 `FETCH 1:*` 设定上界；更大的范围会被拒绝，而不是任其分配内存。
  请求更多的客户端本来就是坏的，它们应当按 UID 范围分页。
* **正文缺失是 `NO`**，而不是被截断的结果：
  `StorageError::BodyMissing` 会变成
  `NO [SERVERBUG] message body missing`，该邮件会出现在
  `ferroma storage verify` 的完整性报告中（见 [storage.md](storage.md) §9）。

序列集与 UID 集接受完整的 RFC 3501 §9 文法：`1`、`1:5`、
`1:*`、`1,3,5:7`、`*`（文件夹中最高的 UID 或序列号）。在空文件夹上使用 `*`
是错误，而不是空集合。

---

## 8. `SEARCH`

`SEARCH` 以空格分隔的匹配*序列号*列表作答；`UID SEARCH` 以 UID 作答。`SEARCH`
并不很适合关系型存储，因此这种转换是显式的：

| 键 | 转换 |
|---|---|
| `ALL` | `expunged_at IS NULL` |
| `SEEN` / `UNSEEN` | `flags` 包含 / 不包含 `seen` |
| `ANSWERED` / `UNANSWERED` | `answered` |
| `FLAGGED` / `UNFLAGGED` | `flagged` |
| `DELETED` / `UNDELETED` | `deleted` |
| `DRAFT` / `UNDRAFT` | `draft` |
| `RECENT` / `OLD` | `new/` 与 `cur/`：属于推导，因此在 SQL 阶段之后用 Rust 求值 |
| `KEYWORD <kw>` / `UNKEYWORD <kw>` | 在 `messages.flags` 中对转小写后的关键字做精确匹配 |
| `FROM <s>` | `messages.sender` / `message_recipients`，要求 `kind = 'sender'` |
| `TO <s>`、`CC <s>`、`BCC <s>` | `message_recipients.address`，要求 `kind` 匹配 |
| `SUBJECT <s>` | `messages_subject_fts_idx`，一个建立在 `to_tsvector('simple', coalesce(subject, ''))` 上的 GIN 索引 |
| `BODY <s>` | 是——直接来自 Maildir 的原始邮件字节，仅在有键需要时读取 |
| `TEXT <s>` | 是——头字段加正文 |
| `HEADER <name> <s>` | 是——每个顶层头字段，展开后匹配；`messages` 也保留一个反规范化的子集 |
| `LARGER <n>` / `SMALLER <n>` | `messages.size_bytes` |
| `BEFORE <date>` / `ON <date>` / `SINCE <date>` | `messages.internal_date` |
| `SENTBEFORE` / `SENTON` / `SENTSINCE` | `messages.sent_at` |
| `UID <set>` | `messages.uid` |
| `NEW`、`OLD`、`RECENT` | 由上述键组合而成 |
| `NOT <key>`、`<key1> <key2>`（AND）、`OR <key1> <key2>` | 组合而成 |
| `<sequence set> <key>` | 组合而成 |
| `CHARSET UTF-8` | 接受；其它一律 `NO [BADCHARSET (UTF-8)]` |

`MessagesRepository::search(MessageSearch)` 是这一切背后的查询构造器；API 的
`/api/v1/messages` 过滤器与 Admin 的搜索使用同一个函数，因此对相同字段的一次
`SEARCH` 与一次 API 查询返回相同的邮件。这就是分层规则在真正起作用：IMAP 层只贡献
解析器。

`BODY`/`TEXT` 搜索会从 Maildir 读取邮件字节，但只对真正需要字节的键这样做——
`crates/ferroma-imap/src/search.rs` 中的 `SearchKey::needs_body` 让纯元数据搜索
（`UNSEEN`、`SINCE`、`FROM`……）完全跳过磁盘读取，而同一条命令里二十个 `BODY` 键
命中同一封邮件时，正文只解码一次而不是二十次。Admin/API 一侧在同样的仓储之上提供了
一个确实可用的主题与头字段搜索。

---

## 9. `IDLE` 与 29 分钟规则

RFC 2177 建议客户端持有 `IDLE` 不超过 29 分钟，并要求服务器终止它。
`config/ferroma.toml`：

```toml
[imap]
enable_idle = true
# 客户端持有 IDLE 的最长时间，单位为秒（RFC 2177 建议小于 30 分钟）。
max_idle_secs = 1740
```

1740 秒正好是 29 分钟。

```text
C: A001 IDLE
S: + idling
      … 会话保持 Selected 并推送未标记更新 …
C: DONE
S: A001 OK IDLE terminated
```

| 规则 | 说明 |
|---|---|
| `+ idling` | 立即发送；此时连接处于类似字面量的续行状态，只有 `DONE` 合法 |
| 推送什么 | `* n EXISTS`、`* n RECENT`、`* n FETCH (FLAGS …)`、`* n EXPUNGE`，即从 `ferroma-events` 到达该会话的事件的一个子集 |
| 数据来源 | `EventBus::subscribe_filtered(EventScope::User(user_id))`，每个会话的每个已选文件夹一个订阅 |
| 服务端超时 | 到达 `imap.max_idle_secs` 时服务器发送 `* BYE Idle timeout` 并关闭。行为良好的客户端会重新发起 `IDLE`，这正是让手机上的套接字保持存活的原因 |
| 除 `DONE` 之外的任何东西 | `BAD` |
| 当 `imap.enable_idle = false` 时的 `IDLE` | `BAD`，且不通告 `IDLE` |
| 在 `Authenticated` 中（未选中任何文件夹）的 `IDLE` | `BAD`，没有可报告的文件夹 |
| 会话空闲超时 | `imap.idle_timeout_secs`（1800）是另一回事：它是会话*什么都不发*的超时，而在活跃文件夹上的 `IDLE` 并不算“什么都不发” |

事件总线只在进程内。一个 `IDLE` 在某个文件夹上的会话会收到同一进程内所做更改的
推送；另一个共享数据库的进程所做的更改不会被推送，而是在下一次 `NOOP`、`CHECK` 或
`SELECT` 时到达，并且对重新连接的客户端始终可见。这与
[architecture.md](architecture.md) §6 和 [security.md](security.md) 中提到的限制相同。

`IDLE` 正是大多数客户端不需要 `CONDSTORE` 的原因：服务器推送发生了什么变化，客户端
就不必轮询 `STATUS` 去发现。

---

## 10. `APPEND` 与 Windows 的 info 分隔符

### 10.1 `APPEND` 限制

| 限制 | 取值 | 应答 |
|---|---|---|
| 最大字面量 | `imap.max_append_size`，26214400（25 MiB） | `NO [TOOBIG] Literal too large` |
| 字面量语法 | `{n}` 与 `{n+}` | — |
| 日期时间 | `APPEND "Sent" (\Seen) "16-Sep-2026 09:12:31 +0000" {n}` | 无法解析的日期是 `BAD`，而不是被忽略 |
| 标志 | 服务器接受的任何标志或关键字 | 不可接受的标志是 `BAD` |
| 目标文件夹必须存在 | 是 | `NO [TRYCREATE]`，即 RFC 3501 §6.3.11 的信号，表示客户端应先 `CREATE` |
| 配额 | 写入之前调用 `MailboxesRepository::check_quota` | `NO [OVERQUOTA]` |
| 结果 | 邮件存入 Maildir、插入行、分配 UID、记录变更 | 通告 `UIDPLUS` 时为 `OK [APPENDUID <uidvalidity> <uid>]` |

`APPEND` 原样存储字面量。与提交不同，它不会在前面加上 `Received:` 头字段：这封邮件
不是经由 SMTP 到达这里的，凭空发明一跳会破坏客户端试图保留的头字段链。它确实会设置
标志，据此把文件移入 `cur/` 或 `new/`，并在目标文件夹分配一个全新的 UID。

`APPEND` 到 `Drafts` 是第三方客户端的“保存草稿”最终与官方客户端的
`POST /api/v1/client/drafts` 落到同一处的方式；两者绝不能分叉，因此服务端草稿会以
`\Draft` 标志镜像到 Drafts 文件夹（[fcp.md](fcp.md) §7）。

### 10.2 Windows 上的 `;` 与 `:` 分隔符

Maildir 文件名在一个“info”段之后携带它的标志：

```text
1758012751.M4821_P3210.mail:2,S
└───────── base ──────────┘ │ │
                            │ └── 标志
                            └──── 版本
```

在 Unix 上分隔符是 `:`，这是二十年来每个 Maildir 实现都在用的事实标准。在 Windows
上它不能是 `:`，因为 NTFS 把 `name:stream` 当作**备用数据流**：创建一个字面名为
`1758012751.M4821_P3210.mail:2,S` 的文件要么失败，要么静默地创建出不是普通文件的
东西，标志会不可见，或者写入直接失败。因此 Windows 上的 Maildir 实现同样长久以来
改用 `;`。

Ferroma 两者都支持，位于 `crates/ferroma-storage/src/maildir.rs`：

```rust
/// 规范的 Maildir “info” 分隔符（没有 RFC 的事实标准，Dovecot 等）。
pub const INFO_SEPARATOR_UNIX: char = ':';

/// 在文件名中保留 `:` 的文件系统上使用的分隔符。
pub const INFO_SEPARATOR_WINDOWS: char = ';';

/// 本次构建写入时使用的 “info” 分隔符。
pub fn info_separator() -> char {
    if cfg!(windows) { INFO_SEPARATOR_WINDOWS } else { INFO_SEPARATOR_UNIX }
}
```

规则，以及为什么两个常量都存在：

| 函数 | 行为 |
|---|---|
| `info_separator()` | *本次构建写入*时用哪个：Windows 上 `;`，其它地方 `:` |
| `with_info(base, flags)` | 用 `info_separator()` 追加 `{sep}2,{flags}`；没有标志时原样返回 `base` |
| `split_info(file_name)` | **两种分隔符都接受**，而且都会尝试，因此在 Linux 上写入的存储能在 Windows 上读，反之亦然 |

```rust
assert_eq!(split_info("1234.M1P.host:2,S"), ("1234.M1P.host", "S"));
assert_eq!(split_info("1234.M1P.host;2,FS"), ("1234.M1P.host", "FS"));
assert_eq!(split_info("1234.M1P.host"), ("1234.M1P.host", ""));
// 有些工具会省略 `2,` 版本标记。
assert_eq!(split_info("1234.M1P.host:S"), ("1234.M1P.host", "S"));
```

三条推论：

1. **邮件存储是可移植的。** 把 `Maildir/` 从 Linux 服务器复制到 Windows 工作站并让
   Ferroma 指向它，不需要重写任何一个文件名，读取接受两种形式。新的写入使用本地
   分隔符，因此一棵树合法地可以同时含有两种，而 `split_info` 能处理。
2. **`split_info` 忽略单字符前缀。** `if base.len() <= 1 { continue; }`
   之所以存在，是因为 Windows 路径开头的 `C:` 看起来与 info 分隔符一模一样；
   没有这道防线，路径前缀会被误认为标志。
3. **挂载方式很重要。** 一个 CIFS/SMB 共享上的 Maildir 挂载到 Linux 后，在服务器是
   Linux 时会以 `:` 写入，而如果远端是 Windows，写入仍可能在 NTFS 层失败。布局
   设置是按部署刻意区分的：`storage.layout = "maildir"` 使用 `Maildir/.Folder`
   子目录，而 `"maildirperfolder"` 每个文件夹一个 Maildir
   （`<local>/INBOX`、`<local>/Sent`），没有带点的目录名，在文件名规则特殊的
   文件系统上这是更安全的选择。

`sanitize_component` 会拒绝它收到的每个路径组件中的 `:`，这防止*文件夹*名把 info 段
偷带进目录名。邮件文件名由 `unique_filename` 构造，绝不来自用户输入。

---

## 11. 兼容性目标

项目书 §12：*“Thunderbird、Apple Mail、Outlook、iPhone Mail、Android
邮件客户端能够逐步兼容”*，即与这五者逐步兼容。

| 客户端 | 平台 | 用到的东西 | 说明 |
|---|---|---|---|
| **Thunderbird** | Windows、Linux、macOS | `CAPABILITY`、`LOGIN`/`AUTHENTICATE PLAIN`、`LIST`、`LSUB`、`SELECT`、`FETCH`、`STORE`、`SEARCH`、`IDLE`、`APPEND` | 五者中最严格的：它用 `LSUB` 构建文件夹面板、在已选文件夹上用 `IDLE`、把 `APPEND` 发往 `Drafts`，并在重新同步后用 `UID SEARCH`。它会请求 `NAMESPACE`，并能容忍 `NO` |
| **Apple Mail** | macOS | 同一套，外加 `STATUS` 轮询与 `UID FETCH` | 期望用 `\Sent` / `\Drafts` / `\Trash` / `\Junk` 特殊用途标记来安置文件夹 |
| **Outlook** | Windows | `CAPABILITY`、`LOGIN`、`SELECT`、`FETCH`、`STORE`、`APPEND`、`IDLE` | 对未通告的扩展容忍度更低；必须针对真实构建做测试 |
| **iPhone Mail** | iOS | `CAPABILITY`、`LOGIN`、`SELECT`、`FETCH`、`STORE`、`APPEND`、`IDLE`、`SEARCH` | 最依赖 `IDLE` 的一个；`IDLE` 一坏，看起来就像“邮件收不到” |
| **Android 邮件客户端** | Android | `CAPABILITY`、`LOGIN`、`SELECT`、`FETCH`、`STORE`、`SEARCH` | K-9 Mail 与 FairEmail 是现实的目标；两者都能处理 `MOVE` 缺失 |

首版的兼容性门槛，来自项目书 §44：

```text
CAPABILITY   LOGIN   SELECT   FETCH   STORE   SEARCH   UID   IDLE
```

其中每一项都必须针对真实客户端演练，而不能只针对一致性脚本。
`openssl s_client -crlf -connect host:143` 加上手工敲出来的会话，能抓住测试套件抓不到
的大部分问题。

与特殊用途标记（§4.3）的配合，是让文件夹安置在五者上都能工作的关键：一个按
`\Sent` 映射、而不是按名称 `Sent` 推断的客户端，在某账户的文件夹名为 `Sent Items` 时
仍能对应。

---

## 12. 手工测试 IMAP

```bash
# 问候、能力清单，以及一次完整的登录/选择/获取，未加密。
openssl s_client -crlf -connect 127.0.0.1:143

# 隐式 TLS。
openssl s_client -connect 127.0.0.1:993

# 只取一个运行中服务器的能力清单。
printf 'a CAPABILITY\r\nb LOGOUT\r\n' | openssl s_client -quiet -crlf -connect 127.0.0.1:143
```

示例会话输出：

```text
* OK [CAPABILITY IMAP4rev1 STARTTLS AUTH=PLAIN AUTH=LOGIN IDLE MOVE UIDPLUS LITERAL+ CHILDREN] Ferroma IMAP4rev1 ready
a LOGIN alice@example.com "…"
a OK LOGIN completed
b LIST "" "*"
* LIST (\HasNoChildren) "/" "INBOX"
* LIST (\HasNoChildren \Sent) "/" "Sent"
* LIST (\HasNoChildren \Drafts) "/" "Drafts"
* LIST (\HasNoChildren) "/" "Archive"
* LIST (\HasChildren) "/" "Archive/2026"
b OK LIST completed
c SELECT INBOX
* 412 EXISTS
* 3 RECENT
* FLAGS (\Answered \Flagged \Deleted \Seen \Draft)
* OK [PERMANENTFLAGS (\Answered \Flagged \Deleted \Seen \Draft \*)] Flags permitted
* OK [UIDVALIDITY 1] UIDs valid
* OK [UIDNEXT 118] Predicted next UID
c OK [READ-WRITE] SELECT completed
d UID FETCH 117 (FLAGS RFC822.SIZE INTERNALDATE BODY.PEEK[HEADER.FIELDS (SUBJECT FROM)])
* 117 FETCH (UID 117 FLAGS (\Seen) RFC822.SIZE 24831 INTERNALDATE "16-Sep-2026 09:12:44 +0000" BODY[HEADER.FIELDS (SUBJECT FROM)] {68}
Subject: Invoice for September
From: Bob <bob@example.net>
)
d OK UID FETCH completed
e LOGOUT
* BYE Ferroma IMAP4rev1 server signing off
e OK LOGOUT completed
```

关于“IMAP 登录失败”，见 [deployment.md](deployment.md) §11。

---

## 13. 相关文档

| 主题 | 文档 |
|---|---|
| 为什么 IMAP 与 FCP 同时存在 | [fcp.md](fcp.md) |
| 文件夹与邮件表、Maildir 投递、完整性检查 | [storage.md](storage.md) |
| 同步游标、墓碑、冲突消解 | [sync.md](sync.md) |
| TLS、`require_tls_for_login`、威胁模型 | [security.md](security.md)、[deployment.md](deployment.md) |
| crate 分层、事件总线、请求生命周期 | [architecture.md](architecture.md) |
