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
//! Task 4.1 wires the source merge and rendering shape. Task 4.2 adds
//! the fzf / inquire frontends on top.

use std::collections::HashSet;

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
}
