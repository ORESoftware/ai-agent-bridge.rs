//! Provider worker process: polls bridge workflows, executes configured model
//! providers, submits results, and coordinates fenced file leases.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Keep the guard alive until the worker exits so metrics and traces flush.
    let _telemetry = fiducia_telemetry::init("fiducia-ai-agent-runner");
    ai_agent_bridge::runner::run_from_env().await
}
