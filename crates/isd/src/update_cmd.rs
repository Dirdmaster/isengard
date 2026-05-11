//! v0.5.2 `isd update` subcommand: self-replace the operator CLI binary
//! with the latest GitHub release.
//!
//! Mirrors `isengard update` but targets the operator's machine (often
//! macOS) instead of a Linux host. Differences from the agent path:
//!
//!   - Supports both Apple Silicon / Intel Mac and x86_64 / aarch64 Linux.
//!   - Does NOT restart any systemd unit: `isd` is a one-shot CLI. The
//!     freshly-installed binary takes over on the next invocation.
//!   - Inlined download + sha verify + atomic rename. We deliberately
//!     don't depend on `isengard-agent::self_update` here: that crate is
//!     Linux-only systemd plumbing, and pulling it into the operator
//!     binary would drag a transitive dep tree we don't otherwise want.
//!
//! Flow:
//!   1. Read `env!("CARGO_PKG_VERSION")` baked at build time.
//!   2. Resolve the target version: either the `--version` flag or
//!      GitHub Releases' `releases/latest` endpoint.
//!   3. Noop if current == target. SemVer-aware, so build metadata is
//!      ignored and a dev `0.1.0-alpha` never matches a real tag.
//!   4. Detect the host triple from `std::env::consts::{OS, ARCH}`.
//!   5. Build the binary + sha256 manifest URLs under
//!      `releases/download/<tag>/`.
//!   6. Print a `cliclack` plan, prompt to confirm (skipped with `--yes`).
//!   7. Fetch the sha256 manifest, parse the 64-hex digest.
//!   8. Download the binary to `<current_exe>.new`, verify, chmod 0755,
//!      atomic-rename onto `<current_exe>`. Cross-fs (EXDEV) falls back
//!      to copy-then-rename inside the original directory.
//!   9. Print an outro pointing the operator at `isd --version`.
//!
//! Edge cases (matched against the spec):
//!   - `<current_exe>` not writable -> friendly "permission denied" with
//!     the "sudo or move to a writable path" hint.
//!   - GitHub API rate-limited (403 / 429) -> tell the operator to pin
//!     `--version vX.Y.Z` so the redirect-based download URL is used.
//!   - No asset for the host triple -> error names the triple and the
//!     tag and points at "build from source".
//!   - Dev build (`0.1.0-alpha`) -> we never short-circuit; the SemVer
//!     pre-release marker always loses to a stable release tag.
//!   - macOS Gatekeeper / quarantine -> the binary is written via
//!     reqwest's bytes(); no `com.apple.quarantine` xattr is set on the
//!     file, so Gatekeeper doesn't kick in. Operators running unsigned
//!     binaries already accept the assistive-tech / dev-id posture.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Upstream releases repository slug. Centralised so the test mocks and
/// the production URL builder share one source of truth.
pub const RELEASES_REPO: &str = "Weavers-Engineering/Isengard";

/// GitHub API base. Overridable via `ISD_UPDATE_GITHUB_API` so the
/// integration tests can point at a wiremock without monkey-patching
/// the constant. Production hits api.github.com.
fn github_api_base() -> String {
    std::env::var("ISD_UPDATE_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".to_string())
}

/// GitHub download base. Overridable via `ISD_UPDATE_GITHUB_DOWNLOAD`.
/// Production hits github.com which 302-redirects to the asset CDN;
/// reqwest follows redirects transparently.
fn github_download_base() -> String {
    std::env::var("ISD_UPDATE_GITHUB_DOWNLOAD").unwrap_or_else(|_| "https://github.com".to_string())
}

/// HTTP timeout for every fetch in this module. The sha256 file is a
/// single line; the API response is small JSON; the binary download has
/// reqwest's own progress so the timeout only fires on a stuck socket.
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

/// Hard cap on the downloaded binary length. The current isd musl
/// binary is ~50 MiB; the cap is generous but bounded so a hostile or
/// corrupt URL can't OOM the operator's machine.
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// User-agent for GitHub API requests. GitHub rejects empty UAs on the
/// API surface with a 403; embedding the package version makes the
/// request traceable in their server logs and tells us which isd
/// release any rate-limit spike came from.
const USER_AGENT: &str = concat!("isd-update/", env!("CARGO_PKG_VERSION"));

/// CLI flags for `isd update`.
#[derive(Debug, Clone, Args)]
pub struct UpdateArgs {
    /// Print "current: vX, latest: vY" and exit. No download, no rename.
    #[arg(long)]
    pub check: bool,
    /// Pin to a specific release tag (e.g. "v0.5.2"). When omitted, the
    /// latest release is resolved from the GitHub API. The leading `v`
    /// is normalised in: "0.5.2" and "v0.5.2" both work.
    #[arg(long)]
    pub version: Option<String>,
    /// Skip the confirmation prompt. Useful for scripted upgrades.
    #[arg(long)]
    pub yes: bool,
}

/// Shape of the `releases/latest` GitHub API response we care about.
/// We deliberately ignore the rest of the (large) payload.
#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
}

/// Top-level entry point. Async because reqwest is async in this crate.
pub async fn run(args: UpdateArgs) -> Result<()> {
    let current = current_version();
    let target_version = resolve_target_version(args.version.as_deref()).await?;

    if is_already_on_target(&current, &target_version) {
        println!("isd {current} is already at {target_version}; nothing to do.");
        return Ok(());
    }

    // `--check` exits before any platform / write-permission checks so
    // an operator can sanity-check from any host. The download path
    // below handles all the platform-specific bits.
    if args.check {
        println!("current: {current}\nlatest:  {target_version}\nrun `isd update` to upgrade.");
        return Ok(());
    }

    let target_triple = detect_target_triple()?;
    let asset = asset_name(&target_triple);
    let binary_url = build_binary_url(&target_version, &target_triple);
    let sha_url = build_sha_url(&target_version, &target_triple);

    let current_exe =
        std::env::current_exe().context("resolving current isd executable for in-place update")?;

    print_plan(&current, &target_version, &asset, &current_exe);

    if !args.yes && !confirm()? {
        cliclack::outro_cancel("update cancelled")?;
        return Ok(());
    }

    let expected_sha = fetch_sha256(&sha_url, &asset)
        .await
        .with_context(|| format!("fetching sha256 manifest from {sha_url}"))?;

    let staging = staging_path(&current_exe);
    download_and_verify(&binary_url, &staging, &expected_sha)
        .await
        .with_context(|| format!("downloading binary from {binary_url}"))?;

    set_executable(&staging)?;
    install_atomic(&staging, &current_exe)
        .with_context(|| format!("installing {staging:?} -> {current_exe:?}"))?;

    cliclack::outro(format!(
        "isd updated to {target_version}. Run `isd --version` to confirm."
    ))?;
    Ok(())
}

/// Current package version baked at build time. Wrapped so future tests
/// can swap it out (today they only assert on the constant).
fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Resolve the target version from the operator's flag or the GitHub API.
async fn resolve_target_version(pin: Option<&str>) -> Result<String> {
    if let Some(v) = pin {
        return Ok(normalise_tag(v));
    }
    fetch_latest_tag().await
}

/// GET `<api>/repos/<repo>/releases/latest` and parse the `tag_name`.
/// On rate-limit, surfaces a friendlier error pointing at `--version`.
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
            "GitHub API rate-limited (status {}). Re-run with --version vX.Y.Z to pin the release tag manually; the download path uses a redirect URL and bypasses the API.",
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
/// pipeline writes. Accepts "0.5.2", "v0.5.2", " v0.5.2 ". Passes
/// anything else through unchanged; the URL fetch will surface a 404 if
/// the operator made a typo, which is the right error surface.
pub(crate) fn normalise_tag(raw: &str) -> String {
    let t = raw.trim();
    if t.starts_with('v') || t.starts_with('V') {
        t.to_string()
    } else {
        format!("v{t}")
    }
}

/// Compare the current package version against a target tag. Returns
/// true if we're already on that version. SemVer-aware: build metadata
/// is ignored (per the spec), and a pre-release current (e.g. the dev
/// `0.1.0-alpha`) never matches a stable release tag.
pub(crate) fn is_already_on_target(current: &str, target_tag: &str) -> bool {
    let current_clean = current.trim_start_matches('v').trim_start_matches('V');
    let target_clean = target_tag.trim_start_matches('v').trim_start_matches('V');
    if current_clean == target_clean {
        return true;
    }
    if let (Ok(a), Ok(b)) = (
        semver::Version::parse(current_clean),
        semver::Version::parse(target_clean),
    ) {
        // Pre-release current vs stable target: always update.
        if !a.pre.is_empty() && b.pre.is_empty() {
            return false;
        }
        // SemVer's derived PartialEq compares build metadata too; the
        // spec says we shouldn't. Compare every field except `build`.
        return a.major == b.major && a.minor == b.minor && a.patch == b.patch && a.pre == b.pre;
    }
    false
}

/// Detect the host's Rust target triple. macOS uses `darwin` in the
/// triple even though `std::env::consts::OS` is `macos`; we translate.
pub(crate) fn detect_target_triple() -> Result<String> {
    detect_target_triple_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// Inner detection helper, parameterised so unit tests can drive every
/// combo without spawning subprocesses or fudging cfg flags.
fn detect_target_triple_for(os: &str, arch: &str) -> Result<String> {
    let triple = match (os, arch) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        _ => bail!(
            "no isd asset published for {os}/{arch}; supported: macOS aarch64/x86_64, Linux x86_64/aarch64. Build from source: cargo install --path crates/isd --git https://github.com/{RELEASES_REPO}"
        ),
    };
    Ok(triple.to_string())
}

/// Asset basename: `isd-<target-triple>`. The release pipeline writes
/// this filename verbatim under `releases/download/<tag>/`.
pub(crate) fn asset_name(target_triple: &str) -> String {
    format!("isd-{target_triple}")
}

/// Download URL for the binary asset. The `releases/download` path
/// returns a 302 to the asset CDN; reqwest follows it.
pub(crate) fn build_binary_url(tag: &str, target_triple: &str) -> String {
    format!(
        "{}/{RELEASES_REPO}/releases/download/{tag}/{}",
        github_download_base(),
        asset_name(target_triple)
    )
}

/// Companion sha256 manifest URL. Same shape as the binary, with
/// `.sha256` appended. Body is `sha256sum`-style (`<hex>  <name>\n`).
pub(crate) fn build_sha_url(tag: &str, target_triple: &str) -> String {
    format!(
        "{}/{RELEASES_REPO}/releases/download/{tag}/{}.sha256",
        github_download_base(),
        asset_name(target_triple)
    )
}

/// Fetch the `.sha256` manifest and parse the digest.
async fn fetch_sha256(url: &str, asset: &str) -> Result<String> {
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
    parse_sha256_manifest(&body, asset)
}

/// Parse the body of a `.sha256` file. Accepts both the two-column
/// `sha256sum` format and a bare digest. Lower-cases the hex.
pub(crate) fn parse_sha256_manifest(body: &str, asset: &str) -> Result<String> {
    let first = body.lines().next().ok_or_else(|| {
        anyhow!("sha256 manifest was empty; expected a line of the form `<hex>  {asset}`")
    })?;
    let mut parts = first.split_whitespace();
    let hex = parts
        .next()
        .ok_or_else(|| anyhow!("sha256 manifest had no digest column"))?;
    let hex = hex.trim().to_ascii_lowercase();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("sha256 manifest digest column was {hex:?}; expected 64 lowercase hex characters");
    }
    // Second column (asset name) is informational; mismatches there
    // mean a CI bug we want to know about but not block the install on.
    let _ = parts;
    Ok(hex)
}

/// `<exe>.new`, in the same directory as the running binary. Same
/// directory guarantees the rename is on one filesystem and thus
/// atomic in the POSIX sense.
pub(crate) fn staging_path(target: &Path) -> PathBuf {
    let mut p = target.to_path_buf();
    let mut name = p
        .file_name()
        .map(|s| s.to_owned())
        .unwrap_or_else(|| std::ffi::OsString::from("isd"));
    name.push(".new");
    p.set_file_name(name);
    p
}

/// Download `url` to `dest`, compute sha256 streaming. Validates the
/// digest against `expected_sha256` (lowercase hex, 64 chars). On any
/// error, removes the staging file so a partial download doesn't sit
/// around between runs.
async fn download_and_verify(url: &str, dest: &Path, expected_sha256: &str) -> Result<()> {
    let expected = expected_sha256.trim().to_ascii_lowercase();
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("expected_sha256 must be 64 hex characters, got {expected:?}");
    }

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("building reqwest client for binary download")?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("non-success status from {url}"))?;

    if let Some(len) = resp.content_length() {
        if len > MAX_DOWNLOAD_BYTES {
            bail!("download size {len} exceeds cap {MAX_DOWNLOAD_BYTES}; refusing to allocate");
        }
    }

    // Best-effort cleanup of any leftover staging artifact. A previous
    // crashed run can leave one behind; without this the write below
    // would just overwrite, but the explicit unlink makes intent clear.
    let _ = std::fs::remove_file(dest);

    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("reading body of {url}"))?;
    if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
        bail!(
            "downloaded {} bytes; exceeds cap {MAX_DOWNLOAD_BYTES}",
            bytes.len()
        );
    }

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let got_hex = hex::encode(hasher.finalize());
    if got_hex != expected {
        bail!("sha256 mismatch: got {got_hex}, expected {expected}; refusing to install");
    }

    if let Err(e) = std::fs::write(dest, &bytes) {
        let _ = std::fs::remove_file(dest);
        return Err(e).with_context(|| format!("writing staged binary to {dest:?}"));
    }
    Ok(())
}

/// chmod 0755 on Unix. No-op on non-Unix (we don't ship Windows assets;
/// the cfg keeps the test compile working on CI runners).
#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .with_context(|| format!("stat {path:?} for chmod"))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).with_context(|| format!("chmod 0755 on {path:?}"))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Atomic install with cross-filesystem fallback.
///
/// Tries `rename(2)` first, which is atomic on the same filesystem.
/// If that fails with `CrossesDevices` (EXDEV on Linux, e.g. when isd
/// is on a tmpfs and the staging path landed elsewhere) we fall back
/// to copy-then-rename inside the parent dir of the target. The copy
/// uses a fresh staging path under the target's directory to keep the
/// rename atomic.
///
/// Permission-denied errors get a friendlier message pointing the
/// operator at sudo / moving isd to a writable path.
pub(crate) fn install_atomic(staging: &Path, target: &Path) -> Result<()> {
    match std::fs::rename(staging, target) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(anyhow!(
            "permission denied installing {}: rerun with sudo or move isd to a writable path (e.g. ~/.local/bin/isd). Staged binary left at {}.",
            target.display(),
            staging.display(),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            // Staging path was on a different filesystem from the
            // target. This can happen if /tmp is tmpfs but the binary
            // lives on a real disk. Re-stage inside the target's parent
            // dir and try again.
            let parent = target
                .parent()
                .ok_or_else(|| anyhow!("install target {} has no parent dir", target.display()))?;
            let new_staging = staging_path(target);
            // `new_staging` is guaranteed to share a filesystem with
            // `target` because it lives in the same directory. Copy
            // bytes across, then rename.
            std::fs::copy(staging, &new_staging).with_context(|| {
                format!(
                    "cross-fs fallback: copying {} -> {} (parent {})",
                    staging.display(),
                    new_staging.display(),
                    parent.display(),
                )
            })?;
            // Best-effort cleanup of the original staging path; the
            // rename below is the real installer.
            let _ = std::fs::remove_file(staging);
            std::fs::rename(&new_staging, target).map_err(|e| {
                anyhow!(
                    "cross-fs rename {} -> {} failed: {e}",
                    new_staging.display(),
                    target.display(),
                )
            })
        }
        Err(e) => Err(anyhow!(
            "atomic rename {} -> {} failed: {e}. Staged binary left at {}; remove it manually after diagnosing.",
            staging.display(),
            target.display(),
            staging.display(),
        )),
    }
}

/// Print the operator-facing plan. Mirrors the wording the spec
/// asks for. We render via `cliclack::note` so the connector glyphs
/// line up with the confirm prompt below.
fn print_plan(current: &str, target: &str, asset: &str, current_exe: &Path) {
    let _ = cliclack::intro(format!("isd update  v{}", env!("CARGO_PKG_VERSION")));
    let body = format!(
        "  Current   v{current}\n  Target    {target}\n  Asset     {asset}\n  Source    github.com/{RELEASES_REPO}\n\n  This will:\n    : Download the new binary\n    : Verify sha256 against the release manifest\n    : Atomic-rename onto {}",
        current_exe.display(),
    );
    let _ = cliclack::note("Update plan", body);
}

/// y/N confirm. Default is "yes": the operator already typed the
/// command, a stray Enter shouldn't punish them.
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
        assert_eq!(normalise_tag("0.5.2"), "v0.5.2");
        assert_eq!(normalise_tag("v0.5.2"), "v0.5.2");
        assert_eq!(normalise_tag(" v0.5.2 "), "v0.5.2");
        assert_eq!(normalise_tag("V1.2.3"), "V1.2.3");
    }

    #[test]
    fn is_already_on_target_string_equal() {
        assert!(is_already_on_target("0.5.2", "v0.5.2"));
        assert!(is_already_on_target("v0.5.2", "v0.5.2"));
        assert!(is_already_on_target("0.5.2", "0.5.2"));
    }

    #[test]
    fn is_already_on_target_string_differ() {
        assert!(!is_already_on_target("0.5.1", "v0.5.2"));
        assert!(!is_already_on_target("0.5.2", "v0.6.0"));
    }

    #[test]
    fn is_already_on_target_alpha_never_matches_stable() {
        // The dev build's CARGO_PKG_VERSION is `0.1.0-alpha`. A real
        // stable tag at `v0.1.0` (improbable but legal) must still
        // trigger an update, because pre-release < stable per SemVer.
        assert!(!is_already_on_target("0.1.0-alpha", "v0.1.0"));
        // Pre-release-to-same-pre-release: equal, no-op.
        assert!(is_already_on_target("0.1.0-alpha", "v0.1.0-alpha"));
    }

    #[test]
    fn is_already_on_target_ignores_build_metadata() {
        // SemVer treats `+build` as informational only.
        assert!(is_already_on_target("0.5.2+abc", "v0.5.2+def"));
    }

    #[test]
    fn detect_target_triple_macos_arm64() {
        assert_eq!(
            detect_target_triple_for("macos", "aarch64").unwrap(),
            "aarch64-apple-darwin"
        );
    }

    #[test]
    fn detect_target_triple_macos_intel() {
        assert_eq!(
            detect_target_triple_for("macos", "x86_64").unwrap(),
            "x86_64-apple-darwin"
        );
    }

    #[test]
    fn detect_target_triple_linux_x86_64() {
        assert_eq!(
            detect_target_triple_for("linux", "x86_64").unwrap(),
            "x86_64-unknown-linux-musl"
        );
    }

    #[test]
    fn detect_target_triple_linux_aarch64() {
        assert_eq!(
            detect_target_triple_for("linux", "aarch64").unwrap(),
            "aarch64-unknown-linux-musl"
        );
    }

    #[test]
    fn detect_target_triple_rejects_windows() {
        let err = detect_target_triple_for("windows", "x86_64").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("windows"), "msg: {msg}");
        assert!(msg.contains("Build from source"), "msg: {msg}");
    }

    #[test]
    fn detect_target_triple_rejects_freebsd() {
        let err = detect_target_triple_for("freebsd", "x86_64").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("freebsd"), "msg: {msg}");
    }

    #[test]
    fn detect_target_triple_rejects_riscv() {
        let err = detect_target_triple_for("linux", "riscv64").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("riscv64"), "msg: {msg}");
    }

    #[test]
    fn asset_name_format() {
        assert_eq!(
            asset_name("aarch64-apple-darwin"),
            "isd-aarch64-apple-darwin"
        );
        assert_eq!(
            asset_name("x86_64-unknown-linux-musl"),
            "isd-x86_64-unknown-linux-musl"
        );
    }

    #[test]
    fn build_binary_url_format() {
        let url = build_binary_url("v0.5.2", "aarch64-apple-darwin");
        assert_eq!(
            url,
            "https://github.com/Weavers-Engineering/Isengard/releases/download/v0.5.2/isd-aarch64-apple-darwin"
        );
    }

    #[test]
    fn build_sha_url_format() {
        let url = build_sha_url("v0.5.2", "x86_64-unknown-linux-musl");
        assert_eq!(
            url,
            "https://github.com/Weavers-Engineering/Isengard/releases/download/v0.5.2/isd-x86_64-unknown-linux-musl.sha256"
        );
    }

    #[test]
    fn parse_sha256_manifest_two_column_format() {
        let body = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789  isd-aarch64-apple-darwin\n";
        let got = parse_sha256_manifest(body, "isd-aarch64-apple-darwin").unwrap();
        assert_eq!(
            got,
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
    }

    #[test]
    fn parse_sha256_manifest_bare_digest() {
        let body = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789\n";
        let got = parse_sha256_manifest(body, "isd-aarch64-apple-darwin").unwrap();
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

    #[test]
    fn staging_path_appends_new_suffix() {
        let p = Path::new("/usr/local/bin/isd");
        let s = staging_path(p);
        assert_eq!(s, Path::new("/usr/local/bin/isd.new"));
    }

    #[test]
    fn staging_path_handles_extensionless_filename() {
        let p = Path::new("/tmp/foo");
        let s = staging_path(p);
        assert_eq!(s, Path::new("/tmp/foo.new"));
    }

    #[tokio::test]
    async fn download_and_verify_rejects_short_digest() {
        let dest = std::env::temp_dir().join("isd-self-update-short-digest");
        let _ = std::fs::remove_file(&dest);
        let res = download_and_verify("http://127.0.0.1:1/never-reached", &dest, "abc").await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("64 hex"));
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn download_and_verify_rejects_non_hex_digest() {
        let dest = std::env::temp_dir().join("isd-self-update-non-hex-digest");
        let _ = std::fs::remove_file(&dest);
        let bad = "g".repeat(64);
        let res = download_and_verify("http://127.0.0.1:1/never-reached", &dest, &bad).await;
        assert!(res.is_err());
        assert!(!dest.exists());
    }

    #[cfg(unix)]
    #[test]
    fn set_executable_sets_0755() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("victim");
        std::fs::write(&p, b"#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&p, perms).unwrap();
        set_executable(&p).expect("chmod");
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755, "expected 0755, got {mode:o}");
    }

    #[test]
    fn install_atomic_replaces_target_when_same_filesystem() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staging = dir.path().join("stage");
        let target = dir.path().join("isd-target");
        std::fs::write(&staging, b"new-binary-bytes").unwrap();
        std::fs::write(&target, b"old-binary-bytes").unwrap();
        install_atomic(&staging, &target).expect("rename");
        assert!(!staging.exists(), "staging should be gone");
        let body = std::fs::read(&target).unwrap();
        assert_eq!(body, b"new-binary-bytes");
    }

    #[test]
    fn install_atomic_creates_target_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staging = dir.path().join("stage");
        let target = dir.path().join("isd-target");
        std::fs::write(&staging, b"new-binary-bytes").unwrap();
        // target intentionally does not exist; rename(2) creates it.
        install_atomic(&staging, &target).expect("rename");
        assert!(target.exists());
        assert!(!staging.exists());
    }
}
