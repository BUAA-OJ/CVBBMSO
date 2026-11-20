use std::{future::pending, time::Duration};

#[cfg(windows)]
use cvbbmso_network::EgressGuard as SystemGuard;
#[cfg(target_os = "macos")]
use cvbbmso_network::MacosEgressGuard as SystemGuard;
use cvbbmso_network::{EgressPolicy, EndpointState, Inspector, NetworkError, ProxyState};
use serde::Serialize;
#[cfg(target_os = "macos")]
use tauri::WebviewWindow;
use tauri::{AppHandle, Emitter, EventTarget, Manager, async_runtime};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Instant, sleep_until},
};

use super::{Desktop, UI, platform};
use crate::preflight::Preflight;

#[cfg(test)]
pub(super) mod fixture;

const VERIFY_INTERVAL: Duration = Duration::from_secs(30);
const STATUS_EVENT: &str = "admission-status";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Failure {
    #[cfg(windows)]
    ElevationRequired,
    #[cfg(target_os = "macos")]
    MacosLoginRequired,
    #[cfg(target_os = "macos")]
    MacosUnsupported,
    #[cfg(target_os = "macos")]
    MacosUntrustedInstallation,
    #[cfg(target_os = "macos")]
    MacosApprovalRequired,
    #[cfg(target_os = "macos")]
    MacosServiceUnavailable,
    #[cfg(target_os = "macos")]
    MacosUnsupportedPfConfiguration,
    #[cfg(target_os = "macos")]
    MacosBusy,
    MonitorUnconfigured,
    InspectionFailed,
    ProxyConfigured,
    ProxyUnknown,
    MonitorUnreachable,
    GuardInstallFailed,
    GuardLost,
    #[cfg(target_os = "macos")]
    ScreenPermission,
    #[cfg(target_os = "macos")]
    ScreenRestartRequired,
}

impl Failure {
    fn retryable(self) -> bool {
        match self {
            Self::MonitorUnconfigured | Self::GuardLost => false,
            #[cfg(windows)]
            Self::ElevationRequired => false,
            #[cfg(target_os = "macos")]
            Self::ScreenRestartRequired
            | Self::MacosLoginRequired
            | Self::MacosUnsupported
            | Self::MacosUntrustedInstallation => false,
            _ => true,
        }
    }

    fn message(self) -> &'static str {
        match self {
            #[cfg(windows)]
            Self::ElevationRequired => "请以管理员身份启动客户端。",
            #[cfg(target_os = "macos")]
            Self::MacosLoginRequired => "请以当前登录用户在 Finder 的「应用程序」中打开 CVBBMSO。",
            #[cfg(target_os = "macos")]
            Self::MacosUnsupported => "当前 macOS 版本不支持考试后台服务，请联系考试管理员。",
            #[cfg(target_os = "macos")]
            Self::MacosUntrustedInstallation => {
                "请使用监考方提供的安装包，将 CVBBMSO.app 从磁盘映像拖到「应用程序」，再双击打开。"
            }
            #[cfg(target_os = "macos")]
            Self::MacosApprovalRequired => {
                "请在系统设置的「登录项与扩展」中允许 CVBBMSO 后台服务，然后返回客户端。"
            }
            #[cfg(target_os = "macos")]
            Self::MacosServiceUnavailable => "考试后台服务尚未就绪，请重试。",
            #[cfg(target_os = "macos")]
            Self::MacosUnsupportedPfConfiguration => {
                "当前 PF 配置无法启用考试网络限制，请联系考试管理员后重试。"
            }
            #[cfg(target_os = "macos")]
            Self::MacosBusy => "另一个客户端或守卫会话仍在运行，请关闭后重试。",
            Self::MonitorUnconfigured => "未配置监考服务地址，请联系考试管理员。",
            Self::InspectionFailed => "无法完成网络预检，请重试。",
            Self::ProxyConfigured => "检测到系统代理，请关闭代理后重试。",
            Self::ProxyUnknown => "无法确认系统代理状态，请联系考试管理员。",
            Self::MonitorUnreachable => "无法连接监考服务，请检查网络后重试。",
            Self::GuardInstallFailed => "无法启用出站网络守卫，请联系考试管理员。",
            Self::GuardLost => "出站网络守卫校验失败，请联系监考人员后重新启动客户端。",
            #[cfg(target_os = "macos")]
            Self::ScreenPermission => "请在系统设置中允许客户端录制屏幕，再重试。",
            #[cfg(target_os = "macos")]
            Self::ScreenRestartRequired => "屏幕录制权限已设置，请重新启动客户端。",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub(super) enum Status {
    Pending,
    Checking,
    Ready,
    Blocked(Failure),
    Exiting,
}

pub(super) struct Admission {
    state: watch::Sender<Status>,
    shutdown: mpsc::Sender<oneshot::Sender<bool>>,
}

impl Admission {
    pub(super) fn launch(app: &AppHandle) {
        let (state, changes) = watch::channel(Status::Pending);
        let (shutdown, stop) = mpsc::channel(1);
        app.manage(Self { state, shutdown });
        async_runtime::spawn(run(app.clone(), changes, stop));
    }

    pub(super) fn start(&self, app: &AppHandle, retry: bool) {
        if !app.state::<Preflight>().is_admitted() {
            return;
        }
        self.set(app, Status::Checking, |status| {
            status == Status::Pending
                || (retry && matches!(status, Status::Blocked(reason) if reason.retryable()))
        });
    }

    #[cfg(target_os = "macos")]
    pub(super) fn recheck_approval(&self, app: &AppHandle) {
        self.set(app, Status::Checking, |status| {
            status == Status::Blocked(Failure::MacosApprovalRequired)
        });
    }

    pub(super) fn is_ready(&self) -> bool {
        self.status() == Status::Ready
    }

    pub(super) fn require_ready(&self) -> Result<(), &'static str> {
        match self.status() {
            Status::Ready => Ok(()),
            Status::Blocked(reason) => Err(reason.message()),
            _ => Err("考试环境检查尚未完成。"),
        }
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<Status> {
        self.state.subscribe()
    }

    pub(super) fn begin_exit(&self, app: &AppHandle) {
        self.set(app, Status::Exiting, |status| status != Status::Exiting);
    }

    pub(super) async fn shutdown(&self) -> bool {
        let (reply, done) = oneshot::channel();
        self.shutdown.send(reply).await.is_ok() && done.await.unwrap_or(false)
    }

    fn status(&self) -> Status {
        *self.state.borrow()
    }

    fn set(&self, app: &AppHandle, next: Status, allows: impl FnOnce(Status) -> bool) {
        let changed = self.state.send_if_modified(|status| {
            if !allows(*status) {
                return false;
            }
            *status = next;
            true
        });
        if changed {
            let _ = app.emit_to(EventTarget::webview(UI), STATUS_EVENT, next);
        }
    }
}

#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri injects the app handle by value"
)]
pub(super) fn admission_status(app: AppHandle, retry: bool) -> Status {
    let admission = app.state::<Admission>();
    if retry {
        admission.start(&app, true);
    }
    admission.status()
}

#[cfg(target_os = "macos")]
#[tauri::command]
pub(super) async fn open_helper_approval_settings(
    window: WebviewWindow,
) -> Result<(), &'static str> {
    const UNAVAILABLE: &str = "当前无需批准考试后台服务。";
    const OPEN_FAILED: &str = "无法打开系统设置。请手动打开「登录项与扩展」。";
    if window.label() != UI {
        return Err(UNAVAILABLE);
    }
    let app = window.app_handle();
    if app.state::<Admission>().status() != Status::Blocked(Failure::MacosApprovalRequired) {
        return Err(UNAVAILABLE);
    }

    let (reply, result) = oneshot::channel();
    let current = app.clone();
    app.run_on_main_thread(move || {
        let opened = if current.state::<Admission>().status()
            == Status::Blocked(Failure::MacosApprovalRequired)
        {
            #[cfg(test)]
            if let Some(fake) = current.try_state::<fixture::Admission>() {
                fake.open_approval_settings();
                let _ = reply.send(Ok(()));
                return;
            }
            SystemGuard::open_approval_settings().map_err(|_| OPEN_FAILED)
        } else {
            Err(UNAVAILABLE)
        };
        let _ = reply.send(opened);
    })
    .map_err(|_| OPEN_FAILED)?;
    result.await.map_err(|_| OPEN_FAILED)?
}

enum Guard {
    System(SystemGuard),
    #[cfg(test)]
    Fixture(fixture::Guard),
}

impl Guard {
    fn interval(&self) -> Duration {
        match self {
            Self::System(_) => VERIFY_INTERVAL,
            #[cfg(test)]
            Self::Fixture(_) => Duration::from_millis(100),
        }
    }

    async fn verify(&self) -> bool {
        match self {
            Self::System(guard) => guard.verify().await.is_ok(),
            #[cfg(test)]
            Self::Fixture(guard) => guard.verify(),
        }
    }

    async fn shutdown(self) -> bool {
        match self {
            Self::System(guard) => guard.shutdown().await.is_ok(),
            #[cfg(test)]
            Self::Fixture(guard) => guard.shutdown(),
        }
    }
}

enum Prepared {
    System(EgressPolicy),
    #[cfg(test)]
    Fixture(fixture::Admission),
}

enum Work {
    Preflight(JoinHandle<Result<Prepared, Failure>>),
    Install(JoinHandle<Result<Guard, Failure>>),
}

enum Completed {
    Preflight(Result<Prepared, Failure>),
    Install(Result<Guard, Failure>),
}

async fn completed(work: Option<&mut Work>) -> Completed {
    match work {
        Some(Work::Preflight(work)) => {
            Completed::Preflight(work.await.unwrap_or(Err(Failure::InspectionFailed)))
        }
        Some(Work::Install(work)) => {
            Completed::Install(work.await.unwrap_or(Err(Failure::InspectionFailed)))
        }
        None => pending().await,
    }
}

async fn next_verify(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        sleep_until(deadline).await;
    } else {
        pending::<()>().await;
    }
}

async fn run(
    app: AppHandle,
    mut changes: watch::Receiver<Status>,
    mut stop: mpsc::Receiver<oneshot::Sender<bool>>,
) {
    let admission = app.state::<Admission>();
    let mut guard = None;
    let mut work = None;
    let mut verify_at = None;
    loop {
        let checking = *changes.borrow_and_update() == Status::Checking;
        if checking && work.is_none() {
            work = Some(Work::Preflight(tokio::spawn(prepare(app.clone()))));
        }
        tokio::select! {
            biased;
            reply = stop.recv() => {
                // An in-flight system install can outlive its future. Keep
                // waiting for its result so shutdown has the guard to remove.
                match work {
                    Some(Work::Install(task)) => {
                        if let Ok(Ok(installed)) = task.await {
                            guard = Some(installed);
                        }
                    }
                    Some(Work::Preflight(task)) => task.abort(),
                    None => {}
                }
                let ok = match guard {
                    Some(guard) => guard.shutdown().await,
                    None => true,
                };
                if let Some(reply) = reply {
                    let _ = reply.send(ok);
                }
                return;
            }
            _ = changes.changed() => {}
            () = next_verify(verify_at) => {
                verify_at = None;
                if admission.status() != Status::Exiting && let Some(guard) = &guard {
                    if guard.verify().await {
                        verify_at = Some(Instant::now() + guard.interval());
                    } else {
                        admission.set(&app, Status::Blocked(Failure::GuardLost), |status| status != Status::Exiting);
                    }
                }
            }
            result = completed(work.as_mut()) => {
                work = None;
                match result {
                    Completed::Preflight(Ok(prepared)) => {
                        if admission.status() == Status::Checking {
                            work = Some(Work::Install(tokio::spawn(install(app.clone(), prepared))));
                        }
                    }
                    Completed::Install(Ok(installed)) => {
                        verify_at = Some(Instant::now() + installed.interval());
                        guard = Some(installed);
                        admission.set(&app, Status::Ready, |status| status == Status::Checking);
                    }
                    Completed::Preflight(Err(error)) | Completed::Install(Err(error)) => {
                        admission.set(&app, Status::Blocked(error), |status| status == Status::Checking);
                    }
                }
            }
        }
    }
}

async fn prepare(app: AppHandle) -> Result<Prepared, Failure> {
    if app.state::<Admission>().status() != Status::Checking {
        return Err(Failure::InspectionFailed);
    }
    #[cfg(test)]
    if let Some(fake) = app.try_state::<fixture::Admission>() {
        fake.check_network()?;
        #[cfg(target_os = "macos")]
        fake.screen()?;
        return Ok(Prepared::Fixture(fake.inner().clone()));
    }
    #[cfg(windows)]
    if !platform::is_elevated() {
        return Err(Failure::ElevationRequired);
    }
    #[cfg(target_os = "macos")]
    {
        if platform::is_root() {
            return Err(Failure::MacosLoginRequired);
        }
        if !platform::is_in_applications() {
            return Err(Failure::MacosUntrustedInstallation);
        }
    }
    let desktop = app.state::<Desktop>();
    let monitor = desktop.monitor.as_ref().map_err(|error| match error {
        crate::monitor::MonitorError::Configuration => Failure::MonitorUnconfigured,
        _ => Failure::InspectionFailed,
    })?;
    let server_origin = monitor.server_base().clone();
    let report = Inspector::new(server_origin.clone())
        .check()
        .await
        .map_err(|_| Failure::InspectionFailed)?;
    match report.proxy {
        ProxyState::Clear => {}
        ProxyState::Configured => return Err(Failure::ProxyConfigured),
        ProxyState::Unknown => return Err(Failure::ProxyUnknown),
    }
    if report.endpoint != EndpointState::Reachable {
        return Err(Failure::MonitorUnreachable);
    }
    let policy = EgressPolicy {
        accoding_origin: desktop
            .login
            .origin()
            .ascii_serialization()
            .parse()
            .map_err(|_| Failure::GuardInstallFailed)?,
        server_origin,
    };
    #[cfg(target_os = "macos")]
    check_screen(&app).await?;
    Ok(Prepared::System(policy))
}

async fn install(app: AppHandle, prepared: Prepared) -> Result<Guard, Failure> {
    if app.state::<Admission>().status() != Status::Checking {
        return Err(Failure::GuardInstallFailed);
    }
    match prepared {
        Prepared::System(policy) => SystemGuard::install(policy)
            .await
            .map(Guard::System)
            .map_err(install_failure),
        #[cfg(test)]
        Prepared::Fixture(fake) => {
            #[cfg(target_os = "macos")]
            fake.check_helper()?;
            Ok(Guard::Fixture(fake.install().await))
        }
    }
}

#[cfg(windows)]
fn install_failure(_: NetworkError) -> Failure {
    Failure::GuardInstallFailed
}

#[cfg(target_os = "macos")]
fn install_failure(error: NetworkError) -> Failure {
    match error {
        NetworkError::MacosUnsupported => Failure::MacosUnsupported,
        NetworkError::MacosUntrustedInstallation => Failure::MacosUntrustedInstallation,
        NetworkError::MacosApprovalRequired => Failure::MacosApprovalRequired,
        NetworkError::MacosServiceUnavailable => Failure::MacosServiceUnavailable,
        NetworkError::MacosUnsupportedPfConfiguration => Failure::MacosUnsupportedPfConfiguration,
        NetworkError::MacosBusy => Failure::MacosBusy,
        _ => Failure::GuardInstallFailed,
    }
}

#[cfg(target_os = "macos")]
async fn check_screen(app: &AppHandle) -> Result<(), Failure> {
    let (reply, result) = oneshot::channel();
    let current = app.clone();
    app.run_on_main_thread(move || {
        if current.state::<Admission>().status() == Status::Checking {
            let _ = reply.send(platform::screen_permission());
        }
    })
    .map_err(|_| Failure::InspectionFailed)?;
    result.await.map_err(|_| Failure::InspectionFailed)?
}
