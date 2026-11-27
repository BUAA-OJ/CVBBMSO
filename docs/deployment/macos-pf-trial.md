# CVBBMSO macOS PF 试用包

适用于 Apple Silicon 和 macOS 14 及以上，用于隔离 Mac 上的管理员试用。`Payload.zip` 内含带临时签名（ad hoc）的试用 App 和守卫。`BUILD.txt` 记录源码提交和监考 HTTPS Origin。

## 准备监考服务

`BUILD.txt` 里的 `monitor_origin` 是本包使用的监考地址，证书须受测试 Mac 信任。若地址是 `https://127.0.0.1:54567/`，在测试 Mac 上准备 HTTPS 反代，转发 `/api/health`、`/api/exams` 和 `/api/exams/{exam_id}/ws`，并支持 WebSocket 升级。回环证书的 IP SAN 须包含 `127.0.0.1`。

在该 Mac 上以当前桌面用户执行以下命令，检查系统信任下的 HTTPS 健康端点。此操作仅发出一次 HTTPS 请求。

```sh
'./Start CVBBMSO Trial.command' --check
```

## 安装和启动

管理员从可信渠道取得 ZIP 和 SHA-256，用 `shasum -a 256 -c CVBBMSO-Trial-macos-arm64.zip.sha256` 核对后解压。保留解压出来的目录，双击 `Start CVBBMSO Trial.command`，按 Terminal 提示授权。

首次打开时，脚本检查 HTTPS 和 `Payload.zip` 的摘要，安装 App 和守卫，然后启动试用。再次打开同一版本会直接启动。升级时先退出旧 App，再双击新版启动脚本。脚本会先完成旧守卫的清理，再替换试用文件。

在 PF 停用且主过滤规则匹配原厂配置或支持的 ToDesk 规则时，守卫会把试用入口放在现有过滤规则之前。原过滤规则由 watchdog 和恢复任务在退出时恢复。试用期间，白名单之外的连接可能中断，包括通过 ToDesk 连接测试 Mac 的远程会话。

App 以当前桌面用户运行。根据控制窗提示检查虚拟机检测、代理和录屏权限。录屏授权提示重启时，先退出 App，等启动脚本结束，再双击启动。

## 退出与恢复

平时从 App 退出，等待启动脚本打印 `Trial ended and PF cleanup confirmed`。若 App 异常退出、启动脚本被中断，或未见 PF 清理确认，先确认 App 和试用会话已经结束，再运行恢复命令并核对结果。

```sh
sudo /Library/PrivilegedHelperTools/cvbbmso-pf-trial.sh recover
```

若恢复报错，保留管理程序和恢复记录，将错误输出交给管理员核对。

## 从源码构建

在 Apple Silicon 的 macOS 上使用仓库锁定的 Rust 和 macOS SDK。构建学校环境时，将下方参数改成学校监考服务的 HTTPS Origin，App 和守卫须使用同一次构建的产物。

```sh
./deploy/macos-egress/build-trial.sh https://127.0.0.1:54567/
```

脚本输出 ZIP 和同目录的 SHA-256 校验文件。
