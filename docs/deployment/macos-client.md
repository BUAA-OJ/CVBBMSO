# macOS 客户端部署

正式版本以 DMG 分发，支持 macOS 14 及以上。打开映像，将 `CVBBMSO.app` 拖到同一窗口的「应用程序」入口，退出映像，再从 `/Applications` 双击应用。使用系统的应用安装权限完成复制。客户端只接受标准 `/Applications/CVBBMSO.app` 位置及经过验证的签名。直接从映像、App Translocation 或用户安装目录启动时无法注册特权服务。

## 授权与网络准入

客户端在普通会话完成前置检查，再通过 `SMAppService` 注册随应用签名的 LaunchDaemon。系统需要批准时，在「系统设置 → 通用 → 登录项与扩展」允许 CVBBMSO 的后台服务，回客户端重试。服务核对调用方的签名，使用同一版本内置的 Accoding 和监考 HTTPS 来源。守卫成功安装并通过内核规则检查后，客户端才能继续准入。启动前应退出旧客户端或另一场会话。

`edu.buaa.accoding.cvbbmso.egress` 是签名标识、launchd 标签和 XPC Mach 服务名。plist 位于 `Contents/Library/LaunchDaemons`，执行文件位于 `/Applications/CVBBMSO.app/Contents/MacOS/cvbbmso-egress-service`。launchd 按 Team ID、签名标识和 Developer ID 类型执行 SpawnConstraint，签名校验也覆盖 App 封存和 helper。系统授权后，daemon 在 root 保护的 `/Library/Application Support/com.cvbbmso` 内保存验签且与运行中代码匹配的恢复副本。watchdog 和恢复任务从这份副本启动，方便应用更新、崩溃或 XPC 断开后的清理。[Apple launchd 启动约束](https://developer.apple.com/documentation/security/applying-launch-environment-and-library-constraints)

### PF 与恢复

应用优先使用已加载且符合 Network 契约的 `com.cvbbmso.egress` 入口。自动激活限于已匹配的 macOS 原始规则和当前未启用的 PF。服务检查启用引用、规则、表和状态。对原厂启动规则，Apple 的 AirDrop 和应用防火墙调度入口可保留，实际子规则必须为空。对空内核规则集，只加入 CVBBMSO 入口。两种情况均先经 `pfctl -n` 解析，再只加载过滤段。系统配置文件继续由宿主维护。正常退出时，服务先清空自己的规则和表，释放自己的 PF 引用，再尝试恢复原过滤段。异常退出还由看门狗及租约负责清理。每次更新和关停由持有守卫的 XPC 会话独占。

发现自定义 PF、规则变化或未知参考状态时，准入暂停。已有宿主 PF 管理流程的机器，应由管理员按[旧部署手册](../../deploy/macos-egress/README.md)配置兼容入口，保留宿主策略。PF 与其它管理程序之间没有公开的原子比较并替换接口，自动激活前后都检查快照。管理员应在宿主策略更新完成后再启动客户端。

如果其他服务启用 PF 或规则在会话期间变化，清理先处理 CVBBMSO 的专用规则与启用引用。无法证明原规则可安全恢复时保留退出后的空入口以及 `/Library/Application Support/com.cvbbmso/managed-pf.v1`。管理员可检查恢复记录和宿主管理规则，按宿主流程恢复。退出客户端并确认守卫已清理后，再移除应用或在系统设置中停用后台服务。旧版管理员配置的固定 helper 和 PF 入口继续适用。

## 构建和核验 DMG

使用原生 macOS shell、Rust 工具链和 macOS SDK。在对应提交中运行脚本，生成本机架构的锁定 Release 构建，脚本输出 DMG 路径和 SHA-256。

```sh
./deploy/macos-egress/build-dmg.sh candidate
./deploy/macos-egress/verify-dmg.sh "$IMAGE" candidate
```

`candidate` 生成 `-unsigned-candidate.dmg`，用于检查映像内容。候选不包含生产 Team 或监考配置，系统特权服务拒绝运行。映像只读挂载进行检查，核对拖放入口、App、plist、Mach 服务名及安装后解析的执行路径。

正式分发改用 `build-dmg.sh release`。发布环境从可信配置提供 `CVBBMSO_MACOS_TEAM_ID`、`CVBBMSO_MACOS_SIGN_IDENTITY`、`CVBBMSO_MACOS_NOTARY_PROFILE` 和 `CVBBMSO_SERVER_ORIGIN`。脚本先用 Developer ID Application 和 Hardened Runtime 签 helper，再封存 App，最后签 DMG、提交 Apple 公证并装订票据。密钥和 notary 凭据留在发布密钥链，监考来源由发布环境注入。[Apple 分发打包文档](https://developer.apple.com/documentation/xcode/packaging-mac-software-for-distribution)

开发检查使用 `test-authorization.sh`、Network 测试替身和 `verify-dmg.sh`。签名授权、Finder 拖入后的实际系统授权路径、实际守卫和宿主流量仍须在隔离环境使用正式签名版本检查。
