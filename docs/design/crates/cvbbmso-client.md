# cvbbmso-client — Tauri 桌面客户端

本 crate 是 Windows/macOS 可执行程序。使用 Tauri 2 承载受控 OJ WebView、登录界面、考试选择与只读考试状态；应用启动预检、系统出站守卫、联网与录像由本 crate 与 [cvbbmso-exam](cvbbmso-exam.md) 编排。

## 桌面底座

主窗口顶部在监考连接断开或录像失败时显示独立的高对比警示，滚动页面时保持可见。恢复连接或结束后更新并清除不再适用的连接警示；录像最终失败时保留警示。原始状态文字继续保留。

客户端在 Windows 和 macOS 启动本地控制窗。Windows 使用安装后的程序，macOS 正式启动要求 macOS 14 或更新版本。先从 DMG 将已签名的 CVBBMSO.app 拖到「应用程序」，再以当前登录用户在 Finder 中双击打开。准入通过后，可以从控制窗打开独立的 Accoding 窗口。关闭控制窗或点击「退出客户端」时先显示退出确认，取消或按 Escape 后继续运行；入场后直至服务端确认本场结束回执，禁止主动关闭客户端或退出当前考试；计算录像 SHA-256、封存合并、可靠事件补报及等待最终 ACK 均在保护范围内。收尾完成后提示「考试结束后，请等待监考老师指示后再关闭客户端。」确定退出后清理网络守卫并关闭 Accoding 窗口。`inspect_session` 将 Cookie 是否可用定向通知本地 UI。加载报名列表时由监考服务复验 Accoding 身份。

部署时可在编译环境中设置 `CVBBMSO_SERVER_ORIGIN`，例如 `https://monitor.example.org`。该值须为 HTTPS Origin，可含端口。显式配置无效地址（含空字符串）会导致启动失败。未设置该变量时，控制窗可照常启动，加载报名列表会提示未配置监考服务地址。远程页面和前端命令均无法设置请求目的地。应用数据根目录通过 Tauri 的 `app_local_data_dir()` 取得。Windows 的 Accoding WebView 用该目录下的 `accoding/` 保存浏览器配置，macOS 使用系统管理的应用 WebKit 数据存储。

关闭 Accoding 窗口会暂时隐藏它。再次点击「打开 Accoding」会恢复原页面和会话。退出应用后的 Cookie 保留遵循站点设置的有效期和 WebView 的存储规则。

同源弹窗链接在现有 Accoding 窗口打开。客户端根据身份接口 URL 读取原生 Cookie Store，并保留适用的 HttpOnly 会话和附加 Cookie。界面只显示是否读取到会话，Cookie 原值仅供 Rust 使用。

运行 `cargo test --release --locked -p cvbbmso-client --bin cvbbmso-client -- --native-smoke` 可在交互式桌面验证预检失败后的重试、两类 WebView 的权限隔离、导航、Release 调试设置、合成 Cookie 更新以及关闭并重新打开 Accoding 窗口后的会话保留。该测试使用模拟 Accoding 登录页和合成会话。将末尾参数换为 `--native-smoke-detected` 可验证检测到虚拟机后的本地阻断与退出。两组冒烟都验证直接调用受限命令不会创建远程 WebView 或外发请求。协议替身绑定回环地址，以临时 TLS 证书和合成凭据验证 HTTPS 接口、WSS 握手及首帧。

将末尾参数换为 `--native-exam-smoke`，可用回环 HTTPS/WSS 服务和合成 Cookie 验证实际考试运行时的入场、重登、断线恢复、导航事件和关闭。测试也会恢复两段由 Recorder 原生编码合成帧生成的 MKV，核对录像摘要、文件 SHA-256 和结束 ACK。测试子进程使用临时证书，准入替身仅编译到测试程序。

`--native-recording-smoke` 供获准的隔离桌面验收使用。该用例实际采集主显示器，使用测试窗口生成变化画面，并验证开始命令、分片封存、最终录像及结束 ACK。录像在测试临时目录，监考替身只绑定回环地址。手动触发的双平台桌面冒烟运行无屏幕采集的恢复用例。实际主屏采集在获准环境单独显式执行。

## 应用接口

本 crate 是二进制入口，公开契约是 Tauri commands。Tauri 本地命令面向可信的本地应用 UI；Accoding 远程网页使用普通 WebView 权限。

| Tauri command | 入参 | 返回数据 | 语义 |
| --- | --- | --- | --- |
| `vm_preflight` | `{ retry: bool }` | `{ status, reason }` | 读取设备检查结果。仅在上次结果为 `unknown` 时响应重试 |
| `admission_status` | `{ retry: bool }` | `{ status, reason? }` | 读取正式准入状态，或重试可恢复的失败。仅本地控制窗可调用 |
| `admission-status` 事件 | 无 | `{ status, reason? }` | 定向通知本地控制窗正式准入状态变化 |
| `open_helper_approval_settings` | 无 | `()` | macOS 等待后台服务批准时，由本地控制窗点击操作后打开系统设置 |
| `list_exams` | 无 | `ExamSummary[]` | 使用当前 Cookie 调用 `GET /api/exams` |
| `open_exam` | `{ exam_id: uuid }` | `{ "connected": true }` | 调用 `Exam::connect`；等待初始 `sync` 后返回。网络、出站守卫和权限预检已在启动时完成 |
| `get_exam_status` | 无 | `ClientView` 或 `null` | 读取当前考试状态快照 |
| `confirm_exit` | 无 | `()` | 本地控制窗确认退出后，触发现有考试与网络守卫收尾流程 |
| `close_exam` | 无 | `{ "closed": true }` | 停止当前考试运行时，封存当前录像片段并释放考试 |
| `cvbbmso-status` 事件 | 无 | `ClientView` | 考试运行时主动更新状态 |

```rust
// 本 crate 的内部组装对象，crate 私有。
struct AppState { /* 浏览器、活动 ExamHandle */ }

enum AppError {
    Auth,
    Network,
    Exam(cvbbmso_exam::ExamError),
    Io,
}

impl AppState {
    /// 读取学生站的原生 Cookie，并列出已报名考试。
    async fn list_exams(&self) -> Result<Vec<ExamSummary>, AppError>;

    /// 启动预检通过后建立考试运行时；同时承载一场活动考试。
    async fn open_exam(&self, exam_id: Uuid) -> Result<(), AppError>;

    /// 返回最近一次考试状态快照。
    fn get_exam_status(&self) -> Option<ClientView>;

    /// 停止当前考试运行时；当前片段被停止并封存。
    async fn close_exam(&self) -> Result<(), AppError>;
}
```

`AppState` 和浏览器适配器保留为 crate 私有类型；上表 Tauri command 是客户端 UI 的实际契约，`ClientView` 由 `cvbbmso-exam` 定义。`open_exam` 等待初始 `sync` 后返回 `connected`，失败时直接返回错误；`close_exam` 是切换考试或退出前的收尾入口，负责停止并封存当前片段。本场尚未收到结束 ACK 时返回 `reporting_pending`，保留运行时和上报队列。启动预检和出站守卫错误归入 `AppError::Network`。

连接期间 `get_exam_status` 返回 `null`，重复入场或同时打开另一场考试返回 `session_busy`。关闭时停止发送当前会话的状态，等待运行时收尾后释放会话。状态事件定向发送到本地控制窗。控制窗收到事件后读取 Rust 中的当前快照，避免延迟到达的旧事件覆盖新状态。

## 启动预检、出站守卫与屏幕权限

启动时先在 Rust 中执行虚拟机预检。其余准入步骤只在 `NotDetected` 后运行。两端都调用 `cvbbmso-network::Inspector::check` 检查系统代理和监考地址。代理状态须为 `clear`，监考状态须为 `reachable`。检查失败时，界面给出原因并等待重试。

- **macOS** 应用以当前登录用户运行。从磁盘映像或其他位置运行时，控制窗提示将应用复制到「应用程序」后重新打开。网络预检通过后，Rust 在主线程检查屏幕录制权限，按需请求系统授权。还未授权或需要重启时，客户端停在控制窗。之后才调用 `MacosEgressGuard::install`，守卫先核对安装来源和签名，再通过系统服务流程注册、等待批准以及启用 PF。等待批准时可打开「登录项与扩展」。返回控制窗会重新检查，拒绝后也可手动重试。PF 无法启用所需规则时保留阻断，管理员修复后可以重试。
- **Windows** 进程先检查管理员权限，再做网络预检并调用 `EgressGuard::install`。Windows 视为具备录屏权限。

刚启动、等待批准与安装期间，Rust 拒绝打开 Accoding、读取会话、请求报名列表和进入考试。守卫返回已验证的句柄后，`admission_status` 才返回 `ready` 并开放这些操作。虚拟机预检通过后，若监考地址缺失，`list_exams` 仍返回 `monitor_unconfigured`。

一次只运行一次准入检查。守卫由应用持有，每 30 秒复查。运行期校验失败时，Rust 阻止新入场并结束活动考试，界面提示联系监考人员后重新启动客户端。关闭一场考试时守卫继续运行。退出客户端会先结束并封存考试，再等待已开始的守卫安装并清理其状态。守卫清理失败时，控制窗仍可关闭，并显示错误类别。准入状态为 `cleanup_failed`，其 `reason` 携带 `code` 和 `diagnostic_saved`。客户端尝试在应用数据目录写入最近一次失败的 `egress-cleanup.json`，只记录操作、时间、平台和错误类别。macOS 的目录为 `~/Library/Application Support/edu.buaa.accoding.cvbbmso/`，Windows 为 `%LOCALAPPDATA%\edu.buaa.accoding.cvbbmso\`。

Release 桌面冒烟使用替身检查权限顺序、并发重试、macOS 等待批准和 PF 阻断，以及运行期守卫失效。`--native-admission-exit` 覆盖安装尚未返回时退出。`--native-admission-cleanup-failure` 覆盖重复关闭、诊断和确认退出。签名 DMG、系统服务授权、真实 PF 与 WFP 在隔离环境中验收。

### 虚拟机预检

- Windows 通过系统自带的本地 CIM 查询读取 `Win32_ComputerSystem` 的厂商和型号。首版识别 Hyper-V、VMware、VirtualBox、Parallels（含 Windows ARM）以及 QEMU/KVM 的已知厂商和型号组合。厂商和型号冲突，或已识别为虚拟化厂商但型号无法核对时，返回 `Unknown`。`HypervisorPresent` 只记录辅助诊断，启用 Hyper-V、VBS 或内存完整性的实体机可在硬件信息通过核验后正常使用。[Windows CIM 字段](https://learn.microsoft.com/en-us/windows/win32/cimwin32prov/win32-computersystem) · [Parallels ARM 来宾设备报告](https://learn.microsoft.com/en-us/answers/questions/5181595/i-am-unable-to-activate-office-365-arm-on-windows?page=6)
- 主板固件可能为实体机提供 `System manufacturer`、`System Product Name` 等占位值。首版对无法核验的占位值显示 `Unknown` 和联系考试管理员的指引。验证补充信号时，可只读采集 `Win32_BaseBoard` 的厂商和产品型号，并与实体机产品资料及虚拟主板样本交叉核对。主板字段也来自 SMBIOS，需在带占位值的实体机与来宾上完成对照后才能作为准入证据。[ASUS 硬件管理手册](https://dlcdnets.asus.com/pub/ASUS/mb/14Utilities/ASUS_Control_Center_Express_EM_WEB.pdf) · [Windows 主板字段](https://learn.microsoft.com/en-us/windows/win32/cimwin32prov/win32-baseboard)
- macOS 读取 `hw.model` 和 `kern.hv_vmm_present`。`VirtualMac` 系列型号与内核客户机标志一致时判为 `Detected`。识别为实体 Mac 型号且内核客户机标志为 0 时判为 `NotDetected`。结果不一致、关键值缺失或无法识别时返回 `Unknown`。该检查不使用宿主机虚拟化能力、图形渲染模式或 WebGL。
- 查询只读。macOS 最长等待 5 秒。Windows 的 15 秒上限涵盖系统进程启动、CIM 加载及查询。关键读取失败、超时或证据冲突的诊断仅在本地保留预定义类别，内容不含设备序列号或唯一标识。`Detected` 显示阻断信息及退出入口。`Unknown` 显示原因和重试入口。初次结果返回前也禁止使用受限命令。
- 新增本地命令 `vm_preflight`，输入 `{ retry: bool }`，返回 `{ status, reason }`，供控制窗显示状态。显式重试只在 `Unknown` 后重新读取系统信息。现有命令的名称、入参和成功载荷保持原契约。阻断时，`open_accoding` 和 `inspect_session` 返回本地提示，`list_exams` 返回 `preflight_blocked`。
- 托管运行器的桌面冒烟仅在测试构建中使用预设结果。换用测试参数 `--native-vm-probe` 可记录当前系统原生探测的结果。部署验收还需记录实际 Windows 实体机（启用内存完整性）、Windows 虚拟机、Apple silicon 实体 Mac 和 Apple silicon macOS 虚拟机的系统版本及适用的虚拟机产品。

## 受控浏览器规范

固定允许的网页 Origin 为 `https://accoding.buaa.edu.cn`。保留正常浏览器中的 Accoding 登录、题目、编辑、输入法、剪贴板与提交功能；主框架导航、弹窗和重定向限定同 Origin。客户端提供精简窗口：地址栏、自由标签页和外部协议入口关闭；Release 模式保持开发者工具与调试快捷键关闭。

Windows 基于 WebView2 配置 `AreDevToolsEnabled=false`、`AreBrowserAcceleratorKeysEnabled=false`；macOS 基于 WKWebView 配置 `isInspectable=false`。资源访问限制由 WebView 平台适配与 OJ 的 CSP 协作，URL 校验以解析后的协议、主机和端口为准。剪贴板是保留的浏览器能力。

## Cookie 登录与考试连接

报名列表中 `phase == ended` 的考试保持显示，但禁止进入。初次渲染和考试会话状态刷新均保持其「进入考试」按钮禁用，入场操作也拒绝该阶段，避免新会话停留在无法收到结束指令的等待状态。

学生在 WebView 加载 `https://accoding.buaa.edu.cn/user/login`。浏览器完成 `GET /user/login`、带 `_csrf` 的表单 POST、跳转和 `Set-Cookie` 处理；Rust 原生代码从 WebView Cookie Store 取得 `connect.sid` 和可用的附加 Cookie，创建 `cvbbmso-accoding::AccodingCookies`。

每次加载或刷新报名列表，`list_exams` 都从原生 Cookie Store 读取当前会话，并通过 [`cvbbmso-exam::Monitor`](cvbbmso-exam.md#公开-rust-api) 向构建时配置的 HTTPS Origin 发送 `GET /api/exams`。列表命令只把通过校验的 `items` 返回本地控制窗。HTTP 200 且列表为空表示用户已登录但未报名。HTTP 401 提示重新登录，HTTP 503 提示稍后重试。刷新时清空旧列表，响应超限、格式无效或请求失败时显示相应错误。

客户端在首次入场时将 `device_id` 持久化到应用数据根目录，后续入场读取同一标识。`instance_id` 在进程启动时生成，同一进程内的重连和重新入场都沿用它。准入通过后，Rust 读取原生 Cookie 并调用 `Exam::connect(config, exam_id, cookies)`，由运行时重新获取 `user` 并校验报名，再连接 `GET /api/exams/{exam_id}/ws`。

活动考试期间，Rust 间隔两秒读取原生 Cookie Store，通过 `UiNotice::CookiesUpdated` 提交有效的变化。读取失败或暂时没有会话时保留上次结果，自动采样只在 Cookie 变化时触发复验。监考服务短时无法复验时，学生可点击「检查会话」，用浏览器里的当前 Cookie 重试。运行时核对考生身份，采用同一考生的新 Cookie 后恢复连接。控制窗根据 `ClientView` 提示重连，在 `ACCODING_AUTH_EXPIRED` 时提供重新登录入口。学生点击后，控制窗在受控 Accoding 窗口打开登录页。

受控窗口拒绝跨源导航或弹窗时，向活动运行时非阻塞地提交 `UiNotice::NavigationDenied`。通知只携带可解析的目标 Origin。关闭或结束考试会丢弃该场考试待提交的浏览器通知，后续考试使用自己的通知队列。

## 模块协作

```mermaid
flowchart LR
    UI[本地 Tauri UI] --> Web[受控 Accoding WebView]
    Web --> Cookie[原生 Cookie Store]
    Cookie --> Auth[cvbbmso-accoding]
    Auth --> Run[cvbbmso-exam]
    UI --> Net[cvbbmso-network / egress guard]
    UI --> Run
    Run --> UI
```

考试开始和结束由 WebSocket 服务端指令驱动；本地 UI 负责展示 `ClientView`，包括当前考试、连接状态、录屏状态及错误码。一个进程同时承载一场活动考试；切换考试前必须调用 `close_exam`。
