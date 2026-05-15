//! Boxed table renderer for `isd` list commands, nushell `rounded`
//! style: rounded corners, light vertical separators between every
//! column, one header rule, no inter-row rules. ALL CAPS headers.
//! STATUS colored by state; `#` and ID columns dimmed; NAMES
//! emphasized. Adaptive column widths; non-TTY output decays to
//! tab-separated plain text.
//!
//! Design: `3 Resources/Superpowers/specs/2026-05-15-isd-table-renderer-design.md`.

/// Color bucket for a STATUS cell, derived from its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusColor {
    Green,
    Yellow,
    Red,
    Grey,
}

/// Classify a docker status string into a color bucket.
///
///   - `Up ...`                       -> green
///   - `Up ... (health: starting)`    -> yellow
///   - `Up ... (unhealthy)`           -> red
///   - `Restarting ...`               -> yellow
///   - `Exited (0) ...`               -> grey
///   - `Exited (<non-zero>) ...`      -> red
///   - `Dead ...`                     -> red
///   - `Created` / `Paused` / other   -> grey
pub fn classify_status(status: &str) -> StatusColor {
    let s = status.trim_start();
    if s.starts_with("Up ") || s == "Up" {
        if s.contains("(unhealthy)") {
            StatusColor::Red
        } else if s.contains("(health: starting)") {
            StatusColor::Yellow
        } else {
            StatusColor::Green
        }
    } else if s.starts_with("Restarting") {
        StatusColor::Yellow
    } else if s.starts_with("Dead") {
        StatusColor::Red
    } else if s.starts_with("Exited") {
        if s.starts_with("Exited (0)") {
            StatusColor::Grey
        } else {
            StatusColor::Red
        }
    } else {
        StatusColor::Grey
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_status_buckets() {
        assert_eq!(classify_status("Up 2 hours"), StatusColor::Green);
        assert_eq!(
            classify_status("Up 5 minutes (health: starting)"),
            StatusColor::Yellow
        );
        assert_eq!(
            classify_status("Up 5 minutes (unhealthy)"),
            StatusColor::Red
        );
        assert_eq!(classify_status("Restarting (1) 4 seconds ago"), StatusColor::Yellow);
        assert_eq!(classify_status("Exited (0) 12 minutes ago"), StatusColor::Grey);
        assert_eq!(classify_status("Exited (137) 1 hour ago"), StatusColor::Red);
        assert_eq!(classify_status("Dead"), StatusColor::Red);
        assert_eq!(classify_status("Created"), StatusColor::Grey);
        assert_eq!(classify_status("Paused"), StatusColor::Grey);
        assert_eq!(classify_status(""), StatusColor::Grey);
    }
}
