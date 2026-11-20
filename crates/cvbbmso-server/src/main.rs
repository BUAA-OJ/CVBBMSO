//! HTTP and WebSocket transport for the exam server.

#![forbid(unsafe_code)]

mod api;
mod hub;
mod socket;

use std::{error::Error, io, net::SocketAddr, process::ExitCode, sync::Arc, time::Duration};

use clap::{Parser, error::ErrorKind};
use cvbbmso_accoding::Verifier;
use cvbbmso_domain::{DomainError, ExamService};
use thiserror::Error;
use tokio::{net::TcpListener, time::Instant};

#[derive(Parser)]
#[command(about = "Run the cvbbmso exam server")]
struct ServerConfig {
    #[arg(long, env = "CVBBMSO_LISTEN_ADDR")]
    listen_addr: SocketAddr,
    #[arg(long, env = "CVBBMSO_DATABASE_URL", hide_env_values = true)]
    database_url: String,
    #[arg(long, env = "CVBBMSO_HEARTBEAT_TIMEOUT_SECS", default_value_t = 25, value_parser = clap::value_parser!(u64).range(1..))]
    heartbeat_timeout_secs: u64,
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
        ServerError::Database(_) => "database",
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

async fn serve(config: ServerConfig) -> Result<(), ServerError> {
    let timeout = config.heartbeat_timeout()?;
    let verifier = Verifier::new().map_err(|error| ServerError::Config(Box::new(error)))?;
    let exam = Arc::new(ExamService::open(&config.database_url).await?);
    exam.recover_online().await?;
    let listener = TcpListener::bind(config.listen_addr).await?;
    let (hub, manager) = hub::Manager::new(Arc::clone(&exam), timeout);
    let router = api::router(api::AppState::new(exam, verifier, hub.clone(), timeout));
    tracing::info!(listen_addr = %listener.local_addr()?, "server listening");
    tokio::select! {
        result = axum::serve(listener, router) => result.map_err(ServerError::Io),
        () = manager.run() => Err(ServerError::Io(io::Error::other("connection manager stopped"))),
        () = hub.schedule() => Err(ServerError::Io(io::Error::other("command scheduler stopped"))),
    }
}
