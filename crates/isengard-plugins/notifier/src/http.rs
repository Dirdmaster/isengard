//! Generic HTTP POST channel. Send events as JSON to any URL with optional
//! custom headers. Default body shape: `{"text": "<formatted event>"}`.
//! Custom `body_template` supports `{{text}}` and `{{kind}}` placeholders.

use std::collections::HashMap;

use async_trait::async_trait;
use isengard_core::Event;
use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use tracing::warn;

use crate::channel::{NotifyChannel, format_event};

#[derive(Debug, Clone, Deserialize, Default)]
pub struct HttpConfig {
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Body template with `{{text}}` and `{{kind}}` placeholders.
    /// Default: `{"text": "<text>"}` JSON.
    #[serde(default)]
    pub body_template: Option<String>,
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default)]
    pub tokens_per_minute: Option<f64>,
}

pub struct HttpChannel {
    url: String,
    headers: HeaderMap,
    body_template: Option<String>,
    kinds: Vec<String>,
    http: Client,
}

impl HttpChannel {
    pub fn from_config(cfg: HttpConfig) -> anyhow::Result<Self> {
        if cfg.url.is_empty() {
            return Err(anyhow::anyhow!("http channel: url required"));
        }
        let mut headers = HeaderMap::new();
        for (k, v) in &cfg.headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| anyhow::anyhow!("bad header name {k}: {e}"))?;
            let value = HeaderValue::from_str(v)
                .map_err(|e| anyhow::anyhow!("bad header value for {k}: {e}"))?;
            headers.insert(name, value);
        }
        Ok(Self {
            url: cfg.url,
            headers,
            body_template: cfg.body_template,
            kinds: cfg.kinds,
            http: Client::builder()
                .user_agent(concat!("isengard-notifier/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|e| anyhow::anyhow!("building http client: {e}"))?,
        })
    }

    fn build_body(&self, event: &Event) -> String {
        let text = format_event(event);
        match &self.body_template {
            Some(tpl) => tpl
                .replace("{{text}}", &json_escape(&text))
                .replace("{{kind}}", &json_escape(&event.kind)),
            None => serde_json::to_string(&serde_json::json!({ "text": text }))
                .unwrap_or_else(|_| String::from("{}")),
        }
    }
}

/// Minimal JSON string-content escaping (quote, backslash, newline).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

#[async_trait]
impl NotifyChannel for HttpChannel {
    fn name(&self) -> &'static str {
        "http"
    }

    fn matches_kind(&self, kind: &str) -> bool {
        self.kinds.iter().any(|k| k == kind)
    }

    async fn send(&self, event: &Event) -> anyhow::Result<()> {
        let body = self.build_body(event);
        let mut req = self.http.post(&self.url).body(body);
        for (k, v) in self.headers.iter() {
            req = req.header(k, v);
        }
        // Default Content-Type to application/json if user didn't override
        if !self.headers.contains_key(reqwest::header::CONTENT_TYPE) {
            req = req.header(reqwest::header::CONTENT_TYPE, "application/json");
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => {
                warn!(status = %resp.status(), "http channel non-success");
                Ok(())
            }
            Err(e) => {
                warn!(error = %e, "http channel send failed");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_errors_on_empty_url() {
        let r = HttpChannel::from_config(HttpConfig::default());
        assert!(r.is_err());
    }

    #[test]
    fn from_config_errors_on_bad_header_name() {
        let mut h = HashMap::new();
        h.insert("invalid header".into(), "x".into());
        let cfg = HttpConfig {
            url: "https://example.com".into(),
            headers: h,
            ..Default::default()
        };
        let r = HttpChannel::from_config(cfg);
        assert!(r.is_err());
    }

    #[test]
    fn default_body_is_text_field_json() {
        let ch = HttpChannel::from_config(HttpConfig {
            url: "https://example.com".into(),
            ..Default::default()
        })
        .unwrap();
        let ev = Event {
            kind: "k".into(),
            occurred_at: chrono::Utc::now(),
            summary: "s".into(),
            ..Default::default()
        };
        let body = ch.build_body(&ev);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(parsed["text"].is_string());
        assert!(parsed["text"].as_str().unwrap().contains("[isengard] k"));
    }

    #[test]
    fn custom_body_template_substitutes_placeholders() {
        let ch = HttpChannel::from_config(HttpConfig {
            url: "https://example.com".into(),
            body_template: Some(r#"{"kind":"{{kind}}","msg":"{{text}}"}"#.into()),
            ..Default::default()
        })
        .unwrap();
        let ev = Event {
            kind: "test.kind".into(),
            occurred_at: chrono::Utc::now(),
            summary: "hi".into(),
            ..Default::default()
        };
        let body = ch.build_body(&ev);
        assert!(body.contains(r#""kind":"test.kind""#));
        assert!(body.contains(r#""msg":"[isengard] test.kind\nhi""#));
    }
}
