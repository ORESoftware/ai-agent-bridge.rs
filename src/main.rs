// Binary entrypoint: loads config from the environment, wires up the shared
// embedder + in-memory state, and runs the HTTP and TCP listeners concurrently
// until shutdown.

use std::sync::Arc;

use ai_agent_bridge::config::Config;
use ai_agent_bridge::embed::Embedder;
use ai_agent_bridge::state::AppState;
use ai_agent_bridge::{http, orchestration, policy, tcp, workflow_security};
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fiducia_telemetry::init("fiducia-ai-agent-bridge");

    let config = Config::from_env()?;
    let workflow_auth = workflow_security::WorkflowSecurity::from_env(
        config.api_auth_bearer.clone(),
        config.max_http_body_bytes,
    )?;
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
        config.max_embedding_response_bytes,
    );

    let state = build_state(config.clone(), embedder).await?;

    let http_addr = std::net::SocketAddr::new(config.host, config.http_port);
    let tcp_addr = std::net::SocketAddr::new(config.host, config.tcp_port);

    let http_listener = TcpListener::bind(http_addr).await?;
    let tcp_listener = TcpListener::bind(tcp_addr).await?;
    info!(%http_addr, %tcp_addr, "listening");

    let app = http::router(state.clone())
        .merge(orchestration::router(state.clone()))
        .merge(policy::router())
        .layer(axum::middleware::from_fn_with_state(
            workflow_auth,
            workflow_security::enforce,
        ));

    let mut server_tasks = JoinSet::new();
    server_tasks.spawn(async move {
        if let Err(e) = axum::serve(http_listener, app).await {
            warn!(error = %e, "http server exited");
        }
    });
    let tcp_state = state.clone();
    server_tasks.spawn(async move {
        if let Err(e) = tcp::serve(tcp_state, tcp_listener).await {
            warn!(error = %e, "tcp server exited");
        }
    });

    tokio::select! {
        _ = shutdown_signal() => info!("shutdown signal received"),
        result = server_tasks.join_next() => warn!(?result, "server task ended"),
    }
    server_tasks.abort_all();
    while server_tasks.join_next().await.is_some() {}
    state.flush_persistence().await;
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
    let state = AppState::new(config, embedder)?.with_db(db.clone());
    if let Some(db) = &db {
        match db.load_state(&state).await {
            Ok(counts) => info!(
                agents = counts.agents,
                channels = counts.channels,
                messages = counts.messages,
                context = counts.context,
                "restored durable state from postgres"
            ),
            Err(e) => warn!(error = %e, "state restore failed (schema not migrated yet?)"),
        }
    }
    Ok(state)
}

#[cfg(not(feature = "postgres"))]
async fn build_state(config: Config, embedder: Embedder) -> anyhow::Result<Arc<AppState>> {
    Ok(AppState::new(config, embedder)?)
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
