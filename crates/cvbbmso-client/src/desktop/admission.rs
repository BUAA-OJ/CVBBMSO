use std::{
    fs,
    future::{Future, pending},
    io,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use cvbbmso_network::{
    EgressGuard, EgressPolicy, EndpointState, Inspector, NetworkError, ProxyState,
};
use serde::Serialize;
#[cfg(target_os = "macos")]
use tauri::WebviewWindow;
use tauri::{AppHandle, Emitter, EventTarget, Manager, async_runtime};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Instant, sleep, sleep_until, timeout},
};

use super::{Desktop, UI, platform};
use crate::preflight::Preflight;

#[cfg(test)]
pub(super) mod fixture;

const VERIFY_INTERVAL: Duration = Duration::from_secs(30);
const NETWORK_CHECK_INTERVAL: Duration = Duration::from_secs(3);
const NETWORK_CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const STATUS_EVENT: &str = "admission-status";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Failure {
    #[cfg(windows)]
    ElevationRequired,
    #[cfg(target_os = "macos")]
    MacosUnsupported,
    #[cfg(target_os = "macos")]
    MacosUntrustedInstallation,
    #[cfg(target_os = "macos")]
    MacosServiceUnavailable,
    #[cfg(target_os = "macos")]
    MacosAuthorizationRequired,
    #[cfg(target_os = "macos")]
    MacosAuthorizationCancelled,
    #[cfg(target_os = "macos")]
    MacosAuthorizationFailed,
    #[cfg(target_os = "macos")]
    MacosUnsupportedPfConfiguration,
    #[cfg(target_os = "macos")]
    MacosBusy,
    MonitorUnconfigured,
    InspectionFailed,
    ProxyConfigured,
    ProxyUnknown,
    TunnelConfigured,
    TunnelUnknown,
    NetworkCheckTimeout,
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
            | Self::MacosUnsupported
            | Self::MacosServiceUnavailable
            | Self::MacosAuthorizationRequired
            | Self::MacosAuthorizationCancelled
            | Self::MacosAuthorizationFailed
            | Self::MacosUntrustedInstallation => false,
            _ => true,
        }
    }

    fn message(self) -> &'static str {
        match self {
            #[cfg(windows)]
            Self::ElevationRequired => "请以管理员身份启动客户端。",
            #[cfg(target_os = "macos")]
            Self::MacosUnsupported => "CVBBMSO 需要 macOS 14 或更新版本，请联系考试管理员。",
            #[cfg(target_os = "macos")]
            Self::MacosUntrustedInstallation => {
                "请将监考方提供的 CVBBMSO.app 拖入「应用程序」，再从那里打开。"
            }
            #[cfg(target_os = "macos")]
            Self::MacosServiceUnavailable => {
                "未能连接考试网络守卫。请退出客户端，再从「应用程序」打开 CVBBMSO。"
            }
            #[cfg(target_os = "macos")]
            Self::MacosAuthorizationRequired => "需要管理员授权才能部署考试网络守卫。",
            #[cfg(target_os = "macos")]
            Self::MacosAuthorizationCancelled => "管理员授权已取消，可以重试。",
            #[cfg(target_os = "macos")]
            Self::MacosAuthorizationFailed => {
                "无法部署或启动考试网络守卫，请重试或联系监考管理员。"
            }
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
            Self::TunnelConfigured => "检测到系统流量经 TUN 或 VPN 路由，请关闭后重试。",
            Self::TunnelUnknown => "无法确认 TUN 或 VPN 路由状态，请联系考试管理员。",
            Self::NetworkCheckTimeout => "网络环境检查超时，请保持客户端运行。",
            Self::MonitorUnreachable => "无法连接监考服务，请检查网络后重试。",
            Self::GuardInstallFailed => "无法启用出站网络守卫，请联系考试管理员。",
            Self::GuardLost => "出站网络守卫校验失败，请联系监考人员后重新启动客户端。",
            #[cfg(target_os = "macos")]
            Self::ScreenPermission => "请在系统设置中允许客户端录制屏幕，再重试。",
            #[cfg(target_os = "macos")]
            Self::ScreenRestartRequired => {
                "屏幕录制权限已设置。请退出客户端，再从「应用程序」重新打开 CVBBMSO。"
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub(super) enum Status {
    Pending,
    Checking,
    #[cfg(target_os = "macos")]
    Authorizing,
    Ready,
    Blocked(Failure),
    Restricted(Failure),
    Exiting,
    CleanupFailed(CleanupReport),
}

pub(super) const CLEANUP_DIAGNOSTIC: &str = "egress-cleanup.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(super) struct CleanupReport {
    code: &'static str,
    diagnostic_saved: bool,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum CleanupError {
    #[error("egress guard cleanup failed")]
    Guard(#[from] NetworkError),
    #[error("the egress guard worker ended during cleanup")]
    WorkerUnavailable,
}

impl CleanupError {
    fn code(&self) -> &'static str {
        match self {
            Self::WorkerUnavailable => "worker_unavailable",
            Self::Guard(NetworkError::Platform) => "platform",
            Self::Guard(NetworkError::Io) => "io",
            Self::Guard(NetworkError::PermissionDenied) => "permission_denied",
            Self::Guard(NetworkError::Policy) => "policy",
            Self::Guard(NetworkError::Resolution) => "resolution",
            #[cfg(target_os = "macos")]
            Self::Guard(NetworkError::MacosUnsupported) => "macos_unsupported",
            #[cfg(target_os = "macos")]
            Self::Guard(NetworkError::MacosUntrustedInstallation) => "macos_untrusted_installation",
            #[cfg(target_os = "macos")]
            Self::Guard(NetworkError::MacosServiceUnavailable) => "macos_service_unavailable",
            #[cfg(target_os = "macos")]
            Self::Guard(NetworkError::MacosUnsupportedPfConfiguration) => {
                "macos_unsupported_pf_configuration"
            }
            #[cfg(target_os = "macos")]
            Self::Guard(NetworkError::MacosBusy) => "macos_busy",
            Self::Guard(_) => "unknown",
        }
    }

    pub(super) async fn report(&self, data_dir: &Path) -> CleanupReport {
        let code = self.code();
        let data_dir = data_dir.to_owned();
        let diagnostic_saved =
            tokio::task::spawn_blocking(move || save_cleanup_report(&data_dir, code))
                .await
                .is_ok_and(|result| result.is_ok());
        CleanupReport {
            code,
            diagnostic_saved,
        }
    }
}

fn save_cleanup_report(data_dir: &Path, code: &'static str) -> io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let mut diagnostic = tempfile::NamedTempFile::new_in(data_dir)?;
    // Only stable classifications enter the report. Replace the previous report
    // instead of appending indefinitely or following a diagnostic-file symlink.
    serde_json::to_writer(
        &mut diagnostic,
        &serde_json::json!({
            "operation": "egress_shutdown",
            "platform": std::env::consts::OS,
            "code": code,
            "unix_seconds": SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|age| age.as_secs()),
        }),
    )?;
    diagnostic.persist(data_dir.join(CLEANUP_DIAGNOSTIC))?;
    Ok(())
}

pub(super) struct Admission {
    state: watch::Sender<Status>,
    shutdown: mpsc::Sender<oneshot::Sender<Result<(), NetworkError>>>,
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

    pub(super) fn is_ready(&self) -> bool {
        self.status() == Status::Ready
    }

    pub(super) fn require_ready(&self) -> Result<(), &'static str> {
        match self.status() {
            Status::Ready => Ok(()),
            Status::Blocked(reason) | Status::Restricted(reason) => Err(reason.message()),
            _ => Err("考试环境检查尚未完成。"),
        }
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<Status> {
        self.state.subscribe()
    }

    pub(super) fn begin_exit(&self, app: &AppHandle) {
        self.set(app, Status::Exiting, |status| status != Status::Exiting);
    }

    #[cfg(target_os = "macos")]
    pub(super) fn is_authorizing(&self) -> bool {
        self.status() == Status::Authorizing
    }

    #[cfg(target_os = "macos")]
    pub(super) fn request_exit(&self, app: &AppHandle) -> bool {
        self.set(app, Status::Exiting, |status| status != Status::Authorizing)
    }

    pub(super) async fn shutdown(&self) -> Result<(), CleanupError> {
        let (reply, done) = oneshot::channel();
        self.shutdown
            .send(reply)
            .await
            .map_err(|_| CleanupError::WorkerUnavailable)?;
        done.await
            .map_err(|_| CleanupError::WorkerUnavailable)?
            .map_err(CleanupError::from)
    }

    pub(super) fn show_cleanup_failure(&self, app: &AppHandle, report: CleanupReport) {
        self.set(app, Status::CleanupFailed(report), |status| {
            status == Status::Exiting
        });
    }

    fn status(&self) -> Status {
        *self.state.borrow()
    }

    fn set(&self, app: &AppHandle, next: Status, allows: impl FnOnce(Status) -> bool) -> bool {
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
        changed
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
pub(super) async fn authorize_macos_egress(window: WebviewWindow) -> Result<(), &'static str> {
    if window.label() != UI {
        return Err("authorization_unavailable");
    }
    let app = window.app_handle();
    let admission = app.state::<Admission>();
    if !app.state::<Preflight>().is_admitted()
        || !admission.set(app, Status::Authorizing, |status| {
            matches!(
                status,
                Status::Blocked(
                    Failure::MacosAuthorizationRequired
                        | Failure::MacosAuthorizationCancelled
                        | Failure::MacosAuthorizationFailed
                )
            )
        })
    {
        return Err("authorization_unavailable");
    }

    // The replacement App provides its own admission; a successful handoff
    // never admits the process that opened the authorization prompt.
    match platform::authorization::launch().await {
        Ok(()) => {
            admission.begin_exit(app);
            app.exit(0);
        }
        Err(error) => {
            let failure = match error {
                platform::authorization::AuthorizationError::Cancelled => {
                    Failure::MacosAuthorizationCancelled
                }
                platform::authorization::AuthorizationError::Bundle => {
                    Failure::MacosUntrustedInstallation
                }
                _ => Failure::MacosAuthorizationFailed,
            };
            admission.set(app, Status::Blocked(failure), |status| {
                status == Status::Authorizing
            });
        }
    }
    Ok(())
}

enum Guard {
    System(EgressGuard),
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

    fn start_network_monitor(&self, app: AppHandle) -> JoinHandle<()> {
        match self {
            Self::System(_) => tokio::spawn(monitor_network(app, check_system_network)),
            #[cfg(test)]
            Self::Fixture(guard) => {
                let guard = guard.clone();
                tokio::spawn(monitor_network(app, move || {
                    let guard = guard.clone();
                    async move { guard.check_network().await }
                }))
            }
        }
    }

    async fn shutdown(self) -> Result<(), NetworkError> {
        match self {
            Self::System(guard) => guard.shutdown().await,
            #[cfg(test)]
            Self::Fixture(guard) => guard.shutdown(),
        }
    }
}

async fn check_system_network() -> Result<(), Failure> {
    check_proxy(
        Inspector::check_proxy()
            .await
            .map_err(|_| Failure::ProxyUnknown)?,
    )?;
    check_tunnel().await
}

async fn monitor_network<Fut>(app: AppHandle, check: impl Fn() -> Fut)
where
    Fut: Future<Output = Result<(), Failure>>,
{
    let admission = app.state::<Admission>();
    loop {
        let probe = check();
        tokio::pin!(probe);
        let result = timeout(NETWORK_CHECK_TIMEOUT, &mut probe).await;
        let next = match result {
            Ok(Ok(())) => Status::Ready,
            Ok(Err(reason)) => Status::Restricted(reason),
            Err(_) => Status::Restricted(Failure::NetworkCheckTimeout),
        };
        admission.set(&app, next, |status| {
            status != next
                && matches!(
                    status,
                    Status::Checking | Status::Ready | Status::Restricted(_)
                )
        });
        if result.is_err() {
            // A blocking platform call can outlive a timeout. Keep one
            // check in flight and use a fresh result to restore admission.
            let _ = probe.await;
        }
        sleep(NETWORK_CHECK_INTERVAL).await;
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
    #[cfg(target_os = "macos")]
    Screen(JoinHandle<Result<(), Failure>>),
}

enum Completed {
    Preflight(Result<Prepared, Failure>),
    Install(Result<Guard, Failure>),
    #[cfg(target_os = "macos")]
    Screen(Result<(), Failure>),
}

async fn completed(work: Option<&mut Work>) -> Completed {
    match work {
        Some(Work::Preflight(work)) => {
            Completed::Preflight(work.await.unwrap_or(Err(Failure::InspectionFailed)))
        }
        Some(Work::Install(work)) => {
            Completed::Install(work.await.unwrap_or(Err(Failure::InspectionFailed)))
        }
        #[cfg(target_os = "macos")]
        Some(Work::Screen(work)) => {
            Completed::Screen(work.await.unwrap_or(Err(Failure::InspectionFailed)))
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
    mut stop: mpsc::Receiver<oneshot::Sender<Result<(), NetworkError>>>,
) {
    let admission = app.state::<Admission>();
    let mut guard = None;
    let mut work = None;
    let mut verify_at = None;
    let mut network_monitor: Option<JoinHandle<()>> = None;
    loop {
        let checking = *changes.borrow_and_update() == Status::Checking;
        if checking && work.is_none() && guard.is_none() {
            work = Some(Work::Preflight(tokio::spawn(prepare(app.clone()))));
        }
        #[cfg(target_os = "macos")]
        if checking && work.is_none() && guard.is_some() && network_monitor.is_none() {
            let app = app.clone();
            work = Some(Work::Screen(tokio::spawn(async move {
                check_screen(&app).await
            })));
        }
        tokio::select! {
            biased;
            reply = stop.recv() => {
                if let Some(task) = network_monitor {
                    task.abort();
                }
                // An in-flight system install can outlive its future. Keep
                // waiting for its result so shutdown has the guard to remove.
                match work {
                    Some(Work::Install(task)) => {
                        if let Ok(Ok(installed)) = task.await {
                            guard = Some(installed);
                        }
                    }
                    Some(Work::Preflight(task)) => task.abort(),
                    #[cfg(target_os = "macos")]
                    Some(Work::Screen(task)) => task.abort(),
                    None => {}
                }
                let result = match guard {
                    Some(guard) => guard.shutdown().await,
                    None => Ok(()),
                };
                if let Some(reply) = reply {
                    let _ = reply.send(result);
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
                        if let Some(task) = network_monitor.take() {
                            task.abort();
                        }
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
                        #[cfg(windows)]
                        {
                            network_monitor = Some(installed.start_network_monitor(app.clone()));
                        }
                        guard = Some(installed);
                    }
                    Completed::Preflight(Err(error)) | Completed::Install(Err(error)) => {
                        admission.set(&app, Status::Blocked(error), |status| status == Status::Checking);
                    }
                    #[cfg(target_os = "macos")]
                    Completed::Screen(result) => {
                        if admission.status() == Status::Checking {
                            match result {
                                Ok(()) => {
                                    network_monitor = guard.as_ref().map(|guard| guard.start_network_monitor(app.clone()));
                                }
                                Err(error) => {
                                    admission.set(&app, Status::Blocked(error), |status| status == Status::Checking);
                                }
                            }
                        }
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
        return Ok(Prepared::Fixture(fake.inner().clone()));
    }
    #[cfg(windows)]
    if !platform::is_elevated() {
        return Err(Failure::ElevationRequired);
    }
    #[cfg(target_os = "macos")]
    let installed = platform::is_in_installed_bundle();
    #[cfg(target_os = "macos")]
    if !installed && !platform::is_in_applications() {
        return Err(Failure::MacosUntrustedInstallation);
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
    check_proxy(report.proxy)?;
    check_tunnel().await?;
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
    if !installed {
        return Err(Failure::MacosAuthorizationRequired);
    }
    Ok(Prepared::System(policy))
}

fn check_proxy(state: ProxyState) -> Result<(), Failure> {
    match state {
        ProxyState::Clear => Ok(()),
        ProxyState::Configured => Err(Failure::ProxyConfigured),
        ProxyState::Unknown => Err(Failure::ProxyUnknown),
    }
}

async fn check_tunnel() -> Result<(), Failure> {
    if Inspector::tunnel_configured()
        .await
        .map_err(|_| Failure::TunnelUnknown)?
    {
        return Err(Failure::TunnelConfigured);
    }
    Ok(())
}

async fn install(app: AppHandle, prepared: Prepared) -> Result<Guard, Failure> {
    if app.state::<Admission>().status() != Status::Checking {
        return Err(Failure::GuardInstallFailed);
    }
    match prepared {
        Prepared::System(policy) => EgressGuard::install(policy)
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
        NetworkError::MacosServiceUnavailable => Failure::MacosServiceUnavailable,
        NetworkError::MacosUnsupportedPfConfiguration => Failure::MacosUnsupportedPfConfiguration,
        NetworkError::MacosBusy => Failure::MacosBusy,
        _ => Failure::GuardInstallFailed,
    }
}

#[cfg(target_os = "macos")]
async fn check_screen(app: &AppHandle) -> Result<(), Failure> {
    #[cfg(test)]
    if let Some(fake) = app.try_state::<fixture::Admission>() {
        return fake.screen();
    }
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
