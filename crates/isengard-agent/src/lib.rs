//! Agent-mode runtime: load plugins, run their lifecycle hooks, wait for
//! shutdown. Phase 1 minimum — no gRPC client, no docker integration.

pub mod agent_state;
pub mod backoff;
pub mod container_snapshot;
pub mod enroll;
pub mod events;
pub mod proxy;
pub mod sync;

/// Convenience alias used throughout the agent.
pub type Result<T> = anyhow::Result<T>;

use std::sync::Arc;

use isengard_core::{EventEmitter, HostMode, Plugin, PluginContext, registrations_for};
use tracing::{info, instrument, warn};

#[derive(Debug, Clone)]
pub struct AgentOptions {
    /// URL of the controller, e.g. `http://controller.example.com:9417`.
    /// Required — Phase 2d removes the `Option` wrapper from Phase 1.
    pub controller_url: String,
    /// Directory where the agent persists its state (`agent.json` etc).
    /// Created if missing.
    pub state_dir: std::path::PathBuf,
    pub config: serde_json::Value,
}

pub fn load_plugins() -> Vec<Box<dyn Plugin>> {
    registrations_for(HostMode::Agent)
        .into_iter()
        .map(|r| (r.constructor)())
        .collect()
}

#[instrument(skip(opts), fields(controller = %opts.controller_url))]
pub async fn run_agent(opts: AgentOptions) -> Result<()> {
    info!(state_dir = ?opts.state_dir, "starting agent");

    // -- determine agent_id ----------------------------------------------
    let existing = agent_state::load(&opts.state_dir).await?;
    let agent_id = match existing {
        Some(state) => {
            info!(agent_id = %state.agent_id, "already enrolled, skipping enroll");
            state.agent_id
        }
        None => {
            info!("no agent.json found, enrolling with controller");
            let token = std::env::var("ISENGARD_TOKEN")
                .map_err(|_| anyhow::anyhow!("ISENGARD_TOKEN env var must be set"))?;
            let host_info = enroll::HostInfo::detect();
            let id = enroll::enroll(&opts.controller_url, &token, host_info).await?;
            agent_state::save(
                &opts.state_dir,
                &agent_state::AgentState {
                    agent_id: id.clone(),
                },
            )
            .await?;
            info!(agent_id = %id, "enrolled");
            id
        }
    };

    // -- build the outbound EventEmitter (mpsc-backed). Receiver lives in
    //    this scope and is passed by &mut into the sync loop so events stay
    //    queued across reconnects.
    let (emitter, mut events_rx) = events::OutboundEmitter::new();
    let emitter: Arc<dyn EventEmitter> = Arc::new(emitter);

    // -- plugin lifecycle (unchanged from Phase 1) -----------------------
    let plugins = load_plugins();
    info!(plugin_count = plugins.len(), "plugins discovered");

    let mut started = Vec::with_capacity(plugins.len());
    for mut plugin in plugins {
        let plugin_name = plugin.name().to_string();
        let plugin_config = opts
            .config
            .get(&plugin_name)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let ctx = PluginContext::new(HostMode::Agent, plugin_config).with_events(emitter.clone());
        plugin.init(&ctx).await?;
        plugin.start(&ctx).await?;
        info!(plugin = %plugin_name, "plugin started");
        started.push(plugin);
    }

    // -- spawn the reverse-proxy supervisor (Phase 8b Task 10).
    //    Hardcoded ports for v1; Plan B (Phase 8e) will read these from
    //    settings. `proxy_state` is also threaded into the sync loop so
    //    Task 13's `apply_config` can mutate the upstream registry when
    //    the controller pushes a `ProxyConfig`.
    let proxy_state = proxy::ProxyState::new();
    {
        let ps = proxy_state.clone();
        let http_port: u16 = 8080;
        let https_port: u16 = 8443;
        tokio::spawn(async move {
            proxy::supervise(ps, http_port, https_port).await;
        });
    }

    // -- run sync loop in background; ctrl_c triggers shutdown ----------
    let token = std::env::var("ISENGARD_TOKEN")
        .map_err(|_| anyhow::anyhow!("ISENGARD_TOKEN env var must be set"))?;
    let cancel = std::sync::Arc::new(tokio::sync::Notify::new());

    // Phase 2e hardcodes 10s heartbeat interval. Phase 2e+ honors
    // heartbeat_interval_secs from EnrollResponse.
    const HEARTBEAT_INTERVAL_SECS: u32 = 10;

    // The sync loop borrows `events_rx` for its lifetime — own it here.
    let sync_url = opts.controller_url.clone();
    let sync_agent_id = agent_id.clone();
    let sync_cancel = cancel.clone();
    let sync_proxy_state = proxy_state.clone();
    let sync_fut = async move {
        sync::run_sync_with_reconnect(
            sync_url,
            token,
            sync_agent_id,
            HEARTBEAT_INTERVAL_SECS,
            sync_cancel,
            &mut events_rx,
            sync_proxy_state,
        )
        .await
    };
    tokio::pin!(sync_fut);

    // Wait for ctrl_c OR for the sync loop to terminate on its own.
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl_c received, shutting down");
            cancel.notify_waiters();

            // Wait for sync to wind down (with a timeout so we don't hang forever).
            let sync_result =
                tokio::time::timeout(std::time::Duration::from_secs(5), &mut sync_fut).await;
            match sync_result {
                Ok(Ok(())) => info!("sync loop exited cleanly"),
                Ok(Err(e)) => warn!(error = %e, "sync loop exited with error"),
                Err(_) => warn!("sync loop timed out on shutdown"),
            }
        }
        res = &mut sync_fut => {
            match res {
                Ok(()) => info!("sync loop exited cleanly before ctrl_c"),
                Err(e) => warn!(error = %e, "sync loop exited with error before ctrl_c"),
            }
        }
    }

    // -- plugin stop ---------------------------------------------------
    for mut plugin in started {
        plugin.stop().await?;
    }

    info!(agent_id = %agent_id, "agent exited cleanly");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn dummy() {
        // placeholder; real tests come in Tasks 2-4 of Phase 2d
        assert_eq!(2 + 2, 4);
    }
}
