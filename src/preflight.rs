//! Credential-safe LAN connectivity diagnostics for bridge operators.
//!
//! The report intentionally contains only coarse probe states and HTTP status
//! codes. It never retains request headers, bearer values, response bodies, or
//! raw transport errors, so printing the report cannot disclose credentials.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::Duration;

use reqwest::header::{HeaderValue, AUTHORIZATION};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VpnObservation {
    Clear,
    HelperOrFilterPresent,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum RouteProbe {
    Reachable,
    NameResolutionFailed,
    Unreachable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TcpProbe {
    Connected,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "status")]
pub enum HttpProbe {
    Status(u16),
    TransportFailed,
}

impl HttpProbe {
    fn is_success(self) -> bool {
        matches!(self, Self::Status(status) if (200..300).contains(&status))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "status")]
pub enum AuthProbe {
    Accepted,
    MissingBearer,
    InvalidBearer,
    Rejected(u16),
    EndpointStatus(u16),
    TransportFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ProbeResults {
    pub vpn: VpnObservation,
    pub route: RouteProbe,
    pub tcp: TcpProbe,
    pub health: HttpProbe,
    pub readiness: HttpProbe,
    pub auth: AuthProbe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Diagnosis {
    Ready,
    LingeringVpnHelperOrFilter,
    NameResolutionFailure,
    RouteFailure,
    TcpFailure,
    HealthTransportFailure,
    HealthStatusFailure,
    ReadinessTransportFailure,
    ReadinessStatusFailure,
    BearerMissing,
    BearerInvalid,
    BearerRejected,
    AuthEndpointFailure,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PreflightReport {
    pub ok: bool,
    pub endpoint: String,
    pub diagnosis: Diagnosis,
    pub message: &'static str,
    pub probes: ProbeResults,
}

/// Classify injected probe observations in strict network-layer order. A VPN
/// helper/filter is only blamed when the route or TCP path is broken; merely
/// having VPN software installed does not turn a healthy bridge red.
pub fn classify(endpoint: String, probes: ProbeResults) -> PreflightReport {
    let (diagnosis, message) = match probes.route {
        RouteProbe::NameResolutionFailed => (
            Diagnosis::NameResolutionFailure,
            "bridge host name could not be resolved",
        ),
        RouteProbe::Unreachable if probes.vpn == VpnObservation::HelperOrFilterPresent => (
            Diagnosis::LingeringVpnHelperOrFilter,
            "a VPN helper or network filter is present and the LAN route is unavailable",
        ),
        RouteProbe::Unreachable => (
            Diagnosis::RouteFailure,
            "no usable network route exists to the bridge host",
        ),
        RouteProbe::Reachable
            if probes.tcp == TcpProbe::Failed
                && probes.vpn == VpnObservation::HelperOrFilterPresent =>
        {
            (
                Diagnosis::LingeringVpnHelperOrFilter,
                "a VPN helper or network filter is present and the TCP path is blocked",
            )
        }
        RouteProbe::Reachable if probes.tcp == TcpProbe::Failed => (
            Diagnosis::TcpFailure,
            "the host is routable but the bridge TCP port could not be reached",
        ),
        RouteProbe::Reachable => match probes.health {
            HttpProbe::TransportFailed => (
                Diagnosis::HealthTransportFailure,
                "TCP connected but the HTTP health request failed in transport",
            ),
            health if !health.is_success() => (
                Diagnosis::HealthStatusFailure,
                "the bridge health endpoint returned a non-success status",
            ),
            _ => match probes.readiness {
                HttpProbe::TransportFailed => (
                    Diagnosis::ReadinessTransportFailure,
                    "health passed but the readiness request failed in transport",
                ),
                readiness if !readiness.is_success() => (
                    Diagnosis::ReadinessStatusFailure,
                    "health passed but the readiness endpoint returned a non-success status",
                ),
                _ => match probes.auth {
                    AuthProbe::Accepted => (Diagnosis::Ready, "LAN bridge preflight passed"),
                    AuthProbe::MissingBearer => (
                        Diagnosis::BearerMissing,
                        "health passed but no bearer was provided for an authenticated probe",
                    ),
                    AuthProbe::InvalidBearer => (
                        Diagnosis::BearerInvalid,
                        "the supplied bearer cannot be encoded as an HTTP authorization header",
                    ),
                    AuthProbe::Rejected(_) => (
                        Diagnosis::BearerRejected,
                        "the bridge rejected the authenticated probe",
                    ),
                    AuthProbe::EndpointStatus(_) | AuthProbe::TransportFailed => (
                        Diagnosis::AuthEndpointFailure,
                        "health passed but the authenticated bridge endpoint failed",
                    ),
                },
            },
        },
    };
    PreflightReport {
        ok: diagnosis == Diagnosis::Ready,
        endpoint,
        diagnosis,
        message,
        probes,
    }
}

pub async fn run(
    base_url: &str,
    tcp_port: u16,
    bearer: Option<&str>,
    timeout: Duration,
) -> anyhow::Result<PreflightReport> {
    let base =
        reqwest::Url::parse(base_url).map_err(|_| anyhow::anyhow!("bridge base URL is invalid"))?;
    anyhow::ensure!(
        matches!(base.scheme(), "http" | "https"),
        "bridge base URL must use http or https"
    );
    anyhow::ensure!(
        base.username().is_empty() && base.password().is_none(),
        "bridge credentials must come from the bearer environment variable, not the URL"
    );
    let host = base
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("bridge base URL has no host"))?;
    let endpoint = base.origin().ascii_serialization();
    let vpn = tokio::task::spawn_blocking(detect_vpn_observation)
        .await
        .unwrap_or(VpnObservation::Unknown);

    let resolved = tokio::net::lookup_host((host, tcp_port)).await;
    let (route, tcp) = match resolved {
        Err(_) => (RouteProbe::NameResolutionFailed, TcpProbe::Failed),
        Ok(mut addresses) => match addresses.next() {
            None => (RouteProbe::NameResolutionFailed, TcpProbe::Failed),
            Some(address) => {
                let route = if route_exists(address) {
                    RouteProbe::Reachable
                } else {
                    RouteProbe::Unreachable
                };
                let tcp = if route == RouteProbe::Reachable
                    && matches!(
                        tokio::time::timeout(timeout, tokio::net::TcpStream::connect(address))
                            .await,
                        Ok(Ok(_))
                    ) {
                    TcpProbe::Connected
                } else {
                    TcpProbe::Failed
                };
                (route, tcp)
            }
        },
    };

    let client = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let health = http_probe(&client, endpoint_url(&base, "/healthz")).await;
    let readiness = http_probe(&client, endpoint_url(&base, "/readyz")).await;
    let auth = auth_probe(
        &client,
        endpoint_url(&base, "/agents"),
        bearer.filter(|value| !value.is_empty()),
    )
    .await;

    Ok(classify(
        endpoint,
        ProbeResults {
            vpn,
            route,
            tcp,
            health,
            readiness,
            auth,
        },
    ))
}

fn endpoint_url(base: &reqwest::Url, path: &str) -> reqwest::Url {
    let mut url = base.clone();
    url.set_path(path);
    url.set_query(None);
    url.set_fragment(None);
    url
}

fn route_exists(target: SocketAddr) -> bool {
    let bind_address = match target.ip() {
        IpAddr::V4(_) => "0.0.0.0:0",
        IpAddr::V6(_) => "[::]:0",
    };
    UdpSocket::bind(bind_address)
        .and_then(|socket| socket.connect(target))
        .is_ok()
}

async fn http_probe(client: &reqwest::Client, url: reqwest::Url) -> HttpProbe {
    match client.get(url).send().await {
        Ok(response) => HttpProbe::Status(response.status().as_u16()),
        Err(_) => HttpProbe::TransportFailed,
    }
}

async fn auth_probe(
    client: &reqwest::Client,
    url: reqwest::Url,
    bearer: Option<&str>,
) -> AuthProbe {
    let Some(bearer) = bearer else {
        return AuthProbe::MissingBearer;
    };
    let Ok(value) = HeaderValue::from_str(&format!("Bearer {bearer}")) else {
        return AuthProbe::InvalidBearer;
    };
    match client.get(url).header(AUTHORIZATION, value).send().await {
        Ok(response) if response.status().is_success() => AuthProbe::Accepted,
        Ok(response) if matches!(response.status().as_u16(), 401 | 403) => {
            AuthProbe::Rejected(response.status().as_u16())
        }
        Ok(response) => AuthProbe::EndpointStatus(response.status().as_u16()),
        Err(_) => AuthProbe::TransportFailed,
    }
}

#[cfg(target_os = "macos")]
fn detect_vpn_observation() -> VpnObservation {
    const NEEDLES: [&str; 6] = [
        "nordvpn",
        "nordsecurity",
        "nordlynx",
        "wireguard",
        "openvpn",
        "tailscale",
    ];
    let helper_present = std::process::Command::new("/usr/bin/pgrep")
        .args([
            "-ifl",
            "nordvpn|nordsecurity|nordlynx|wireguard|openvpn|tailscale",
        ])
        .output()
        .is_ok_and(|output| output.status.success());
    let filter_present = std::process::Command::new("/usr/bin/systemextensionsctl")
        .arg("list")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).to_lowercase())
        .is_some_and(|output| NEEDLES.iter().any(|needle| output.contains(needle)));
    if helper_present || filter_present {
        VpnObservation::HelperOrFilterPresent
    } else {
        VpnObservation::Clear
    }
}

#[cfg(not(target_os = "macos"))]
fn detect_vpn_observation() -> VpnObservation {
    VpnObservation::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> ProbeResults {
        ProbeResults {
            vpn: VpnObservation::Clear,
            route: RouteProbe::Reachable,
            tcp: TcpProbe::Connected,
            health: HttpProbe::Status(200),
            readiness: HttpProbe::Status(200),
            auth: AuthProbe::Accepted,
        }
    }

    #[test]
    fn injected_results_distinguish_vpn_interference_from_plain_route_failure() {
        let mut probes = healthy();
        probes.route = RouteProbe::Unreachable;
        assert_eq!(
            classify("http://bridge:8142".into(), probes).diagnosis,
            Diagnosis::RouteFailure
        );

        probes.vpn = VpnObservation::HelperOrFilterPresent;
        assert_eq!(
            classify("http://bridge:8142".into(), probes).diagnosis,
            Diagnosis::LingeringVpnHelperOrFilter
        );
    }

    #[test]
    fn injected_results_keep_tcp_health_readiness_and_auth_failures_distinct() {
        let mut probes = healthy();
        probes.tcp = TcpProbe::Failed;
        assert_eq!(
            classify("http://bridge:8142".into(), probes).diagnosis,
            Diagnosis::TcpFailure
        );

        probes = healthy();
        probes.health = HttpProbe::Status(503);
        assert_eq!(
            classify("http://bridge:8142".into(), probes).diagnosis,
            Diagnosis::HealthStatusFailure
        );

        probes = healthy();
        probes.readiness = HttpProbe::TransportFailed;
        assert_eq!(
            classify("http://bridge:8142".into(), probes).diagnosis,
            Diagnosis::ReadinessTransportFailure
        );

        probes = healthy();
        probes.auth = AuthProbe::Rejected(401);
        assert_eq!(
            classify("http://bridge:8142".into(), probes).diagnosis,
            Diagnosis::BearerRejected
        );
    }

    #[test]
    fn serialized_report_cannot_contain_a_bearer_value() {
        let secret = "top-secret-bearer";
        let report = classify("http://bridge:8142".into(), healthy());
        let serialized = serde_json::to_string(&report).expect("serialize report");
        assert!(!serialized.contains(secret));
        assert!(!serialized.to_ascii_lowercase().contains("authorization"));
    }
}
