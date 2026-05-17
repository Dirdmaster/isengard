//! `isd init`: bootstrap a controller (+ first agent) on the operator's
//! docker context. Swarm-style: one command, one minute, you have a
//! cluster and a join-token in your terminal.
//!
//! Step machine (each step is a discrete async function with its own
//! error context):
//!   1. check no existing controller (or --force tears it down)
//!   2. create iso-controller-state docker volume
//!   3. generate master.key inside the volume via bootstrap container
//!   4. docker compose up -d controller (embedded recipe)
//!   5. wait for controller to become discoverable + healthy
//!   6. mint first agent join-token via `docker exec controller isengard join-token`
//!   7. docker compose up -d agent (with ISENGARD_ENROLL_TOKEN)
//!   8. wait for agent to enrol (GET /api/v1/hosts returns one)
//!   9. render swarm-style join-block for ADDITIONAL hosts
//!
//! Phase 4 implements steps 1-6. Phase 5 fills in steps 7-9.

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

const EMBEDDED_COMPOSE: &str = include_str!("../../../install/compose.yaml");

/// Image used for the one-shot bootstrap container that seeds
/// `/state/master.key` in the iso-controller-state volume. Pinned to a
/// concrete minor so the binary's behaviour does not silently shift if
/// Docker Hub re-tags `alpine:latest`.
const BOOTSTRAP_IMAGE: &str = "alpine:3.21";

pub async fn run(args: InitArgs, context: Option<&str>) -> Result<()> {
    // Resolve docker URI from context; error early if missing.
    let docker_uri = crate::ps::resolve_docker_uri(context)?.ok_or_else(|| {
        anyhow!("context has no docker endpoint; create one with `isd context import <name>` first")
    })?;

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

    let join_block = step_render_join_block(&docker_uri, &join_token).await?;
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

    let mut output = String::new();
    if let StartExecResults::Attached {
        output: mut stream, ..
    } = docker.client().start_exec(&exec.id, None).await?
    {
        while let Some(item) = stream.next().await {
            let chunk = item.context("reading token mint stdout")?;
            output.push_str(&chunk.to_string());
        }
    }
    let token = output.trim().to_string();
    if token.is_empty() {
        return Err(anyhow!(
            "token mint returned empty output; check `docker logs iso-controller`"
        ));
    }
    Ok(token)
}

// === Step stubs (Phase 5 implements these) ===

async fn step_compose_up_agent(_docker_uri: &str, _token: &str) -> Result<()> {
    Err(anyhow!(
        "isd init: step_compose_up_agent not implemented yet (Phase 5)"
    ))
}
async fn step_wait_for_agent_enrolled(_docker_uri: &str) -> Result<()> {
    Err(anyhow!(
        "isd init: step_wait_for_agent_enrolled not implemented yet (Phase 5)"
    ))
}
async fn step_render_join_block(_docker_uri: &str, _token: &str) -> Result<String> {
    Err(anyhow!(
        "isd init: step_render_join_block not implemented yet (Phase 5)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_compose_carries_discovery_labels() {
        assert!(EMBEDDED_COMPOSE.contains("io.isengard.role: controller"));
        assert!(EMBEDDED_COMPOSE.contains("io.isengard.api.version: \"1\""));
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
