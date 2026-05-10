//! Bearer-token auth challenge handling.
//!
//! OCI registries respond to anonymous requests at `/v2/...` with HTTP
//! 401 + a `WWW-Authenticate: Bearer realm=...,service=...,scope=...`
//! header. The client hits the realm with the requested scope as a
//! query parameter and gets back JSON `{"token":"..."}` (or
//! `{"access_token":"..."}` on some implementations). The token is
//! then attached to the original request.
//!
//! Anonymous-only in 0.2: no client credentials, no refresh-token flow.

use serde::Deserialize;

use crate::error::WispImageError;

/// Parsed `WWW-Authenticate: Bearer ...` challenge components. Only
/// `realm` is required by the OCI dist spec; `service` and `scope` are
/// what the registry asks the client to forward to the realm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub realm: String,
    pub service: Option<String>,
    pub scope: Option<String>,
}

/// Parse a `WWW-Authenticate` header value. Returns `None` if the
/// scheme isn't `Bearer` (Basic, Digest, etc.). Tolerates surrounding
/// whitespace, quoted values that contain commas, and missing optional
/// fields.
pub fn parse_challenge(header_value: &str) -> Option<Challenge> {
    let trimmed = header_value.trim();
    let rest = trimmed
        .strip_prefix("Bearer")
        .or_else(|| trimmed.strip_prefix("bearer"))?;
    // Require a separator between scheme and params so we don't match
    // a malformed `BearerXYZ` literal.
    let rest = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('\t'))?;
    let params = parse_params(rest);

    let realm = params
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("realm"))?
        .1
        .clone();
    let service = params
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("service"))
        .map(|(_, v)| v.clone());
    let scope = params
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("scope"))
        .map(|(_, v)| v.clone());

    Some(Challenge {
        realm,
        service,
        scope,
    })
}

/// Hit `challenge.realm` anonymously, forwarding the requested
/// service + scope as query parameters, and return the bearer token
/// from the response body. Falls back to `access_token` if `token` is
/// absent (some registries emit only the OAuth2 field name).
pub fn obtain_token(
    http: &reqwest::blocking::Client,
    challenge: &Challenge,
) -> Result<String, WispImageError> {
    let mut query: Vec<(&str, &str)> = Vec::new();
    if let Some(svc) = challenge.service.as_deref() {
        query.push(("service", svc));
    }
    if let Some(sc) = challenge.scope.as_deref() {
        query.push(("scope", sc));
    }

    let resp = http
        .get(&challenge.realm)
        .query(&query)
        .send()
        .map_err(|e| WispImageError::Network(format!("token realm {}: {e}", challenge.realm)))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().unwrap_or_else(|_| String::from("<no body>"));
        let snippet = clip(&body, 1024);
        return Err(WispImageError::Auth(format!(
            "realm {} returned {status}: {snippet}",
            challenge.realm
        )));
    }

    let parsed: TokenResponse = resp
        .json()
        .map_err(|e| WispImageError::Auth(format!("realm response not JSON: {e}")))?;
    parsed
        .token
        .or(parsed.access_token)
        .ok_or_else(|| WispImageError::Auth("realm response missing token field".into()))
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

/// Tokenise a `WWW-Authenticate` parameter list into `(key, value)`
/// pairs. Handles quoted values: a quoted run starts at `"` and ends at
/// the next unescaped `"`, so commas inside the quotes don't split the
/// value. Bare values run up to the next comma or end-of-string.
fn parse_params(input: &str) -> Vec<(String, String)> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Skip whitespace and stray commas.
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // Read key up to '='.
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' && bytes[i] != b',' {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            // Param without `=`; skip.
            continue;
        }
        let key = std::str::from_utf8(&bytes[key_start..i])
            .unwrap_or("")
            .trim()
            .to_string();
        i += 1; // consume '='
        // Skip whitespace before value.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            out.push((key, String::new()));
            break;
        }
        // Quoted or bare value.
        let value = if bytes[i] == b'"' {
            i += 1;
            let value_start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                // Backslash-escape: skip the next byte.
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                i += 1;
            }
            let v = std::str::from_utf8(&bytes[value_start..i])
                .unwrap_or("")
                .to_string();
            if i < bytes.len() {
                i += 1; // consume closing quote
            }
            v
        } else {
            let value_start = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            std::str::from_utf8(&bytes[value_start..i])
                .unwrap_or("")
                .trim()
                .to_string()
        };
        out.push((key, value));
    }
    out
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Avoid splitting in the middle of a UTF-8 sequence.
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn parse_challenge_extracts_realm_service_scope() {
        let header = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#;
        let c = parse_challenge(header).expect("bearer challenge");
        assert_eq!(c.realm, "https://auth.docker.io/token");
        assert_eq!(c.service.as_deref(), Some("registry.docker.io"));
        assert_eq!(c.scope.as_deref(), Some("repository:library/alpine:pull"));
    }

    #[test]
    fn parse_challenge_handles_quoted_values_with_commas() {
        // A scope with an embedded comma must survive parsing because
        // it lives inside quotes.
        let header =
            r#"Bearer realm="https://realm",service="svc",scope="repository:foo:pull,push""#;
        let c = parse_challenge(header).expect("parsed");
        assert_eq!(c.scope.as_deref(), Some("repository:foo:pull,push"));
    }

    #[test]
    fn parse_challenge_returns_none_for_non_bearer() {
        assert!(parse_challenge("Basic realm=\"foo\"").is_none());
        assert!(parse_challenge("Digest realm=\"foo\"").is_none());
        assert!(parse_challenge("").is_none());
        // No separator between scheme and params.
        assert!(parse_challenge("Bearer123").is_none());
    }

    #[tokio::test]
    async fn obtain_token_calls_realm_with_scope_and_returns_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .and(query_param("service", "registry.example"))
            .and(query_param("scope", "repository:foo:pull"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "abc123"
            })))
            .mount(&server)
            .await;

        let realm = format!("{}/token", server.uri());
        let challenge = Challenge {
            realm,
            service: Some("registry.example".into()),
            scope: Some("repository:foo:pull".into()),
        };

        // Run the blocking call on a worker thread so it doesn't
        // block the (single-threaded) test runtime.
        let token = tokio::task::spawn_blocking(move || {
            let http = reqwest::blocking::Client::new();
            obtain_token(&http, &challenge)
        })
        .await
        .expect("join")
        .expect("token");
        assert_eq!(token, "abc123");
    }

    #[tokio::test]
    async fn obtain_token_falls_back_to_access_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "fallback"
            })))
            .mount(&server)
            .await;

        let realm = format!("{}/token", server.uri());
        let challenge = Challenge {
            realm,
            service: None,
            scope: None,
        };
        let token = tokio::task::spawn_blocking(move || {
            let http = reqwest::blocking::Client::new();
            obtain_token(&http, &challenge)
        })
        .await
        .expect("join")
        .expect("token");
        assert_eq!(token, "fallback");
    }

    #[tokio::test]
    async fn obtain_token_handles_400_with_clear_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad scope"))
            .mount(&server)
            .await;

        let realm = format!("{}/token", server.uri());
        let challenge = Challenge {
            realm: realm.clone(),
            service: None,
            scope: Some("nonsense".into()),
        };
        let err = tokio::task::spawn_blocking(move || {
            let http = reqwest::blocking::Client::new();
            obtain_token(&http, &challenge)
        })
        .await
        .expect("join")
        .expect_err("400 should error");
        let msg = format!("{err}");
        assert!(msg.contains("400"), "missing status: {msg}");
        assert!(msg.contains("bad scope"), "missing body: {msg}");
        assert!(msg.contains(&realm), "missing realm: {msg}");
    }
}
