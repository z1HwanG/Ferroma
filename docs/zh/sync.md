# 同步

**读者对象：** 任何实现或调试同步链路的人，无论做的是
`ferroma-sync` / `ferroma-storage::repository::sync` 里的服务端部分，
还是 `client/src/sync/` 里的客户端部分。

本文说明 Ferroma 客户端如何与服务端保持同步：为什么在 IMAP 之外还有 FCP，
「服务端是事实来源」约束了什么，`change_log.seq` 如何成为客户端保存的游标，
使重放安全的「先应用后推进」契约，墓碑如何让客户端得知它错过的删除，
游标过旧时该怎么办，为什么 `uid_validity` 会让文件夹缓存失效，
`operations` 如何让重试的变更无害，冲突如何解决，离线发件箱是什么，
以及每一类失败会发生什么。**线上格式**（请求与响应的形状、头部、
WebSocket 分帧）冻结在 [fcp.md](fcp.md) 中，本文不再重复。

> **状态：** 混合。服务端原语已实现：
> `migrations/0001_initial.sql` 中的
> `change_log`、`client_sync_states` 与 `operations`；
> `crates/ferroma-storage/src/repository/sync.rs` 中的 `ChangeLogRepository`、
> `SyncStatesRepository`、`OperationsRepository` 与 `OperationOutcome`。
> 把这些变成 `GET /api/v1/client/sync` 的服务位于 `ferroma-sync`，
> 状态为 _(计划中)_，`crates/ferroma-sync/src/lib.rs` 只是一个骨架。
> 客户端的同步引擎（`client/src/sync/`）状态为 _(计划中)_。

---

## 1. 为什么在 IMAP 之外还有 FCP

IMAP 已经能同步邮件。项目书 §20 与 §53 明确指出 FCP 不取代它，
[fcp.md](fcp.md) §1 给出了分工：

| 需求 | IMAP | FCP |
|---|---|---|
| 首次抓取一个文件夹 | `SELECT` + `FETCH 1:*`，可以 | `GET /sync?cursor=0`，分页 |
| 只抓取上次之后变化的部分 | 需要 `CONDSTORE`/`QRESYNC` 或完整 `FETCH`，还要基于 `EXPUNGE` 做差异比对，而多数服务端的实现各不相同 | 一个游标，`seq > last` |
| 在离线期间知道某封邮件被**删除** | `EXPUNGE` 响应只在会话内有效；当时没有连接的客户端永远看不到 | 变更流中的 `message_deleted` 墓碑 |
| 跨设备共享的服务端草稿 | 向 `Drafts` 执行 `APPEND`，再加猜测 | `drafts` 行，镜像到 `Drafts`（[fcp.md](fcp.md) §7） |
| 设备清单与远程吊销 | 无法表达 | `devices` + `device.revoked` |
| 分块、可续传的附件上传 | 无法表达 | `/attachments/init`、`/chunk`、`/complete`（[fcp.md](fcp.md) §6） |
| 实时推送 | `IDLE`，每个连接一个文件夹，每 29 分钟重新建立一次（[imap.md](imap.md) §9） | 整个账号一个 WebSocket |
| 幂等的变更重放 | `UID` 让部分操作可重复，但不是全部 | 每个变更请求都带 `operation_id` |

最后一行才是对桌面客户端真正关键的一行。IMAP 无法表达「我已经请你把 4821 标为
已读，而我的请求超时了，告诉我发生了什么」，因此重试的客户端要么重复应用，
要么只能用一个 `FETCH` 重新推导状态。FCP 的 `operations` 表正好回答这个问题。

同时跑两套的代价是真实的：每个变更都必须能通过两个入口看到，
这正是 [architecture.md](architecture.md) §3 中那条分层规则存在的原因。
通过 IMAP 做的标志变更必须产生与通过 FCP 做的相同的 `change_log` 行，
否则两种视图就会分叉。

---

## 2. `Server = Source of Truth`

项目书 §26 与 §55 都给出了这条规则：

```text
Server = Source of Truth
Client SQLite = Local Cache
```

它具体约束了什么：

| 客户端可以 | 客户端不得 |
|---|---|
| 离线时从缓存展示邮件、文件夹、草稿和标志 | 在服务端不同意时，把缓存状态当作权威 |
| 在本地排队变更，并乐观地把它应用到缓存 | 发送一个服务端尚未接受的变更，并当作已完成 |
| 在服务端删除邮件之后继续保留其正文 | 在看到墓碑之后继续提供已删除的邮件 |
| 自行决定缓存淘汰策略 | 因为缓存被淘汰就断定邮件不存在 |
| 持有一个游标并信任它 | 编造服务端从未发送过的状态 |

三条会在代码里显现出来的后果：

1. **客户端绝不写入自己没有收到的 UID。** UID 来自
   `messages.uid`；自行分配 UID 的客户端在任何一次重新同步之后都会与服务端不一致。
2. **本地缓存未命中不是删除。** 客户端应当去抓取正文
   （`GET /api/v1/client/messages/:id`），而不是断定邮件已经不存在。
   只有墓碑才是删除。
3. **冲突朝服务端一侧解决，草稿除外。** 见 §9。

---

## 3. `change_log` 是游标的来源

### 3.1 表结构

```sql
CREATE TABLE change_log (
    seq        BIGSERIAL   PRIMARY KEY,
    user_id    BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    mailbox_id BIGINT      REFERENCES mailboxes(id) ON DELETE CASCADE,
    folder_id  BIGINT,
    message_id BIGINT,
    kind       TEXT        NOT NULL,
    payload    JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

四个索引：`change_log_cursor_idx (user_id, seq)`，用于同步查询；
`change_log_mailbox_idx (mailbox_id, seq)`，用于按地址同步；
`change_log_folder_idx (folder_id, seq)`，用于按文件夹同步；
`change_log_created_idx (created_at)`，用于保留期修剪。

`message_id` 与 `folder_id` **都不是**外键。表结构在注释里说明了原因：
*"tombstones must outlive rows"*，即墓碑必须比数据行活得更久。这两列正是
`*_deleted` 变更所指的对象，任何一列上的约束都会在关键时刻打断日志——`folder_deleted`
是在文件夹行已消失*之后*写入的，而在 `migrations/0003_change_log_folder_tombstones.sql`
之前，那条插入会以 `change_log_folder_id_fkey` 报 `500`。`mailbox_id` 保留级联：
并不存在 `mailbox_deleted` 这种 kind，因此不会有任何代码写入邮箱墓碑。
这是同步设计中最重要的一个表结构决策，§5 会解释它。

### 3.2 为什么用 `seq` 作游标

游标必须满足四件事：单调、可比较、存储便宜、对客户端不透明。

| 性质 | `seq` 如何满足 |
|---|---|
| 单调 | `BIGSERIAL`，由 PostgreSQL 分配，在一个序列内绝不重复使用 |
| 可比较 | `seq > $2` 就是整个查询 |
| 便宜 | `client_sync_states` 中每个（设备、邮箱、文件夹）一个 `BIGINT` |
| 不透明 | 客户端把它当作字符串，绝不解析它（[fcp.md](fcp.md) §3） |

最后一条是刻意的：今天的 `ferroma_core::Cursor` 是 `i64` 的 newtype，
而 API 契约规定客户端不得依赖这一点。如果这个机制将来必须改变，
比如改成按用户的序列、时间戳加 id，或面向多节点的向量时钟，
那些把它当作不透明值的客户端可以照常工作。

**`seq` 是按用户全局的，不是按文件夹的。** 同步五个文件夹的客户端可以持有
五个游标，却仍然能看到它们之间一致的相对顺序，因为这五个值来自同一个序列。
正因如此，把目标文件夹流中到达的 `message_moved` 与源文件夹流中的
`message_deleted` 合在一起才是安全的，无论顺序如何：两者都以 `message_id` 为键。
[fcp.md](fcp.md) §3 给出了同样的保证。

### 3.3 查询

```rust
// crates/ferroma-storage/src/repository/sync.rs
pub async fn changes_since(
    &self,
    user_id: UserId,
    after: Cursor,
    limit: i64,
) -> Result<Vec<ChangeLogEntry>> {
    // SELECT * FROM change_log
    //  WHERE user_id = $1 AND seq > $2
    //  ORDER BY seq ASC LIMIT $3
}
```

`after` 是**排他的**：传入客户端已有的最后一条记录的 `seq`，绝不会重复它。
`changes_since_in_mailbox` 是同样的查询加上 `AND mailbox_id = $2`。

`limit` 是 `client.sync_page_size`（500），而 `limit_of(limit)` 会对它做钳制，
客户端无法一次索要一百万条变更。填满一页的客户端用新的末尾 `seq` 再问一次，
页边界不是事务边界，也不需要是，因为每个变更都可以独立应用。

`max_seq(user_id)` 返回 `COALESCE(MAX(seq), 0)`，
`has_more: false` 的响应和 WebSocket 的 `hello` 帧都把它报告为 `last_seq`。

### 3.4 `client_sync_states`

```sql
CREATE TABLE client_sync_states (
    id         BIGSERIAL PRIMARY KEY,
    device_id  BIGINT NOT NULL REFERENCES devices(id)   ON DELETE CASCADE,
    mailbox_id BIGINT NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
    folder_id  BIGINT REFERENCES folders(id) ON DELETE CASCADE,   -- NULL = 账号级
    cursor     BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX client_sync_states_key
    ON client_sync_states (device_id, mailbox_id, COALESCE(folder_id, 0));
```

这一行是服务端对每个*设备*读到哪里的记录。它不是客户端用来决定抓取什么的东西，
客户端保存自己的游标并把它发上来。服务端这一行存在的目的是：

* Admin 设备视图可以显示同步进度，
* 丢失本地状态的设备可以用 `SyncStatesRepository::reset_for_device` 重置，
* `change_log` 的保留期清扫知道是否可以安全地修剪（§6）。

`SyncStatesRepository::get` 对缺失的行返回 `0`：「还没读过」与
「从未同步过」是同一种状态。

唯一索引里的 `COALESCE(folder_id, 0)` 承担了两处关键作用：它既是
`SyncStatesRepository::set` 中 upsert 的冲突目标，而没有它，
PostgreSQL 的 NULL 互不相等特性会让一台设备累积出无限多账号级的行。
客户端上 SQLite 的等价物是一个覆盖 `(account_id, folder_id)` 的 `UNIQUE` 索引，
账号级用哨兵值 `0`。

---

## 4. 先应用后推进的契约

摘自 [fcp.md](fcp.md) §3，这里作为客户端的义务重述，并给出理由：

```text
loop:
    page = GET /sync?mailbox_id=A&folder_id=F&cursor=<stored>&limit=500
    BEGIN LOCAL TRANSACTION
        for change in page.changes:      # 按顺序
            apply(change)                # 在本地插入 / 更新 / 删除
        store_cursor(page.next_cursor)   # 在同一个事务里
    COMMIT
    if not page.has_more: break
```

事务内部的顺序就是全部要点：

| 崩溃点 | 结果 |
|---|---|
| `COMMIT` 之前 | 什么都没被应用，游标也没动。下一次同步会抓取同一页 |
| `COMMIT` 之后、套接字关闭之前 | 这一页已应用，游标已推进。下一次同步抓取*下一页* |
| `apply` 期间、页中间 | 事务回滚，因此看不到半页，游标也没有动 |

因此**每个变更处理器都必须是幂等的**，因为一页可能被合法地投递两次。具体来说：

| 变更 | 幂等实现 |
|---|---|
| `message_created` | `INSERT … ON CONFLICT (server_id) DO UPDATE` |
| `message_updated` | 把标志设为 payload 中的绝对值，绝不「切换」 |
| `message_deleted` | `DELETE WHERE server_id = ?`，什么都没删掉也没关系 |
| `message_moved` | 移到指定的那个文件夹；已经在那里就是空操作 |
| `folder_created` | `INSERT … ON CONFLICT (name) DO NOTHING` |

由此得到的规则是：**变更携带绝对状态，绝不携带增量。** 一条
`message_updated` 的 payload 携带结果标志字符串，而不是「切换
`\Seen`」，正是为了让重放它无害。这就是为什么
`Event::mail_flag_changed` 携带的是 `flags: String`（完整的数据库标志字符串），
而不是一个差分。

一个先应用一页、*然后*在事务之外保存游标的客户端，会有一段已应用变更
却尚未记录它们的窗口；崩溃之后它会重新抓取这一页，这没问题，
因为处理器是幂等的。事务是一种优化，不是正确性要求，
而契约在设计上就具备这一性质。

**绝不把游标推进到你没有应用的变更之后。** 被跳过的变更会永久不可见：
`seq > cursor` 再也不会返回它。如果客户端无法应用某个变更（payload 格式错误、
本地约束冲突），正确的做法是停下来、报告它，并把该文件夹从 `0` 重新同步，
而不是跳过并推进。

---

## 5. 墓碑

### 5.1 问题

离线两天的客户端重新连接并同步。它的缓存里某个文件夹有 400 封邮件。
服务端有 395 封。哪五封没了？

它无法通过比对来回答，因为它不知道本地存在的某封邮件之所以「缺失」，
是因为它被删除了，还是因为客户端的缓存不完整。它也无法从 `messages`
回答，因为那些行已经不在了。

### 5.2 机制

每次删除都会向 `change_log` 追加一条 `message_deleted` 变更，
而在 `messages` 行被移除之后，那一行**仍然留着**。这就是
`change_log.message_id` 没有外键换来的东西：

```text
   messages                    change_log
   ────────                    ──────────
   id 4821  "Invoice"          seq 1836  kind message_created  message_id 4821
   …                           seq 1839  kind message_moved    message_id 4821
                               seq 1841  kind message_deleted  message_id 4821
   (行已删除)                        ▲
                                    └─ 仍然在这里，直到永远（或直到保留期清扫）
```

payload 携带客户端无需再往返一次就能应用它所需的内容：

```json
{ "type": "message_deleted", "seq": 1841, "message_id": 4821, "folder_id": 5, "permanent": false }
```

`permanent: false` 表示「已移到回收站」；`permanent: true` 表示行已经消失。
这个区别很重要，因为客户端应当为前者保留正文，而可以丢弃后者的正文。

两种删除会产生墓碑，对应 `messages.deleted_at` 与
`messages.expunged_at`（[storage.md](storage.md) §2.3）：

| 事件 | 变更 `kind` | 墓碑？ |
|---|---|---|
| 设置 `\Deleted` / 不带 `permanent` 的 `DELETE` | `message_updated`（标志），如果进了回收站则还有 `message_moved` | 否，邮件仍然存在 |
| Expunge / `DELETE ?permanent=true` | `message_deleted` | **是** |

### 5.3 保留期

墓碑不会永久存在。保留期清扫拿 `client.tombstone_retention_days`（30）
与 `operations.created_at` 和 `change_log.created_at` 比较，
[fcp.md](fcp.md) §1 为幂等键记录了同一个值。

```rust
// ChangeLogRepository / OperationsRepository
pub async fn prune_older_than(&self, cutoff: DateTime<Utc>) -> Result<u64>
pub async fn purge_older_than(&self, cutoff: DateTime<Utc>) -> Result<u64>
```

`ChangeLogRepository::prune_older_than` 的文档里带了一条警告，
说清扫有责任知道什么时候才是安全的：**只修剪每个设备都已经越过的变更。**
修剪一个设备尚未读到的变更，会把该设备的下一次同步变成一个空洞，
而空洞会被当作损坏处理（§7）。安全的截止点是该用户所有
`client_sync_states` 行中游标的最小值，并以保留窗口为下限。

三十天是设计目标：一台关机一个月的桌面客户端回来时应当做一次重新同步，
而不是面对一个无声的空洞。

---

## 6. 游标过旧的恢复

当游标指向的变更已被修剪，或者用户的 `change_log` 在游标与当前之间
已经不再包含任何东西时，游标就过旧了。没有办法重建缺失的那段增量，
服务端也不能假装有办法。

契约（[fcp.md](fcp.md) §3 第 6 项）：

```http
GET /api/v1/client/sync?mailbox_id=3&folder_id=5&cursor=9
```

```http
HTTP/1.1 409 Conflict
Content-Type: application/json

{ "error": { "code": "conflict", "message": "cursor too old; full resync required" } }
```

客户端的应对：

```text
1. 停止该文件夹的增量同步
2. 把该文件夹的缓存标记为无效            （删除该文件夹的邮件行）
3. 把该文件夹的游标设为 0
4. 从 0 开始同步                        （首次同步，§3）
5. 用 GET /client/mailboxes 的 folders.message_count 显示同步进度
```

`409 conflict` 与 API 对重放的 `operation_id` 使用的状态码相同
（[api.md](api.md) §1.3），因此客户端必须按上下文区分：来自
`/sync` 的 `409` 意味着重新同步，其它任何来源都意味着把它暴露给用户。

### 6.1 空洞

正常运行时 `seq` 对每个用户都是无空洞的，但有一个合法的空洞来源：
**修剪**。看到空洞的客户端，比如 `1836` 之后是 `1851`，
不得试图对缺失的区间做推理。[fcp.md](fcp.md) §3
第 3 项给出了规则：丢弃游标，从 `0` 重新同步。

为什么不直接索要那段空洞？因为服务端可能恰好修剪掉了那些行，
请求会再次失败；也因为落后到那种程度的客户端本来就需要一次重新同步。
这个检查很便宜：

```text
if any(change.seq != previous_seq + 1 for consecutive changes): full resync
```

不检查的客户端会静默地漏掉空洞里的变更；这个检查之所以不是强制性的，
唯一原因是游标过旧的 `409` 覆盖了常见情况。

---

## 7. `uid_validity` 失效

UID 是文件夹范围的且不透明的（[imap.md](imap.md) §5）。`folders.uid_validity`
标识一个文件夹 UID 空间的*代际*，而缓存了 UID → 邮件映射的客户端
必须以 `(folder_id, uid_validity)` 为这个缓存建键，而不是只用
`folder_id`。

```text
   缓存: folder 5, uid_validity 1, uid 117 -> server message_id 4821
   服务端: SELECT 返回 "OK [UIDVALIDITY 2]"
        ⇒ folder 5 的每一个缓存 UID 都不再有意义
        ⇒ 丢弃该文件夹的 UID 索引（以 message_id 为键的邮件正文可以保留）
        ⇒ cursor = 0，重新同步
```

`GET /api/v1/client/mailboxes` 返回每个文件夹的 `uid_validity`
（[fcp.md](fcp.md) §4），因此客户端甚至可以在选中该文件夹之前就发现变化。

| 事件 | `uid_validity` | 客户端动作 |
|---|---|---|
| 正常运行 | 不变 | 什么都不做 |
| 文件夹被重命名 | 不变 | 什么都不做，同一个 UID 空间 |
| 邮件被 expunge | 不变 | 什么都不做 |
| 有邮件移入或移出 | 不变（目标文件夹的 `uid_next` 前进） | 应用 `message_moved` 变更 |
| 数据丢失后的一次重建 | 递增 | 丢弃该文件夹的缓存，重新同步 |
| `ferroma storage verify --repair` 给某文件夹重新编号 | 递增 | 丢弃该文件夹的缓存，重新同步 |
| 从早于某次重建的备份恢复 | 递增 | 丢弃该文件夹的缓存，重新同步 |

客户端的规则与 IMAP 的规则相同，理由也相同：UID 只有在与签发它的
`uid_validity` 一起看时才有意义。忽略这一点的客户端会撞上经典的 IMAP
bug，即服务端重建之后打开错误的邮件，而且它是静默的，
这正是值得说两遍的原因。

**服务端侧：** 只要某个操作有可能让旧的 UID → 邮件映射变错，就必须调用
`FoldersRepository::set_uid_validity`，其它情况下绝不调用。
一次多余的递增会让每个客户端都多做一次文件夹重新同步。

---

## 8. 通过 `operations` 实现幂等

### 8.1 规则

每个变更型的客户端请求都携带一个客户端生成的 `operation_id`（项目书 §55）。
服务端记录该操作，如果再次看到同一个 id，就重放原始的响应。

摘自 [fcp.md](fcp.md) §5：

```json
{ "operation_id": "op_9f2c41…", "type": "mark_read", "message_id": 4821 }
```

> 客户端应当在本地入队之前生成 id，绝不在发送时生成。

最后这句话就是整个设计。在发送时生成的 id 在每次重试时都是不同的 id，
因此它什么也保护不了。在用户操作时生成、并与待处理操作一起保存的 id，
在每次重试、每次重连和每次进程重启中都保持稳定。

`ferroma_core::OperationId::generate()` 生成 `op_` 加上去掉连字符的 UUID v4。

### 8.2 机制

```rust
// crates/ferroma-storage/src/repository/sync.rs
pub enum OperationOutcome {
    Fresh,          // 新 id：这份工作归调用方
    Replay(Operation),  // 之前见过：返回缓存的那一行
}

pub async fn begin(&self, operation_id: &str, user_id: Option<UserId>, kind: &str)
    -> Result<OperationOutcome>
```

`begin` 是一条语句：

```sql
INSERT INTO operations (operation_id, user_id, kind, status)
VALUES ($1, $2, $3, 'applied')
ON CONFLICT (operation_id) DO NOTHING
RETURNING *
```

原子性就是要点：对同一请求的两个并发重试中，恰好一个拿到 `Fresh`，
另一个拿到 `Replay`。不存在两者都以为自己最先的读后写窗口。如果
`RETURNING *` 什么都没返回，说明该行已存在，`find(operation_id)` 会把它读回来；
代码会把这次读取重试三次，然后以
`StorageError::Conflict("operation … was claimed but disappeared")` 放弃，
而这只可能在两条语句之间有清理运行时发生。

变更型请求完整的服务端序列：

```text
1. OperationsRepository::begin(op_id, user_id, kind)
       Fresh  -> 2
       Replay -> 原样返回 operation.result，并带上原始状态码
2. 做事（调用仓储层、调用邮件核心、写 Maildir）
3. 成功：OperationsRepository::complete(op_id, result_json)
   失败：OperationsRepository::fail(op_id, error_json)
4. 追加 change_log 行，发布事件
```

`complete` 与 `fail` 都写入 `operations.result`，也都设置
`completed_at`。失败的操作同样会被缓存：重放它会返回同样的错误，
而不是重新执行一次注定同样失败的调用。

### 8.3 HTTP 中 id 从哪来

| 入口 | 载体 |
|---|---|
| 客户端 API（FCP） | 请求体中的 `operation_id`（[fcp.md](fcp.md) §5） |
| 管理 API | `Idempotency-Key` 头（[api.md](api.md) §1.5） |

两者最终都进入同一列。头字段这种形式之所以存在，是因为浏览器端的
Webmail 客户端并不总能往请求体里放字段（`DELETE` 就没有请求体）。

### 8.4 幂等覆盖不到的情况

* **非变更型请求不需要 id**，带 id 的 `GET` 会被忽略。
* **id 不是锁。** 同一个逻辑操作的两个*不同* id 都会执行；幂等防的是重试，
  不是用户点了两次。客户端的工作是不为一个用户操作生成两个 id。
* **窗口是 `client.tombstone_retention_days`**（30）。早于该窗口的操作 id
  已被清理（`OperationsRepository::purge_older_than`），会被当作全新的。
  客户端重试一个月前的请求不是重试，而是重新发起，
  而重新发起正是用户要求的。
* **操作是按用户的。** `operations.user_id` 是行的一部分，
  因此用户 A 的 id 无法与用户 B 的 id 有意义地冲突；但主键只是那个 id 字符串，
  所以客户端应当按账号给自己的 id 加命名空间，`op_<uuid4>` 正是这样做的。

---

## 9. 冲突解决

### 9.1 标志与文件夹：最后写入者胜，以服务器时钟为准

两台设备同时把同一封邮件标为已读和未读。服务端应用后到的那个请求；
另一台设备在下一次同步时得知结果，因为每次变更都会追加一条变更日志条目。
这里没有合并，也没有向量时钟，对一种布尔标志而言这就是正确的答案：
用户最近的意图是唯一重要的事，而一次「冲突」不值得告诉他。

客户端的义务是**不要与服务端对抗**：应用了乐观的本地变更之后，
它不得在服务端状态不一致时重新发送它，因为待处理操作已被确认，
所以它已经从队列中移除，而到来的变更获胜。

### 9.2 草稿：最后写入者胜，并且会告诉客户端

草稿是唯一一处糟糕的合并会丢掉用户输入文本的地方，因此策略是明确的
（[fcp.md](fcp.md) §7）：

```json
{ "id": 44, "updated_at": "2026-09-16T12:00:01Z",
  "conflict": { "detected": true, "server_updated_at": "2026-09-16T11:59:58Z" } }
```

| 情形 | 行为 |
|---|---|
| 草稿在一台设备上编辑过，没有其它编辑 | 正常更新 |
| 草稿在两台设备上编辑过，后到的编辑带的 `updated_at` 比已存储的那一行更旧 | 已存储的那一行获胜，响应中会说明这一点 |
| 客户端看到 `conflict.detected` | 必须把它暴露出来：把用户的版本作为副本留在本地，或者提示用户。不得静默重试 |
| 草稿在服务端被删除、在客户端被编辑 | `PATCH` 返回 `404 not_found`；客户端应当提议从本地副本重新创建 |

选择最后写入者胜而不是合并，是因为富文本正文的两次并发编辑没有正确的自动合并。
`conflict` 对象让它变得可以接受：输掉的那次写入不会被静默丢弃，
而是被通知到。

检测使用行中的 `updated_at`，因此比较是服务器时钟对服务器时钟，
不涉及任何设备的时钟偏差。

### 9.3 发送

离线时发出的邮件进入发件箱，并在网络恢复后发出。如果同一份草稿也从另一台设备
发出过，两者都会发出：服务端无从知道它们是「同一封」邮件，
而抑制其中一封比重复更糟。`operation_id` 防止的是同一次发送的*重试*
产生两封，而那才是实际会发生的故障。

### 9.4 永不合并的内容

| 实体 | 策略 |
|---|---|
| 邮件的存在性 | 服务端绝对获胜。墓碑即删除 |
| 邮件标志 | 最后写入者胜 |
| 文件夹归属 | 最后写入者胜；任一方向的 `message_moved` 都会被应用 |
| 文件夹的存在性与名称 | 服务端获胜；`folder_deleted` 在本地删除它 |
| 草稿内容 | 最后写入者胜，并报告冲突 |
| 账号设置 | 服务端获胜，从 `GET /client/account` 重新同步 |
| 仅本地的设置（窗口大小、主题、缓存预算） | 从不同步 |

---

## 10. 离线发件箱

项目书 §27 与 §28。发件箱保存的是*待处理操作*，不是待发邮件，
一次发送只是操作的一种。

```text
   撰写 ──► 本地发件箱行（state = draft 或 pending）
                    │
                    │ 网络可用
                    ▼
              同步引擎 ──► 带 operation_id 的 FCP 请求
                    │
                    ├── 2xx  ──► 删除该行（或标记为已发送）
                    ├── 4xx  ──► 标记为失败，保留该行，暴露给用户
                    └── 5xx / 离线 ──► 带退避重试，保留该行
```

项目书 §28 中的**八个状态**：

```text
Draft   Pending   Uploading   Queued   Sending   Sent   Failed   Retrying
```

这两套词汇，即客户端的发件箱状态与服务端的
`mail_queue.status`，是相关但不相同的，因为「Uploading」与
「Retrying」是客户端侧的事实：

| 客户端状态 | 含义 | 服务端对应物 |
|---|---|---|
| `Draft` | 已撰写，尚未排入发件箱 | 无（如果保存过，则有一行 `drafts`） |
| `Pending` | 已在本地排队，等待同步引擎把它取走 | 无 |
| `Uploading` | 附件正在（分块）传输 | `/attachments/chunk` 的进度 |
| `Queued` | 服务端已接受：`POST /client/messages` 返回 `{message_id, queued}` | `mail_queue.status = 'pending'`，每个收件人一行 |
| `Sending` | 服务端正在投递给远端 MX | `mail_queue.status = 'delivering'` |
| `Sent` | 每个收件人都已投递 | `mail_queue.status = 'delivered'` |
| `Retrying` | 一次投递尝试临时失败；服务端会再试一次 | `mail_queue.status = 'retry'`、`next_attempt_at` |
| `Failed` | 永久失败，或尝试次数已耗尽 | `mail_queue.status = 'failed'` |

由客户端驱动与由服务端驱动的状态转换：

```text
   客户端驱动:  Draft → Pending → Uploading → Queued
                   Pending/Uploading → Failed        （本地校验，413）
                   Retrying → Sending → Sent         （在一次同步之后）

   服务端驱动:  Queued → Sending → Sent
                   Queued/Sending → Retrying → Sending → …
                   Queued/Sending/Retrying → Failed
```

客户端从两个来源得知服务端驱动的状态转换，并且必须把它们视为等价：

1. **同步变更**，变更流中的 `delivery.updated`，带有
   `queue_id`、`recipient`、`status`、`attempts`、`last_error`
   （`crates/ferroma-events/src/event.rs` 中的 `DeliveryUpdated`）。
2. **实时**，通过 WebSocket 送达的同一个事件（[fcp.md](fcp.md) §8）。

因为套接字是一种优化而游标是事实来源，所以错过了
`delivery.updated` 帧的客户端仍会在下一次同步时得知。
只监听套接字的客户端会永远把某封邮件显示为 `Sending`。

发件箱规则：

* **待处理操作只有在服务端确认之后才会被移除**，确认形式是针对其
  `operation_id` 的 `2xx` 或重放的缓存响应。超时绝不是确认。
* **用户输入的任何内容都不得因网络错误而丢失**（[fcp.md](fcp.md)
  §11）。附件在发送之前上传，其 id 存放在待处理行中，
  因此重启是续传而不是重新上传。
* **`5xx` 与网络错误采用带抖动的指数退避**，上限由客户端自己的策略决定。
  服务端的 `Retry-After` 存在时优先。
* **`4xx` 对该操作是终局**（`429` 除外，那是限流）。
  `413 limit_exceeded` 意味着邮件太大，重试帮不上忙，
  客户端保留草稿并告诉用户。
* **顺序没有保证。** 两个排队的操作可能在一次重试之后乱序应用。
  每个操作在编写时都可以独立应用，这正是变更携带绝对状态的原因（§4）。

---

## 11. 失败矩阵

按错误类别说明客户端与排队中的工作会发生什么。状态码与错误码取自
[api.md](api.md) §1.3 与 [fcp.md](fcp.md) §11。

| 类别 | 服务端响应 | 客户端必须 | 发件箱 | 游标 |
|---|---|---|---|---|
| **离线 / DNS 失败 / 连接重置** | 无 | 在本地排队，带退避与抖动重试，绝不丢弃 | 保留 | 不变 |
| **`401 unauthorized`** | `401` | 刷新一次访问令牌，重试一次。第二次 `401` ⇒ 登出，保留缓存与发件箱 | 跨登出保留 | 不变 |
| **`403 forbidden`** | `403` | 暴露给用户；不要重试 | 标记为失败 | 不变 |
| **`404 not_found`** | `404` | 目标已不存在。丢弃该操作，在下一次同步时应用服务端的状态 | 移除 | 不变 |
| **来自 `/sync` 的 `409 conflict`** | `409` | 游标过旧 ⇒ 丢弃该文件夹的缓存，从 `0` 同步（§6） | 不受影响 | 重置为 `0` |
| **来自某个变更的 `409 conflict`** | `409` | 一个被重放的 `operation_id`（本应重放缓存响应），或是一次状态违规 ⇒ 暴露给用户 | 标记为失败 | 不变 |
| **`413 limit_exceeded`** | `413` | 告诉用户什么太大了；保留草稿 | 从发件箱移除，草稿保留 | 不变 |
| **`426 unsupported`** | `426` | 拒绝运行；提示升级。低于 `client.min_protocol_version` 的客户端无法被服务 | 冻结 | 冻结 |
| **`429 rate_limited`** | `429` + `Retry-After` | 严格遵循 `Retry-After`；在本地排队 | 保留 | 不变 |
| **`500 storage_error` / `internal_error`** | `500` | 当作临时错误；退避；绝不丢失该操作 | 保留 | 不变 |
| **`502 dns_error` / `network_error` / `timeout`** | `502` | 当作临时错误；退避 | 保留 | 不变 |
| **WebSocket 断开** | — | 重连，然后在信任套接字之前**先从已存储的游标同步** | 不受影响 | 不变 |
| **`{"replay_gap": true}`** | WS 帧 | 事件总线为该订阅者丢弃了事件 ⇒ 立即运行一次同步 | 不受影响 | 不变 |
| **`device.revoked` / 吊销之后的 `401`** | WS 帧 + `401` | 清除令牌，停止同步，保留缓存，显示「此设备已被登出」 | 冻结 | 冻结 |
| **应用在发送中途被杀** | — | 重启后从发件箱继续。已上传的附件按 id 引用，因此不会重发 | 继续 | 不变 |
| **应用在同步中途被杀** | — | 该页的事务要么已提交（游标已推进），要么没有（重新抓取该页）。幂等的处理器让两者都安全 | 不受影响 | 一致 |
| **检测到 `seq` 中的空洞** | — | 丢弃游标，从 `0` 重新同步（§6.1） | 不受影响 | 重置为 `0` |
| **`uid_validity` 变化** | — | 丢弃该文件夹的 UID 索引，重新同步该文件夹（§7） | 不受影响 | 该文件夹重置为 `0` |
| **设备之间的时钟偏差** | — | 无关紧要：冲突按服务器时钟解决（§9.2） | 不受影响 | 不变 |

最重要的一行是第一行：**用户输入的任何内容都不得因网络错误而丢失。**
这张表里其它每一条策略都从属于它，也正是因为如此，
待处理操作和草稿会一直留在客户端的 SQLite 数据库中，直到服务端确认它们；
也正是因为如此，幂等键是在用户操作时生成，而不是在请求发出时生成。

---

## 12. 相关文档

| 主题 | 文档 |
|---|---|
| FCP 线上格式：端点、游标、分帧、分块上传 | [fcp.md](fcp.md) |
| HTTP 管理接口 | [api.md](api.md) |
| 事件名称、作用域、重放、进程内限制 | [architecture.md](architecture.md) §6 |
| 表结构：`change_log`、`operations`、`client_sync_states` | [storage.md](storage.md) §2.7 |
| IMAP 一侧的 UID 与 UIDVALIDITY 语义 | [imap.md](imap.md) §5 |
| 客户端的同步引擎、SQLite 缓存、发件箱 UI | [client.md](client.md) |
| 队列状态、重试计划、退信 | [smtp.md](smtp.md) §11 |
