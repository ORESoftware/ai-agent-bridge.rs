// Dedicated Slack slash-command process for ORESoftware agent work.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    ai_agent_bridge::slack_commands::run().await
}
