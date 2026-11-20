# cvbbmso-exam — 客户端考试运行时

本 crate 管理客户端参加单场考试的运行过程：建立 WebSocket、响应服务端命令、驱动录像、维护本地可靠事件队列及断线恢复。运行时直接以 `exam_id` 连接监考服务端，考生身份由 Cookie 验证得到；业务标识为 `(exam_id, oj_user_id)`。

本文档定义公开接口、运行时职责和跨模块行为；本地表结构、任务划分和重连算法由实现负责。网络预检、系统出站守卫和屏幕权限属于 [cvbbmso-client](cvbbmso-client.md)。

## 公开 Rust API

```rust
pub struct ExamConfig {
    pub server_base: url::Url,
    pub app_data_dir: std::path::PathBuf,
    pub device_id: uuid::Uuid,
    pub instance_id: uuid::Uuid,
}

pub struct Exam;
pub struct ExamHandle { /* 私有运行时句柄 */ }

pub enum ExamError {
    Http(Option<reqwest::Error>),
    WebSocket(Option<tokio_tungstenite::tungstenite::Error>),
    Auth,
    NotEnrolled,
    Recording(cvbbmso_recorder::RecordingError),
    Storage(std::io::Error),
    Closed,
}

pub enum UiNotice {
    CookiesUpdated(cvbbmso_accoding::AccodingCookies),
    NavigationDenied { target_origin: String },
}

pub struct ClientView {
    pub exam_id: uuid::Uuid,
    pub connected: bool,
    pub state: cvbbmso_protocol::ClientState,
    pub last_error_code: Option<String>,
}

impl Exam {
    /// 用指定考试 ID 和 Accoding Cookie 建立或恢复考试运行时。
    /// 返回时已完成初始 sync；之后由内部任务持续维护连接、录屏与事件队列。
    pub async fn connect(
        config: ExamConfig,
        exam_id: uuid::Uuid,
        cookies: cvbbmso_accoding::AccodingCookies,
    ) -> Result<ExamHandle, ExamError>;
}

impl ExamHandle {
    /// 订阅只读状态快照，供受信任的本地 UI 展示。
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<ClientView>;

    /// 提交 Cookie 更新或浏览器拒绝导航通知。
    pub async fn notify(&self, notice: UiNotice) -> Result<(), ExamError>;

    /// 停止采集并封存当前片段，关闭 WebSocket，保留已封存分片。
    /// 合并与最终 manifest 由后续 Recorder::finish 完成。
    pub async fn shutdown(self) -> Result<(), ExamError>;
}
```

`Exam::connect` 首先调用 `GET /api/exams` 确认用户身份和考试报名，以返回的 `user.id` 定位本地 `(exam_id, oj_user_id)` 数据，然后建立 WSS 并等待初始 `sync`，再按 `sync.recording_policy` 打开/恢复 recorder。`ExamConfig.instance_id` 由客户端进程启动时生成，在该进程的所有重连中保持一致；`device_id` 是持久设备标识。`Exam::connect` 是该 crate 的唯一启动入口。

`ExamError` 是运行时错误类别：`Auth` / `NotEnrolled` 对应认证与报名错误，`Http` / `WebSocket` / `Recording` / `Storage` 对应连接、录像或存储失败，并在有底层原因时携带它，供应用层通过 `anyhow` 输出完整 source 链；`Closed` 表示句柄已关闭。本地数据写盘失败时，运行时继续录屏，在内存中保留待保存事件，并通过 `ClientView.last_error_code` 显示 `INTERNAL_ERROR`。写盘恢复后按序发送积压事件，并清除存储错误提示。

一个客户端进程内每个 `exam_id` 最多有一个活跃 `ExamHandle`，客户端整体同时承载一场活动考试。`ExamHandle` 提供状态订阅、UI 通知和关闭。

## WebSocket 协作

| 时机 | 接口与消息 | 运行时职责 |
| --- | --- | --- |
| 入场 | `GET /api/exams/{exam_id}/ws` | 传递原生 Cookie，建立单条 WSS |
| 建连 | `hello` → `sync` | 上报设备、进程实例和当前 `ClientState`；按 `sync.ack_event_through` 恢复事件队列 |
| 考前 | `status` | 上报 `ready/idle` |
| 到开始时间 | `command(start_exam)` | 确保采集运行；上报 `command_result`，等待 `CommandAck` |
| 考试中 | `heartbeat`、`status`、`event` | 保持长连接，可靠上报分片与异常 |
| 录屏故障 | `event(incident)` | 上报 `recording_failed`，重试；恢复后上报 `recording_recovered` |
| 到结束时间 | `command(end_exam)` | 停止采集、`finish`、上报摘要与 `command_result` |
| 完成 | `command_ack` | 进入 `finished`，关闭连接 |

字段及消息 Schema 由 [cvbbmso-protocol](cvbbmso-protocol.md) 唯一定义。每条连接在收到合法 `sync` 后才接受命令并开始每 5 秒发送无载荷心跳。业务事件按 `seq` 递增并先持久化后发送。事件确认通过 `sync.ack_event_through` 返回，重连后重新发送 `seq > ack_event_through` 的事件。`CommandAck` 确认对应命令的结果，等待较早命令的 ACK 不阻塞新命令的结果。`end_exam` 的 `CommandAck` 到达后，客户端清理本场事件队列并关闭连接。

### 命令语义

- `start_exam` 表示“确保采集正在运行”：已在采集时幂等成功；有已封存分片但采集已停止时恢复采集；启动失败时持续重试（见[时限与重试](#时限与重试)）。
- `end_exam` 表示“停止采集、finalize 并收尾”：已在 finalize 或已完成时返回同一 manifest；没有可收尾分片时按失败处理。
- 每个 `command_id` 唯一；重连时由服务端根据 `hello.state` 和 `phase` 生成新命令。
- `status` 仅用于状态对齐；命令确认由 `CommandAck` 负责，结束由 `command_result` 驱动。

## 运行状态

```mermaid
stateDiagram-v2
    [*] --> Ready: WSS sync
    Ready --> Recording: command(start_exam) / Recorder::start 成功
    Recording --> Recording: Fault / incident + 重试
    Recording --> Finalizing: command(end_exam) 或 end_at + 60s
    Finalizing --> Finished: finish 结论确定 + command_result + CommandAck
    Finished --> [*]: shutdown
```

`execution` 表示考试运行阶段，`recording` 表示采集阶段；二者必须符合 [protocol](cvbbmso-protocol.md#clientstate-合法组合) 的组合矩阵：

- `ready` 只能与 `idle` 同时出现。
- `recording` 与 `capturing` 或 `failed` 同时出现；`failed` 表示故障或重试中，考试仍继续。
- `finalizing` 与 `finalizing`、`complete` 或 `failed` 同时出现；`complete` / `failed` 表示 finalize 已结束、等待结束握手。
- `finished` 与 `complete` 或 `failed` 同时出现。

连接状态独立于录制状态：WSS 中断后，已运行的录屏任务继续执行，事件继续写入本地队列；重连并完成 `hello/sync` 后恢复待确认事件传输。

## 生命周期、离线与认证

- **正常完成**：收到 `end_exam` 后停止采集并执行 `finish`。成功时按录像摘要中的分片索引补记尚未入队的分片事件，先保存并发送这些事件和录像摘要。失败时先保存并发送失败事件。随后发送 `command_result` 并等待 `CommandAck`。收尾结论确定后停止接收新的导航异常，确保结束结果是本次握手的最后一条业务消息。收到 ACK 后进入 `finished`、关闭 WebSocket。等待 ACK 超时按断线重连处理。
- **finalize 失败**：`finish` 进行有限重试；仍失败时上报 `recording_failed`，发送 `command_result(failed, RECORDING_FINISH_FAILED)`，收到 `CommandAck` 后以 `finished/failed` 关闭。没有可收尾分片（`NoSegments`）同样走失败路径；`recording_finalized` 只在成功时发送。
- **主动关闭**：`ExamHandle::shutdown` 调用 `Recorder::stop` 停止采集并封存当前片段，关闭 WebSocket；合并与 manifest 由后续 `finish` 完成。已封存分片保留，下次 `Recorder::open` 恢复。服务端按普通断开记录 `connection_lost`。
- **断网跨过结束时间**：客户端使用 `sync.server_time` 计算服务端时钟偏移；到达 `end_at + 60s` 后调用 `Recorder::stop` 封存当前片段，进入 `finalizing/finalizing`，等待 `end_exam`。重连后按服务端下发的 `end_exam` 执行 `finish` 并完成收尾。
- **Cookie 过期**：运行时继续录屏，保持本地事件队列；通过 `ClientView` 通知 UI 重新登录，并按退避策略重试连接。认证失效期间录像继续。
- **更新 Cookie**：收到 `CookiesUpdated` 时，通过 `GET /api/exams` 验证新 Cookie 的报名状态，并核对 `user.id` 与当前运行时绑定的考生一致。验证通过后采用新 Cookie，离线时立即重试连接。验证失败时保留原 Cookie，账号不符时通过 `ACCODING_AUTH_EXPIRED` 提示重新登录。
- **断线重连**：保留 `instance_id` 和本地状态，完成 `hello/sync` 后补发事件。服务端根据 `hello.state` 决定是否生成新的 `start_exam` / `end_exam`。重连沿用已验证的 Cookie，按 1 秒起、每次翻倍、上限 30 秒退避。每次 `sync` 都使上一连接的命令失效，未确认的命令结果不再重发。
- **进程重启**：采集不会跨进程延续。本地记录为 `recording/*` 时，`hello.state` 报 `recording/failed`：考试进行中服务端不再生成命令，运行时自行重启采集，成功后上报 `recording_recovered`；考试已结束时服务端生成 `end_exam` 完成收尾。本地记录为 `finalizing/complete` 或 `finalizing/failed` 时，沿用已持久化的收尾结论和事件，等待新的 `end_exam` 完成确认。本地记录为 `finished/*` 时，完成 `sync` 后即关闭连接且不再重连。其他本地状态按原值上报。

### 时限与重试

| 项 | 取值 |
| --- | --- |
| 心跳 | 每 5 秒发送 `heartbeat`，同时发送 WebSocket Ping |
| 判定连接失效 | 20 秒内未收到任何帧（含 Pong）；握手与首个 `sync` 同样以 20 秒为限 |
| 发送阻塞 | 单次加入发送队列或一次事件补发最多等待 5 秒，且受本地停录截止时间约束；WebSocket 写入受连接存活期限约束。超时按断线重连处理 |
| 运行时数据写盘 | 失败后每 3 秒重试，其他写盘也可提前恢复 |
| 启动采集 | 前 5 次间隔 3 秒，之后每 60 秒一次，直到成功或考试收尾 |
| finalize | 最多 3 次，间隔 3 秒；`NoSegments` 不重试 |
| 等待 `CommandAck` | 30 秒，超时按断线重连处理 |

业务消息（`event`、`status`、`command_result`）只在当前连接完成 `hello/sync` 后发送；之前产生的事件留在本地队列，由 `sync` 后的补发送出。同一来源的 `navigation_denied` 在 5 秒内只上报一次，`detail` 为截断到 200 字符的目标 Origin。

## 录屏故障与重试

- 录屏采集、编码或写盘失败时，recorder 发送 `Fault`；运行时上报 `recording_failed` incident，并将 `recording` 置为 `failed`，`execution` 保持 `recording`。
- 运行时持续调用 `Recorder::start` 重试（间隔见上表）；成功后 `recording` 回到 `capturing`，上报 `recording_recovered`。重试期间保持 `recording/failed`，考试继续。同一次故障只上报一条 `recording_failed`。
- `start_exam` 的 `command_result` 在首次成功启动后回 `completed`；前 5 次均失败时回 `failed(RECORDING_START_FAILED)`，此后的重试不改变已发送的结果。
- 采集故障后的 `end_exam` 仍尝试 `Recorder::finish` 收尾已封存分片；没有分片时按 `RECORDING_FINISH_FAILED` 失败。
- 录屏采集范围为主显示器，音频关闭。屏幕权限探测与申请由 `cvbbmso-client` 负责。

## 本地数据所有权

本 crate 管理 `<app-data>/exams/<exam-id>/<oj-user-id>/runtime/` 下的运行时数据：考试执行阶段、待确认的可靠事件队列，以及已入队的分片 `index`。录像文件和分片元信息属于 [cvbbmso-recorder](cvbbmso-recorder.md)，位于同一考生目录下的 `recording/`。命令结果只在当前连接内保留：重连后旧命令由服务端标记为 superseded，并按 `hello.state` 生成新命令，因此不跨连接重发命令结果。

`Recorder::open` 会重新发出全部已恢复分片；已入队的分片保留其原 `seq` 继续等待确认，已确认的分片不再上报，未入队的分片分配下一个事件序号。服务端按 `seq` 去重，因此重复上报安全。分片事件仍以 `SegmentMeta.index` 作为稳定身份，服务端可按 `(exam_id, oj_user_id, index)` 做辅助去重。

事件序号由本地数据首次收到的 `sync.ack_event_through` 锚定：首次 `sync` 之前产生的事件先入队不编号，之后从 `ack_event_through + 1` 起编号；此后每次 `sync` 都不让序号回退。本地数据丢失或损坏（损坏文件改名保留后重建）时，新序号因此始终位于服务端已持久化的序号之后，不会与服务端已有的 `seq` 冲突。达到 `u64` 上限后，后续事件留在本地并显示 `INTERNAL_ERROR`。可靠事件在原子写入磁盘成功后才进入 WebSocket 发送队列。进程在写盘恢复前退出时，仅存于内存的事件随之丢失。

本地运行时数据由本 crate 管理，存储细节封装在 crate 内部；对客户端和其他 crate 提供运行时接口。
