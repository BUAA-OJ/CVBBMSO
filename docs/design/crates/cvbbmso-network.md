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

除上表外，守卫拒绝其他浏览器、代理、VPN、局域网和第三方 CDN 等名单外 IPv4/IPv6 出站流量。Windows 保留宿主入站判定。macOS 同时按源地址、协议和源端口阻止会产生名单外回包的 TCP/UDP 入站报文。macOS 的 IPv6 邻居发现和路由器请求例外保持原有范围。平台适配层按读取到的加密 DNS 配置放行 DoH/DoT 地址和端口。Accoding 页面只允许使用 `accoding.buaa.edu.cn` 自身 Origin 的资源。需要第三方 CDN 时必须显式扩展策略。

### 允许 IP 集

- 安装时解析两个 Origin 的全部 A/AAAA 记录，得到初始允许 IP 集。
- 每次校验会读取系统服务配置。配置变化时先更新服务规则，让新配置的解析器可用，再刷新 Origin。Origin 每 60 秒重新解析，替换下来的 IP 保留 120 秒。一次刷新用原子操作替换规则。
- 解析暂时失败时保留上一份允许 IP 集；两个 Origin 都没有可用地址时，`verify` 返回 `Resolution`。
- 允许集按解析后的 IP 匹配；TLS 仍由客户端正常校验。

### 生命周期与清理

客户端每 30 秒调用 `verify`。过滤规则或允许 IP 集失效时，客户端负责停止考试并展示错误。安装需要管理员或 root 权限。安装失败时客户端停留在考试外。

- **Windows** 使用动态 WFP 会话。IPv4 和 IPv6 的 ALE 连接层与出站传输层在同一事务安装和替换，校验检查两层所有过滤器的 ID。ALE 保留原有默认拒绝和回环规则。出站传输层只按远端 IP 和端口检查 TCP/UDP，覆盖被动打开的 TCP 握手回包、已存在的连接和 UDP 回包。名单内流量仍受宿主防火墙判定。IPv6 邻居发现继续由宿主处理，包括同链路全局地址和唯一本地地址的单播 NS/NA。出站传输层尚未覆盖 ICMP/ICMPv6 回包。正常退出关闭会话，进程崩溃后由 WFP 删除会话对象。[Microsoft WFP TCP 报文流转](https://learn.microsoft.com/en-us/windows/win32/fwp/tcp-packet-flows) · [RFC 4861](https://www.rfc-editor.org/rfc/rfc4861.html)
- **macOS** 从 `/Library/PrivilegedHelperTools/com.cvbbmso.egress-watchdog` 启动看门狗。安装包须由 root 将 helper 安装到此路径，并禁止其他用户修改 helper 或其父目录。安装先检查主过滤规则入口，再清理守卫遗留的规则、持久化所有者状态并登记看门狗和 launchd 恢复任务。随后加载专用 anchor、持有启用引用并校验内核规则。启动成功前按非回环本机地址撤销两个方向的旧 PF 状态。既有连接可能需要重连。
- **macOS 清理** 使用跨进程锁协调正常退出、看门狗和 launchd。看门狗发现客户端退出后撤销 anchor、释放本次安装持有的 PF 引用并清理状态。平台命令的子进程保留同一把锁并有 5 秒期限，超时会终止进程。即使父进程退出，子进程仍有自己的截止信号。若内核里的命令尚未退出，恢复方等待锁并在有界等待失败后重试。恢复任务每分钟检查，负责在看门狗失效或 15 分钟租约到期后重试清理。客户端每 5 分钟续期。

macOS 在 PF 已清理但 launchd 任务状态未确认时保留受保护的 `egress-cleanup.pending`，记录需要卸载的任务。仍在运行的看门狗、下一次计划任务或新版安装只对该标识重试。CLI 超时本身无法撤销 launchd 已收到的请求。

PF 启用前保存引用快照，子进程在执行 `pfctl -E` 前将自己的 PID 写入 `pf.enable.capture`，随后在同一文件捕获原始输出。核对完整令牌行与内核中的 PF 引用归属后，将文件原子改名为旧 helper 使用的 `pf.enable`。客户端在改名前退出时，旧 helper 可以清理 anchor、所有者记录和恢复任务，启用记录留给新版恢复或下一次新版安装。没有完整令牌且内核仍列出引用时，新版保留记录并报告清理失败。内核可能复用引用令牌。释放前记录 PID、进程名和创建时间，再次释放前核对这些记录。若数字已分给另一引用，保留新引用。归属无法确认时保留故障记录。PF 的释放接口按令牌操作，无法原子核对其他字段。 [Apple XNU 的 PF 引用实现](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/net/pf_ioctl.c)

新版 guard 在改动 PF 或登记任务前，以有界命令探测 helper 的 `--capabilities`。确认响应 `cvbbmso-egress-leases-v2` 后，每个租约分配新代次，使用唯一的 launchd 标识和一次写入的 plist 路径。持续运行和计划恢复都携带代次。任务在锁内核对当前所有者，过期任务只能清自己的任务文件和标识。旧接口 `--watch PID`、`--recover` 及旧租约状态仍可用。

| guard | helper | 恢复行为 |
| --- | --- | --- |
| 旧版 | 旧版 | 使用固定任务标识，两端平台命令可能无界等待 |
| 旧版 | 新版 | 使用固定任务标识，helper 有界，guard 仍可能无界等待 |
| 新版 | 旧版 | 使用固定任务标识，guard 有界，helper 仍可能无界等待 |
| 新版 | 新版 | 双方执行有界平台命令，按代次隔离迟到的 launchd 请求 |

滚动升级时先停止旧租约并确认旧恢复完成，将新版 helper 原子安装到原路径。新版 helper 可先和旧 guard 配合。随后升级 guard 并检查能力探测。回退时先退出新版租约，核对没有待清理的新版任务，再回退 guard，最后按需回退 helper。部署脚本应保留租约计数，检查新版待清理记录和任务。固定标识的兼容组合仍可能受迟到请求影响。

系统服务白名单按 IP、协议和端口匹配。平台未报告的加密 DNS 配置和部署证书链独有的 OCSP、CRL 或时间戳地址需要随部署验证。

### macOS 部署

部署方须在 PF 主过滤规则的第一条配置并加载这个入口。anchor 由守卫独占，初始为空，规则由守卫在安装时加载。

```pf
anchor "com.cvbbmso.egress"
```

运行期间，专用 anchor 只加载自己的地址表和终止式拒绝规则。TCP/UDP 出站报文按远端目的地址和端口匹配，入站按远端源地址和端口匹配。全机名单外的入站 IPv4/IPv6 TCP/UDP 会被丢弃，包括通常使用临时源端口的 SSH、远程桌面等远程接入。名单内报文继续由宿主 PF 判定，包括原有的入站和出站拒绝。IPv4 DHCP 仍使用已发现的单播服务器与 UDP 源端口 67。校验同时核对内核里的双向过滤规则和地址表。

名单收窄时，守卫撤销本机地址与被撤销对端地址之间两个方向的 PF 状态，让后续报文重新经过规则。地址宽限期内继续使用现有名单。PF 按地址而非按端口删除状态，同一对端地址的其他端口也可能需要重连。当前变更保持 IPv6 邻居发现、路径 MTU 和其他非 TCP/UDP 入站处理。宿主为普通 ICMP/ICMPv6 查询建立的状态仍可能让相关回包跳过出站规则，该类回包尚未覆盖。

内核中缺少入口、入口排位不符、入口带 `out` 或 `quick`、非回环接口跳过 PF，或检测到绕过过滤阶段的地址转换时，`install` 返回 `NetworkError::Platform`。更新时先退出守卫，保存宿主规则源，将旧的 `out` 或 `out quick` 项换成上述双向入口。旧的静态 `load anchor` 在确认属于本项目后移除。管理员先做语法检查，再按宿主流程加载与核对内核入口。守卫只管理专用 anchor 内容。回退时先完全退出新守卫，再从配置管理恢复旧入口及旧版本，并按宿主流程验证生效。

同步到 `feat/client` 时需要一并更新固定入口片段、部署脚本的内核入口校验、正式 DMG 的 `managed_pf` 候选过滤段及校验快照。DMG 应以同一版本提供 daemon 和签名恢复程序。`stage_recovery` 校验并原子更新恢复程序，随后在 `managed_pf::authorize` 前确认 V2 能力。能力缺失时结束受管启动。看门狗和 launchd 应使用已验签的恢复程序路径，并在卸载时一并保留 `managed_pf::restore` 的宿主配置恢复。正式签名 DMG 的既有授权流程继续适用。

macOS 方案面向系统协议栈产生的普通连接。PF 会丢弃未能重组的非回环出站分片，因为守卫无法读取这些分片的传输层端口。Darwin PF 的规则语法无法单独匹配部分保留值或协议与地址族不符的畸形报文。需要过滤管理员生成的原始报文或转发流量的部署场景，须由宿主策略处理。

### 隔离验收

在确认隔离的 Windows/macOS 上分别测 IPv4/IPv6。守卫启用前建立入站 TCP/UDP，启用后发回包，再从名单外尝试新建连接，并核对名单内双向通信、回环、DHCP 和 IPv6 链路控制。宿主已拒绝的名单内流量仍应被拒绝。刷新时验证宽限前后两个方向的现有连接、回包与规则替换。另测正常退出、崩溃、命令超时及迟到的 launchd 请求，核对只恢复项目自己的规则和引用。

### 失败处理

- `install` 失败：客户端停留在考试外，展示错误并允许用户重试。
- `verify` 失败：停止考试，保留已录制分片，提示用户联系监考。
- 清理失败：记录本地错误并提示用户；下次启动继续清理。
- 出站守卫是本地执行约束；服务端仍是考试时间和业务的权威来源。
