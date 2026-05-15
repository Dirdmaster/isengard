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

/// Truncate `text` to at most `max` display columns. Generic strings
/// middle-truncate (`ab...ij`). When `is_image` is true, the suffix is
/// preserved instead (`...isengard-agent:next`): for an image ref the
/// tag end is the meaningful part, the registry prefix is droppable.
fn truncate_cell(text: &str, max: usize, is_image: bool) -> String {
    let w = console::measure_text_width(text);
    if w <= max {
        return text.to_string();
    }
    if max <= 3 {
        return ".".repeat(max);
    }
    let chars: Vec<char> = text.chars().collect();
    if is_image {
        // Keep the last `max - 3` chars, prefix with "...".
        let keep = max - 3;
        let tail: String = chars[chars.len() - keep..].iter().collect();
        format!("...{tail}")
    } else {
        // Middle-truncate: head + "..." + tail.
        let budget = max - 3;
        let head_len = budget.div_ceil(2);
        let tail_len = budget - head_len;
        let head: String = chars[..head_len].iter().collect();
        let tail: String = chars[chars.len() - tail_len..].iter().collect();
        format!("{head}...{tail}")
    }
}

/// Decide the rendered width of each column given a terminal cap.
/// Returns one width per column. If the natural layout fits, returns
/// natural widths unchanged. Otherwise shrinks columns starting from
/// the lowest `shrink_priority`, each down to its `min_width`. If the
/// table still does not fit (every column at its min and the sum of
/// minimums + chrome still exceeds `term_width`), keeps shrinking
/// past min in the same priority order down to 1: a tiny terminal
/// gets a usable (heavily-truncated) table instead of a wrapped one.
fn fit_widths(table: &Table, term_width: usize) -> Vec<usize> {
    let mut widths = natural_widths(table);
    let n = widths.len();
    // Chrome per column: 2 spaces padding + 1 border char, plus the
    // leading border char.
    let chrome = n * 3 + 1;
    let total = |w: &[usize]| -> usize { w.iter().sum::<usize>() + chrome };

    if total(&widths) <= term_width {
        return widths;
    }

    // Column indices ordered by shrink_priority ascending (shrink the
    // lowest-priority columns first).
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| table.columns[i].shrink_priority);

    // Pass 1: shrink down to min_width.
    for &i in &order {
        if total(&widths) <= term_width {
            break;
        }
        let min = table.columns[i].min_width;
        if widths[i] <= min {
            continue;
        }
        let overflow = total(&widths) - term_width;
        let reclaimable = widths[i] - min;
        let take = overflow.min(reclaimable);
        widths[i] -= take;
    }

    // Pass 2: still too wide? Shrink past min_width down to 1. A user
    // with a 40-col terminal still gets a parseable table.
    if total(&widths) > term_width {
        for &i in &order {
            if total(&widths) <= term_width {
                break;
            }
            if widths[i] <= 1 {
                continue;
            }
            let overflow = total(&widths) - term_width;
            let reclaimable = widths[i] - 1;
            let take = overflow.min(reclaimable);
            widths[i] -= take;
        }
    }
    widths
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
    match style {
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
    }
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
/// width (shrinks low-priority columns then middle-truncates with
/// `...`); `color` toggles ANSI styling.
pub fn render(table: &Table, term_width: usize, color: bool) -> String {
    let widths = fit_widths(table, term_width);
    let mut out = String::new();

    out.push_str(&dim_border(&border_line(&widths, TL, T_DOWN, TR), color));
    out.push('\n');

    // Header row: headers always render with the Dim treatment of the
    // box itself, not the column's data style.
    out.push_str(&dim_border(&V.to_string(), color));
    for (i, col) in table.columns.iter().enumerate() {
        let truncated = truncate_cell(col.header, widths[i], false);
        let padded = pad(&truncated, widths[i], col.align);
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
            let is_image = col.header == "IMAGE";
            let truncated = truncate_cell(&row[i], widths[i], is_image);
            let padded = pad(&truncated, widths[i], col.align);
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

/// Render for a non-TTY stdout: tab-separated plain text, no borders,
/// no color, no `#` index column (the index is a TTY-only affordance).
/// ALL CAPS headers are kept on the first line. This is the pipeline
/// contract: stable, parseable, `cut -f` friendly.
pub fn render_plain(table: &Table) -> String {
    // Drop the leading `#` column if present.
    let skip_index = table
        .columns
        .first()
        .map(|c| c.header == "#")
        .unwrap_or(false);
    let start = usize::from(skip_index);

    let mut out = String::new();
    let headers: Vec<&str> = table.columns[start..].iter().map(|c| c.header).collect();
    out.push_str(&headers.join("\t"));
    for row in &table.rows {
        out.push('\n');
        out.push_str(&row[start..].join("\t"));
    }
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
    fn truncate_middle_preserves_both_ends() {
        assert_eq!(truncate_cell("abcdefghij", 7, false), "ab...ij");
        // Already short enough: unchanged.
        assert_eq!(truncate_cell("abc", 7, false), "abc");
        // Image refs preserve the meaningful suffix (tag end). `max` is
        // the total budget including the leading ellipsis: keep is
        // `max - 3` tail chars.
        assert_eq!(
            truncate_cell("ghcr.io/weavers/isengard-agent:next", 20, true),
            "...engard-agent:next"
        );
        assert_eq!(
            console::measure_text_width(&truncate_cell(
                "ghcr.io/weavers/isengard-agent:next",
                20,
                true
            )),
            20
        );
    }

    #[test]
    fn render_shrinks_to_terminal_width() {
        let table = Table {
            columns: ps_columns(),
            rows: ps_rows(),
        };
        // A width far below natural: every rendered line must fit.
        let out = render(&table, 50, false);
        for line in out.lines() {
            assert!(
                console::measure_text_width(line) <= 50,
                "line exceeds term width 50: {line:?} ({})",
                console::measure_text_width(line)
            );
        }
        // Low-priority columns (PORTS, IMAGE) truncate before STATUS / #.
        assert!(out.contains("..."), "something was truncated");
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
    fn render_plain_is_tab_separated_no_box_no_index() {
        let table = Table {
            columns: ps_columns(),
            rows: ps_rows(),
        };
        let out = render_plain(&table);
        // No box drawing, no ANSI.
        assert!(!out.contains('╭'));
        assert!(!out.contains('│'));
        assert!(!out.contains('\u{1b}'));
        // Tab-separated.
        assert!(out.lines().all(|l| l.contains('\t')));
        // The `#` column is dropped: header line starts with CONTAINER ID.
        let header = out.lines().next().unwrap();
        assert!(header.starts_with("CONTAINER ID"));
        assert!(!header.starts_with('#'));
        // ALL CAPS headers retained, one header line + one line per row.
        assert!(header.contains("NAMES"));
        assert_eq!(out.lines().count(), 1 + ps_rows().len());
        // Row content present, columns joined by tabs.
        assert!(out.contains("a1b2c3d4e5f6\tnginx:1.27\tUp 2 hours"));
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
