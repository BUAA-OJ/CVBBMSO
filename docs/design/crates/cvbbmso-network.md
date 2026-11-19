# cvbbmso-network — 网络环境检查

本 crate 提供 Windows/macOS 的系统代理检查及监考服务器连通性检查。系统相关 API 封装在 crate 内，其他模块使用统一 `NetworkReport`。错误展示和考试流程判断由 `cvbbmso-exam` 与 `cvbbmso-client` 根据检查结果负责。

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

## 映射到协议状态

`cvbbmso-exam` 将 `NetworkReport` 映射为 [协议](cvbbmso-protocol.md) 的 `NetworkState`，优先级如下：

| 条件 | `NetworkState` |
| --- | --- |
| `endpoint = unreachable` | `unreachable` |
| `proxy = configured` | `proxy_detected` |
| `proxy = unknown` | `unknown` |
| 其余（`proxy = clear` 且 `endpoint = reachable`） | `clear` |

`check` 返回 `Err(NetworkError)` 时，运行时按 `proxy_check_failed` 上报异常，并将状态视为 `unknown`。

## 行为约定

- **进入考试前**：客户端必须检查一次；进入考试流程的条件是 `NetworkState::Clear`。检测到代理、服务端不可达或状态未知时，客户端停留在考试外，展示对应错误。
- **考试中**：默认每 30 秒检查一次，可由客户端配置。发现代理或检查失败时，运行时通过 `ExamEvent::Incident` 上报 `proxy_detected` / `proxy_check_failed`，考试继续；网络恢复后发送新的 `status` 快照。
- 检查与自动修改系统代理属于不同职责；本 crate 读取当前用户环境并提供检查结果。
