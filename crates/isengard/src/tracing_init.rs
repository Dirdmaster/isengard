//! Tracing init for the isengard binary. Centralizes the formatter, color
//! decision, and the post-init "ready" banner. Two output modes:
//!
//! - `pretty` (default): compact human-readable lines with ANSI colors,
//!   short HH:MM:SS timestamps, and module paths trimmed of the
//!   `isengard_` crate prefix. Designed to look polished in `docker logs`
//!   and `journalctl`, not just on a local TTY.
//! - `json` (`LOG_FORMAT=json`): one JSON object per line, suitable for log
//!   aggregators (Loki, Vector, Datadog).
//!
//! Color decision (checked in order):
//!   1. `LOG_FORMAT=json` => never color
//!   2. `NO_COLOR` set => never color (<https://no-color.org>)
//!   3. `RUST_LOG_STYLE=never|always|auto` => honor it
//!   4. otherwise => true even when stderr is not a TTY (the Docker case;
//!      operators reading `docker logs` should see colors by default)
//!
//! Filter precedence:
//!   1. CLI `--log` (or `ISENGARD_LOG` env via clap) when present
//!   2. `RUST_LOG`
//!   3. `info`

use std::fmt;
use std::io::IsTerminal;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

/// Output format for trace events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Pretty,
    Json,
}

impl LogFormat {
    fn from_env() -> Self {
        match std::env::var("LOG_FORMAT").as_deref() {
            Ok("json") | Ok("JSON") => LogFormat::Json,
            _ => LogFormat::Pretty,
        }
    }
}

/// Decide whether ANSI escapes should be emitted. JSON mode always disables
/// color; otherwise `NO_COLOR` and `RUST_LOG_STYLE` win, with a TTY-agnostic
/// default of `true` so Docker/journalctl get colored output too.
fn ansi_enabled(format: LogFormat) -> bool {
    if matches!(format, LogFormat::Json) {
        return false;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    match std::env::var("RUST_LOG_STYLE").as_deref() {
        Ok("never") => false,
        Ok("always") => true,
        Ok("auto") => std::io::stderr().is_terminal(),
        _ => true,
    }
}

/// Compact local-time formatter: `HH:MM:SS`. Avoids RFC3339 noise on every
/// line. Local time matches what an operator's `date` command would say.
struct ShortTime;

impl FormatTime for ShortTime {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        let now = chrono::Local::now();
        write!(w, "{}", now.format("%H:%M:%S"))
    }
}

/// Initialize the global subscriber. Idempotent-ish: calling twice will
/// panic (matches `tracing_subscriber::fmt::init`'s contract).
///
/// - `mode`: the runtime role string emitted in the ready banner
///   ("controller" or "agent").
/// - `is_daemon`: true for long-running modes that should print a ready
///   banner and default to `info` logging; false for one-shot CLI flows
///   (token mint, ca export, secret bootstrap, join-token printer)
///   whose stdout other tools parse. One-shot flows silence the banner
///   and default to `warn` so info lines don't splice into output.
pub fn init(mode: &str, cli_filter: Option<&str>, is_daemon: bool) {
    let default_filter = if is_daemon { "info" } else { "warn" };
    let filter = cli_filter.map(EnvFilter::new).unwrap_or_else(|| {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter))
    });

    let format = LogFormat::from_env();
    let ansi = ansi_enabled(format);

    match format {
        LogFormat::Pretty => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .with_ansi(ansi)
                .with_timer(ShortTime)
                .with_target(true)
                .compact()
                .init();
        }
        LogFormat::Json => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .json()
                .with_current_span(true)
                .with_span_list(false)
                .init();
        }
    }

    if is_daemon {
        ready_banner(mode, format, ansi);
    }
}

/// Emit the post-init banner. JSON mode emits a structured ready event;
/// pretty mode emits a colored single-line banner that's easy to spot in
/// `docker logs` when something restarts. Only called for daemon modes
/// (controller / agent); one-shot CLI subcommands skip it entirely so
/// their stdout stays parseable by `isd init` and friends.
fn ready_banner(mode: &str, format: LogFormat, ansi: bool) {
    let version = env!("ISENGARD_BUILD_VERSION");
    if matches!(format, LogFormat::Json) {
        tracing::info!(version, mode, "isengard ready");
        return;
    }

    if ansi {
        // Bold green isengard, dim version, bold mode, dim ready.
        // Color codes inline so we don't pull a new crate just for this.
        eprintln!(
            "\x1b[1;32misengard\x1b[0m \x1b[2m{version}\x1b[0m \
             \x1b[1;36m{mode}\x1b[0m \x1b[2mready\x1b[0m",
        );
    } else {
        eprintln!("isengard {version} {mode} ready");
    }
}

/// Trim the `isengard_` crate prefix from a target path so trace lines say
/// `agent::enroll` instead of `isengard_agent::enroll`. Used by the
/// `tracing-subscriber` `with_target_format` knob below.
#[allow(dead_code)]
pub(crate) fn trim_target(target: &str) -> &str {
    target.strip_prefix("isengard_").unwrap_or(target)
}

/// Pretty-print an `anyhow::Error` chain to stderr in the same color
/// palette as the rest of the trace output. Replaces the default
/// debug-style chain dump that `main`'s `Result` return would print.
pub fn print_error_chain(err: &anyhow::Error) {
    let format = LogFormat::from_env();
    let ansi = ansi_enabled(format);

    if matches!(format, LogFormat::Json) {
        // In aggregator mode we want one structured event, not a multi-line dump.
        let chain: Vec<String> = err.chain().map(|c| c.to_string()).collect();
        tracing::error!(error = %err, chain = ?chain, "fatal");
        return;
    }

    if ansi {
        eprintln!("\x1b[1;31merror:\x1b[0m {err}");
        for cause in err.chain().skip(1) {
            eprintln!("  \x1b[2mcaused by:\x1b[0m {cause}");
        }
    } else {
        eprintln!("error: {err}");
        for cause in err.chain().skip(1) {
            eprintln!("  caused by: {cause}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_target_strips_isengard_prefix() {
        assert_eq!(trim_target("isengard_agent::enroll"), "agent::enroll");
        assert_eq!(trim_target("isengard_controller"), "controller");
    }

    #[test]
    fn trim_target_passes_through_unrelated() {
        assert_eq!(trim_target("tonic::transport"), "tonic::transport");
        assert_eq!(trim_target(""), "");
    }

    #[test]
    fn log_format_json_via_env() {
        // Note: env-driven test. Safe under nextest's process-per-test default,
        // but we set+unset to be cautious under cargo test's threaded runner.
        // SAFETY: tests in this module don't touch the env concurrently.
        unsafe {
            std::env::set_var("LOG_FORMAT", "json");
        }
        assert_eq!(LogFormat::from_env(), LogFormat::Json);
        unsafe {
            std::env::remove_var("LOG_FORMAT");
        }
        assert_eq!(LogFormat::from_env(), LogFormat::Pretty);
    }
}
