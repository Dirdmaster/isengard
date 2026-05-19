//! `isd init`: bootstrap a controller (+ first agent) on the operator's
//! docker context. Swarm-style: one command, one minute, you have a
//! cluster ready in your terminal.
//!
//! Step machine (each step is a discrete async function with its own
//! error context):
//!   1. check no existing controller (or --force tears it down)
//!   2. create iso-controller-state docker volume
//!   3. generate master.key inside the volume via bootstrap container
//!   4. docker compose up -d controller (embedded recipe)
//!   5. wait for controller to become discoverable + healthy
//!   6. mint first agent join-token via `docker exec controller isengard join-token`
//!   7. docker compose up -d agent (token passed through env)
//!   8. wait for agent to enrol (GET /api/v1/hosts returns one)
//!   9. print a one-line "Cluster ready" hint pointing at `isd join-token`
//!      for operators who want to add more hosts
//!
//! Track F (2026-05-18): the agent verifies the controller's CA on first
//! connect via the fingerprint embedded in the join token. The
//! operator-side CA export step is gone; so is the docker-run join block
//! that used to flood the terminal at the end of every `isd init`.

use anyhow::{Context, Result, anyhow};
use clap::Args;

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Tear down any existing iso-controller / iso-agent containers
    /// before bringing up new ones. Preserves the iso-controller-state
    /// docker volume (master key + SQLite kept). To wipe everything:
    /// `docker volume rm iso-controller-state`.
    #[arg(long)]
    pub force: bool,

    /// Skip the local agent bootstrap (controller-only host). Default
    /// is to bundle the agent, matching `docker swarm init` UX.
    #[arg(long)]
    pub no_agent: bool,
}

pub(crate) const EMBEDDED_COMPOSE: &str = include_str!("../../../install/compose.yaml");

/// Image used for the one-shot bootstrap container that seeds
/// `/state/master.key` in the iso-controller-state volume. Pinned to a
/// concrete minor so the binary's behaviour does not silently shift if
/// Docker Hub re-tags `alpine:latest`.
const BOOTSTRAP_IMAGE: &str = "alpine:3.21";

pub async fn run(args: InitArgs, context: Option<&str>) -> Result<()> {
    // Resolve docker URI from the docker context (Track H).
    let docker_uri = crate::docker_context::resolve_docker_uri(context)?;

    eprintln!("isd init: bootstrapping controller on {docker_uri}");

    step_check_no_existing_controller(&docker_uri, args.force).await?;
    step_create_state_volume(&docker_uri).await?;
    step_generate_master_key(&docker_uri).await?;
    step_compose_up_controller(&docker_uri).await?;
    step_wait_for_controller_ready(&docker_uri).await?;
    let join_token = step_mint_first_join_token(&docker_uri).await?;

    if !args.no_agent {
        step_compose_up_agent(&docker_uri, &join_token).await?;
        step_wait_for_agent_enrolled(&docker_uri).await?;
    }

    let join_block = step_render_join_block().await?;
    println!("{join_block}");
    Ok(())
}

// === Step 1: no existing controller (or --force tears it down) ===

async fn step_check_no_existing_controller(docker_uri: &str, force: bool) -> Result<()> {
    use bollard::container::{ListContainersOptions, RemoveContainerOptions};
    use std::collections::HashMap;

    let docker = isd_runtime::DockerBackend::from_uri(docker_uri).await?;
    let mut filters = HashMap::new();
    filters.insert(
        "label".to_string(),
        vec![format!(
            "{}={}",
            isd_runtime::discovery_labels::ROLE_LABEL,
            isd_runtime::discovery_labels::ROLE_CONTROLLER
        )],
    );
    let containers = docker
        .client()
        .list_containers(Some(ListContainersOptions {
            all: true,
            filters,
            ..Default::default()
        }))
        .await?;

    if containers.is_empty() {
        return Ok(());
    }
    if !force {
        let names: Vec<String> = containers
            .iter()
            .filter_map(|c| c.names.as_ref().and_then(|v| v.first().cloned()))
            .collect();
        return Err(anyhow!(
            "found existing isengard controller(s) on this host: {names:?}. \
             Re-run with --force to recreate, or `docker compose down -v` to wipe."
        ));
    }
    eprintln!("isd init: --force; removing existing controller container(s)");
    for c in &containers {
        if let Some(id) = c.id.as_deref() {
            docker
                .client()
                .remove_container(
                    id,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await
                .with_context(|| format!("removing container {id}"))?;
        }
    }
    Ok(())
}

// === Step 2: create the iso-controller-state docker volume ===

async fn step_create_state_volume(docker_uri: &str) -> Result<()> {
    use bollard::volume::CreateVolumeOptions;

    let docker = isd_runtime::DockerBackend::from_uri(docker_uri).await?;
    let options = CreateVolumeOptions::<&str> {
        name: "iso-controller-state",
        driver: "local",
        ..Default::default()
    };
    docker
        .client()
        .create_volume(options)
        .await
        .context("creating iso-controller-state volume")?;
    eprintln!("isd init: volume iso-controller-state ready");
    Ok(())
}

// === Step 3: generate /state/master.key via a bootstrap container ===

async fn step_generate_master_key(docker_uri: &str) -> Result<()> {
    use bollard::container::{
        Config, CreateContainerOptions, StartContainerOptions, WaitContainerOptions,
    };
    use bollard::image::CreateImageOptions;
    use bollard::models::HostConfig;
    use futures_util::StreamExt;

    let docker = isd_runtime::DockerBackend::from_uri(docker_uri).await?;

    // Ensure alpine image present (no-op if cached).
    let mut pull = docker.client().create_image(
        Some(CreateImageOptions {
            from_image: BOOTSTRAP_IMAGE,
            ..Default::default()
        }),
        None,
        None,
    );
    while let Some(item) = pull.next().await {
        item.context("pulling alpine for bootstrap")?;
    }

    let cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        "test -f /state/master.key || (head -c32 /dev/urandom > /state/master.key && chmod 0400 /state/master.key)".to_string(),
    ];
    let config = Config::<String> {
        image: Some(BOOTSTRAP_IMAGE.into()),
        cmd: Some(cmd),
        host_config: Some(HostConfig {
            binds: Some(vec!["iso-controller-state:/state".into()]),
            auto_remove: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    let created = docker
        .client()
        .create_container(None::<CreateContainerOptions<String>>, config)
        .await
        .context("creating master-key bootstrap container")?;
    docker
        .client()
        .start_container(&created.id, None::<StartContainerOptions<String>>)
        .await
        .context("starting master-key bootstrap container")?;
    let mut wait = docker
        .client()
        .wait_container(&created.id, None::<WaitContainerOptions<String>>);
    while let Some(item) = wait.next().await {
        let resp = item.context("waiting for master-key bootstrap to exit")?;
        if resp.status_code != 0 {
            return Err(anyhow!(
                "master-key bootstrap exited with code {} (msg: {:?})",
                resp.status_code,
                resp.error
            ));
        }
    }
    eprintln!("isd init: master key ready (in iso-controller-state volume)");
    Ok(())
}

// === Step 4: docker compose up -d controller against embedded recipe ===

async fn step_compose_up_controller(docker_uri: &str) -> Result<()> {
    use std::io::Write;

    let mut tmp =
        tempfile::NamedTempFile::new().context("creating tmp file for embedded compose")?;
    tmp.write_all(EMBEDDED_COMPOSE.as_bytes())
        .context("writing embedded compose to tmp")?;
    tmp.flush().ok();

    let status = tokio::process::Command::new("docker")
        .env("DOCKER_HOST", docker_uri)
        .arg("compose")
        .arg("-f")
        .arg(tmp.path())
        .arg("up")
        .arg("-d")
        .arg("controller")
        .status()
        .await
        .context("docker compose up -d controller")?;
    if !status.success() {
        return Err(anyhow!(
            "docker compose up -d controller failed (exit {:?})",
            status.code()
        ));
    }
    Ok(())
}

// === Step 5: poll until the controller is discoverable + responsive ===

async fn step_wait_for_controller_ready(docker_uri: &str) -> Result<()> {
    use tokio::time::{Duration, Instant, sleep};

    let docker = isd_runtime::DockerBackend::from_uri(docker_uri).await?;
    let deadline = Instant::now() + Duration::from_secs(30);

    eprint!("isd init: waiting for controller to become ready");
    loop {
        if Instant::now() > deadline {
            eprintln!();
            return Err(anyhow!(
                "controller did not become ready within 30s. Check `docker logs iso-controller`."
            ));
        }
        if let Ok(endpoint) = isd_runtime::discover(docker.client()).await {
            // Discovery succeeded. Verify REST surface answers.
            let url = format!(
                "http://{}:{}/api/v1/hosts",
                endpoint.host_ip, endpoint.host_port
            );
            if let Ok(resp) = reqwest::get(&url).await {
                if resp.status().is_success() {
                    eprintln!(" ready");
                    return Ok(());
                }
            }
        }
        eprint!(".");
        sleep(Duration::from_secs(1)).await;
    }
}

// === Step 6: mint the first agent join-token via docker exec ===

async fn step_mint_first_join_token(docker_uri: &str) -> Result<String> {
    use bollard::container::LogOutput;
    use bollard::exec::{CreateExecOptions, StartExecResults};
    use futures_util::StreamExt;

    let docker = isd_runtime::DockerBackend::from_uri(docker_uri).await?;
    let exec = docker
        .client()
        .create_exec(
            "iso-controller",
            CreateExecOptions::<String> {
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                cmd: Some(vec![
                    "isengard".into(),
                    "controller".into(),
                    "token".into(),
                    "mint".into(),
                    "--role".into(),
                    "agent".into(),
                    "--format".into(),
                    "token".into(),
                ]),
                ..Default::default()
            },
        )
        .await
        .context("creating exec for token mint")?;

    // Read stdout-only from the exec stream: the controller binary
    // writes its tracing banner to stderr. We additionally scan for a
    // token-shaped line as defense in depth so future logging changes
    // can't silently corrupt the agent's enrollment token.
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let StartExecResults::Attached {
        output: mut stream, ..
    } = docker.client().start_exec(&exec.id, None).await?
    {
        while let Some(item) = stream.next().await {
            match item.context("reading token mint output")? {
                LogOutput::StdOut { message } => {
                    stdout.push_str(&String::from_utf8_lossy(&message));
                }
                LogOutput::StdErr { message } => {
                    stderr.push_str(&String::from_utf8_lossy(&message));
                }
                LogOutput::Console { message } | LogOutput::StdIn { message } => {
                    stdout.push_str(&String::from_utf8_lossy(&message));
                }
            }
        }
    }

    let token = extract_join_token(&stdout).ok_or_else(|| {
        anyhow!(
            "token mint did not emit a parseable join token.\n  stdout: {:?}\n  stderr: {:?}",
            stdout.trim(),
            stderr.trim()
        )
    })?;
    Ok(token)
}

/// Pull a `TK<base32>.<base32>` join token out of the controller's stdout.
///
/// Scans line-by-line and returns the last line that parses as a packed
/// token. Tolerates accidental log noise mixed into stdout (e.g. an
/// always-on banner from a future binary) without silently feeding it to
/// the agent as `ISENGARD_ENROLL_TOKEN`.
fn extract_join_token(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .rfind(|line| isengard_core::join_token::parse(line).is_ok())
        .map(str::to_string)
}

// === Step 7: docker compose up -d agent with the join token via env ===

async fn step_compose_up_agent(docker_uri: &str, token: &str) -> Result<()> {
    use std::io::Write;

    let mut tmp =
        tempfile::NamedTempFile::new().context("creating tmp file for embedded compose")?;
    tmp.write_all(EMBEDDED_COMPOSE.as_bytes())
        .context("writing embedded compose to tmp")?;
    tmp.flush().ok();

    // The embedded recipe references ${ISENGARD_ENROLL_TOKEN} on the agent
    // service. docker compose interpolates from the parent process env.
    // DOCKER_HOST routes the spawn to the operator's context. Track F: the
    // CA pin used to ride alongside via ISENGARD_CONTROLLER_CA_PEM_BASE64;
    // the agent now fetches the CA from the controller on first connect and
    // verifies the embedded fingerprint, so we no longer thread it here.
    let status = tokio::process::Command::new("docker")
        .env("DOCKER_HOST", docker_uri)
        .env("ISENGARD_ENROLL_TOKEN", token)
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
    Ok(())
}

// === Step 8: poll the controller until the agent has enrolled ===

async fn step_wait_for_agent_enrolled(docker_uri: &str) -> Result<()> {
    use tokio::time::{Duration, Instant, sleep};

    let docker = isd_runtime::DockerBackend::from_uri(docker_uri).await?;
    let endpoint = isd_runtime::discover(docker.client())
        .await
        .context("rediscovering controller for enrol-wait")?;
    let url = format!(
        "http://{}:{}/api/v1/hosts",
        endpoint.host_ip, endpoint.host_port
    );
    let deadline = Instant::now() + Duration::from_secs(60);

    eprint!("isd init: waiting for agent to enrol");
    loop {
        if Instant::now() > deadline {
            eprintln!();
            return Err(anyhow!(
                "agent did not enrol within 60s. Check `docker logs iso-agent`."
            ));
        }
        if let Ok(resp) = reqwest::get(&url).await
            && resp.status().is_success()
            && let Ok(rows) = resp.json::<serde_json::Value>().await
            && rows.as_array().map(|a| !a.is_empty()).unwrap_or(false)
        {
            eprintln!(" enrolled");
            return Ok(());
        }
        eprint!(".");
        sleep(Duration::from_secs(2)).await;
    }
}
// === Step 9: clean one-liner pointing operators at `isd join-token` ===

/// Track F: cluster-ready output is a clean one-liner. The verbose
/// docker-run join block is gone; operators run `isd join-token` when
/// they actually want to add a host.
async fn step_render_join_block() -> Result<String> {
    Ok("Cluster ready. 1 host. To add more hosts:\n  isd join-token".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_compose_carries_discovery_labels() {
        assert!(EMBEDDED_COMPOSE.contains("io.isengard.role: controller"));
        assert!(EMBEDDED_COMPOSE.contains("io.isengard.api.version: \"1\""));
    }

    /// Minted by `isengard controller token mint --format token` against a
    /// real CA. Used as a token-shape fixture in the tests below.
    const FIXTURE_TOKEN: &str = "TKY2ZPGCLMIWR6UZFIVZOVXTNAY626OE2G2FVELW35YIT3H7RBCQPQ.\
                                 ZUGFQZXZQECT3BCZDBFMX7UL2V7KSYG5RZHZQKTWO3YOFOAEAGGQ";

    #[test]
    fn extract_join_token_strips_banner_noise() {
        // What lausanne 0.6.0-pre actually emitted: ANSI-colored banner
        // line ahead of the token. extract_join_token should ignore the
        // banner and return the packed token.
        let stdout = format!(
            "\x1b[1;32misengard\x1b[0m \x1b[2mnext\x1b[0m \
             \x1b[1;36mcontroller\x1b[0m \x1b[2mready\x1b[0m\n{FIXTURE_TOKEN}\n"
        );
        assert_eq!(extract_join_token(&stdout).as_deref(), Some(FIXTURE_TOKEN));
    }

    #[test]
    fn extract_join_token_returns_last_match() {
        // Defense in depth: if two tokens somehow end up on stdout,
        // prefer the last one (the most recent mint).
        let stale = "TKAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA.\
                     AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let stdout = format!("{stale}\n{FIXTURE_TOKEN}\n");
        assert_eq!(extract_join_token(&stdout).as_deref(), Some(FIXTURE_TOKEN));
    }

    #[test]
    fn extract_join_token_returns_none_on_banner_only() {
        let stdout = "\x1b[1;32misengard\x1b[0m next controller ready\n";
        assert!(extract_join_token(stdout).is_none());
    }

    #[test]
    fn extract_join_token_returns_none_on_empty() {
        assert!(extract_join_token("").is_none());
        assert!(extract_join_token("   \n  \n").is_none());
    }

    // Integration-style; runs only against a real local docker.
    #[tokio::test]
    #[ignore]
    async fn create_state_volume_is_idempotent() {
        let r1 = step_create_state_volume("unix:///var/run/docker.sock").await;
        let r2 = step_create_state_volume("unix:///var/run/docker.sock").await;
        assert!(r1.is_ok());
        assert!(r2.is_ok());
    }
}
