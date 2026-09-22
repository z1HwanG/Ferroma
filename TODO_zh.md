# TODO — 尚未完成的事

英文原版见 [`TODO.md`](TODO.md)，本文件是它的中文译本。

本队列只收录仓库自己已经承认尚未完成的事。素材来自 [`CHANGELOG_zh.md`](CHANGELOG_zh.md)
里仍未了结的条目，以及文档体系自身的缺口。这里没有新想法；已经发布的内容见
[`CHANGELOG_zh.md`](CHANGELOG_zh.md)，已退休内容的取舍理由见其 `## [未发布]` 一节。

## 文档

- `docs/dockerhub.md` 是单份双语文件，而不是一对译本：它的中文半部分就在同一个文件里，因为它要粘贴到的页面没有语言切换，所以不存在 `docs/zh/dockerhub.md`——[dockerhub.md](docs/dockerhub.md)。
- `AGENTS.md` 没有中文版，仓库也没有任何规则要求它有——[AGENTS.md](AGENTS.md)。

## 刻意不写的测试

- 桌面客户端已于 0.1.8 移出本仓库，因此 FCP 验收路径改为直接驱动 API
  （`the_sync_cursor_sees_the_delivery`）。一个端到端说 FCP 的客户端，其验收测试
  属于客户端自己的仓库——两侧共同实现的是 [`docs/zh/fcp.md`](docs/zh/fcp.md) 冻结的契约。

## 已决定不做

- 备份与恢复边车保持退休状态：两个 compose 文件都不再定义 `backup` 或 `restore` 服务，`scripts/backup.sh` 与 `scripts/restore.sh` 已删除，改为由运维把数据库与 `ferroma-data` 卷一起备份——[CHANGELOG_zh — 移除](CHANGELOG_zh.md#移除)。
- 滚动次版本标签（`0.1`、`0.2`……）与 registry 层缓存 `buildcache` 保持退休状态：一次发布只产出 `X.Y.Z` 与 `latest`，缓存只在本地 `.cache/buildx`——[CHANGELOG_zh — 移除](CHANGELOG_zh.md#移除)。
