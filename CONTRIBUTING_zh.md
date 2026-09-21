# 参与 Ferroma 的开发

Ferroma 存的是别人的邮件。看起来很小的一处改动，可能弄丢某个人的信，所以这里的标准不是"能编译"，
而是"出坏的那一天会怎样"。下面每一条都是从这一点推出来的。

## 动手之前

* **[`AGENTS.md`](AGENTS.md) 是工作约定。** 里面有仓库结构、这台机器的两个特殊之处，以及不可
  让步的规范：不可信输入上不用 `unwrap()`、不用 `sqlx::query!` 宏、全栈 rustls、每个公开项都要有
  文档。
* **行为改动必须带一个"没有它就会红"的测试。** "能编译"和"我试过一次"都不算完成；一个两种情况下都
  通过的测试，只是多花了几步写出来的注释。
* **文档是双语的。** 改动 `docs/` 下的文档，就等于改动它的配对篇目，而 `node tools/check-zh.mjs`
  就是那个负责说话的东西——它比对标题、代码块，以及每处交叉引用指向的语言。

## 必须通过的检查

```bash
cargo test --workspace --offline      # 单元、集成，以及端到端验收
node tools/check-docs.mjs             # 每个链接与锚点，以及中英配对
node tools/check-zh.mjs               # 中文这一套：术语、配对、引用
node tools/check-deploy.mjs           # 部署产物，含 Dockerfile
(cd web && node tools/check.mjs)      # 模块图、元素 id、词条覆盖
(cd admin && node tools/check.mjs)
```

前端检查是刻意严格的。它会在"视图里不存在的元素 id"、"中文词条表没覆盖的 `t('…')` 字符串"、
以及"`shared/api.js` 之外的 `fetch()`"上失败——这三种都曾经以 bug 的形式发布出去过。

## 提交与合并请求

* 一个改动一个提交，并且说明**为什么**。diff 已经说清改了什么，说不清的是：当时的另一种做法是什么、
  为什么没有选它。
* 标题行用祈使句，约 72 个字符以内。
* 有对应 issue 时引用它。

## 签署（DCO）

参与贡献即表示你同意[开发者原创证明](https://developercertificate.org/)（DCO），并通过签署提交来记录它：

```bash
git commit -s -m "imap: …"
```

这会在提交信息里加一行：

```text
Signed-off-by: 你的名字 <you@example.com>
```

这就是全部主张：代码是你写的，或者你有权把它交出去。本项目没有 CLA。贡献按项目自身的许可
[AGPL-3.0-only](LICENSE) 进入，也就是所有人拿到它时所用的同一份条款。

## 安全

如果一个漏洞是读者看到就能利用的，不要开 issue。威胁模型见 [`docs/zh/security.md`](docs/zh/security.md)；
请私下发给仓库资料里的地址，并期待修复之前先收到确认。
