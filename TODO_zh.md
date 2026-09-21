# TODO — 尚未完成的事

英文原版见 [`TODO.md`](TODO.md)，本文件是它的中文译本。

本队列只收录仓库自己已经承认尚未完成的事。素材来自 [`CHANGELOG_zh.md`](CHANGELOG_zh.md)
里仍未了结的条目、文档中仍然保留的 `_(planned)_` 断言，以及文档体系自身的缺口。这里没有
新想法；已经发布的内容见 [`CHANGELOG_zh.md`](CHANGELOG_zh.md)，已退休内容的取舍理由见其
`## [未发布]` 一节。

## 下一版（0.1.9）

- 七篇文档仍在描述骨架：里面留有写于各 crate 尚不存在之时的 `_(planned)_` 断言，其中四篇——`imap.md`、`security.md`、`smtp.md`、`sync.md`——开头仍挂着把已实现 crate 称作未实现的状态横幅——[README — 带入 0.1.9 的待办](README_zh.md#带入-019-的待办)、[CHANGELOG_zh — 带入 0.1.9 的已知问题](CHANGELOG_zh.md#带入-019-的已知问题)。

## 文档已写、尚未交付

以下每行是一篇文档，以及它仍然保留的 `_(planned)_` 断言数量。数量按当前源码树统计，随着
逐条核实会发生变化；`architecture.md` 里还留着解释这个标记本身的那句话，它算在该篇的两处
之内。

- `docs/architecture.md` 保留 2 处 `_(planned)_` 断言，其中一处就是解释该标记本身的那句话——[architecture.md](docs/zh/architecture.md)。
- `docs/deployment.md` 保留 1 处 `_(planned)_` 断言——[deployment.md](docs/zh/deployment.md)。
- `docs/imap.md` 保留 19 处 `_(planned)_` 断言——[imap.md](docs/zh/imap.md)。
- `docs/security.md` 保留 25 处 `_(planned)_` 断言——[security.md](docs/zh/security.md)。
- `docs/smtp.md` 保留 24 处 `_(planned)_` 断言——[smtp.md](docs/zh/smtp.md)。
- `docs/storage.md` 保留 1 处 `_(planned)_` 断言——[storage.md](docs/zh/storage.md)。
- `docs/sync.md` 保留 2 处 `_(planned)_` 断言——[sync.md](docs/zh/sync.md)。

## 文档

- `docs/dockerhub.md` 是单份双语文件，而不是一对译本：它的中文半部分就在同一个文件里，因为它要粘贴到的页面没有语言切换，所以不存在 `docs/zh/dockerhub.md`——[dockerhub.md](docs/dockerhub.md)。
- `AGENTS.md` 没有中文版，仓库也没有任何规则要求它有——[AGENTS.md](AGENTS.md)。

## 测试

- bootstrap 服务器（连不上数据库时提供设置页的那个模式）没有验收测试：`server/tests/e2e.rs` 只覆盖正常运行中的服务器，所以"根路径挂载"以及 `serve` 对"`/` 由哪个应用回答"的选择（取决于是否已有管理员）目前只由 `crates/ferroma-api/src/router.rs` 里的单元测试守着，没有任何测试真的通过 socket 走一遍——[router.rs](crates/ferroma-api/src/router.rs)。

## 已决定不做

- 备份与恢复边车保持退休状态：两个 compose 文件都不再定义 `backup` 或 `restore` 服务，`scripts/backup.sh` 与 `scripts/restore.sh` 已删除，改为由运维把数据库与 `ferroma-data` 卷一起备份——[CHANGELOG_zh — 移除](CHANGELOG_zh.md#移除)。
- 滚动次版本标签（`0.1`、`0.2`……）与 registry 层缓存 `buildcache` 保持退休状态：一次发布只产出 `X.Y.Z` 与 `latest`，缓存只在本地 `.cache/buildx`——[CHANGELOG_zh — 移除](CHANGELOG_zh.md#移除)。
