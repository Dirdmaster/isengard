//! `isd ssh trust <host>` + `isd ssh untrust <host>` implementation.
//!
//! Bootstrap of a non-Isengard server: piggyback on the operator's
//! existing SSH access to install `/etc/ssh/isengard_ca.pub` plus a
//! `sshd_config.d/40-isengard-ca.conf` drop-in that points
//! `TrustedUserCAKeys` at it. Reload sshd. Record the host locally so
//! the picker added in PR 4 can show it alongside Isengard hosts.
//!
//! Untrust is local-only: it removes the entry from
//! `~/.config/isd/trusted_hosts.toml` and leaves the remote sshd
//! config alone. Pulling the CA off a server is an operator-managed
//! action (it touches their other workflows, not isd).

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use ssh_key::{HashAlg, PublicKey};

use crate::session::Session;

use super::trusted_hosts::{TrustedHost, TrustedHostsFile};
use super::{CaPubkeyResponse, TrustArgs, UntrustArgs, default_user};

/// Render the bootstrap script that runs on the remote host (as root,
/// via `sudo sh`).
///
/// Uses a quoted heredoc (`<<'EOF'`) so the pubkey contents (which can
/// contain `$`, backticks, or other shell metacharacters in the
/// comment field) are NOT shell-expanded on the remote side. The CA
/// pubkey lands at `/etc/ssh/isengard_ca.pub`; the drop-in lands at
/// `/etc/ssh/sshd_config.d/40-isengard-ca.conf`. sshd is reloaded via
/// systemctl when available, otherwise a SIGHUP to the parent sshd.
pub fn bootstrap_script(pubkey: &str) -> String {
    format!(
        "set -e\n\
         install -d -m 0755 /etc/ssh\n\
         cat > /etc/ssh/isengard_ca.pub <<'EOF'\n\
         {pubkey}\n\
         EOF\n\
         chmod 0644 /etc/ssh/isengard_ca.pub\n\
         install -d -m 0755 /etc/ssh/sshd_config.d\n\
         cat > /etc/ssh/sshd_config.d/40-isengard-ca.conf <<'EOF'\n\
         # Managed by isd ssh trust.\n\
         TrustedUserCAKeys /etc/ssh/isengard_ca.pub\n\
         EOF\n\
         if command -v systemctl >/dev/null 2>&1; then\n\
         systemctl reload sshd 2>/dev/null || systemctl reload ssh\n\
         else\n\
         pkill -HUP -o sshd\n\
         fi\n"
    )
}

/// Implementation of `isd ssh trust <host>`.
///
/// Fetches the controller CA pubkey, fingerprints it, then ssh's into
/// `<host>` (using the operator's existing key) and pipes the
/// bootstrap script into `sudo sh`. On success, records the host in
/// `~/.config/isd/trusted_hosts.toml` unless `--no-record` was passed.
///
/// # Errors
///
/// Returns `Err` when the controller is unreachable, when the CA
/// pubkey cannot be parsed, when the remote ssh/sudo invocation fails,
/// or when the local trust store cannot be written.
pub async fn run_trust(args: TrustArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let controller_url = session.require_controller()?;
    let url = format!("{controller_url}/api/v1/ssh/ca");
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let body: CaPubkeyResponse = resp
        .error_for_status()
        .context("fetching SSH CA pubkey")?
        .json()
        .await
        .context("decoding SSH CA pubkey JSON")?;
    let pubkey = body.pubkey.trim().to_string();

    let fingerprint = PublicKey::from_openssh(&pubkey)
        .context("parsing controller CA pubkey")?
        .fingerprint(HashAlg::Sha256)
        .to_string();

    let bootstrap_user = args.user.clone().unwrap_or_else(default_user);
    if bootstrap_user.is_empty() {
        return Err(anyhow!(
            "could not determine bootstrap user (whoami and $USER both empty); pass --user explicitly"
        ));
    }
    let script = bootstrap_script(&pubkey);

    eprintln!(
        "trusting {bootstrap_user}@{}:{} (CA {fingerprint}) ...",
        args.host, args.port
    );
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.arg("-p")
        .arg(args.port.to_string())
        .arg(format!("{bootstrap_user}@{}", args.host))
        .arg("sudo")
        .arg("sh");
    cmd.stdin(std::process::Stdio::piped());
    let mut child = cmd.spawn().context("spawning ssh for bootstrap")?;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("ssh stdin handle missing"))?;
        stdin
            .write_all(script.as_bytes())
            .await
            .context("piping bootstrap script to remote sudo sh")?;
        stdin.shutdown().await.ok();
    }
    let status = child
        .wait()
        .await
        .context("waiting on ssh bootstrap child")?;
    if !status.success() {
        return Err(anyhow!(
            "remote bootstrap failed: ssh sudo sh exited with {status}"
        ));
    }

    if !args.no_record {
        let path = TrustedHostsFile::default_path()?;
        let mut store = TrustedHostsFile::load(&path)?;
        store.upsert(TrustedHost {
            hostname: args.host.clone(),
            trusted_at: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            ca_fingerprint: fingerprint,
            ssh_user: None,
            ssh_port: if args.port == 22 {
                None
            } else {
                Some(args.port)
            },
        });
        store.save(&path)?;
        eprintln!("recorded {} in {}", args.host, path.display());
    }
    Ok(())
}

/// Implementation of `isd ssh untrust <host>`.
///
/// Local-only. Loads `trusted_hosts.toml`, removes the entry, saves.
/// Errors with an actionable message if the host is not present so a
/// typo does not silently no-op.
///
/// # Errors
///
/// Returns `Err` when the trust store cannot be read or written, or
/// when `<host>` is not in the file.
pub async fn run_untrust(args: UntrustArgs) -> Result<()> {
    let path = TrustedHostsFile::default_path()?;
    let mut store = TrustedHostsFile::load(&path)?;
    let before = store.entries.len();
    store.remove(&args.host);
    if store.entries.len() == before {
        return Err(anyhow!(
            "{} is not in {} (nothing to remove)",
            args.host,
            path.display()
        ));
    }
    store.save(&path)?;
    eprintln!("removed {} from {}", args.host, path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_script_writes_ca_and_dropin() {
        let s = bootstrap_script("ssh-ed25519 AAAA... isengard-ca");
        assert!(
            s.contains("/etc/ssh/isengard_ca.pub"),
            "missing CA pubkey path: {s}"
        );
        assert!(
            s.contains("TrustedUserCAKeys /etc/ssh/isengard_ca.pub"),
            "missing TrustedUserCAKeys directive: {s}"
        );
        assert!(
            s.contains("ssh-ed25519 AAAA... isengard-ca"),
            "missing pubkey body: {s}"
        );
        assert!(
            s.contains("/etc/ssh/sshd_config.d/40-isengard-ca.conf"),
            "missing sshd drop-in path: {s}"
        );
    }

    #[test]
    fn bootstrap_script_reloads_sshd() {
        let s = bootstrap_script("ssh-ed25519 AAAA");
        // Accept either systemctl reload or a SIGHUP fallback. Both
        // are valid: the script picks whichever the host supports.
        assert!(
            s.contains("systemctl reload sshd") || s.contains("pkill -HUP -o sshd"),
            "missing sshd reload step: {s}"
        );
    }

    #[test]
    fn bootstrap_script_uses_quoted_heredoc() {
        // The pubkey may contain `$` (in the comment field) or other
        // shell metacharacters that we MUST NOT let the remote shell
        // expand. A quoted heredoc (`<<'EOF'`) suppresses expansion;
        // an unquoted one (`<<EOF`) does not.
        let s = bootstrap_script("AAAA$with`backticks");
        assert!(
            s.contains("<<'EOF'"),
            "heredoc must use quoted EOF to suppress shell expansion: {s}"
        );
        // Both heredocs (the CA pubkey one and the drop-in one) must
        // use the quoted form.
        let quoted_count = s.matches("<<'EOF'").count();
        assert_eq!(
            quoted_count, 2,
            "expected 2 quoted heredocs (CA pubkey + drop-in), found {quoted_count}: {s}"
        );
    }

    #[test]
    fn bootstrap_script_sets_strict_error_mode() {
        // Without `set -e` the script keeps going if any step fails
        // (chmod, install, the heredoc cat) and reports success at
        // the end. That hides bootstrap failures from the operator.
        let s = bootstrap_script("AAAA");
        assert!(
            s.starts_with("set -e"),
            "script must start with `set -e`: {s}"
        );
    }
}
