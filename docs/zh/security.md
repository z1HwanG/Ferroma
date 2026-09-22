# 安全

**谁应该读这份文档：**任何准备把Ferroma暴露到互联网之前评审它的人，任何改动认证、TLS、
邮件策略或存储层的人，以及任何需要知道一次Ferroma部署防住了什么、又没防住什么的运维者。

这份文档是威胁模型，也是控制项清单。它先陈述前提假设，再逐条走过每一项控制（密码哈希、
令牌与会话、登录限流、中继预防、发件人与收件人校验、SPF/DKIM/DMARC、TLS、速率与大小限制、
HTML 处理、路径穿越、日志卫生与密钥管理），并给出实现它的真实类型、函数与配置键。文档以
一张把每项控制映射到实现位置的表结束，另有一节「已知缺口」，列出Ferroma v1刻意不防的东西。

> **状态：**已实现。这里描述的一切——密码、令牌与会话、登录限流、中继预防、
> 发件人与收件人校验、SPF/DKIM/DMARC、TLS、速率与大小限制、HTML 净化、路径穿越
> 防护与日志卫生——都存在于`ferroma-core`、`ferroma-auth`、`ferroma-storage`、
> `ferroma-mail`、`ferroma-smtp`和`ferroma-api`中，并由`cargo test --workspace`
> 与验收测试覆盖。Ferroma v1 刻意**不做**的控制项在 §14 中陈述，而不是在这里标记
> 为待办。

---

## 1. 威胁模型

### 1.1 Ferroma 在保护什么

| 资产 | 存放位置 | 失陷后果 |
|---|---|---|
| 邮件内容 | `storage.maildir_root`下的Maildir，`storage.attachment_root`下的二进制对象 | 每个用户的往来信件被完整读取 |
| 凭据 | `users.password_hash`（Argon2id PHC 字符串） | 离线破解，随后账号被接管 |
| 会话与令牌 | `sessions.token_hash`，内存中的访问令牌 | 无需密码即可实时接管账号 |
| DKIM 私钥 | `domains.dkim_private_key`、`[dkim] private_key_path` | 以运维者的域名伪造已签名邮件 |
| TLS 私钥 | `tls.key_path` | 冒充这台服务器，解密已录制的流量 |
| `api.jwt_secret` | 环境变量（`FERROMA_JWT_SECRET`） | 为任意用户签发有效访问令牌 |
| 服务器的发信声誉 | IP 地址与域名 | 这台机器变成垃圾邮件源并被列入黑名单 |

### 1.2 对手是谁

| 对手 | 能力 | 主要控制 |
|---|---|---|
| **远端 SMTP 对端** | 发送任意命令、`MAIL FROM`、收件人、`DATA`、MIME | 中继策略（§5）、发件人/收件人校验（§6）、大小与速率限制（§11）、解析限制（§10） |
| **远端 IMAP 对端** | 任意命令、字面量、超大序号集合 | 认证（§2–§4）、`imap.require_tls_for_login`、`limits.max_fetch_messages`、`imap.max_append_size` |
| **未认证的 HTTP 客户端** | 任意 JSON 正文、请求头、URL | 令牌校验（§3）、不泄露任何信息的错误信封（[api.md](api.md) §1.3）、`api.trust_proxy_headers`默认关闭 |
| **滥用自身账号的合法用户** | 能认证、能发信、能存储 | 按账号的速率限制（`submission_rate_limit`、`daily_send_limit`）、配额、`From`上的`mailboxes.user_id`归属校验 |
| **被攻陷的客户端设备** | 持有一枚刷新令牌 | 基于轮换的盗用检测（§3）、设备吊销（§4）、令牌在静态存储时以哈希形式保存 |
| **拥有文件系统访问权的本地攻击者** | 读取文件 | 密码哈希是 Argon2id，令牌只以哈希形式存储，邮件存储中没有明文机密 |
| **拥有数据库访问权的本地攻击者** | 读写数据行 | 路径穿越门控（§12）意味着即使`storage_path`被篡改，也仍然逃不出邮件根目录 |
| **畸形邮件的作者** | 深层嵌套的 MIME、超大头部、错误编码 | `ParseLimits`（20 层、200 个部分、`limits.max_message_size`）、永不 panic 的全量解析 |

### 1.3 明确的非假设

* **网络是敌对的。**公网上的 SMTP 与 IMAP 默认是明文，TLS 是机会式的，因此25端口上的
  邮件内容被假定可被被动观察者读取。真正保护凭据与要紧内容的是提交端口（587）和
  IMAP（993）上的 TLS。
* **`Received:`、`From:`、`HELO`以及每一个显示名都由攻击者控制。**它们会被解析、存储和
  显示，但从不被用来做任何授权决定。
* **数据库可信，库里的文件系统路径不可信。**两个存储层都会重新校验交给它们的每一条
  相对路径，见§12。
* **单进程、单主机。**不支持多节点部署，事件总线不跨进程
  （[architecture.md](architecture.md) §6）。

---

## 2. 密码

### 2.1 Argon2id 参数

`crates/ferroma-auth/src/password.rs`：

```rust
impl Default for Argon2Params {
    fn default() -> Self {
        // OWASP 2024：m=19456 KiB（19 MiB），t=2，p=1。
        Argon2Params { memory_kib: 19_456, iterations: 2, parallelism: 1 }
    }
}
```

存储形式是一个 PHC 字符串，参数因此随哈希一起走：

```text
$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>
```

| 参数 | 取值 | 理由 |
|---|---|---|
| 算法 | Argon2id（`Algorithm::Argon2id`） | 混合型：与数据无关的第一趟抵御侧信道攻击，与数据相关的后续趟抵御时间与内存的权衡。它是 OWASP 对新应用的首选 |
| 内存 | 19 456 KiB（19 MiB） | 内存硬性才是让 GPU 与 ASIC 破解昂贵的根本。19 MiB 是 OWASP 2024 年的建议值，也能从容放进每连接的预算 |
| 迭代次数 | 2 | OWASP 推荐的第二个调节轴；在 19 MiB 之下，增加趟数买到的东西不如增加内存 |
| 并行度 | 1 | 每个哈希一条通道，登录洪水无法借设备的核心数被放大 |
| 盐 | 来自`OsRng`的 16 个随机字节（`SaltString::generate`） | 每个密码唯一，彩虹表因此无用，两个用同一密码的用户哈希也不同 |
| 版本 | `Version::V0x13` | 当前的 Argon2 版本 |

它满足的要求是*离线*抗性：`users.password_hash`可能出现在备份、数据库转储或 SQL 注入的
结果里，而 19 MiB × 2 趟让字典攻击的每一次猜测都要付出真金白银。

### 2.2 校验

```rust
pub fn verify(&self, password: &str, stored: &str) -> bool {
    match PasswordHash::new(stored) {
        Ok(parsed) => Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}
```

两条刻意的性质：

* **密码错误与存储的哈希无法解析，都返回`false`。**一行损坏的数据无法通过比较错误信息
  被探出信息。
* **校验相对于哈希是常数时间的**，这是`argon2` crate 的保证。派生密钥的比较走的是 crate
  内部常数时间的`subtle`路径。

### 2.3 透明重哈希

`PasswordHasher::needs_rehash(stored)`把 PHC 字符串里记录的参数与当前参数比较，
`AuthService::login`则在还持有明文的时候重哈希密码：

```rust
if self.hasher.needs_rehash(&user.password_hash) {
    match self.hash_password(password).await { … }
}
```

升级失败会以`warn`记录，并且**不会让登录失败**。于是参数集日后可以调高，整个数据库会
随时间自我升级，既不需要迁移，也不会把任何人锁在门外。`Argon2Params::stored_params`
从一个已有哈希里读出参数，用于审计。

### 2.4 密码策略

`validate_password`强制执行长度上下界，`strength(password) -> u8`为 UI 提供一个评分：

| 常量 | 取值 |
|---|---|
| `MIN_PASSWORD_LENGTH` | 8 |
| `MAX_PASSWORD_LENGTH` | 1024，Argon2 本身没有上限，这一条限定的是请求大小 |

**没有**组合规则（没有「必须包含一个数字」），因为真正要紧的性质是长度，而组合规则会把
用户推向`Password1!`。**没有**泄露语料库检查，见§14。

`AuthService::change_password`会吊销其他每一个会话：改密码是用户怀疑自己被攻陷时做出的
动作，让其他会话继续活着会使这个动作失效。

### 2.5 登录失败记账

| 列 / 配置 | 效果 |
|---|---|
| `users.failed_logins` | 连续失败次数，由`record_login_success`重置 |
| `users.locked_until` | 由`record_login_failure(user_id, now, lockout_secs, max_failed_logins)`设置 |
| `limits.max_failed_logins` | 10，阈值 |
| `limits.login_lockout_secs` | 900，即 15 分钟 |

---

## 3. 访问令牌、刷新令牌与基于轮换的盗用检测

### 3.1 两种令牌

| | 访问令牌 | 刷新令牌 |
|---|---|---|
| 格式 | JWT，HS256 | 不透明：`rt_`+base64url(32 个随机字节) |
| 常量 | — | `REFRESH_PREFIX = "rt_"`、`OPAQUE_TOKEN_BYTES = 32` |
| 生存期 | `api.access_token_ttl_secs`，3600 秒 | `api.refresh_token_ttl_secs`，2 592 000 秒（30 天） |
| 服务端是否存储？ | **否**，无状态，靠签名校验 | **是，存哈希**：`sessions.token_hash` |
| 发送方式 | 每个请求上的`Authorization: Bearer …` | 只发给`/auth/refresh` |
| 可否吊销 | 不能直接吊销；靠吊销会话或轮换密钥来吊销 | 可以，`sessions.revoked_at` |

为什么两者都要：无状态的访问令牌让热路径免去一次数据库往返，而短生存期给泄露令牌造成的
损害封顶。不透明的刷新令牌才是服务器真正能吊销的令牌，而 JWT 从根本上做不到这一点。

### 3.2 访问令牌的声明及其校验

`crates/ferroma-auth/src/token.rs`中的`Claims` / `AccessClaims`：

```json
{ "sub": "7", "sid": 12, "typ": "access", "iss": "mail.example.com",
  "iat": 1789000000, "exp": 1789003600, "jti": "9f2c41…" }
```

`TokenService::verify_access_at`按以下顺序校验：

| 校验 | 失败时 |
|---|---|
| 恰好是三个以点分隔的部分 | `unauthorized("malformed token")` |
| 头部可解码且可解析 | `unauthorized("malformed token header")` |
| **`alg == "HS256"`，精确匹配** | `unauthorized("unsupported token algorithm")`，这是对`alg: none`与算法混淆的防御；不存在协商 |
| 签名校验通过，且**在**载荷被解析**之前** | `unauthorized("invalid token signature")` |
| `typ == "access"` | `unauthorized("not an access token")`，刷新令牌不能当访问令牌用 |
| `iss == self.issuer` | `unauthorized("token issued for a different server")` |
| `exp > now` | `unauthorized("token expired")` |
| `iat <= now + 5 minutes` | `unauthorized("token issued in the future")`，伪造或时钟错乱的声明会被拒绝，而不是被无限信任 |

顺序本身就是重点：**在把任何攻击者控制的声明反序列化成结构体之前，先校验签名**，而 HMAC
比较是常数时间的（`mac.verify_slice`，底层是`subtle`）。这里没有由`alg`驱动的分派可供混淆。

`jti`是每枚令牌一个 UUID，因此单枚令牌可以在日志里被指认出来，而不必把令牌记进日志。

### 3.3 基于轮换的盗用检测

每次刷新都会轮换：出示的那个会话被吊销，并开一个同类型的新会话。

```rust
// crates/ferroma-auth/src/service.rs
if session.revoked_at.is_some() {
    // 复用了已吊销的令牌：假定已被攻陷，烧掉整个家族。
    let revoked = self.repos.sessions.revoke_all_for_user(user_id).await?;
    tracing::warn!(user_id = …, session_id = session.id, revoked,
                   "revoked refresh token reused; all sessions revoked");
    return Err(FerromaError::Unauthorized(
        "refresh token was already used; all sessions have been revoked".into(),
    ));
}
```

理由：诚实的客户端只会出示一次刷新令牌，然后把它丢掉。若一枚令牌被出示两次，要么客户端
有 bug，要么有两方持有它，而服务器无法分辨是哪一种。吊销整个家族是保守的答案，也是刷新
令牌轮换的标准模式。

```text
  登录       ──► refresh_1（以哈希形式存入 sessions）
  刷新       ──► refresh_1 被吊销，签发 refresh_2
  refresh_2  ──► 正常
  攻击者出示 refresh_1
             ──► "already used" ⇒ 吊销该用户的每一个会话
             ──► 合法客户端的 refresh_2 也随之失效
             ──► 用户被迫重新登录，而这次盗用在日志里看得见
```

`AuthService::refresh`也会拒绝已过期的会话（`unauthorized("refresh token expired")`）和
已停用的账号（`unauthorized("account disabled")`）。

### 3.4 为什么存哈希，而不是存令牌

`sessions.token_hash`保存的是`hash_token(raw)`，即 SHA-256、小写十六进制，来自
`crates/ferroma-auth/src/token.rs`：

> 原始令牌的 SHA-256，小写十六进制。这是唯一会被写入数据库的形式。

这里用普通 SHA-256 而不是 Argon2 是正确的，不是抄近路：输入是 32 字节的 CSPRNG 输出，
没有字典可攻，也没有必要再加工作因子。真正要紧的是，一份数据库转储里不会含有可直接使用
的刷新令牌。

`looks_like_opaque_token(value)`存在的目的，是让日志清洗能凭前缀（`rt_`或`st_`）认出
一枚令牌，而不必知道它的值。

### 3.5 `api.jwt_secret`

`TokenService::from_config`在`api.jwt_secret`已设置且非空白时使用它。未设置时：

```rust
tracing::warn!(
    "api.jwt_secret is not configured: generating an ephemeral secret. \
     Every restart will invalidate all sessions. Set FERROMA_JWT_SECRET in production."
);
```

并且`has_ephemeral_secret()`返回`true`，调用方因此可以拒绝在该状态下提供服务。
`docker-compose.yml`与`docker-compose.prod.yml`都要求它
（`${FERROMA_JWT_SECRET:?set FERROMA_JWT_SECRET in .env}`），`.env.example`给出了生成
命令：

```bash
openssl rand -base64 48
```

HS256 是对称签名，所以这个密钥与它签发的每一个会话同等敏感。见§13。

---

## 4. 会话与设备吊销

### 4.1 `sessions`行

`kind`是`web`、`api`、`client`、`imap`、`smtp`之一（由`sessions_kind_known`强制），
因此 IMAP 会话与 Webmail 会话可以区分，也能各自独立吊销。`SessionKind::is_refreshable()`
标记出可以使用`/auth/refresh`的类型。

生命周期：

| 操作 | 方法 | 效果 |
|---|---|---|
| 开启 | `AuthService::open_session(user, kind, device_id, ip, user_agent)` | 插入数据行，返回`(Session, TokenPair)` |
| 认证 | `AuthService::authenticate(bearer)` | 校验访问令牌，随后要求会话存在且未被吊销 |
| 吊销单个 | `AuthService::logout(session_id)` | 设置`revoked_at` |
| 吊销某用户全部 | `AuthService::logout_all(user_id)` / `SessionsRepository::revoke_all_for_user` | 一次管理性锁定 |
| 清理 | `AuthService::purge_expired_sessions()` | 删除超过`expires_at`的数据行，由`sessions_expiry_idx … WHERE revoked_at IS NULL`索引 |

**签名有效并不够。**`authenticate`在校验令牌之后还会检查会话行，这才使吊销立即生效，
而不是等到访问令牌过期。`sessions_expiry_idx`对`revoked_at IS NULL`是部分索引，因此清扫器
不会扫描已死的数据行。

### 4.2 设备

`devices`按安装实例划分，键为`(user_id, device_uid)`，其中`device_uid`由客户端生成且保持
稳定。`device_uid`会被校验：非空且不超过 128 个字符（`AuthService::register_device`）。

吊销一台设备就是规范§33所说的远程擦除动作：

```rust
pub async fn revoke_device(&self, device_id: DeviceId) -> Result<u64>
```

1. 把设备标记为已吊销（`devices.revoked_at`），
2. 吊销属于它的每一个会话，
3. 发布`Event::device_revoked(device_id, user_id)`，使该设备上活跃的 WebSocket 断开
   （[fcp.md](fcp.md) §9）。

被吊销客户端的下一次请求是`401`。吊销你正在发起调用的那台设备是允许的，并且立即生效：
丢了笔记本的用户必须能从任何其他设备切断它，包括一台看起来像它的设备。

### 4.3 空闲会话

`client.session_idle_days`（90）是设备会话在多长时间未使用后应被吊销的策略。它由周期性
清扫执行，而不是靠`expires_at`列：刷新令牌自身的`api.refresh_token_ttl_secs`（30 天）是
硬上限，而空闲策略捕捉的是那种一直在刷新、却从未真正被使用的设备。

---

## 5. 登录限流与锁定

两套相互独立的机制，在`AuthService::login`中按此顺序检查。

### 5.1 按源 IP，在任何昂贵工作之前检查

```rust
if let Some(ref ip_text) = ip_str {
    let failures = self.repos.login_attempts
        .count_failures_for_ip(ip_text, now - self.failure_window).await?;
    if failures >= i64::from(self.limits.max_failed_logins) * 3 {
        tracing::warn!(ip = %ip_text, failures, "login throttled by source address");
        self.record_attempt(&email, ip_str.as_deref(), "password", false).await;
        return Err(FerromaError::RateLimited);
    }
}
```

阈值是每账号阈值的**三倍**，因为一个 NAT 出口或办公楼出口地址完全可能合法地容纳十个以上
的用户。检查发生在用户查询**之前**、Argon2**之前**，因此凭据洪水无法从单一来源为每个
请求花掉 19 MiB 和两趟计算。这个顺序正是这段代码放在最前面的全部理由。

`failure_window`由`AuthService::with_failure_window`设置，测试因此可以把它钉死。

### 5.2 按账号锁定

| 步骤 | 效果 |
|---|---|
| 密码错误 | `UsersRepository::record_login_failure(user_id, now, lockout_secs, max_failed_logins)`递增`failed_logins`，并在达到阈值时设置`locked_until` |
| 已锁定 | `User::is_login_allowed(now)`为假 ⇒ `FerromaError::RateLimited` |
| 成功 | `record_login_success`清除`failed_logins`与`locked_until`，写入`last_login_at` |
| 两种情况 | 无论哪种都会写下一行`login_attempts`，用于审计轨迹 |

两者在 HTTP 上都是`429 rate_limited`（[api.md](api.md) §1.3），在 SMTP 上是`454 4.7.0`
（[smtp.md](smtp.md) §8）。

### 5.3 刻意保持一致的部分

| 情形 | 响应 |
|---|---|
| 未知账号 | `invalid_credentials()` |
| 密码错误 | `invalid_credentials()` |
| 已停用账号 | `invalid_credentials()`**并且**记一条点名用户 id 的`warn`日志 |

代码里的注释说得很明确：*「未知账号：与密码错误相同的信息、相同的开销特征。」*攻击者无法
通过登录端点枚举账号，而两种情况下每次尝试都恰好花掉一次 Argon2 校验，这也正是上面的限流
必须存在的原因。

服务器**确实**区别对待的唯一地方是审计日志，已停用账号会在那里产生
`"login refused: account disabled"`。那是给运维者看的，不是给调用方看的。

### 5.4 保留期

`login_attempts`会随每一次尝试增长。针对`(created_at)`的`login_attempts_created_idx`就是
为按时间删除的保留期清扫而存在的。没有它，在繁忙的服务器上这张表会是整个 schema 中最大的
一张。

---

## 6. 开放中继预防、发件人与收件人校验

### 6.1 中继策略

在[../AGENTS.md](../../AGENTS.md) §4.6与规范§9.4中陈述，并在[smtp.md](smtp.md) §6中完整规定：

```text
  收件人域为本地  →  接受（配额与限制照常生效）
  收件人域为远端  →  必须先成功完成 AUTH
```

| 连接 | 收件人 | 结果 |
|---|---|---|
| 未认证，25 端口 | 本地 | 接受并投递 |
| 未认证，25 端口 | 远端 | `550 5.7.1 Relaying denied` |
| 已认证 | 本地或远端 | 接受，远端则入队 |

没有任何配置键能打开中继。安全的行为是「打错字也打不开」，这正是策略与建议之间的差别。

### 6.2 收件人校验

`RCPT TO`通过数据库解析，而不是靠猜文件系统：

1. 域名：`DomainsRepository::find_by_name(normalise_domain(domain))`，并且
   `domains.enabled`必须为真。未知或已停用 ⇒ `550 5.1.2`。
2. 本地部分：`MailboxesRepository::find_by_address(domain, local_part)`，索引是
   `mailboxes_address_key (domain_id, local_part)`。找不到 ⇒ 试`AliasesRepository`，
   再试`domains.catch_all`；仍然找不到 ⇒ `550 5.1.1`。
3. `mailboxes.enabled`必须为真；已停用的地址是`550 5.1.1`。
4. 收件人数量对照`limits.max_recipients` ⇒ 超限时`452 4.5.3`。

两次查询都使用小写形式（`EmailAddress::to_lowercase`、`normalise_domain`），schema 也强制
这一点：`mailboxes_local_lowercase CHECK (local_part = lower(local_part))`、
`domains_name_lowercase CHECK (name = lower(name))`、
`users_email_lowercase CHECK (email = lower(email))`。大小写不敏感既是正确性要求，也是
安全要求：两行只在大小写上不同的数据会让「这个地址是谁」变得含混。

### 6.3 发件人校验与地址语法

`MAIL FROM`由`ferroma_core::EmailAddress::parse`解析，它刻意严格。`validate_local_part`与
`validate_domain`会拒绝（其中包括）：

| 被拒绝的输入 | 原因 |
|---|---|
| `alice`（无域名） | 不是地址 |
| `alice@`、`@example.com` | 有一半是空的 |
| `alice@@example.com` | 两个分隔符 |
| `.alice@…`、`alice.@…`、`al.ice..x@…` | 点的位置非法 |
| `Alice <alice@example.com>` | 显示名不是地址；解析器不做猜测 |
| `ali ce@…` | 含空白 |
| `alice@-example.com`、`alice@example-.com` | 标签不能以`-`开头或结尾 |
| `alice@example..com` | 空标签 |
| `alice@[192.0.2.1]` | 域名字面量在 SMTP 中合法，但永远不是本地邮箱域，接受它只会造出一个谁也到不了的邮箱 |
| 含`\r`、`\n`或`\0`的带引号本地部分 | 头部注入 |

长度界限：`MAX_LOCAL_PART_LEN` 64、`MAX_DOMAIN_LABEL_LEN` 63、`MAX_DOMAIN_LEN` 255。

**本地`From`必须归已认证用户所有。**一次已认证的提交，如果`MAIL FROM`落在本地域，就只
允许点名`user_id`属于该会话的那一行`mailboxes`；否则返回
`550 5.7.1 Sender address rejected: not owned by user`。没有这项检查，任何用户都能以域内
任何其他用户的身份发信，而这正是 SPF 与 DMARC 在*接收*端存在的原因。

### 6.4 别名与 catch-all

`aliases.target`是一个完整地址，或者一个表示「同域」的裸本地部分。`domains.catch_all`是
一个本地部分，用于接收发往不存在邮箱的邮件。两者都由管理员控制，从不由用户控制，并且都在
直接查询邮箱*之后*解析，因此catch-all永远不会遮蔽一个真实地址。

catch-all天生就是垃圾邮件放大面：它会为任意的本地部分收信。它默认关闭（`catch_all`为
`NULL`），只有在有理由时才应打开。

---

## 7. TLS

### 7.1 策略

| 监听器 | 配置 | 策略 |
|---|---|---|
| SMTP 25 | `smtp.port` | 明文，但提供`STARTTLS`，属机会式，因为拒绝明文收信会丢邮件 |
| 提交 587 | `smtp.submission_port` | `STARTTLS`；`smtp.require_auth_on_submission`强制 AUTH，`smtp.require_tls_for_auth`强制在 AUTH 之前完成 TLS |
| SMTPS 465 | `smtp.smtps_port` | 从第一个八位组起就是隐式 TLS |
| IMAP 143 | `imap.port` | 明文，支持`STARTTLS` |
| IMAPS 993 | `imap.imaps_port` | 隐式 TLS |
| HTTPS | `api.tls_port` | `0`表示由反向代理终止 TLS |

`tls.enabled`统管这一切。当`tls.enabled = false`而`smtp.smtps_port != 0`、
`imap.imaps_port != 0`或`api.tls_port != 0`时，`Config::validate()`会拒绝启动：一个配置了
TLS 端口却关掉了 TLS 的监听器，会在客户端以为已加密的端口上提供明文。

### 7.2 只用 rustls

工作区中每一个支持 TLS 的依赖都钉在 rustls 上：`rustls`、`tokio-rustls`、
`rustls-pemfile`、`rustls-pki-types`、`webpki-roots`，以及
`default-features = false, features = ["rustls-tls", …]`的`reqwest`。`AGENTS.md` §1.1
禁止引入任何会拉进`native-tls`、`openssl`或`schannel`的 crate，并给出两条彼此一致的理由：
本开发主机上的 Windows `schannel`栈会以`SEC_E_NO_CREDENTIALS`失败；而对一台自己终止 SMTP、
IMAP 与 HTTPS 的服务器来说，内存安全且带显式密码套件策略的 TLS 实现才是正确选择。

### 7.3 证书处理

| 键 | 含义 |
|---|---|
| `tls.cert_path` | PEM 包：叶证书后跟中间证书 |
| `tls.key_path` | PEM 私钥，PKCS#8 或 PKCS#1 |
| `tls.self_signed_fallback` | 未配置 PEM 时在启动时生成一张证书 |
| `tls.allow_insecure_dev_mode` | 使用回退所必需 |
| `tls.min_version` | `"1.2"`或`"1.3"`；其他任何值都拒绝启动 |
| `tls.use_platform_roots` | 发信校验时也信任操作系统安装的根证书 |

`self_signed_fallback`明确只用于本地开发与 CI。它由`rcgen`生成，并被两重门控：
`tls.allow_insecure_dev_mode`必须为真，且`Config::validate()`会拒绝其他组合：

```text
tls.self_signed_fallback requires tls.allow_insecure_dev_mode = true
```

一台MX用自签名证书就无法被任何发信服务器校验，于是每一次发信 TLS 握手都会失败，更糟的是，
运维者可能被引诱去在别处关掉校验。

### 7.4 TLS 买到了什么、没买到什么

* **提交（587/465）与 IMAP（993）：**凭据和内容在链路上受到保护。这是用户数据的安全边界。
* **收信的 25 端口：**机会式。不做 TLS 的发信 MTA 就发明文，而 Ferroma 要么接受，要么丢
  邮件。25 端口上的邮件内容应被假定可被网络观察者读取。端到端加密是唯一的防御，而
  Ferroma 没有实现它。
* **MTA-STS**对遵守它的发信方抬高了门槛；DNS 记录与策略文件规定在
  [deployment.md](deployment.md) §2。

### 7.5 `require_tls_for_auth`与`require_tls_for_login`

两者默认都是`false`，这样一次裸的`cargo run`无需证书就能跑起来，而两者在
`docker-compose.prod.yml`中都被设为`true`：

```yaml
FERROMA__SMTP__REQUIRE_TLS_FOR_AUTH: 'true'
FERROMA__IMAP__REQUIRE_TLS_FOR_LOGIN: 'true'
```

`AUTH PLAIN`、`AUTH LOGIN`与 IMAP 的`LOGIN`都会以 base64 或明文发送密码。在未加密的
套接字上，被动观察者读得到。生产部署若把它们留作 false，距离账号被接管只差一次`tcpdump`，
而拒绝是显式的而不是静默的：SMTP 上返回
`538 5.7.11 Encryption required for requested authentication mechanism`，IMAP 上返回
`NO [PRIVACYREQUIRED]`。

`Config::allows_plaintext_auth()`无需重新推导就能回答整个配置的这个问题。

---

## 8. SPF、DKIM 与 DMARC

> 已实现。`ferroma-smtp`的`spf`、`dkim`与`dmarc`模块（`crates/ferroma-smtp/src/spf.rs`、
> `dkim.rs`、`dmarc.rs`，在`inbound.rs`中组装，并在`server.rs`中于`DATA`之后评估）
> 承担了这一职责。配置键位于`config/ferroma.toml`的`[dkim]`与`[policy]`中，也存在于
> `crates/ferroma-core/src/config.rs`的`DkimConfig` / `PolicyConfig`中。

### 8.1 收信

| 校验 | 配置 | 失败时 |
|---|---|---|
| SPF（RFC 7208） | `policy.spf_enabled`、`policy.spf_max_lookups`（10） | 本身不拒绝——判定结果写入`Authentication-Results`并汇入 DMARC |
| DKIM 校验（RFC 6376） | `dkim.verify_inbound` | 本身不拒绝——判定结果汇入 DMARC 对齐检查 |
| DMARC（RFC 7489） | `policy.dmarc_enabled`、`policy.dmarc_failure_action` | `policy.dmarc_failure_action`是`none`、`quarantine`或`reject`；默认是`"quarantine"`，投进`Junk`；发布或本地为`reject`时返回`550 5.7.1 Message rejected by the DMARC policy of <domain>` |
| `Authentication-Results` | `policy.add_auth_results` | 该头字段会被前置插入判定结果 |

`dmarc_failure_action`默认取`quarantine`而不是`reject`，有一个具体原因：对一封*被转发*的
邮件（邮件列表、校友转发器）评估 DMARC `p=reject`，通常会 SPF 与 DKIM 双双失败，而这封
邮件是合法的。隔离把它放进`Junk`，用户还能找到；拒绝则直接丢掉。已经测量过自己转发容忍度
的运维者可以提高它。

`policy.spf_max_lookups`（10）是 RFC 7208 §4.6.4 的限制；它存在，是因为一条 SPF 记录可以被
构造成强制产生无上限次数的 DNS 查询，这是同时针对 Ferroma 和解析器的拒绝服务途径。

### 8.2 发信

| 控制 | 配置 |
|---|---|
| 对哪些域签名 | `dkim.domain`（单个域），未设置时对每一个本地域签名 |
| 选择器 | `dkim.selector`（默认`default`），发布在`<selector>._domainkey.<domain>` |
| 签名密钥 | `dkim.private_key_path`，或`domains.dkim_private_key` |
| 规范化 | `dkim.canonicalization`，`"relaxed"`或`"simple"` |
| 参与签名的头字段 | `dkim.headers_to_sign`：`From`、`To`、`Cc`、`Subject`、`Date`、`Message-ID`、`MIME-Version`、`Content-Type`、`Content-Transfer-Encoding`、`Reply-To`、`In-Reply-To`、`References` |

`From`被签名不是可选项：一个不覆盖`From`的 DKIM 签名可以被换上别的发件人重放，而这正是
DMARC 对齐要检查的东西。`headers_to_sign`把它列在第一位，这个列表与规范§16的流程一致
（规范化 → 头字段哈希 → 正文哈希 → 签名 → `DKIM-Signature`）。

私钥绝不能被记入日志、绝不能在 API 响应里导出到公开记录之外、也绝不能被放进保护强度低于
数据库的备份里。`GET /api/v1/domains/:id/dkim`只返回**公开**记录：

```json
{ "selector": "default", "record_name": "default._domainkey.example.com", "record_type": "TXT", "record_value": "v=DKIM1; k=rsa; p=MIIBIjANBg…" }
```

### 8.3 与邮件相关的 DNS 安全

| 记录 | 安全作用 |
|---|---|
| **PTR** | PTR 缺失或不匹配是合法服务器被拒的最常见单一原因。它是投递率控制，不是认证控制，这也是它在[deployment.md](deployment.md) §2的原因 |
| **SPF** | 授权发信主机；`-all`是严格形式 |
| **DKIM** | 证明邮件由该域签名，且未被改动 |
| **DMARC** | 告诉接收方在 SPF 与 DKIM 双双失败时该怎么做，以及如何上报（`rua`） |
| **MTA-STS** | 要求发往该域的收信必须用 TLS，挫败降级 |
| **CAA** | 限制哪些 CA 可以为该域签发证书 |
| **DNSSEC** | Ferroma 未实现也不要求；在区域支持它的地方，它保护上面这些记录 |

`[dns]`配置Ferroma自己使用的解析器：显式的`resolvers`、`timeout_secs`（5）、`attempts`（3）、
`cache_ttl_secs`（300）、`negative_ttl_secs`（60）、`tcp_fallback`。请使用你信任的解析器：
控制了解析器的攻击者可以伪造收件人域的 MX 记录，从而收走你正在投递的邮件。

---

## 9. 限制与请求面

完整的逐项限制表在[smtp.md](smtp.md) §7、[imap.md](imap.md) §10与[api.md](api.md) §1.6。
以下是安全相关的部分摘要：

| 限制 | 键 | 默认值 | 它约束的威胁 |
|---|---|---|---|
| 邮件大小 | `limits.max_message_size` | 25 MiB | 磁盘耗尽、每连接内存 |
| 每事务收件人数 | `limits.max_recipients` | 100 | 放大：一条连接，多个受害者 |
| 并发连接数 | `limits.max_connections` | 100 | 资源耗尽 |
| 每 IP 连接数 | `limits.max_connections_per_ip` | 10 | 单一来源独占监听器 |
| 收信命令数/分钟/IP | `limits.smtp_rate_limit` | 100 | 命令洪水 |
| 提交数/小时/账号 | `limits.submission_rate_limit` | 50 | 被攻陷的账号变成垃圾邮件炮台 |
| 邮件数/天/账号 | `limits.daily_send_limit` | 500 | 同上，节奏更慢 |
| 邮箱配额 | `limits.mailbox_quota` | 1 GiB | 单个用户塞满磁盘 |
| 失败登录 | `limits.max_failed_logins` | 10 | 密码猜测 |
| 锁定窗口 | `limits.login_lockout_secs` | 900 | — |
| MIME 嵌套深度 | `limits.max_mime_depth` | 20 | 解析器递归 |
| 每封邮件的部分数 | `ParseLimits::max_parts` | 200 | 解析器扇出 |
| IMAP 每条命令的抓取量 | `limits.max_fetch_messages` | 5000 | 在超大文件夹上的一次`FETCH 1:*` |
| IMAP `APPEND`字面量 | `imap.max_append_size` | 25 MiB | — |
| HTTP 请求正文 | `api.max_request_size` | 25 MiB | — |
| 认证命令 | `limits.idle_timeout_secs`、`limits.data_timeout_secs` | 300 / 600 | slowloris |

畸形输入必须是**限制**失败，而不是 panic。`ParseLimits`：

```rust
pub struct ParseLimits { pub max_depth: usize, pub max_parts: usize,
                        pub max_part_size: usize, pub max_message_size: usize }
// 默认值：20 / 200 / 26214400 / 26214400
```

`ParsedMessage::parse_with_limits`返回`FerromaError::LimitExceeded`，而不是无界递归；
`ParseLimits::from_limits(&Limits)`从平台配置派生出这些值，于是修改它们只有一个地方。
`AGENTS.md` §4.4禁止在对端输入上使用`unwrap()`，解析器就是原因。

---

## 10. 邮件处理：MIME、HTML 与逃生通道

### 10.1 全量解析

> 解析是*全量*的：任意字节串都会产出一个`ParsedMessage`。唯一的错误是`ParseLimits`中显式
> 的资源限制，因为一台拒绝自己无法渲染的邮件的邮件服务器会丢掉真实邮件。

（引自`crates/ferroma-mail/src/message.rs`）

这既是可用性属性，也是安全属性。一个会在某类输入上失败的解析器会造出一类被静默丢弃的邮件，
攻击者只要追加一段字节序列，就能用它压掉一封邮件（比如说一封密码重置邮件）。MIME 解码同样
宽容：`decode_base64`、`decode_quoted_printable`与`decode_charset`返回尽力而为的结果，只有
真正无法解码的输入才有显式错误。

### 10.2 HTML 净化

已实现。`crates/ferroma-api/src/routes/mail/store.rs` 中的 `sanitize_html` 在
HTML 正文入库时运行，由 `config/ferroma.toml` 的 `security.sanitize_html` 门控。
规则是：

* **绝不把原始`html_body`渲染进有特权的源。**Webmail 与 Admin 这两个 SPA 与 API 同源提供，
  因此一个未净化的 HTML 正文，就是对能调用 admin API 的会话的存储型 XSS。
* **在沙箱化的 iframe 中渲染**，使用`sandbox="allow-popups"`且不带
  `allow-scripts`/`allow-same-origin`，并默认拦截远端内容，使跟踪像素无法确认一封邮件被读过。
* **官方客户端不是浏览器。**它的阅读器不应执行邮件里的任何东西；HTML 渲染走同一个净化器，
  净化器缺席时回退到纯文本部分。
* **附件绝不以 HTML 内联渲染。**`Content-Type`由攻击者控制；附件上的`text/html`不是运行它
  的理由。`attachments.is_inline`与`content_id`是显示提示，不是信任凭据。

净化器落地时必须剥掉`<script>`、带`expression`的`<style>`、`<iframe>`、`<object>`、
`<embed>`、`<form>`、`<base>`、`<meta http-equiv>`、所有`on*`属性以及所有
`javascript:`/`data:` URL，并且必须在服务端运行（客户端不可信任，而 API 会喂给多个客户端）。

### 10.3 Ferroma 不对邮件内容做的事

* 它不执行邮件里的任何东西。
* 它从不跟进邮件中的链接，不预取、不展开链接、不做会抓取远端内容的图片代理。
* 它不把邮件反序列化成能构造路径、SQL 片段或 shell 参数的类型。邮件内容以绑定参数的形式
  到达数据库（`sqlx::query_as`，从不用`sqlx::query!`，也从不做字符串拼接），而到达文件系统
  只经过`sanitize_component`（§12）。
* 它不按发件人选定的名字存储附件。二进制对象的路径是内容的 SHA-256；`attachments.filename`
  是用于显示的元数据，并且在进入`Content-Disposition`头字段之前会被净化。

---

## 11. Maildir 与二进制存储中的路径穿越防御

它们存在，而它们正是一行被篡改的数据库记录读不到`/etc/passwd`的原因。完整细节见
[storage.md](storage.md) §7。

### 11.1 `sanitize_component`

```rust
// crates/ferroma-storage/src/maildir.rs
pub fn sanitize_component(component: &str) -> Result<String> {
    let trimmed = component.trim();
    if trimmed.is_empty() { return Err(StorageError::Invalid("empty path component".into())); }
    if trimmed == "." || trimmed == ".." {
        return Err(StorageError::Invalid(format!("invalid path component: {trimmed}")));
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains('\0') {
        return Err(StorageError::Invalid(format!("path component contains a separator: {trimmed}")));
    }
    if trimmed.contains(':') {
        return Err(StorageError::Invalid(format!("path component contains a colon: {trimmed}")));
    }
    Ok(trimmed.to_string())
}
```

它被施加于域名、本地部分（在`Maildir::mailbox_dir`中）、主机名（在`Maildir::new`中）以及
文件夹名的每一段（在`maildir_folder_name`中）。有一个测试枚举了那些有意思的输入：
`["..", ".", "a/b", "a\\b", "", "  ", "a:b", "x\0y"]`必须全部被拒绝。

对`:`的拒绝不是装饰性的：它是 Maildir 的信息分隔符，而在 Windows 上它会引入 NTFS 备用
数据流（[imap.md](imap.md) §10）。

### 11.2 `absolute`

两个存储层各暴露一个，它是唯一把存储的`storage_path`变成真实路径的函数：

```rust
// Maildir::absolute 与 AttachmentStore::absolute
if candidate.is_absolute() {
    return Err(StorageError::Invalid(format!("storage path must be relative: {relative_path}")));
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
```

每一次读、写、删除、`set_flags`与`exists`都会先调用它。已测：

```rust
assert!(m.absolute("../../etc/passwd").is_err());
assert!(m.absolute("/etc/passwd").is_err());
assert!(s.absolute("../../secret").is_err());
assert!(!s.exists("../../secret"));
```

`Component::Prefix(_)`正是阻止 Windows 上的`C:\Windows\...`被当作相对路径并拼接到根目录上
的东西。

### 11.3 残留风险

两道门控校验的都是*字符串*。由其他进程放进邮件根目录的符号链接不会被发现，因为`std::fs`
会跟随符号链接。Ferroma 在任何地方都不创建符号链接，而邮件根目录应当归服务用户所有、
没有其他写入者：`Dockerfile`以 uid 10001（`ferroma`）运行，并执行
`chown -R ferroma:ferroma /var/lib/ferroma`。能往邮件根目录里写的攻击者早已通过其他手段
赢了，但值得说明：这项控制针对的是被攻陷的*数据库*，不是被攻陷的*主机*。

---

## 12. 日志：记录什么、永不记录什么

### 12.1 永不记录

`AGENTS.md` §4.7与规范§40：

| 永不记录 | 规则在何处被遵守 |
|---|---|
| 任何形式的密码 | `AuthService`校验后丢弃；`PasswordHasher`从不返回明文 |
| 刷新令牌与会话令牌 | 只有`hash_token(raw)`被存储；`looks_like_opaque_token`的存在使令牌能被认出以便清洗 |
| 访问令牌 | `jti`声明能指认一枚令牌而不泄露它 |
| 私钥 | DKIM 与 TLS 密钥由签名者和监听器读取 |
| **完整邮件正文** | `MailReceived`携带`snippet`，而不是正文（[architecture.md](architecture.md) §6） |
| `info`级别的 SMTP/IMAP 协议载荷 | 协议调试是`debug`，那不是生产级别 |
| 静态日志中的邮件主题 | `database.log_statements`默认是`false`，配置注释里把理由写明了：*「它会打印邮件主题」* |

`crates/ferroma-core/src/logging.rs`在 crate 文档中写明了这一点：

> 密码、AUTH 令牌、私钥与完整邮件正文永不记录。

### 12.2 记录什么

规范§40中的 SMTP 会话字段，正是让一次事故可以在不读任何人邮件的前提下被调查的那些字段：

```text
connection_id   remote_ip   helo   authenticated_user
sender          recipient   message_id   result   duration
```

`connection_id`是一个 UUID，它也会出现在Ferroma前置插入的`Received:`头字段中，因此一行
日志与一个头字段可以关联起来（[smtp.md](smtp.md) §10），运维者无需打开邮件就能回答
「这封邮件从哪来」。

### 12.3 日志配置

| 键 | 默认值 | 安全提示 |
|---|---|---|
| `server.log_level` | `"info"` | 在`ferroma_smtp`或`ferroma_imap`上用`debug`/`trace`会记录协议细节；生产环境请保持关闭 |
| `server.log_format` | `"text"` | 投递日志时用`"json"`；两种格式的字段相同 |
| `database.log_statements` | `false` | **生产环境绝不启用** |
| `NOISY_DEFAULTS` | `hyper=warn,h2=warn,sqlx=warn,hickory_resolver=warn,hickory_proto=warn,rustls=warn,tokio_tungstenite=warn` | 除非运维者显式选择加入，第三方 crate 都被保持在`warn`，因此一个依赖无法开始打印请求数据 |
| `RUST_LOG` / `FERROMA_LOG_LEVEL` | — | `logging::init_for_tests`会读取它们；测试运行默认是安静的 |

`logging::build_filter`保留运维者的指令，只对指令尚未提及的目标追加嘈杂 crate 的默认值，
因此`sqlx=debug`会被尊重，而不是被覆盖。

### 12.4 审计轨迹

`audit_logs`与运行日志分开，且意在持久：`actor_user_id`（外键`ON DELETE SET NULL`，因此
数据行能在账号消失后存活）、`action`、`target_type`、`target_id`、`ip`、`user_agent`、
`details JSONB`、`created_at`。管理动作（创建用户、删除域、吊销设备、修改设置）属于这里，
而不属于一条会被轮转掉的`tracing`行。

---

## 13. 密钥管理

| 密钥 | 必须放在哪里 | 绝不能放在哪里 |
|---|---|---|
| `api.jwt_secret` / `FERROMA_JWT_SECRET` | 环境变量，或由密钥管理器以环境变量注入；两者都没有配置时，服务器会生成一个并写入 `<data_dir>/jwt_secret` | 版本控制里的配置文件；放在别人可读位置的 `.env` 或 `ferroma-data` 归档 —— 不再有随附的备份工具，因此没有任何东西替你排除凭据 |
| `POSTGRES_PASSWORD` | `.env`，已 gitignore，或密钥管理器 | compose 文件，它们会插值`${POSTGRES_PASSWORD:?…}`并在缺少它时拒绝启动 |
| DKIM 私钥 | 只读挂载上的`dkim.private_key_path`，或`domains.dkim_private_key` | 公开的`GET /api/v1/domains/:id/dkim`响应，它只返回`p=`公钥 |
| TLS 私钥 | `tls.key_path`，只读挂载（`./tls:/etc/ferroma/tls:ro`） | 镜像里 |
| 用户密码 | 任何地方都不放，永远不放 | — |

不再有随附的备份工具，因此一份备份的保护完全由运维者负责。这一点要紧，因为数据卷的
备份是含密的：里面有 DKIM 私钥，以及当密钥是被生成而非配置时的 `<data_dir>/jwt_secret`；
卷里的 `<data_dir>/database.json` 记住了数据库地址，URL 里可能就带着凭据。请加密这份
归档，或像对待数据库本身一样严格控制它的访问。

仓库已经强制执行的实践：

* **单机 compose 在缺少密钥时快速失败。** `docker-compose.yml` 里的
  `${FERROMA_JWT_SECRET:?set FERROMA_JWT_SECRET in .env}` 与 `${POSTGRES_PASSWORD:?…}`
  意味着缺少密钥的部署根本不会启动，而不是带着默认值启动。`docker-compose.prod.yml`
  刻意不设 JWT 密钥：服务器首次启动时会生成一个并写进数据卷，这也是那个卷含密的
  原因之一。
* **备份是运维者的事，凭据也一样。** 不再有脚本替你排除 `*.env` 或 `credentials*`；
  `.env` 或 `ferroma-data` 卷的归档必须加密，并像它所含的密钥一样存放。
* **`.env.example`只带占位符和生成命令**，从不带真实取值。
* **数据库容器不对外发布。**`docker-compose.yml`在内部网络上用`expose: ['5432']`，而不是
  映射宿主端口。
* **配置以只读方式挂载。**`./config/ferroma.toml:/etc/ferroma/ferroma.toml:ro`。
* **容器以非特权身份运行。**`USER ferroma`，uid 10001，`/var/lib/ferroma`已改归它所有。

需要轮换时：

| 密钥 | 轮换代价 |
|---|---|
| `FERROMA_JWT_SECRET` | 每一个访问令牌都失效；客户端刷新后继续。用户不会被登出（刷新令牌是不透明的，不受影响） |
| `POSTGRES_PASSWORD` | 更新`.env`，重启两个服务 |
| DKIM 密钥 | 先发布新选择器的 TXT 记录，再切换`dkim.selector`。在用它签名的邮件老去之前不要删除旧记录（一周安全；30 天更安全） |
| TLS 证书 | 重新加载；监听器在启动时读取 PEM |

---

## 14. 控制项 → 实现位置对照表

| # | 控制项 | 实现 | 状态 |
|---|---|---|---|
| 1 | Argon2id，m=19456 t=2 p=1 | `Argon2Params::default`、`PasswordHasher`（`crates/ferroma-auth/src/password.rs`） | 已实现 |
| 2 | 登录时透明重哈希 | `PasswordHasher::needs_rehash`、`AuthService::login` | 已实现 |
| 3 | 密码长度策略 | `validate_password`、`MIN_PASSWORD_LENGTH`、`MAX_PASSWORD_LENGTH` | 已实现 |
| 4 | 常数时间的密码校验 | `PasswordHasher::verify`（argon2 + `subtle`） | 已实现 |
| 5 | 访问令牌：HS256，无`alg`协商 | `TokenService::verify_access_at` | 已实现 |
| 6 | 解析声明之前先校验签名 | `verify_access_at`的顺序 | 已实现 |
| 7 | 访问令牌 TTL | `api.access_token_ttl_secs`（3600）、`TokenService::access_ttl_secs` | 已实现 |
| 8 | 刷新令牌轮换 | `AuthService::refresh` | 已实现 |
| 9 | 盗用检测：复用即吊销整个家族 | `AuthService::refresh` + `revoke_all_for_user` | 已实现 |
| 10 | 刷新令牌只以哈希形式存储 | `sessions.token_hash`、`hash_token` | 已实现 |
| 11 | 会话吊销立即生效 | `AuthService::authenticate`检查会话行 | 已实现 |
| 12 | 改密码吊销其他会话 | `AuthService::change_password` | 已实现 |
| 13 | 设备注册与吊销 | `devices`、`AuthService::register_device` / `revoke_device`、`Event::device_revoked` | 已实现 |
| 14 | 空闲会话策略 | `client.session_idle_days`（90） | 已实现（清扫可经`AuthService::purge_expired_sessions`调用，尚未接入定时任务） |
| 15 | 哈希之前的按 IP 登录限流 | `AuthService::login` + `LoginAttemptsRepository::count_failures_for_ip` | 已实现 |
| 16 | 按账号锁定 | `users.failed_logins`、`users.locked_until`、`record_login_failure`、`User::is_login_allowed` | 已实现 |
| 17 | 不枚举账号 | 未知/错误/停用一律返回相同的`invalid_credentials()` | 已实现 |
| 18 | 登录尝试审计轨迹 | `login_attempts`、`AuthService::record_attempt` | 已实现 |
| 19 | 开放中继预防 | `SmtpSession::may_relay`、`smtp.require_auth_on_submission`、`550 5.7.1 Relaying denied` | 已实现 |
| 20 | 收件人校验 | `MailboxesRepository::find_by_address`、`DomainsRepository`、`AliasesRepository`、`domains.catch_all`、`550 5.1.1 User unknown` | 已实现 |
| 21 | 发件人地址语法校验 | `EmailAddress::parse`、`validate_local_part`、`validate_domain` | 已实现 |
| 22 | 本地`From`必须归该用户所有 | API 发送路径中的`mailboxes.user_id`检查（`crates/ferroma-api/src/routes/mail/store.rs` 中的 `resolve_sender`） | 已实现（API 路径）；SMTP 提交监听器不会重新核对`MAIL FROM` |
| 23 | 地址大小写归一 | schema 的`CHECK` + `normalise_domain` + `to_lowercase` | 已实现 |
| 24 | SPF | `policy.spf_enabled`、`policy.spf_max_lookups`、`spf.rs` | 已实现 |
| 25 | DKIM 校验 | `dkim.verify_inbound`、`dkim.rs`（`DkimVerifier`） | 已实现 |
| 26 | DKIM 签名 | `[dkim]`块、`DkimConfig`、`DkimSigner` | 已实现 |
| 27 | DMARC | `policy.dmarc_enabled`、`policy.dmarc_failure_action`、`dmarc.rs` + `inbound.rs` | 已实现 |
| 28 | `Authentication-Results` | `policy.add_auth_results` | 已实现 |
| 29 | 凡能终止TLS处都用TLS | `[tls]`、`tls.min_version`、`smtps_port`、`imaps_port`、`api.tls_port` | 已实现 |
| 30 | 只用 rustls | 工作区`Cargo.toml`的钉版；`AGENTS.md` §1.1 | 已实现 |
| 31 | 自签名证书两重门控 | `Config::validate()` + `tls.allow_insecure_dev_mode` | 已实现 |
| 32 | 生产环境无明文 AUTH/LOGIN | `smtp.require_tls_for_auth`（`may_auth` → `538 5.7.11`）、`imap.require_tls_for_login` | 已实现 |
| 33 | 邮件大小、收件人、连接数与速率限制 | `Limits`、`Limits::validate()`、`[limits]`，在 SMTP 命令循环与 API 中强制 | 已实现 |
| 34 | 解析限制，没有无界递归 | `ParseLimits`、`ParsedMessage::parse_with_limits` | 已实现 |
| 35 | 全量解析：不因解析错误丢邮件 | `ferroma-mail`解析器的设计 | 已实现 |
| 36 | HTML 净化 | `security.sanitize_html`、`ferroma-api` `store.rs` 中的 `sanitize_html` | 已实现 |
| 37 | 路径穿越防御，邮件根目录 | `sanitize_component`、`Maildir::absolute` | 已实现 |
| 38 | 路径穿越防御，二进制存储 | `AttachmentStore::absolute`、`path_for_digest`校验 | 已实现 |
| 39 | 不对对端输入使用`unwrap()` | 约定，`AGENTS.md` §4.4 | 已实现 |
| 40 | 只用绑定的 SQL 参数 | `sqlx::query_as`，不用`sqlx::query!`，见`AGENTS.md` §4.3 | 已实现 |
| 41 | 密钥永不记录 | `logging.rs`契约、`looks_like_opaque_token` | 已实现 |
| 42 | 关闭`log_statements` | `database.log_statements = false` | 已实现 |
| 43 | 嘈杂依赖被压在`warn` | `NOISY_DEFAULTS`、`build_filter` | 已实现 |
| 44 | 持久审计轨迹 | `audit_logs`、`AuditRepository` | 已实现 |
| 45 | 启动时强制要求密钥 | compose 的`${VAR:?}`插值 | 已实现 |
| 46 | 容器以非特权身份运行 | `Dockerfile`的`USER ferroma`，uid 10001 | 已实现 |
| 47 | 数据库不对外发布 | `postgres`服务上用`expose`而不是`ports` | 已实现 |
| 48 | 生产环境的`Secure` cookie | `api.secure_cookies`，由`docker-compose.prod.yml`设为 true | 已实现（配置） |
| 49 | 默认关闭 CORS | `api.cors_origins = []`（仅同源） | 已实现（配置） |
| 50 | 仅在可信时使用`X-Forwarded-For` | `api.trust_proxy_headers = false`默认 | 已实现（配置） |
| 51 | 未认证 HTTP 到不了数据 | 除`/health`、`/version`、`/.well-known/*`外，每条`/api/v1`路由都要 bearer/cookie | 已实现 |
| 52 | Admin 端点要求`is_admin` | `Authenticated::is_admin()`检查 | 已实现 |

这些行背后的接线在[api.md](api.md)与[smtp.md](smtp.md)中描述。

---

## 15. 已知缺口

Ferroma v1 中刻意的省略。每一条都是决定，不是疏忽；「缓解」一栏说的是运维者应当改做什么。

### 15.1 没有杀毒或恶意软件扫描

Ferroma 不扫描附件。没有 ClamAV 集成、没有`clamd`套接字、没有`virus_scan`配置键。附件被
存储只是因为它到了；它是否有恶意是收件人的问题。

**为什么 v1 可以接受：**杀毒引擎是一个庞大、有状态、有自己的更新通道和自己的失败模式的
依赖，而一个悄悄停止更新的扫描器比没有扫描器更糟，因为它制造虚假的信心。它在可扩展性清单
上（规范§57，「病毒扫描」）。

**运维者的缓解措施：**跑一个`clamd`，在带外扫描邮件根目录，或者让收信经过一道网关。在接收
客户端屏蔽可执行附件类型，那才是用户真正打开它们的地方。

### 15.2 没有贝叶斯或启发式垃圾邮件过滤

没有内容分类器、没有`X-Spam-Score`、没有`spamassassin`集成。唯一的收信过滤是：

* SPF/DKIM/DMARC判定，它们认证发件人，不分类内容；
* `Junk`文件夹及其`special_use = \Junk`，以及 DMARC `quarantine`把邮件投进它；
* 速率限制与连接限制，它们约束的是量，不是内容。

**为什么：**贝叶斯过滤器需要语料、按用户的训练和一个调参闭环，而调得不好的过滤器会产生
误报、丢掉真实邮件，规范§54把这一点认定为风险。发布一个悄悄吃掉发票的过滤器，比不发布
更糟。

**运维者的缓解措施：**在前面放一道过滤网关，或者用托管过滤服务。`Junk`文件夹与`\Junk`
特殊用途标记已经在 schema 里（`special_use`的`CHECK`），因此日后加过滤器不需要迁移。

### 15.3 没有 OIDC、没有 OAuth2、没有 2FA

规范§15把`OAuth2`、`OIDC`与`2FA`列在「后续」之下，§57又重复了一遍。Ferroma v1只支持
密码认证，经由：

* `POST /api/v1/auth/login`与`/api/v1/client/auth/login`，
* SMTP 的`AUTH PLAIN` / `AUTH LOGIN`，
* IMAP 的`LOGIN` / `AUTHENTICATE PLAIN` / `AUTHENTICATE LOGIN`。

没有 TOTP、没有 WebAuthn、没有恢复码、没有`mfa_required`标志。一个被钓走的密码就是完整的
账号接管，只受登录限流和`limits.max_failed_logins`约束。

**运维者的缓解措施：**对 Webmail 这一面，在前面放一个执行 2FA 并传递已认证身份的 SSO
代理；对 SMTP/IMAP，除了在Ferroma之外管理应用专用密码，没有诚实的缓解办法。不要把一次
Ferroma部署说成「受 2FA 保护」。

### 15.4 没有跨进程事件总线

`EventBus`是一个进程内对象（`crates/ferroma-events/src/bus.rs`）。没有 Redis、NATS 或
PostgreSQL 的`LISTEN`/`NOTIFY`后端。

后果：

| 情形 | 结果 |
|---|---|
| 两个`ferroma`进程共用一个数据库 | 两条互不相干的事件流 |
| 进程 A 上的 WebSocket 客户端 | 收不到进程 B 产生的事件 |
| 进程 A 上正在`IDLE`的 IMAP 会话 | 不会被推送通过进程 B 做出的变更 |
| 重连 / 下一次同步 | 变更**会**被投递，因为`change_log`在共享数据库里 |

因此失败模式是通知延迟，而不是数据丢失，而这正是让单进程总线在 v1 可接受的性质。但这
意味着水平扩展**不是**一次配置变更：在负载均衡后面跑两个副本，用户的实时体验取决于他落在
了哪个副本上。

**为什么：**broker 是又一个需要运维、加固和监控的有状态服务，而规范的第一版 Docker 栈
（§41）明确就是 Ferroma 加 PostgreSQL，Redis 被放在「后期」。

**运维者的缓解措施：**只跑一个`ferroma`进程。如果需要更多容量，先扩数据库和存储；两者都
比事件扇出更可能是瓶颈。

### 15.5 更小的缺口，直说

| 缺口 | 后果 | 缓解 |
|---|---|---|
| 没有`SEARCH BODY` / `TEXT`（[imap.md](imap.md) §8） | 搜索正文的客户端从服务器得不到结果 | 在客户端里搜，或用 API 的头字段/主题搜索 |
| 没有 S/MIME 或 PGP | Ferroma 无法端到端加密或验证签名 | 用一个能做的客户端 |
| 没有 DKIM ARC 封签 | 被转发的邮件失去它的认证 | 把转发留给能封签的客户端 |
| 没有 Sieve 或服务端规则 | 过滤只能在客户端做 | — |
| 没有泄露语料库密码检查 | 用户可能设置一个已知被泄露的密码 | 在创建账号时按你信任的清单强制执行 |
| 没有针对`AUTH`的按用户 IP 白名单 | 偷来的密码在任何地方都能用 | 设备吊销，并监控`sessions.ip` |
| 没有 DMARC 聚合报告处理 | 除非运维者去读，`rua`报告无人问津 | 把`rua`指向你会查看的邮箱 |
| webhook 没有请求签名 | webhook 一旦到来，就是未认证的 HTTP POST | 不要在不可信网络上启用 webhook |
| `GET /.well-known/ferroma`没有速率限制 | 一个未认证端点可被用于侦察和加载 | 如果在意，就在前面加一层代理限制 |
| 收信时不校验 PTR | 来自没有 PTR 的主机的邮件仍被接受 | SPF/DKIM/DMARC 与一道网关 |
| 没有告警 | 队列增长、磁盘写满或登录洪水，只有盯着看的人才知道 | 监控健康检查端点与`mail_queue_status_idx`计数，见[deployment.md](deployment.md) §10 |

---

## 16. 相关文档

| 主题 | 文档 |
|---|---|
| 端点级错误映射与认证头 | [api.md](api.md) §1 |
| FCP 认证、客户端侧的令牌轮换 | [fcp.md](fcp.md) §2、§11 |
| 应答码、中继策略、TLS 端口角色、`Received:`头字段 | [smtp.md](smtp.md) |
| `require_tls_for_login`、标志处理、`APPEND`限制 | [imap.md](imap.md) |
| 路径穿越函数的全文、配额、备份与恢复 | [storage.md](storage.md) §7、§5、§8 |
| 幂等性、墓碑保留、失败矩阵 | [sync.md](sync.md) |
| DNS 记录、TLS 终止、`.env`中的密钥、加固检查清单 | [deployment.md](deployment.md) |
| crate 依赖图、分层规则、事件总线的范围 | [architecture.md](architecture.md) |
| 构建期的 rustls 约束与约定 | [../AGENTS.md](../../AGENTS.md) |
