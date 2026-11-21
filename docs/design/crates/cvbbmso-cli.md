# cvbbmso-cli — 服务端本机管理 CLI

本 crate 提供服务器本机命令行工具，使用 Clap 解析命令，调用 `cvbbmso-domain::AdminService::execute(actor, op)` 完成业务操作。CLI 负责参数解析、CSV 读取和结果展示；考试规则、调度、数据修改、异常持久化与审计由领域层负责。CLI 通过进程内调用领域层，与 server 共用同一套管理接口。

本文档定义命令、参数、输出约定和退出码；具体参数解析和表格渲染属于实现细节。

## Rust 入口与接口

```rust
// 二进制 crate 内部类型。
struct Cli { /* Clap 参数树 */ }

pub enum CliError {
    InvalidArgs,
    Domain(cvbbmso_domain::DomainError),
    Io,
}

impl Cli {
    /// 解析参数，映射为 AdminOperation 并输出 AdminOutput。
    async fn run(self) -> Result<(), CliError>;
}
```

管理员以 `exam_id` 定位考试，以 Accoding 的 `oj_user_id` 定位考生。所有命令接受可选 `--actor <name>`；写操作将 actor 写入审计，缺省值为当前操作系统用户名。审计记录由领域层保存，供合规审查。成功返回退出码 `0`；参数错误返回 `2`；领域层或 I/O 错误返回 `1`。

## 命令规范

```bash
cvbbmso-cli exams create \
  --title "程序设计期末考试" \
  --start 2026-12-20T09:00:00+08:00 \
  --end 2026-12-20T12:00:00+08:00 \
  --segment-duration 30 --fps 15 \
  --actor admin
cvbbmso-cli exams list
cvbbmso-cli exams show --exam <exam-id>

cvbbmso-cli participants add --exam <exam-id> --oj-user-id 12345
cvbbmso-cli participants remove --exam <exam-id> --oj-user-id 12345
cvbbmso-cli participants import --exam <exam-id> --csv students.csv
cvbbmso-cli participants list --exam <exam-id>

cvbbmso-cli students status --exam <exam-id>
cvbbmso-cli students show --exam <exam-id> --oj-user-id 12345
cvbbmso-cli events list --exam <exam-id> --oj-user-id 12345
cvbbmso-cli incidents list --exam <exam-id>
cvbbmso-cli incidents list --exam <exam-id> --oj-user-id 12345
cvbbmso-cli incidents list --exam <exam-id> --kind connection_lost \
  --since 2026-12-20T09:00:00+08:00 --until 2026-12-20T12:00:00+08:00
cvbbmso-cli incidents show --id <incident-id>
cvbbmso-cli recordings show --exam <exam-id> --oj-user-id 12345
```

- `--start`、`--end`、`--since`、`--until` 使用带时区的 RFC 3339；服务端按绝对时刻比较。
- `--oj-user-id` 使用 Accoding `/api/users/me` 返回的正整数 `id`。
- `--segment-duration` 单位为秒，和 `--fps` 一样必须大于零。
- `participants import` 的 CSV 读取 `oj_user_id` 列；CLI 解析后以 `Vec<i64>` 传给领域层，重复 ID 由领域层去重。
- 所有查询命令支持 `--format text|json`，默认 `text`。`--format` 是全局参数，可放在子命令前后。
- `participants remove` 只能在考试开始前执行。
- `incidents list` 支持 `--exam`、`--oj-user-id`、`--kind`、`--since`、`--until` 组合筛选，结果按 `occurred_at`、`incident_id` 升序排列。

## AdminOperation 映射

| CLI 命令 | `AdminOperation` | `AdminOutput` |
| --- | --- | --- |
| `exams create` | `ExamsCreate` | `ExamCreated` |
| `exams list` | `ExamsList` | `Exams` |
| `exams show` | `ExamsShow` | `ExamDetail` |
| `participants add` | `ParticipantsAdd` | `ParticipantAdded` |
| `participants remove` | `ParticipantsRemove` | `ParticipantRemoved` |
| `participants import` | `ParticipantsImport` | `ParticipantsImported` |
| `participants list` | `ParticipantsList` | `Participants` |
| `students status` | `StudentsStatus` | `Students` |
| `students show` | `StudentsShow` | `Student` |
| `events list` | `EventsList` | `Events` |
| `incidents list` | `IncidentsList` | `Incidents` |
| `incidents show` | `IncidentsShow` | `Incident` |
| `recordings show` | `RecordingsShow` | `Recording` |

## 查询结果约定

| 命令 | 返回数据 | 字段解释 |
| --- | --- | --- |
| `exams show` | `ExamSummary` + `RecordingPolicy` | 考试固定时间、当前 `phase` 与录像参数 |
| `students status` | `StudentRuntime[]` | 每人连接状态、当前执行阶段、最近心跳 |
| `students show` | `StudentRuntime` | 指定 `(exam_id, oj_user_id)` 的最新状态 |
| `events list` | `EventRecord[]` | 客户端上报的 `seq`、`event`、`received_at` |
| `incidents list` | `IncidentRecord[]` | 服务端及客户端异常的发生时间与上下文 |
| `incidents show` | `IncidentRecord` | 单个异常的完整信息 |
| `recordings show` | `RecordingSummary` | 分片数 `segment_count` 和合并录像 SHA-256 `merged_sha256`；没有摘要时返回 `NotFound` |

通过 `events list` 查看已上报的 `segment_finalized` 分片明细。

`StudentRuntime` 的核心字段为 `exam_id: uuid`、`oj_user_id: i64`、`connected: bool`、`execution: ExecutionState`、`last_seen_at: datetime?`；`ExecutionState` 为 `ready` / `recording` / `finalizing` / `finished`。`EventRecord` 为 `exam_id`、`oj_user_id`、`seq`、`event`、`received_at`，其中 `event` 遵循 [cvbbmso-protocol](cvbbmso-protocol.md)。`IncidentRecord` 为 `id`、`exam_id`、`oj_user_id`、`source`、`kind`、`occurred_at`、`detail`，字段定义见 [cvbbmso-domain](cvbbmso-domain.md)。`detail` 是可选字符串，用于记录 `heartbeat_timeout`、`replaced`、`capture stream stopped` 等上下文。

## 示例

```bash
cvbbmso-cli incidents list --exam <exam-id> --kind connection_lost --format json
```

```json
[
  {
    "id":"00000000-0000-0000-0000-000000000099",
    "exam_id":"00000000-0000-0000-0000-000000000010",
    "oj_user_id":12345,
    "source":"server",
    "kind":"connection_lost",
    "occurred_at":"2026-12-20T02:10:00Z",
    "detail":"heartbeat_timeout"
  }
]
```

`incidents list` 的 `--kind` 支持 `connection_lost`、`connection_restored`、`process_restarted`、`recording_failed`、`recording_recovered`、`navigation_denied`。`connection_lost` 与 `connection_restored` 是两条独立记录；客户端保持离线时，CLI 仍能查询到服务端记录的 `connection_lost`。
