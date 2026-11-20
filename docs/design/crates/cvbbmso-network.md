# cvbbmso-network — 客户端网络预检与系统出站守卫

本 crate 提供 Windows/macOS 的启动网络预检，以及应用生命周期内的系统级出站网络守卫。预检检查系统代理和监考服务端可达性；出站守卫把系统网络限制为 Accoding、监考服务器和少量必要系统服务。平台相关 API 封装在 crate 内，其他模块使用统一 `NetworkReport` / `EgressGuard`。本 crate 由 `cvbbmso-client` 在应用启动时安装，并在运行期校验；错误展示和是否进入考试由 `cvbbmso-client` 负责。

## 公开 Rust API

```rust
pub struct Inspector { /* 平台实现及服务器地址 */ }

pub enum ProxyState { Clear, Configured, Unknown }
pub enum EndpointState { Reachable, Unreachable }

pub struct NetworkReport {
    pub proxy: ProxyState,
    pub endpoint: EndpointState,
}

pub struct EgressPolicy {
    /// 受控浏览器固定访问的 Accoding Origin。
    pub accoding_origin: url::Url,
    /// 监考服务器 Origin，HTTP API 与 WSS 使用同一 Origin。
    pub server_origin: url::Url,
}

pub struct EgressGuard { /* 平台过滤规则、允许 IP 集和清理任务 */ }

pub enum NetworkError {
    Platform,
    Io,
    PermissionDenied,
    Policy,
    Resolution,
}

impl Inspector {
    /// 指定可信监考服务器 Origin，创建检查器。
    pub fn new(server_origin: url::Url) -> Self;

    /// 获取系统代理配置和监考服务器可达性快照。
    pub async fn check(&self) -> Result<NetworkReport, NetworkError>;
}

impl EgressGuard {
    /// 安装系统级出站过滤；需要管理员/root 权限。
    /// 安装前清理上一次运行遗留的规则；任一步失败时回滚已应用的规则。
    pub async fn install(policy: EgressPolicy) -> Result<Self, NetworkError>;

    /// 校验当前过滤规则和允许 IP 集仍然生效。
    pub async fn verify(&self) -> Result<(), NetworkError>;

    /// 撤销过滤规则，并清理平台使用的看门狗和 TTL 任务。
    pub async fn shutdown(self) -> Result<(), NetworkError>;
}
```

`Inspector::new` 接受监考服务器的 HTTP(S) Origin；检查器据此探测服务端可达性。`check` 返回 `Err(NetworkError)` 表示检查本身无法执行（平台 API 或 I/O 失败）；检查可以完成但代理配置无法确定时，返回 `ProxyState::Unknown`。

`EgressGuard::install` 要求 `accoding_origin` 和 `server_origin` 是 `https` Origin，主机非空；否则返回 `Policy`。无法解析任一允许域名时返回 `Resolution`。缺少管理员/root 权限时返回 `PermissionDenied`。平台过滤 API 或清理任务失败时返回 `Platform` / `Io`。

## 状态语义

| 字段 | 值 | 含义 |
| --- | --- | --- |
| `proxy` | `clear` | 系统代理、PAC 和代理环境变量均为关闭或空 |
| `proxy` | `configured` | 检测到系统代理、PAC 或代理环境变量 |
| `proxy` | `unknown` | 系统代理信息无法确定 |
| `endpoint` | `reachable` | 监考服务端探测成功 |
| `endpoint` | `unreachable` | 监考服务端探测失败 |

Windows 使用 WinINet、WinHTTP 与代理环境变量；macOS 使用 SystemConfiguration/CFNetwork 与代理环境变量。`Inspector` 读取当前用户环境。

## 启动预检

- **调用时机**：`cvbbmso-client` 在应用启动时调用一次；预检失败后应用保留，用户重试进入考试时再次调用。
- **允许进入考试的条件**：`proxy = clear` 且 `endpoint = reachable`。
- **阻止进入考试的条件**：`proxy = configured`、`proxy = unknown`、`endpoint = unreachable`，或 `check` 返回 `Err(NetworkError)`。客户端停留在考试外并展示对应错误，等待用户重试。
- 本 crate 读取当前用户环境并返回检查结果；网络异常的上报与展示由 `cvbbmso-client` 负责。检查与自动修改系统代理属于不同职责。

## 系统出站守卫

`EgressGuard` 在 Accoding 登录前安装，保持到应用退出；安装期间要求客户端以管理员/root 运行。Windows 使用 WFP，macOS 使用 PF；规则按 IP/端口匹配，域名在安装和刷新时解析为 IP 集。

### 允许的出站流量

| 目的 | 协议 / 端口 | 说明 |
| --- | --- | --- |
| `accoding_origin` 解析出的 IPv4/IPv6 地址 | Origin 端口（默认 443），TCP 与 UDP（QUIC） | 受控 WebView 登录、题目和提交 |
| `server_origin` 解析出的 IPv4/IPv6 地址 | Origin 端口（默认 443），TCP 与 UDP（QUIC） | HTTP API、WSS、重连 |
| 回环地址 `127.0.0.1` / `::1` | 全部端口 | Tauri/WebView 本地 IPC |
| 系统配置的 DNS 解析器 | UDP/TCP 53 | 解析允许域名和系统服务 |
| 系统配置的 DHCP / NTP 服务 | UDP 67、123 | 单播 DHCPv4 续租和时间同步。DHCP 回复从入站 UDP 68 接收 |
| 平台适配层维护的证书服务 | TCP 80 或 443，端口随服务配置 | macOS 使用 Apple 公布的证书验证目的地，Windows 使用系统信任列表服务 |

除上表外，系统所有进程的出站 IPv4/IPv6 连接均被拒绝，包括其他浏览器、代理、VPN、局域网和任意第三方 CDN。入站连接不在守卫范围内。macOS 还放行 IPv6 邻居发现和路由器请求所需的链路控制报文。平台适配层按读取到的加密 DNS 配置放行 DoH/DoT 地址和端口。Accoding 页面只允许使用 `accoding.buaa.edu.cn` 自身 Origin 的资源；需要第三方 CDN 时必须显式扩展策略。

### 允许 IP 集

- 安装时解析两个 Origin 的全部 A/AAAA 记录，得到初始允许 IP 集。
- 每次校验会读取系统服务配置。配置变化时先更新服务规则，让新配置的解析器可用，再刷新 Origin。Origin 每 60 秒重新解析，替换下来的 IP 保留 120 秒。一次刷新用原子操作替换规则。
- 解析暂时失败时保留上一份允许 IP 集；两个 Origin 都没有可用地址时，`verify` 返回 `Resolution`。
- 允许集按解析后的 IP 匹配；TLS 仍由客户端正常校验。

### 生命周期与清理

客户端每 30 秒调用 `verify`。过滤规则或允许 IP 集失效时，客户端负责停止考试并展示错误。安装需要管理员或 root 权限。安装失败时客户端停留在考试外。

- **Windows** 使用动态 WFP 会话。初次安装和规则替换都通过事务提交，校验按过滤器 ID 查询 WFP。正常退出关闭会话，进程崩溃后由 WFP 删除会话对象。
- **macOS** 从 `/Library/PrivilegedHelperTools/com.cvbbmso.egress-watchdog` 启动看门狗。安装包须由 root 将 helper 安装到此路径，并禁止其他用户修改 helper 或其父目录。安装先清理遗留规则、持久化所有者状态并登记看门狗和 launchd 恢复任务，然后加载 PF anchor、持有启用引用并校验内核规则。临时主规则使用 `/etc/pf.conf` 作为基础，安装时会重新加载主过滤规则。安装后始发于本机的旧 PF 状态会被撤销，以便流量重新匹配规则。
- **macOS 清理** 使用跨进程锁协调正常退出、看门狗和 launchd。看门狗发现客户端退出后撤销 anchor、释放本次安装持有的 PF 引用并清理状态。恢复任务每分钟检查，负责在看门狗失效或 15 分钟租约到期后重试清理。客户端每 5 分钟续期。

系统服务白名单按 IP、协议和端口匹配。平台未报告的加密 DNS 配置和部署证书链独有的 OCSP、CRL 或时间戳地址需要随部署验证。动态加载到 macOS PF 主规则集、且只存在于内核的第三方过滤规则也需要在部署环境验证兼容性。

### 失败处理

- `install` 失败：客户端停留在考试外，展示错误并允许用户重试。
- `verify` 失败：停止考试，保留已录制分片，提示用户联系监考。
- 清理失败：记录本地错误并提示用户；下次启动继续清理。
- 出站守卫是本地执行约束；服务端仍是考试时间和业务的权威来源。
