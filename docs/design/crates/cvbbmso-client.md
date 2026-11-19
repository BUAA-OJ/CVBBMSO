# cvbbmso-client — Tauri 桌面客户端

本 crate 是 Windows/macOS 可执行程序。使用 Tauri 2 承载受控 OJ WebView、登录界面、考试选择与只读考试状态；应用运行、联网与录像由 [cvbbmso-exam](cvbbmso-exam.md) 编排。

## 桌面底座

当前底座可在 Windows 和 macOS 启动本地控制窗，并通过独立窗口打开 Accoding 登录页。用 `cargo run --release --locked -p cvbbmso-client` 启动。底座提供 `open_accoding`、`inspect_session` 和 `list_exams` 三个本地命令。关闭控制窗将退出应用，同时关闭 Accoding 窗口。`inspect_session` 将 Cookie 是否可用定向通知本地 UI。加载报名列表时由监考服务复验 Accoding 身份。

部署时可在编译环境中设置 `CVBBMSO_SERVER_ORIGIN`，例如 `https://monitor.example.org`。该值须为 HTTPS Origin，可含端口。地址缺失或无效时，控制窗可照常使用，加载报名列表会提示配置错误。远程页面和前端命令均无法设置请求目的地。应用数据根目录通过 Tauri 的 `app_local_data_dir()` 取得。Windows 的 Accoding WebView 用该目录下的 `accoding/` 保存浏览器配置，macOS 使用系统管理的应用 WebKit 数据存储。

关闭 Accoding 窗口会暂时隐藏它。再次点击「打开 Accoding」会恢复原页面和会话。退出应用后的 Cookie 保留遵循站点设置的有效期和 WebView 的存储规则。

同源弹窗链接在现有 Accoding 窗口打开。客户端根据身份接口 URL 读取原生 Cookie Store，并保留适用的 HttpOnly 会话和附加 Cookie。界面只显示是否读取到会话，Cookie 原值仅供 Rust 使用。

运行 `cargo test --release --locked -p cvbbmso-client --bin cvbbmso-client -- --native-smoke` 可在交互式桌面验证两类 WebView 的权限隔离、导航、Release 调试设置、合成 Cookie 更新以及关闭并重新打开 Accoding 窗口后的会话保留。该测试也会打开真实 Accoding 登录页，无需登录。协议替身绑定回环地址，以临时 TLS 证书和合成凭据验证 HTTPS 接口、WSS 握手及首帧。

## 应用接口

本 crate 是二进制入口，公开契约是 Tauri commands。Tauri 本地命令面向可信的本地应用 UI；Accoding 远程网页使用普通 WebView 权限。

| Tauri command | 入参 | 返回数据 | 语义 |
| --- | --- | --- | --- |
| `list_exams` | 无 | `ExamSummary[]` | 使用当前 Cookie 调用 `GET /api/exams` |
| `open_exam` | `{ exam_id: uuid }` | `{ "connected": true }` | 完成网络预检并调用 `Exam::connect`；等待初始 `sync` 后返回 |
| `get_exam_status` | 无 | `ClientView` 或 `null` | 读取当前考试状态快照 |
| `close_exam` | 无 | `{ "closed": true }` | 调用 `ExamHandle::shutdown`，释放当前考试 |
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

    /// 网络预检通过后建立考试运行时；同时承载一场活动考试。
    async fn open_exam(&self, exam_id: Uuid) -> Result<(), AppError>;

    /// 返回最近一次考试状态快照。
    fn get_exam_status(&self) -> Option<ClientView>;

    /// 停止当前考试运行时；已封存录像保留，可由下次连接恢复。
    async fn close_exam(&self) -> Result<(), AppError>;
}
```

`AppState` 和浏览器适配器保留为 crate 私有类型；上表 Tauri command 是客户端 UI 的实际契约，`ClientView` 由 `cvbbmso-exam` 定义。`open_exam` 等待初始 `sync` 后返回 `connected`，失败时直接返回错误；`close_exam` 是切换考试或退出前的收尾入口。

## 受控浏览器规范

固定允许的网页 Origin 为 `https://accoding.buaa.edu.cn`。保留正常浏览器中的 Accoding 登录、题目、编辑、输入法、剪贴板与提交功能；主框架导航、弹窗和重定向限定同 Origin。客户端提供精简窗口：地址栏、自由标签页和外部协议入口关闭；Release 模式保持开发者工具与调试快捷键关闭。

Windows 基于 WebView2 配置 `AreDevToolsEnabled=false`、`AreBrowserAcceleratorKeysEnabled=false`；macOS 基于 WKWebView 配置 `isInspectable=false`。资源访问限制由 WebView 平台适配与 OJ 的 CSP 协作，URL 校验以解析后的协议、主机和端口为准。剪贴板是保留的浏览器能力。

## 网络预检

学生选择考试后，客户端先调用 `cvbbmso-network::Inspector::check`。进入考试流程的条件是 `NetworkState::Clear`；检测到代理、服务端不可达或状态未知时，客户端停留在考试外，展示对应错误并允许重新检查。考试中的周期性检查和异常上报由 `cvbbmso-exam` 负责。

## Cookie 登录与考试连接

学生在 WebView 加载 `https://accoding.buaa.edu.cn/user/login`。浏览器完成 `GET /user/login`、带 `_csrf` 的表单 POST、跳转和 `Set-Cookie` 处理；Rust 原生代码从 WebView Cookie Store 取得 `connect.sid` 和可用的附加 Cookie，创建 `cvbbmso-accoding::AccodingCookies`。

每次加载或刷新报名列表，`list_exams` 都从原生 Cookie Store 读取当前会话。Rust 仅向构建时配置的 HTTPS Origin 发送 `GET /api/exams`，禁用重定向和代理。Rust 用 `ExamListResponse` 解析成功响应，只把 `items` 返回本地控制窗。HTTP 200 且列表为空表示用户已登录但未报名。HTTP 401 提示重新登录，HTTP 503 提示稍后重试。刷新时清空旧列表，失败时保留错误提示。

学生选择考试后生成当前进程 `instance_id` 并调用 `Exam::connect(config, exam_id, cookies)`；运行时重新拉取 `user` 并校验报名，再连接 `GET /api/exams/{exam_id}/ws`。Cookie 变化通过 `ExamHandle::notify(UiNotice::CookiesUpdated(...))` 传给运行时，用于后续重连；受控浏览器拒绝导航时，通过 `ExamHandle::notify(UiNotice::NavigationDenied { target_origin })` 通知运行时。

## 模块协作

```mermaid
flowchart LR
    UI[本地 Tauri UI] --> Web[受控 Accoding WebView]
    Web --> Cookie[原生 Cookie Store]
    Cookie --> Auth[cvbbmso-accoding]
    Auth --> Run[cvbbmso-exam]
    UI --> Net[cvbbmso-network]
    Run --> Net
    UI --> Run
    Run --> UI
```

考试开始和结束由 WebSocket 服务端指令驱动；本地 UI 负责展示 `ClientView`，包括当前考试、连接状态、录屏状态及错误码。一个进程同时承载一场活动考试；切换考试前必须调用 `close_exam`。
