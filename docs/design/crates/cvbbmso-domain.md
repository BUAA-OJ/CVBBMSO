# cvbbmso-domain — 考试业务与异常记录

本 crate 统一管理考试规则、报名关系、服务端时钟调度、考生运行状态、指令、可靠事件、异常记录和录像摘要。业务记录以 `(exam_id, oj_user_id)` 唯一定位每名考生的考试过程。HTTP 和 WebSocket 解析由 `cvbbmso-server` 负责。可选的 HTTP 控制面板和设计中的本机 CLI 均通过 `AdminService` 执行管理操作；面板鉴权及反向代理部署边界归[服务端](cvbbmso-server.md#管理控制面板)。

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

pub enum DisconnectReason { Closed, HeartbeatTimeout, Replaced, ServerRestart }

pub enum DomainError {
    NotFound,
    NotEnrolled,
    InvalidInput,
    Conflict,
    Storage { source: Box<dyn std::error::Error + Send + Sync> },
}

impl ExamService {
    /// 打开 SQLite 并运行迁移，使用系统 UTC 时钟。
    pub async fn open(database_url: &str) -> Result<Self, DomainError>;

    /// 打开 SQLite 并运行迁移，使用传入的时钟。
    pub async fn open_with_clock(
        database_url: &str,
        clock: impl Fn() -> time::OffsetDateTime + Send + Sync + 'static,
    ) -> Result<Self, DomainError>;

    /// 获取共享数据库与时钟的管理服务。
    pub fn admin(&self) -> AdminService;

    /// 返回服务端观察连接状态时使用的 UTC 时钟。
    pub fn now(&self) -> time::OffsetDateTime;

    /// 服务端绑定端口后、处理请求前，将上次进程遗留的在线记录转为离线。
    pub async fn recover_online(&self) -> Result<(), DomainError>;

    /// 列出指定 Accoding 用户报名的考试。
    pub async fn list_for(
        &self, oj_user_id: i64,
    ) -> Result<Vec<cvbbmso_protocol::ExamSummary>, DomainError>;

    /// 在同一只读快照中先检查考试存在，再检查报名，并返回考试时长。
    pub async fn check_enrollment(
        &self, exam_id: uuid::Uuid, oj_user_id: i64,
    ) -> Result<time::Duration, DomainError>;

    /// 确认报名前置检查通过，且现有连接仍可接收回执。
    pub async fn check_connected(
        &self, exam_id: uuid::Uuid, oj_user_id: i64,
    ) -> Result<(), DomainError>;

    /// 建立或恢复考试考生记录；返回服务端快照与待下发命令。
    pub async fn connect(
        &self,
        exam_id: uuid::Uuid,
        oj_user_id: i64,
        hello: cvbbmso_protocol::ClientHello,
    ) -> Result<ConnectOutcome, DomainError>;

    /// 处理客户端状态、心跳和可靠事件。
    pub async fn receive(
        &self,
        exam_id: uuid::Uuid,
        oj_user_id: i64,
        message: cvbbmso_protocol::ClientMessage,
    ) -> Result<(), DomainError>;

    /// 在同一事务归档当前会话校验过的终态回执并更新考生状态。
    pub async fn record_command_result(
        &self,
        exam_id: uuid::Uuid,
        oj_user_id: i64,
        command_id: uuid::Uuid,
        name: cvbbmso_protocol::CommandName,
        status: cvbbmso_protocol::CommandStatus,
        error_code: Option<String>,
    ) -> Result<(), DomainError>;

    /// 记录当前有效连接的丢失、心跳超时或替换事件。
    pub async fn disconnect(
        &self,
        exam_id: uuid::Uuid,
        oj_user_id: i64,
        reason: DisconnectReason,
    ) -> Result<(), DomainError>;

    /// 使用原观察时间记录断开，暂时锁竞争时由调用方重试。
    pub async fn disconnect_at(
        &self,
        exam_id: uuid::Uuid,
        oj_user_id: i64,
        reason: DisconnectReason,
        observed_at: time::OffsetDateTime,
    ) -> Result<(), DomainError>;

    /// 根据固定考试时间返回在线考生的候选命令。
    pub async fn due_commands(&self) -> Result<Vec<DirectedCommand>, DomainError>;
}

impl AdminService {
    /// 为 CLI 打开 SQLite 并运行迁移。
    pub async fn open(database_url: &str) -> Result<Self, DomainError>;

    /// 执行管理操作并返回结构化结果；写操作将 actor 写入审计。
    pub async fn execute(
        &self,
        actor: String,
        op: AdminOperation,
    ) -> Result<AdminOutput, DomainError>;
}
```

`ExamService::check_enrollment` 接收正数用户 ID。参数无效返回 `InvalidInput`，考试缺失返回 `NotFound`，考试存在但未报名返回 `NotEnrolled`，数据库读取失败返回 `Storage`。WS 升级后由 `connect` 在写事务内重新验证。

`ExamService::connect` 验证报名资格，并在相同 `(exam_id, oj_user_id)` 上恢复既有状态。服务端根据 `hello.state` 决定是否生成命令。`hello` 上报 `finished` 时，服务端核对已持久化的完成状态和对应的 `end_exam` 终态回执。

`connect` 记录异常的条件：

- `hello.instance_id` 与上次不同，且上次状态为 `recording` 或 `finalizing` 时，写入 `process_restarted`。
- 旧记录在线且未完成时，写入 `connection_lost`（`detail = "replaced"`）和 `connection_restored`。旧记录离线且未完成时，只写入 `connection_restored`。首次连接直接建立记录。

同一连接中的 `status` 只允许执行阶段保持或前进：`ready` 可保持或进入 `recording` / `finalizing`，`recording` 可保持或进入 `finalizing`，`finalizing` 只能保持。`recording → ready`、`finalizing → ready/recording` 返回 `InvalidInput`，事务不修改原状态；server 映射为 `PROTOCOL_ERROR` 并关闭连接。同一执行阶段内仍允许合法的 `recording` 字段变化，包括录屏故障与恢复。`status` 上报 `finished` 不会提交终态；服务端已提交 `finished` 后，后续合法 `status` 均不覆盖终态。这项顺序限制不适用于重连 `hello`；考试进行中丢失本地状态的客户端仍可用 `ready/idle` 重连，由服务端生成新开始命令。

`receive` 持久化可靠事件并推进确认位置。`record_command_result` 校验命令失败码，并拒绝已知事件缺口；成功结束还须确认录像摘要和此前事件已连续落库。回执与考生状态一起提交，服务端在提交成功后发出 `CommandAck`。`disconnect` 在同一事务将当前记录设为离线，并为未完成的考生写入 `connection_lost`。已完成的考生静默转为离线。

`due_commands` 根据服务端时钟生成开始/结束命令；实际网络推送由服务端执行。命令由当前在线考生触发；离线考生的命令在 `connect` 时按状态生成。每个命令使用新的唯一 `command_id`。

`ConnectOutcome.sync` 是 `ServerMessage::Sync`，`commands` 中每个元素都是 `ServerMessage::Command`；`DirectedCommand.command` 同样承载 `Command` 变体。

## 管理操作与输出

`AdminOperation` 与 CLI 命令一一对应；CLI 负责参数解析、CSV 读取和展示，域层负责校验、持久化和审计。CLI 打开数据库时保留在线状态。服务端启动调用 `recover_online` 时，以发现在线遗留记录的 UTC 时间写入 `connection_lost`，`detail` 为 `server_restart`。已完成的在线记录静默转为离线。

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
    Recording(cvbbmso_protocol::RecordingSummary),
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
    pub seq: i64,
    pub event: cvbbmso_protocol::ExamEvent,
    pub received_at: OffsetDateTime,
}

pub struct IncidentRecord {
    pub id: Uuid,
    pub exam_id: Uuid,
    pub oj_user_id: i64,
    pub source: cvbbmso_protocol::admin::IncidentSource,
    pub kind: String,
    pub occurred_at: OffsetDateTime,
    pub detail: Option<String>,
}
```

`actor` 由调用方提供：控制面板使用 `control_panel`，CLI 设计默认使用当前操作系统用户名。审计记录包含整数 `audit_id`、`actor`、序列化的操作类型与参数，以及 `occurred_at`；创建考试的审计包含生成的 `exam_id`。`AdminOperation` 的输入先经过域层校验：`start_at < end_at`，`RecordingPolicy` 参数大于零，`oj_user_id` 为正整数，批量导入去重。报名只能在 `now < start_at` 时撤销，时间检查与删除在同一事务内完成。错误以 `DomainError` 返回，由 server 或 CLI 映射为对外错误。

`RecordingsShow` 在录像状态为 `complete` 时从 `recording_finalized` 事件读取 `RecordingSummary`。尚未 finalize、finalize 失败或没有摘要事件时返回 `NotFound`。已单独上报的分片明细通过 `EventsList` 查询。

## 数据所有权

本 crate 拥有以下业务数据；具体表结构和索引由实现决定：

| 数据 | 含义 |
| --- | --- |
| 考试记录 | `exam_id`、标题、固定起止时间、录像策略 |
| 报名关系 | `(exam_id, oj_user_id)` 是否允许参加 |
| 考生运行状态 | 当前设备/进程、连接状态、执行状态、最后心跳、事件确认位置 |
| 指令记录 | 客户端实际回报的开始/结束命令终态、唯一 `command_id` 和对应失败码 |
| 可靠事件 | 客户端上报的 `seq`、事件内容和接收时间 |
| 异常记录 | 服务端与客户端按发生时间保存的证据 |
| 录像摘要 | 保存在 `recording_finalized` 可靠事件中的分片数和合并文件 SHA-256 |
| 管理审计 | actor、操作、目标、时间 |

`crates/cvbbmso-domain/migration/` 维护 SQLite 迁移。首次打开空库时，一条初始迁移创建七张业务表，录像摘要存放在可靠事件表中。

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
- 同一 WebSocket 会话由 server 跟踪已发出的命令与结果，按命令类型去重调度。重连时按 `hello.state` 和考试阶段重新判断是否下发命令。
- 终态归档由客户端的实际回执触发。若结果或 ACK 丢失，重连后按 `hello.state` 判断是否需要重新执行，需回复时使用新下发的命令 ID。

## 事件、异常与录像摘要

客户端 `event.seq` 按 `(exam_id, oj_user_id)` 连续递增。`receive` 以联合标识和 `seq` 去重，持久化后推进最大连续确认序号 `ack_event_through`；确认位置在下一次 `sync` 中返回。重复 `seq` 内容相同则忽略，内容不同按协议错误处理。

`Recorder::open` 恢复的分片可能以原 `seq` 重报；服务端按 `seq` 幂等，并可按 `(exam_id, oj_user_id, index)` 做辅助去重。`recording_finalized` 保存正数分片数量和合并录像的 SHA-256，两字段均相同的摘要可重报，任一字段不同则拒绝。`incident` 保存为异常记录。

异常记录按发生时间保存，每条记录包含 `occurred_at`：

| 来源 | `kind` | 含义 |
| --- | --- | --- |
| server | `connection_lost` | 当前连接关闭、心跳超时或被新连接替换；`detail` 说明原因 |
| server | `connection_restored` | 此前丢失的考生连接重新建立成功 |
| server | `process_restarted` | `instance_id` 变化，表明客户端进程重启 |
| client | `recording_failed` | 录屏启动、采集、编码、写盘、合并或哈希失败 |
| client | `recording_recovered` | 录屏恢复；作为独立证据保存 |
| client | `navigation_denied` | 受控浏览器拒绝了导航请求 |

`connection_lost` 与 `connection_restored` 是两条独立记录，CLI 按 `occurred_at`、`incident_id` 排序展示。`source` 由异常 `kind` 推导。`detail` 为可选字符串，例如 `heartbeat_timeout`、`replaced`、`server_restart`、`capture stream stopped`。

## 错误映射

`DomainError` 是领域层对外错误类别，server 和 CLI 负责转换为各自协议：

| 错误 | 典型场景 | 对外映射 |
| --- | --- | --- |
| `NotFound` | 考试、考生、异常或录像不存在 | `EXAM_NOT_FOUND` / CLI 错误 |
| `NotEnrolled` | 访问者不在该考试的报名名单中 | `NOT_ENROLLED` |
| `InvalidInput` | 时间、策略、CSV 或参数非法 | `INVALID_REQUEST` |
| `Conflict` | 重复报名、开考后撤销报名、命令结果与当前命令状态不匹配 | 命令结果用 `COMMAND_STATE_CONFLICT`，其他用 `INVALID_REQUEST` |
| `Storage` | 数据库或持久化失败 | `INTERNAL_ERROR` |

客户端 `command_result` 到达时，server 校验 `command_id` 属于当前 WebSocket 会话。会话内重复的相同结果幂等返回同 ID 的 `CommandAck`，未知命令或冲突结果使用 `COMMAND_STATE_CONFLICT`。
