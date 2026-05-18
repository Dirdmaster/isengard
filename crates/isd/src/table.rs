//! Table rendering for `isd ps`. Produces a column-aligned ASCII table
//! the way docker ps renders: header in CAPS, one row per container.

use comfy_table::{Cell, Color, ContentArrangement, Table, presets::NOTHING};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

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
    /// Container labels surfaced by the source (bollard direct path or
    /// controller DTO when available). Empty when the source omits them.
    /// Used by the Track G protection guard to detect
    /// `io.isengard.role=controller|agent`.
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

/// Render container rows. Header preserves docker-ps casing (`CONTAINER
/// ID` with the space). Empty input still emits the header so scripts
/// piping into `wc -l` see a stable shape.
///
/// Track G Phase 2: when `group_by_host` is true and the row set spans
/// more than one distinct host, output is grouped per host with a
/// `HOST: <name>` section header. Hosts render in alphabetical order
/// (BTreeMap iteration). When `group_by_host` is false, or when the row
/// set is single-host, the flat docker-style table is emitted. Index
/// assignment (if any caller adds an index column upstream) is done
/// BEFORE grouping so the index stays globally monotonic across
/// sections.
pub fn render_container_table(rows: &[ContainerPsRow], group_by_host: bool) -> String {
    let distinct_hosts: HashSet<&str> = rows.iter().map(|r| r.host.as_str()).collect();
    if !group_by_host || distinct_hosts.len() <= 1 {
        return render_flat(rows);
    }

    // Group by host, sorted alphabetically via BTreeMap iteration.
    let mut by_host: BTreeMap<&str, Vec<&ContainerPsRow>> = BTreeMap::new();
    for row in rows {
        by_host.entry(row.host.as_str()).or_default().push(row);
    }

    let mut out = String::new();
    for (i, (host, host_rows)) in by_host.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("HOST: {host}\n"));
        out.push_str(&render_flat_slice(host_rows));
    }
    out
}

/// Flat docker-style table. The original `render_container_table` shape
/// before Track G Phase 2; kept here as the inner renderer.
fn render_flat(rows: &[ContainerPsRow]) -> String {
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

/// Same as `render_flat` but accepts a slice of references so the
/// grouping path can render each host's subset without cloning rows.
fn render_flat_slice(rows: &[&ContainerPsRow]) -> String {
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

    fn make_row_with_host(name: &str, host: &str) -> ContainerPsRow {
        ContainerPsRow {
            container_id: format!("{name}-id"),
            image: "test:latest".into(),
            command: "sh".into(),
            status: "Up 1m".into(),
            host: host.into(),
            stack: "".into(),
            names: name.into(),
            labels: HashMap::new(),
        }
    }

    /// Phase 0.18: container-first table renders the docker-ps header
    /// row even with no rows, and inserts each row's content when
    /// populated.
    #[test]
    fn container_table_renders_header_and_rows() {
        let table = render_container_table(&[], false);
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
            labels: HashMap::new(),
        }];
        let table = render_container_table(&rows, false);
        assert!(table.contains("a1b2c3d4e5f6"));
        assert!(table.contains("nginx:alpine"));
        assert!(table.contains("homelab-01"));
        assert!(table.contains("hello-web.1"));
    }

    /// Track G Phase 2: when group_by_host is true and rows span more
    /// than one distinct host, emit a `HOST: <name>` section per host
    /// in alphabetical order. lausanne sorts before lyon.
    #[test]
    fn render_groups_when_multiple_hosts() {
        let rows = vec![
            make_row_with_host("bazarr", "lausanne"),
            make_row_with_host("plex", "lausanne"),
            make_row_with_host("qbit", "lyon"),
        ];
        let out = render_container_table(&rows, true);
        assert!(out.contains("HOST: lausanne"));
        assert!(out.contains("HOST: lyon"));
        // lausanne first (alphabetical), so "bazarr" appears before "qbit".
        let bazarr_pos = out.find("bazarr").expect("bazarr present");
        let qbit_pos = out.find("qbit").expect("qbit present");
        assert!(
            bazarr_pos < qbit_pos,
            "lausanne section should render before lyon section"
        );
        // The HOST headers themselves should also sort alphabetically.
        let lausanne_hdr = out.find("HOST: lausanne").unwrap();
        let lyon_hdr = out.find("HOST: lyon").unwrap();
        assert!(lausanne_hdr < lyon_hdr);
    }

    /// Track G Phase 2: a single-host row set never grows a HOST: header
    /// even when group_by_host is true. Single-operator vaults stay flat.
    #[test]
    fn render_flat_when_one_host() {
        let rows = vec![make_row_with_host("bazarr", "lausanne")];
        let out = render_container_table(&rows, true);
        assert!(!out.contains("HOST:"));
        assert!(out.contains("bazarr"));
    }

    /// Track G Phase 2: `--no-group` (group_by_host=false) forces flat
    /// rendering even when the row set spans multiple hosts.
    #[test]
    fn render_no_group_flag_forces_flat() {
        let rows = vec![
            make_row_with_host("bazarr", "lausanne"),
            make_row_with_host("qbit", "lyon"),
        ];
        let out = render_container_table(&rows, false);
        assert!(!out.contains("HOST:"));
        assert!(out.contains("bazarr"));
        assert!(out.contains("qbit"));
    }
}
