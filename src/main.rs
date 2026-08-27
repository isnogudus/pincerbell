//! pincerbell -- an independent Matrix Push Gateway.
//!
//! Implements `POST /_matrix/push/v1/notify` per
//! <https://spec.matrix.org/latest/push-gateway-api/>.

mod api;
mod apns;
mod config;
mod dedup;
mod fcm;
mod gateway;
mod poller;
mod provider;
mod queue;
mod webpush;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use config::Config;
use gateway::AppState;
use tracing_subscriber::EnvFilter;

fn config_path() -> Result<PathBuf, String> {
    let mut path: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => match args.next() {
                Some(p) => path = Some(p.into()),
                None => return Err(format!("{arg} requires a path argument")),
            },
            "-V" | "--version" => {
                println!("pincerbell {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => {
                return Err(format!(
                    "unknown argument: {other}\nusage: pincerbell [-c CONFIG]"
                ));
            }
        }
    }
    Ok(path
        .or_else(|| std::env::var("PINCERBELL_CONF").ok().map(PathBuf::from))
        .unwrap_or_else(|| "pincerbell.toml".into()))
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let path = match config_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let config = match Config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("config: {e}");
            return ExitCode::FAILURE;
        }
    };

    tracing::info!(
        listen = %config.listen,
        apps = config.apps.len(),
        reject_unknown_apps = config.reject_unknown_apps,
        queue = config.queue.is_some(),
        poll_upstreams = config.poll.len(),
        "starting pincerbell {}",
        env!("CARGO_PKG_VERSION"),
    );

    let upstreams: Vec<poller::Upstream> = match config
        .poll
        .iter()
        .map(|p| poller::Upstream::load(p, config.proxy.as_deref()))
        .collect()
    {
        Ok(u) => u,
        Err(e) => {
            tracing::error!("config: {e}");
            return ExitCode::FAILURE;
        }
    };

    let listen = config.listen.clone();
    let state = match AppState::new(config) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("config: {e}");
            return ExitCode::FAILURE;
        }
    };
    let state = Arc::new(state);
    for up in upstreams {
        tokio::spawn(poller::run(state.clone(), up));
    }
    let app = gateway::router(state);
    let listener = match tokio::net::TcpListener::bind(&listen).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("bind {listen}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down");
    };
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
    {
        tracing::error!("server: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
