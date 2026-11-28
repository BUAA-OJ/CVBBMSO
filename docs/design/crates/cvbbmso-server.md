# cvbbmso-server — Axum HTTP / WebSocket 服务

本 crate 提供监考服务端可执行程序。Axum 负责 `/api` 路由、Cookie 认证和 WebSocket 升级；`cvbbmso-domain` 负责考试规则与数据持久化；`cvbbmso-accoding` 负责向 Accoding 学生站复验身份。

本文档定义 HTTP/WS 端点、错误映射、连接职责和部署边界；路由 handler、WS Hub、计时器等内部实现保持 crate 私有。

## Rust 入口

```rust
// 本 crate 为二进制程序，接口保持 crate 私有。
struct ServerConfig {
    listen_addr: std::net::SocketAddr,
    database_url: String,
    heartbeat_timeout_secs: u64,
}

enum ServerError {
    Config,
    Database,
    Io,
}

async fn serve(config: ServerConfig, admin_token: Option<admin::Token>) -> Result<(), ServerError>;
```

`serve` 先绑定监听地址，再打开数据库并恢复遗留在线记录。恢复成功后才启动 HTTP Router、WebSocket Hub（含统一 `DatabaseWriter` 组提交与心跳回写缓存）和独立考试调度任务，并在收到停机信号后按序排空在途操作、回写心跳与断开记录后关闭数据库。`heartbeat_timeout_secs` 默认 25 秒，组提交窗口、批次上限、全局与单连接背压阈值及认证并发上限均支持通过 CLI 参数或环境变量调整。服务端按单实例部署，TLS 由反向代理终止；Rust 进程监听内网 HTTP/WS。客户端与服务端同版本部署。

## HTTP API

| 方法 | 路径 | 认证 | 成功响应 | 语义 |
| --- | --- | --- | --- | --- |
| `GET` | `/api/health` | 无 | `200 {"status":"ok"}` | 服务存活检查 |
| `GET` | `/api/exams` | Accoding Cookie | `200 ExamListResponse` | 返回当前考生报名的考试 |
| `GET` | `/api/exams/{exam_id}/ws` | Accoding Cookie | `101 WebSocket Upgrade` | 连接或重连指定考试 |

`exam_id` 是 UUID。客户端在 `GET /api/exams` 和 WS Upgrade 时显式发送 `Cookie: connect.sid=...`，服务器调用 Accoding `/api/users/me` 复验，得到 `oj_user_id`，再按 `(exam_id, oj_user_id)` 检查报名及运行记录。业务标识为 `(exam_id, oj_user_id)`。

错误响应为：

```json
{"error":{"code":"ACCODING_AUTH_EXPIRED","message":"Accoding login required"}}
```

| HTTP 状态 | `error.code` | 语义 |
| --- | --- | --- |
| `400` | `INVALID_REQUEST` | 参数、路径、请求格式或 Cookie 无效，包括重复的 `connect.sid` |
| `401` | `ACCODING_AUTH_EXPIRED` | 会话 Cookie 缺失、为空或已失效，或 Accoding 返回匿名状态 |
| `403` | `NOT_ENROLLED` | 访问者不在该考试的报名名单中 |
| `404` | `EXAM_NOT_FOUND` | 指定的考试不存在 |
| `500` | `INTERNAL_ERROR` | 内部错误 |
| `503` | `ACCODING_UNAVAILABLE` | Accoding 身份服务暂时不可用 |

错误码本身由 [cvbbmso-protocol](cvbbmso-protocol.md) 定义；本表维护 HTTP 状态码映射。同一考生从新设备连接时，服务端按“替换旧连接”处理。

HTTP 与 WebSocket 拒绝日志同时记录协议错误码和经过脱敏的失败原因。日志仅记录错误类别和安全诊断字段，例如上游 HTTP 状态与 SQLite 错误号。客户端继续收到协议错误码和通用文案。

## 管理控制面板

`/admin` 提供同源页面，不依赖额外前端服务。管理员身份与考生 Cookie 独立；未设置 `CVBBMSO_ADMIN_TOKEN` 时不挂载页面和管理 API。令牌由服务端进程环境注入，格式为 32 字节随机值的小写十六进制表示；格式无效时拒绝启动。浏览器仅在当前页面内存保存令牌，显式发送 `Authorization: Bearer <token>`，不使用 Cookie、URL 参数或浏览器持久存储。退出或刷新后需重新输入。管理响应禁止缓存。

生产环境应经 HTTPS 反向代理访问，并将 `/admin`、`/admin/` 下的资源和 `/api/admin/` 限制在管理网、VPN 或受信任代理之后；不要记录 `Authorization`。首版为单个共享管理员凭据，不区分个人和角色；写入领域审计的操作者为 `control_panel`。通过更新环境变量并重启服务轮换令牌。

本地试用（PowerShell 7，在仓库根目录运行）：

```powershell
$env:CVBBMSO_ADMIN_TOKEN = [Convert]::ToHexString([Security.Cryptography.RandomNumberGenerator]::GetBytes(32)).ToLowerInvariant()
cargo run --locked -p cvbbmso-server -- --listen-addr 127.0.0.1:8080 --database-url 'sqlite://exams.sqlite?mode=rwc'
```

在浏览器打开 `http://127.0.0.1:8080/admin`，输入刚生成的令牌。`/admin/` 会重定向到 `/admin`。仅本地回环可用明文 HTTP；不要在网络上使用这套本地示例配置。

| 方法 | `/api/admin` 下的路径 | 成功响应 |
| --- | --- | --- |
| `GET` | `/exams` | `200 ExamSummary[]` |
| `POST` | `/exams` | `201 CreatedExam`，请求为 `CreateExam` |
| `GET` | `/exams/{exam_id}` | `200 ExamDetail` |
| `GET` | `/exams/{exam_id}/roster` | `200 RosterEntry[]`，按用户 ID 升序 |
| `POST` | `/exams/{exam_id}/participants` | `200 ImportedParticipants`，请求为 `ImportParticipants` |
| `DELETE` | `/exams/{exam_id}/participants/{oj_user_id}` | `204`，只允许在开考前撤销 |
| `GET` | `/exams/{exam_id}/incidents` | `200 Incident[]`，支持 `?oj_user_id=...` |
| `GET` | `/exams/{exam_id}/students/{oj_user_id}/events` | `200 EventPage`，按序号降序，每页最多 20 条 |
| `GET` | `/exams/{exam_id}/students/{oj_user_id}/recording` | `200 RecordingSummary`，无摘要时返回 `404` |

管理载荷由 `cvbbmso_protocol::admin` 定义。创建考试的标题去掉首尾空白后为 1–120 个字符，起止时间使用带时区的 RFC 3339，`policy` 使用现有 `RecordingPolicy`。报名导入支持逐个或批量提交，单次为 1–1,000 个正 `i64` 用户 ID，使用 `{"oj_user_ids":["12345"]}`；重复报名忽略，响应 `count` 仅统计新增。管理 JSON 请求体上限为 64 KiB。JSON 中的考生 ID 和事件序号用十进制字符串传输，避免浏览器损失 64 位整数精度。

名单合并当前报名和运行记录；`runtime: null` 代表从未连接，`runtime.connected: false` 代表有历史记录但当前离线，`enrolled: false` 代表报名已撤销但仍有运行记录。面板可见时每 5 秒轮询考生状态。可靠事件的最新页同样每 5 秒刷新，仅在浏览器页面可见且打开该考生的事件列表时轮询；历史页不自动刷新。异常和录像摘要仍按需查询，不提供录像上传、下载或播放。

事件响应为 `{"items":[...],"next_before":"21"}`，没有更早事件时 `next_before` 为 `null`。默认取最新页，使用 `?before=21` 取严格小于该事件序号的下一页。游标是正 `i64` 序号，浏览器按字符串传递。

管理业务错误返回 `{"code":"..."}`：`401 ADMIN_AUTH_REQUIRED`、`400 INVALID_INPUT`、`404 NOT_FOUND`、`409 CONFLICT`、`413 BODY_TOO_LARGE`、`415 JSON_REQUIRED`、`503 STORAGE_BUSY`、`500 INTERNAL_ERROR`。路径或查询参数无法解析时由 Axum 拒绝；考生端错误格式保持不变。

## WebSocket 处理

协议端点为 `wss://<监考服务端>/api/exams/{exam_id}/ws`。服务端在升级前从握手 Cookie 确认考生身份，失败时按上表返回同格式的 HTTP 错误。升级成功后，客户端先发送 `hello`，服务端调用 `ExamService::connect(exam_id, oj_user_id, hello)` 返回 `sync` 和待下发的新命令。后续 `heartbeat`、`status`、`event` 交给 `ExamService::receive`，会话校验 `command_result` 后交给 `ExamService::record_command_result`。

所有考试的入站 WebSocket 单帧载荷和重组后整条消息均以 64 KiB（65,536 字节）为上限，超限时返回 `PROTOCOL_ERROR` 并关闭连接。每条连接最多有一条应用消息在共享队列中等待或处理。所有消息字段见 [cvbbmso-protocol](cvbbmso-protocol.md)。

命令与确认规则：

- 服务端根据 `hello.state` 和考试阶段生成命令；每个命令生成新的唯一 `command_id`。
- 调度器向在线连接推送 `due_commands` 返回的命令；离线考生的命令在重连时由 `connect` 生成。
- 事件确认通过 `sync.ack_event_through` 返回；服务端持久化连续事件并在重连时通过 `sync` 返回确认位置。
- 收到 `command_result` 后，服务端在同一事务归档终态并更新考生状态，提交后回复 `CommandAck { command_id }`。会话内重报相同结果会收到相同 ACK；命令 ID 必须由当前会话发出。非法状态组合或未知命令按 `COMMAND_STATE_CONFLICT` / `PROTOCOL_ERROR` 处理，并在协议错误时关闭连接。
- `CommandAck` 丢失后，客户端可用 `finalizing` 重连。服务端保留已提交的完成状态，并下发新的 `end_exam` 让客户端重报终态。
- 考前撤销报名时，Hub 对目标考生建立写入屏障，暂停心跳续期及新业务准入并等待在途操作完成，在事务内删除报名、将运行记录设为离线并合并最新心跳后，立即向旧连接发送 `NOT_ENROLLED` 并关闭连接。
- 调度器在独立任务中每秒通过只读连接池查询候选命令并交由 Hub 校验连接代次、当前状态及命令槽位后下发；若存在影响命令资格的在途写入，待提交后重新评估。
- `status` 用于状态对齐；同一连接内执行阶段倒退由 domain 拒绝，映射为 `PROTOCOL_ERROR` 并关闭连接，原状态保留。结束状态由 `end_exam` 的 `command_result` 判定。重连仍按 `hello.state` 重新生成命令。

同一 `(exam_id, oj_user_id)` 同时保留一条有效 WebSocket。服务端维护“当前连接”：

- 新连接到达时等待旧连接已开始的在途事务及待恢复失联记录结算，再提交新连接并替换旧连接，随后关闭旧 socket；旧 socket 之后触发的关闭事件按替换结果处理。
- 服务端对处于 `finished` 的连接按正常完成处理；其余当前连接的关闭或心跳超时调用 `Disconnect` 提交离线与 `connection_lost`。
- 收到 `end_exam` 的 `command_result` 后，服务端在组提交事务归档终态并将执行状态置为 `finished`，提交成功后回复 `CommandAck`；该连接随后关闭，属于正常完成。
- 被替换的旧连接以 `Replaced` 原因记录 `connection_lost`，`detail` 为 `replaced`。
- 合法应用层心跳通过轻量路径即时续期并写入内存 `HeartbeatCache`，约每秒由 `DatabaseWriter` 按 `(exam_id, oj_user_id, connection_id, connected)` 条件批量落盘；连接断开或撤销时将最新有效心跳并入对应生命周期事务。
- WebSocket 读取与可靠业务写入解耦，限制单连接待处理可靠消息数及全局待写队列数量与载荷字节数；触发背压时暂停读取且不误判心跳超时。HTTP/WS 认证阶段对上游 Accoding 请求限制并发并设等待超时。

## 连接丢失与恢复

```mermaid
flowchart TD
    A[当前有效 WebSocket] --> B{连接状态}
    B -->|持续收到心跳| C[更新 last_seen_at]
    C --> B
    B -->|连接关闭或心跳超时且未 finished| D[ExamService::disconnect]
    D --> E[记录 connection_lost]
    E --> F{考生重连?}
    F -->|是| G[记录 connection_restored]
    G --> H{instance_id 变化且上次为 recording/finalizing?}
    H -->|是| I[记录 process_restarted]
    H -->|否| J[视为同一进程重连]
    I --> K[发送 sync 与按状态生成的新命令]
    J --> K
    F -->|否| L[保持离线，记录可查询]
```

- 客户端异常退出或网络中断时，服务器观测到 WebSocket 关闭或心跳超时，记录 `connection_lost`。客户端退出后保持离线时，CLI 仍能查到该记录。
- 重连成功时，根据旧记录的在线状态和执行阶段记录失联与恢复。若新 `hello.instance_id` 与上次不同且上次状态为 `recording/finalizing`，再记录 `process_restarted`。三条记录独立保存，CLI 按 `occurred_at`、`incident_id` 排序展示。
- 服务端启动时恢复遗留在线记录，给未完成的考生记 `connection_lost`，`detail` 为 `server_restart`。
- SQLite 忙锁时，同一可靠消息由对应考生的有序队列最多尝试写入四轮，轮间等待 50 毫秒（不同考生互不阻塞；只读 `CommandAck` 重放经只读连接池即时确认）。到达当前心跳截止时间或观察到传输关闭后，停止后续重试。心跳到期前耗尽尝试次数时，发送既有的 `INTERNAL_ERROR` 并关闭连接。
- 持续性 SQLite 存储故障（如磁盘满或 I/O 异常）期间，服务端记录告警并暂停新连接准入，待后续写入恢复后自动解除；未确认可靠事件由客户端本地 Journal 保留。
- 已开始的事务先结算，再处理断开或接纳替代连接。断开写入失败时保留观察时间并重试，成功后该考生才能重新接入。
- 正常完成考试时连接静默关闭，按正常完成处理。

## 模块协作

`cvbbmso-server` 拥有 HTTP/WS 传输及连接生命周期。考生身份校验委托 `cvbbmso-accoding`，考试授权、时序、ACK、异常保存委托 `cvbbmso-domain`。控制面板通过服务端管理路由经 Hub 协调调用 `AdminService`，不直接访问数据库。
