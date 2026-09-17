# Ferroma 客户端协议（FCP）v1

FCP 是 Ferroma Server 与官方 Ferroma 客户端之间的契约。

它**不**取代 IMAP 或 SMTP。第三方客户端，Thunderbird、Apple Mail、
Outlook、手机端，继续使用 IMAP 与 SMTP，并且是一等公民
（项目书 §53）。FCP 之所以存在，是因为这些协议无法表达官方客户端
所需的内容：带可续传游标的增量同步、服务端草稿、
设备管理、分块附件传输与实时推送。

```text
                    Ferroma
                       │
          ┌────────────┼────────────┐
          ▼            ▼            ▼
     Client API       IMAP         SMTP
          │            │            │
          ▼            ▼            ▼
   Official client  Thunderbird   Outlook
                    Apple Mail
```

* 基础路径：`/api/v1/client`
* 传输：HTTPS（开发期可在可信代理之后使用纯 HTTP）
* 媒体类型：`application/json; charset=utf-8`
* 实时：`GET /api/v1/client/events`升级为 WebSocket
* 参考实现：`ferroma-api::client`（服务端）、`ferroma-client::api`（客户端）

---

## 1. 版本协商

客户端在每次请求中声明自身：

```http
X-Ferroma-Client: FerromaClient/0.7.0
X-Ferroma-Protocol: 1
X-Ferroma-Platform: windows
```

服务端在应答中返回它实际使用的协议：

```http
X-Ferroma-Protocol: 1
X-Ferroma-Server: 0.1.0
```

| 情形 | 服务端行为 |
|---|---|
| `X-Ferroma-Protocol` ≥ `client.min_protocol_version` | 正常工作 |
| 低于最低版本 | `426 Upgrade Required`，响应体 `{"error":{"code":"unsupported",…}}` |
| 头字段缺失 | 视为协议 `1`（为 `curl` 与监控提供的便利） |
| 高于 `client.protocol_version` | 照常服务，并把 `X-Ferroma-Protocol` 设为服务端版本；客户端必须优雅降级 |

`GET /api/v1/client/account`返回协商后的取值，
供客户端记录：

```json
{
  "user": { "id": 7, "email": "alice@example.com", "display_name": "Alice" },
  "mailboxes": [ { "id": 3, "address": "alice@example.com", "is_primary": true } ],
  "protocol_version": 1,
  "min_protocol_version": 1,
  "server_version": "0.1.0",
  "server_hostname": "mail.example.com",
  "limits": { "max_message_size": 26214400, "max_recipients": 100, "attachment_chunk_size": 1048576, "sync_page_size": 500 },
  "features": ["sync", "events", "drafts", "attachments", "devices", "search"]
}
```

`features`是客户端在不提升版本号的前提下发现可选能力的方式。

---

## 2. 认证

```text
POST /api/v1/client/auth/login     { email, password, device }
POST /api/v1/client/auth/refresh   { refresh_token, device_uid }
POST /api/v1/client/auth/logout    { refresh_token? }
GET  /api/v1/client/account
```

`device`标识该安装并完成注册：

```json
{
  "device_uid": "3f2c…",           // 由客户端生成，对该安装保持稳定
  "name": "Alice's laptop",
  "platform": "windows",           // windows | linux | macos | android | ios
  "client_version": "0.7.0"
}
```

登录返回与管理 API 相同的令牌对结构，外加设备 id：

```json
{
  "access_token": "eyJ…",
  "refresh_token": "rt_…",
  "token_type": "Bearer",
  "expires_in": 3600,
  "device_id": 12,
  "user": { "id": 7, "email": "alice@example.com" }
}
```

规则：

* 访问令牌有效期为 1 小时。客户端在收到`401`时刷新一次，然后重试。
* **刷新令牌会轮换。**每次刷新都会返回一个新令牌并使旧令牌失效。
  出示已使用过的刷新令牌会吊销整个令牌族，
  并返回`401`，这正是令牌被盗的信号。
* 每次登录都会写入一行`devices`记录，但不发布任何事件；设备*吊销*
  会发布`device.revoked`，使该设备的其他会话丢弃自己的令牌。
* 被吊销或已过期的设备在每个端点上都会收到`401`，包括同步端点。

注销会吊销会话。客户端丢弃两个令牌，并在用户要求时
清除本地缓存。

---

## 3. 同步游标

这是 FCP 的核心。

**服务端是唯一事实来源**（项目书 §55）。客户端按文件夹持有一份缓存
和一个游标；它从不重新下载整个邮箱。

游标是一个不透明字符串。目前它是
`change_log`行的十进制序号，但客户端不得解析它，它只会被原样回传。

### `GET /api/v1/client/sync?mailbox_id=3&folder_id=5&cursor=0&limit=500`

| 参数 | 含义 |
|---|---|
| `mailbox_id` | 哪个地址（必填） |
| `folder_id` | 单个文件夹；省略表示账号级变更（文件夹列表、草稿、设置） |
| `cursor` | 客户端成功应用的上一个游标；首次同步时为`0`（或省略） |
| `limit` | 最多返回的变更数，上限为`client.sync_page_size` |

应答：

```json
{
  "next_cursor": "1841",
  "has_more": true,
  "changes": [
    { "type": "message_created", "seq": 1836, "message_id": 4821, "uid": 117 },
    { "type": "message_updated", "seq": 1837, "message_id": 4821, "flags": "seen" },
    { "type": "message_deleted", "seq": 1838, "message_id": 4712 },
    { "type": "message_moved",   "seq": 1839, "message_id": 4700, "from_folder_id": 5, "to_folder_id": 6 },
    { "type": "folder_created",  "seq": 1840, "folder_id": 6, "name": "Archive/2026" }
  ]
}
```

### 契约

1. **先应用，再推进。**客户端按顺序、持久地应用每一条变更，
   然后才存储`next_cursor`。存储前崩溃，同一页会被再次拉取；
   因此每条变更在构造上都是幂等的。
2. **持续分页，直到`has_more`为 false。**绝不要假定一个应答就是全部增量。
3. **顺序按`seq`升序，且对每个用户无缺口。**客户端一旦发现
   缺口，就必须丢弃自己的游标并从`0`重新同步。
4. **先元数据，正文按需获取。**`message_created`携带 id 与标志，而不携带
   邮件本身。客户端用`GET /api/v1/client/messages/:id`（或`…/raw`）
   惰性获取正文。
5. **删除是墓碑。**即使数据行可能已经不存在，仍会发出`message_deleted`变更；
   墓碑的保存时间长于邮件本身，为
   `client.tombstone_retention_days`。
6. **过期游标是可恢复的。**早于保留窗口的游标会收到
   `409 conflict`，以及`{"error":{"code":"conflict","message":"cursor too old; full resync required"}}`。
   客户端对此的响应是丢弃该文件夹的缓存，并从`0`同步。
7. **首次同步就是从游标`0`开始的普通同步。**为展示进度，客户端
   会把已应用的变更数与`GET /api/v1/client/mailboxes`
   所报告的该文件夹`message_count`进行比较。

### 跨文件夹的顺序保证

`seq`对每个用户全局唯一，因此同步多个文件夹的客户端可以为每个文件夹使用一个游标，
仍能看到一致的相对顺序。把出现在目标文件夹事件流中的`message_moved`
与来自源文件夹事件流的`message_deleted`合并，
两种顺序都是安全的，因为二者都以`message_id`为键。

---

## 4. 邮箱与文件夹

```text
GET /api/v1/client/mailboxes
```

```json
{
  "mailboxes": [
    {
      "id": 3, "address": "alice@example.com", "display_name": "Alice", "is_primary": true,
      "folders": [
        { "id": 5, "name": "INBOX",   "special_use": null,      "message_count": 412, "unseen_count": 3, "uid_validity": 1, "uid_next": 118 },
        { "id": 6, "name": "Sent",    "special_use": "\\Sent",  "message_count": 152, "unseen_count": 0, "uid_validity": 1, "uid_next": 153 },
        { "id": 7, "name": "Drafts",  "special_use": "\\Drafts","message_count": 2,   "unseen_count": 0, "uid_validity": 1, "uid_next": 3 },
        { "id": 8, "name": "Trash",   "special_use": "\\Trash", "message_count": 9,   "unseen_count": 0, "uid_validity": 1, "uid_next": 10 },
        { "id": 9, "name": "Junk",    "special_use": "\\Junk",  "message_count": 1,   "unseen_count": 1, "uid_validity": 1, "uid_next": 2 },
        { "id": 10, "name": "Archive","special_use": "\\Archive","message_count": 39, "unseen_count": 0, "uid_validity": 1, "uid_next": 40 }
      ]
    }
  ]
}
```

客户端通过映射`special_use`而不是猜测名称，因此 Sent 文件夹
名为`Sent Items`的账号依然能正确对应。

当服务端对文件夹重新编号时（重建、迁移），`uid_validity`会变化。
客户端若发现同一个文件夹 id 的`uid_validity`不同，必须丢弃
该文件夹的缓存并重新同步，它的 UID 缓存已无意义。

---

## 5. 邮件

```text
GET    /api/v1/client/messages?mailbox_id=3&folder_id=5&limit=50&offset=0
GET    /api/v1/client/messages/:id            # 元数据 + 头部 + 摘要片段
GET    /api/v1/client/messages/:id/raw        # RFC 5322 原始字节
POST   /api/v1/client/messages                # 发送
PATCH  /api/v1/client/messages/:id            # { seen?, flagged?, answered?, deleted? }
DELETE /api/v1/client/messages/:id            # ?permanent=true
POST   /api/v1/client/messages/:id/read
POST   /api/v1/client/messages/:id/unread
POST   /api/v1/client/messages/:id/star
POST   /api/v1/client/messages/:id/archive
POST   /api/v1/client/messages/:id/move       # { folder_id }
POST   /api/v1/client/messages/:id/trash
```

列表项结构只含头部与标志，不含正文：

```json
{
  "items": [
    {
      "id": 4821, "uid": 117, "folder_id": 5,
      "subject": "Invoice for September",
      "from": { "address": "bob@example.net", "name": "Bob" },
      "to": [ { "address": "alice@example.com", "name": null } ],
      "snippet": "Hi Alice, attached is the invoice for September…",
      "flags": "seen",
      "size_bytes": 24831,
      "has_attachments": true,
      "attachment_count": 1,
      "internal_date": "2026-09-16T09:12:44Z",
      "sent_at": "2026-09-16T09:12:31Z",
      "rfc_message_id": "<20260916091231.7f3a@example.net>"
    }
  ],
  "total": 412, "limit": 50, "offset": 0
}
```

完整邮件额外包含`text_body`、`html_body`、`attachments[]`与原始头字段列表。
当`security.sanitize_html`打开时，`html_body`会在服务端做净化处理。

### 离线操作

每个会改变状态的请求都携带一个由客户端生成的操作 id：

```json
{ "operation_id": "op_9f2c41…", "type": "mark_read", "message_id": 4821 }
```

服务端会记录该操作，再次见到同一个 id 时重放原始应答，
因此超时后的重试，或客户端在请求中途崩溃后的重试，
都不会重复生效。这正是发件箱（Outbox）安全的原因（项目书 §55）。

客户端应在本地入队之前生成 id，绝不要在发送时才生成。

---

## 6. 附件

元数据与小文件：

```text
POST   /api/v1/client/attachments        multipart/form-data
GET    /api/v1/client/attachments/:id    streams bytes; supports Range and ETag
```

对于大文件，或要续传被中断的上传：

```text
POST   /api/v1/client/attachments/init       { filename, content_type, size_bytes }
   -> { attachment_id, chunk_size, upload_token }
PUT    /api/v1/client/attachments/:id/chunk?index=N   (原始字节，除最后一块外都精确为 chunk_size)
POST   /api/v1/client/attachments/:id/complete        { sha256 }
   -> { id, filename, content_type, size_bytes, sha256 }
```

`complete`会校验摘要；不匹配则返回`409 conflict`并丢弃
该上传。分块可以乱序到达，也可以重试；服务端维护一份
已接收分块的位图。`GET /api/v1/client/attachments/:id/status`会报告
它已持有哪些分块，因此崩溃后续传的客户端只需上传缺口部分。

下载是内容寻址的，因此`ETag`就是该二进制对象的 SHA-256，同一文件的
重复下载永远不会重传字节。

---

## 7. 草稿

```text
POST   /api/v1/client/drafts        { subject?, text?, html?, to[], cc[], bcc[], in_reply_to?, references[], attachment_ids[] }
GET    /api/v1/client/drafts
PATCH  /api/v1/client/drafts/:id
DELETE /api/v1/client/drafts/:id
```

草稿存放在服务端，因此同一个草稿会出现在每台设备上。服务端把内容
以 JSON 存储，并把它作为一封真实邮件（带`\Draft`标志）镜像到
该邮箱的 Drafts 文件夹，让 IMAP 客户端也能看到。从任一界面删除草稿
都会把它从两边一并移除。

冲突策略：后写入者胜出，并会告知客户端它覆盖了什么：

```json
{ "id": 44, "updated_at": "2026-09-16T12:00:01Z", "conflict": { "detected": true, "server_updated_at": "2026-09-16T11:59:58Z" } }
```

---

## 8. 实时事件

```text
GET /api/v1/client/events?cursor=1841     Upgrade: websocket
```

该套接字使用同一个 bearer 令牌认证。连接时客户端可以传入
`cursor`以重放它错过的事件：

```json
{ "type": "hello", "protocol_version": 1, "heartbeat_secs": 30, "last_seq": 1841 }
```

随后是一串帧，每帧一个事件：

```json
{ "seq": 1842, "id": "0f4c…", "at": "2026-09-16T12:00:00Z", "scope": "user:7",
  "type": "mail.received", "mailbox_id": 3, "message_id": 4822, "from": "bob@example.net",
  "subject": "Re: Invoice", "snippet": "Thanks, got it." }
```

事件名称：`mail.received`、`mail.sent`、`mail.deleted`、`mail.read`、
`mail.flag_changed`、`mail.moved`、`draft.created`、`draft.updated`、
`delivery.updated`、`device.revoked`。

客户端义务：

* 对`ping`帧应以`pong`应答；当`heartbeat_secs`（默认 30 秒）
  内没有收到任何内容时，应发送`ping`。
* 重新连接后，先按已存储的游标同步，再重新信任套接字。
  套接字只是优化；**游标才是唯一事实来源。**中断期间事件可能丢失，
  它们会由同步重放。
* 收到`{"replay_gap": true}`表示事件总线丢弃了该订阅者的事件；
  立即执行一次同步。

---

## 9. 设备

```text
GET    /api/v1/client/devices
DELETE /api/v1/client/devices/:id
POST   /api/v1/client/devices/:id/revoke
```

```json
{
  "devices": [
    { "id": 12, "device_uid": "3f2c…", "name": "Alice's laptop", "platform": "windows",
      "client_version": "0.7.0", "protocol_version": 1,
      "last_seen_at": "2026-09-16T12:00:00Z", "last_ip": "203.0.113.44", "created_at": "2026-08-01T10:00:00Z", "revoked": false }
  ]
}
```

吊销设备：

1. 将该设备标记为已吊销，
2. 吊销属于它的所有会话，
3. 发布`device.revoked`，使该设备的在线套接字断开连接。

被吊销的客户端必须清除自己的令牌与缓存；它的下一个请求会收到`401`。
吊销你当前正在发起调用的设备是被允许的，并立即生效。

---

## 10. 服务端搜索回退

本地搜索是客户端的职责（项目书 §30），且必须离线可用。当本地索引
没有答案时，客户端向服务端查询，由服务端搜索相同的
字段：

```text
GET /api/v1/client/search?q=from:bob+subject:invoice+has:attachment+after:2026-01-01&mailbox_id=3&folder_id=5&limit=50
```

运算符集合为`from:`、`to:`、`subject:`、`body:`、`has:attachment`、
`is:unread`、`is:flagged`、`before:`、`after:`、`folder:`。结果是邮件列表
项，按时间从新到旧排列。

---

## 11. 客户端的错误处理规则

| 状态码 | `code` | 客户端必须做什么 |
|---|---|---|
| 401 | `unauthorized` | 刷新一次、重试一次；第二次收到 401 时注销并保留缓存 |
| 403 | `forbidden` | 呈现给用户；不要重试 |
| 409 | `conflict` | 若来自`sync`，对该文件夹做全量重新同步；否则呈现给用户 |
| 413 | `limit_exceeded` | 告诉用户哪部分过大；保留草稿 |
| 426 | `unsupported` | 拒绝运行；提示升级 |
| 429 | `rate_limited` | 遵守`Retry-After`；本地排队并重试 |
| 5xx / 网络 | — | 视为临时故障，保留待处理操作，带抖动地指数退避，绝不丢失它们 |

**用户输入的任何内容都不应因网络错误而丢失。**待处理操作与
草稿会一直留在本地数据库中，直到服务端确认它们。
