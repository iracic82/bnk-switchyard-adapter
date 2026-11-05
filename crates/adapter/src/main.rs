use bnk_switchyard_adapter::{config::Config, serve, telemetry::Telemetry};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let telemetry = Telemetry::init()?;
    let path = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("SWITCHYARD_ADAPTER_CONFIG").ok())
        .unwrap_or_else(|| "/etc/switchyard-adapter/adapter.toml".into());
    let config = Config::load(&path)?;
    let listener = tokio::net::TcpListener::bind(&config.server.listen).await?;
    let result = serve(&config, listener, shutdown_signal()).await;
    telemetry.shutdown();
    result
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutting down");
}
