<div align="center">

# Ferroma

[English](README.md) · **简体中文**

**一个用 Rust 写的自建邮件服务器。**

收信、发信，并在浏览器中阅读与管理。邮件保存在运行 Ferroma 的主机上。

[![Rust](https://img.shields.io/badge/Rust-stable-black?logo=rust)](https://www.rust-lang.org/)
[![PostgreSQL](https://img.shields.io/badge/PostgreSQL-18-4169E1?logo=postgresql&logoColor=white)](https://www.postgresql.org/)
[![Docker](https://img.shields.io/badge/Docker-Compose-2496ED?logo=docker&logoColor=white)](https://hub.docker.com/r/wesukilaye/ferroma)
[![License](https://img.shields.io/badge/License-AGPL--3.0-blue)](LICENSE)

文档：<https://ferroma.z1hwang.cn/>

</div>

## 安装

部署需要 PostgreSQL。`docker compose up -d` 只启动 Ferroma；引导页收集数据库信息。

```bash
git clone https://github.com/z1HwanG/Ferroma && cd Ferroma
cp .env.example .env          # 把 FERROMA_VERSION 设成要部署的版本
docker compose up -d
```

打开 `http://localhost:8080`。以 `FERROMA_VERSION=0.1.12` 锁定镜像；`latest` 会随新发布移动。`docker-compose.demo.yml` 是明文演示，不属于上述命令。

DNS、TLS、DKIM 和备份见[部署指南](docs/zh/deployment.md)。

## 已完成

| 组件 | 状态 |
|---|---|
| `ferroma-core` — 配置、错误、类型化 id、地址、限额、日志 | 完成 |
| `ferroma-mail` — RFC 5322 与 MIME 的解析和构建、邮件头、标志、信封 | 完成 |
| `ferroma-storage` — PostgreSQL 模式、仓库、Maildir、内容寻址的附件 | 完成 |
| `ferroma-events` — 带重放的类型化事件总线，供重连的客户端使用 | 完成 |
| `ferroma-auth` — Argon2id、HS256 访问令牌、轮换的刷新令牌、设备、限流 | 完成 |
| `ferroma-smtp` — 服务器、出站客户端、MX 解析、DKIM/SPF/DMARC、入站策略 | 完成 |
| `ferroma-imap` — IMAP4rev1 服务器，含 IDLE、APPEND、MOVE 与 EXPUNGE | 完成 |
| `ferroma-sync` — 变更日志、游标、幂等的客户端操作 | 完成 |
| `ferroma-api` — REST API、Ferroma 客户端协议、WebSocket、前端托管 | 完成 |
| `server` — `ferroma` 二进制及其运维命令 | 完成 |
| Webmail、管理控制台 | 完成 |

## 贡献

[`CONTRIBUTING.md`](CONTRIBUTING.md) 是约定：要过的检查、一项改动必须带上的测试，以及 DCO 签署。[`AGENTS.md`](AGENTS.md) 是更长的一份，包含一台开发机的特殊之处。

## 许可证

**AGPL-3.0-only。** 可以运行、修改、自行托管——对服务器真正要紧的是第 13 条：如果让别人通过网络使用一个*修改过的*版本，就必须向他们提供该版本的源码。本仓库就是上游源码，全文在 [`LICENSE`](LICENSE)。
