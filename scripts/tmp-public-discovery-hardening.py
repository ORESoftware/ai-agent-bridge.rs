#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    target = Path(path)
    text = target.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: {label}: expected one replacement target, found {count}")
    target.write_text(text.replace(old, new, 1), encoding="utf-8")


replace_once(
    "src/workflow_security/middleware.rs",
    '''fn is_public_path(path: &str) -> bool {
    matches!(path, "/" | "/health" | "/healthz" | "/readyz" | "/metrics")
}

fn error_response(status: StatusCode, error: &'static str) -> Response {
    (status, Json(json!({ "ok": false, "error": error }))).into_response()
}
''',
    '''fn is_public_path(path: &str) -> bool {
    matches!(
        path,
        "/"
            | "/health"
            | "/healthz"
            | "/readyz"
            | "/metrics"
            | "/.well-known/agent-pontifex"
    )
}

fn error_response(status: StatusCode, error: &'static str) -> Response {
    (status, Json(json!({ "ok": false, "error": error }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::is_public_path;

    #[test]
    fn agent_pontifex_discovery_is_public() {
        assert!(is_public_path("/.well-known/agent-pontifex"));
    }
}
''',
    "public auth allow-list",
)

replace_once(
    "src/main.rs",
    '''        "/" | "/health" | "/healthz" | "/readyz" | "/metrics"
''',
    '''        "/"
            | "/health"
            | "/healthz"
            | "/readyz"
            | "/metrics"
            | "/.well-known/agent-pontifex"
''',
    "admission bypass allow-list",
)

sdk_path = "sdk/agent-pontifex-sdk/src/lib.rs"
replace_once(
    sdk_path,
    "self.decode(self.request(Method::GET, url)).await?",
    "self.decode(self.public_request(Method::GET, url)).await?",
    "credential-free discovery request",
)
replace_once(
    sdk_path,
    '''    fn request(&self, method: Method, url: Url) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .request(method, url)
            .header(ACCEPT, "application/json");
        if let Some(authorization) = &self.authorization {
            request = request.header(AUTHORIZATION, authorization.clone());
        }
        request
    }
''',
    '''    fn public_request(&self, method: Method, url: Url) -> reqwest::RequestBuilder {
        self.http
            .request(method, url)
            .header(ACCEPT, "application/json")
    }

    fn request(&self, method: Method, url: Url) -> reqwest::RequestBuilder {
        let mut request = self.public_request(method, url);
        if let Some(authorization) = &self.authorization {
            request = request.header(AUTHORIZATION, authorization.clone());
        }
        request
    }
''',
    "public request builder",
)
replace_once(
    sdk_path,
    '''    #[test]
    fn dynamic_identifiers_are_encoded_as_single_path_segments() {
''',
    '''    #[test]
    fn discovery_request_does_not_attach_application_credentials() {
        let client = Client::new("https://example.com")
            .unwrap()
            .with_bearer("application-secret")
            .unwrap();
        let url = client.endpoint(&DISCOVERY_PATH_SEGMENTS).unwrap();
        let request = client.public_request(Method::GET, url).build().unwrap();
        assert!(request.headers().get(AUTHORIZATION).is_none());
    }

    #[test]
    fn dynamic_identifiers_are_encoded_as_single_path_segments() {
''',
    "credential regression test",
)
