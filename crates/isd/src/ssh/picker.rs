//! Interactive host picker for `isd ssh` (no args).
//!
//! Merges two sources into a single dial menu:
//!
//!  - `GET /api/v1/hosts`: Isengard-enrolled hosts the controller knows
//!    about. Reported as `HostClass::Isengard`, with the agent's last
//!    heartbeat surfaced as `last_seen`.
//!  - `~/.config/isd/trusted_hosts.toml`: non-Isengard servers the
//!    operator onboarded via `isd ssh trust`. Reported as
//!    `HostClass::Trusted`, no last-seen data.
//!
//! When `fzf` is on `$PATH` the picker shells out to it (full-screen,
//! fuzzy-search). Otherwise it falls back to `inquire::Select`, which
//! works in any terminal without external deps.

use std::collections::HashSet;

use anyhow::Result;

/// Source bucket for a picker row. The picker prints the bucket label
/// in square brackets so the operator can tell at a glance which hosts
/// will dial through the Isengard CA versus those bootstrapped via
/// `isd ssh trust`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostClass {
    /// Host enrolled with the Isengard controller (agent reports in).
    Isengard,
    /// Host bootstrapped via `isd ssh trust` (entry in trusted_hosts.toml).
    Trusted,
}

/// One row in the picker menu. `last_seen` is `None` for trusted hosts
/// (the local store does not record heartbeats) and for Isengard hosts
/// the controller has never heard from.
#[derive(Debug, Clone)]
pub struct PickerRow {
    /// Dial target as the operator typed it (or as the agent reported).
    pub hostname: String,
    /// Source bucket for visual disambiguation.
    pub class: HostClass,
    /// RFC3339 timestamp the agent last heartbeated, when known.
    pub last_seen: Option<String>,
}

/// Merge the two source lists into a deduped picker menu. Isengard
/// hosts win on collision: when the same hostname appears in both
/// sources, the trusted entry is dropped. This mirrors how `isd ssh
/// <host>` resolves: an Isengard-enrolled host is dialed through the
/// fleet's CA, never via the operator-managed bootstrap path.
pub fn merge_sources(isengard: Vec<PickerRow>, trusted: Vec<PickerRow>) -> Vec<PickerRow> {
    let mut out = isengard;
    let known: HashSet<String> = out.iter().map(|r| r.hostname.clone()).collect();
    for t in trusted {
        if !known.contains(&t.hostname) {
            out.push(t);
        }
    }
    out
}

/// Render a row for the picker. Tab-separated so `fzf --delimiter=\t`
/// can pick the hostname as the first field while still rendering the
/// `[class]` and last-seen columns for the operator's eyes. The
/// `inquire` fallback shows the same string verbatim.
pub fn render_row(r: &PickerRow) -> String {
    let class = match r.class {
        HostClass::Isengard => "isengard",
        HostClass::Trusted => "trusted",
    };
    let last = r.last_seen.as_deref().unwrap_or("-");
    format!("{}\t[{}]\t{}", r.hostname, class, last)
}

/// Extract the hostname from a picker-rendered line. Returns the
/// trimmed first tab field, or `None` when the line is empty (e.g. the
/// user dismissed fzf without picking). Anything past the first tab is
/// presentation-only: `[isengard]\t2026-05-...` etc.
pub fn parse_selected_line(s: &str) -> Option<String> {
    let token = s.split('\t').next()?.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// True when `fzf` is on `$PATH`. Used by the dispatcher to pick the
/// preferred picker frontend: fzf when available, `inquire::Select`
/// otherwise.
pub fn fzf_available() -> bool {
    which::which("fzf").is_ok()
}

/// Spawn `fzf`, pipe the rendered rows to its stdin, and parse the
/// selected line. Returns `Ok(None)` when the user dismissed the
/// picker (Esc / Ctrl-C / no match): the caller treats that as "no
/// host selected" rather than an error.
///
/// `--with-nth=1,2,3` and `--delimiter=\t` mean fzf displays all three
/// columns but searches against the full line, which keeps the
/// hostname-first UX while letting the operator fuzzy-match on
/// `[isengard]` or the last-seen timestamp.
pub async fn pick_with_fzf(rows: &[PickerRow]) -> Result<Option<String>> {
    use tokio::io::AsyncWriteExt;

    let input: String = rows
        .iter()
        .map(render_row)
        .collect::<Vec<_>>()
        .join("\n");
    let mut child = tokio::process::Command::new("fzf")
        .arg("--prompt=host: ")
        .arg("--height=40%")
        .arg("--reverse")
        .arg("--with-nth=1,2,3")
        .arg("--delimiter=\t")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("fzf stdin missing"))?;
        stdin.write_all(input.as_bytes()).await?;
        stdin.shutdown().await.ok();
    }
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        // fzf exits 1 on no-match and 130 on Ctrl-C. Both mean "the
        // operator did not pick a host"; surface as Ok(None).
        return Ok(None);
    }
    let line = String::from_utf8_lossy(&output.stdout).trim_end().to_string();
    Ok(parse_selected_line(&line))
}

/// Render the picker via `inquire::Select`. Used when fzf is not on
/// PATH (operator never installed it, or stripped PATH in a CI shim).
/// `prompt_skippable` returns `Ok(None)` on Esc instead of erroring,
/// matching the fzf-cancel semantics.
pub fn pick_with_inquire(rows: &[PickerRow]) -> Result<Option<String>> {
    use inquire::Select;
    let options: Vec<String> = rows.iter().map(render_row).collect();
    let pick = Select::new("host:", options).prompt_skippable()?;
    Ok(pick.and_then(|s| parse_selected_line(&s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_sources_dedups_by_hostname_isengard_wins() {
        let isengard = vec![PickerRow {
            hostname: "edge-1".into(),
            class: HostClass::Isengard,
            last_seen: Some("2026-05-21T10:00:00Z".into()),
        }];
        let trusted = vec![PickerRow {
            hostname: "edge-1".into(),
            class: HostClass::Trusted,
            last_seen: None,
        }];
        let merged = merge_sources(isengard, trusted);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].class, HostClass::Isengard);
        // The Isengard row's last_seen survives the merge.
        assert_eq!(merged[0].last_seen.as_deref(), Some("2026-05-21T10:00:00Z"));
    }

    #[test]
    fn merge_sources_appends_unique_trusted() {
        let isengard = vec![PickerRow {
            hostname: "edge-1".into(),
            class: HostClass::Isengard,
            last_seen: None,
        }];
        let trusted = vec![PickerRow {
            hostname: "media.local".into(),
            class: HostClass::Trusted,
            last_seen: None,
        }];
        let merged = merge_sources(isengard, trusted);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].hostname, "edge-1");
        assert_eq!(merged[1].hostname, "media.local");
        assert_eq!(merged[1].class, HostClass::Trusted);
    }

    #[test]
    fn merge_sources_preserves_isengard_order() {
        // The Isengard list defines the leading order; trusted rows
        // append after. The picker surface relies on this so the
        // "fleet hosts first" UX is stable across runs.
        let isengard = vec![
            PickerRow {
                hostname: "a".into(),
                class: HostClass::Isengard,
                last_seen: None,
            },
            PickerRow {
                hostname: "b".into(),
                class: HostClass::Isengard,
                last_seen: None,
            },
        ];
        let trusted = vec![PickerRow {
            hostname: "c".into(),
            class: HostClass::Trusted,
            last_seen: None,
        }];
        let merged = merge_sources(isengard, trusted);
        let names: Vec<_> = merged.iter().map(|r| r.hostname.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn render_row_formats_class_and_last_seen() {
        let row = PickerRow {
            hostname: "edge-1".into(),
            class: HostClass::Isengard,
            last_seen: Some("2026-05-21T10:00:00Z".into()),
        };
        let line = render_row(&row);
        assert!(line.starts_with("edge-1"));
        assert!(line.contains("[isengard]"));
        assert!(line.contains("2026-05-21T10:00:00Z"));
    }

    #[test]
    fn render_row_uses_dash_when_last_seen_absent() {
        let row = PickerRow {
            hostname: "media.local".into(),
            class: HostClass::Trusted,
            last_seen: None,
        };
        let line = render_row(&row);
        assert!(line.starts_with("media.local"));
        assert!(line.contains("[trusted]"));
        // The trailing tab field is the dash placeholder so the column
        // never collapses in fzf's --with-nth output.
        assert!(line.ends_with("\t-"));
    }

    #[test]
    fn render_row_tab_separates_three_fields() {
        let row = PickerRow {
            hostname: "edge-1".into(),
            class: HostClass::Isengard,
            last_seen: Some("ts".into()),
        };
        let line = render_row(&row);
        let parts: Vec<&str> = line.split('\t').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "edge-1");
        assert_eq!(parts[1], "[isengard]");
        assert_eq!(parts[2], "ts");
    }

    #[test]
    fn parse_selected_line_extracts_hostname() {
        let h = parse_selected_line("edge-1\t[isengard]\t2026-05-21T10:00:00Z");
        assert_eq!(h.as_deref(), Some("edge-1"));
    }

    #[test]
    fn parse_selected_line_handles_empty() {
        assert_eq!(parse_selected_line(""), None);
    }

    #[test]
    fn parse_selected_line_trims_whitespace() {
        // fzf sometimes appends a trailing newline or space; the
        // parser must strip it so the dial path does not receive
        // `"edge-1\n"` as a hostname.
        let h = parse_selected_line("  edge-1  \t[isengard]\t-");
        assert_eq!(h.as_deref(), Some("edge-1"));
    }

    #[test]
    fn parse_selected_line_returns_none_for_whitespace_only_first_field() {
        let h = parse_selected_line("   \t[isengard]\t-");
        assert_eq!(h, None);
    }
}
