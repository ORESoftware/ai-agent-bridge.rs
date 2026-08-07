use std::collections::BTreeSet;
use std::net::IpAddr;

use reqwest::Url;

pub(crate) const INTERNAL_HTTP_HOSTS_ENV: &str =
    "AI_AGENT_RUNNER_BRIDGE_INTERNAL_HTTP_HOSTS";

pub(crate) fn validate_bridge_origin(
    url: &Url,
    bearer: Option<&str>,
    internal_http_hosts: Option<&str>,
) -> Result<(), String> {
    let host = url
        .host_str()
        .ok_or_else(|| "bridge URL requires a host".to_string())?
        .to_ascii_lowercase();
    let loopback = is_loopback_host(&host);

    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        "http" => {
            let allowlist = parse_internal_http_hosts(internal_http_hosts)?;
            if !allowlist.contains(&host) {
                return Err(format!(
                    "remote bridge HTTP host is not listed in {INTERNAL_HTTP_HOSTS_ENV}"
                ));
            }
        }
        _ => return Err("bridge URL scheme must be http or https".to_string()),
    }

    if !loopback && bearer.is_none() {
        return Err("remote bridge URLs require a bearer token".to_string());
    }
    Ok(())
}

fn parse_internal_http_hosts(raw: Option<&str>) -> Result<BTreeSet<String>, String> {
    let mut hosts = BTreeSet::new();
    let Some(raw) = raw else {
        return Ok(hosts);
    };

    for entry in raw.split(',') {
        let host = entry.trim().to_ascii_lowercase();
        if host.is_empty() {
            return Err(format!(
                "{INTERNAL_HTTP_HOSTS_ENV} contains an empty host"
            ));
        }
        if !is_kubernetes_service_fqdn(&host) {
            return Err(format!(
                "{INTERNAL_HTTP_HOSTS_ENV} entries must be exact *.svc.cluster.local DNS names"
            ));
        }
        hosts.insert(host);
    }
    Ok(hosts)
}

fn is_kubernetes_service_fqdn(host: &str) -> bool {
    if host.len() > 253 || !host.ends_with(".svc.cluster.local") {
        return false;
    }
    host.split('.').all(valid_dns_label)
}

fn valid_dns_label(label: &str) -> bool {
    if label.is_empty() || label.len() > 63 {
        return false;
    }
    let bytes = label.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(|character| character == '[' || character == ']')
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(value: &str) -> Url {
        Url::parse(value).unwrap()
    }

    #[test]
    fn loopback_http_requires_no_exception_or_bearer() {
        assert!(validate_bridge_origin(&url("http://127.0.0.1:8142/"), None, None).is_ok());
        assert!(validate_bridge_origin(&url("http://localhost:8142/"), None, None).is_ok());
    }

    #[test]
    fn remote_https_requires_a_bearer() {
        assert!(validate_bridge_origin(
            &url("https://bridge.example.com/"),
            Some("test-only"),
            None,
        )
        .is_ok());
        assert!(validate_bridge_origin(&url("https://bridge.example.com/"), None, None)
            .unwrap_err()
            .contains("bearer"));
    }

    #[test]
    fn kubernetes_http_requires_exact_host_and_bearer() {
        let bridge = url("http://dd-ai-agent-bridge.default.svc.cluster.local:8142/");
        let allowed = "dd-ai-agent-bridge.default.svc.cluster.local";
        assert!(validate_bridge_origin(&bridge, Some("test-only"), Some(allowed)).is_ok());
        assert!(validate_bridge_origin(&bridge, None, Some(allowed))
            .unwrap_err()
            .contains("bearer"));
        assert!(validate_bridge_origin(
            &bridge,
            Some("test-only"),
            Some("other.default.svc.cluster.local"),
        )
        .unwrap_err()
        .contains(INTERNAL_HTTP_HOSTS_ENV));
    }

    #[test]
    fn public_and_wildcard_http_exceptions_are_rejected() {
        assert!(validate_bridge_origin(
            &url("http://bridge.example.com/"),
            Some("test-only"),
            Some("bridge.example.com"),
        )
        .unwrap_err()
        .contains("svc.cluster.local"));
        assert!(validate_bridge_origin(
            &url("http://dd-ai-agent-bridge.default.svc.cluster.local/"),
            Some("test-only"),
            Some("*.default.svc.cluster.local"),
        )
        .unwrap_err()
        .contains("svc.cluster.local"));
    }

    #[test]
    fn malformed_allowlist_entries_fail_closed() {
        let bridge = url("http://dd-ai-agent-bridge.default.svc.cluster.local/");
        for raw in [
            "dd-ai-agent-bridge.default.svc.cluster.local,",
            "https://dd-ai-agent-bridge.default.svc.cluster.local",
            "dd-ai-agent-bridge.default.svc.cluster.local:8142",
            "-bridge.default.svc.cluster.local",
        ] {
            assert!(validate_bridge_origin(&bridge, Some("test-only"), Some(raw)).is_err());
        }
    }

    #[test]
    fn non_http_schemes_are_rejected() {
        assert!(validate_bridge_origin(&url("ftp://127.0.0.1/"), None, None)
            .unwrap_err()
            .contains("http or https"));
    }
}
