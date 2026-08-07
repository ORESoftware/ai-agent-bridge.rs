pub async fn enforce(
    State(security): State<Arc<WorkflowSecurity>>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let inspect_context = is_context_write(&method, &path);
    let rule = access_rule(&method, &path);

    let (parts, body) = request.into_parts();
    let body_bytes = match to_bytes(body, security.max_body_bytes).await {
        Ok(bytes) => bytes,
        Err(_) => return error_response(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
    };
    let body_json = if body_bytes.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(&body_bytes).ok()
    };

    if inspect_context && contains_reserved_context_key(body_json.as_ref()) {
        return error_response(StatusCode::FORBIDDEN, "reserved_context_namespace");
    }

    let mut request = Request::from_parts(parts, Body::from(body_bytes));
    if !security.scoped_mode() || is_public_path(&path) {
        return next.run(request).await;
    }

    let presented = bearer_token(request.headers());
    if presented
        .as_deref()
        .is_some_and(|token| security.is_admin_token(token))
    {
        return next.run(request).await;
    }

    let Some(rule) = rule else {
        return error_response(StatusCode::FORBIDDEN, "scope_denied");
    };
    let Some(identity) = presented
        .as_deref()
        .and_then(|token| security.authenticate(token))
    else {
        return error_response(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    if !identity.has_scope(rule.scope) {
        return error_response(StatusCode::FORBIDDEN, "scope_denied");
    }
    if let Some(field) = rule.identity_field {
        let matches = body_json
            .as_ref()
            .and_then(|value| value.get(field))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .is_some_and(|value| value == identity.agent_key);
        if !matches {
            return error_response(StatusCode::FORBIDDEN, "adapter_identity_mismatch");
        }
    }

    request.extensions_mut().insert(identity);
    if let Some(global) = &security.global_bearer {
        let Ok(value) = HeaderValue::from_str(&format!("Bearer {global}")) else {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "auth_configuration_error",
            );
        };
        request.headers_mut().insert(header::AUTHORIZATION, value);
    }
    next.run(request).await
}

fn bearer_token(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string)
}

fn access_rule(method: &Method, path: &str) -> Option<AccessRule> {
    let rule = match (method, path) {
        (&Method::POST, "/blind-workflows") => AccessRule {
            scope: "workflow:create",
            identity_field: Some("created_by"),
        },
        (&Method::POST, path)
            if path.starts_with("/blind-workflows/") && path.ends_with("/submissions") =>
        {
            AccessRule {
                scope: "workflow:submit",
                identity_field: Some("agent_key"),
            }
        }
        (&Method::POST, path)
            if path.starts_with("/blind-workflows/") && path.ends_with("/reveal") =>
        {
            AccessRule {
                scope: "workflow:read",
                identity_field: None,
            }
        }
        (&Method::GET, path) if path.starts_with("/blind-workflows/") => AccessRule {
            scope: "workflow:read",
            identity_field: None,
        },
        (&Method::POST, "/workflows") => AccessRule {
            scope: "workflow:create",
            identity_field: Some("created_by"),
        },
        (&Method::GET, "/workflows") => AccessRule {
            scope: "workflow:read",
            identity_field: None,
        },
        (&Method::POST, path)
            if path.starts_with("/workflows/") && path.ends_with("/admission/usage") =>
        {
            AccessRule {
                scope: "workflow:usage",
                identity_field: Some("updated_by"),
            }
        }
        (&Method::POST, path)
            if path.starts_with("/workflows/")
                && (path.ends_with("/admission/complete")
                    || path.ends_with("/admission/cancel")) =>
        {
            AccessRule {
                scope: "workflow:usage",
                identity_field: Some("updated_by"),
            }
        }
        (&Method::POST, path)
            if path.starts_with("/workflows/") && path.ends_with("/admission") =>
        {
            AccessRule {
                scope: "workflow:admit",
                identity_field: Some("requested_by"),
            }
        }
        (&Method::POST, path)
            if path.starts_with("/workflows/") && path.ends_with("/submissions") =>
        {
            AccessRule {
                scope: "workflow:submit",
                identity_field: Some("agent_key"),
            }
        }
        (&Method::GET, path) if path.starts_with("/workflows/") => AccessRule {
            scope: "workflow:read",
            identity_field: None,
        },
        (&Method::POST, "/agents/register") => AccessRule {
            scope: "agent:register",
            identity_field: Some("agent_key"),
        },
        (&Method::GET, "/agents") | (&Method::GET, "/agents/by-file") => AccessRule {
            scope: "agent:read",
            identity_field: None,
        },
        (&Method::POST, "/channels") => AccessRule {
            scope: "channel:create",
            identity_field: Some("created_by"),
        },
        (&Method::POST, "/channels/resolve") => AccessRule {
            scope: "channel:create",
            identity_field: Some("created_by"),
        },
        (&Method::GET, "/channels") | (&Method::POST, "/channels/search") => AccessRule {
            scope: "channel:read",
            identity_field: None,
        },
        (&Method::GET, path)
            if path.starts_with("/channels/")
                && !path.ends_with("/context")
                && !path.ends_with("/messages") =>
        {
            AccessRule {
                scope: "channel:read",
                identity_field: None,
            }
        }
        (&Method::GET, path) if path.starts_with("/channels/") && path.ends_with("/context") => {
            AccessRule {
                scope: "context:read",
                identity_field: None,
            }
        }
        (method, path)
            if matches!(method, &Method::POST | &Method::PUT)
                && path.starts_with("/channels/")
                && path.ends_with("/context") =>
        {
            AccessRule {
                scope: "context:write",
                identity_field: Some("updated_by"),
            }
        }
        (&Method::POST, path) if path.starts_with("/channels/") && path.ends_with("/messages") => {
            AccessRule {
                scope: "channel:post",
                identity_field: Some("from"),
            }
        }
        (&Method::GET, path) if path.starts_with("/channels/") && path.ends_with("/messages") => {
            AccessRule {
                scope: "channel:read",
                identity_field: None,
            }
        }
        (&Method::POST, path)
            if path.starts_with("/channels/")
                && (path.ends_with("/join") || path.ends_with("/leave")) =>
        {
            AccessRule {
                scope: "channel:join",
                identity_field: Some("agent_key"),
            }
        }
        (&Method::POST, path) if path.starts_with("/file-leases") => AccessRule {
            scope: "lease:operate",
            identity_field: Some("agent_key"),
        },
        _ => return None,
    };
    Some(rule)
}

fn is_context_write(method: &Method, path: &str) -> bool {
    matches!(method, &Method::POST | &Method::PUT)
        && path.starts_with("/channels/")
        && path.ends_with("/context")
}

fn contains_reserved_context_key(body: Option<&serde_json::Value>) -> bool {
    body.and_then(|value| value.get("key"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .is_some_and(|key| {
            RESERVED_CONTEXT_PREFIXES
                .iter()
                .any(|prefix| key.starts_with(prefix))
        })
}

fn is_public_path(path: &str) -> bool {
    matches!(
        path,
        "/" | "/health" | "/healthz" | "/readyz" | "/metrics" | "/.well-known/agent-pontifex"
    )
}

fn error_response(status: StatusCode, error: &'static str) -> Response {
    (status, Json(json!({ "ok": false, "error": error }))).into_response()
}

#[cfg(test)]
mod agent_pontifex_discovery_tests {
    use super::is_public_path;

    #[test]
    fn agent_pontifex_discovery_is_public() {
        assert!(is_public_path("/.well-known/agent-pontifex"));
    }
}
