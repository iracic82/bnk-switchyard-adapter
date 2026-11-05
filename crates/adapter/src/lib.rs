//! Switchyard model selection in front of F5 BIG-IP Next for Kubernetes inference pools.
//!
//! Request path: external BNK Gateway -> this adapter -> internal BNK Gateway -> InferencePool
//! -> F5 EPP -> model server. The adapter chooses the logical model; BNK chooses the endpoint.

pub mod config;
pub mod engine;
pub mod extproc;
pub mod http;
pub mod live;
pub mod metrics;
pub mod registry;
pub mod telemetry;
pub mod tenancy;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::http::HeaderName;
use tokio::net::TcpListener;

use crate::config::Config;
use crate::engine::Engine;
use crate::http::AppState;
use crate::metrics::Metrics;
use crate::registry::Registry;
use crate::tenancy::Tenants;

pub fn build_state(config: &Config) -> anyhow::Result<Arc<AppState>> {
    let hop_header = HeaderName::from_bytes(config.server.hop_header.as_bytes())?;
    Ok(Arc::new(AppState {
        engine: Engine {
            registry: Arc::new(Registry::build(config)?),
            metrics: Arc::new(Metrics::new()),
        },
        tenants: Tenants::new(config),
        request_timeout: Duration::from_secs(config.server.request_timeout_secs),
        routing_timeout: Duration::from_secs(config.server.routing_timeout_secs),
        hop_header,
        ready: AtomicBool::new(false),
        live: config
            .server
            .live_view
            .then(|| live::Live::new(config.server.live_prompt_preview_chars)),
    }))
}

/// Serves until `shutdown` resolves.
pub async fn serve(
    config: &Config,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let state = build_state(config)?;
    let app = http::router(Arc::clone(&state), config.server.max_body_bytes);
    state.ready.store(true, Ordering::Relaxed);
    tracing::info!(addr = %listener.local_addr()?, "bnk-switchyard-adapter listening");
    let drain = Duration::from_secs(config.server.shutdown_drain_secs);
    let draining = Arc::clone(&state);
    let shutdown = async move {
        shutdown.await;
        // Endpoints take a moment to drop a terminating pod; keep serving until they have.
        draining.ready.store(false, Ordering::Relaxed);
        tracing::info!(drain_secs = drain.as_secs(), "draining before shutdown");
        tokio::time::sleep(drain).await;
    };
    if let Some(ext) = &config.ext_proc {
        let addr: std::net::SocketAddr = ext.listen.parse()?;
        let service = extproc::ExtProc::server(Arc::clone(&state), ext.tenant.clone());
        tracing::info!(%addr, tenant = %ext.tenant, "ext_proc server listening");
        tokio::spawn(async move {
            if let Err(error) = tonic::transport::Server::builder()
                .add_service(service)
                .serve(addr)
                .await
            {
                tracing::error!(%error, "ext_proc server stopped");
            }
        });
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}
