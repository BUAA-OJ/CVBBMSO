# CVBBMSO: Centralized Verification and Browser-Based Monitoring System for Online-exams

CVBBMSO 是配合 [Accoding](https://accoding.buaa.edu.cn) 使用的程序设计考试系统，包含 Windows / macOS 桌面考试客户端与监考服务端。

- **考试客户端**：启动时完成虚拟机与网络预检，安装系统级出站网络守卫以限制考试设备访问范围；在内置受控浏览器中打开 Accoding，按服务端指令录制主显示器画面（无音频）。录像分片保存并在考后合并于考生本地设备，服务端仅保留录像摘要。
- **监考服务端**：单实例运行，通过 Accoding 会话 Cookie 复验考生身份与报名资格，经 WebSocket 按考试时间统一下发开始/结束指令，持久化考试事件、失联/恢复/重启异常和录像摘要，并提供可选的 Web 管理控制面板。

## 客户端安装与构建

客户端与服务端需保持同版本部署，构建安装包时通过发布配置注入监考服务的 HTTPS Origin。

- **Windows x64（MSI）**：需以管理员权限安装与运行，详见 [Windows 客户端部署](docs/deployment/windows-client.md)。
- **macOS 14 及以上（DMG）**：需将已签名的 `CVBBMSO.app` 拖入 `/Applications` 并批准后台特权服务，详见 [macOS 客户端部署](docs/deployment/macos-client.md)。

## 服务端运行与部署

在仓库根目录本地启动服务端，考试数据写入 SQLite 数据库（如 `exams.sqlite`）：

```sh
cargo run --locked -p cvbbmso-server -- \
  --listen-addr 127.0.0.1:8080 \
  --database-url 'sqlite://exams.sqlite?mode=rwc'
```

- 启动参数也支持通过 `CVBBMSO_LISTEN_ADDR`、`CVBBMSO_DATABASE_URL` 等环境变量传入；服务存活检查路径为 `GET /api/health`。
- 设置 `CVBBMSO_ADMIN_TOKEN`（须为 32 字节随机值的 64 位小写十六进制字符串，可用 `openssl rand -hex 32` 生成）后，服务端会挂载 `/admin` 管理控制面板，用于创建考试、导入或考前撤销报名名单，以及查看考生状态、事件、异常与录像摘要；未设置时不挂载管理端点。
- 生产环境应按单实例部署，由同机反向代理在 `443` 端口终止 TLS，将 HTTPS 与 WebSocket 请求转发至内网监听地址（如 `127.0.0.1:8080`）。`/admin` 与 `/api/admin/` 须限制在管理网、VPN 或受信任代理之后，详见 [服务端文档与部署说明](docs/design/crates/cvbbmso-server.md#管理控制面板)。

## 仓库结构

| Crate | 职责 | 设计文档 |
| --- | --- | --- |
| `cvbbmso-client` | Tauri 2 桌面客户端、启动预检编排与受控 Accoding WebView | [client](docs/design/crates/cvbbmso-client.md) |
| `cvbbmso-exam` | 客户端考试运行时、WebSocket 重连、可靠事件队列与状态同步 | [exam](docs/design/crates/cvbbmso-exam.md) |
| `cvbbmso-network` | 系统代理/连通性预检与系统级出站网络守卫（Windows WFP / macOS PF） | [network](docs/design/crates/cvbbmso-network.md) |
| `cvbbmso-recorder` | 主显示器分段录屏、崩溃恢复、合并与 SHA-256 摘要 | [recorder](docs/design/crates/cvbbmso-recorder.md) |
| `cvbbmso-accoding` | Accoding Cookie 归一化与 `/api/users/me` 身份复验 | [accoding](docs/design/crates/cvbbmso-accoding.md) |
| `cvbbmso-server` | Axum HTTP / WebSocket 监考服务、并发组提交与可选管理面板 | [server](docs/design/crates/cvbbmso-server.md) |
| `cvbbmso-domain` | 考试规则、调度、SQLite 持久化（含 SeaORM 迁移）与异常记录 | [domain](docs/design/crates/cvbbmso-domain.md) |
| `cvbbmso-protocol` | 客户端与服务端共用的 HTTP / WebSocket 数据结构、枚举与错误码 | [protocol](docs/design/crates/cvbbmso-protocol.md) |

## 开发与验证

Rust 工具链版本由 [rust-toolchain.toml](rust-toolchain.toml) 固定（Rust 1.99.1，Edition 2024），工程规范见 [AGENTS.md](AGENTS.md)。

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
```

更多架构与协议细节请参阅 [系统总览](docs/design/overview.md) 与 [通信协议](docs/design/crates/cvbbmso-protocol.md)。
