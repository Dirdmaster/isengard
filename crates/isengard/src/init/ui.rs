//! Visual chrome for `isengard init`. Pure print helpers + an `inquire`
//! theme. Linear, ANSI-colored, scrollback-friendly: every line stays in
//! the terminal after the install completes so the operator can review
//! exactly what was configured.
//!
//! No state machine, no event channel: each helper is `&self`-free and
//! prints synchronously. The async install steps in [`super::Plan`] call
//! these helpers between awaits.
//!
//! The mockup (see Phase 0.11 spec) is the source of truth for layout.
//! When the mockup and this module disagree, the mockup wins.

use console::{Term, style};
use inquire::set_global_render_config;
use inquire::ui::{Attributes, Color, RenderConfig, StyleSheet, Styled};

/// Hand-rolled ASCII art for the `ISENGARD` banner. Six rows of block
/// characters; the leading newline keeps the banner from butting up
/// against the previous shell prompt.
///
/// Width: 56 columns of art plus a 2-space left indent. Falls back to
/// the small banner when the terminal is narrower than 60 columns.
const BANNER_LARGE: &str = r"
  ███████ ███████ ███████ ███    ██  ██████   █████  ██████  ██████
     ██   ██      ██      ████   ██ ██       ██   ██ ██   ██ ██   ██
     ██   ███████ █████   ██ ██  ██ ██   ███ ███████ ██████  ██   ██
     ██        ██ ██      ██  ██ ██ ██    ██ ██   ██ ██   ██ ██   ██
     ██   ███████ ███████ ██   ████  ██████  ██   ██ ██   ██ ██████
";

/// Single-line fallback banner for narrow terminals (under 60 cols).
const BANNER_SMALL: &str = "  ISENGARD";

/// Print the banner, tagline, and version line. Called once at the top
/// of the install.
pub fn banner() {
    let cols = term_width();
    if cols < 60 {
        println!();
        println!("{}", style(BANNER_SMALL).cyan().bold());
    } else {
        println!("{}", style(BANNER_LARGE).cyan().bold());
    }
    println!(
        "  {}",
        style("Daemonless container engine for the homelab.").white()
    );
    println!(
        "  {}",
        style(format!(
            "v{} . controller + agent . wisp runtime",
            env!("CARGO_PKG_VERSION")
        ))
        .dim()
    );
    println!();
}

/// One row in the [`detected`] box. Label is dimmed, value is white.
pub struct DetectedRow {
    pub label: &'static str,
    pub value: String,
}

/// Render the detected-host block. Two-column layout, 11-col label gutter,
/// indented 2 spaces. Caller passes already-collected rows so the helper
/// can stay platform-agnostic; the Linux probe lives in [`probe_host`].
pub fn detected(rows: &[DetectedRow]) {
    if rows.is_empty() {
        return;
    }
    println!("  {}", style("Detected").cyan().bold());
    for r in rows {
        println!(
            "    {:<10} {}",
            style(r.label).dim(),
            style(&r.value).white()
        );
    }
    println!();
}

/// Print a section divider:
///
/// ```text
///   --- Network identity ---------------------------------------
/// ```
///
/// Total visual width tracks the terminal width, capped at 70 columns
/// so the divider doesn't sprawl on wide windows. The horizontal line is
/// box-drawing `─` (U+2500), not an em or en dash.
pub fn section(title: &str) {
    let cols = term_width().clamp(40, 70);
    let visible_title = title.chars().count();
    // visible width consumed before the trailing fill: 2 indent + 4 box
    // chars + space + title + space.
    let consumed = 2 + 4 + 1 + visible_title + 1;
    let fill = cols.saturating_sub(consumed);
    println!();
    println!(
        "  {}{} {}",
        style("─── ").cyan().bold(),
        style(title).cyan().bold(),
        style("─".repeat(fill)).cyan().bold()
    );
    println!();
}

/// Print a styled prompt header (label + optional help text) before
/// invoking the underlying `inquire` widget. The widget renders its own
/// prompt line via the global theme; this helper paints the human label
/// + dim help on the lines above so the prompt reads:
///
/// ```text
///   > ACME contact email
///     Used for Let's Encrypt expiry notifications.
/// ```
///
/// The widget's own prompt (`> <input cursor>`) shows up immediately
/// below.
pub fn prompt_header(title: &str, help: Option<&str>) {
    println!("  {} {}", style("❯").cyan().bold(), style(title).bold());
    if let Some(h) = help {
        println!("    {}", style(h).dim());
    }
}

/// Apply the global inquire theme. Idempotent: safe to call multiple
/// times (the second call wins, but the values are identical).
///
/// Notes / inquire 0.7 gaps:
/// - The prompt prefix glyph is global. We use `>` (a styled chevron)
///   for the active prefix and the same glyph for the answered prefix
///   so the transcript stays consistent.
/// - inquire's `Select` does not support per-option help text in its
///   public API; we work around it by including the description in the
///   `Display` impl of the option struct (see [`super::SelectOption`]).
/// - inquire styles the chosen answer with the `answer` stylesheet but
///   prints it on the same line as the prompt: `> ACME directory  Let's...`.
///   That's the ›-after-Enter behavior the spec calls for.
pub fn install_inquire_theme() {
    let cyan_bold: StyleSheet = StyleSheet::new()
        .with_fg(Color::LightCyan)
        .with_attr(Attributes::BOLD);
    let dim: StyleSheet = StyleSheet::new().with_fg(Color::DarkGrey);
    let white: StyleSheet = StyleSheet::new().with_fg(Color::White);
    let green_bold: StyleSheet = StyleSheet::new()
        .with_fg(Color::LightGreen)
        .with_attr(Attributes::BOLD);
    let red: StyleSheet = StyleSheet::new().with_fg(Color::LightRed);

    let cfg = RenderConfig::default_colored()
        .with_prompt_prefix(Styled::new(">").with_style_sheet(cyan_bold))
        .with_answered_prompt_prefix(Styled::new(">").with_style_sheet(green_bold))
        .with_highlighted_option_prefix(Styled::new("●").with_style_sheet(green_bold))
        .with_help_message(dim)
        .with_default_value(dim)
        .with_answer(cyan_bold)
        .with_text_input(white)
        .with_canceled_prompt_indicator(Styled::new("(canceled)").with_style_sheet(red))
        .with_error_message(
            inquire::ui::ErrorMessageRenderConfig::default_colored()
                .with_prefix(Styled::new("✗").with_style_sheet(red))
                .with_message(red),
        );
    set_global_render_config(cfg);
}

/// Print a one-line note, indented 2 spaces, in dim white. Used by the
/// non-interactive path to announce "Using flag-driven config".
pub fn note(msg: &str) {
    println!("  {}", style(msg).dim());
}

/// Heading for the bootstrap step list. Same shape as [`section`] but
/// kept in a tiny helper for symmetry with the mockup.
pub fn bootstrap_header() {
    section("Bootstrap");
}

/// Print the final "Ready" summary box. Rounded corners, centered title
/// in green/bold, body wrapped to fit the box. The box stays in
/// scrollback so the operator can scroll up after install completes.
///
/// `lines` is rendered verbatim inside the box, one row per entry. Lines
/// longer than the inner width get hard-wrapped at the box edge.
pub fn summary_box(title: &str, lines: &[String]) {
    // 78-col cap stays under a typical 80-col terminal while leaving
    // enough inner width for a `--token <52-char-base32>` shell line
    // (which has no whitespace and would otherwise hard-wrap mid-token).
    let total = term_width().clamp(60, 78);
    let inner = total.saturating_sub(2); // leave one col for each border
    let inner_pad = inner.saturating_sub(2); // 2-col left/right padding inside borders

    // Top border with title.
    let title_visible = title.chars().count();
    // total inner width minus "─ " before title and trailing "─"
    let bar_after = inner.saturating_sub(4 + title_visible);
    let top = format!(
        "{}{}{} {} {}{}",
        style("╭").green().bold(),
        style("─").green().bold(),
        style("─").green().bold(),
        style(title).green().bold(),
        style("─".repeat(bar_after)).green().bold(),
        style("╮").green().bold(),
    );
    println!();
    println!("{top}");
    println!("{}", border_line(inner));

    for line in lines {
        // Caller pre-formats every line to fit; we only fall back to
        // [`wrap`] for over-long lines (wrap preserves the original
        // formatting only on a word boundary, so a line that's already
        // shaped right passes through unchanged).
        let chunks = if visible_width(line) <= inner_pad {
            vec![line.clone()]
        } else {
            wrap(line, inner_pad)
        };
        for chunk in chunks {
            // Pad chunk to inner_pad so the right border lines up.
            let pad = inner_pad.saturating_sub(visible_width(&chunk));
            println!(
                "{} {}{} {}",
                style("│").green().bold(),
                chunk,
                " ".repeat(pad),
                style("│").green().bold()
            );
        }
    }

    println!("{}", border_line(inner));
    println!(
        "{}{}{}",
        style("╰").green().bold(),
        style("─".repeat(inner)).green().bold(),
        style("╯").green().bold()
    );
    println!();
}

/// Empty padding row inside the summary box (top + bottom whitespace).
fn border_line(inner: usize) -> String {
    format!(
        "{}{}{}",
        style("│").green().bold(),
        " ".repeat(inner),
        style("│").green().bold()
    )
}

/// Best-effort wrap at `width` columns, breaking on whitespace. Falls
/// back to a hard cut for words longer than the inner box.
fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    if visible_width(text) <= width {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for word in text.split(' ') {
        if current.is_empty() {
            if visible_width(word) > width {
                // hard cut: split by chars
                let mut acc = String::new();
                for ch in word.chars() {
                    if visible_width(&acc) + 1 > width {
                        out.push(acc.clone());
                        acc.clear();
                    }
                    acc.push(ch);
                }
                if !acc.is_empty() {
                    current = acc;
                }
            } else {
                current.push_str(word);
            }
        } else if visible_width(&current) + 1 + visible_width(word) <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            out.push(current.clone());
            current.clear();
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Approximate visible width of `s`: counts chars and skips ANSI escape
/// sequences (`ESC [ ... m`). Good enough for our use, since the summary
/// box receives mostly plain ASCII strings.
fn visible_width(s: &str) -> usize {
    let mut count = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // skip until 'm'
            for nc in chars.by_ref() {
                if nc == 'm' {
                    break;
                }
            }
        } else {
            count += 1;
        }
    }
    count
}

/// Detected-host probe (Linux). Returns a small ordered list of rows;
/// best-effort: missing values are skipped silently. Non-Linux callers
/// should not invoke this.
#[cfg(target_os = "linux")]
pub fn probe_host() -> Vec<DetectedRow> {
    let mut rows = Vec::new();

    if let Some(host) = host_label() {
        rows.push(DetectedRow {
            label: "Host",
            value: host,
        });
    }
    if let Some(k) = kernel_release() {
        rows.push(DetectedRow {
            label: "Kernel",
            value: k,
        });
    }
    if let Some(i) = systemd_label() {
        rows.push(DetectedRow {
            label: "Init",
            value: i,
        });
    }
    if let Some(c) = cgroup_label() {
        rows.push(DetectedRow {
            label: "CGroup",
            value: c,
        });
    }
    if let Some(m) = memory_label() {
        rows.push(DetectedRow {
            label: "Memory",
            value: m,
        });
    }

    rows
}

/// Stub for non-Linux platforms. The interactive path bails earlier in
/// `preflight` so this is unreachable in practice; defining it keeps the
/// caller free of `cfg` gates.
#[cfg(not(target_os = "linux"))]
pub fn probe_host() -> Vec<DetectedRow> {
    Vec::new()
}

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
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(target_os = "linux")]
fn systemd_label() -> Option<String> {
    // `systemctl --version` first line: "systemd 255 (255.4-1ubuntu8)".
    let out = std::process::Command::new("systemctl")
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let first = s.lines().next()?.trim().to_string();
    Some(first)
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
            // /proc/meminfo reports MemTotal in kB.
            let gib = n as f64 / (1024.0 * 1024.0);
            return Some(format!("{gib:.1} GiB"));
        }
    }
    None
}

/// Terminal width in columns, fallback 80 when not on a TTY.
fn term_width() -> usize {
    // `console::Term::size()` returns `(rows, cols)`. The fallback is
    // `(24, 80)` when stderr is not a terminal, which is the case for
    // piped sessions (e.g. `orb run` capturing stdout). We always want
    // the column count.
    let (_, cols) = Term::stderr().size();
    if cols == 0 { 80 } else { cols as usize }
}
