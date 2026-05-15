//! Table rendering for `isd ps`. Produces a column-aligned ASCII table
//! the way docker ps renders: header in CAPS, one row per container.

use comfy_table::{Cell, Color, ContentArrangement, Table, presets::NOTHING};
use serde::{Deserialize, Serialize};

/// Phase 0.18: container-first row used by the rewritten `isd ps`.
/// Columns: CONTAINER ID, IMAGE, COMMAND, STATUS, HOST, STACK, NAMES.
/// STATUS includes the host-offline qualifier when applicable; the
/// rendering itself stays here so the qualifier formatting is in one
/// place.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerPsRow {
    pub container_id: String,
    pub image: String,
    pub command: String,
    pub status: String,
    pub host: String,
    pub stack: String,
    pub names: String,
}

/// Render container rows. Header preserves docker-ps casing (`CONTAINER
/// ID` with the space). Empty input still emits the header so scripts
/// piping into `wc -l` see a stable shape.
pub fn render_container_table(rows: &[ContainerPsRow]) -> String {
    let mut t = Table::new();
    t.load_preset(NOTHING)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_header(vec![
            "CONTAINER ID",
            "IMAGE",
            "COMMAND",
            "STATUS",
            "HOST",
            "STACK",
            "NAMES",
        ]);
    for row in rows {
        t.add_row(vec![
            Cell::new(row.container_id.as_str()),
            Cell::new(row.image.as_str()),
            Cell::new(row.command.as_str()),
            container_status_cell(row.status.as_str()),
            Cell::new(row.host.as_str()),
            Cell::new(row.stack.as_str()),
            Cell::new(row.names.as_str()),
        ]);
    }
    t.to_string()
}

/// Phase 0.18 STATUS-column palette. Tokens at the start of the
/// status_message map to colour:
///   - "Up "                              -> green
///   - "Exited"                           -> red
///   - "Paused" / "Restarting" / "Created" -> yellow
///   - "Dead" / "Removing"                -> red
///   - anything else                      -> dim
///
/// The host-offline qualifier suffix doesn't change the colour: the
/// underlying status is still the most useful signal.
fn container_status_cell(status: &str) -> Cell {
    let cell = Cell::new(status);
    if status.starts_with("Up ") {
        cell.fg(Color::Green)
    } else if status.starts_with("Exited") {
        cell.fg(Color::Red)
    } else if status.starts_with("Paused")
        || status.starts_with("Restarting")
        || status.starts_with("Created")
    {
        cell.fg(Color::Yellow)
    } else if status.starts_with("Dead") || status.starts_with("Removing") {
        cell.fg(Color::Red)
    } else {
        cell.fg(Color::DarkGrey)
    }
}

/// JSON output: raw API row array (see `ps::ContainerApiDto`). The
/// caller hands the deserialized row vec straight through so consumers
/// see the exact shape the controller emits.
pub fn render_container_json<T: Serialize>(rows: &[T]) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(rows)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase 0.18: container-first table renders the docker-ps header
    /// row even with no rows, and inserts each row's content when
    /// populated.
    #[test]
    fn container_table_renders_header_and_rows() {
        let table = render_container_table(&[]);
        assert!(table.contains("CONTAINER ID"));
        assert!(table.contains("STATUS"));

        let rows = vec![ContainerPsRow {
            container_id: "a1b2c3d4e5f6".into(),
            image: "nginx:alpine".into(),
            command: "nginx -g 'daemon off;'".into(),
            status: "Up 5m".into(),
            host: "homelab-01".into(),
            stack: "hello".into(),
            names: "hello-web.1".into(),
        }];
        let table = render_container_table(&rows);
        assert!(table.contains("a1b2c3d4e5f6"));
        assert!(table.contains("nginx:alpine"));
        assert!(table.contains("homelab-01"));
        assert!(table.contains("hello-web.1"));
    }
}
