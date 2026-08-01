// Dedicated Slack ingress/egress process for authenticated dual-model workflows.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    ai_agent_bridge::slack_bridge::run().await
}
