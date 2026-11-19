# cvbbmso-network — 客户端启动网络预检

本 crate 提供 Windows/macOS 的系统代理检查及监考服务器连通性检查。系统相关 API 封装在 crate 内，其他模块使用统一 `NetworkReport`。本 crate 由 `cvbbmso-client` 在启动预检时调用，预检结果用于决定是否允许进入考试；错误展示由 `cvbbmso-client` 负责。

## 公开 Rust API

```rust
pub struct Inspector { /* 平台实现及服务器地址 */ }

pub enum ProxyState { Clear, Configured, Unknown }
pub enum EndpointState { Reachable, Unreachable }

pub struct NetworkReport {
    pub proxy: ProxyState,
    pub endpoint: EndpointState,
}

pub enum NetworkError {
    Platform,
    Io,
}

impl Inspector {
    /// 指定可信监考服务器 Origin，创建检查器。
    pub fn new(server_origin: url::Url) -> Self;

    /// 获取系统代理配置和监考服务器可达性快照。
    pub async fn check(&self) -> Result<NetworkReport, NetworkError>;
}
```

`Inspector::new` 接受监考服务器的 HTTP(S) Origin；检查器据此探测服务端可达性。`check` 返回 `Err(NetworkError)` 表示检查本身无法执行（平台 API 或 I/O 失败）；检查可以完成但代理配置无法确定时，返回 `ProxyState::Unknown`。

## 状态语义

| 字段 | 值 | 含义 |
| --- | --- | --- |
| `proxy` | `clear` | 系统代理、PAC 和代理环境变量均为关闭或空 |
| `proxy` | `configured` | 检测到系统代理、PAC 或代理环境变量 |
| `proxy` | `unknown` | 系统代理信息无法确定 |
| `endpoint` | `reachable` | 监考服务端探测成功 |
| `endpoint` | `unreachable` | 监考服务端探测失败 |

Windows 使用 WinINet、WinHTTP 与代理环境变量；macOS 使用 SystemConfiguration/CFNetwork 与代理环境变量。`Inspector` 读取当前用户环境。

## 行为约定

- **调用时机**：`cvbbmso-client` 在应用启动时调用一次；预检失败后应用保留，用户重试进入考试时再次调用。
- **允许进入考试的条件**：`proxy = clear` 且 `endpoint = reachable`。
- **阻止进入考试的条件**：`proxy = configured`、`proxy = unknown`、`endpoint = unreachable`，或 `check` 返回 `Err(NetworkError)`。客户端停留在考试外并展示对应错误，等待用户重试。
- 本 crate 读取当前用户环境并返回检查结果；网络异常的上报与展示由 `cvbbmso-client` 负责。检查与自动修改系统代理属于不同职责。
