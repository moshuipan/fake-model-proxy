//! fakemodelproxy 入口：装配配置、存储、后端与管理界面。

use fakemodelproxy::records::RecordLog;
use fakemodelproxy::tasks::TaskRegistry;
use fakemodelproxy::{backend, build_router, config, state, store};
use std::sync::{Arc, RwLock};
use tracing_subscriber::EnvFilter;

#[tokio::main]
/// 装配持久存储并启动本地服务。
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("FMP_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Arc::new(config::Config::from_env()?);
    if !config.is_loopback() && !config.requires_api_key() {
        // 非回环监听且没有调用 key：任何能连到端口的人都能用你的额度。
        eprintln!(
            "refusing to start: FMP_LISTEN={} is not loopback but FMP_API_KEY is empty",
            config.listen
        );
        std::process::exit(1);
    }

    let store = store::Store::open(&config.database_path()).await?;
    let registry = Arc::new(backend::Registry::new());
    let records = Arc::new(RecordLog::open(
        &config.records_path(),
        config.record_capacity,
    )?);
    let tasks = Arc::new(RwLock::new(TaskRegistry::new()));
    let usage = Arc::new(fakemodelproxy::usage::UsageLedger::new(store.clone()));
    usage.load_today().await?;
    let router = Arc::new(fakemodelproxy::routing::Router {
        usage: Some(usage.clone()),
        ..Default::default()
    });
    let state = state::AppState::new(
        config.clone(),
        store,
        registry,
        records,
        tasks,
        router,
        usage,
    );
    state.reload_backends().await?;
    fakemodelproxy::tasks::scheduler::spawn(state.clone());
    let quota_job = fakemodelproxy::accounts::quota_refresh::spawn(state.clone());
    let model_job = fakemodelproxy::routing::planner::spawn_refresh(state.clone());
    let app = build_router(state.clone());

    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    tracing::info!(
        listen = %config.listen,
        data_dir = %config.data_dir.display(),
        auth = config.requires_api_key(),
        max_request_body_bytes = config.max_request_body_bytes,
        record_capacity = config.record_capacity,
        records_path = %config.records_path().display(),
        "fakemodelproxy started"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    state.quota_refresh.shutdown.cancel();
    model_job.await?;
    if let Some(job) = quota_job {
        job.await?;
    }
    Ok(())
}

/// 等待 Ctrl+C 或 SIGTERM，触发优雅退出。
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "failed to listen for ctrl-c");
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "failed to listen for SIGTERM");
                // 保持 future 挂起，避免误导退出流程。
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
