use std::time::Duration;

const HELP: &str = r#"Credential-safe LAN preflight for fiducia-ai-agent-bridge

Usage:
  fiducia-ai-agent-bridge-preflight [--base-url URL] [--tcp-port PORT]
                                    [--timeout-seconds SECONDS]

Environment:
  FIDUCIA_BRIDGE_BASE_URL             default http://127.0.0.1:8142
  FIDUCIA_BRIDGE_TCP_PORT             default 8143
  FIDUCIA_BRIDGE_PREFLIGHT_BEARER     preferred bearer source
  API_AUTH_BEARER                     fallback bearer source

The bearer is intentionally unavailable as a command-line argument so it does
not appear in shell history or process listings. Reports never contain request
headers, response bodies, bearer values, or raw transport errors.
"#;

struct Args {
    base_url: String,
    tcp_port: u16,
    timeout: Duration,
}

fn parse_args() -> anyhow::Result<Option<Args>> {
    let mut base_url =
        std::env::var("FIDUCIA_BRIDGE_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8142".into());
    let mut tcp_port = std::env::var("FIDUCIA_BRIDGE_TCP_PORT")
        .ok()
        .map(|value| value.parse::<u16>())
        .transpose()
        .map_err(|_| anyhow::anyhow!("FIDUCIA_BRIDGE_TCP_PORT is invalid"))?
        .unwrap_or(8143);
    let mut timeout_seconds = 5_u64;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--help" | "-h" => return Ok(None),
            "--base-url" => {
                base_url = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--base-url requires a value"))?;
            }
            "--tcp-port" => {
                tcp_port = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--tcp-port requires a value"))?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--tcp-port is invalid"))?;
            }
            "--timeout-seconds" => {
                timeout_seconds = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--timeout-seconds requires a value"))?
                    .parse::<u64>()
                    .map_err(|_| anyhow::anyhow!("--timeout-seconds is invalid"))?;
                anyhow::ensure!(
                    (1..=60).contains(&timeout_seconds),
                    "--timeout-seconds must be between 1 and 60"
                );
            }
            _ => anyhow::bail!("unsupported argument; run with --help"),
        }
    }
    Ok(Some(Args {
        base_url,
        tcp_port,
        timeout: Duration::from_secs(timeout_seconds),
    }))
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => {
            print!("{HELP}");
            return;
        }
        Err(error) => {
            eprintln!("preflight configuration error: {error}");
            std::process::exit(2);
        }
    };
    let bearer = std::env::var("FIDUCIA_BRIDGE_PREFLIGHT_BEARER")
        .ok()
        .or_else(|| std::env::var("API_AUTH_BEARER").ok());
    match ai_agent_bridge::preflight::run(
        &args.base_url,
        args.tcp_port,
        bearer.as_deref(),
        args.timeout,
    )
    .await
    {
        Ok(report) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).expect("preflight report serializes")
            );
            if !report.ok {
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("preflight configuration error: {error}");
            std::process::exit(2);
        }
    }
}
