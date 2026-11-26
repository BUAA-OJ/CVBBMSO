# cvbbmso-protocol — HTTP / WebSocket 协议

本 crate 是线上消息及其 Schema 的唯一来源，定义可序列化的 DTO、枚举、字段约束和错误码。所有 JSON 字段与枚举值使用 `snake_case`；错误码使用大写 `SNAKE_CASE`；时间使用 UTC RFC 3339 字符串，UUID 使用标准文本形式，SHA-256 使用 64 位小写十六进制字符串。

客户端与服务端同版本部署；协议按同版本契约设计。

## 公开 Rust 类型

```rust
pub struct AccodingUser {
    pub id: i64,
    pub username: String,
    pub nickname: String,
}

pub struct ExamSummary {
    pub id: Uuid,
    pub title: String,
    pub start_at: OffsetDateTime,
    pub end_at: OffsetDateTime,
    pub phase: ExamPhase,
}

pub struct ExamListResponse {
    pub user: AccodingUser,
    pub items: Vec<ExamSummary>,
}

#[serde(rename_all = "snake_case")]
pub enum ExamPhase { Scheduled, Running, Ended }

#[serde(rename_all = "snake_case")]
pub enum Platform { Windows, Macos }

#[serde(rename_all = "snake_case")]
pub enum ExecutionState { Ready, Recording, Finalizing, Finished }

#[serde(rename_all = "snake_case")]
pub enum RecordingState { Idle, Capturing, Finalizing, Complete, Failed }

#[serde(rename_all = "snake_case")]
pub enum CommandName { StartExam, EndExam }

#[serde(rename_all = "snake_case")]
pub enum CommandStatus { Completed, Failed }

pub struct ClientState {
    pub execution: ExecutionState,
    pub recording: RecordingState,
}

pub struct RecordingPolicy {
    pub segment_duration_secs: u32,
    pub fps: u32,
}

pub struct Sha256Digest(String);
impl Sha256Digest {
    /// 校验并构造 64 位小写十六进制摘要。
    pub fn parse(value: &str) -> Result<Self, ProtocolError>;
    /// 返回适用于 JSON 的摘要原文。
    pub fn as_str(&self) -> &str;
}

pub enum ProtocolError {
    InvalidDigest,
    InvalidUserId,
    InvalidRecordingPolicy,
    InvalidTimeRange,
    InvalidSegmentIndex,
    InvalidSegmentCount,
}

pub struct SegmentMeta {
    /// 从 1 开始的录制顺序索引。
    pub index: u32,
    pub start_at: OffsetDateTime,
    pub end_at: OffsetDateTime,
    pub sha256: Sha256Digest,
}

pub struct RecordingSummary {
    pub segment_count: NonZeroU32,
    pub merged_sha256: Sha256Digest,
}

pub struct ClientIncident {
    pub code: IncidentCode,
    pub occurred_at: OffsetDateTime,
    pub detail: Option<String>,
}

#[serde(rename_all = "snake_case")]
pub enum IncidentCode {
    RecordingFailed,
    RecordingRecovered,
    NavigationDenied,
}

#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ExamEvent {
    SegmentFinalized(SegmentMeta),
    RecordingFinalized(RecordingSummary),
    Incident(ClientIncident),
}
```

`AccodingUser.id` 为正整数。`ExamPhase` 由服务端时钟和固定考试时间推导。`RecordingPolicy` 两个参数均大于零。`SegmentMeta.index` 从 1 开始，按录制顺序连续递增；`start_at <= end_at`；`sha256` 对应封存视频片段的完整文件字节。`recording_finalized` 的载荷仅包含 `segment_count` 与 `merged_sha256`。前者为正 `u32`，后者对合并后的整个视频文件计算。

`AccodingUser`、`ExamSummary`、`RecordingPolicy`、`SegmentMeta`、`RecordingSummary` 均提供校验构造器（`new`），违反上述约束时返回 `ProtocolError`；反序列化执行同一套校验，非法消息直接失败。`RecordingSummary` 拒绝额外字段。时间字段在构造和反序列化时归一化为 UTC，并按 RFC 3339 序列化。`Sha256Digest` 在构造和反序列化时校验格式，摘要统一为 64 位小写十六进制。`ClientIncident.detail` 为可选的简短字符串，异常分类以稳定的 `code` 为准。

### `ClientState` 合法组合

`execution` 与 `recording` 只能以以下组合出现；服务端校验该矩阵，其他组合按 `PROTOCOL_ERROR` 处理：

| `execution` | 允许的 `recording` | 含义 |
| --- | --- | --- |
| `ready` | `idle` | 已连接，尚未开始录制 |
| `recording` | `capturing` | 正常录制 |
| `recording` | `failed` | 录制故障、重试中或重试耗尽；考试继续 |
| `finalizing` | `finalizing` | 已停止采集，正在封存或合并 |
| `finalizing` | `complete` | finalize 已完成，等待结束握手 |
| `finalizing` | `failed` | finalize 失败，等待结束握手 |
| `finished` | `complete` | 正常结束 |
| `finished` | `failed` | 结束但录像摘要失败 |

`ClientHello.state` 描述连接时的客户端状态，服务端据此决定生成什么命令。上报 `finished` 时须与服务端已提交的 `end_exam` 终态一致。丢失 ACK 的客户端可用 `finalizing` 重连，收到新命令后重报终态。`Status` 用于状态对齐，结束由 `command_result` 驱动。同一连接中，`Status.execution` 只允许在 `ready → recording → finalizing` 的顺序上保持或前进，可跳过中间阶段；倒退按 `PROTOCOL_ERROR` 关闭连接，原状态不变。同阶段仍允许合法的采集状态变化。`Status` 上报 `finished` 不提交终态，且不能覆盖已提交终态。重连 `Hello` 按前述恢复规则独立处理，不受同一连接的阶段顺序限制。

## HTTP 接口

| HTTP 资源 | JSON 结构 | 字段含义 |
| --- | --- | --- |
| `GET /api/exams` 响应 | `ExamListResponse` | `user` 是当前 Accoding 用户；`items` 为已报名考试 |
| `GET /api/health` 响应 | `{ "status": "ok" }` | 服务存活状态 |
| 所有 HTTP 错误 | `{ "error": { "code": string, "message": string } }` | `code` 是稳定错误码，`message` 是展示文案 |

`ExamSummary.phase` 由服务端时钟确定：开始前为 `scheduled`，进行中为 `running`，结束后为 `ended`。`GET /api/exams` 的 `user` 与后续 WebSocket 认证身份一致。

```json
{
  "user": {"id":12345,"username":"user@example.com","nickname":"示例用户"},
  "items": [{
    "id":"00000000-0000-0000-0000-000000000010",
    "title":"程序设计期末考试",
    "start_at":"2026-12-20T01:00:00Z",
    "end_at":"2026-12-20T04:00:00Z",
    "phase":"scheduled"
  }]
}
```

管理员 HTTP 独立于考生协议，数据结构位于 `cvbbmso_protocol::admin`，使用相同的时间、UUID、录像参数和事件格式。管理载荷里的用户 ID 与事件序号使用十进制字符串；路由、认证及错误映射见 [服务端文档](cvbbmso-server.md#管理控制面板)。

## 错误码

所有 HTTP 错误体、WebSocket `error` 和 `command_result.error_code` 使用同一套错误码。错误码表示稳定类别；展示文案由 `message` 提供。

| 错误码 | 使用位置 | 含义 |
| --- | --- | --- |
| `INVALID_REQUEST` | HTTP 400 / WS error | 参数、路径或消息格式无效 |
| `ACCODING_AUTH_EXPIRED` | HTTP 401 / WS error | Cookie 缺失，或 Accoding 返回匿名状态 |
| `NOT_ENROLLED` | HTTP 403 / WS error | 访问者不在该考试的报名名单中 |
| `EXAM_NOT_FOUND` | HTTP 404 | 指定的考试不存在 |
| `ACCODING_UNAVAILABLE` | HTTP 503 | Accoding 身份服务暂时不可用 |
| `INTERNAL_ERROR` | HTTP 500 / WS error / CommandResult | 内部错误 |
| `PROTOCOL_ERROR` | WS error | 消息顺序、类型、字段或状态组合与协议不符 |
| `COMMAND_STATE_CONFLICT` | WS error / CommandResult | 客户端状态无法执行指令，或服务端收到与命令状态冲突的结果 |
| `RECORDING_START_FAILED` | CommandResult | 启动或恢复录屏失败 |
| `RECORDING_FINISH_FAILED` | CommandResult | 停止、合并或计算摘要失败，包括无分片 |

HTTP 状态码与错误码的对应关系由 [cvbbmso-server](cvbbmso-server.md) 维护；本表定义错误码本身。

## WebSocket 消息类型

连接端点为 `GET /api/exams/{exam_id}/ws`，使用 WSS，身份来自握手时的 Accoding Cookie。连接建立后客户端先发送 `hello`，服务端返回 `sync`，随后在同一连接上双向传输指令、状态、心跳和事件。每条消息是一个 UTF-8 JSON 文本帧，顶层必须有 `type` 字段。

```rust
pub struct ClientHello {
    pub device_id: Uuid,
    pub instance_id: Uuid,
    pub platform: Platform,
    pub state: ClientState,
}

#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello(ClientHello),
    Heartbeat,
    Status { state: ClientState },
    Event { seq: i64, event: ExamEvent },
    CommandResult {
        command_id: Uuid,
        status: CommandStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_code: Option<String>,
    },
}

#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Sync {
        server_time: OffsetDateTime,
        phase: ExamPhase,
        start_at: OffsetDateTime,
        end_at: OffsetDateTime,
        recording_policy: RecordingPolicy,
        ack_event_through: i64,
    },
    Command {
        command_id: Uuid,
        name: CommandName,
        effective_at: OffsetDateTime,
    },
    CommandAck {
        command_id: Uuid,
    },
    Error { code: String, message: String },
}
```

`ClientMessage` 与 `ServerMessage` 是 Serde 封闭枚举，协议字段由对应变体直接决定。`Hello(ClientHello)` 序列化时将 `ClientHello` 字段直接展开在顶层 JSON。`Hello.device_id` 是持久设备标识，`instance_id` 是每次客户端进程启动生成的新 UUID。`Status` 发送运行快照；`Heartbeat` 是无载荷消息，默认每 5 秒发送一次。

`CommandResult` 使用当前 WebSocket 会话收到的 `command_id`，报告终态 `completed` / `failed`。`completed` 省略 `error_code`；`failed` 必须提供错误码。两种命令均允许 `INTERNAL_ERROR` 和 `COMMAND_STATE_CONFLICT`；录像相关失败码分别为 `start_exam` 的 `RECORDING_START_FAILED` 和 `end_exam` 的 `RECORDING_FINISH_FAILED`。每个命令由服务端生成新的唯一 `command_id`；重连或进程重启后，服务端根据 `hello.state` 与考试阶段决定是否生成新的 `start_exam` / `end_exam`。客户端对命令语义幂等：已在采集时再次 `start_exam` 保持当前采集，已完成 finalize 时再次 `end_exam` 返回同一录像摘要。

`CommandAck` 表示服务端已提交对应的命令终态。客户端收到 ACK 后可以清理对应的本地命令结果。`Command.effective_at` 是服务端时间线上的生效时刻；客户端使用 `sync.server_time` 计算时钟偏移，当前时间早于生效时刻则等待，达到生效时刻后立即执行。

| 方向 | `type` | 关键字段 | 含义 |
| --- | --- | --- | --- |
| C→S | `hello` | `device_id`, `instance_id`, `platform`, `state` | 新连接恢复身份绑定与执行状态 |
| C→S | `heartbeat` | 无 | 心跳，默认 5 秒一次 |
| C→S | `status` | `state: ClientState` | 客户端当前状态快照 |
| C→S | `event` | `seq: i64`, `event: ExamEvent` | 可靠、顺序上报的业务事件 |
| C→S | `command_result` | `command_id: uuid`, `status`, `error_code?` | 当前服务端指令的执行终态 |
| S→C | `sync` | `server_time`, `phase`, `start_at`, `end_at`, `recording_policy`, `ack_event_through` | 服务器权威快照及已确认事件序号 |
| S→C | `command` | `command_id`, `name`, `effective_at` | 开始或结束指令；每次生成新 ID |
| S→C | `command_ack` | `command_id` | 已接收并处理对应 `command_result` |
| S→C | `error` | `code`, `message` | 协议错误或权限错误 |

### 消息示例

```json
{"type":"hello","device_id":"00000000-0000-0000-0000-000000000020","instance_id":"00000000-0000-0000-0000-000000000030","platform":"windows","state":{"execution":"ready","recording":"idle"}}
```

```json
{"type":"sync","server_time":"2026-12-20T00:59:55Z","phase":"scheduled","start_at":"2026-12-20T01:00:00Z","end_at":"2026-12-20T04:00:00Z","recording_policy":{"segment_duration_secs":30,"fps":15},"ack_event_through":3}
```

```json
{"type":"command","command_id":"00000000-0000-0000-0000-000000000040","name":"start_exam","effective_at":"2026-12-20T01:00:00Z"}
```

```json
{"type":"heartbeat"}
```

```json
{"type":"status","state":{"execution":"recording","recording":"failed"}}
```

```json
{"type":"event","seq":4,"event":{"kind":"segment_finalized","data":{"index":1,"start_at":"2026-12-20T01:00:00Z","end_at":"2026-12-20T01:00:30Z","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}}
```

```json
{"type":"event","seq":5,"event":{"kind":"incident","data":{"code":"recording_failed","occurred_at":"2026-12-20T01:00:31Z","detail":"capture stream stopped"}}}
```

```json
{"type":"event","seq":6,"event":{"kind":"recording_finalized","data":{"segment_count":1,"merged_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}}
```

```json
{"type":"command_result","command_id":"00000000-0000-0000-0000-000000000040","status":"completed"}
```

```json
{"type":"command_ack","command_id":"00000000-0000-0000-0000-000000000040"}
```

```json
{"type":"error","code":"PROTOCOL_ERROR","message":"hello must be the first message"}
```

`ExamEvent` 的 JSON 采用 `{"kind":"...","data":{...}}`，`kind` 支持 `segment_finalized`、`recording_finalized`、`incident`。`IncidentCode` 的线上字符串为对应变体的 `snake_case`。服务端异常记录（`connection_lost`、`connection_restored`、`process_restarted`）由领域层保存；客户端 `ExamEvent` 覆盖分片、录像摘要和客户端异常。

## 顺序、去重与时序约定

- 可靠 `event.seq` 从每个 `(exam_id, oj_user_id)` 的 `1` 开始，持久化后递增，最大为 `i64::MAX`。服务端以该联合标识加 `seq` 去重。
- 事件确认通过 `sync.ack_event_through` 返回，取值 `0` 至 `i64::MAX`，`0` 表示尚无连续事件。客户端重连后补发 `seq > ack_event_through` 的事件。收到当前 `end_exam` 的 `CommandAck` 后，客户端才可清理本场事件。
- `Recorder::open` 恢复的已封存分片会全部重新发出 `SegmentFinalized`；运行时为每个分片复用已持久化的原 `seq`，没有原 `seq` 的才分配新序号。服务端按 `seq` 幂等去重。同一 `seq` 重复且内容相同则忽略；内容不同按 `PROTOCOL_ERROR` 处理。
- `command_id` 标识一次具体命令下发；每个命令 ID 唯一。服务端按 `hello.state` 与 `phase` 生成新命令；客户端记录处理结果并回复终态。
- 考试开始与结束均以 `command` 为触发源；`sync.phase` 用于状态对齐及决定应生成什么命令。`status` 用于状态对齐；结束由 `command_result` 与 `CommandAck` 完成。
- `end_exam` 得出成功或失败结果后，客户端先按序发送本场尚未送达的可靠事件，再在同一连接发送 `command_result`。成功时发送 `recording_finalized`，服务端确认摘要及截至该事件的序号均已连续落库。分片数和合并哈希均相同的摘要可重报，任一字段不同按 `PROTOCOL_ERROR` 处理。
- 服务端在同一事务归档收到的终态并将执行状态置为 `finished`，提交后才回复 `CommandAck`。客户端等待 ACK 后关闭连接。连接在 `finished` 之前关闭、心跳超时或被替换时按连接丢失处理。
- 客户端与服务端同版本部署，协议在两端同步升级。
