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

/// Horizontal alignment of a column's cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// Visual treatment of a column's cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellStyle {
    /// Dim grey: `#` and `CONTAINER ID`.
    Dim,
    /// Default foreground: `IMAGE`, `PORTS`.
    Plain,
    /// Bright + bold: `NAMES`.
    Emphasis,
    /// Colored by `classify_status` of the cell text: `STATUS`.
    State,
}

/// A column definition. `shrink_priority`: higher shrinks last when the
/// table is wider than the terminal. `min_width`: the content width
/// this column will not shrink below before truncation begins.
#[derive(Debug, Clone)]
pub struct Column {
    pub header: &'static str,
    pub align: Align,
    pub style: CellStyle,
    pub shrink_priority: u8,
    pub min_width: usize,
}

impl Column {
    pub fn new(
        header: &'static str,
        align: Align,
        style: CellStyle,
        shrink_priority: u8,
        min_width: usize,
    ) -> Self {
        Self {
            header,
            align,
            style,
            shrink_priority,
            min_width,
        }
    }
}

/// A table: column definitions plus rows of pre-stringified cells.
/// `rows[i].len()` must equal `columns.len()`.
#[derive(Debug, Clone)]
pub struct Table {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<String>>,
}

/// Box-drawing glyphs for the nushell `rounded` style.
const TL: char = '╭';
const TR: char = '╮';
const BL: char = '╰';
const BR: char = '╯';
const H: char = '─';
const V: char = '│';
const T_DOWN: char = '┬';
const T_UP: char = '┴';
const T_RIGHT: char = '├';
const T_LEFT: char = '┤';
const CROSS: char = '┼';

/// Natural display width of each column: the max of the header and
/// every cell, ignoring terminal width.
fn natural_widths(table: &Table) -> Vec<usize> {
    table
        .columns
        .iter()
        .enumerate()
        .map(|(i, col)| {
            let header_w = console::measure_text_width(col.header);
            let cell_w = table
                .rows
                .iter()
                .map(|r| console::measure_text_width(&r[i]))
                .max()
                .unwrap_or(0);
            header_w.max(cell_w)
        })
        .collect()
}

/// Build one horizontal border line with the given corner/join glyphs.
fn border_line(widths: &[usize], left: char, mid: char, right: char) -> String {
    let mut s = String::new();
    s.push(left);
    for (i, w) in widths.iter().enumerate() {
        // +2 for the one-space cell padding on each side.
        for _ in 0..(w + 2) {
            s.push(H);
        }
        s.push(if i + 1 == widths.len() { right } else { mid });
    }
    s
}

/// Pad `text` to `width` display columns with the given alignment.
/// Assumes `measure_text_width(text) <= width` (the caller truncates
/// first).
fn pad(text: &str, width: usize, align: Align) -> String {
    let used = console::measure_text_width(text);
    let gap = width.saturating_sub(used);
    match align {
        Align::Left => format!("{text}{}", " ".repeat(gap)),
        Align::Right => format!("{}{text}", " ".repeat(gap)),
    }
}

/// Apply the column's `CellStyle` to already-padded cell text. Borders
/// are styled by the caller via [`dim_border`]. When `color` is false
/// the text is returned unchanged. The caller's `color` flag is
/// authoritative: `force_styling(true)` bypasses the global TTY check
/// so a piped-and-explicitly-colored render still emits ANSI.
fn style_cell(padded: &str, raw: &str, style: CellStyle, color: bool) -> String {
    if !color {
        return padded.to_string();
    }
    use console::Style;
    let mk = || Style::new().force_styling(true);
    let styled = match style {
        CellStyle::Dim => mk().dim().apply_to(padded).to_string(),
        CellStyle::Plain => padded.to_string(),
        CellStyle::Emphasis => mk().bold().apply_to(padded).to_string(),
        CellStyle::State => {
            let s = match classify_status(raw) {
                StatusColor::Green => mk().green(),
                StatusColor::Yellow => mk().yellow(),
                StatusColor::Red => mk().red(),
                StatusColor::Grey => mk().dim(),
            };
            s.apply_to(padded).to_string()
        }
    };
    styled
}

/// Dim a border line when `color` is on; pass through otherwise.
fn dim_border(line: &str, color: bool) -> String {
    if color {
        console::Style::new()
            .force_styling(true)
            .dim()
            .apply_to(line)
            .to_string()
    } else {
        line.to_string()
    }
}

/// Render the table as a boxed string. `term_width` caps the total
/// width (enforced in Task 5); `color` toggles ANSI styling.
pub fn render(table: &Table, _term_width: usize, color: bool) -> String {
    let widths = natural_widths(table);
    let mut out = String::new();

    out.push_str(&dim_border(&border_line(&widths, TL, T_DOWN, TR), color));
    out.push('\n');

    // Header row: headers always render with the Dim treatment of the
    // box itself, not the column's data style.
    out.push_str(&dim_border(&V.to_string(), color));
    for (i, col) in table.columns.iter().enumerate() {
        let padded = pad(col.header, widths[i], col.align);
        let cell = if color {
            console::Style::new()
                .force_styling(true)
                .dim()
                .apply_to(format!(" {padded} "))
                .to_string()
        } else {
            format!(" {padded} ")
        };
        out.push_str(&cell);
        out.push_str(&dim_border(&V.to_string(), color));
    }
    out.push('\n');

    out.push_str(&dim_border(&border_line(&widths, T_RIGHT, CROSS, T_LEFT), color));
    out.push('\n');

    for row in &table.rows {
        out.push_str(&dim_border(&V.to_string(), color));
        for (i, col) in table.columns.iter().enumerate() {
            let padded = pad(&row[i], widths[i], col.align);
            out.push(' ');
            out.push_str(&style_cell(&padded, &row[i], col.style, color));
            out.push(' ');
            out.push_str(&dim_border(&V.to_string(), color));
        }
        out.push('\n');
    }

    out.push_str(&dim_border(&border_line(&widths, BL, T_UP, BR), color));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ps_columns() -> Vec<Column> {
        vec![
            Column::new("#", Align::Right, CellStyle::Dim, 9, 1),
            Column::new("CONTAINER ID", Align::Left, CellStyle::Dim, 7, 12),
            Column::new("IMAGE", Align::Left, CellStyle::Plain, 2, 10),
            Column::new("STATUS", Align::Left, CellStyle::State, 8, 10),
            Column::new("PORTS", Align::Left, CellStyle::Plain, 1, 6),
            Column::new("NAMES", Align::Left, CellStyle::Emphasis, 6, 8),
        ]
    }

    fn ps_rows() -> Vec<Vec<String>> {
        vec![
            vec![
                "0".into(),
                "a1b2c3d4e5f6".into(),
                "nginx:1.27".into(),
                "Up 2 hours".into(),
                ":80->80".into(),
                "web-proxy".into(),
            ],
            vec![
                "1".into(),
                "7f8e9d0c1b2a".into(),
                "postgres:16".into(),
                "Up 5 days".into(),
                "5432".into(),
                "app-db".into(),
            ],
        ]
    }

    #[test]
    fn render_draws_rounded_box_with_caps_headers() {
        let table = Table {
            columns: ps_columns(),
            rows: ps_rows(),
        };
        let out = render(&table, 200, false);
        // Rounded corners and joins.
        assert!(out.contains('╭'), "top-left rounded corner");
        assert!(out.contains('╮'), "top-right rounded corner");
        assert!(out.contains('╰'), "bottom-left rounded corner");
        assert!(out.contains('╯'), "bottom-right rounded corner");
        assert!(out.contains('┼'), "header-rule cross join");
        // ALL CAPS headers (input is already caps; assert they survive).
        assert!(out.contains("CONTAINER ID"));
        assert!(out.contains("NAMES"));
        // Content present.
        assert!(out.contains("web-proxy"));
        assert!(out.contains("postgres:16"));
        // Exactly one header rule: count lines starting with ├.
        let rule_lines = out.lines().filter(|l| l.starts_with('├')).count();
        assert_eq!(rule_lines, 1, "exactly one header rule, no inter-row rules");
        // Every data + border line is the same display width.
        let widths: Vec<usize> = out
            .lines()
            .map(console::measure_text_width)
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "all rendered lines share one width: {widths:?}"
        );
    }

    #[test]
    fn render_with_color_emits_ansi_for_styled_columns() {
        let table = Table {
            columns: ps_columns(),
            rows: ps_rows(),
        };
        let colored = render(&table, 200, true);
        let plain = render(&table, 200, false);
        // Color path emits ANSI escape bytes; the no-color path does not.
        assert!(colored.contains('\u{1b}'), "color=true emits ANSI");
        assert!(!plain.contains('\u{1b}'), "color=false emits no ANSI");
        // Display width is unchanged by styling (ANSI is zero-width).
        let cw: Vec<usize> = colored.lines().map(console::measure_text_width).collect();
        let pw: Vec<usize> = plain.lines().map(console::measure_text_width).collect();
        assert_eq!(cw, pw, "ANSI styling does not change display width");
    }

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
