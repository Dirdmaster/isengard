//! `isd upgrade`: pull a new controller + agent image tag, recreate the
//! containers, health-check the resulting cluster.
//!
//! Default takes an encrypted backup first (same pipeline as `isd backup`)
//! to `<tmpdir>/iso-upgrade-pre-<date>.tgz.age`. On failure, the error
//! surfaces the backup path + `docker logs isd-controller` so the operator
//! can roll back via `isd restore <path>`.
//!
//! Step machine:
//!   1. Detect the current tag from `docker inspect isd-controller`
//!      (parses `Config.Image`, falls back to `Image` if needed).
//!   2. Compute the target tag (operator-supplied via `--tag`, or
//!      re-pull the current tag for image-digest refresh).
//!   3. Confirm prompt (skipped with `--yes`).
//!   4. Auto-backup unless `--skip-backup`.
//!   5. `docker pull` controller + agent images at the target tag.
//!   6. `docker compose up -d` against the embedded recipe with
//!      `ISENGARD_IMAGE_TAG=<target>` so compose recreates containers
//!      whose image reference changed. State volumes (isd-controller-state,
//!      isd-agent-state, isd-stacks) survive because they are external
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
    /// Pin to a specific image tag.
    ///
    /// Default: re-pull the current tag (refreshes a moving target
    /// like `:next` to the latest digest).
    #[arg(long)]
    pub tag: Option<String>,
    /// Skip the auto-backup before upgrade.
    ///
    /// Risks data loss if the upgraded controller fails to come back up.
    #[arg(long)]
    pub skip_backup: bool,
    /// Skip the confirm prompt.
    #[arg(long)]
    pub yes: bool,
    /// Seconds to wait for the controller to answer after recreate.
    ///
    /// First-boot migrations on a non-trivial DB plus image pull on
    /// a slow link can easily eat 90s; default bumped to 240s so the
    /// poll outlasts a realistic upgrade rather than aborting halfway.
    #[arg(long, default_value_t = 240)]
    pub wait_secs: u64,
    /// Return as soon as the containers are recreated; skip the
    /// readiness poll.
    ///
    /// Use when you know the upgrade will take longer than the wait
    /// window (large DB migrations, slow disk) and you want to watch
    /// progress with `isd logs isd-controller` yourself.
    #[arg(long)]
    pub no_wait: bool,
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

    // Open the cliclack ladder: every subsequent step renders as a
    // completed ◇ entry under this intro, and the active step shows
    // an animated ● spinner with our embedded braille progress bar.
    cliclack::intro(format!("isd upgrade  {current_tag} \u{2192} {target_tag}"))?;

    // 3. Auto-backup.
    let backup_path = if args.skip_backup {
        cliclack::log::step("backup skipped (--skip-backup)")?;
        None
    } else {
        let path = std::env::temp_dir().join(format!(
            "iso-upgrade-pre-{}.tgz.age",
            chrono::Local::now().format("%Y%m%d%H%M")
        ));
        let sp = cliclack::spinner();
        sp.start("taking pre-upgrade backup");
        crate::backup_cmd::run(
            crate::backup_cmd::BackupArgs {
                out: Some(path.clone()),
                ..Default::default()
            },
            context,
        )
        .await
        .context("taking pre-upgrade backup (re-run with --skip-backup to bypass)")?;
        sp.stop(format!("backup at {}", path.display()));
        Some(path)
    };

    // 4. Pull both images at the target tag.
    let sp = cliclack::spinner();
    sp.start(format!("pulling controller:{target_tag}"));
    pull_image(&docker, CONTROLLER_IMAGE, &target_tag).await?;
    sp.stop(format!("pulled controller:{target_tag}"));

    let sp = cliclack::spinner();
    sp.start(format!("pulling agent:{target_tag}"));
    pull_image(&docker, AGENT_IMAGE, &target_tag).await?;
    sp.stop(format!("pulled agent:{target_tag}"));

    // 5. docker compose up -d against the embedded recipe with the target tag.
    let sp = cliclack::spinner();
    sp.start("recreating containers");
    compose_up(&docker_uri, &target_tag, backup_path.as_deref())?;
    sp.stop("containers recreated");

    // 6. Health-check via Session (which handles SSH tunneling and
    //    TLS-pinned contexts) + GET /api/v1/hosts. Skipped entirely
    //    when --no-wait is set.
    if args.no_wait {
        cliclack::log::warning(
            "readiness poll skipped (--no-wait); tail `isd logs isd-controller` to watch boot",
        )?;
        cliclack::outro(format!("Upgraded to {target_tag}."))?;
    } else {
        match wait_for_controller_ready(
            &docker,
            context,
            std::time::Duration::from_secs(args.wait_secs),
        )
        .await
        {
            Ok(elapsed) => {
                cliclack::outro(format!(
                    "Upgraded to {target_tag}. Cluster healthy in {elapsed}s."
                ))?;
            }
            Err(e) => {
                let restore_hint = backup_path
                    .as_ref()
                    .map(|p| format!("isd restore {}", p.display()))
                    .unwrap_or_else(|| "<no backup taken; recreate via `isd init --force`>".into());
                cliclack::outro_cancel(format!(
                    "{e}\nTail `isd logs isd-controller` to watch progress. Roll back with `{restore_hint}`."
                ))?;
                return Err(e);
            }
        }
    }
    Ok(())
}

/// `docker inspect isd-controller` and parse the tag out of the
/// `Config.Image` field (operator-visible spec like
/// `ghcr.io/weavers-engineering/isengard-controller:next`). Falls back
/// to the top-level `Image` field (digest form) only if Config is empty.
async fn inspect_current_tag(docker: &isd_runtime::DockerBackend) -> Result<String> {
    let inspect = docker
        .client()
        .inspect_container("isd-controller", None)
        .await
        .context(
            "inspecting isd-controller (is the controller running? bootstrap with `isd init`)",
        )?;

    let image_ref = inspect
        .config
        .as_ref()
        .and_then(|c| c.image.clone())
        .or(inspect.image)
        .ok_or_else(|| anyhow!("isd-controller has no Image field; cannot detect current tag"))?;

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
             Roll back with `{restore_hint}` and inspect with `docker logs isd-controller`.",
            status.code()
        ));
    }
    Ok(())
}

/// Poll until the upgraded controller answers `GET /api/v1/hosts`.
/// Renders an indicatif progress bar with braille fill characters
/// that ticks as `elapsed/timeout` grows, plus a live phase label
/// derived from tailing the controller logs. Beats the old
/// dot-and-pray UX where the operator couldn't tell if the boot was
/// progressing or hung. Default deadline is operator-tunable via
/// `--wait-secs`.
async fn wait_for_controller_ready(
    docker: &isd_runtime::DockerBackend,
    context: Option<&str>,
    timeout: std::time::Duration,
) -> Result<u64> {
    use tokio::time::{Duration, Instant, sleep};

    let deadline = Instant::now() + timeout;
    let started = Instant::now();
    let total = timeout.as_secs();
    let sp = cliclack::spinner();
    sp.start("controller booting");

    let mut phase: &'static str = "booting";
    loop {
        let elapsed = started.elapsed().as_secs().min(total);
        if let Some(new_phase) = peek_controller_phase(docker).await {
            phase = new_phase;
        }
        let bar = render_braille_bar(elapsed, total, 20);
        sp.set_message(format!(
            "controller {phase}\n\u{2502}   {bar}  {elapsed}s/{total}s"
        ));

        if Instant::now() > deadline {
            sp.error(format!(
                "controller did not answer in {total}s (last: {phase})"
            ));
            return Err(anyhow!(
                "controller did not become ready within {total}s after upgrade (last log phase: {phase})"
            ));
        }
        // Open a Session each iteration so the SSH tunnel is rebuilt
        // when the controller comes up on a fresh container (port may
        // have changed; the previous session's tunnel is stale). The
        // probe goes through the same transport every other isd verb
        // uses, so SSH-backed contexts work the same as direct ones.
        if let Ok(session) = crate::session::Session::open(context).await {
            if let Ok(url) = session.require_controller() {
                let probe = format!("{url}/api/v1/hosts");
                if let Ok(resp) = session.client.get(&probe).send().await {
                    if resp.status().is_success() {
                        sp.stop("controller ready");
                        return Ok(elapsed);
                    }
                }
            }
        }
        sleep(Duration::from_secs(1)).await;
    }
}

/// Render a `width`-cell braille progress bar at `pos/total`. Each
/// cell crawls through 8 sub-steps as the fill grows so even narrow
/// bars feel fluid. Trailing cells use the soft `\u{2840}` (⡀)
/// baseline rather than blank space; matches the chosen ladder
/// design preview.
fn render_braille_bar(pos: u64, total: u64, width: usize) -> String {
    if total == 0 {
        return "\u{2840}".repeat(width);
    }
    let progress = (pos as f64 / total as f64).clamp(0.0, 1.0);
    let total_subcells = (width * 8) as f64;
    let filled_subcells = (progress * total_subcells).round() as usize;
    let full_cells = filled_subcells / 8;
    let partial = filled_subcells % 8;
    let mut out = String::with_capacity(width * 3);
    for i in 0..width {
        if i < full_cells {
            out.push('\u{28FF}'); // ⣿
        } else if i == full_cells && partial > 0 {
            out.push(match partial {
                1 => '\u{2840}', // ⡀
                2 => '\u{2844}', // ⡄
                3 => '\u{2846}', // ⡆
                4 => '\u{2847}', // ⡇
                5 => '\u{28C7}', // ⣇
                6 => '\u{28E7}', // ⣧
                7 => '\u{28F7}', // ⣷
                _ => '\u{28FF}',
            });
        } else {
            out.push('\u{2840}'); // ⡀ soft tail
        }
    }
    out
}

#[cfg(test)]
mod render_bar_tests {
    use super::render_braille_bar;

    #[test]
    fn empty_total_renders_soft_baseline() {
        let b = render_braille_bar(0, 0, 4);
        assert_eq!(b.chars().count(), 4);
        assert!(b.chars().all(|c| c == '\u{2840}'));
    }

    #[test]
    fn full_progress_renders_full_cells() {
        let b = render_braille_bar(10, 10, 4);
        assert_eq!(b.chars().count(), 4);
        assert!(b.chars().all(|c| c == '\u{28FF}'));
    }

    #[test]
    fn half_progress_fills_left_half() {
        let b = render_braille_bar(5, 10, 4);
        let cs: Vec<_> = b.chars().collect();
        assert_eq!(cs.len(), 4);
        assert_eq!(cs[0], '\u{28FF}');
        assert_eq!(cs[1], '\u{28FF}');
        assert_eq!(cs[2], '\u{2840}');
        assert_eq!(cs[3], '\u{2840}');
    }
}

/// Tail the last few lines of the controller container's logs and
/// map them onto a coarse phase the operator recognises. Best-effort:
/// any error short-circuits to `None` and the spinner keeps its
/// previous phase. Phases are stable strings; the matcher is the
/// only place that knows about controller-internal log markers.
async fn peek_controller_phase(docker: &isd_runtime::DockerBackend) -> Option<&'static str> {
    use futures_util::stream::StreamExt;
    let opts = bollard::container::LogsOptions::<String> {
        follow: false,
        stdout: true,
        stderr: true,
        tail: "40".to_string(),
        ..Default::default()
    };
    let mut stream = docker.client().logs("isd-controller", Some(opts));
    let mut blob = String::new();
    while let Some(chunk) = stream.next().await {
        if let Ok(c) = chunk {
            blob.push_str(&c.to_string());
        }
    }
    // Walk from newest to oldest; the latest match wins.
    for line in blob.lines().rev() {
        if line.contains("gRPC server listening") || line.contains("agent connected") {
            return Some("almost ready (server listening)");
        }
        if line.contains("loading controller plugin") {
            return Some("loading plugins");
        }
        if line.contains("applying migration") || line.contains("migration error") {
            return Some("running migrations");
        }
        if line.contains("opening inventory") {
            return Some("opening database");
        }
        if line.contains("starting controller") {
            return Some("starting");
        }
    }
    None
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
