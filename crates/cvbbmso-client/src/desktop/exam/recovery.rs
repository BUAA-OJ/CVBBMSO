//! Recovers the verified account's ended exams while the foreground slot is idle.

use cvbbmso_protocol::ExamPhase;
use tauri::async_runtime;

use super::{
    AccodingCookies, AppHandle, COOKIE_SAMPLE_INTERVAL, Desktop, Duration, Exam, ExamHandle,
    ExecutionState, JoinHandle, Manager, Uuid, config, current_cookies, fs, oneshot, same_cookies,
    sleep,
};

const MAX_DELAY: Duration = Duration::from_secs(30);

pub(super) struct Worker {
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Worker {
    pub(super) fn start(app: AppHandle, instance_id: Uuid) -> Self {
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(run(app, instance_id, stopped));
        Self { stop, task }
    }
}

pub(super) async fn stop(worker: &mut Option<Worker>) {
    if let Some(Worker { stop, task }) = worker.take() {
        let _ = stop.send(());
        if let Err(error) = task.await {
            tracing::error!(%error, "exam finalization recovery task stopped unexpectedly");
        }
    }
}

async fn run(app: AppHandle, instance_id: Uuid, mut stopped: oneshot::Receiver<()>) {
    let mut delay = COOKIE_SAMPLE_INTERVAL;
    loop {
        let candidates = tokio::select! {
            biased;
            _ = &mut stopped => return,
            result = discover(&app) => result,
        };
        let mut completed = false;
        if let Some((cookies, user, exams)) = candidates {
            for exam in exams {
                let setup = tokio::select! {
                    biased;
                    _ = &mut stopped => return,
                    result = async {
                        if !still_signed_in(&app, &cookies).await {
                            return None;
                        }
                        config(&app, instance_id).await.ok()
                    } => result,
                };
                let Some(config) = setup else { break };
                // Opening the recorder may recover files. Let that work finish
                // before handing the slot back to a foreground session.
                match Exam::resume_finalization(config, exam, cookies.clone(), user).await {
                    Ok(handle) => {
                        let finished = tokio::select! {
                            biased;
                            _ = &mut stopped => None,
                            result = wait_for_completion(&app, &handle, &cookies) => Some(result),
                        };
                        if let Err(error) = handle.shutdown().await {
                            tracing::warn!(%error, %exam, "saving recovered exam on shutdown failed");
                        } else if finished == Some(true) {
                            completed = true;
                        }
                        if finished.is_none() {
                            return;
                        }
                    }
                    Err(error) => tracing::debug!(%error, %exam, "exam finalization deferred"),
                }
            }
        }
        if completed {
            delay = COOKIE_SAMPLE_INTERVAL;
        }
        tokio::select! {
            _ = &mut stopped => return,
            () = sleep(delay) => {}
        }
        delay = (delay * 2).min(MAX_DELAY);
    }
}

async fn discover(app: &AppHandle) -> Option<(AccodingCookies, i64, Vec<Uuid>)> {
    let desktop = app.state::<Desktop>();
    let monitor = desktop.monitor.as_ref().ok()?;
    let root = desktop.data_dir.join("exams");
    if !async_runtime::spawn_blocking(move || fs::exists(root))
        .await
        .ok()?
        .ok()?
    {
        return None;
    }
    let cookies = current_cookies(app).await.ok()?;
    let verified = monitor.list_exams(&cookies).await.ok()?;
    let mut pending = Vec::new();
    for item in verified.items {
        if item.phase == ExamPhase::Ended
            && matches!(
                Exam::has_pending_finalization(&desktop.data_dir, item.id, verified.user.id).await,
                Ok(true)
            )
        {
            pending.push(item.id);
        }
    }
    Some((cookies, verified.user.id, pending))
}

async fn still_signed_in(app: &AppHandle, expected: &AccodingCookies) -> bool {
    current_cookies(app)
        .await
        .is_ok_and(|cookies| same_cookies(expected, &cookies))
}

async fn wait_for_completion(
    app: &AppHandle,
    handle: &ExamHandle,
    cookies: &AccodingCookies,
) -> bool {
    let mut updates = handle.subscribe();
    let mut sample = tokio::time::interval(COOKIE_SAMPLE_INTERVAL);
    sample.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        {
            let view = updates.borrow_and_update();
            if view.state.execution == ExecutionState::Finished {
                return true;
            }
            // Retry from a fresh enrollment check, and let any other pending
            // exams in this sweep get a turn.
            if !view.connected {
                return false;
            }
        }
        tokio::select! {
            result = updates.changed() => {
                if result.is_err() {
                    return false;
                }
            }
            () = async { sample.tick().await; } => {
                if !still_signed_in(app, cookies).await {
                    return false;
                }
            }
        }
    }
}
