//! `isd info <id>` (alias `inspect`): top-level detail view for one
//! resource. Auto-detects type by matching against the controller's
//! known hosts, stacks, and the docker context's containers.
//!
//! Defined by the v0.7 CLI lexicon spec
//! (`3 Resources/Superpowers/specs/2026-05-22-isd-cli-lexicon-design.md`):
//! one canonical detail verb across every namespace. The deeper
//! `isd stack info <name>` (also from PR E in the spec) is a more
//! specific surface; this top-level form is the operator's "I have an
//! id, what is it" probe.
//!
//! Resolution order (first match wins):
//!
//! 1. Hosts: matched by hostname or dial_target against
//!    `GET /api/v1/hosts`.
//! 2. Stacks: matched by name against `GET /api/v1/stacks`.
//! 3. Containers: matched by name (or id prefix) against the docker
//!    context's running + stopped containers.
//!
//! No match: error with the three buckets we tried so the operator
//! can spot the typo.
//!
//! When `<id>` is omitted: open a unified picker over hosts + stacks +
//! containers, then info the picked id.

use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Utc};
use clap::Args;
use serde::Deserialize;

use crate::session::Session;

/// CLI flags for `isd info`.
///
/// `id` is optional: bare `isd info` opens the unified picker
/// (lexicon spec interactive-mode contract).
#[derive(Debug, Args)]
pub struct InfoArgs {
    /// Hostname, dial target, stack name, or container name / id prefix.
    ///
    /// Omit to open the picker.
    pub id: Option<String>,
}

/// Top-level dispatcher. Resolves `id` via the picker if missing, then
/// auto-detects the resource type and renders the matching detail box.
///
/// # Errors
///
/// Returns `Err` on HTTP/docker failures, an unmatched id, or a
/// cancelled picker.
pub async fn run(args: InfoArgs, context: Option<&str>) -> Result<()> {
    let context_owned = context.map(str::to_owned);
    let id = crate::picker_or_arg::pick_or_arg(args.id, || async {
        pick_any_resource(context_owned.as_deref()).await
    })
    .await?;
    dispatch(&id, context).await
}

/// Run the type-detection ladder and render. Picker callers go through
/// here so a picked id flows through the same resolution logic as a
/// typed positional.
async fn dispatch(id: &str, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    if let Some(host) = match_host(&session, id).await? {
        print_host(&host);
        return Ok(());
    }
    if let Some(stack) = match_stack(&session, id).await? {
        print_stack(&stack);
        return Ok(());
    }
    if let Some(c) = match_container(context, id).await? {
        print_container(&c);
        return Ok(());
    }
    Err(anyhow!(
        "unknown id {id:?}. Tried hosts, stacks, and containers; \
         try `isd hosts ls`, `isd stack ls`, or `isd ps` to see \
         valid names."
    ))
}

/// One host as returned by `GET /api/v1/hosts`. Kept local to this
/// module so it stays decoupled from `ssh::HostRow` (which carries
/// only the fields the ssh path needs).
#[derive(Debug, Clone, Deserialize)]
struct HostDetail {
    /// Host ULID.
    #[serde(default)]
    id: String,
    /// Reported hostname (uname -n on the agent).
    hostname: String,
    /// Last heartbeat we saw from this host's agent, if any.
    #[serde(default)]
    last_seen_at: Option<DateTime<Utc>>,
    /// Operator-facing dial target, when known.
    #[serde(default)]
    dial_target: Option<String>,
    /// Agent ULID, when the row carries it. Older controllers may omit.
    #[serde(default)]
    agent_id: Option<String>,
}

/// One stack as returned by `GET /api/v1/stacks`.
#[derive(Debug, Clone, Deserialize)]
struct StackDetail {
    /// Stringified surrogate key.
    #[serde(default)]
    id: String,
    /// Owning host ULID.
    #[serde(default)]
    host_id: String,
    /// Operator-facing stack name.
    name: String,
    /// Origin tag (`compose`, `imported`, ...).
    #[serde(default)]
    source: String,
    /// When the controller first observed this stack.
    #[serde(default)]
    discovered_at: Option<DateTime<Utc>>,
}

/// Subset of `isd_runtime::ContainerSummary` we render. Built by
/// [`match_container`] so the info command stays decoupled from the
/// docker backend's exact shape.
#[derive(Debug, Clone)]
struct ContainerDetail {
    /// Full container id.
    id: String,
    /// First container name with the leading `/` stripped.
    name: String,
    /// Image reference.
    image: String,
    /// Docker's human status string.
    status: String,
    /// Published + private ports, comma-joined.
    ports: String,
}

/// Try to match `id` against the controller's host list. Returns the
/// first row whose `hostname`, `id`, or `dial_target` equals `id`.
async fn match_host(session: &Session, id: &str) -> Result<Option<HostDetail>> {
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/hosts");
    let rows: Vec<HostDetail> = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .context("listing hosts for `isd info`")?
        .json()
        .await
        .context("decoding hosts JSON for `isd info`")?;
    Ok(rows
        .into_iter()
        .find(|h| h.hostname == id || h.id == id || h.dial_target.as_deref() == Some(id)))
}

/// Try to match `id` against the controller's stack list. Returns the
/// first row whose `name` equals `id`.
async fn match_stack(session: &Session, id: &str) -> Result<Option<StackDetail>> {
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/stacks");
    let rows: Vec<StackDetail> = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .context("listing stacks for `isd info`")?
        .json()
        .await
        .context("decoding stacks JSON for `isd info`")?;
    Ok(rows.into_iter().find(|s| s.name == id))
}

/// Try to match `id` against the docker context's containers. Matches
/// on container name first, then on container id (full or prefix of
/// at least three chars to avoid trivial false positives).
async fn match_container(context: Option<&str>, id: &str) -> Result<Option<ContainerDetail>> {
    use isd_runtime::DockerBackend;
    let docker_uri = crate::docker_context::resolve_docker_uri(context)?;
    let backend = DockerBackend::from_uri(&docker_uri)
        .await
        .with_context(|| format!("opening docker backend at {docker_uri}"))?;
    let containers = backend
        .list_containers(true)
        .await
        .context("listing containers for `isd info`")?;
    let by_name = containers.iter().find(|c| c.names == id);
    let by_id = containers
        .iter()
        .find(|c| c.id == id || (id.len() >= 3 && c.id.starts_with(id)));
    let pick = by_name.or(by_id);
    Ok(pick.map(|c| ContainerDetail {
        id: c.id.clone(),
        name: c.names.clone(),
        image: c.image.clone(),
        status: c.status.clone(),
        ports: c.ports.clone(),
    }))
}

/// Open the unified picker over hosts + stacks + containers. Each row
/// is prefixed with a `[class]` tag so the operator can tell which
/// bucket a name came from.
async fn pick_any_resource(context: Option<&str>) -> Result<Option<String>> {
    let rows = collect_pickable_rows(context).await?;
    if rows.is_empty() {
        return Err(anyhow!("no hosts, stacks, or containers to pick from"));
    }
    let display: Vec<String> = rows
        .iter()
        .map(|r| format!("[{class}] {name}", class = r.class, name = r.name))
        .collect();
    let picked = crate::picker::pick(display, "ID", "filter resources...").await?;
    Ok(picked.map(|s| strip_class_prefix(&s).to_string()))
}

/// One row in the unified info picker.
struct PickRow {
    /// Resource bucket label rendered in the `[<class>]` prefix.
    class: &'static str,
    /// Underlying id passed to [`dispatch`] after the picker returns.
    name: String,
}

/// Drop the `[<class>] ` prefix the picker adds so [`dispatch`] sees
/// the raw id. Hostnames, stack names, and container names cannot
/// legitimately start with `[` so this is unambiguous.
fn strip_class_prefix(s: &str) -> &str {
    s.split_once("] ").map(|(_, rest)| rest).unwrap_or(s)
}

/// Walk hosts + stacks + containers and build the picker source list.
/// Per-bucket failures are reported on stderr and the run continues so
/// a temporarily-down controller route still lets the operator pick a
/// container, etc.
async fn collect_pickable_rows(context: Option<&str>) -> Result<Vec<PickRow>> {
    let mut out: Vec<PickRow> = Vec::new();

    // Hosts + stacks share a session; one open if either is reachable.
    let session = Session::open(context).await?;
    let controller_url = session.require_controller()?;

    match list_hosts(&session, controller_url).await {
        Ok(rows) => {
            for h in rows {
                out.push(PickRow {
                    class: "host",
                    name: h.hostname,
                });
            }
        }
        Err(e) => eprintln!("isd info: hosts unreachable: {e}"),
    }
    match list_stacks(&session, controller_url).await {
        Ok(rows) => {
            for s in rows {
                out.push(PickRow {
                    class: "stack",
                    name: s.name,
                });
            }
        }
        Err(e) => eprintln!("isd info: stacks unreachable: {e}"),
    }
    match list_container_names(context).await {
        Ok(names) => {
            for n in names {
                out.push(PickRow {
                    class: "container",
                    name: n,
                });
            }
        }
        Err(e) => eprintln!("isd info: containers unreachable: {e}"),
    }

    Ok(out)
}

/// `GET /api/v1/hosts` for the picker.
async fn list_hosts(session: &Session, controller_url: &str) -> Result<Vec<HostDetail>> {
    let url = format!("{controller_url}/api/v1/hosts");
    let rows: Vec<HostDetail> = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?
        .json()
        .await?;
    Ok(rows)
}

/// `GET /api/v1/stacks` for the picker.
async fn list_stacks(session: &Session, controller_url: &str) -> Result<Vec<StackDetail>> {
    let url = format!("{controller_url}/api/v1/stacks");
    let rows: Vec<StackDetail> = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?
        .json()
        .await?;
    Ok(rows)
}

/// Pull every container's name from the docker context for the
/// picker. Returns just the names; full detail is fetched again on
/// dispatch (cheap, and keeps the picker source uniformly shaped).
async fn list_container_names(context: Option<&str>) -> Result<Vec<String>> {
    use isd_runtime::DockerBackend;
    let docker_uri = crate::docker_context::resolve_docker_uri(context)?;
    let backend = DockerBackend::from_uri(&docker_uri).await?;
    let containers = backend.list_containers(true).await?;
    Ok(containers.into_iter().map(|c| c.names).collect())
}

/// Render a key/value box for one host.
fn print_host(h: &HostDetail) {
    let last = h
        .last_seen_at
        .map(relative_time)
        .unwrap_or_else(|| "-".into());
    let agent = h.agent_id.clone().unwrap_or_else(|| "-".into());
    let dial = h.dial_target.clone().unwrap_or_else(|| "(unset)".into());
    let rows = [
        ("NAME", h.hostname.as_str()),
        ("DIAL TARGET", dial.as_str()),
        ("LAST SEEN", last.as_str()),
        ("AGENT ID", agent.as_str()),
        ("HOST ID", h.id.as_str()),
    ];
    println!("{}", render_kv_box(&rows));
}

/// Render a key/value box for one stack.
fn print_stack(s: &StackDetail) {
    let discovered = s
        .discovered_at
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| "-".into());
    let rows = [
        ("NAME", s.name.as_str()),
        ("ID", s.id.as_str()),
        ("HOST ID", s.host_id.as_str()),
        ("SOURCE", s.source.as_str()),
        ("DISCOVERED", discovered.as_str()),
    ];
    println!("{}", render_kv_box(&rows));
}

/// Render a key/value box for one container.
fn print_container(c: &ContainerDetail) {
    let ports = if c.ports.is_empty() {
        "(none)"
    } else {
        c.ports.as_str()
    };
    let rows = [
        ("NAME", c.name.as_str()),
        ("ID", c.id.as_str()),
        ("IMAGE", c.image.as_str()),
        ("STATUS", c.status.as_str()),
        ("PORTS", ports),
    ];
    println!("{}", render_kv_box(&rows));
}

/// Render a tiny key/value box with rounded corners. Two columns: dim
/// ALL-CAPS keys on the left, values on the right. Width adapts to
/// the longest cell so short ids do not leave a stretched chrome.
///
/// The output deliberately mirrors the visual language of the table
/// renderer in `crate::render` (rounded corners, light vertical
/// separator) without dragging in the table's column-fit machinery:
/// info boxes have two columns by construction.
pub(crate) fn render_kv_box(rows: &[(&str, &str)]) -> String {
    let key_width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(1);
    let val_width = rows.iter().map(|(_, v)| v.len()).max().unwrap_or(1);
    let key_inner = key_width + 2; // one space pad on each side
    let val_inner = val_width + 2;

    let mut out = String::new();
    out.push('╭');
    out.push_str(&"─".repeat(key_inner));
    out.push('┬');
    out.push_str(&"─".repeat(val_inner));
    out.push_str("╮\n");
    for (k, v) in rows {
        out.push('│');
        out.push(' ');
        out.push_str(k);
        out.push_str(&" ".repeat(key_width - k.len() + 1));
        out.push('│');
        out.push(' ');
        out.push_str(v);
        out.push_str(&" ".repeat(val_width - v.len() + 1));
        out.push_str("│\n");
    }
    out.push('╰');
    out.push_str(&"─".repeat(key_inner));
    out.push('┴');
    out.push_str(&"─".repeat(val_inner));
    out.push('╯');
    out
}

/// Format an RFC3339 timestamp as a coarse "just now / Nm ago / Nh
/// ago / Nd ago" relative string. Mirrors `ssh::picker::relative_time`
/// so the info LAST SEEN cell reads the same as the picker's.
fn relative_time(t: DateTime<Utc>) -> String {
    let now = Utc::now();
    let secs = now.signed_duration_since(t).num_seconds();
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct WrapArgs {
        #[command(flatten)]
        a: InfoArgs,
    }

    /// `isd info <id>` parses with the positional set.
    #[test]
    fn info_args_parses_with_id() {
        let w = WrapArgs::try_parse_from(["x", "lausanne"]).unwrap();
        assert_eq!(w.a.id.as_deref(), Some("lausanne"));
    }

    /// Bare `isd info` (no id) parses with `id = None` so the runtime
    /// falls into the unified picker per the lexicon spec.
    #[test]
    fn info_args_bare_parses_with_no_id() {
        let w = WrapArgs::try_parse_from(["x"]).unwrap();
        assert!(w.a.id.is_none());
    }

    /// The kv-box renderer emits rounded corners and uniform padding.
    /// Pure formatter so chrome regressions surface as a unit-test
    /// failure rather than a visual eyeball check.
    #[test]
    fn render_kv_box_has_rounded_chrome_and_aligned_columns() {
        let rows = [("NAME", "lausanne"), ("DIAL", "ops@10.0.0.1")];
        let out = render_kv_box(&rows);
        let lines: Vec<&str> = out.lines().collect();
        // Top + bottom borders carry the rounded corners + tee
        // characters joining the two inner columns.
        assert!(lines[0].starts_with('╭'));
        assert!(lines[0].contains('┬'));
        assert!(lines[0].ends_with('╮'));
        assert!(lines[lines.len() - 1].starts_with('╰'));
        assert!(lines[lines.len() - 1].contains('┴'));
        assert!(lines[lines.len() - 1].ends_with('╯'));
        // Every body line wraps the data with the box's vertical edges.
        for body in &lines[1..lines.len() - 1] {
            assert!(body.starts_with('│'), "body: {body:?}");
            assert!(body.ends_with('│'), "body: {body:?}");
        }
        // The right value column extends to the widest value so the
        // chrome stays a rectangle.
        let widths: Vec<usize> = lines.iter().map(|l| l.chars().count()).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "every box line shares the same character count: {widths:?}"
        );
    }

    /// Empty values still render cleanly with at least one space of
    /// padding (no zero-width cells).
    #[test]
    fn render_kv_box_handles_empty_value_without_collapsing() {
        let rows = [("KEY", "")];
        let out = render_kv_box(&rows);
        // Body line: `│ KEY │   │` (padded with at least one inner
        // space on each side of the empty value cell).
        let body = out.lines().nth(1).unwrap();
        assert!(body.starts_with("│ KEY"));
        assert!(body.ends_with('│'));
    }

    /// `strip_class_prefix` peels the `[host] ` / `[stack] ` /
    /// `[container] ` tag the unified picker adds so `dispatch` sees
    /// the raw id.
    #[test]
    fn strip_class_prefix_removes_tag_when_present() {
        assert_eq!(strip_class_prefix("[host] lausanne"), "lausanne");
        assert_eq!(strip_class_prefix("[stack] servarr"), "servarr");
        assert_eq!(strip_class_prefix("[container] web-1"), "web-1");
    }

    /// Strings without the tag pass through untouched (defensive
    /// against operator-typed picker rows in tests).
    #[test]
    fn strip_class_prefix_passes_through_when_absent() {
        assert_eq!(strip_class_prefix("lausanne"), "lausanne");
    }
}
