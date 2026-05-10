//! Probes + format helpers for `isengard init`. All visual chrome (intro,
//! outro, prompts, spinners, step glyphs, connector bar) is rendered by
//! `cliclack`; this module only contributes the data that goes inside its
//! widgets.
//!
//! Two responsibilities live here:
//!
//!   1. [`probe_host`] inspects /proc, uname, systemctl, and /sys/fs/cgroup
//!      so [`format_host_info`] can render a 5-line two-column block that
//!      sits under the intro as a `cliclack::note("Detected host", ...)`.
//!   2. [`format_ready_block`] builds the multi-line body for the outro
//!      note. Same shape as the mockup: labeled rows, then a `join`
//!      block with shell-continuation backslashes, then `Next:`.

use anyhow::{Context, Result, anyhow};
use sha2::Digest;

use super::Plan;

/// Information about the host we're installing on. Best-effort: any
/// missing piece falls back to a sensible string so the rendered note
/// never has a dangling column.
#[derive(Debug, Clone)]
pub struct HostInfo {
    pub host_label: String,
    pub kernel: String,
    pub init: String,
    pub cgroup: String,
    pub memory: String,
}

/// Detected-host probe (Linux). Best-effort: missing values render as
/// `unknown` rather than disappearing entirely so the column stays even.
#[cfg(target_os = "linux")]
pub fn probe_host() -> HostInfo {
    HostInfo {
        host_label: host_label().unwrap_or_else(|| "unknown".to_string()),
        kernel: kernel_release().unwrap_or_else(|| "unknown".to_string()),
        init: systemd_label().unwrap_or_else(|| "unknown".to_string()),
        cgroup: cgroup_label().unwrap_or_else(|| "unknown".to_string()),
        memory: memory_label().unwrap_or_else(|| "unknown".to_string()),
    }
}

/// Stub for non-Linux. `preflight` bails earlier so this is only here
/// to keep callers free of cfg gates.
#[cfg(not(target_os = "linux"))]
pub fn probe_host() -> HostInfo {
    HostInfo {
        host_label: "unknown".to_string(),
        kernel: "unknown".to_string(),
        init: "unknown".to_string(),
        cgroup: "unknown".to_string(),
        memory: "unknown".to_string(),
    }
}

/// Render the detected-host info as a 5-line two-column block. Used as
/// the body of `cliclack::note("Detected host", ...)`. The column gutter
/// matches the `done` step label padding (10 chars + 1 space) so the
/// two blocks visually align under the connector bar.
pub fn format_host_info(h: &HostInfo) -> String {
    format!(
        "{:<10} {}\n{:<10} {}\n{:<10} {}\n{:<10} {}\n{:<10} {}",
        "Host",
        h.host_label,
        "Kernel",
        h.kernel,
        "Init",
        h.init,
        "CGroup",
        h.cgroup,
        "Memory",
        h.memory,
    )
}

/// Build the multi-line outro body the installer prints at the bottom
/// of a successful run. Mirrors the mockup: labeled URLs / paths, a
/// dim journalctl hint, then a `join` block + `Next:` deploy hint.
pub fn format_ready_block(plan: &Plan, token: &str) -> String {
    let host_ip = if plan.host_ip.is_empty() {
        "<your-host-ip>".to_string()
    } else {
        plan.host_ip.clone()
    };

    let ca_pem_path = plan.etc_dir.join("ca.pem");

    // CA fingerprint is best-effort: if it fails (file missing, malformed)
    // we still emit the join block, just without the optional pin hint.
    let ca_fingerprint = std::fs::read_to_string(&ca_pem_path)
        .ok()
        .and_then(|pem| ca_sha256_fingerprint(&pem).ok());

    let mut out = String::new();
    out.push_str(&format!("{:<13}https://{}:9417\n", "Controller", host_ip));
    out.push_str(&format!("{:<13}http://localhost:9418\n", "Dashboard"));
    out.push_str(&format!("{:<13}{}\n", "State", plan.state_dir.display()));
    out.push_str(&format!(
        "{:<13}journalctl -fu iso-controller -u iso-agent\n",
        "Logs"
    ));
    out.push('\n');
    out.push_str("Add another host:\n");
    out.push_str("  isengard join \\\n");
    out.push_str(&format!("    --token {token} \\\n"));
    out.push_str(&format!("    --ca-pem-path {} \\\n", ca_pem_path.display()));
    out.push_str(&format!("    https://{host_ip}:9417"));
    if let Some(fp) = &ca_fingerprint {
        out.push('\n');
        let short: String = fp.chars().take(12).collect();
        out.push_str(&format!("  CA fingerprint: sha256:{short}..."));
    }
    out.push_str("\n\nNext:\n  isd deploy --file ./compose.toml --stack hello");

    out
}

/// Helper used by `execute` to format every step's stop line so the
/// label column lines up. 26-col left-pad chosen to fit `Installed systemd units`
/// + two spaces + the detail.
pub fn done(label: &str, detail: &str) -> String {
    format!("{label:<26}{detail}")
}

// ---------- validators ----------

/// Validate a comma-separated list of hostnames / wildcards. Returns
/// `Ok(())` even on an empty input so operators running an internal-only
/// deploy can skip the field entirely. The check is intentionally lax:
/// anything matching `^[A-Za-z0-9._*-]+$` per item is accepted.
///
/// Signature is `&String` (not `&str`) because cliclack's `validate` is
/// generic over the prompt's value type and `Input` defaults to `String`.
#[allow(clippy::ptr_arg)]
pub fn validate_hostnames(input: &String) -> Result<(), &'static str> {
    let s = input.trim();
    if s.is_empty() {
        return Ok(());
    }
    for raw in s.split(',') {
        let h = raw.trim();
        if h.is_empty() {
            continue;
        }
        if !h
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '*'))
        {
            return Err("hostnames may only contain letters, digits, '.', '-', '_', '*'");
        }
    }
    Ok(())
}

/// Validate an ACME contact email. Permissive: requires an '@' and a '.'
/// in the domain part, nothing more. Empty is allowed (internal-only
/// deploys don't need a contact).
///
/// `&String` for the same reason as [`validate_hostnames`]: cliclack's
/// validate signature takes the inferred value type by reference.
#[allow(clippy::ptr_arg)]
pub fn validate_email(input: &String) -> Result<(), &'static str> {
    let s = input.trim();
    if s.is_empty() {
        return Ok(());
    }
    let (local, domain) = match s.split_once('@') {
        Some(pair) => pair,
        None => return Err("email must contain '@'"),
    };
    if local.is_empty() || domain.is_empty() {
        return Err("email must have a local part and a domain");
    }
    if !domain.contains('.') {
        return Err("domain part must contain a '.'");
    }
    Ok(())
}

// ---------- helpers ----------

/// Compute the lowercase-hex sha256 of the first PEM CERTIFICATE block.
/// Duplicated from `mod.rs` because the join flow there is the only other
/// caller; keeping it local here means `format_ready_block` doesn't need
/// to reach into `super`.
fn ca_sha256_fingerprint(pem: &str) -> Result<String> {
    let der = pem_to_der(pem)?;
    let mut h = sha2::Sha256::new();
    h.update(&der);
    Ok(hex::encode(h.finalize()))
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    let begin = pem
        .find("-----BEGIN CERTIFICATE-----")
        .ok_or_else(|| anyhow!("no BEGIN CERTIFICATE in CA pem"))?;
    let after_begin = begin + "-----BEGIN CERTIFICATE-----".len();
    let end = pem
        .find("-----END CERTIFICATE-----")
        .ok_or_else(|| anyhow!("no END CERTIFICATE in CA pem"))?;
    let body = &pem[after_begin..end];
    let cleaned: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .context("base64-decode CA cert body")
}

// ---------- /proc / uname / systemctl probes ----------

#[cfg(target_os = "linux")]
fn host_label() -> Option<String> {
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()?
        .trim()
        .to_string();
    let ip = super::detect_host_ip().ok();
    Some(match ip {
        Some(ip) => format!("{hostname} ({ip})"),
        None => hostname,
    })
}

#[cfg(target_os = "linux")]
fn kernel_release() -> Option<String> {
    let out = std::process::Command::new("uname")
        .arg("-r")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let release = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Some(format!("Linux {release}"))
}

#[cfg(target_os = "linux")]
fn systemd_label() -> Option<String> {
    let out = std::process::Command::new("systemctl")
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let first = s.lines().next()?.trim();
    // First line is e.g. "systemd 255 (255.4-1ubuntu8)"; collapse the
    // parenthetical so the row reads "systemd 255".
    let trimmed = match first.find(" (") {
        Some(i) => &first[..i],
        None => first,
    };
    Some(trimmed.to_string())
}

#[cfg(target_os = "linux")]
fn cgroup_label() -> Option<String> {
    let raw = std::fs::read_to_string("/sys/fs/cgroup/cgroup.controllers").ok()?;
    let controllers = raw.trim();
    if controllers.is_empty() {
        Some("v2".to_string())
    } else {
        Some(format!("v2 ({controllers})"))
    }
}

#[cfg(target_os = "linux")]
fn memory_label() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let n: u64 = rest.split_whitespace().next()?.parse().ok()?;
            let gib = n as f64 / (1024.0 * 1024.0);
            return Some(format!("{gib:.0} GiB"));
        }
    }
    None
}
