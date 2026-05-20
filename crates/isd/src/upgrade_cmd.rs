//! `isd upgrade`: pull a new controller + agent image tag, recreate the
//! containers, health-check the resulting cluster.
//!
//! Default takes an encrypted backup first (same pipeline as `isd backup`)
//! to `<tmpdir>/iso-upgrade-pre-<date>.tgz.age`. On failure, the error
//! surfaces the backup path + `docker logs iso-controller` so the operator
//! can roll back via `isd restore <path>`.
//!
//! Step machine:
//!   1. Detect the current tag from `docker inspect iso-controller`
//!      (parses `Config.Image`, falls back to `Image` if needed).
//!   2. Compute the target tag (operator-supplied via `--tag`, or
//!      re-pull the current tag for image-digest refresh).
//!   3. Confirm prompt (skipped with `--yes`).
//!   4. Auto-backup unless `--skip-backup`.
//!   5. `docker pull` controller + agent images at the target tag.
//!   6. `docker compose up -d` against the embedded recipe with
//!      `ISENGARD_IMAGE_TAG=<target>` so compose recreates containers
//!      whose image reference changed. State volumes (iso-controller-state,
//!      iso-agent-state, iso-stacks) survive because they are external
//!      named volumes.
//! 7. Poll discovery + `GET /api/v1/hosts` for 90s.
//!   8. On success print the upgraded tag + backup path.

use anyhow::{Context, Result, anyhow};
use clap::Args;

/// Controller image repository on GHCR.
const CONTROLLER_IMAGE: &str = "ghcr.io/weavers-engineering/isengard-controller";
/// Agent image repository on GHCR.
const AGENT_IMAGE: &str = "ghcr.io/weavers-engineering/isengard-agent";

/// CLI flags for `isd upgrade`.
#[derive(Debug, Args)]
pub struct UpgradeArgs {
    /// Pin to a specific image tag. Default: re-pull the current tag
    /// (refreshes a moving target like `:next` to the latest digest).
    #[arg(long)]
    pub tag: Option<String>,
    /// Skip the auto-backup before upgrade. Risks data loss if the
    /// upgraded controller fails to come back up.
    #[arg(long)]
    pub skip_backup: bool,
    /// Skip the confirm prompt.
    #[arg(long)]
    pub yes: bool,
}

/// Pull a new controller + agent image tag and recreate the
/// containers, taking an encrypted backup first by default.
///
/// # Errors
///
/// Returns `Err` on any step failure. Includes a restore-hint pointing
/// at `isd restore <path>` when the post-recreate health check fails.
pub async fn run(args: UpgradeArgs, context: Option<&str>) -> Result<()> {
    let docker_uri = crate::docker_context::resolve_docker_uri(context)?;
    let docker = isd_runtime::DockerBackend::from_uri(&docker_uri).await?;

    // 1. Detect current tag.
    let current_tag = inspect_current_tag(&docker).await?;
    let target_tag = args.tag.clone().unwrap_or_else(|| current_tag.clone());

    // 2. Confirm.
    if !args.yes {
        eprintln!(
            "isd upgrade: {current_tag} -> {target_tag} (controller + agent, recreate containers)."
        );
        if !args.skip_backup {
            eprintln!("isd upgrade: will take an encrypted backup first.");
        } else {
            eprintln!("isd upgrade: --skip-backup set; no pre-upgrade snapshot.");
        }
        eprint!("Continue? [y/N]: ");
        use std::io::Write;
        std::io::stderr().flush().ok();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
            eprintln!("isd upgrade: aborted.");
            return Ok(());
        }
    }

    // 3. Auto-backup.
    let backup_path = if args.skip_backup {
        None
    } else {
        let path = std::env::temp_dir().join(format!(
            "iso-upgrade-pre-{}.tgz.age",
            chrono::Local::now().format("%Y%m%d%H%M")
        ));
        crate::backup_cmd::run(
            crate::backup_cmd::BackupArgs {
                out: Some(path.clone()),
                ..Default::default()
            },
            context,
        )
        .await
        .context("taking pre-upgrade backup (re-run with --skip-backup to bypass)")?;
        Some(path)
    };

    // 4. Pull both images at the target tag.
    eprintln!("isd upgrade: pulling {CONTROLLER_IMAGE}:{target_tag}");
    pull_image(&docker, CONTROLLER_IMAGE, &target_tag).await?;
    eprintln!("isd upgrade: pulling {AGENT_IMAGE}:{target_tag}");
    pull_image(&docker, AGENT_IMAGE, &target_tag).await?;

    // 5. docker compose up -d against the embedded recipe with the target tag.
    eprintln!("isd upgrade: recreating containers via docker compose up -d");
    compose_up(&docker_uri, &target_tag, backup_path.as_deref())?;

    // 6. Health-check via discovery + GET /api/v1/hosts.
    wait_for_controller_ready(&docker, std::time::Duration::from_secs(90))
        .await
        .map_err(|e| {
            let restore_hint = backup_path
                .as_ref()
                .map(|p| format!("isd restore {}", p.display()))
                .unwrap_or_else(|| "<no backup taken; recreate via `isd init --force`>".into());
            anyhow!(
                "{e}\n\
                 Cluster did not come back healthy. Inspect with `docker logs iso-controller`. \
                 Roll back with `{restore_hint}`."
            )
        })?;

    println!("Upgraded to {target_tag}. Cluster healthy.");
    if let Some(p) = backup_path {
        println!("Pre-upgrade backup at {}.", p.display());
    }
    Ok(())
}

/// `docker inspect iso-controller` and parse the tag out of the
/// `Config.Image` field (operator-visible spec like
/// `ghcr.io/weavers-engineering/isengard-controller:next`). Falls back
/// to the top-level `Image` field (digest form) only if Config is empty.
async fn inspect_current_tag(docker: &isd_runtime::DockerBackend) -> Result<String> {
    let inspect = docker
        .client()
        .inspect_container("iso-controller", None)
        .await
        .context(
            "inspecting iso-controller (is the controller running? bootstrap with `isd init`)",
        )?;

    let image_ref = inspect
        .config
        .as_ref()
        .and_then(|c| c.image.clone())
        .or(inspect.image)
        .ok_or_else(|| anyhow!("iso-controller has no Image field; cannot detect current tag"))?;

    Ok(parse_tag(&image_ref))
}

/// Extract the tag from an image reference. Handles:
///   - `name:tag` -> `tag`
///   - `registry/name:tag` -> `tag`
///   - `registry:port/name:tag` -> `tag` (the rightmost `:` segment that
///     follows the last `/` is the tag)
///   - `name@sha256:...` -> `latest` (digest pin, no tag visible)
///   - `name` (no colon) -> `latest`
fn parse_tag(image_ref: &str) -> String {
    // Digest pin: no tag.
    if let Some((before_at, _)) = image_ref.split_once('@') {
        return parse_tag_no_digest(before_at);
    }
    parse_tag_no_digest(image_ref)
}

/// Inner tag parser. `parse_tag` strips the digest pin first, then
/// hands the registry/name/tag form to this helper.
fn parse_tag_no_digest(image_ref: &str) -> String {
    // Find the position of the last `/` (separates registry from name).
    let last_slash = image_ref.rfind('/').map(|i| i + 1).unwrap_or(0);
    let name_part = &image_ref[last_slash..];
    match name_part.split_once(':') {
        Some((_, tag)) if !tag.is_empty() => tag.to_string(),
        _ => "latest".to_string(),
    }
}

/// `docker pull <image>:<tag>`. Drains the bollard progress stream;
/// surfaces any item-level error verbatim.
async fn pull_image(docker: &isd_runtime::DockerBackend, image: &str, tag: &str) -> Result<()> {
    use bollard::image::CreateImageOptions;
    use futures_util::StreamExt;

    let from_image = format!("{image}:{tag}");
    let mut pull = docker.client().create_image(
        Some(CreateImageOptions {
            from_image: from_image.clone(),
            ..Default::default()
        }),
        None,
        None,
    );
    while let Some(item) = pull.next().await {
        item.with_context(|| format!("pulling {from_image}"))?;
    }
    Ok(())
}

/// Run `docker compose -f <embedded-recipe> up -d` against the operator's
/// docker endpoint with `ISENGARD_IMAGE_TAG=<target>` set in the
/// subprocess environment. Compose's `${ISENGARD_IMAGE_TAG:-next}`
/// interpolation in the recipe steers the controller + agent images to
/// the target tag, and `up -d` recreates a container when its image
/// reference changed.
fn compose_up(
    docker_uri: &str,
    target_tag: &str,
    backup_path: Option<&std::path::Path>,
) -> Result<()> {
    use std::io::Write;

    let mut tmp =
        tempfile::NamedTempFile::new().context("creating tmp file for embedded compose recipe")?;
    tmp.write_all(crate::init_cmd::EMBEDDED_COMPOSE.as_bytes())
        .context("writing embedded compose to tmp")?;
    tmp.flush().ok();

    // Use a blocking spawn so we can `?` directly; compose is short.
    let status = std::process::Command::new("docker")
        .env("DOCKER_HOST", docker_uri)
        .env("ISENGARD_IMAGE_TAG", target_tag)
        .arg("compose")
        .arg("-f")
        .arg(tmp.path())
        .arg("up")
        .arg("-d")
        .status()
        .context("invoking `docker compose up -d`")?;

    if !status.success() {
        let restore_hint = backup_path
            .map(|p| format!("isd restore {}", p.display()))
            .unwrap_or_else(|| "<no backup taken>".into());
        return Err(anyhow!(
            "`docker compose up -d` failed (exit {:?}). \
             Roll back with `{restore_hint}` and inspect with `docker logs iso-controller`.",
            status.code()
        ));
    }
    Ok(())
}

/// Poll until the upgraded controller answers `GET /api/v1/hosts`.
/// 90 second deadline by default; bumps with each successful upgrade.
async fn wait_for_controller_ready(
    docker: &isd_runtime::DockerBackend,
    timeout: std::time::Duration,
) -> Result<()> {
    use tokio::time::{Duration, Instant, sleep};

    let deadline = Instant::now() + timeout;
    eprint!("isd upgrade: waiting for controller to become ready");
    loop {
        if Instant::now() > deadline {
            eprintln!();
            return Err(anyhow!(
                "controller did not become ready within {}s after upgrade",
                timeout.as_secs()
            ));
        }
        if let Ok(endpoint) = isd_runtime::discover(docker.client()).await {
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
        sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tag_simple() {
        assert_eq!(parse_tag("isengard-controller:next"), "next");
        assert_eq!(parse_tag("isengard-controller:v0.3.5"), "v0.3.5");
    }

    #[test]
    fn parse_tag_with_registry() {
        assert_eq!(
            parse_tag("ghcr.io/weavers-engineering/isengard-controller:next"),
            "next"
        );
        assert_eq!(
            parse_tag("ghcr.io/weavers-engineering/isengard-controller:v0.5.4"),
            "v0.5.4"
        );
    }

    #[test]
    fn parse_tag_with_registry_port() {
        // registry-with-port is the colon-collision worst case for naive
        // splitters: `registry.local:5000/name:tag`.
        assert_eq!(parse_tag("registry.local:5000/name:tag"), "tag");
        assert_eq!(parse_tag("registry.local:5000/name"), "latest");
    }

    #[test]
    fn parse_tag_no_tag_defaults_to_latest() {
        assert_eq!(parse_tag("ghcr.io/foo/bar"), "latest");
        assert_eq!(parse_tag("alpine"), "latest");
    }

    #[test]
    fn parse_tag_digest_pin_defaults_to_latest() {
        // `name@sha256:...` is a digest pin; no human-readable tag is
        // visible. Default to `latest` so the operator can override.
        assert_eq!(parse_tag("ghcr.io/foo/bar@sha256:abcd1234"), "latest");
        assert_eq!(parse_tag("ghcr.io/foo/bar:next@sha256:abcd1234"), "next");
    }
}
