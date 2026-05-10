//! Spinner + step rendering for the `isengard init` bootstrap phase.
//!
//! Two flavors of step:
//!
//! - **instant**: prints a static `✓ <label>  <detail>` line with no
//!   animation. Used for filesystem ops and cheap state mutations.
//! - **async**: renders an in-place braille spinner via [`indicatif`],
//!   then on completion clears the spinner line and prints a static
//!   line in its place. The spinner is animated only while the step
//!   is in flight; once the step ends, only the final line remains in
//!   scrollback.
//!
//! Both flavors leave the same final line in the transcript so an
//! operator scrolling up after install sees a uniform `✓ ...` list.

use std::time::{Duration, Instant};

use console::style;
use indicatif::{ProgressBar, ProgressStyle};

/// Frames for the braille spinner. Matches the `dots` profile from
/// `cli-spinners`; we hand-roll the list so we don't pull a separate
/// crate.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A handle for a step that may take a non-trivial amount of time. The
/// returned [`StepGuard`] owns the spinner; calling [`StepGuard::done`]
/// or [`StepGuard::fail`] replaces the spinner line with the final
/// status line. Dropping the guard without calling either logs a warn
/// and clears the spinner so the terminal is not left in a weird state.
pub struct StepGuard {
    pb: ProgressBar,
    started: Instant,
    label: String,
    finished: bool,
}

impl StepGuard {
    /// Replace the spinner with a green check + detail line. Includes
    /// the elapsed duration in dim suffix.
    pub fn done(mut self, detail: impl AsRef<str>) {
        let elapsed = self.started.elapsed();
        self.pb.finish_and_clear();
        println!(
            "  {} {:<32} {} {}",
            style("✓").green().bold(),
            self.label,
            style(detail.as_ref()).dim(),
            style(format_elapsed(elapsed)).dim()
        );
        self.finished = true;
    }

    /// Replace the spinner with a red cross + error detail line.
    pub fn fail(mut self, detail: impl AsRef<str>) {
        let elapsed = self.started.elapsed();
        self.pb.finish_and_clear();
        println!(
            "  {} {:<32} {} {}",
            style("✗").red().bold(),
            self.label,
            style(detail.as_ref()).red(),
            style(format_elapsed(elapsed)).dim()
        );
        self.finished = true;
    }

    /// Update the spinner's status message in-place. Useful for
    /// long-running steps that move through sub-phases.
    pub fn set_message(&self, msg: impl AsRef<str>) {
        self.pb.set_message(msg.as_ref().to_string());
    }
}

impl Drop for StepGuard {
    fn drop(&mut self) {
        if !self.finished {
            // Caller forgot to finalize; clear the spinner so we don't
            // leave a stale frame in the terminal.
            self.pb.finish_and_clear();
            tracing::warn!(
                step = %self.label,
                "init step dropped without explicit done/fail; cleared spinner"
            );
        }
    }
}

/// Start a spinner for a long-running step. The returned guard must be
/// finalized via `.done(...)` or `.fail(...)`; otherwise the step's
/// final line never makes it into scrollback.
pub fn start(label: impl Into<String>) -> StepGuard {
    let label = label.into();
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("  {spinner:.yellow} {msg}")
            .unwrap()
            .tick_strings(SPINNER_FRAMES),
    );
    pb.set_message(format!("{label}…"));
    pb.enable_steady_tick(Duration::from_millis(80));
    StepGuard {
        pb,
        started: Instant::now(),
        label,
        finished: false,
    }
}

/// Print a static "step done" line for an instant operation. Mirrors
/// [`StepGuard::done`] but without the elapsed suffix (the operation
/// took microseconds; the suffix is noise).
pub fn instant_done(label: impl AsRef<str>, detail: impl AsRef<str>) {
    println!(
        "  {} {:<32} {}",
        style("✓").green().bold(),
        label.as_ref(),
        style(detail.as_ref()).dim()
    );
}

fn format_elapsed(d: Duration) -> String {
    let s = d.as_secs_f32();
    if s < 1.0 {
        format!("({:.0}ms)", d.as_millis())
    } else if s < 60.0 {
        format!("({s:.1}s)")
    } else {
        let mins = (d.as_secs() / 60) as u32;
        let secs = (d.as_secs() % 60) as u32;
        format!("({mins}m{secs:02}s)")
    }
}
