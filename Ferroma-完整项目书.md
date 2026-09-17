# Ferroma v1.0 完整项目书

**项目名称：** Ferroma  
**项目定位：** 基于 Rust 的完整自建邮件平台 + 官方跨平台客户端生态  
**技术路线：** Rust + Tokio + Axum + PostgreSQL + Mail Storage + Docker，并为官方客户端提供统一 Client API、增量同步协议与实时事件体系  
**文档版本：** v1.0  
**编写日期：** 2026-09-16  
**文档状态：** 技术规划 / 开发规格书

---

# 1. 项目概述

Ferroma 是一个面向个人、团队及小型组织的自建邮件平台。

v1.0 在原有纯 Rust 邮件服务器设计的基础上，将项目正式扩展为：

> **邮件服务器核心 + Webmail + 管理后台 + 官方跨平台客户端 + 统一客户端通信与同步体系**

项目仍以 Rust 为核心开发语言，以 Tokio 异步运行时为基础，自主实现 SMTP、IMAP、邮件存储、邮件队列、SMTP 出站投递、用户认证、HTTP API、Webmail 等核心能力，并通过 Docker 提供标准化部署方式。

项目不依赖 Stalwart、Postfix、Dovecot 等成熟邮件服务器作为邮件核心。

同时，Ferroma v1.0 将官方客户端视为平台的一等组成部分，而不是单纯依赖 IMAP/SMTP 的第三方客户端。

最终形成：

```text
                         Ferroma Platform
                               │
          ┌────────────────────┼────────────────────┐
          │                    │                    │
       Webmail            Official Clients      Admin Panel
          │                    │                    │
          └────────────────────┼────────────────────┘
                               │
                         Client / HTTP API
                               │
                    ┌──────────▼──────────┐
                    │     Ferroma Core    │
                    └──────────┬──────────┘
                               │
        ┌──────────┬───────────┼───────────┬──────────┐
        ▼          ▼           ▼           ▼          ▼
      SMTP       IMAP       Storage      Queue      DNS
        │          │           │           │          │
        └──────────┴───────────┴───────────┴──────────┘
                               │
                         PostgreSQL
```

---

# 2. 项目目标

## 2.1 平台目标

Ferroma 应支持：

1. 多域名。
2. 多邮箱账户。
3. SMTP 收信。
4. SMTP 出站投递。
5. IMAP 邮件访问。
6. Webmail。
7. 管理后台。
8. TLS。
9. SPF、DKIM、DMARC。
10. 邮件队列与失败重试。
11. 邮件存储与备份。
12. Docker 部署。
13. HTTP API。
14. 官方桌面客户端。
15. 多账户客户端。
16. 客户端增量同步。
17. 离线缓存。
18. 实时邮件事件。
19. 附件同步。
20. 草稿与 Outbox。
21. 设备管理。
22. 桌面通知。
23. 为 Android / iOS 预留客户端架构。

## 2.2 核心原则

开发顺序保持：

```text
协议正确性
      ↓
稳定性
      ↓
安全
      ↓
客户端兼容性
      ↓
功能完整
      ↓
性能优化
```

---

# 3. 非目标

第一阶段不追求完整覆盖所有邮件 RFC。

以下能力作为后续版本：

- 完整 IMAP 扩展生态
- OAuth 1.0 / OIDC
- Sieve
- CalDAV
- CardDAV
- 高级反垃圾系统
- 大规模集群
- 分布式邮件存储
- 多节点高可用
- 企业级全文搜索
- AI 邮件分类

第一版重点仍然是：

> 稳定完成邮件收取、存储、读取、发送以及基本客户端兼容。

---

# 4. 产品形态

Ferroma v1.0 包含四个主要产品：

```text
Ferroma
│
├── Ferroma Server
│
├── Ferroma Webmail
│
├── Ferroma Admin
│
└── Ferroma Client
```

## 4.1 Ferroma Server

负责：

- SMTP Server
- SMTP Client
- IMAP Server
- Mail Core
- Mail Storage
- Mail Queue
- Delivery
- Authentication
- DNS
- TLS
- HTTP API
- Event Bus
- Sync Service
- Logging
- Monitoring

## 4.2 Ferroma Webmail

浏览器邮件客户端。

## 4.3 Ferroma Admin

管理：

- 域名
- 用户
- 邮箱
- 别名
- 队列
- DNS
- TLS
- 系统
- 日志
- 存储
- 设备
- 客户端会话

## 4.4 Ferroma Client

官方客户端，目标平台：

```text
Windows
Linux
macOS
```

后续：

```text
Android
iOS
```

---

# 5. 总体架构

```text
                              Internet
                                  │
             ┌────────────────────┼────────────────────┐
             │                    │                    │
            SMTP                  IMAP                HTTPS
             │                    │                    │
             ▼                    ▼                    ▼
       ┌────────────────────────────────────────────────────┐
       │                    Ferroma Server                  │
       │                                                    │
       │  ┌─────────┐ ┌─────────┐ ┌─────────────────────┐  │
       │  │ SMTP    │ │ IMAP    │ │ HTTP / Client API  │  │
       │  │ Server  │ │ Server  │ │ Webmail / Admin    │  │
       │  └────┬────┘ └────┬────┘ └──────────┬──────────┘  │
       │       │            │                 │             │
       │       └────────────┼─────────────────┘             │
       │                    ▼                               │
       │             ┌──────────────┐                       │
       │             │  Mail Core   │                       │
       │             └──────┬───────┘                       │
       │                    │                               │
       │       ┌────────────┼────────────┐                  │
       │       ▼            ▼            ▼                  │
       │   Storage        Queue         Auth                │
       │       │            │            │                  │
       │       │            ▼            │                  │
       │       │        Delivery         │                  │
       │       │            │            │                  │
       │       └────────────┼────────────┘                  │
       │                    ▼                               │
       │               Event Bus                            │
       │                    │                               │
       └────────────────────┼────────────────────────────────┘
                            │
                 ┌──────────┼──────────┐
                 ▼          ▼          ▼
              Webmail    Client     Admin
```

---

# 6. 技术栈

## 6.1 服务端

| 技术 | 用途 |
|---|---|
| Rust | 核心开发语言 |
| Tokio | 异步运行时 |
| Axum | HTTP API |
| SQLx | PostgreSQL 数据访问 |
| Serde | 数据序列化 |
| Argon2id | 密码哈希 |
| rustls | TLS |
| Hickory DNS | DNS/MX 查询 |
| tracing | 日志与诊断 |
| thiserror | 错误类型 |
| anyhow | 应用级错误处理 |

## 6.2 数据库

推荐 PostgreSQL。

保存：

- 用户
- 域名
- 邮箱
- 邮件元数据
- 邮件队列
- 会话
- 设备
- 客户端同步状态
- 审计日志
- 系统配置

邮件正文和附件不建议全部直接存入数据库。

## 6.3 客户端

建议采用 Rust Client Core：

```text
Rust
├── Tokio
├── Reqwest
├── Serde
├── SQLx
├── SQLite
├── WebSocket
└── tracing
```

UI 可选：

```text
Slint
```

或者：

```text
Tauri + Web UI
```

如果项目强调 Rust 原生客户端，优先考虑 Rust + Slint。

---

# 7. Cargo Workspace

建议将项目从单一服务器工程升级为 Workspace：

```text
ferroma/
├── Cargo.toml
├── Cargo.lock
│
├── crates/
│   ├── ferroma-core/
│   ├── ferroma-mail/
│   ├── ferroma-smtp/
│   ├── ferroma-imap/
│   ├── ferroma-api/
│   ├── ferroma-storage/
│   ├── ferroma-auth/
│   ├── ferroma-sync/
│   └── ferroma-events/
│
├── server/
├── client/
├── web/
├── admin/
├── migrations/
├── docs/
├── scripts/
└── tests/
```

---

# 8. Ferroma Core

Mail Core 是平台的核心。

负责统一处理：

```text
Envelope
Message
Mailbox
User
Domain
Attachment
Flags
Delivery
Queue
```

建议定义：

```rust
struct MailMessage {
    id: MessageId,
    envelope: Envelope,
    headers: Headers,
    body: MailBody,
    attachments: Vec<Attachment>,
}
```

SMTP、IMAP、Webmail、Client API 均不得各自实现独立的邮件业务逻辑。

统一：

```text
Protocol Layer
      ↓
Mail Core
      ↓
Storage / Queue / Delivery
```

---

# 9. SMTP Server

## 9.1 MVP

支持：

```text
EHLO
HELO
MAIL FROM
RCPT TO
DATA
RSET
NOOP
QUIT
```

第二阶段：

```text
AUTH
STARTTLS
```

## 9.2 SMTP 状态机

```rust
enum SmtpState {
    Connected,
    Greeted,
    MailFrom,
    RcptTo,
    Data,
    Authenticated,
}
```

## 9.3 Session

```rust
struct SmtpSession {
    state: SmtpState,
    remote_addr: SocketAddr,
    helo: Option<String>,
    authenticated_user: Option<UserId>,
    envelope_from: Option<String>,
    recipients: Vec<String>,
}
```

## 9.4 Open Relay

默认策略：

```text
本地域名
    ↓
允许本地投递

外部域名
    ↓
必须认证
```

服务器绝不能成为 Open Relay。

---

# 10. SMTP Outbound

流程：

```text
SMTP / Webmail / Client API
             │
             ▼
         Mail Core
             │
             ▼
         Mail Queue
             │
             ▼
          DNS MX
             │
             ▼
        Remote MX
             │
             ▼
          TCP :25
             │
             ▼
       SMTP Delivery
```

需要：

- MX 查询
- 多 MX 主机
- 连接超时
- TLS
- 临时失败
- 永久失败
- 重试
- 投递日志

---

# 11. Mail Queue

状态：

```text
Pending
   │
   ▼
Delivering
   │
   ├── Success → Delivered
   │
   ├── Temporary Failure → Retry
   │
   └── Permanent Failure → Failed
```

默认重试策略可以采用：

```text
1 分钟
5 分钟
15 分钟
1 小时
6 小时
24 小时
```

实际策略根据错误类型进一步调整。

---

# 12. IMAP Server

第一版：

```text
CAPABILITY
LOGIN
LOGOUT
NOOP

LIST
LSUB

SELECT
EXAMINE
STATUS

FETCH
STORE

SEARCH
UID
```

后续：

```text
APPEND
COPY
MOVE
EXPUNGE
IDLE
```

目标：

> Thunderbird、Apple Mail、Outlook、iPhone Mail、Android 邮件客户端能够逐步兼容。

---

# 13. Mail Storage

推荐 Maildir：

```text
/var/lib/ferroma/
└── example.com/
    └── alice/
        └── Maildir/
            ├── cur/
            ├── new/
            └── tmp/
```

数据库保存：

```text
message_id
mailbox_id
storage_path
size
subject
sender
date
flags
```

文件系统保存：

```text
RFC 5322 message
raw MIME
attachments
```

---

# 14. MIME 系统

独立模块：

```text
MIME Parser
MIME Builder
Header Parser
Attachment Handler
```

需要处理：

- Plain Text
- HTML
- 图片
- 附件
- 多级 MIME
- 编码 Header
- MIME Boundary
- 内容编码

---

# 15. Authentication

密码：

```text
Argon2id
```

SMTP：

```text
AUTH PLAIN
AUTH LOGIN
```

IMAP：

```text
LOGIN
```

Web / Client：

```text
Session
Secure Cookie
Access Token
Refresh Token
```

后续：

```text
OAuth2
OIDC
2FA
```

---

# 16. DNS / SPF / DKIM / DMARC

DNS：

```text
A
AAAA
MX
TXT
PTR
```

内部模块：

```text
MxResolver
SpfChecker
DkimChecker
DmarcChecker
```

DKIM：

```text
Mail
 ↓
Canonicalization
 ↓
Header Hash
 ↓
Body Hash
 ↓
Signature
 ↓
DKIM-Signature
 ↓
SMTP Delivery
```

DMARC：

```text
p=none
p=quarantine
p=reject
```

管理后台提供 DNS Health：

```text
MX       ✓
A        ✓
AAAA     -
SPF      ✓
DKIM     ✓
DMARC    ✓
PTR      ✓
TLS      ✓
```

---

# 17. TLS

SMTP：

```text
25   SMTP
465  SMTPS
587  Submission
```

IMAP：

```text
143  IMAP + STARTTLS
993  IMAPS
```

使用：

```text
rustls
```

证书：

```text
Let's Encrypt
```

或者管理员提供：

```text
PEM certificate
PEM private key
```

---

# 18. HTTP API

基础：

```text
/api/v1
```

认证：

```text
POST /api/v1/auth/login
POST /api/v1/auth/logout
GET  /api/v1/auth/me
```

用户：

```text
GET    /api/v1/users
POST   /api/v1/users
GET    /api/v1/users/:id
PATCH  /api/v1/users/:id
DELETE /api/v1/users/:id
```

域名：

```text
GET    /api/v1/domains
POST   /api/v1/domains
DELETE /api/v1/domains/:id
GET    /api/v1/domains/:id/dns
```

消息：

```text
GET    /api/v1/messages
GET    /api/v1/messages/:id
POST   /api/v1/messages
PATCH  /api/v1/messages/:id
DELETE /api/v1/messages/:id
```

---

# 19. 官方 Client API

官方客户端使用独立的 Client API。

建议：

```text
/api/v1/client
```

## 19.1 Authentication

```text
POST /api/v1/client/auth/login
POST /api/v1/client/auth/refresh
POST /api/v1/client/auth/logout
GET  /api/v1/client/account
```

## 19.2 Mailboxes

```text
GET /api/v1/client/mailboxes
```

## 19.3 Messages

```text
GET    /api/v1/client/messages
GET    /api/v1/client/messages/:id
POST   /api/v1/client/messages
PATCH  /api/v1/client/messages/:id
DELETE /api/v1/client/messages/:id
```

## 19.4 Message Operations

```text
POST /api/v1/client/messages/:id/read
POST /api/v1/client/messages/:id/unread
POST /api/v1/client/messages/:id/star
POST /api/v1/client/messages/:id/archive
POST /api/v1/client/messages/:id/move
POST /api/v1/client/messages/:id/trash
```

## 19.5 Draft

```text
POST   /api/v1/client/drafts
GET    /api/v1/client/drafts
PATCH  /api/v1/client/drafts/:id
DELETE /api/v1/client/drafts/:id
```

## 19.6 Attachment

```text
GET  /api/v1/client/attachments/:id
POST /api/v1/client/attachments
```

---

# 20. Ferroma Client Protocol

Ferroma v1.0 建议定义：

> **Ferroma Client Protocol（FCP）**

FCP 不是取代 SMTP/IMAP，而是服务于官方客户端。

包括：

```text
Authentication
Synchronization
Events
Message Operations
Draft Operations
Attachment Operations
Device Management
Push Notification
```

结构：

```text
FCP
│
├── Authentication
├── Session
├── Mailbox State
├── Message State
├── Sync Cursor
├── Events
├── Attachments
└── Devices
```

---

# 21. 增量同步

客户端不应每次重新下载整个邮箱。

接口：

```text
GET /api/v1/client/sync
```

请求：

```json
{
  "mailbox": "inbox",
  "cursor": "abc123"
}
```

响应：

```json
{
  "next_cursor": "abc456",
  "changes": [
    {
      "type": "message_created",
      "id": "msg_123"
    },
    {
      "type": "message_updated",
      "id": "msg_456"
    },
    {
      "type": "message_deleted",
      "id": "msg_789"
    }
  ]
}
```

客户端保存：

```text
cursor
```

下一次只请求变化。

---

# 22. Event Bus

服务端增加统一事件系统：

```text
Mail Core
    │
    ▼
 Event Bus
    │
    ├── MailReceived
    ├── MailSent
    ├── MailDeleted
    ├── MailRead
    ├── MailFlagChanged
    ├── DraftCreated
    └── DeliveryUpdated
```

事件消费者：

```text
Webmail
Client
Admin
Notification
Webhook
```

---

# 23. WebSocket / 实时事件

官方客户端支持实时事件：

```text
Ferroma Server
      │
      │ WebSocket
      ▼
Ferroma Client
```

事件：

```text
mail.received
mail.updated
mail.deleted
mail.read
mail.flag_changed
draft.updated
delivery.updated
```

新邮件流程：

```text
SMTP Receive
     ↓
Mail Core
     ↓
Storage
     ↓
Event Bus
     ↓
WebSocket
     ↓
Client
     ↓
Desktop Notification
```

---

# 24. Ferroma Client

## 24.1 目标平台

第一阶段：

```text
Windows
Linux
macOS
```

第二阶段：

```text
Android
iOS
```

## 24.2 客户端架构

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

---

# 25. Client Core

建议目录：

```text
client/src/
├── main.rs
├── app/
├── account/
├── api/
├── sync/
├── mail/
├── draft/
├── outbox/
├── attachment/
├── search/
├── notification/
├── database/
├── device/
├── settings/
└── ui/
```

核心职责：

```text
Account Manager
API Client
Sync Engine
Local Database
Mail Cache
Attachment Cache
Search Engine
Notification
Outbox
```

---

# 26. 本地数据库

客户端使用 SQLite。

建议保存：

```text
accounts
mailboxes
messages
message_headers
attachments
drafts
outbox
sync_state
devices
settings
```

邮件正文可以按缓存策略保存。

原则：

```text
Server = Source of Truth

Client SQLite = Local Cache
```

---

# 27. 离线模式

客户端支持：

```text
查看已同步邮件
本地搜索
查看已缓存附件
写邮件
保存草稿
回复
删除
标记已读
```

离线操作进入：

```text
Pending Operations
```

网络恢复：

```text
Pending
   ↓
Sync Engine
   ↓
Server
   ↓
Success
```

---

# 28. Outbox

发送流程：

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

状态：

```text
Draft
Pending
Uploading
Queued
Sending
Sent
Failed
Retrying
```

客户端可以显示：

```text
发件箱

正在发送    2
发送失败    1
已发送    152
```

---

# 29. 附件系统

支持：

```text
流式上传
分块上传
断点续传
流式下载
下载进度
本地缓存
缓存清理
文件大小限制
MIME 类型
文件校验
```

客户端：

```text
Online
   ↓
Stream

Offline
   ↓
Local Cache
```

---

# 30. 本地搜索

客户端可以建立本地搜索索引。

支持类似：

```text
from:alice@example.com
subject:invoice
attachment:pdf
after:2026-01-01
```

策略：

```text
先搜索本地缓存
       ↓
没有结果
       ↓
请求服务器搜索
```

---

# 31. 多账户

客户端支持：

```text
Accounts
│
├── Personal
│   └── alice@example.com
│
├── Work
│   └── alice@company.com
│
└── Other
    └── test@example.org
```

功能：

```text
添加账户
删除账户
暂停同步
重新认证
修改账户
查看同步状态
```

---

# 32. 自动发现

支持：

```text
https://example.com/.well-known/ferroma
```

示例：

```json
{
  "api": "https://mail.example.com/api/v1",
  "imap": {
    "host": "mail.example.com",
    "port": 993,
    "tls": true
  },
  "smtp": {
    "host": "mail.example.com",
    "port": 587,
    "tls": true
  },
  "web": "https://webmail.example.com"
}
```

同时为未来兼容：

```text
autoconfig
autodiscover
```

---

# 33. 设备管理

API：

```text
GET    /api/v1/client/devices
DELETE /api/v1/client/devices/:id
POST   /api/v1/client/devices/:id/revoke
```

设备信息：

```text
Device ID
Device Name
Platform
Client Version
Last Seen
IP
Created At
```

---

# 34. 推送通知

桌面端：

```text
Windows Notification
Linux Notification
macOS Notification
```

移动端预留：

```text
APNs
FCM
```

架构：

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

---

# 35. Webmail

功能：

```text
登录
收件箱
已发送
草稿
垃圾箱
回收站
搜索
邮件阅读
附件下载
写邮件
回复
回复全部
转发
删除
标记已读
标记未读
```

Webmail 与官方客户端共享：

```text
Mail Core
API
Event Bus
Authentication
```

---

# 36. Admin Panel

管理员：

```text
Dashboard
Domains
Users
Mailboxes
Aliases
Mail Queue
Delivery Logs
DNS Diagnostics
System Logs
Storage
Devices
Settings
```

Dashboard：

```text
用户数量
域名数量
今日收件
今日发件
队列数量
失败邮件
磁盘使用率
系统状态
在线客户端
```

---

# 37. 数据库设计

核心表：

```text
users
domains
mailboxes
messages
mail_queue
sessions
devices
client_sync_states
audit_logs
```

## users

```sql
CREATE TABLE users (
    id BIGSERIAL PRIMARY KEY,
    email TEXT UNIQUE NOT NULL,
    password_hash TEXT NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    quota_bytes BIGINT NOT NULL DEFAULT 1073741824,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

## domains

```sql
CREATE TABLE domains (
    id BIGSERIAL PRIMARY KEY,
    name TEXT UNIQUE NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

## mailboxes

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

## messages

```sql
CREATE TABLE messages (
    id BIGSERIAL PRIMARY KEY,
    mailbox_id BIGINT NOT NULL REFERENCES mailboxes(id),
    message_id TEXT,
    sender TEXT,
    subject TEXT,
    size_bytes BIGINT NOT NULL,
    storage_path TEXT NOT NULL,
    flags TEXT NOT NULL DEFAULT '',
    received_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

## mail_queue

```sql
CREATE TABLE mail_queue (
    id BIGSERIAL PRIMARY KEY,
    message_id BIGINT NOT NULL REFERENCES messages(id),
    recipient TEXT NOT NULL,
    status TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

客户端相关表后续增加：

```text
devices
client_sync_states
```

---

# 38. 安全设计

## 网络层

限制：

```text
连接速率
并发连接
单 IP 连接数
SMTP 命令速率
邮件大小
收件人数量
```

## 账户层

```text
Argon2id
登录失败限制
Session 过期
账户禁用
管理员权限
Access Token
Refresh Token
```

## 邮件层

```text
Open Relay 防护
Sender 验证
Recipient 验证
SMTP AUTH
Spam Rate Limit
附件大小限制
```

## 客户端层

```text
Token 安全存储
本地数据库保护
敏感配置保护
设备撤销
Session Rotation
TLS 强制
```

---

# 39. 限制策略

```toml
[limits]

max_message_size = 26214400
max_recipients = 100
max_connections = 100
max_connections_per_ip = 10
smtp_rate_limit = 100
submission_rate_limit = 50
daily_send_limit = 500
mailbox_quota = 1073741824
```

---

# 40. 日志与可观测性

使用：

```text
tracing
tracing-subscriber
```

日志等级：

```text
ERROR
WARN
INFO
DEBUG
TRACE
```

SMTP 会话：

```text
connection_id
remote_ip
helo
authenticated_user
sender
recipient
message_id
result
duration
```

不得记录：

```text
密码
AUTH Token
私钥
完整邮件正文
```

后期：

```text
Prometheus
Grafana
```

指标：

```text
smtp_connections_total
smtp_messages_received_total
smtp_messages_rejected_total
smtp_delivery_success_total
smtp_delivery_failed_total
queue_pending_messages
queue_retry_messages
imap_connections
mail_storage_bytes
active_users
active_client_sessions
sync_operations_total
```

---

# 41. Docker

第一版：

```text
Docker Compose
│
├── ferroma
└── postgres
```

后期：

```text
redis
prometheus
grafana
```

端口：

```text
25
465
587
143
993
8080
```

生产环境需要：

- 健康检查
- 网络隔离
- 密钥管理
- 数据卷
- 备份
- 恢复策略

---

# 42. 域名规划

假设：

```text
example.com
```

建议：

```text
mail.example.com
webmail.example.com
admin.example.com
```

DNS：

```text
example.com MX mail.example.com
mail.example.com A SERVER_IP
```

以及：

```text
SPF
DKIM
DMARC
PTR
```

---

# 43. 测试策略

## 单元测试

```text
SMTP Parser
IMAP Parser
MIME Parser
Header Parser
DNS Parser
DKIM
SPF
DMARC
Mail Queue
Sync Protocol
FCP
```

## 集成测试

```text
SMTP → Storage
SMTP → Queue
Queue → SMTP Delivery
IMAP → Storage
HTTP API → Mail Core
Client API → Mail Core
Sync → SQLite
Event Bus → Client
```

## 客户端测试

```text
Windows
Linux
macOS
```

以及：

```text
Thunderbird
Apple Mail
Outlook
iPhone Mail
Android Mail
```

---

# 44. 协议兼容性测试

SMTP：

```text
openssl s_client
swaks
telnet
```

测试：

```text
EHLO
MAIL FROM
RCPT TO
DATA
AUTH
STARTTLS
```

IMAP：

```text
CAPABILITY
LOGIN
SELECT
FETCH
STORE
SEARCH
UID
IDLE
```

---

# 45. 性能目标

MVP：

```text
SMTP 并发连接：100+
IMAP 并发连接：100+
Web API：稳定处理常规并发
单邮件大小：25 MB
邮箱容量：可配置
```

官方客户端：

```text
首次同步：可显示同步进度
增量同步：只获取变化
本地搜索：优先本地
附件：支持流式传输
```

---

# 46. 备份

至少备份：

```text
PostgreSQL
Mail Storage
Configuration
DKIM Private Keys
TLS Certificates
```

建议：

```text
每日数据库备份
每日邮件存储增量备份
定期完整备份
```

必须实际测试恢复。

---

# 47. 灾难恢复

恢复：

```text
1. Docker
2. PostgreSQL
3. Mail Storage
4. Configuration
5. DKIM
6. TLS
7. Ferroma
8. DNS
```

目标：

```text
RPO：根据备份频率确定
RTO：根据服务器规模确定
```

第一版可以手工恢复。

---

# 48. 项目最终目录

```text
ferroma/
│
├── Cargo.toml
├── Cargo.lock
├── README.md
├── LICENSE
├── CHANGELOG.md
├── Dockerfile
├── docker-compose.yml
├── .env.example
│
├── crates/
│   ├── ferroma-core/
│   ├── ferroma-mail/
│   ├── ferroma-smtp/
│   ├── ferroma-imap/
│   ├── ferroma-api/
│   ├── ferroma-storage/
│   ├── ferroma-auth/
│   ├── ferroma-sync/
│   └── ferroma-events/
│
├── server/
│   └── src/
│       ├── main.rs
│       ├── config/
│       ├── smtp/
│       ├── imap/
│       ├── mail/
│       ├── mime/
│       ├── storage/
│       ├── queue/
│       ├── delivery/
│       ├── auth/
│       ├── dns/
│       ├── tls/
│       ├── api/
│       ├── sync/
│       ├── events/
│       ├── db/
│       └── observability/
│
├── client/
│   ├── Cargo.toml
│   ├── src/
│   │   ├── main.rs
│   │   ├── app/
│   │   ├── account/
│   │   ├── api/
│   │   ├── sync/
│   │   ├── mail/
│   │   ├── draft/
│   │   ├── outbox/
│   │   ├── attachment/
│   │   ├── search/
│   │   ├── notification/
│   │   ├── database/
│   │   ├── device/
│   │   ├── settings/
│   │   └── ui/
│   └── assets/
│
├── web/
├── admin/
├── migrations/
├── config/
├── scripts/
├── docs/
│   ├── architecture.md
│   ├── smtp.md
│   ├── imap.md
│   ├── storage.md
│   ├── api.md
│   ├── client.md
│   ├── fcp.md
│   ├── sync.md
│   ├── security.md
│   └── deployment.md
│
└── tests/
    ├── smtp/
    ├── imap/
    ├── delivery/
    ├── api/
    ├── client/
    ├── sync/
    └── integration/
```

---

# 49. 开发阶段

## Phase 1：基础框架

```text
Cargo Workspace
Tokio
Config
Logging
PostgreSQL
Docker
```

验收：

```text
docker compose up
```

## Phase 2：用户系统

```text
Users
Domains
Mailboxes
Authentication
Password Hash
```

## Phase 3：SMTP

```text
TCP
SMTP Parser
SMTP State Machine
Local Delivery
Maildir
```

里程碑：

> 两个本地邮箱可以通过 SMTP 正常互发邮件。

## Phase 4：SMTP Outbound

```text
MX Resolver
SMTP Client
Delivery Queue
Retry
Delivery Logs
```

里程碑：

> 本地邮箱能够向外部邮件服务器投递邮件。

## Phase 5：IMAP

```text
LOGIN
SELECT
LIST
FETCH
STORE
SEARCH
UID
```

里程碑：

> Thunderbird 等客户端能够读取邮箱。

## Phase 6：TLS

```text
SMTPS
IMAPS
STARTTLS
```

## Phase 7：HTTP API

```text
Login
Mailbox
Messages
Send
Search
Attachment
```

## Phase 8：Webmail

```text
Inbox
Compose
Reply
Forward
Search
Attachment
```

## Phase 9：邮件安全

```text
SPF
DKIM
DMARC
Rate Limit
Anti Open Relay
```

## Phase 10：客户端基础

```text
Client Core
Account Manager
API Client
SQLite
Mail Cache
Inbox
Message View
Compose
```

## Phase 11：客户端同步

```text
Sync Cursor
Incremental Sync
Draft Sync
Outbox
WebSocket
Realtime Events
```

## Phase 12：客户端高级能力

```text
Offline Mode
Local Search
Attachment Cache
Multi Account
Device Management
Notifications
```

## Phase 13：生产化

```text
Docker
Health Check
Metrics
Backup
Recovery
Admin Panel
Documentation
Security Hardening
```

---

# 50. MVP 版本

## v0.1

```text
Rust
Tokio
PostgreSQL
Docker
SMTP Server
本地邮箱
Maildir
用户认证
基本 SMTP 收信
```

## v0.2

```text
SMTP Outbound
MX
Mail Queue
Retry
```

## v0.3

```text
IMAP
TLS
```

## v0.4

```text
HTTP API
```

## v0.5

```text
Webmail
```

## v0.6

```text
SPF
DKIM
DMARC
DNS Health
```

## v0.7

官方客户端 MVP：

```text
Windows
Linux
macOS

登录
账户管理
收件箱
邮件阅读
写邮件
回复
转发
删除
已读/未读
附件
基础同步
```

## v0.8

```text
Offline
Local Cache
Incremental Sync
Local Search
WebSocket
```

## v0.9

```text
Multi Account
Device Management
Push Notification
Draft Sync
Outbox
```

## v1.0

```text
Server
Webmail
Admin
Official Client
Monitoring
Backup
Security Hardening
Docker Production Deployment
```

---

# 51. 客户端 UI 规划

## 主界面

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

---

# 52. 客户端设置

```text
账户
同步
通知
外观
阅读
写信
附件
搜索
存储
安全
设备
关于
```

同步设置：

```text
同步全部邮件
仅同步最近 30 天
仅同步最近 90 天
附件自动下载
仅 Wi-Fi 下载
最大缓存大小
```

---

# 53. 客户端与 IMAP 的关系

Ferroma 官方客户端：

```text
优先：
HTTPS / FCP
       ↓
Client API
       ↓
Sync / Event
```

第三方客户端：

```text
IMAP
SMTP
```

因此：

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

这样不会破坏第三方客户端兼容性。

---

# 54. 项目风险

## RFC 兼容性

SMTP / IMAP 细节非常多。

策略：

```text
MVP
 ↓
协议测试
 ↓
RFC 覆盖率增加
 ↓
客户端兼容性测试
```

## 邮件投递率

需要考虑：

```text
PTR
SPF
DKIM
DMARC
TLS
IP Reputation
DNS
服务器 IP
25 端口
```

## Open Relay

必须默认：

```text
未认证用户不能向外部域投递邮件
```

## 邮件安全

邮件可能包含：

```text
恶意 HTML
恶意附件
钓鱼链接
超大附件
畸形 MIME
```

第一版至少：

```text
大小限制
解析限制
超时
资源限制
附件策略
```

## 客户端同步冲突

必须定义：

```text
Server Source of Truth
Client Pending Operations
Conflict Resolution
Cursor Recovery
Retry
Idempotency
```

---

# 55. 客户端同步一致性

建议采用：

```text
Server = Source of Truth
```

客户端所有操作具有操作 ID：

```text
operation_id
```

例如：

```json
{
  "operation_id": "op_123",
  "type": "mark_read",
  "message_id": "msg_456"
}
```

服务器需要保证重复提交不会重复执行。

这样可以安全处理：

```text
断线
重试
超时
重复请求
客户端崩溃
```

---

# 56. API 版本策略

统一：

```text
/api/v1
```

未来：

```text
/api/v2
```

客户端声明：

```text
client_name
client_version
protocol_version
platform
```

例如：

```text
FerromaClient/0.7.0
FCP/1
Windows
```

服务器可以根据版本控制兼容性。

---

# 57. 可扩展能力

稳定后可以加入：

```text
邮件规则
Sieve
全文搜索
标签
别名
Catch-All
自动转发
自动回复
日历
联系人
OAuth2
2FA
反垃圾
病毒扫描
对象存储
多节点
高可用
AI 邮件分类
```

客户端未来还可以增加：

```text
Calendar
Contacts
Tasks
Unified Search
Rules
Templates
Quick Reply
Signature
Encryption
```

---

# 58. 最终产品架构

```text
                              FERROMA
                                  │
       ┌──────────────────────────┼──────────────────────────┐
       │                          │                          │
       ▼                          ▼                          ▼
    Webmail                 Official Clients              Admin
       │                          │                          │
       │                    ┌─────┼─────┐                    │
       │                    ▼     ▼     ▼                    │
       │                  Win   Linux  macOS                 │
       │
       └──────────────────────────┼──────────────────────────┘
                                  │
                          Client / HTTP API
                                  │
                         ┌────────▼────────┐
                         │  Ferroma Core   │
                         └────────┬────────┘
                                  │
        ┌───────────┬─────────────┼─────────────┬───────────┐
        ▼           ▼             ▼             ▼           ▼
      SMTP        IMAP         Storage        Queue        DNS
        │           │             │             │           │
        └───────────┴─────────────┼─────────────┴───────────┘
                                  │
                              Event Bus
                                  │
                    ┌─────────────┼─────────────┐
                    ▼             ▼             ▼
                 Webmail       Clients       Admin
                                  │
                              WebSocket
                                  │
                              Push/Event
```

---

# 59. 第一阶段实施建议

第一阶段仍然不要直接开发完整 Webmail 和客户端。

先完成：

```text
Rust
 ↓
Tokio
 ↓
Cargo Workspace
 ↓
Configuration
 ↓
PostgreSQL
 ↓
TCP
 ↓
SMTP Parser
 ↓
SMTP State Machine
 ↓
Mail Core
 ↓
User Authentication
 ↓
Maildir
 ↓
Docker
```

最小里程碑：

```text
alice@example.com
        │
        │ SMTP
        ▼
     Ferroma
        │
        ▼
     Mail Core
        │
        ▼
      Maildir
        │
        ▼
  bob@example.com
```

稳定以后：

```text
Internet
   │
   ▼
SMTP Receive
   │
   ▼
Ferroma
   │
   ├── Local Delivery
   │
   └── Outbound Queue
             │
             ▼
          MX Lookup
             │
             ▼
        Remote SMTP
```

然后再实现：

```text
IMAP
 ↓
HTTP API
 ↓
Webmail
 ↓
Client Core
 ↓
Client API
 ↓
Sync
 ↓
Offline
 ↓
Realtime Events
```

---

# 60. 第一阶段验收标准

服务器：

1. Docker 能启动 Ferroma。
2. PostgreSQL 正常连接。
3. 创建用户 `alice@example.com`。
4. 创建用户 `bob@example.com`。
5. SMTP Server 监听 25。
6. Alice 向 Bob 发送邮件。
7. 邮件成功保存到 Bob 的 Maildir。
8. PostgreSQL 保存邮件元数据。
9. SMTP 日志能够追踪完整投递过程。
10. 非法收件人能够被正确拒绝。
11. 未认证用户不能利用服务器向外部地址中继。

完成以上内容后进入：

```text
SMTP Outbound
→ IMAP
→ TLS
```

---

# 61. 官方客户端 MVP 验收标准

客户端第一版必须能够：

1. 添加 Ferroma 账户。
2. 完成身份认证。
3. 获取邮箱列表。
4. 获取收件箱。
5. 查看邮件列表。
6. 打开邮件。
7. 查看 HTML / Plain Text。
8. 下载附件。
9. 标记已读。
10. 标记未读。
11. 删除邮件。
12. 写邮件。
13. 发送邮件。
14. 保存草稿。
15. 查看发送状态。
16. 基础增量同步。
17. 断线后自动恢复同步。
18. Windows / Linux / macOS 基础运行。

---

# 62. v1.0 核心设计原则总结

Ferroma v1.0 不再只是：

```text
Rust Mail Server
```

而是：

```text
Rust Mail Platform
```

核心组成：

```text
Server
SMTP
IMAP
Mail Core
Storage
Queue
Delivery
DNS
TLS
Authentication
API
Webmail
Admin
Client
Sync
Event Bus
Offline
Local Cache
Search
Attachment
Outbox
Device Management
Notification
```

核心关系：

```text
                    Ferroma Platform
                           │
       ┌───────────────────┼───────────────────┐
       │                   │                   │
    Protocols            APIs              Clients
       │                   │                   │
 SMTP / IMAP          Client API          Desktop
       │                   │               Mobile
       └───────────────────┼───────────────────┘
                           │
                      Mail Core
                           │
              ┌────────────┼────────────┐
              ▼            ▼            ▼
           Storage        Queue        Event Bus
              │            │            │
              └────────────┼────────────┘
                           ▼
                       PostgreSQL
```

最终目标：

> **从协议层开始，用 Rust 自己构建邮件服务器核心，并在其上建立 Webmail、管理后台以及拥有统一同步协议和客户端核心的官方跨平台邮件客户端生态。**

Ferroma 的技术路线遵循：

```text
协议正确性
      ↓
稳定性
      ↓
安全
      ↓
兼容性
      ↓
同步一致性
      ↓
客户端体验
      ↓
功能完整
      ↓
性能优化
```

---

# 63. 下一步开发入口

服务器：

```text
Cargo.toml
server/src/main.rs
server/src/config/
crates/ferroma-core/
crates/ferroma-smtp/
crates/ferroma-storage/
crates/ferroma-auth/
migrations/
Dockerfile
docker-compose.yml
```

客户端：

```text
client/Cargo.toml
client/src/main.rs
client/src/app/
client/src/account/
client/src/api/
client/src/sync/
client/src/database/
client/src/mail/
client/src/draft/
client/src/outbox/
client/src/attachment/
client/src/notification/
client/src/ui/
```

协议文档：

```text
docs/api.md
docs/client.md
docs/fcp.md
docs/sync.md
docs/architecture.md
```

---

# 64. 结论

Ferroma v1.0 的最终目标不是实现一个简单的“Rust 邮件服务器”，而是建立一个完整的：

> **Rust-native 自建邮件平台。**

平台同时面向：

```text
服务器管理员
邮箱用户
第三方邮件客户端
Ferroma 官方客户端
Webmail
开发者
```

通过：

```text
SMTP
IMAP
HTTP API
Ferroma Client Protocol
WebSocket
Event Bus
```

连接整个生态。

服务器负责邮件系统的权威状态，客户端通过同步协议维护本地缓存，并通过事件系统实现实时更新。

最终产品形态：

```text
                    ┌─────────────────────┐
                    │      Ferroma        │
                    │   Mail Platform     │
                    └──────────┬──────────┘
                               │
       ┌───────────────────────┼───────────────────────┐
       │                       │                       │
       ▼                       ▼                       ▼
    Server                  Webmail                  Admin
       │
       ├── SMTP
       ├── IMAP
       ├── Mail Core
       ├── Storage
       ├── Queue
       ├── Delivery
       ├── DNS
       ├── TLS
       ├── Auth
       ├── API
       ├── Sync
       └── Event Bus
                               │
                               ▼
                       Official Client
                               │
                 ┌─────────────┼─────────────┐
                 ▼             ▼             ▼
              Windows        Linux         macOS
                               │
                         Future Mobile
                         Android / iOS
```

**Ferroma v1.0 = Mail Server + Mail Platform + Official Client Ecosystem。**
