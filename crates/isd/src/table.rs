//! Table rendering for `isd ps`.
//!
//! Produces a column-aligned, no-color, ASCII table the way kubectl /
//! docker ps render: header in CAPS, one row per service, one blank line
//! between stacks. Matches the JSON output column-for-column so operators
//! who pipe through `jq` see the same shape.

use comfy_table::{ContentArrangement, Table, presets::NOTHING};
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
            row.stack.as_str(),
            row.service.as_str(),
            row.host.as_str(),
            row.backend.as_str(),
            row.state.as_str(),
            row.image.as_str(),
            row.last_seen.as_str(),
        ]);
    }
    t.to_string()
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
