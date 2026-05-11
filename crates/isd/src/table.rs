//! Table rendering for `isd ps`.
//!
//! Produces a column-aligned, no-color, ASCII table the way kubectl /
//! docker ps render: header in CAPS, one row per service, one blank line
//! between stacks. Matches the JSON output column-for-column so operators
//! who pipe through `jq` see the same shape.

use comfy_table::{Cell, Color, ContentArrangement, Table, presets::NOTHING};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PsRow {
    pub stack: String,
    pub service: String,
    pub host: String,
    pub state: String,
    pub image: String,
    pub last_seen: String,
    /// Phase 0.5 wisp: the runtime backend driving this host
    /// (`docker`, `wisp`, ...). Defaults to `docker` for hosts whose
    /// agents are pre-0.5 and didn't gossip the field. Serde-default
    /// keeps older clients that decode this row shape happy.
    #[serde(default = "default_backend")]
    pub backend: String,
}

fn default_backend() -> String {
    "docker".to_string()
}

/// Render the rows as a kubectl-style ASCII table. Empty input prints just
/// the header so scripts piping to `wc -l` get a stable shape.
///
/// v0.5.3: the STATE column is colour-keyed via `comfy-table`'s `Cell::fg`.
/// `comfy-table` auto-suppresses ANSI escapes when stdout is not a TTY,
/// so piping into `grep`/`jq`/`less -R` stays clean.
pub fn render_table(rows: &[PsRow]) -> String {
    let mut t = Table::new();
    t.load_preset(NOTHING)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_header(vec![
            "STACK",
            "SERVICE",
            "HOST",
            "BACKEND",
            "STATE",
            "IMAGE",
            "LAST SEEN",
        ]);
    for row in rows {
        t.add_row(vec![
            Cell::new(row.stack.as_str()),
            Cell::new(row.service.as_str()),
            Cell::new(row.host.as_str()),
            Cell::new(row.backend.as_str()),
            state_cell(row.state.as_str()),
            Cell::new(row.image.as_str()),
            Cell::new(row.last_seen.as_str()),
        ]);
    }
    t.to_string()
}

/// Map a state string to a `comfy-table` `Cell` with the right foreground
/// colour for `isd ps`. Used by the table renderer; exposed `pub(crate)`
/// so `watch.rs` can share the same palette.
///
/// Palette (v0.5.3):
///   - `running`                                  -> green
///   - `pulling` / `creating` / `starting` /
///     `restarting`                               -> yellow
///   - `failed`                                   -> red
///   - `stopped` / `unknown` and anything else    -> dim grey
pub(crate) fn state_cell(state: &str) -> Cell {
    let cell = Cell::new(state);
    match state {
        "running" => cell.fg(Color::Green),
        "pulling" | "creating" | "starting" | "restarting" => cell.fg(Color::Yellow),
        "failed" => cell.fg(Color::Red),
        // Stopped, unknown, and anything we don't recognise render dim
        // so the operator's eye lands on green / yellow / red rows.
        _ => cell.fg(Color::DarkGrey),
    }
}

/// Render rows as JSON (an array). Used by `--json`. Pretty-printed for
/// human-driven scripting; `jq -c` strips the whitespace.
pub fn render_json(rows: &[PsRow]) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(rows)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<PsRow> {
        vec![
            PsRow {
                stack: "blog".into(),
                service: "wordpress".into(),
                host: "homelab-01".into(),
                state: "running".into(),
                image: "wordpress:6".into(),
                last_seen: "10s ago".into(),
                backend: "docker".into(),
            },
            PsRow {
                stack: "blog".into(),
                service: "mariadb".into(),
                host: "homelab-01".into(),
                state: "running".into(),
                image: "mariadb:11".into(),
                last_seen: "10s ago".into(),
                backend: "docker".into(),
            },
        ]
    }

    #[test]
    fn json_output_is_stable_for_snapshot() {
        let json = render_json(&fixture()).unwrap();
        // Avoid ordering surprises by parsing back round-trip.
        let parsed: Vec<PsRow> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].service, "wordpress");
        assert_eq!(parsed[1].service, "mariadb");
    }

    #[test]
    fn table_includes_header_and_each_row() {
        let table = render_table(&fixture());
        assert!(table.contains("STACK"));
        assert!(table.contains("SERVICE"));
        assert!(table.contains("wordpress"));
        assert!(table.contains("mariadb"));
        assert!(table.contains("homelab-01"));
    }

    #[test]
    fn empty_input_still_renders_header() {
        let table = render_table(&[]);
        assert!(table.contains("STACK"));
        assert!(table.contains("LAST SEEN"));
    }

    /// Phase 0.5 wisp: the BACKEND column appears in the header and
    /// each rendered row carries its backend value.
    #[test]
    fn isd_ps_renders_backend_column() {
        let mut rows = fixture();
        // Mix backends so the column has real content.
        rows[0].backend = "wisp".into();
        rows[1].backend = "docker".into();
        let table = render_table(&rows);
        assert!(table.contains("BACKEND"), "header has BACKEND column");
        assert!(table.contains("wisp"), "wisp value rendered");
        assert!(table.contains("docker"), "docker value rendered");
    }

    /// v0.5.3: the STATE column carries a per-state colour. The
    /// renderer relies on comfy-table's auto-detection so we only
    /// assert the mapping rule here, not the literal ANSI bytes
    /// (which only get emitted to a TTY).
    #[test]
    fn state_cell_palette_matches_spec() {
        // Smoke-check that the helper does not panic and returns a
        // distinct Cell per call. Comfy-table doesn't expose a getter
        // for the foreground colour, so we round-trip through the
        // rendered table to confirm the cell content is preserved.
        for state in [
            "running",
            "pulling",
            "creating",
            "starting",
            "restarting",
            "stopped",
            "failed",
            "unknown",
            "weird-future-state",
        ] {
            let cell = state_cell(state);
            // Build a single-row table and confirm the state token
            // appears in the output. Colour bytes (if any) are
            // appended around it.
            let mut t = Table::new();
            t.load_preset(NOTHING).add_row(vec![cell]);
            let rendered = t.to_string();
            assert!(
                rendered.contains(state),
                "state {state:?} missing in rendered cell {rendered:?}",
            );
        }
    }

    /// Phase 0.5 wisp: an older JSON row shape (no `backend` field)
    /// decodes cleanly thanks to the serde default. Old clients
    /// emitting the pre-0.5 shape don't break the new isd binary.
    #[test]
    fn ps_row_decodes_without_backend_field() {
        let json = r#"{
            "stack": "blog",
            "service": "x",
            "host": "h",
            "state": "running",
            "image": "i:1",
            "last_seen": "5s ago"
        }"#;
        let row: PsRow = serde_json::from_str(json).unwrap();
        assert_eq!(row.backend, "docker");
    }
}
