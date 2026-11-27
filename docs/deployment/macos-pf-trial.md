# CVBBMSO macOS PF 试用包

适用于 Apple Silicon 和 macOS 14 及以上，用于隔离 Mac 上的管理员试用。包内有带临时签名（ad hoc）的 `CVBBMSO Trial.app`、管理程序、启动脚本及 `BUILD.txt`。后者记录源码提交和编译进 App、守卫的监考 HTTPS Origin。

## 准备监考服务

回环联调包绑定 `https://127.0.0.1:54567/`。该地址指向运行 App 的那台 Mac。请在这台隔离 Mac 上启动同版监考服务的 HTTPS 反代，让反代转发 `/api/health`、`/api/exams` 和 `/api/exams/{exam_id}/ws`，并支持 WebSocket 升级。证书须在有效期内并受该 Mac 信任，证书的 IP SAN 须包含 `127.0.0.1`。监考服务、反代和证书由管理员在隔离环境准备。

在该 Mac 上以当前桌面用户执行以下命令，检查系统信任下的 HTTPS 健康端点。此操作仅发出一次 HTTPS 请求。

```sh
'./Start CVBBMSO Trial.command' --check
```

## 安装和启动

管理员从可信渠道取得 ZIP 与 SHA-256，先在仅管理员可写的暂存目录核对 `shasum -a 256 -c CVBBMSO-Trial-macos-arm64.zip.sha256`，再解包。首次安装前确认以下目标尚未安装，且所用父目录由 root 控制。若已有试用版，先结束会话并按下节确认 PF 清理，再处理旧文件。

从解包后受保护的 `CVBBMSO Trial` 目录执行：

```sh
trial_home='/Library/Application Support/com.cvbbmso-trial'
if [ ! -d /Library/PrivilegedHelperTools ]; then
  sudo /usr/bin/install -d -o root -g wheel -m 0755 /Library/PrivilegedHelperTools
fi
sudo /usr/bin/install -d -o root -g wheel -m 0755 "$trial_home"
sudo /usr/bin/ditto 'CVBBMSO Trial.app' "$trial_home/CVBBMSO Trial.app"
sudo /usr/sbin/chown -R root:wheel "$trial_home/CVBBMSO Trial.app"
sudo /bin/chmod -R go-w "$trial_home/CVBBMSO Trial.app"
sudo /usr/bin/install -o root -g wheel -m 0755 admin/cvbbmso-pf-trial \
  /Library/PrivilegedHelperTools/com.cvbbmso.pf-trial
sudo /usr/bin/install -o root -g wheel -m 0755 admin/pf-trial.sh \
  /Library/PrivilegedHelperTools/cvbbmso-pf-trial.sh
sudo /Library/PrivilegedHelperTools/cvbbmso-pf-trial.sh status
```

由具备 sudo 权限的当前桌面用户打开 `Start CVBBMSO Trial.command`。脚本先检查 HTTPS，再请管理员授权启动守卫。管理程序以当前桌面用户启动 App。根据控制窗提示检查虚拟机检测、代理和录屏权限。录屏授权提示重启时，先退出 App，等启动脚本结束，再重新打开启动脚本。

## 退出与恢复

平时从 App 退出，等待启动脚本打印 `Trial ended and PF cleanup confirmed`。安装命令中的 `status` 仅检查安装、控制台用户和 HTTPS 策略。若 App 异常退出、启动脚本被中断，或未见 PF 清理确认，先确认 App 和试用会话已经结束，再运行恢复命令并核对结果。

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
