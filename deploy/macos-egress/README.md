# macOS 出站守卫部署

管理员负责安装 watchdog 和配置宿主 PF。脚本管理固定路径 `/Library/PrivilegedHelperTools/com.cvbbmso.egress-watchdog`，只查询内核 PF 和 launchd 状态。宿主 PF 配置的合并、加载和回滚沿用主机现有流程。安装 helper 后，客户端进程仍须具备运行守卫所需的 root 权限。

## 构建和安装

在对应客户端提交的干净 checkout 中，以普通用户在原生 macOS shell 运行。构建需有 Rust 工具链和 macOS SDK。脚本使用仓库的 `Cargo.lock`，输出提交、架构、Rust 版本、产物路径和 SHA-256。

```sh
./deploy/macos-egress/build-watchdog.sh
```

管理员从可信发布源取得脚本、对应架构的 watchdog 和摘要。执行 `sudo` 前将脚本及产物放在可信且普通用户不可修改的位置。交付包含源码和构建脚本，构建产物尚未经过 Developer ID 发布签名或 Apple 公证。SHA-256 校验依赖可信来源提供的摘要。正式分发时以完成签名的产物计算交付摘要。[Apple 分发文档](https://developer.apple.com/documentation/xcode/packaging-mac-software-for-distribution)

退出客户端并保持停用，直到安装或撤销结束。将下面的 `WATCHDOG` 和 `SHA256` 替换为产物的绝对路径和批准的摘要。

```sh
sudo ./deploy/macos-egress/deploy.sh install "$WATCHDOG" "$SHA256"
```

脚本核对本机架构和摘要，将新文件以 `root:wheel`、`0755` 原子安装。同一产物可重复安装。升级或回滚时，第三个参数传入已经部署版本的可信摘要 `PREVIOUS_SHA256`。遇到不合规的父目录、链接、扩展 ACL、未知的现有文件、活动守卫或恢复记录，脚本停止并报告原因。已有目录权限由宿主管理员处理。

## 合入并验证 PF 入口

1. 保存宿主管理的原配置或版本。把 [pf-entry.conf](pf-entry.conf) 的单条 `anchor "com.cvbbmso.egress" out` 合入首条实际过滤规则的位置，在原有 `pass`、`block` 和过滤 anchor 前。保留宿主的选项、包规范化、NAT、重定向规则和其他 anchor。专用 anchor 初始为空，入口没有 `quick`，无需 `load anchor`。
2. 如果已有同名入口，核实它属于本项目，再迁移旧的 `out quick` 或旧的静态 `load anchor`。同名 anchor 里有宿主自定义内容时，管理员需先处理命名冲突并保留原策略。非回环 `skip` 和带 `pass` 的 NAT、重定向或双向转换可绕过过滤，现有守卫会拒绝这类配置。
3. 对完整候选运行语法检查。确认差异后，管理员按宿主流程显式激活，随后检查实际加载的规则。`lint` 只解析，`check` 核对 helper、内核中的入口与空 anchor、接口跳过以及绕过过滤的地址转换。`check` 适用于客户端尚未启动时。

```sh
./deploy/macos-egress/deploy.sh lint "$PF_CONF"
sudo ./deploy/macos-egress/deploy.sh check "$SHA256" "$PF_CONF"
```

`PF_CONF` 是管理员准备的完整规则文件，在能解析原配置 include 的环境运行。`pfctl -n` 可能打印通用的 `-f` 提示，规则是否已激活以 `check` 查询到的内核规则为准。更新或回滚均由宿主现有 PF 管理流程负责。

## 退出、恢复与撤销

正常退出时让客户端完成守卫关闭。崩溃时先保留已安装的 helper 和恢复记录，让 watchdog 或周期恢复任务清理。确认客户端已经退出并保持停用后，管理员可用原版本的 helper 手动触发一次恢复。

```sh
sudo /Library/PrivilegedHelperTools/com.cvbbmso.egress-watchdog --recover
```

恢复后核对 `com.cvbbmso.egress` 的规则和表已清空，`com.cvbbmso.egress-cleanup` 任务和 plist 已撤除，自有 PF 引用已经释放。手动恢复返回成功时也要检查状态。有活跃租约时本次恢复会跳过清理。若记录损坏或清理报错，保留 `/Library/Application Support/com.cvbbmso/` 中的 `egress.state`、`pf.rules` 和 `pf.enable` 供管理员核对。用 `pfctl -s References` 对照保存的 token，只在归属明确时处理专用 anchor 及自己的启用引用。保留其他 PF 规则、引用和连接状态。

守卫清理完成后，按宿主流程从持久配置和在用规则中撤除本项目的入口并验证。最后删除与批准摘要匹配的 helper。已完成撤销时可重复运行 `remove`。如果上一次部署被强制中断而留下 `.cvbbmso-egress-deploy.lock`，先确认没有部署进程仍在使用它，再移除该空目录。

```sh
sudo ./deploy/macos-egress/deploy.sh remove "$SHA256"
```

## 开发验收

在 macOS 以普通用户执行 `./deploy/macos-egress/test-deploy.sh`。测试用临时目录执行文件操作，用替身查询 PF 和 launchd，并对样例及无效规则运行真实 `pfctl -n`。可传入已构建的 watchdog 绝对路径，验证实际产物的原生 Mach-O 架构。真实部署、规则激活、考试流量与崩溃恢复须在隔离环境验收。
