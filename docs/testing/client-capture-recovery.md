# 客户端采集恢复与校园网认证验收

## 自动回归

在客户端独立工作区运行，不启动管理服务，不修改系统网络或电源配置：

```powershell
cargo fmt --all --check
cargo clippy --locked -p cvbbmso-client -p cvbbmso-exam -p cvbbmso-recorder -p cvbbmso-network --all-targets --all-features -- -D warnings
cargo test --locked -p cvbbmso-client -p cvbbmso-exam -p cvbbmso-recorder -p cvbbmso-network --all-features
cargo test --locked -p cvbbmso-client --bin cvbbmso-client -- --native-smoke
cargo test --locked -p cvbbmso-client --bin cvbbmso-client -- --native-exam-smoke
```

Recorder 假帧源覆盖采集故障前尾段封存、重复故障后索引连续、编码故障不发布可疑尾段、首帧超时以及首帧被拒绝时不报告恢复。网络假解析器覆盖认证主机仅放行 TCP 443、DNS 失败保留旧地址和更换地址的宽限期。

桌面冒烟使用回环页面与合成会话，覆盖准入前认证入口的权限限制、监考不可达时打开认证窗口、入场后的认证入口、跨源导航与远程命令拒绝，以及显著的断线/录像故障提示。该测试不采集真实桌面，也不安装系统守卫。

## Windows 实机复核

使用经授权的独立测试考试与可录制桌面，记录候选程序 SHA-256、PID、创建时间、系统版本、分片时长和帧率。不要向并行开发中的 Server 工作区或正在进行的考试注入故障。

| 场景 | 预期结果与证据 |
| --- | --- |
| 正常录制超过原自动息屏时间 | 屏幕不会因空闲自动关闭，录制期间存在显示器与系统唤醒请求；结束后释放。可以用只读的 `powercfg /requests` 辅助核对。 |
| 人工息屏或锁屏后恢复 | 如发生采集失效，报告含操作名和 HRESULT；故障前已编码的尾段能够封存。恢复事件不早于第一帧被接受。 |
| 重复三次息屏/恢复 | 已封存分片不被覆盖，索引和可靠事件顺序连续；逐次检查新故障原因，避免把一条恢复事件当成后续稳定录制的证明。 |
| 断开监考连接后恢复 | 主窗口出现独立警示；录制继续。恢复连接后警示清除，最终录像失败的警示仍保留。 |
| 校园 Wi-Fi 重新认证 | 在认证窗口打开指定 `gw.buaa.edu.cn` 入口；认证后监考自动重连。若需要初始准入重试，用户点击检查按钮。记录额外跳转/资源地址，未核验的站点保持拒绝。 |
| 后台守卫复查 | 正常录制时持续观察超过两个复查周期，没有 PowerShell/w32tm 控制台闪窗。疑似 UAC 窗口另行核对身份。 |
| 结束与退出 | 校验分片元信息、服务端事件和文件 SHA-256；完整解码最终 MP4。结束确认前仍不能主动退出，确认后可以正常关闭。 |

对照每段真实 UTC 起止时间和采集故障事件，分别统计墙钟缺口与媒体时长。设备实际休眠或桌面不可读期间的画面无法补回，不使用重复静帧或伪造时间戳掩盖缺口。附上具体 HRESULT、恢复首帧和随后分片的证据，才能判断亮屏后是否稳定恢复。

macOS 编译由 CI 验证；录屏、PF 守卫和认证页面的实机运行需要在对应平台另行验收。
