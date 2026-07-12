// Binary entrypoint: loads config from the environment, wires up the shared
// embedder + in-memory state, and runs the HTTP and TCP listeners concurrently
// until shutdown.

use std::sync::Arc;

use ai_agent_bridge::config::Config;
use ai_agent_bridge::embed::Embedder;
use ai_agent_bridge::state::AppState;
use ai_agent_bridge::{http, tcp};
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config = Config::from_env()?;
    info!(
        http_port = config.http_port,
        tcp_port = config.tcp_port,
        embeddings = config.embeddings_url.as_deref().unwrap_or("local"),
        max_members = ai_agent_bridge::config::MAX_MEMBERS,
        "starting ai-agent-bridge"
    );

    let embedder = Embedder::new(
        config.embed_dim,
        config.embeddings_url.clone(),
        config.embeddings_model.clone(),
        config.embeddings_bearer.clone(),
    );

    let state = build_state(config.clone(), embedder).await?;

    let http_addr = std::net::SocketAddr::new(config.host, config.http_port);
    let tcp_addr = std::net::SocketAddr::new(config.host, config.tcp_port);

    let http_listener = TcpListener::bind(http_addr).await?;
    let tcp_listener = TcpListener::bind(tcp_addr).await?;
    info!(%http_addr, %tcp_addr, "listening");

    let app = http::router(state.clone());

    let http_task = tokio::spawn(async move {
        if let Err(e) = axum::serve(http_listener, app).await {
            warn!(error = %e, "http server exited");
        }
    });
    let tcp_task = tokio::spawn(async move {
        if let Err(e) = tcp::serve(state, tcp_listener).await {
            warn!(error = %e, "tcp server exited");
        }
    });

    tokio::select! {
        _ = shutdown_signal() => info!("shutdown signal received"),
        _ = http_task => warn!("http task ended"),
        _ = tcp_task => warn!("tcp task ended"),
    }
    Ok(())
}

#[cfg(feature = "postgres")]
async fn build_state(config: Config, embedder: Embedder) -> anyhow::Result<Arc<AppState>> {
    let db = match &config.database_url {
        Some(url) => match ai_agent_bridge::db::Db::connect(url).await {
            Ok(db) => {
                info!("postgres connected — persistence enabled (best-effort)");
                Some(db)
            }
            Err(e) => {
                warn!(error = %e, "postgres connect failed — running in-memory only");
                None
            }
        },
        None => {
            info!("no DATABASE_URL — running in-memory only");
            None
        }
    };
    let state = AppState::new(config, embedder).with_db(db.clone());
    if let Some(db) = &db {
        match db.load_channels(&state).await {
            Ok(n) => info!(channels = n, "restored channels from postgres"),
            Err(e) => warn!(error = %e, "channel restore failed (schema not migrated yet?)"),
        }
    }
    Ok(state)
}

#[cfg(not(feature = "postgres"))]
async fn build_state(config: Config, embedder: Embedder) -> anyhow::Result<Arc<AppState>> {
    Ok(AppState::new(config, embedder))
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,ai_agent_bridge=info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    // JSON logs in-cluster (matches the other services), pretty logs locally.
    if std::env::var("LOG_FORMAT")
        .map(|v| v == "json")
        .unwrap_or(false)
    {
        builder.json().init();
    } else {
        builder.init();
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
