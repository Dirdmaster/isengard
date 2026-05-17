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
//! Phase 3 ships the surface + step stubs. Phase 4 implements steps 1-6,
//! Phase 5 implements steps 7-9.

use anyhow::{Result, anyhow};
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

// Phase 4 + 5 step bodies will reference this constant when invoking
// `docker compose up -d`; the test below also asserts it carries the
// discovery labels. Phase 3 only ships the surface, so the runtime
// references land later.
#[allow(dead_code)]
const EMBEDDED_COMPOSE: &str = include_str!("../../../install/compose.yaml");

pub async fn run(args: InitArgs, context: Option<&str>) -> Result<()> {
    // Resolve docker URI from context; error early if missing.
    let docker_uri = crate::ps::resolve_docker_uri(context)?.ok_or_else(|| {
        anyhow!(
            "context has no docker endpoint; create one with `isd context import <name>` first"
        )
    })?;

    eprintln!("isd init: bootstrapping controller on {docker_uri}");

    // Phase 4 will fill these in. Phase 3 ships the skeleton.
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

// === Step stubs (Phase 4 + 5 implement these) ===

async fn step_check_no_existing_controller(_docker_uri: &str, _force: bool) -> Result<()> {
    Err(anyhow!(
        "isd init: step_check_no_existing_controller not implemented yet (Phase 4)"
    ))
}
async fn step_create_state_volume(_docker_uri: &str) -> Result<()> {
    Err(anyhow!(
        "isd init: step_create_state_volume not implemented yet (Phase 4)"
    ))
}
async fn step_generate_master_key(_docker_uri: &str) -> Result<()> {
    Err(anyhow!(
        "isd init: step_generate_master_key not implemented yet (Phase 4)"
    ))
}
async fn step_compose_up_controller(_docker_uri: &str) -> Result<()> {
    Err(anyhow!(
        "isd init: step_compose_up_controller not implemented yet (Phase 4)"
    ))
}
async fn step_wait_for_controller_ready(_docker_uri: &str) -> Result<()> {
    Err(anyhow!(
        "isd init: step_wait_for_controller_ready not implemented yet (Phase 4)"
    ))
}
async fn step_mint_first_join_token(_docker_uri: &str) -> Result<String> {
    Err(anyhow!(
        "isd init: step_mint_first_join_token not implemented yet (Phase 4)"
    ))
}
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
}
