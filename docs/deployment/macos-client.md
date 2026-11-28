# macOS 客户端部署

CVBBMSO 提供 Apple Silicon 和 Intel 两种 DMG，要求 macOS 14 或更新版本。监考服务须使用 macOS 已信任的 HTTPS 证书，并支持 WSS。

## 考前启动

1. 从监考方取得适合设备架构的 DMG 和 SHA-256。用 `shasum -a 256 -c <校验文件>` 核对后，打开映像，将 `CVBBMSO.app` 拖到「应用程序」。推出映像，再从「应用程序」双击 CVBBMSO。
2. 客户端通过设备和网络检查后会自动请求一次系统管理员授权。管理员在 macOS 提示框中授权。需要退出时先取消授权。取消或失败后，控制窗可重试，Accoding 和考试入口保持关闭。
3. 授权成功后，系统安装或更新受保护的应用副本，并启动网络守卫。守卫以当前桌面用户身份启动客户端，引导窗口随接棒结束。若系统要求录屏权限，在「系统设置 → 隐私与安全性」允许 CVBBMSO 录制屏幕。需要重启时先退出客户端，再从「应用程序」双击 CVBBMSO。

受保护的客户端在连接守卫前遇到网络预检失败时，请检查网络和代理设置，退出客户端后从「应用程序」重新打开。管理员授权前的网络检查仍可在原窗口重试。

从试用版或更早的正式部署迁移时，先退出旧客户端并等待其守卫恢复。新入口使用 `edu.buaa.accoding.cvbbmso` 应用标识。旧安装、Cookie 与录像仍在原位置，权限变更可能需要重新授予录屏权限。

## 守卫与恢复

应用安装在 `/Library/Application Support/com.cvbbmso-client/CVBBMSO.app`。包内的 `Contents/Helpers/cvbbmso-pf` 负责 PF、看门狗和 launchd 恢复。管理员操作先把 App 复制到 root 保护的临时目录，核验整包签名，并将签名摘要与正在运行的 App 对照。升级会先确认旧会话已恢复，并持有会话锁更换 App 和 helper。

正常退出时等客户端清理守卫。异常退出时保留已安装的 App 和 `/Library/Application Support/com.cvbbmso/` 下的恢复记录，看门狗和 launchd 会继续尝试恢复。确认客户端已经退出后，管理员可手动触发恢复。

```sh
sudo '/Library/Application Support/com.cvbbmso-client/CVBBMSO.app/Contents/Helpers/cvbbmso-pf' recover
```

自定义 PF 主过滤规则应在首条过滤规则位置保留双向 `anchor "com.cvbbmso.egress"`。宿主规则有冲突，或 CVBBMSO 无法确认原规则可恢复时，管理员按原有宿主管理流程核对内核规则和恢复记录。详见 [Network 的 PF 契约](../design/crates/cvbbmso-network.md)。

## 构建和核验

使用对应架构的原生 macOS、macOS SDK 和 `rust-toolchain.toml` 指定的 Rust 工具链。以下命令生成临时签名的候选 DMG 和独立 SHA-256 文件。

```sh
export CVBBMSO_SERVER_ORIGIN=https://monitor.example.edu/
./deploy/macos-egress/build-dmg.sh candidate
./deploy/macos-egress/verify-dmg.sh "$IMAGE" candidate
```

`IMAGE` 为构建脚本输出的 DMG 路径。正式签名交付使用同一脚本的 `release` 模式，设置 `CVBBMSO_MACOS_TEAM_ID`、`CVBBMSO_MACOS_SIGN_IDENTITY` 和 `CVBBMSO_MACOS_NOTARY_PROFILE`。脚本用 Developer ID 依次签 helper、App 和 DMG，然后提交 Apple 公证并装订票据。密钥和公证凭据存放在发布密钥链。[Apple 分发打包文档](https://developer.apple.com/documentation/xcode/packaging-mac-software-for-distribution)

构建检查会只读挂载映像，核对「应用程序」入口、App 标识、源码提交、嵌套签名、架构和最低系统版本。管理员授权、录屏权限、实际 PF 续期及异常恢复须在隔离实体 Mac 验收。
