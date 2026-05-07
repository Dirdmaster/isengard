//! v0.3c manual smoke test for the compose import path.
//!
//! Reads every running container labeled `isengard.enable=true`, groups
//! them by stack (`isengard.stack` overriding `com.docker.compose.project`),
//! runs them through `compose_export::build_compose`, and writes the
//! result + a sibling `meta.toml` under `/tmp/iso_smoke/<stack>/`.
//!
//! Use this to eyeball the YAML output before opening a PR. Requires a
//! reachable Docker daemon at the bollard default socket.
//!
//! Run with:
//!
//!   cargo run -p isengard-agent --example compose_smoke

use bollard::Docker;
use bollard::container::ListContainersOptions;
use isengard_agent::compose_export::build_compose;
use isengard_agent::compose_writer::{sha256_hex, write_compose};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let docker = Docker::connect_with_local_defaults()?;
    let containers = docker
        .list_containers(Some(ListContainersOptions::<String> {
            all: false,
            ..Default::default()
        }))
        .await?;

    let mut by_stack: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for c in &containers {
        let labels = c.labels.as_ref();
        let enabled = labels
            .and_then(|m| m.get("isengard.enable"))
            .map(|v| v == "true")
            .unwrap_or(false);
        if !enabled {
            continue;
        }
        let stack_name = labels
            .and_then(|m| {
                m.get("isengard.stack")
                    .or_else(|| m.get("com.docker.compose.project"))
            })
            .cloned();
        let Some(stack_name) = stack_name else {
            continue;
        };
        if let Some(id) = c.id.as_ref() {
            by_stack.entry(stack_name).or_default().push(id.clone());
        }
    }

    for (stack_name, ids) in by_stack {
        println!("\n=== stack: {} ({} containers) ===", stack_name, ids.len());
        let mut inspects = Vec::new();
        for id in &ids {
            inspects.push(docker.inspect_container(id, None).await?);
        }
        let yaml = build_compose(&stack_name, &inspects, Some("01TEST_HOST"))?;
        println!("{}", yaml);
        println!("--- sha256: {}", sha256_hex(&yaml));

        let dir = PathBuf::from(format!("/tmp/iso_smoke/{}", stack_name));
        let outcome = write_compose(
            &dir,
            &yaml,
            "01TEST_HOST",
            &chrono::Utc::now().to_rfc3339(),
            false,
        )?;
        println!("--- write outcome: {:?}", outcome);
        println!("--- wrote: {}/compose.yaml + meta.toml", dir.display());
    }
    Ok(())
}
