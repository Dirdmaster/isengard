//! `isd open <stack>`: resolve the stack's primary public hostname from
//! routing rules and shell out to the OS default browser.

use anyhow::{Context as _, Result, anyhow};
use clap::Args;
use serde::Deserialize;

use crate::login::{pinned_session, verify_pinned_response};

#[derive(Debug, Args)]
pub struct OpenArgs {
    /// Stack name (matches the `name` field in `/api/v1/stacks`).
    pub stack: String,

    /// Print the resolved URL instead of launching the browser. Useful in
    /// SSH sessions and tests.
    #[arg(long)]
    pub print: bool,
}

#[derive(Debug, Deserialize)]
struct StackDto {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize, Clone)]
struct RoutingRuleDto {
    public_hostname: String,
    /// `id` from the controller's storage row. We only need it as a stable
    /// tiebreak when multiple rules match the same stack.
    #[serde(default)]
    id: i64,
    /// Optional service identifier the controller attaches when the rule
    /// was generated from a stack's compose file. v0.3a ships an
    /// approximation: when the controller exposes `stack_id`, we filter
    /// directly. Otherwise the routing-rules endpoint returns rules for
    /// the whole fleet and we filter by string match on the rule path.
    #[serde(default)]
    stack_id: Option<i64>,
    #[serde(default)]
    state: Option<String>,
}

pub async fn run(args: OpenArgs, context: Option<&str>) -> Result<()> {
    let (ctx, client) = pinned_session(context).await?;

    let stacks_resp = client
        .get(format!("{}/api/v1/stacks", ctx.controller_url))
        .bearer_auth(&ctx.token)
        .send()
        .await
        .context("GET /api/v1/stacks")?;
    verify_pinned_response(&stacks_resp, &ctx.ca_fingerprint_sha256)?;
    let stacks: Vec<StackDto> = stacks_resp
        .error_for_status()
        .context("listing stacks")?
        .json()
        .await
        .context("decoding stacks JSON")?;

    let stack = stacks
        .iter()
        .find(|s| s.name == args.stack)
        .ok_or_else(|| anyhow!("no stack named {:?}", args.stack))?;
    let stack_id_int: i64 = stack.id.parse().with_context(|| {
        format!(
            "stack id {:?} is not numeric (cannot match routing rule)",
            stack.id
        )
    })?;

    let rules_resp = client
        .get(format!("{}/api/v1/routing/rules", ctx.controller_url))
        .bearer_auth(&ctx.token)
        .send()
        .await
        .context("GET /api/v1/routing/rules")?;
    verify_pinned_response(&rules_resp, &ctx.ca_fingerprint_sha256)?;
    let rules: Vec<RoutingRuleDto> = rules_resp
        .error_for_status()
        .context("listing routing rules")?
        .json()
        .await
        .context("decoding routing rules JSON")?;

    let primary = pick_primary_rule(&rules, stack_id_int)
        .ok_or_else(|| anyhow!("no enabled routing rule for stack {:?}", args.stack))?;
    let url = host_to_url(&primary.public_hostname);
    if args.print {
        println!("{url}");
        return Ok(());
    }
    open::that_detached(&url).with_context(|| format!("launching browser for {url}"))?;
    println!("Opened {url}");
    Ok(())
}

/// Pick the rule we want to open in a browser:
///   1. matches `stack_id` directly when the rule has one set; for label-
///      discovered rules `stack_id` is None and we accept anyway. The
///      controller's stack list filtered by name already narrowed scope,
///      so a None-stack-id rule on a single-stack lookup is still our rule.
///   2. operational state: `active` or `pending`. `draining` and `failed`
///      are skipped. `pending` is included because v0.3a sees freshly
///      label-discovered rules in this state before the proxy adopts them.
///   3. lowest `id` (stable tiebreak across calls).
fn pick_primary_rule(rules: &[RoutingRuleDto], stack_id: i64) -> Option<RoutingRuleDto> {
    let mut candidates: Vec<&RoutingRuleDto> = rules
        .iter()
        .filter(|r| match r.stack_id {
            None => true,
            Some(id) => id == stack_id,
        })
        .filter(|r| {
            matches!(
                r.state.as_deref(),
                None | Some("active") | Some("pending") | Some("enabled")
            )
        })
        .collect();
    candidates.sort_by_key(|r| r.id);
    candidates.first().map(|r| (*r).clone())
}

fn host_to_url(host: &str) -> String {
    if host.starts_with("http://") || host.starts_with("https://") {
        host.to_string()
    } else {
        format!("http://{host}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: i64, host: &str, stack: Option<i64>, state: Option<&str>) -> RoutingRuleDto {
        RoutingRuleDto {
            id,
            public_hostname: host.into(),
            stack_id: stack,
            state: state.map(str::to_string),
        }
    }

    #[test]
    fn picks_lowest_id_among_matching_enabled_rules() {
        let rules = vec![
            rule(5, "five.local", Some(1), Some("enabled")),
            rule(3, "three.local", Some(1), Some("enabled")),
            rule(7, "seven.local", Some(1), Some("disabled")),
            rule(1, "other.local", Some(2), Some("enabled")),
        ];
        let chosen = pick_primary_rule(&rules, 1).unwrap();
        assert_eq!(chosen.public_hostname, "three.local");
    }

    #[test]
    fn missing_state_field_is_treated_as_enabled() {
        let rules = vec![rule(2, "ok.local", Some(1), None)];
        let chosen = pick_primary_rule(&rules, 1).unwrap();
        assert_eq!(chosen.public_hostname, "ok.local");
    }

    #[test]
    fn pending_active_states_are_openable() {
        let rules = vec![
            rule(1, "a.local", Some(1), Some("pending")),
            rule(2, "b.local", Some(1), Some("active")),
            rule(3, "c.local", Some(1), Some("draining")),
            rule(4, "d.local", Some(1), Some("failed")),
        ];
        let chosen = pick_primary_rule(&rules, 1).unwrap();
        assert_eq!(chosen.public_hostname, "a.local");
    }

    #[test]
    fn label_discovered_rules_with_no_stack_id_are_accepted() {
        let rules = vec![rule(1, "label.local", None, Some("active"))];
        let chosen = pick_primary_rule(&rules, 42).unwrap();
        assert_eq!(chosen.public_hostname, "label.local");
    }

    #[test]
    fn host_to_url_adds_http_when_missing() {
        assert_eq!(host_to_url("hello.local"), "http://hello.local");
        assert_eq!(host_to_url("https://hello.local"), "https://hello.local");
        assert_eq!(host_to_url("http://hello.local"), "http://hello.local");
    }
}
