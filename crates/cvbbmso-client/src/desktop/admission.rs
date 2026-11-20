use std::{future::pending, time::Duration};

use cvbbmso_network::{EgressGuard, EgressPolicy, EndpointState, Inspector, ProxyState};
use serde::Serialize;
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
    ElevationRequired,
    MonitorUnconfigured,
    InspectionFailed,
    ProxyConfigured,
    ProxyUnknown,
    MonitorUnreachable,
    GuardInstallFailed,
    GuardLost,
    #[cfg(any(target_os = "macos", test))]
    ScreenPermission,
    #[cfg(target_os = "macos")]
    ScreenRestartRequired,
}

impl Failure {
    fn retryable(self) -> bool {
        match self {
            Self::ElevationRequired | Self::MonitorUnconfigured | Self::GuardLost => false,
            #[cfg(target_os = "macos")]
            Self::ScreenRestartRequired => false,
            _ => true,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::ElevationRequired => "请以管理员身份启动客户端。",
            Self::MonitorUnconfigured => "未配置监考服务地址，请联系考试管理员。",
            Self::InspectionFailed => "无法完成网络预检，请重试。",
            Self::ProxyConfigured => "检测到系统代理，请关闭代理后重试。",
            Self::ProxyUnknown => "无法确认系统代理状态，请联系考试管理员。",
            Self::MonitorUnreachable => "无法连接监考服务，请检查网络后重试。",
            Self::GuardInstallFailed => "无法启用出站网络守卫，请联系考试管理员。",
            Self::GuardLost => "出站网络守卫校验失败，请联系监考人员后重新启动客户端。",
            #[cfg(any(target_os = "macos", test))]
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

    async fn shutdown(self) -> bool {
        match self {
            Self::System(guard) => guard.shutdown().await.is_ok(),
            #[cfg(test)]
            Self::Fixture(guard) => guard.shutdown(),
        }
    }
}

enum Work {
    Network(JoinHandle<Result<Option<Guard>, Failure>>),
    Screen(JoinHandle<Result<(), Failure>>),
}

enum Completed {
    Network(Result<Option<Guard>, Failure>),
    Screen(Result<(), Failure>),
}

async fn completed(work: Option<&mut Work>) -> Completed {
    match work {
        Some(Work::Network(work)) => {
            Completed::Network(work.await.unwrap_or(Err(Failure::InspectionFailed)))
        }
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
    mut stop: mpsc::Receiver<oneshot::Sender<bool>>,
) {
    let admission = app.state::<Admission>();
    let mut guard = None;
    let mut work = None;
    let mut verify_at = None;
    loop {
        let checking = *changes.borrow_and_update() == Status::Checking;
        if checking && work.is_none() {
            work = Some(Work::Network(tokio::spawn(prepare(
                app.clone(),
                guard.is_some(),
            ))));
        }
        tokio::select! {
            biased;
            reply = stop.recv() => {
                // An in-flight system install can outlive its future. Keep
                // waiting for its result so shutdown has the guard to remove.
                match work {
                    Some(Work::Network(task)) => {
                        if let Ok(Ok(Some(installed))) = task.await {
                            guard = Some(installed);
                        }
                    }
                    Some(Work::Screen(task)) => task.abort(),
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
                    Completed::Network(Ok(installed)) => {
                        if let Some(installed) = installed {
                            verify_at = Some(Instant::now() + installed.interval());
                            guard = Some(installed);
                        }
                        if admission.status() == Status::Checking {
                            work = Some(Work::Screen(tokio::spawn(check_screen(app.clone()))));
                        }
                    }
                    Completed::Network(Err(error)) | Completed::Screen(Err(error)) => {
                        admission.set(&app, Status::Blocked(error), |status| status == Status::Checking);
                    }
                    Completed::Screen(Ok(())) => {
                        admission.set(&app, Status::Ready, |status| status == Status::Checking);
                    }
                }
            }
        }
    }
}

async fn prepare(app: AppHandle, guard_installed: bool) -> Result<Option<Guard>, Failure> {
    if app.state::<Admission>().status() != Status::Checking {
        return Ok(None);
    }
    #[cfg(test)]
    if let Some(fake) = app.try_state::<fixture::Admission>() {
        return fake
            .prepare(guard_installed)
            .await
            .map(|guard| guard.map(Guard::Fixture));
    }
    if !platform::is_elevated() {
        return Err(Failure::ElevationRequired);
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
    if guard_installed || app.state::<Admission>().status() != Status::Checking {
        return Ok(None);
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
    EgressGuard::install(policy)
        .await
        .map(|guard| Some(Guard::System(guard)))
        .map_err(|_| Failure::GuardInstallFailed)
}

async fn check_screen(app: AppHandle) -> Result<(), Failure> {
    #[cfg(test)]
    if let Some(fake) = app.try_state::<fixture::Admission>() {
        return fake.screen();
    }
    #[cfg(target_os = "macos")]
    {
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
    #[cfg(windows)]
    {
        let _ = app;
        Ok(())
    }
}
