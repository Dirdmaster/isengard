//! Agent on-disk state: a single `agent.json` carrying the controller-assigned
//! `agent_id`. Survives agent restarts so the agent doesn't re-enroll.
//!
//! Atomic write: write to a `.tmp` file, then `rename` over the target.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Result;

const STATE_FILENAME: &str = "agent.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentState {
    pub agent_id: String,
}

/// Load the state file from `<state_dir>/agent.json`. Returns `Ok(None)` if the
/// file doesn't exist yet (first boot). Returns `Err` only on actual IO or
/// parse errors.
pub async fn load(state_dir: &Path) -> Result<Option<AgentState>> {
    let path = state_dir.join(STATE_FILENAME);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let state: AgentState = serde_json::from_slice(&bytes)
                .map_err(|e| anyhow::anyhow!("parsing {path:?}: {e}"))?;
            Ok(Some(state))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("reading {path:?}: {e}")),
    }
}

/// Atomically write the state file to `<state_dir>/agent.json`.
/// Strategy: serialize → write to `<file>.tmp` → rename over target.
pub async fn save(state_dir: &Path, state: &AgentState) -> Result<()> {
    let path = state_dir.join(STATE_FILENAME);
    let tmp = state_dir.join(format!("{STATE_FILENAME}.tmp"));

    let body = serde_json::to_vec_pretty(state)
        .map_err(|e| anyhow::anyhow!("serialising state: {e}"))?;

    tokio::fs::write(&tmp, &body).await
        .map_err(|e| anyhow::anyhow!("writing {tmp:?}: {e}"))?;
    tokio::fs::rename(&tmp, &path).await
        .map_err(|e| anyhow::anyhow!("rename {tmp:?} -> {path:?}: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn load_returns_none_for_empty_dir() {
        let dir = TempDir::new().unwrap();
        let result = load(dir.path()).await.expect("load should not error");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn save_then_load_round_trips() {
        let dir = TempDir::new().unwrap();
        let original = AgentState {
            agent_id: "01J7XYZAGENTID000000000000".into(),
        };

        save(dir.path(), &original).await.expect("save");
        let loaded = load(dir.path()).await.expect("load").expect("Some");
        assert_eq!(loaded, original);
    }

    #[tokio::test]
    async fn load_returns_error_on_malformed_file() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("agent.json"), b"{not valid json").await.unwrap();

        let err = load(dir.path()).await.expect_err("must fail");
        let msg = format!("{err}");
        assert!(msg.contains("parsing") || msg.contains("agent.json"), "got: {msg}");
    }

    #[tokio::test]
    async fn save_overwrites_existing() {
        let dir = TempDir::new().unwrap();
        let first = AgentState { agent_id: "first".into() };
        let second = AgentState { agent_id: "second".into() };
        save(dir.path(), &first).await.unwrap();
        save(dir.path(), &second).await.unwrap();
        let loaded = load(dir.path()).await.unwrap().unwrap();
        assert_eq!(loaded, second);
    }
}
