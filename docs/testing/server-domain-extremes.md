# Server / Domain 极端场景回归

测试入口为 `crates/cvbbmso-server/src/api/tests/extremes.rs`，复用 server 的临时 SQLite 和考试夹具，实际调用 Hub、`ExamService` 与 `AdminService`。状态倒退和事件缺口场景走内存 HTTP/WebSocket；其余场景使用共享 `TestPeer` 的 Hub 消息通道，以便确定消息处理顺序并检查没有输出。身份验证使用测试替身，不运行真实客户端、原生录屏或 Accoding 网络请求。

这组测试验证状态倒退拒绝及当前恢复行为。结束后以 `ready` 重连和永久事件缺口仍包含恢复限制；这两项通过表示复现及对照断言成立，不表示限制已修复。未来修改恢复策略时，需要同步调整相应断言及设计契约。

## 执行

在仓库根目录运行整组，共 5 个测试函数，包含 9 个场景分支：

```sh
cargo test --locked -p cvbbmso-server --all-features api::tests::extremes::
```

单项执行时，将过滤条件替换为下表的函数名，并核对实际执行数量为 1。domain/server 的完整回归及静态检查：

```sh
cargo fmt --all --check
cargo test --locked -p cvbbmso-domain -p cvbbmso-server --all-features
cargo clippy --locked -p cvbbmso-domain -p cvbbmso-server --all-targets --all-features -- -D warnings
```

## 场景与断言

| 项目 | 测试函数 | 构造与当前结果 |
| --- | --- | --- |
| 同一会话状态倒退 | `same_session_status_regression_is_rejected_and_reconnect_restores_recording` | 分别从 `recording` 上报 `ready`、从 `finalizing` 上报 `ready` 或 `recording`。收到 `PROTOCOL_ERROR` 并关闭连接，原执行阶段与已保存事件保持。考试进行中以 `ready/idle` 重连后取得新开始命令，重复成功回执幂等 ACK，恢复 `recording`，到点仍收到结束命令。 |
| 结束后丢失本地状态 | `runtime_loss_after_end_stays_ready_until_explicit_finalizing_reconnect` | 先成功开考并保存事件，再断线，直到 `end_at` 才以 `ready/idle` 重连。服务端保留 `ack_event_through=1` 和原事件，记录失联、恢复及进程重启；重复心跳和调度均不产生开始或结束命令，状态停在 `ready`。对照连接主动上报 `finalizing/failed` 后，可取得结束命令并以失败完成握手；这不代表客户端具备自动恢复能力。 |
| 永久事件缺口 | `permanent_event_gap_rejects_failed_end_across_reconnects` | 保存 `seq=1,3`，不提供 `seq=2`。首次和随后两次重连的失败结束回执均收到 `COMMAND_STATE_CONFLICT` 并关闭连接；新结束命令 ID 不同，确认位置始终为 1，状态保持 `finalizing`，原事件不变。对照补齐 `seq=2` 后，失败结束结果获得 ACK，并可用 `finished/failed` 重连，确认位置为 3。 |
| 替换、超时和结束回执交错 | `replacement_timeout_and_end_result_interleavings_preserve_current_session` | 枚举回执先提交、替换先发生、旧连接先超时三种顺序，在旧连接心跳截止点附近替换。旧连接迟到的超时、关闭、事件和结束结果不能影响当前连接；重复调度不重复下发结束命令，新连接重复回执获得同 ID ACK。已完成的旧连接不产生异常失联；其余顺序各保存一次 lost/restored/restarted，失联原因分别为 `replaced` 或 `heartbeat_timeout`，正常完成后的关闭不增加异常。 |
| 重试期间其他考生的进展 | `sqlite_retry_allows_other_student_read_only_ack_then_recovers_writes` | 独立 SQLite 事务持有写锁，等待考生 A 的事件明确进入重试。锁仍被持有时，考生 B 重报已归档的开始结果，通过只读校验获得 ACK；A 的写入仍未完成。释放锁后，A 的事件成功保存一次，B 可写事件及心跳，两人的数据独立、连接保持在线，无失联记录。 |

## 验收解释

- **状态倒退**：`Status` 在合法组合校验之外拒绝同一连接执行阶段倒退；命令仍按会话去重，重连恢复使用新命令 ID。domain 的 `status_updates_follow_execution_transition_matrix` 验证 48 个非终态来源的状态组合，`status_updates_cannot_replace_a_committed_terminal_state` 验证 16 个已提交终态上的状态组合，涵盖正常前进、同阶段故障恢复和终态保护。
- **结束后 `ready`**：当前 domain 命令表规定 `ended + ready/idle` 无命令。测试明确验证这一行为和无法自动收尾的后果，不擅自引入恢复规则。
- **永久缺口**：当前契约拒绝存在已知事件缺口的结束结果，包括失败结果；反复重连不能弥补永久丢失的事件。补齐缺口的对照用于证明拒绝原因。若要支持无法补齐时的管理收尾，需要另行定义契约。
- **竞争顺序**：Hub 按队列串行处理请求，测试通过处理完成确认、barrier 和虚拟时间指定交错顺序，不靠随机并发或真实等待碰撞。
- **锁竞争**：SQLite 写锁影响整个数据库，不能据此要求考生 B 在锁内完成新写入。此处验证重试期间仍能处理其他考生的只读 ACK，以及解锁后的写入恢复。它不证明单次数据库等待期间 Hub 完全没有阻塞，也不是延迟或容量验收。超时只用于防止测试挂起，不作为性能指标。

验收报告应记录执行版本、工作树差异、命令、实际测试数量与结果，并单列上述恢复限制。本次修改基于 `ebc7775148c04a627d1337f774330af1d1a14fa5`；后续验收以当时实际提交版本为准。
