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

`ExamService::connect` 验证报名资格，并在相同 `(exam_id, oj_user_id)` 上恢复既有状态。`hello.instance_id` 与上次不同且考生已开始考试（`execution` 为 recording / finalizing / finished）时，写入 `process_restarted` 异常；重连成功时写入 `connection_restored`。`receive` 持久化可靠事件并返回累计 ACK；`disconnect` 由 server 对当前有效连接调用，`Closed`/`HeartbeatTimeout` 写入 `connection_lost`，`Replaced` 以 `detail = "replaced"` 写入 `connection_lost`。正常完成考试时连接静默关闭，连接丢失记录由 `disconnect` 产生。

`due_commands` 根据服务端时钟生成开始/结束命令；实际网络推送由服务端执行。`connect` 可在迟到、重连或进程重启时补发命令。公共结果是 `completed` / `failed`；投递与重发细节由服务端内部处理。

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

## 数据所有权

本 crate 拥有以下业务数据；具体表结构和索引由实现决定：

| 数据 | 含义 |
| --- | --- |
| 考试记录 | `exam_id`、标题、固定起止时间、录像策略 |
| 报名关系 | `(exam_id, oj_user_id)` 是否允许参加 |
| 考生运行状态 | 当前设备/进程、连接状态、执行状态、最后心跳、事件确认位置 |
| 指令记录 | 已下发或待下发的开始/结束命令及其终态结果 |
| 可靠事件 | 客户端上报的 `seq`、事件内容和接收时间 |
| 异常记录 | 服务端与客户端按发生时间保存的证据 |
| 录像摘要 | 分片元信息和合并文件摘要 |
| 管理审计 | actor、操作、目标、时间 |

## 考试阶段与调度

`phase` 由服务端时钟推导：

- `now < start_at`：`scheduled`
- `start_at <= now < end_at`：`running`
- `now >= end_at`：`ended`

调度器定期调用 `due_commands`：

- 到达 `start_at` 时，为已报名且需要开始录制的考生生成 `start_exam`；离线考生保留命令，重连后补发。
- 到达 `end_at` 时，为已开始录制且需要收尾的考生生成 `end_exam`；离线考生在重连后补发。
- 迟到、重连或进程重启时，`connect` 可以补发 `start_exam` / `end_exam`；需要重新开始录屏时生成新的 `start_exam` 和新的 `command_id`。
- 同一考生同一命令类型的待完成命令复用既有 `command_id`；上一命令进入终态后，需要重新开始录屏时生成新的 `start_exam` 和新的 `command_id`。客户端对重复命令幂等处理。

## 事件、异常与录像摘要

客户端 `event.seq` 按 `(exam_id, oj_user_id)` 连续递增。`receive` 以联合标识和 `seq` 去重，持久化后推进最大连续确认序号 `ack_event_through`。`recording_finalized` 保存分片清单和合并摘要；`incident` 保存为异常记录。分片按 `index` 去重，重启后的重复上报按 `index` 幂等处理。

异常记录按发生时间保存，每条记录包含 `occurred_at`：

| 来源 | `kind` | 含义 |
| --- | --- | --- |
| server | `connection_lost` | 当前连接关闭、心跳超时、被新连接替换或服务端重启；`detail` 说明原因 |
| server | `connection_restored` | 考生重新连接成功 |
| server | `process_restarted` | `instance_id` 变化，表明客户端进程重启 |
| client | `proxy_detected` | 检测到系统代理/PAC/代理环境变量 |
| client | `proxy_check_failed` | 代理检查无法完成 |
| client | `recording_failed` | 录屏启动、采集、编码或写盘失败 |
| client | `recording_recovered` | 录屏恢复；作为独立证据保存 |
| client | `navigation_denied` | 受控浏览器拒绝了导航请求 |

`connection_lost` 与 `connection_restored` 是两条独立记录，CLI 按 `occurred_at` 排序展示和关联。`detail` 为可选字符串，例如 `heartbeat_timeout`、`replaced`、`system proxy enabled`。异常按发生记录保存，消费方按时间判断关联。

## 错误映射

`DomainError` 是领域层对外错误类别，server 和 CLI 负责转换为各自协议：

| 错误 | 典型场景 | 对外映射 |
| --- | --- | --- |
| `NotFound` | 考试、考生、异常或录像不存在 | `EXAM_NOT_FOUND` / CLI 错误 |
| `NotEnrolled` | 访问者不在该考试的报名名单中 | `NOT_ENROLLED` |
| `InvalidInput` | 时间、策略、CSV 或参数非法 | `INVALID_REQUEST` |
| `Conflict` | 重复报名、状态冲突 | `INVALID_REQUEST` / CLI 错误 |
| `Storage` | 数据库或持久化失败 | `INTERNAL_ERROR` |
