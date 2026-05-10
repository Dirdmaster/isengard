//! Wizard color palette and reusable styles.
//!
//! Centralized so swapping the palette later is a one-file diff. The colors
//! lean Catppuccin Mocha-ish without strictly being Catppuccin: cyan for
//! active selection, green for success, red for failure, yellow for
//! pending/progress, dim white for borders, dark gray for hints.

use ratatui::style::{Color, Modifier, Style};

/// Primary brand accent. Active step icon, focus ring, banner color.
pub const ACCENT: Color = Color::Cyan;
/// Success state: completed steps, valid input, healthy services.
pub const SUCCESS: Color = Color::Green;
/// Failure state: invalid input, failed steps, error messages.
pub const FAILURE: Color = Color::Red;
/// Pending / in-progress state: spinners, "running" indicators.
pub const PENDING: Color = Color::Yellow;
/// Primary readable text. Stays terminal default contrast.
pub const TEXT: Color = Color::White;
/// Hints, help text, secondary labels. Dimmed.
pub const HINT: Color = Color::DarkGray;
/// Border color: dim white blends well on most palettes.
pub const BORDER: Color = Color::Gray;

/// Style for the sidebar header and content section titles.
pub fn title() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// Style for primary readable body text.
pub fn body() -> Style {
    Style::default().fg(TEXT)
}

/// Style for hint / help / secondary text.
pub fn hint() -> Style {
    Style::default().fg(HINT)
}

/// Style for the active step in the sidebar.
pub fn active_step() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// Style for completed / pending steps in the sidebar.
pub fn completed_step() -> Style {
    Style::default().fg(SUCCESS)
}

pub fn pending_step() -> Style {
    Style::default().fg(HINT)
}

pub fn failed_step() -> Style {
    Style::default().fg(FAILURE).add_modifier(Modifier::BOLD)
}

/// Style for borders on unfocused widgets.
pub fn border_unfocused() -> Style {
    Style::default().fg(BORDER)
}
