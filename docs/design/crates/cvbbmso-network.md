# cvbbmso-network — 客户端网络预检与系统出站守卫

本 crate 提供 Windows/macOS 的启动网络预检和系统级出站守卫。预检检查系统代理和监考服务器可达性。守卫只放行 Accoding、监考服务器和必要的系统服务。客户端使用统一的 `NetworkReport` 和 `EgressGuard`，负责运行期校验、错误展示和考试准入。

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

    /// 只读取系统代理配置，不探测服务端。
    pub async fn check_proxy() -> Result<ProxyState, NetworkError>;

    /// 检查默认路由或常见拆分默认路由是否关联 TUN 或 VPN。
    #[cfg(any(target_os = "macos", windows))]
    pub async fn tunnel_configured() -> Result<bool, NetworkError>;
}

impl EgressGuard {
    /// Windows 安装系统级出站过滤，macOS 附着管理员启动的守卫。
    pub async fn install(policy: EgressPolicy) -> Result<Self, NetworkError>;

    /// 校验当前过滤规则和允许 IP 集仍然生效。
    pub async fn verify(&self) -> Result<(), NetworkError>;

    /// 撤销过滤规则，并清理平台使用的看门狗和 TTL 任务。
    pub async fn shutdown(self) -> Result<(), NetworkError>;
}
```

`Inspector::new` 接受监考服务器的 HTTP(S) Origin；检查器据此探测服务端可达性。`check` 返回 `Err(NetworkError)` 表示检查本身无法执行（平台 API 或 I/O 失败）；检查可以完成但代理配置无法确定时，返回 `ProxyState::Unknown`。

`EgressGuard::install` 要求两个 Origin 使用 HTTPS 且主机非空，否则返回 `Policy`。本地安装无法解析任一允许域名时返回 `Resolution`，缺少管理员权限时返回 `PermissionDenied`。macOS 无法连接或验证管理员会话时返回 `MacosServiceUnavailable`。平台过滤 API 或清理任务失败时可返回 `Platform` 或 `Io`。

## 状态语义

| 字段 | 值 | 含义 |
| --- | --- | --- |
| `proxy` | `clear` | 系统代理、PAC 和代理环境变量均为关闭或空 |
| `proxy` | `configured` | 检测到系统代理、PAC 或代理环境变量 |
| `proxy` | `unknown` | 系统代理信息无法确定 |
| `endpoint` | `reachable` | 监考服务端探测成功 |
| `endpoint` | `unreachable` | 监考服务端探测失败 |

Windows 使用 WinINet、WinHTTP 与代理环境变量；macOS 使用 SystemConfiguration/CFNetwork 与代理环境变量。`Inspector` 读取当前用户环境。

客户端运行时使用 `check_proxy` 复查代理。macOS 通过 `netstat -rn` 只读检查非接口限定的默认路由及常见拆分默认路由，读取超时 2 秒后终止子进程并返回 I/O 错误。Windows 通过 IP Helper 读取同类路由及对应适配器的运行状态，识别系统隧道、Wintun 和常见 VPN 适配器。仅有空闲隧道接口或私网路由时不会触发阻断。

## 启动预检

- **调用时机**：`cvbbmso-client` 在应用启动时调用一次；预检失败后应用保留，用户重试进入考试时再次调用。
- **允许进入考试的条件**：`proxy = clear` 且 `endpoint = reachable`。
- **阻止进入考试的条件**：`proxy = configured`、`proxy = unknown`、`endpoint = unreachable`，或 `check` 返回 `Err(NetworkError)`。客户端停留在考试外并展示对应错误，等待用户重试。
- 本 crate 读取当前用户环境并返回检查结果；网络异常的上报与展示由 `cvbbmso-client` 负责。检查与自动修改系统代理属于不同职责。

## 系统出站守卫

`EgressGuard` 在 Accoding 登录前建立，保持到应用退出。Windows 客户端以管理员身份安装 WFP。macOS 客户端以普通用户身份连接持有 PF 的 root helper。规则按 IP 和端口匹配，域名在安装和刷新时解析为 IP 集。

### 允许的出站流量

校园网重新认证额外放行固定主机 `gw.buaa.edu.cn` 解析出的单播 IPv4/IPv6 地址，仅允许 TCP 443。地址变化沿用 120 秒宽限，暂时解析失败保留上次地址；该范围在初始安装和后续网络配置刷新中均保留。不放行其他校园域名或整个网段。

Windows 后台发现加密 DNS 和时间同步配置时，PowerShell 与 `w32tm.exe` 使用 `CREATE_NO_WINDOW`，复查不会创建控制台窗口。

| 目的 | 协议 / 端口 | 说明 |
| --- | --- | --- |
| `accoding_origin` 解析出的 IPv4/IPv6 地址 | Origin 端口（默认 443），TCP 与 UDP（QUIC） | 受控 WebView 登录、题目和提交 |
| `server_origin` 解析出的 IPv4/IPv6 地址 | Origin 端口（默认 443），TCP 与 UDP（QUIC） | HTTP API、WSS、重连 |
| 回环地址 `127.0.0.1` / `::1` | 全部端口 | Tauri/WebView 本地 IPC |
| 系统配置的 DNS 解析器 | UDP/TCP 53 | 解析允许域名和系统服务 |
| 系统配置的 DHCP / NTP 服务 | UDP 67、123 | 单播 DHCPv4 续租和时间同步。DHCP 回复从入站 UDP 68 接收 |
| 平台适配层维护的证书服务 | TCP 80 或 443，端口随服务配置 | macOS 使用 Apple 公布的证书验证目的地，Windows 使用系统信任列表服务 |

除上表外，守卫拒绝其他浏览器、代理、VPN、局域网和第三方 CDN 等名单外 IPv4/IPv6 出站流量。Windows 保留宿主入站判定。macOS 同时按源地址、协议和源端口阻止会产生名单外回包的 TCP/UDP 入站报文。macOS 的 IPv6 邻居发现和路由器请求例外保持原有范围。平台适配层按读取到的加密 DNS 配置放行 DoH/DoT 地址和端口。Accoding 页面只允许使用 `accoding.buaa.edu.cn` 自身 Origin 的资源。需要第三方 CDN 时必须显式扩展策略。

### 允许 IP 集

- 安装时解析两个 Origin 的全部 A/AAAA 记录，得到初始允许 IP 集。
- 每次校验会读取系统服务配置。配置变化时先更新服务规则，让新配置的解析器可用，再刷新 Origin。Origin 每 60 秒重新解析，替换下来的 IP 保留 120 秒。一次刷新用原子操作替换规则。
- 解析暂时失败时保留上一份允许 IP 集；两个 Origin 都没有可用地址时，`verify` 返回 `Resolution`。
- 允许集按解析后的 IP 匹配；TLS 仍由客户端正常校验。

### 生命周期与清理

客户端每 30 秒调用 `verify`。过滤规则或允许 IP 集失效时，客户端负责停止考试并展示错误。修改系统过滤器需要管理员权限。Windows 由提权后的客户端安装，macOS 由 root helper 安装。守卫未通过校验时客户端停留在考试外。

- **Windows** 使用动态 WFP 会话。IPv4 和 IPv6 的 ALE 连接层与出站传输层在同一事务安装和替换，校验检查两层所有过滤器的 ID。ALE 保留原有默认拒绝和回环规则。出站传输层只按远端 IP 和端口检查 TCP/UDP，覆盖被动打开的 TCP 握手回包、已存在的连接和 UDP 回包。名单内流量仍受宿主防火墙判定。IPv6 邻居发现继续由宿主处理，包括同链路全局地址和唯一本地地址的单播 NS/NA。出站传输层尚未覆盖 ICMP/ICMPv6 回包。正常退出关闭会话，进程崩溃后由 WFP 删除会话对象。[Microsoft WFP TCP 报文流转](https://learn.microsoft.com/en-us/windows/win32/fwp/tcp-packet-flows) · [RFC 4861](https://www.rfc-editor.org/rfc/rfc4861.html)
- **macOS** 包内的 root helper 先清理遗留状态并登记看门狗和 launchd 恢复任务，再激活主过滤入口、加载专用 anchor、获取 PF 启用引用并核对内核规则。启动成功前按非回环本机地址撤销两个方向的旧 PF 状态。既有连接可能需要重连。
- **macOS 清理** 使用跨进程锁协调正常退出、看门狗和 launchd。helper 在 App 退出时撤销 anchor、释放本场 PF 引用并清理状态。平台命令的子进程保留同一把锁并有 5 秒期限，超时会终止进程。即使父进程退出，子进程仍有自己的截止信号。若内核里的命令尚未退出，恢复方等待锁并在有界等待失败后重试。看门狗监视 helper 的退出和租约。launchd 任务每分钟检查，在守卫进程退出或 15 分钟租约到期后重试清理。helper 至多每 5 分钟续期一次。

macOS 在 PF 已清理但 launchd 任务状态未确认时保留受保护的 `egress-cleanup.pending`，记录需要卸载的任务。仍在运行的看门狗、下一次计划任务或新版安装只对该标识重试。CLI 超时本身无法撤销 launchd 已收到的请求。

PF 启用前保存引用快照。子进程必须先将 PID 和行末换行符完整写入 `pf.enable.capture`，才能执行 `pfctl -E`。如果捕获文件为空，或只写入了 PID 行的一部分，恢复程序可在没有其他引用归属记录时清除本次启用记录，宿主 PF 引用保持不动。PID 行已写完但令牌缺失或截断时，若内核仍列出引用，新版保留记录并报告清理失败。核对完整令牌行与内核中的 PF 引用归属后，将文件原子改名为旧 helper 使用的 `pf.enable`。采样无法确认引用创建时间时，启动失败，完整捕获留待后续精确采样。客户端在改名前退出时，旧 helper 可以清理 anchor、所有者记录和恢复任务，启用记录留给新版恢复或下一次新版安装。

新版开始清理时原子移走 `pf.enable`，避免旧 helper 接手续跑新版已发出的释放操作。释放前记录 PID、进程名和创建时间，再次释放前核对这些记录。内核把同一数值令牌分给另一引用时，保留新引用。归属无法确认时保留故障记录。PF 的释放接口按令牌操作，无法原子核对其他字段。 [Apple XNU 的 PF 引用实现](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/net/pf_ioctl.c)

helper 在改动 PF 或登记任务前，以有界命令确认受保护的 helper 路径支持 `cvbbmso-egress-leases-v2`。新租约使用独立代次、唯一的 launchd 标识和一次写入的 plist。任务在锁内核对当前所有者，过期任务只能清自己的任务文件和标识。恢复仍读取旧租约、引用日志和待清理记录，并接受已有任务使用的 `--watch PID` 和 `--recover`。升级应先结束活动会话并完成恢复，保留旧恢复程序和记录，直到遗留清理完成。

系统服务白名单按 IP、协议和端口匹配。平台未报告的加密 DNS 配置和部署证书链独有的 OCSP、CRL 或时间戳地址需要随部署验证。

### macOS 部署

macOS 14 及以上支持 arm64 和 x86_64。DMG 内的引导 App 请求系统管理员授权，安装至 `/Library/Application Support/com.cvbbmso-client/CVBBMSO.app`，再调用包内的 `Contents/Helpers/cvbbmso-pf launch`。helper 以 root 管理 PF，并降权到当前控制台用户启动受保护的 App。授权调用在确认内核 PF 已安装且指定进程已执行该 App 后返回。该返回只用于引导窗交接，录屏权限仍由普通用户 App 处理。 [macOS DMG 部署](../../deployment/macos-client.md)

macOS 客户端使用 `EgressGuard::install(policy).await` 附着会话。Unix socket 核验 helper 的 root 对端身份，以及 App 的当前控制台 UID、启动进程和可执行文件。构建时固定两个 HTTPS Origin。实际 PF 校验成功后客户端才能拿到守卫。安装后最多等待 45 秒完成启动和附着。已附着时 helper 定期校验并续期，断连后清理。进程异常退出时，由看门狗和临时 launchd 任务恢复。`status` 检查受保护安装和构建配置，`recover` 处理无活动会话时的遗留状态。

PF 主过滤规则的第一条使用下列入口。anchor 由守卫独占，启动前为空。

```pf
anchor "com.cvbbmso.egress"
```

运行期间，专用 anchor 只加载自己的地址表和终止式拒绝规则。TCP/UDP 出站报文按远端目的地址和端口匹配，入站按远端源地址和端口匹配。全机名单外的入站 IPv4/IPv6 TCP/UDP 会被丢弃，包括通常使用临时源端口的 SSH、远程桌面等远程接入。名单内报文继续由宿主 PF 判定，包括原有的入站和出站拒绝。IPv4 DHCP 仍使用已发现的单播服务器与 UDP 源端口 67。校验同时核对内核里的双向过滤规则和地址表。

名单收窄时，守卫撤销本机地址与被撤销对端地址之间两个方向的 PF 状态，让后续报文重新经过规则。地址宽限期内继续使用现有名单。PF 按地址而非按端口删除状态，同一对端地址的其他端口也可能需要重连。当前变更保持 IPv6 邻居发现、路径 MTU 和其他非 TCP/UDP 入站处理。宿主为普通 ICMP/ICMPv6 查询建立的状态仍可能让相关回包跳过出站规则，该类回包尚未覆盖。

守卫可用内核快照临时给受支持的宿主规则添加上述入口，也可使用管理员已设置的入口。入口位置错误、带 `out` 或 `quick`、非回环接口跳过 PF，或地址转换会绕过过滤阶段时，启动失败。看门狗和 launchd 使用同一受保护 helper 和租约代次。清理释放本场 PF 引用后恢复原过滤段。恢复失败时保留原租约和记录以供重试。旧的 `out` 入口只有在存在旧还原记录且内核严格匹配旧版空闲快照时才会自动撤除。

macOS 方案面向系统协议栈产生的普通连接。PF 会丢弃未能重组的非回环出站分片，因为守卫无法读取这些分片的传输层端口。Darwin PF 的规则语法无法单独匹配部分保留值或协议与地址族不符的畸形报文。需要过滤管理员生成的原始报文或转发流量的部署场景，须由宿主策略处理。

### 隔离验收

在确认隔离的 Windows/macOS 上分别测 IPv4/IPv6。守卫启用前建立入站 TCP/UDP，启用后发回包，再从名单外尝试新建连接，并核对名单内双向通信、回环、DHCP 和 IPv6 链路控制。宿主已拒绝的名单内流量仍应被拒绝。刷新时验证宽限前后两个方向的现有连接、回包与规则替换。另测正常退出、崩溃、命令超时及迟到的 launchd 请求，核对只恢复项目自己的规则和引用。

### 失败处理

- `install` 失败：客户端停留在考试外，展示错误并允许用户重试。
- `verify` 失败：停止考试，保留已录制分片，提示用户联系监考。
- 清理失败：记录本地错误并提示用户；下次启动继续清理。
- 出站守卫是本地执行约束；服务端仍是考试时间和业务的权威来源。
