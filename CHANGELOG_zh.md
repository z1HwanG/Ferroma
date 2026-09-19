# 更新日志

Ferroma 的重要变更，新的在前。

每个已发布版本都是一个 `vX.Y.Z` 标签，其值必须与 `Cargo.toml` 一致——因为
`.github/workflows/docker-publish.yml` 会拒绝与清单不符的标签，标签因此无法标记一棵
它并未构建的源码树。1.0 之前，次版本号可能改变任何东西，修订号则只修缺陷。

章节形式大体沿用 [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)：某个版本
没有内容的章节直接省略，不留空标题。英文原版见 [`CHANGELOG.md`](CHANGELOG.md)。

## [未发布]

### 带入 0.1.4 的已知问题

两条都是发 0.1.3 时发现的。都不是 0.1.3 引入的回归，也都不是一次机械替换能了结的。

- **已发布的镜像说不清自己是哪个构建。** 容器里执行 `ferroma version`、以及 Admin
  侧栏页脚，都显示 `built: unknown` / `revision: unknown`。`ferroma-core/src/version.rs`
  用 `option_env!` 读取 `FERROMA_BUILD_TIMESTAMP` 与 `FERROMA_GIT_SHA`，而 `Dockerfile`
  只把构建标识传给了 OCI 标签——于是 `docker inspect` 能说出提交，二进制自己却说不出。
  标签是对的；修法与其中唯一的坑见
  [README — 带入 0.1.4 的待办](README_zh.md#带入-014-的待办)。
- **八篇文档还在描述骨架。** 里面留有 80 处写于各 crate 尚不存在之时的 `_(planned)_`
  断言，其中五篇（`client.md`、`imap.md`、`security.md`、`smtp.md`、`sync.md`）开头仍挂着
  把已实现 crate 称作未实现的状态横幅。每一处都是关于行为的一句断言，需要对着代码逐条
  核实，所以这是一次通读，而不是查找替换。

## [0.1.3] — 2026-09-19

### 新增

- **两个前端都支持简体中文，与英文并存。** 英文是源语言，其文本**就是**查找键，因此没有
  译文的字符串会退化为英文而不是出错；`shared/locales/zh-CN.js` 收录 881 条。语言选择器
  在「设置」里，选择按浏览器记忆，并且会一并要求 API 用该语言返回报错。
- **API 的错误消息支持按 `Accept-Language` 本地化**（RFC 9110 的 q 值）。`code` 不随语言
  变化。消息按「种类」与「细节」两半分别翻译——正是这一点覆盖了 126 处 `format!` 调用点，
  而不必给每一处都穿一个消息键。
- **`shared/`：两个前端共用的模块只留一份。** 服务端把它挂在 `/shared`，因此
  `../shared/api.js` 从 `/main.js` 与从 `/admin/main.js` 解析到同一个 URL；
  `api.shared_dir` 指定该目录，镜像也把它打进去。
- `tools/serve-frontends.mjs`——按路由器的同样三个挂载提供 `web/`、`admin/`、`shared/`，
  用于不需要服务器与数据库的前端开发。
- `ferroma user create --disabled`，与 `POST /users` 传 `enabled: false` 对应。

### 变更

- `/auth/me` 返回完整的邮箱记录，而不再是精简地址摘要，客户端因此能读到每个地址的配额与
  当前用量。
- `GET /users` 带上每个账号的地址；`GET /audit` 把操作者解析成地址，而不只发一个 id。
- 向导被停用时，`GET /setup` 返回 `404`，而不再是 `200 {required:false}`——客户端此前无法
  区分「已停用」与「已有管理员」。提交的 `hostname` 与 `server.hostname` 不一致时返回
  `400` 并指出该改哪个配置项，且校验发生在任何写入之前。
- 未知的 `/api/v1` 路径、以及方法不匹配的已知路径，都返回 `docs/api.md` §1.3 承诺的错误
  信封。此前未知路径会落到 Webmail 的 SPA 回退，以 `200` 返回 `index.html`。
- Admin 控制台新增筛选栏、可排序可多选的表格、详情抽屉与真正的分区布局；`hidden` 属性现在
  真的会隐藏它标记的内容。
- Webmail 的写信对话框恢复成可以发送与保存。

### 修复

- **外发邮件现在真的会做 DKIM 签名。** `[dkim]` 早已完整实现（密钥解析、规范化、签名），
  却在生产路径上没有任何调用者：即便启用了 DKIM、发布了选择器、密钥也在库里，所有外发
  邮件仍然没有签名。
- Webmail 把每封邮件都显示为未读、每个星标都藏起来：API 发的是规范的小写标志串
  （`seen flagged`，由 `docs/fcp.md` §4 冻结），而客户端在匹配 `\Seen`。现在标志也会以
  布尔字段一并下发。
- `INBOX` 从来匹配不上，因为它不带 `special_use`——RFC 6154 没有 `\Inbox`。现在按名字
  识别；标准文件夹用显示名呈现，而 `folder.name` 始终是 IMAP 数据，绝不翻译。
- `POST /users` 会静默丢弃 `enabled`，控制台里的「账号已启用」勾选框因此毫无作用；清空
  显示名会报告成功，却什么都没改。
- Admin 仪表盘五个卡片里有四个渲染出字符串 `undefined`。
- 修改密码时「当前密码错误」被当作会话过期而重试；附件下载把 `304 Not Modified` 当成失败；
  徽章颜色在标签被提前翻译后丢失。
- `doctor` 的 loopback 遮蔽测试断言的是 Windows 的 `SO_REUSEADDR` 语义，在任何 Linux 主机
  上都会失败。
- `ferroma-api` 的集成测试辅助用「先查后建」创建共享测试库，与该测试二进制里的其它测试
  互相竞争。
- `scripts/docker-publish.sh` 现在可执行——它自己的用法示例正是这么用的。

### 移除

- `backup/` 与 `.probe/`——从未纳入版本控制的本地杂物：一个所含提交早已在历史里的打包，
  以及上一轮审计用完即弃的验证程序。

## [0.1.2] — 2026-09-17

### 新增

- Admin 应用在 `/admin/` 提供服务，并带目录重定向，使其相对资源 URL 解析到 Admin 目录内
  而不是 Webmail 的目录。

### 修复

- 两个前端改为读取 API 实际发出的字段名，并补上此前一直空着的区域。
- 前端静态检查在文档所述的位置运行。

## [0.1.1] — 2026-09-17

### 修复

- 一处未定义的 `renderChrome` 引用，它导致 Webmail 完全无法启动。

## [0.1.0] — 2026-09-17

首个版本：同一个进程里的 SMTP、IMAP 与 HTTP API，底层是 PostgreSQL 元数据、存放邮件正文
的 Maildir 与内容寻址的附件存储，以及 Webmail、Admin 两个应用和官方客户端的核心与命令行。

### 新增

- 发信中继，服务于 IP 没有 PTR 记录的主机。
- Docker Hub 发布，由 `v*` 标签驱动。

### 变更

- 运维脚本可执行。

### 修复

- `scripts/deploy.sh` 在 PostgreSQL 不可达时快速失败，可自行置备容器化 PostgreSQL，能在没有
  `postgres` 数据库的集群上存活，并且会去问容器它的超级用户是谁，而不是靠假设。
- `ferroma database init` 能在不假定存在 `postgres` 数据库的前提下创建缺失的数据库。
- TLS 配置被带入 IMAP 配置；`FERROMA_PUBLIC_URL` 以服务端真正读取的名字传进去。
