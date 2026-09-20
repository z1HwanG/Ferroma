# 术语表 — Ferroma 文档中译

本文件是 `docs/` 中文版的**唯一术语来源**。十份文档由不同译者在同一时间产出，只有
共用这份表，读者才能把它们当成一套文档来读。

英文侧的术语源文件是 [`../GLOSSARY.md`](../GLOSSARY.md)：它记录同一批决定，并给出每条术语
在英文文档中的首选写法。两份文件不是译本关系，改动一处就要同步另一处。

## 一、绝不翻译的内容

以下内容**一律原样保留**，包括大小写与标点。译错一个字符就等于文档说谎：

* **配置键**：`server.hostname`、`limits.max_message_size`、`api.jwt_secret`、
  `[dns] resolvers`、`tls.cert_path` 等。保留点号与方括号。
* **环境变量**：`DATABASE_URL`、`FERROMA_JWT_SECRET`、`FERROMA__API__WEBMAIL_DIR`、
  `FERROMA__API__ADMIN_DIR`、`FERROMA__API__SHARED_DIR`。
* **命令行**：`ferroma serve`、`ferroma database init`、`cargo test --workspace`。
* **HTTP 方法与路径**：`GET /api/v1/messages/{id}`、`DELETE /api/v1/drafts/{id}`。
* **协议关键字**：`EHLO`、`MAIL FROM`、`RCPT TO`、`DATA`、`IDLE`、`APPEND`、`FETCH`。
* **错误码与应答码**：`550 5.1.1`、`426 Upgrade Required`、`4.2.2`、`5.3.4`。
* **Rust 类型与标识符**：`FerromaError::MailboxFull`、`ClientHandle`、`SyncChange`、
  `ferroma-mail`、`Database::connect`。
* **数据库对象**：表名、列名、`change_log`、`rfc_message_id`、`TIMESTAMPTZ`。
* **JSON 字段名**：`"type"`、`message_id_header`、`access_token`。
* **文件名与路径**：`web/`、`migrations/0001_initial.sql`、`/var/lib/ferroma`。
* **RFC 编号**：RFC 5322、RFC 3501、RFC 7489（写「RFC 5322」而不是「RFC 5322 标准」）。

## 二、术语对照

| 英文 | 中文 | 说明 |
|---|---|---|
| mail core | 邮件核心 | 指 `ferroma-mail` 这一层 |
| repository / repositories | 仓储层 | 不译作「仓库」（那是 git 的意思） |
| mailbox | 邮箱 | 指一个地址对应的容器 |
| folder | 文件夹 | IMAP 文件夹，与邮箱区分 |
| message | 邮件 | 不用「消息」；email 也写「邮件」 |
| envelope | 信封 | SMTP 信封（MAIL FROM / RCPT TO） |
| header | 头部 / 头字段 | 按语境；`Received:` 这类叫「头字段」 |
| body | 正文 | 不用「身体」 |
| attachment | 附件 | |
| blob store | 二进制存储 | 内容寻址的附件存储 |
| content-addressed | 内容寻址 | |
| quota | 配额 | |
| cursor | 游标 | 增量同步位置 |
| change log | 变更日志 | 数据库表 `change_log` |
| incremental sync | 增量同步 | |
| idempotency key | 幂等键 | |
| outbox | 发件箱 | 客户端待发队列 |
| device | 设备 | 客户端注册的设备 |
| session | 会话 | |
| access token | 访问令牌 | |
| refresh token | 刷新令牌 | |
| throttling | 限流 | 登录尝试限制 |
| open relay | 开放中继 | |
| relay | 中继 | |
| submission | 提交 | 587 端口，认证后发信 |
| inbound / outbound | 收信 / 发信 | 不用「入站 / 出站」描述邮件方向 |
| delivery | 投递 | |
| queue | 队列 | |
| retry | 重试 | |
| bounce | 退信 | |
| quarantine | 隔离 | 策略判定后放入 Junk |
| deliverability | 投递率 | |
| resolver | 解析器 | DNS |
| lookup | 查询 | DNS 查询 |
| blackhole | 黑洞式丢弃 | 指不回应答的解析器 |
| deadline / budget | 超时上限 / 预算 | 区分「兜底上限」与「实际预算」 |
| preflight | 预检 | `ferroma doctor` |
| health check | 健康检查 | |
| migration | 迁移 | 数据库 |
| Maildir | Maildir | 保持原样 |
| front-end | 前端 | 名词与定语，指 `web/` 与 `admin/`；不写作 frontend |
| Webmail | Webmail | 保持原样，不译作「网页邮件」 |
| Admin | Admin | 保持原样；不译作「管理后台」，首次出现可写「Admin 管理后台」 |
| client | 客户端 | 指官方桌面客户端 |
| shell | 外壳 | 指图形外壳 |
| core | 核心 | |
| seam | 接缝 | 接口边界 |
| contract | 契约 | 指冻结的接口约定 |
| drift | 漂移 | 实现与文档不一致 |
| hollow green | 虚假的绿色 | 测试跳过却被当成通过 |
| ratchet | 棘轮 | 只允许单向收紧的约束 |
| acceptance run | 验收运行 | `server/tests/e2e.rs` |
| fixture | 夹具 | 测试数据 |
| gate / gated | 门控 | |
| operator | 运维者 | 指运行主机的人；不写作「操作者」，也别与搜索运算符混淆 |
| login / log in | 登录 | 名词与动词同形 |
| backup / back up | 备份 | 名词、定语与动词同形 |
| real time | 实时 | real-time 作定语时也写「实时」 |
| first-run wizard | 首次运行向导 | 首次出现写全称，之后可简称「向导」 |
| named volume | 命名卷 | Docker 存储，与「绑定挂载」相对 |
| bind mount | 绑定挂载 | Docker 存储，与「命名卷」相对 |
| reverse proxy | 反向代理 | 首次出现写全称，之后可简称「代理」 |
| release | 发布 | 一次发布是 `X.Y.Z` |
| tag | 标签 | 给发布命名的标签，如 `0.2.0`、`latest` |
| digest | 摘要 | 镜像 manifest 的不可变摘要；密码与校验和才叫「哈希」 |
| provenance attestation | 来源证明 | BuildKit 附加到已发布镜像的声明 |
| layer cache | 层缓存 | 本地 buildx 缓存：`.cache/buildx`，检出路径含非 ASCII 字符时改用 `$HOME/.cache/ferroma-buildx` |
| sidecar | 边车 | **已废弃**：0.1.3 及更早版本随附的备份/恢复容器，只在历史说明中出现 |
| job | 作业 | 指「某人分内的事」时写「……的事」 |
| roadmap | 路线图 | |
| TODO | 待办 | 代码里的 `TODO(0.1.5)` 保持原样 |
| setup / set up | 设置（名词）/ 设置（动词） | 名词与定语用 setup，动词写 set up；别写 set-up |

## 三、文体

* 用简体中文，技术文档语气：直接、克制、不用感叹号。
* 英文原文里的破折号插入语（em dash）在中文里用逗号或括号处理，不要照搬 `—`。
* 代码块内的**注释**要翻译，**代码本身**不翻译。
* 表格的表头翻译，单元格里的标识符不翻译。
* 中文与拉丁字母、数字之间加**一个**空格：写 `使用 rustls`、`共 14 天`，不写 `使用rustls`、`共14 天`。
  全角标点两侧不加空格，照中文排版直接相连：写 `邮件，共 12 封`。标识符（路径、配置键、命令）一律放进反引号。
* 首次出现的术语可以「中文（English）」，之后只用中文。
* 保持原文的**段落划分与章节顺序**，不要合并或拆分小节。
* 保留原文所有的 Markdown 结构：代码块、表格、列表、引用块、链接。

## 四、交叉链接

中文版位于 `docs/zh/`，与英文版同级文件名。文档之间的链接**指向同目录的中文版**：

```markdown
[architecture.md](architecture.md)      ← 正确，指向 docs/zh/architecture.md
[architecture.md](../architecture.md)   ← 错误
```

指向仓库其它位置（`../crates/`、`../config/`）的链接保持原样，因为那些文件没有中文版。
