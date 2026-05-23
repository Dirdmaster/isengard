//! Shared inline-TUI terminal infrastructure for both pickers
//! (`crate::picker` and `crate::ssh::picker`). The two pickers ship
//! their own state machines and chrome but the terminal lifecycle is
//! identical: raw mode + cursor-hide on enter, restore on drop or
//! panic, same inline-viewport budget.
//!
//! Factored out so a fix to either restore path lands in one place.

use std::io::stdout;

use crossterm::{
    cursor, execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};

/// Compute the inline viewport height (rows) for `n_rows` source items.
///
/// Layout budget: six rows of chrome (top border, header, header rule,
/// divider, input row, bottom border) plus one row per item. Result is
/// clamped to `[8, 18]`. The lower bound keeps the picker parseable
/// with an empty fleet; the upper bound keeps it inline rather than
/// scrolling the surrounding shell.
pub fn picker_height(n_rows: usize) -> u16 {
    let raw = (n_rows as u16).saturating_add(6);
    raw.clamp(8, 18)
}

/// RAII guard: disable raw mode + restore cursor visibility on drop.
/// Stack-allocated so unwinding through the TUI still restores the
/// terminal. No alt-screen exit needed; inline rendering never enters
/// one.
pub struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let mut out = stdout();
        let _ = execute!(out, cursor::Show);
        let _ = disable_raw_mode();
    }
}

/// Enter raw mode and hide the hardware cursor. Pairs with
/// [`TermGuard`] which restores both on drop.
///
/// Hiding the cursor matters even when the picker is in NORMAL mode
/// (where ratatui's `set_cursor_position` is intentionally skipped):
/// crossterm's `show_cursor: false` only suppresses ratatui's tracked
/// cursor, leaving the OS terminal cursor at its last position. Tools
/// that capture the terminal (cheese, asciinema) render that stray
/// glyph. Explicit `cursor::Hide` shuts it off for the whole
/// inline-viewport lifetime.
///
/// # Errors
///
/// Returns the underlying crossterm error if the terminal refuses raw
/// mode (no controlling TTY, OS denial).
pub fn enter_raw_mode() -> std::io::Result<()> {
    enable_raw_mode()?;
    let mut out = stdout();
    let _ = execute!(out, cursor::Hide);
    Ok(())
}

/// Install a panic hook that restores the terminal before delegating
/// to the previous hook. Safe to call multiple times per process: each
/// installation chains the previous hook so multiple inline TUIs in
/// one run still restore correctly.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut out = stdout();
        let _ = execute!(out, cursor::Show);
        let _ = disable_raw_mode();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_height_clamps_to_eight_and_eighteen() {
        assert_eq!(picker_height(0), 8);
        assert_eq!(picker_height(1), 8);
        assert_eq!(picker_height(2), 8);
        assert_eq!(picker_height(3), 9);
        assert_eq!(picker_height(12), 18);
        assert_eq!(picker_height(100), 18);
    }
}
