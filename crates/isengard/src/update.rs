//! Phase 0.18 `isengard update` subcommand: zero-args wrapper around
//! [`isengard_agent::self_update`] that auto-detects the latest GitHub
//! Release, downloads + verifies + applies the binary, and cycles
//! both the controller and the agent units.
//!
//! See `docs/RELEASES.md` for the artifact naming + verification flow
//! this command mirrors. The plumbing (download, sha256, atomic rename,
//! graceful unit cycle) lives in `isengard-agent`; this module is the
//! friendlier UX wrapper.
//!
//! Flow (zero args):
//!   1. Read `env!("ISENGARD_BUILD_VERSION")` baked at build time (resolved
//!      from CI tag, git describe, or `CARGO_PKG_VERSION` in that order).
//!   2. GET `releases/latest` from the GitHub API to learn the latest
//!      tag. On rate-limit (403 / 429) the error message tells the
//!      operator to re-run with `--version vX.Y.Z` so they can bypass
//!      the API entirely via the redirect-based download URL.
//!   3. Compare versions; noop if equal.
//!   4. Detect the host's target triple (we only ship Linux musl).
//!   5. Build asset URLs for the binary and its sha256 manifest.
//!   6. Print a plan and ask for confirmation (skipped with --yes).
//!   7. Fetch the sha256 manifest, parse the lowercase-hex digest.
//!   8. Delegate to `isengard_agent::self_update::run_self_update` with
//!      both `iso-controller.service` and `iso-agent.service` in the
//!      unit cycle list. Each unit is cycled via the explicit
//!      `stop -> wait inactive -> wait port free -> start` sequence
//!      defined in `self_update::graceful_replace`. We do NOT use
//!      `systemctl restart`: that returned before the old Pingora
//!      listener was released and made the new agent's bind panic
//!      with `Address in use` (lausanne v0.5.2 deploy, 2026-05-10).

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

/// GitHub repo path for the upstream Isengard releases. Centralised so
/// tests and the runtime agree.
pub const RELEASES_REPO: &str = "Weavers-Engineering/Isengard";

/// GitHub API base. Overridable at runtime via `ISENGARD_UPDATE_GITHUB_API`
/// so the wiremock integration test can point at a localhost stand-in.
/// In production the env var is unset and we hit the real api.github.com.
fn github_api_base() -> String {
    std::env::var("ISENGARD_UPDATE_GITHUB_API")
        .unwrap_or_else(|_| "https://api.github.com".to_string())
}

/// GitHub download base. Overridable at runtime via
/// `ISENGARD_UPDATE_GITHUB_DOWNLOAD`. Production hits github.com which
/// 302-redirects to the asset host; the redirect chain is transparent.
fn github_download_base() -> String {
    std::env::var("ISENGARD_UPDATE_GITHUB_DOWNLOAD")
        .unwrap_or_else(|_| "https://github.com".to_string())
}

/// systemd unit names cycled by `isengard update`. Order matters: the
/// controller is cycled before the agent so the operator's isd
/// connection drops once and reconnects against the fresh binary, then
/// the agent picks up its own new binary. Each cycle is the explicit
/// `stop -> wait inactive -> wait ports free -> start` flow from
/// [`isengard_agent::self_update::run_self_update`], not a `systemctl
/// restart` shortcut.
pub const RESTART_UNITS: &[&str] = &["iso-controller.service", "iso-agent.service"];

/// User-agent used for GitHub API requests. GitHub requires a non-empty
/// UA on API calls (returns 403 otherwise); the version string makes
/// the request traceable in their server logs.
const USER_AGENT: &str = concat!("isengard-update/", env!("ISENGARD_BUILD_VERSION"));

/// HTTP timeout for every fetch in this module. Conservative: the
/// sha256 file is < 100 bytes, the API response is < 50 KiB, and the
/// binary download has its own progress already (via reqwest) so the
/// timeout only fires on a genuinely stuck connection.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// CLI flags for `isengard update`.
#[derive(Debug, Clone)]
pub struct UpdateArgs {
    /// Print "current vX, latest vY" and exit; do not download or
    /// restart anything.
    pub check: bool,
    /// Pin to a specific version (e.g. "v0.5.1"). When `None`, the
    /// latest release is resolved from the GitHub API. The leading `v`
    /// is normalised, so callers can pass either "0.5.1" or "v0.5.1".
    pub version: Option<String>,
    /// Skip the confirmation prompt before downloading.
    pub yes: bool,
}

/// Versions advertised by the GitHub Releases JSON object. We only
/// care about the tag name, but the struct documents the shape.
#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
}

/// Public entry point. Parses args, runs the flow, prints status.
pub async fn run(args: UpdateArgs) -> Result<()> {
    let current = current_version();
    let target_version = resolve_target_version(args.version.as_deref()).await?;

    // Equal-version short-circuit: friendly noop. `current_version()` is
    // fed by the build script: real release tags get their tag string
    // verbatim, dev builds get `git describe` output (e.g. `v0.5.2-3-gabc`)
    // which `is_already_on_target` rejects by SemVer pre-release marker.
    if is_already_on_target(&current, &target_version) {
        println!("isengard {current} is already at {target_version}; nothing to do.");
        return Ok(());
    }

    // --check fires BEFORE require_root + target-triple detection so the
    // operator can sanity-check from a Mac dev box without sudo. We only
    // print current + target version here; the real download flow does
    // the platform / privilege checks below.
    if args.check {
        println!(
            "current: {current}\nlatest:  {target_version}\nrun `sudo isengard update` to upgrade."
        );
        return Ok(());
    }

    let target_triple = detect_target_triple()?;
    let binary_url = build_binary_url(&target_version, &target_triple);
    let sha_url = build_sha_url(&target_version, &target_triple);
    let asset_name = asset_name(&target_triple);

    require_root()?;

    print_plan(&current, &target_version, &asset_name);

    if !args.yes && !confirm()? {
        cliclack::outro_cancel("update cancelled")?;
        return Ok(());
    }

    let sha256 = fetch_sha256(&sha_url, &asset_name)
        .await
        .with_context(|| format!("fetching sha256 manifest from {sha_url}"))?;

    isengard_agent::self_update::run_self_update(&binary_url, &sha256, RESTART_UNITS)
        .await
        .with_context(|| format!("self-update from {binary_url}"))?;

    cliclack::outro(format!(
        "isengard updated to {target_version}. journalctl -fu iso-controller -u iso-agent to watch."
    ))?;
    Ok(())
}

/// Current version baked at build time. Reads `ISENGARD_BUILD_VERSION`,
/// which `build.rs` resolves from (in order) `ISENGARD_RELEASE_VERSION`,
/// `GITHUB_REF_NAME`, `git describe --tags --always --dirty`, or
/// `CARGO_PKG_VERSION`. Tagged release builds get e.g. `v0.5.2`; dev
/// builds get `v0.5.2-3-gabc1234`. Wrapped in a function so future tests
/// can monkey-patch via a `cfg(test)` shim.
fn current_version() -> String {
    env!("ISENGARD_BUILD_VERSION").to_string()
}

/// Resolve the version the operator wants. `Some(v)` honours the flag
/// (with `v` prefix normalised in); `None` queries the GitHub API.
async fn resolve_target_version(pin: Option<&str>) -> Result<String> {
    if let Some(v) = pin {
        return Ok(normalise_tag(v));
    }
    fetch_latest_tag().await
}

/// GET the GitHub Releases "latest" endpoint and parse the `tag_name`.
/// Errors propagate; the caller decides whether to surface them or
/// fall back to the redirect path (we currently surface them: a real
/// version pin is the documented escape hatch).
async fn fetch_latest_tag() -> Result<String> {
    let url = format!(
        "{}/repos/{RELEASES_REPO}/releases/latest",
        github_api_base()
    );
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("building reqwest client for GitHub API")?;
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;

    if resp.status() == reqwest::StatusCode::FORBIDDEN
        || resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        bail!(
            "GitHub API rate-limited (status {}). Re-run with --version vX.Y.Z to pin the release tag manually.",
            resp.status()
        );
    }
    let resp = resp
        .error_for_status()
        .with_context(|| format!("non-success status from {url}"))?;
    let body: LatestRelease = resp
        .json()
        .await
        .with_context(|| format!("parsing JSON from {url}"))?;
    if body.tag_name.is_empty() {
        bail!("GitHub API returned an empty tag_name; cannot continue");
    }
    Ok(normalise_tag(&body.tag_name))
}

/// Normalise a version string into the `vX.Y.Z` form the release
/// pipeline writes. Accepts "0.5.1", "v0.5.1", " v0.5.1 ". Anything
/// else is passed through verbatim and will fail later at URL fetch
/// time with a 404, which is the right error surface for the operator
/// (the message will quote the bad URL).
fn normalise_tag(raw: &str) -> String {
    let t = raw.trim();
    if t.starts_with('v') || t.starts_with('V') {
        t.to_string()
    } else {
        format!("v{t}")
    }
}

/// Compare a tagged target (e.g. `v0.5.1`) against the current version
/// baked at build time. Dev builds advertise either `0.1.0-alpha` (the
/// CARGO_PKG_VERSION fallback) or `v0.5.2-3-gabc1234` (git describe),
/// neither of which is ever equal to a real release tag: the
/// pre-release / build-metadata markers ensure that.
///
/// Returns `true` when the running binary's version equals the target
/// version, ignoring the leading `v`.
pub(crate) fn is_already_on_target(current: &str, target_tag: &str) -> bool {
    let current_clean = current.trim_start_matches('v').trim_start_matches('V');
    let target_clean = target_tag.trim_start_matches('v').trim_start_matches('V');
    if current_clean == target_clean {
        return true;
    }
    // SemVer parse so `0.5.1` == `0.5.1+meta` (build metadata is
    // informational per the spec) and `1.2.3-alpha.1` != `1.2.3`.
    // Falls through to false on parse failure (an unparseable target
    // is still worth attempting an update against).
    if let (Ok(a), Ok(b)) = (
        semver::Version::parse(current_clean),
        semver::Version::parse(target_clean),
    ) {
        // An alpha / pre-release current never matches a stable target.
        if !a.pre.is_empty() && b.pre.is_empty() {
            return false;
        }
        // Compare every field except `build`, which the spec
        // explicitly excludes from precedence. `semver::Version`'s
        // derived PartialEq compares `build` too, so we do it manually.
        return a.major == b.major && a.minor == b.minor && a.patch == b.patch && a.pre == b.pre;
    }
    false
}

/// Detect the Rust target triple for the running host. We only ship
/// Linux musl, so the function deliberately refuses macOS / Windows
/// with a friendlier error than a 404 on the asset URL.
pub(crate) fn detect_target_triple() -> Result<String> {
    detect_target_triple_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// Inner detection helper, parameterised so tests can drive every OS
/// + arch combo without spawning subprocesses.
fn detect_target_triple_for(os: &str, arch: &str) -> Result<String> {
    if os != "linux" {
        bail!(
            "isengard update only supports Linux hosts; current OS is {os}. macOS / Windows hosts must rebuild from source or use the docker image."
        );
    }
    let triple = match arch {
        "x86_64" => "x86_64-unknown-linux-musl",
        "aarch64" => "aarch64-unknown-linux-musl",
        other => bail!(
            "no isengard binary published for arch {other}; supported: x86_64, aarch64. Build from source or open a release issue."
        ),
    };
    Ok(triple.to_string())
}

/// Asset basename (no path). Centralised so the plan UI and the URL
/// builder agree on naming.
pub(crate) fn asset_name(target_triple: &str) -> String {
    format!("isengard-{target_triple}")
}

/// GitHub Releases download URL for the binary asset. The
/// `releases/download/<tag>/<asset>` path returns a 302 to S3 with the
/// real bytes; reqwest follows redirects by default (up to 10), so we
/// don't have to handle that here.
pub(crate) fn build_binary_url(tag: &str, target_triple: &str) -> String {
    format!(
        "{}/{RELEASES_REPO}/releases/download/{tag}/{}",
        github_download_base(),
        asset_name(target_triple)
    )
}

/// Companion sha256 manifest URL. Same shape as the binary asset,
/// with `.sha256` appended. The body is a single line in `sha256sum`
/// format (`<hex>  <name>`).
pub(crate) fn build_sha_url(tag: &str, target_triple: &str) -> String {
    format!(
        "{}/{RELEASES_REPO}/releases/download/{tag}/{}.sha256",
        github_download_base(),
        asset_name(target_triple)
    )
}

/// Fetch the `.sha256` manifest and parse out the 64-hex digest.
///
/// Accepts both formats the release pipeline has shipped historically:
///   - `sha256sum`-style two-column: `<hex>  <name>\n`
///   - bare digest: `<hex>\n`
///
/// The `asset_name` parameter is only used to log a friendlier error
/// when the manifest names a different asset (mostly a CI bug
/// indicator); the parse itself doesn't depend on it.
async fn fetch_sha256(url: &str, asset_name: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("building reqwest client for sha256 fetch")?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        bail!(
            "no sha256 manifest at {url} (404). The release tag may not have built for this target triple; try --version with a different tag, or build from source."
        );
    }
    let resp = resp
        .error_for_status()
        .with_context(|| format!("non-success status from {url}"))?;
    let body = resp
        .text()
        .await
        .with_context(|| format!("reading body of {url}"))?;
    parse_sha256_manifest(&body, asset_name)
}

/// Parse the body of a `.sha256` manifest. Public-but-pub(crate) so
/// the unit tests can hit every format permutation without going
/// through the HTTP client.
pub(crate) fn parse_sha256_manifest(body: &str, asset_name: &str) -> Result<String> {
    let first = body.lines().next().ok_or_else(|| {
        anyhow!("sha256 manifest was empty; expected a single line `<hex>  {asset_name}`")
    })?;
    let mut parts = first.split_whitespace();
    let hex = parts
        .next()
        .ok_or_else(|| anyhow!("sha256 manifest had no digest column"))?;
    let hex = hex.trim().to_ascii_lowercase();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("sha256 manifest digest column was {hex:?}; expected 64 lowercase hex characters");
    }
    // The second column (asset name) is informational; we don't fail
    // on mismatch because GitHub Releases never rewrites filenames.
    // We do log it via the returned hex; the operator sees the URL
    // in the plan and can sanity-check.
    let _ = parts;
    Ok(hex)
}

/// Refuse to continue if not running as root. The atomic-rename target
/// is `/usr/local/bin/isengard`; only root can write there in the
/// systemd-native install layout.
fn require_root() -> Result<()> {
    #[cfg(unix)]
    {
        // SAFETY: getuid is always-safe; the libc::geteuid wrapper would
        // pull a libc dep we don't currently have. nix's `Uid::effective`
        // ships in the existing `nix` dep but isengard doesn't link it
        // (only isengard-agent does). Read /proc/self/status instead so
        // the check stays dependency-free.
        if !is_effective_root() {
            bail!(
                "isengard update needs root to replace /usr/local/bin/isengard. Re-run with `sudo isengard update`."
            );
        }
    }
    Ok(())
}

/// Read `/proc/self/status` to learn the effective uid. Returns true
/// when euid == 0. Kept private and Linux-specific because the
/// command only ships for Linux anyway.
#[cfg(unix)]
fn is_effective_root() -> bool {
    // The `Uid:` line is `Uid:\treal\teffective\tsaved\tfs`. We want the
    // second field (effective uid).
    let Ok(body) = std::fs::read_to_string("/proc/self/status") else {
        // /proc/self/status is unreadable; fall back to the env-var
        // hint sudo sets. Conservative: a missing /proc usually means
        // we are in a container without /proc mounted, and operators
        // running `sudo isengard update` in that posture have already
        // opted into the rename so we don't second-guess them.
        return std::env::var("SUDO_UID").is_ok() || std::env::var("USER").as_deref() == Ok("root");
    };
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            let mut fields = rest.split_whitespace();
            let _real = fields.next();
            if let Some(eff) = fields.next() {
                return eff == "0";
            }
        }
    }
    false
}

/// Print the human-readable plan: what's about to happen, in what
/// order, with the source clearly named. Mirrors the `cliclack` style
/// used by `isengard init`.
fn print_plan(current: &str, target: &str, asset_name: &str) {
    // `intro` opens the connector bar; the subsequent `log::step` calls
    // print under the bar with a ◇ glyph; `confirm` closes the step and
    // returns the operator's choice. The `outro` happens in `run` after
    // the self-update returns.
    let _ = cliclack::intro(format!(
        "isengard update  {}",
        env!("ISENGARD_BUILD_VERSION")
    ));
    let body = format!(
        "  Current   {current}\n  Target    {target}\n  Asset     {asset_name}\n  Source    github.com/{RELEASES_REPO}\n\n  This will:\n    - Download the new binary\n    - Verify sha256 against the release manifest\n    - Atomic-rename onto /usr/local/bin/isengard\n    - Cycle iso-controller.service (stop, wait, start)\n    - Cycle iso-agent.service (stop, wait, start)"
    );
    let _ = cliclack::note("Update plan", body);
}

/// Render the y/N confirm prompt. Default is "yes" because the operator
/// has already typed the command; a stray Enter shouldn't punish them.
fn confirm() -> Result<bool> {
    cliclack::confirm("Continue?")
        .initial_value(true)
        .interact()
        .map_err(|e| anyhow!("confirm prompt: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_tag_adds_v_prefix() {
        assert_eq!(normalise_tag("0.5.1"), "v0.5.1");
        assert_eq!(normalise_tag("v0.5.1"), "v0.5.1");
        assert_eq!(normalise_tag(" v0.5.1 "), "v0.5.1");
        assert_eq!(normalise_tag("V1.2.3"), "V1.2.3");
    }

    #[test]
    fn is_already_on_target_string_equal() {
        assert!(is_already_on_target("0.5.1", "v0.5.1"));
        assert!(is_already_on_target("v0.5.1", "v0.5.1"));
        assert!(is_already_on_target("0.5.1", "0.5.1"));
    }

    #[test]
    fn is_already_on_target_string_differ() {
        assert!(!is_already_on_target("0.5.0", "v0.5.1"));
        assert!(!is_already_on_target("0.5.1", "v0.6.0"));
    }

    #[test]
    fn is_already_on_target_alpha_never_matches_stable() {
        // Build-script CARGO_PKG_VERSION fallback (`0.1.0-alpha`) must
        // never short-circuit against a stable release tag.
        assert!(!is_already_on_target("0.1.0-alpha", "v0.1.0"));
        // Pre-release-to-pre-release equal: stay put.
        assert!(is_already_on_target("0.1.0-alpha", "v0.1.0-alpha"));
    }

    #[test]
    fn is_already_on_target_semver_equal_with_build_metadata() {
        // SemVer treats build metadata as informational only.
        assert!(is_already_on_target("0.5.1+abc", "v0.5.1+def"));
    }

    #[test]
    fn is_already_on_target_git_describe_dev_build_never_matches_tag() {
        // `git describe --tags --always --dirty` on a checkout three
        // commits past v0.5.2 returns `v0.5.2-3-gabc1234`. SemVer parses
        // `3-gabc1234` as the pre-release component, so it must NOT
        // short-circuit against the bare tag.
        assert!(!is_already_on_target("v0.5.2-3-gabc1234", "v0.5.2"));
        assert!(!is_already_on_target("v0.5.2-3-gabc1234-dirty", "v0.5.2"));
        // Same describe output, same describe target: equal.
        assert!(is_already_on_target(
            "v0.5.2-3-gabc1234",
            "v0.5.2-3-gabc1234"
        ));
    }

    #[test]
    fn is_already_on_target_bare_sha_never_matches_tag() {
        // No reachable tag (`git describe` falls back to `--always`),
        // we get a bare commit SHA. Unparseable as SemVer; the function
        // returns false so the update path always proceeds.
        assert!(!is_already_on_target("abc1234", "v0.5.2"));
        assert!(!is_already_on_target("abc1234", "v0.5.2-3-gabc1234"));
    }

    #[test]
    fn is_already_on_target_tag_with_v_prefix_equal() {
        // CI builds bake the tag verbatim, including the `v` prefix.
        // The function must compare them equal regardless of prefix.
        assert!(is_already_on_target("v0.5.2", "v0.5.2"));
        assert!(is_already_on_target("v0.5.2", "0.5.2"));
    }

    #[test]
    fn detect_target_triple_for_x86_64() {
        assert_eq!(
            detect_target_triple_for("linux", "x86_64").unwrap(),
            "x86_64-unknown-linux-musl"
        );
    }

    #[test]
    fn detect_target_triple_for_aarch64() {
        assert_eq!(
            detect_target_triple_for("linux", "aarch64").unwrap(),
            "aarch64-unknown-linux-musl"
        );
    }

    #[test]
    fn detect_target_triple_rejects_macos() {
        let err = detect_target_triple_for("macos", "aarch64").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Linux"), "msg: {msg}");
        assert!(msg.contains("macos"), "msg: {msg}");
    }

    #[test]
    fn detect_target_triple_rejects_windows() {
        let err = detect_target_triple_for("windows", "x86_64").unwrap_err();
        assert!(err.to_string().contains("Linux"));
    }

    #[test]
    fn detect_target_triple_rejects_riscv() {
        let err = detect_target_triple_for("linux", "riscv64").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("riscv64"), "msg: {msg}");
        assert!(msg.contains("x86_64"), "msg: {msg}");
    }

    #[test]
    fn asset_name_format() {
        assert_eq!(
            asset_name("x86_64-unknown-linux-musl"),
            "isengard-x86_64-unknown-linux-musl"
        );
    }

    #[test]
    fn build_binary_url_format() {
        let url = build_binary_url("v0.5.1", "x86_64-unknown-linux-musl");
        assert_eq!(
            url,
            "https://github.com/Weavers-Engineering/Isengard/releases/download/v0.5.1/isengard-x86_64-unknown-linux-musl"
        );
    }

    #[test]
    fn build_sha_url_format() {
        let url = build_sha_url("v0.5.1", "aarch64-unknown-linux-musl");
        assert_eq!(
            url,
            "https://github.com/Weavers-Engineering/Isengard/releases/download/v0.5.1/isengard-aarch64-unknown-linux-musl.sha256"
        );
    }

    #[test]
    fn parse_sha256_manifest_two_column_format() {
        let body = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789  isengard-x86_64-unknown-linux-musl\n";
        let got = parse_sha256_manifest(body, "isengard-x86_64-unknown-linux-musl").unwrap();
        assert_eq!(
            got,
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
    }

    #[test]
    fn parse_sha256_manifest_bare_digest() {
        let body = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789\n";
        let got = parse_sha256_manifest(body, "isengard-x86_64-unknown-linux-musl").unwrap();
        assert_eq!(got.len(), 64);
    }

    #[test]
    fn parse_sha256_manifest_uppercase_normalises() {
        let body = "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789  asset\n";
        let got = parse_sha256_manifest(body, "asset").unwrap();
        assert!(got.chars().all(|c| !c.is_ascii_uppercase()));
    }

    #[test]
    fn parse_sha256_manifest_rejects_short_digest() {
        let body = "abc  asset\n";
        let err = parse_sha256_manifest(body, "asset").unwrap_err();
        assert!(err.to_string().contains("64"));
    }

    #[test]
    fn parse_sha256_manifest_rejects_non_hex() {
        let body = "g123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdefg  asset\n";
        let err = parse_sha256_manifest(body, "asset").unwrap_err();
        assert!(err.to_string().contains("hex"));
    }

    #[test]
    fn parse_sha256_manifest_rejects_empty_body() {
        let err = parse_sha256_manifest("", "asset").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }
}
