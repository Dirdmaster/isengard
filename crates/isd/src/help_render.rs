//! Render the root `isd --help` with subcommands grouped into named
//! sections. clap has no native multi-group subcommand help (issue
//! clap-rs/clap#1553 open; PR #5816 closed, #5819 open as of 4.6.1),
//! so we render the root ourselves from clap's own introspection.
//!
//! Subcommands keep clap's stock auto-help. Only the root is custom.

use clap::Command;
use console::Style;

/// Groups in render order. Every top-level subcommand must appear in
/// exactly one group (enforced by [`coverage`] in tests).
pub const GROUPS: &[(&str, &[&str])] = &[
    (
        "Containers",
        &["ps", "logs", "stop", "start", "restart", "rm", "kill"],
    ),
    ("Stacks", &["stack", "open"]),
    ("Fleet", &["hosts", "service", "route", "secret"]),
    ("Setup", &["context", "update"]),
];

/// Build the styled grouped help string from a clap `Command`. Honors
/// `console::colors_enabled()` for NO_COLOR / non-TTY decay.
pub fn render(cmd: &Command) -> String {
    let color = console::colors_enabled();
    let header = Style::new().bold();
    let cmd_name = Style::new().bold();

    let mut out = String::new();
    let about = cmd
        .get_about()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "Operate Docker fleets from your terminal".into());
    out.push_str(&about);
    out.push_str("\n\n");
    out.push_str(&style(&header, "Usage", color));
    out.push_str(": ");
    out.push_str(cmd.get_name());
    out.push_str(" [OPTIONS] <COMMAND>\n\n");

    // Build a name -> about map from the Command's subcommands.
    let about_by_name: std::collections::HashMap<String, String> = cmd
        .get_subcommands()
        .map(|sub| {
            (
                sub.get_name().to_string(),
                sub.get_about().map(|s| s.to_string()).unwrap_or_default(),
            )
        })
        .collect();

    for (group, names) in GROUPS {
        out.push_str(&style(&header, group, color));
        out.push('\n');
        for name in *names {
            let about = about_by_name.get(*name).cloned().unwrap_or_default();
            out.push_str("  ");
            out.push_str(&style(&cmd_name, name, color));
            // Pad command name column to 10 chars for alignment.
            for _ in name.len()..10 {
                out.push(' ');
            }
            out.push_str(&about);
            out.push('\n');
        }
        out.push('\n');
    }

    // Global options block. Pull from clap so we do not duplicate them.
    out.push_str(&style(&header, "Options", color));
    out.push('\n');
    for arg in cmd
        .get_arguments()
        .filter(|a| a.is_global_set() || matches!(a.get_id().as_str(), "help" | "version"))
    {
        let long = arg.get_long().unwrap_or_default();
        let about = arg.get_help().map(|s| s.to_string()).unwrap_or_default();
        out.push_str(&format!("  --{long:<14} {about}\n"));
    }
    out
}

fn style(s: &Style, text: &str, color: bool) -> String {
    if color {
        s.apply_to(text).to_string()
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Every subcommand on the root must appear in exactly one group.
    /// New top-level commands fail CI until they are slotted.
    #[test]
    fn every_subcommand_is_in_exactly_one_group() {
        let cmd = crate::Cli::command();
        let actual: std::collections::HashSet<String> = cmd
            .get_subcommands()
            .map(|s| s.get_name().to_string())
            .collect();
        let mut grouped: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (_, names) in GROUPS {
            for n in *names {
                assert!(
                    grouped.insert(n.to_string()),
                    "command {n:?} listed in two groups"
                );
            }
        }
        let in_actual_not_grouped: Vec<_> = actual
            .difference(&grouped)
            .filter(|s| s.as_str() != "help")
            .cloned()
            .collect();
        assert!(
            in_actual_not_grouped.is_empty(),
            "ungrouped subcommands: {in_actual_not_grouped:?}"
        );
        let in_grouped_not_actual: Vec<_> = grouped.difference(&actual).cloned().collect();
        assert!(
            in_grouped_not_actual.is_empty(),
            "stale group entries: {in_grouped_not_actual:?}"
        );
    }

    #[test]
    fn render_contains_every_group_and_command() {
        let cmd = crate::Cli::command();
        let out = render(&cmd);
        for (group, names) in GROUPS {
            assert!(out.contains(group), "missing group {group}");
            for n in *names {
                assert!(out.contains(n), "missing command {n}");
            }
        }
        assert!(out.contains("Usage"));
        assert!(out.contains("Options"));
    }
}
