//! HTTP and WebSocket transport for the exam server.

#![forbid(unsafe_code)]

mod api;
mod hub;
mod socket;
mod writer;

use std::{
    error::Error, io, net::SocketAddr, num::NonZeroUsize, process::ExitCode, sync::Arc,
    time::Duration,
};

use clap::{Args, Parser, error::ErrorKind};
use cvbbmso_accoding::Verifier;
use cvbbmso_domain::{DomainError, ExamService};
use thiserror::Error;
use tokio::{net::TcpListener, time::Instant};

use crate::{api::AuthConfig, hub::HubConfig, writer::WriterConfig};

const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(about = "Run the cvbbmso exam server")]
struct ServerConfig {
    #[arg(long, env = "CVBBMSO_LISTEN_ADDR")]
    listen_addr: SocketAddr,
    #[arg(long, env = "CVBBMSO_DATABASE_URL", hide_env_values = true)]
    database_url: String,
    #[arg(long, env = "CVBBMSO_HEARTBEAT_TIMEOUT_SECS", default_value_t = 25, value_parser = clap::value_parser!(u64).range(1..))]
    heartbeat_timeout_secs: u64,
    #[command(flatten)]
    tuning: ServerTuning,
}

#[derive(Clone, Copy, Debug, Args)]
struct ServerTuning {
    #[arg(long, env = "CVBBMSO_GROUP_COMMIT_DELAY_MS", default_value_t = 50)]
    group_commit_delay_ms: u64,
    #[arg(long, env = "CVBBMSO_MAX_BATCH_SIZE", default_value_t = NonZeroUsize::new(256).unwrap())]
    max_batch_size: NonZeroUsize,
    #[arg(long, env = "CVBBMSO_WRITER_QUEUE_CAPACITY", default_value_t = NonZeroUsize::new(1024).unwrap())]
    writer_queue_capacity: NonZeroUsize,
    #[arg(long, env = "CVBBMSO_MAX_GLOBAL_PENDING_WRITES", default_value_t = NonZeroUsize::new(8192).unwrap())]
    max_global_pending_writes: NonZeroUsize,
    #[arg(long, env = "CVBBMSO_MAX_GLOBAL_PENDING_BYTES", default_value_t = NonZeroUsize::new(32 * 1024 * 1024).unwrap())]
    max_global_pending_bytes: NonZeroUsize,
    #[arg(long, env = "CVBBMSO_MAX_PER_CONN_PENDING", default_value_t = NonZeroUsize::new(4).unwrap())]
    max_per_conn_pending: NonZeroUsize,
    #[arg(long, env = "CVBBMSO_HEARTBEAT_FLUSH_INTERVAL_MS", default_value_t = 1000, value_parser = clap::value_parser!(u64).range(1..))]
    heartbeat_flush_interval_ms: u64,
    #[arg(long, env = "CVBBMSO_SCHEDULE_INTERVAL_MS", default_value_t = 1000, value_parser = clap::value_parser!(u64).range(1..))]
    schedule_interval_ms: u64,
    #[arg(long, env = "CVBBMSO_AUTH_MAX_CONCURRENCY", default_value_t = NonZeroUsize::new(64).unwrap())]
    auth_max_concurrency: NonZeroUsize,
    #[arg(long, env = "CVBBMSO_AUTH_WAIT_TIMEOUT_MS", default_value_t = 5000, value_parser = clap::value_parser!(u64).range(1..))]
    auth_wait_timeout_ms: u64,
}

impl Default for ServerTuning {
    fn default() -> Self {
        Self {
            group_commit_delay_ms: 50,
            max_batch_size: NonZeroUsize::new(256).unwrap(),
            writer_queue_capacity: NonZeroUsize::new(1024).unwrap(),
            max_global_pending_writes: NonZeroUsize::new(8192).unwrap(),
            max_global_pending_bytes: NonZeroUsize::new(32 * 1024 * 1024).unwrap(),
            max_per_conn_pending: NonZeroUsize::new(4).unwrap(),
            heartbeat_flush_interval_ms: 1000,
            schedule_interval_ms: 1000,
            auth_max_concurrency: NonZeroUsize::new(64).unwrap(),
            auth_wait_timeout_ms: 5000,
        }
    }
}

impl ServerConfig {
    fn heartbeat_timeout(&self) -> Result<Duration, ServerError> {
        let timeout = Duration::from_secs(self.heartbeat_timeout_secs);
        Instant::now().checked_add(timeout).ok_or_else(|| {
            ServerError::Config(
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "heartbeat timeout exceeds the monotonic clock range",
                )
                .into(),
            )
        })?;
        Ok(timeout)
    }

    fn hub_config(&self, heartbeat_timeout: Duration) -> HubConfig {
        HubConfig {
            heartbeat_timeout,
            heartbeat_flush_interval: Duration::from_millis(
                self.tuning.heartbeat_flush_interval_ms,
            ),
            schedule_interval: Duration::from_millis(self.tuning.schedule_interval_ms),
            max_per_conn_pending: self.tuning.max_per_conn_pending.get(),
            writer: WriterConfig {
                group_commit_delay: Duration::from_millis(self.tuning.group_commit_delay_ms),
                max_batch_size: self.tuning.max_batch_size.get(),
                queue_capacity: self.tuning.writer_queue_capacity.get(),
                max_global_pending_writes: self.tuning.max_global_pending_writes.get(),
                max_global_pending_bytes: self.tuning.max_global_pending_bytes.get(),
            },
        }
    }

    fn auth_config(&self) -> AuthConfig {
        AuthConfig {
            max_concurrency: self.tuning.auth_max_concurrency.get(),
            wait_timeout: Duration::from_millis(self.tuning.auth_wait_timeout_ms),
        }
    }
}

#[derive(Debug, Error)]
enum ServerError {
    #[error("server configuration failed")]
    Config(#[source] Box<dyn Error + Send + Sync>),
    #[error("server database operation failed")]
    Database(#[from] DomainError),
    #[error("server I/O failed")]
    Io(#[from] io::Error),
}

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(error) = tracing_subscriber::fmt().json().try_init() {
        eprintln!(
            "{}",
            serde_json::json!({
                "category": "config",
                "error": "failed to initialize JSON logging",
                "source": error.to_string(),
            })
        );
        return ExitCode::FAILURE;
    }
    let config = match ServerConfig::try_parse() {
        Ok(config) => config,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            error.exit();
        }
        Err(error) => {
            log_failure(&ServerError::Config(Box::new(error)));
            return ExitCode::from(2);
        }
    };
    match serve(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log_failure(&error);
            ExitCode::FAILURE
        }
    }
}

fn log_failure(error: &ServerError) {
    let category = match error {
        ServerError::Config(_) => "config",
        ServerError::Database(source) => {
            tracing::error!(category = "database", %error, reason = %source.diagnostic(), "server stopped");
            return;
        }
        ServerError::Io(_) => "io",
    };
    let mut sources = Vec::new();
    let mut next = error.source();
    while let Some(source) = next {
        sources.push(source.to_string());
        next = source.source();
    }
    tracing::error!(category, %error, ?sources, "server stopped");
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sigterm) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sigterm.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

async fn serve(config: ServerConfig) -> Result<(), ServerError> {
    let timeout = config.heartbeat_timeout()?;
    let hub_config = config.hub_config(timeout);
    let auth_config = config.auth_config();
    let verifier = Verifier::new().map_err(|error| ServerError::Config(Box::new(error)))?;
    let listener = TcpListener::bind(config.listen_addr).await?;
    let exam = Arc::new(ExamService::open(&config.database_url).await?);
    exam.recover_online().await?;
    let (hub, manager) = hub::Manager::with_config(Arc::clone(&exam), hub_config);
    let router = api::router(api::AppState::new(
        exam,
        verifier,
        hub.clone(),
        timeout,
        auth_config,
    ));
    tracing::info!(listen_addr = %listener.local_addr()?, "server listening");

    let manager_task = tokio::spawn(manager.run());
    let scheduler_task = tokio::spawn(hub.clone().schedule());
    let server = axum::serve(listener, router).with_graceful_shutdown(shutdown_signal());
    tokio::pin!(manager_task);
    tokio::pin!(scheduler_task);

    tokio::select! {
        result = server => {
            scheduler_task.abort();
            let _ = tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, async {
                hub.shutdown().await;
                let _ = manager_task.await;
            })
            .await;
            result.map_err(ServerError::Io)
        }
        res = &mut manager_task => {
            scheduler_task.abort();
            match res {
                Ok(()) => Err(ServerError::Io(io::Error::other("connection manager stopped"))),
                Err(error) => Err(ServerError::Io(io::Error::other(error))),
            }
        }
        res = &mut scheduler_task => {
            let _ = tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, hub.shutdown()).await;
            let _ = tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, manager_task).await;
            match res {
                Ok(()) => Err(ServerError::Io(io::Error::other("command scheduler stopped"))),
                Err(error) => Err(ServerError::Io(io::Error::other(error))),
            }
        }
    }
}
