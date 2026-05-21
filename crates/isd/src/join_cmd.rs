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
    println!("Joined cluster as agent.");
    Ok(())
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
