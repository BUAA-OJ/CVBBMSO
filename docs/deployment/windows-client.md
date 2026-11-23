# Windows x64 客户端安装包

## 生成候选包

使用 x64 Windows、PowerShell 7、Node.js 22 和带 Windows SDK 的 Visual Studio C++ Build Tools。使用仓库指定的 Rust 1.99.1 原生 `x86_64-pc-windows-msvc` 工具链。CI 安装同一版本。Tauri CLI 由脚本按固定版本下载，WiX 由 Tauri 管理。生成安装包的主机需要能访问 npm、Cargo 和 GitHub 工具下载源。[Tauri 构建依赖](https://v2.tauri.app/start/prerequisites/)

在干净的 Git 工作树根目录运行以下命令，并清除本地 `TAURI_CONFIG` 覆盖。将示例域名替换为部署监考服务的 HTTPS Origin。客户端从 Windows 证书信任库验证监考服务证书，部署前应按考场组织的流程安装所需的根证书。

```powershell
pwsh -File ./scripts/windows/build-candidate.ps1 `
  -MonitorOrigin 'https://monitor.example.org' -OutputDirectory './target/windows-candidate'
```

也可手动运行仓库的 CI 工作流，为 `windows_candidate_origin` 填写该地址。工作流在托管 Windows 上安装和卸载候选包，检查 EXE 架构、Visual C++ 运行库依赖和考试数据保留，再上传 MSI 与 `candidate.json`。清单记录源提交、监考 Origin、SHA-256 和签名状态。

```powershell
gh workflow run ci.yml --ref feat/client -f windows_candidate_origin=https://monitor.example.org
```

当前链路生成供验收的候选包。生产发布由具备签名材料的一方对客户端和安装包签名，再使用实际监考地址完成部署验收。

## 安装、启动和卸载

1. 用管理员权限安装 MSI。默认安装到 `%ProgramFiles%\CVBBMSO`。安装时如果系统缺少 Microsoft Edge WebView2 Evergreen Runtime，安装器通过微软的引导程序联网安装。无法联网的考场应先由管理员部署 Evergreen Runtime。客户端静态链接 Visual C++ 运行库。[Tauri Windows 安装器](https://v2.tauri.app/distribute/windows-installer/)
2. 关闭安装结尾的启动选项。用参加考试的同一个 Windows 账号登录，右键开始菜单里的 CVBBMSO，选择「以管理员身份运行」。如果用另一个账号提权，应用数据会写入那个账号的配置目录。
3. 升级或卸载前先正常退出客户端，等待考试收尾。升级时运行较新版本 MSI。卸载可从「设置 → 应用 → 已安装的应用」执行，或使用 `msiexec.exe /x <安装包路径>`。

考试录像保存在 `%LOCALAPPDATA%\edu.buaa.accoding.cvbbmso\exams\<考试 ID>\<考生 ID>\recording`。标准 MSI 安装和卸载均保留这个目录。需要移交录像时，从运行客户端的 Windows 用户配置目录复制考试数据。
