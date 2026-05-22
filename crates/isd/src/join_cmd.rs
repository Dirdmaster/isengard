//! `isd join`: bring up an isd-agent container on the target docker
//! context, enrol it against an existing controller.
//!
//! Mirrors `docker swarm join`. Usage:
//!
//!   isd join --controller https://controller.local:9417 \
//!     --token TKxxxx.yyyy --context <target>
//!
//! The command runs on the operator's machine (Mac); the agent
//! container lives on `--context`'s host (via the docker context).

use anyhow::{Context, Result, anyhow};
use clap::Args;

/// CLI flags for `isd join`.
#[derive(Debug, Args)]
pub struct JoinArgs {
    /// Controller URL (e.g. https://controller.local:9417).
    #[arg(long)]
    pub controller: String,
    /// Packed join-token (TK<bytes>.<fingerprint>) from `isd join-token`.
    #[arg(long)]
    pub token: String,

    /// Operator-facing host name to record on the new agent's host
    /// row. This is the label rendered in `isd ssh hosts`, NOT the
    /// agent's container hostname (which is the Docker short hash on
    /// docker-in-docker setups). When omitted, the target daemon's
    /// `docker info` name is used; when that is also empty the
    /// agent-reported value (container hash) is kept and the
    /// operator can override later via
    /// `isd ssh hosts set <agent> --name <foo>`.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
}

/// Bundled compose recipe used to bring the agent up. Embedded at
/// compile time so the binary stays self-contained and the operator
/// never has to fetch a YAML file to run `isd join`.
const EMBEDDED_COMPOSE: &str = include_str!("../../../install/compose.yaml");

/// Enrol the target docker context as an agent against an existing
/// controller.
///
/// Validates the join-token format pre-flight, writes the embedded
/// compose recipe to a temp file, then runs `docker compose up -d
/// agent` with `ISENGARD_ENROLL_TOKEN` + `ISENGARD_CONTROLLER_URL` in
/// the subprocess env. Polls the controller's `GET /api/v1/hosts` for
/// up to 60 seconds; first non-empty response means the agent
/// fingerprint-verified the controller CA and enrolled.
///
/// # Errors
///
/// Returns `Err` on invalid token format, docker compose failures,
/// missing temp file space, or a 60-second enrol timeout (controller
/// reachable but no host appeared).
pub async fn run(args: JoinArgs, context: Option<&str>) -> Result<()> {
    use std::io::Write;

    // Pre-flight: validate the token format so we fail before touching docker
    isengard_core::join_token::parse(&args.token).map_err(|e| {
        anyhow!("invalid token: {e}\nExpected TK<bytes>.<fingerprint> format from `isd join-token`")
    })?;

    let docker_uri = crate::docker_context::resolve_docker_uri(context)?;

    eprintln!("isd join: bringing up agent on {docker_uri}");

    let mut tmp =
        tempfile::NamedTempFile::new().context("creating tmp file for embedded compose")?;
    tmp.write_all(EMBEDDED_COMPOSE.as_bytes())
        .context("writing embedded compose to tmp")?;
    tmp.flush().ok();

    let status = tokio::process::Command::new("docker")
        .env("DOCKER_HOST", &docker_uri)
        .env("ISENGARD_ENROLL_TOKEN", &args.token)
        .env("ISENGARD_CONTROLLER_URL", &args.controller)
        .arg("compose")
        .arg("-f")
        .arg(tmp.path())
        .arg("up")
        .arg("-d")
        .arg("agent")
        .status()
        .await
        .context("docker compose up -d agent")?;
    if !status.success() {
        return Err(anyhow!(
            "docker compose up -d agent failed (exit {:?})",
            status.code()
        ));
    }

    poll_for_enrolment(&args.controller).await?;
    // Best-effort: PATCH the just-enrolled host with the operator's
    // dial target (derived from the docker context URL) AND an
    // operator-facing host name (from --name or the target daemon's
    // info.name). Failures are logged + swallowed.
    record_host_metadata(&args.controller, &docker_uri, args.name.as_deref()).await;
    println!("Joined cluster as agent.");
    Ok(())
}

/// Best-effort PATCH of the just-enrolled host row with the
/// operator's host name (from `--name` or `docker info`) and dial
/// target (from the docker context URL). Mirrors `step_record_host_metadata`
/// in `init_cmd.rs`; kept separate because `isd join` already knows the
/// controller URL up front (passed by the operator) so we don't run
/// the runtime discovery dance.
///
/// Failures are reported on stderr and swallowed: enrollment already
/// succeeded; the operator can recover with
/// `isd ssh hosts set <agent> --name <foo> --dial <target>`.
async fn record_host_metadata(controller_url: &str, docker_uri: &str, name_override: Option<&str>) {
    // Dial target: only set for SSH-backed docker contexts.
    let dial_target = crate::docker_context::dial_target_from_docker_uri(docker_uri);

    // Host name: try `docker info` on the target context. Skipped on
    // backend-open failure so a busted SSH tunnel doesn't kill the
    // enrollment summary; `--name` still works because it doesn't
    // need the daemon round-trip.
    let daemon_name = match isd_runtime::DockerBackend::from_uri(docker_uri).await {
        Ok(backend) => match crate::host_name::resolve_from_docker_info(backend.client()).await {
            Ok(opt) => opt,
            Err(e) => {
                eprintln!("isd join: docker info name lookup failed: {e:#}");
                None
            }
        },
        Err(e) => {
            eprintln!("isd join: skipping docker info name lookup ({e:#})");
            None
        }
    };
    let hostname = crate::host_name::pick_enroll_hostname(name_override, daemon_name.as_deref());

    crate::host_name::patch_latest_host_best_effort(
        controller_url,
        hostname.as_deref(),
        dial_target.as_deref(),
    )
    .await;
}

/// Poll `GET /<controller>/api/v1/hosts` until the agent shows up.
///
/// 60 second deadline, 2 second cadence, 5 second per-request timeout.
/// A non-empty `hosts` array is the signal: a freshly enrolled agent
/// emits a heartbeat almost immediately on its first connection, so we
/// don't need to match on a specific host_id.
///
/// # Errors
///
/// Returns `Err` when no host is visible inside the 60 second budget.
/// The message points the operator at `docker logs isd-agent` on the
/// target host since the failure is almost always agent-side
/// (fingerprint mismatch, network partition).
async fn poll_for_enrolment(controller_url: &str) -> Result<()> {
    use tokio::time::{Duration, Instant, sleep};

    let deadline = Instant::now() + Duration::from_secs(60);
    let url = format!("{}/api/v1/hosts", controller_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("building enrolment poll client")?;

    eprint!("isd join: waiting for agent to enrol");
    while Instant::now() < deadline {
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                if let Ok(rows) = resp.json::<serde_json::Value>().await {
                    if rows.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                        eprintln!(" enrolled");
                        return Ok(());
                    }
                }
            }
        }
        eprint!(".");
        sleep(Duration::from_secs(2)).await;
    }
    eprintln!();
    Err(anyhow!(
        "agent did not enrol within 60s. Check `docker logs isd-agent` on the target host."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `isd join --name <foo>` lands the operator-supplied name in
    /// `JoinArgs.name` so the post-enroll PATCH carries it.
    #[test]
    fn join_args_parses_name_flag() {
        use clap::Parser;
        #[derive(Debug, Parser)]
        struct Wrap {
            #[command(flatten)]
            join: JoinArgs,
        }
        let parsed = Wrap::try_parse_from([
            "wrap",
            "--controller",
            "https://controller.local:9417",
            "--token",
            "TKxxxxx.yyyyy",
            "--name",
            "geneva",
        ])
        .expect("parses");
        assert_eq!(parsed.join.name.as_deref(), Some("geneva"));
        assert_eq!(parsed.join.controller, "https://controller.local:9417");
        assert_eq!(parsed.join.token, "TKxxxxx.yyyyy");
    }

    /// Omitting `--name` leaves the field `None` so the post-enroll
    /// PATCH falls back to the daemon's `docker info` name.
    #[test]
    fn join_args_default_name_is_none() {
        use clap::Parser;
        #[derive(Debug, Parser)]
        struct Wrap {
            #[command(flatten)]
            join: JoinArgs,
        }
        let parsed = Wrap::try_parse_from([
            "wrap",
            "--controller",
            "https://controller.local:9417",
            "--token",
            "TKxxxxx.yyyyy",
        ])
        .expect("parses");
        assert!(parsed.join.name.is_none());
    }
}
