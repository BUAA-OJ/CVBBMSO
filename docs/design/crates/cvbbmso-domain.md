# cvbbmso-domain — 考试业务与异常记录

本 crate 统一管理考试规则、报名关系、服务端时钟调度、考生运行状态、指令、可靠事件、异常记录和录像摘要。业务记录以 `(exam_id, oj_user_id)` 唯一定位每名考生的考试过程。HTTP 和 WebSocket 解析由 `cvbbmso-server` 负责，管理员操作由 `cvbbmso-cli` 调用本 crate。

本文档定义公开 Rust 接口、数据所有权和跨模块行为；Repository、事务、SQL 实体和表结构由实现负责。

## 公开 Rust API

```rust
pub struct ExamService { /* 私有数据库与领域状态 */ }
pub struct AdminService { /* 私有数据库与领域状态 */ }

pub struct ConnectOutcome {
    pub sync: cvbbmso_protocol::ServerMessage,
    pub commands: Vec<cvbbmso_protocol::ServerMessage>,
}

pub struct DirectedCommand {
    pub exam_id: uuid::Uuid,
    pub oj_user_id: i64,
    pub command: cvbbmso_protocol::ServerMessage,
}

pub enum DisconnectReason { Closed, HeartbeatTimeout, Replaced }

pub enum DomainError {
    NotFound,
    NotEnrolled,
    InvalidInput,
    Conflict,
    Storage,
}

impl ExamService {
    /// 列出指定 Accoding 用户报名的考试。
    pub async fn list_for(
        &self, oj_user_id: i64,
    ) -> Result<Vec<cvbbmso_protocol::ExamSummary>, DomainError>;

    /// 建立或恢复考试考生记录；返回服务端快照与待下发命令。
    pub async fn connect(
        &self,
        exam_id: uuid::Uuid,
        oj_user_id: i64,
        hello: cvbbmso_protocol::ClientHello,
    ) -> Result<ConnectOutcome, DomainError>;

    /// 处理客户端状态、心跳、事件和命令结果；返回协议应答。
    pub async fn receive(
        &self,
        exam_id: uuid::Uuid,
        oj_user_id: i64,
        message: cvbbmso_protocol::ClientMessage,
    ) -> Result<Vec<cvbbmso_protocol::ServerMessage>, DomainError>;

    /// 记录当前有效连接的丢失、心跳超时或替换事件。
    pub async fn disconnect(
        &self,
        exam_id: uuid::Uuid,
        oj_user_id: i64,
        reason: DisconnectReason,
    ) -> Result<(), DomainError>;

    /// 根据固定考试时间生成并返回需要推送的持久化命令。
    pub async fn due_commands(&self) -> Result<Vec<DirectedCommand>, DomainError>;
}

impl AdminService {
    /// 执行一个 CLI 管理操作并返回结构化结果；actor 写入审计。
    pub async fn execute(
        &self,
        actor: String,
        op: AdminOperation,
    ) -> Result<AdminOutput, DomainError>;
}
```

`ExamService::connect` 验证报名资格，并在相同 `(exam_id, oj_user_id)` 上恢复既有状态。`hello.state` 是客户端连接时刻的真实状态；服务端据此校正持久状态并决定是否生成命令。

`connect` 记录异常的条件：

- `hello.instance_id` 与上次不同，且上次状态为 `recording` 或 `finalizing` 时，写入 `process_restarted`。
- 此前存在尚未恢复的 `connection_lost` 时，写入 `connection_restored`；首次连接直接建立记录。

`receive` 持久化可靠事件并推进确认位置，处理命令结果并返回 `CommandAck`。`disconnect` 由 server 对当前有效连接调用：执行状态为 `finished` 的连接关闭按正常完成处理；其他关闭按 `Closed` / `HeartbeatTimeout` / `Replaced` 分别写入 `connection_lost`，其中 `Replaced` 的 `detail` 为 `"replaced"`。正常完成考试时连接静默关闭。

`due_commands` 根据服务端时钟生成开始/结束命令；实际网络推送由服务端执行。命令由当前在线考生触发；离线考生的命令在 `connect` 时按状态生成。每个命令使用新的唯一 `command_id`。

`ConnectOutcome.sync` 是 `ServerMessage::Sync`，`commands` 中每个元素都是 `ServerMessage::Command`；`DirectedCommand.command` 同样承载 `Command` 变体。

## 管理操作与输出

`AdminOperation` 与 CLI 命令一一对应；CLI 负责参数解析、CSV 读取和展示，域层负责校验、持久化和审计。

```rust
pub enum AdminOperation {
    ExamsCreate {
        title: String,
        start_at: OffsetDateTime,
        end_at: OffsetDateTime,
        policy: cvbbmso_protocol::RecordingPolicy,
    },
    ExamsList,
    ExamsShow { exam_id: Uuid },
    ParticipantsAdd { exam_id: Uuid, oj_user_id: i64 },
    ParticipantsRemove { exam_id: Uuid, oj_user_id: i64 },
    ParticipantsImport { exam_id: Uuid, oj_user_ids: Vec<i64> },
    ParticipantsList { exam_id: Uuid },
    StudentsStatus { exam_id: Uuid },
    StudentsShow { exam_id: Uuid, oj_user_id: i64 },
    EventsList { exam_id: Uuid, oj_user_id: i64 },
    IncidentsList {
        exam_id: Uuid,
        oj_user_id: Option<i64>,
        kind: Option<String>,
        since: Option<OffsetDateTime>,
        until: Option<OffsetDateTime>,
    },
    IncidentsShow { incident_id: Uuid },
    RecordingsShow { exam_id: Uuid, oj_user_id: i64 },
}

pub enum AdminOutput {
    ExamCreated { exam_id: Uuid },
    Exams(Vec<cvbbmso_protocol::ExamSummary>),
    ExamDetail {
        summary: cvbbmso_protocol::ExamSummary,
        policy: cvbbmso_protocol::RecordingPolicy,
    },
    ParticipantAdded,
    ParticipantRemoved,
    ParticipantsImported { count: usize },
    Participants(Vec<ParticipantRecord>),
    Students(Vec<StudentRuntime>),
    Student(StudentRuntime),
    Events(Vec<EventRecord>),
    Incidents(Vec<IncidentRecord>),
    Incident(IncidentRecord),
    Recording(cvbbmso_protocol::RecordingManifest),
}

pub struct ParticipantRecord {
    pub exam_id: Uuid,
    pub oj_user_id: i64,
}

pub struct StudentRuntime {
    pub exam_id: Uuid,
    pub oj_user_id: i64,
    pub connected: bool,
    pub execution: cvbbmso_protocol::ExecutionState,
    pub last_seen_at: Option<OffsetDateTime>,
}

pub struct EventRecord {
    pub exam_id: Uuid,
    pub oj_user_id: i64,
    pub seq: u64,
    pub event: cvbbmso_protocol::ExamEvent,
    pub received_at: OffsetDateTime,
}

#[serde(rename_all = "snake_case")]
pub enum IncidentSource { Server, Client }

pub struct IncidentRecord {
    pub id: Uuid,
    pub exam_id: Uuid,
    pub oj_user_id: i64,
    pub source: IncidentSource,
    pub kind: String,
    pub occurred_at: OffsetDateTime,
    pub detail: Option<String>,
}
```

`actor` 由 CLI 提供，缺省为当前操作系统用户名；域层写入审计记录。`AdminOperation` 的输入先经过域层校验：`start_at < end_at`，`RecordingPolicy` 参数大于零，`oj_user_id` 为正整数，CSV 导入去重。错误以 `DomainError` 返回，由 server 或 CLI 映射为对外错误。

`RecordingsShow` 返回已完成的 `RecordingManifest`；未 finalize 或 finalize 失败时返回 `NotFound`。

## 数据所有权

本 crate 拥有以下业务数据；具体表结构和索引由实现决定：

| 数据 | 含义 |
| --- | --- |
| 考试记录 | `exam_id`、标题、固定起止时间、录像策略 |
| 报名关系 | `(exam_id, oj_user_id)` 是否允许参加 |
| 考生运行状态 | 当前设备/进程、连接状态、执行状态、最后心跳、事件确认位置 |
| 指令记录 | 已下发或待下发的开始/结束命令、唯一 `command_id`、终态结果与 ACK 状态 |
| 可靠事件 | 客户端上报的 `seq`、事件内容和接收时间 |
| 异常记录 | 服务端与客户端按发生时间保存的证据 |
| 录像摘要 | 分片元信息和合并文件摘要 |
| 管理审计 | actor、操作、目标、时间 |

## 考试阶段与调度

`phase` 由服务端时钟推导：

- `now < start_at`：`scheduled`
- `start_at <= now < end_at`：`running`
- `now >= end_at`：`ended`

调度器定期调用 `due_commands`；命令生成规则如下：

| 阶段 | 客户端 `hello.state` / 当前状态 | 生成命令 |
| --- | --- | --- |
| `scheduled` | 任意 | 无 |
| `running` | `ready/idle` | 新 `start_exam` |
| `running` | `recording/capturing` 或 `recording/failed` | 无（录屏故障由客户端自行重试） |
| `running` | `finalizing/*` | 无 |
| `running` | `finished/*` | 无 |
| `ended` | `recording/*` 或 `finalizing/*` | 新 `end_exam` |
| `ended` | `ready/idle` | 无 |
| `ended` | `finished/*` | 无 |

- 到达 `start_at` 时，对在线且处于 `ready/idle` 的考生生成 `start_exam`；离线考生在重连后由 `connect` 生成。
- 到达 `end_at` 时，对在线且已开始录制的考生生成 `end_exam`；离线考生在重连后由 `connect` 生成。
- 迟到、重连或进程重启时，`connect` 按上表补生成命令。每个命令使用新的 `command_id`。
- 在线调度生成前检查当前连接是否已有同类型未终态命令；已有则等待结果。
- `connect` 遇到上一连接遗留的未终态命令时，将其标记为 superseded；若 `hello.state` 表明命令效果未生效，则生成新 ID 的新命令。
- 服务端收到 `command_result` 后持久化终态并回复 `CommandAck`。若结果丢失，重连时以 `hello.state` 校正：已体现命令效果的状态按完成处理；需要继续执行时生成新命令。

## 事件、异常与录像摘要

客户端 `event.seq` 按 `(exam_id, oj_user_id)` 连续递增。`receive` 以联合标识和 `seq` 去重，持久化后推进最大连续确认序号 `ack_event_through`；确认位置在下一次 `sync` 中返回。重复 `seq` 内容相同则忽略，内容不同按协议错误处理。

`Recorder::open` 恢复的分片可能以原 `seq` 重报；服务端按 `seq` 幂等，并可按 `(exam_id, oj_user_id, index)` 做辅助去重。`recording_finalized` 保存非空分片清单和合并摘要；`incident` 保存为异常记录。

异常记录按发生时间保存，每条记录包含 `occurred_at`：

| 来源 | `kind` | 含义 |
| --- | --- | --- |
| server | `connection_lost` | 当前连接关闭、心跳超时或被新连接替换；`detail` 说明原因 |
| server | `connection_restored` | 此前丢失的考生连接重新建立成功 |
| server | `process_restarted` | `instance_id` 变化，表明客户端进程重启 |
| client | `recording_failed` | 录屏启动、采集、编码、写盘、合并或哈希失败 |
| client | `recording_recovered` | 录屏恢复；作为独立证据保存 |
| client | `navigation_denied` | 受控浏览器拒绝了导航请求 |

`connection_lost` 与 `connection_restored` 是两条独立记录，CLI 按 `occurred_at` 排序展示和关联。`detail` 为可选字符串，例如 `heartbeat_timeout`、`replaced`、`capture stream stopped`。异常按发生记录保存，消费方按时间判断关联。

## 错误映射

`DomainError` 是领域层对外错误类别，server 和 CLI 负责转换为各自协议：

| 错误 | 典型场景 | 对外映射 |
| --- | --- | --- |
| `NotFound` | 考试、考生、异常或录像不存在 | `EXAM_NOT_FOUND` / CLI 错误 |
| `NotEnrolled` | 访问者不在该考试的报名名单中 | `NOT_ENROLLED` |
| `InvalidInput` | 时间、策略、CSV 或参数非法 | `INVALID_REQUEST` |
| `Conflict` | 重复报名、命令结果与当前命令状态不匹配 | 命令结果用 `COMMAND_STATE_CONFLICT`，其他用 `INVALID_REQUEST` |
| `Storage` | 数据库或持久化失败 | `INTERNAL_ERROR` |

客户端 `command_result` 到达时，`receive` 校验 `command_id` 属于当前考生且命令仍可确认；重复的相同结果幂等忽略，未知或状态不匹配返回 `Conflict`，由 server 映射为 `COMMAND_STATE_CONFLICT`。
