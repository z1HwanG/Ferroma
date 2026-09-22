# TODO — 尚未完成的事项

英文原文见 [`TODO.md`](TODO.md)，本文件为其译文。

本清单只收录仓库已标明尚未完成的事项。来源是 [`CHANGELOG_zh.md`](CHANGELOG_zh.md)
中仍未关闭的条目，以及文档集自身的缺口。此处不记录新提议。已发布的内容见
[`CHANGELOG_zh.md`](CHANGELOG_zh.md)，已停用部分的理由见其 `## [未发布]` 一节。

## 文档

- `docs/dockerhub.md` 是一份双语文件，而不是一对译文：中文部分位于同一文件，因为目标页面没有语言切换，因此不存在 `docs/zh/dockerhub.md`。[dockerhub.md](docs/dockerhub.md)。
- `AGENTS.md` 没有中文版，仓库也没有规则要求提供。[AGENTS.md](AGENTS.md)。

## 刻意不写的测试

- 桌面客户端已于 0.1.8 移出本仓库，因此 FCP 验收路径改为直接调用 API
  （`the_sync_cursor_sees_the_delivery`）。端到端使用 FCP 的客户端，其验收测试
  属于该客户端的仓库。双方实现的契约是 [`docs/zh/fcp.md`](docs/zh/fcp.md)。

## 已决定不做

- 备份与恢复边车保持停用：两份 compose 文件均不再定义 `backup` 或 `restore` 服务，`scripts/backup.sh` 与 `scripts/restore.sh` 已删除。数据库与 `ferroma-data` 卷由运维者一并备份。[CHANGELOG_zh — 移除](CHANGELOG_zh.md#移除)。
- 滚动次版本标签（`0.1`、`0.2`……）与 registry 层缓存 `buildcache` 保持停用：一次发布只产出 `X.Y.Z` 与 `latest`，缓存仅位于本地 `.cache/buildx`。[CHANGELOG_zh — 移除](CHANGELOG_zh.md#移除)。
