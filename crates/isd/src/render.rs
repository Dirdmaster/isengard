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
    /// Up and healthy.
    Green,
    /// Mid-transition (`Restarting`, `health: starting`).
    Yellow,
    /// Failed (`unhealthy`, non-zero `Exited`, `Dead`).
    Red,
    /// Neutral (`Exited (0)`, `Created`, `Paused`).
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
    /// Left-align (text columns).
    Left,
    /// Right-align (numeric columns like `#`).
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
    /// Cyan foreground: `DIAL TARGET` in the host picker (the dial
    /// string is the action verb of that table).
    Cyan,
}

/// A column definition. `shrink_priority`: higher shrinks last when the
/// table is wider than the terminal. `min_width`: the content width
/// this column will not shrink below before truncation begins.
#[derive(Debug, Clone)]
pub struct Column {
    /// ALL-CAPS header label.
    pub header: &'static str,
    /// Alignment within the cell.
    pub align: Align,
    /// Visual treatment for cell text.
    pub style: CellStyle,
    /// Lower values shrink first when the table doesn't fit.
    pub shrink_priority: u8,
    /// Floor below which the column resists shrinking. Pass-2 shrink
    /// past this still happens when the terminal is genuinely tiny.
    pub min_width: usize,
}

impl Column {
    /// Build a column. Convenience over struct-literal-init to keep
    /// the column tables in `ps.rs` etc readable.
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
    /// Column definitions in render order.
    pub columns: Vec<Column>,
    /// Row data; each inner Vec has `columns.len()` cells.
    pub rows: Vec<Vec<String>>,
}

/// Top-left corner.
const TL: char = '╭';
/// Top-right corner.
const TR: char = '╮';
/// Bottom-left corner.
const BL: char = '╰';
/// Bottom-right corner.
const BR: char = '╯';
/// Horizontal line.
const H: char = '─';
/// Vertical line.
const V: char = '│';
/// Top join (column separator on the top border).
const T_DOWN: char = '┬';
/// Bottom join (column separator on the bottom border).
const T_UP: char = '┴';
/// Left tee (header rule's left edge).
const T_RIGHT: char = '├';
/// Right tee (header rule's right edge).
const T_LEFT: char = '┤';
/// Header rule join (column separator on the header rule).
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
/// middle-truncate (`ab...ij`). When `is_image` is true, progressively
/// strip leftmost path segments (registry, namespace) before falling
/// back to middle-truncate, so an image ref keeps its meaningful tail
/// (`bazarr:latest` from `lscr.io/linuxserver/bazarr:latest`).
fn truncate_cell(text: &str, max: usize, is_image: bool) -> String {
    let w = console::measure_text_width(text);
    if w <= max {
        return text.to_string();
    }
    if max <= 3 {
        return ".".repeat(max);
    }
    if is_image {
        // Strip leftmost path segments one by one until it fits.
        let mut s = text.to_string();
        while console::measure_text_width(&s) > max {
            match s.find('/') {
                Some(pos) => s = s[pos + 1..].to_string(),
                None => break,
            }
        }
        if console::measure_text_width(&s) <= max {
            return s;
        }
        // No more path segments to strip and still too long. Middle-
        // truncate the final segment so the tag end survives.
        let chars: Vec<char> = s.chars().collect();
        let budget = max - 3;
        let head_len = budget.div_ceil(2);
        let tail_len = budget - head_len;
        let head: String = chars[..head_len].iter().collect();
        let tail: String = chars[chars.len() - tail_len..].iter().collect();
        return format!("{head}...{tail}");
    }
    // Generic middle-truncate.
    let chars: Vec<char> = text.chars().collect();
    let budget = max - 3;
    let head_len = budget.div_ceil(2);
    let tail_len = budget - head_len;
    let head: String = chars[..head_len].iter().collect();
    let tail: String = chars[chars.len() - tail_len..].iter().collect();
    format!("{head}...{tail}")
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
        CellStyle::Cyan => mk().cyan().apply_to(padded).to_string(),
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

    out.push_str(&dim_border(
        &border_line(&widths, T_RIGHT, CROSS, T_LEFT),
        color,
    ));
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

/// Input row spec for [`render_to_lines`]: when supplied, the renderer
/// emits a full-width footer row inside the box (under a `┴`-joined
/// divider) instead of closing with the usual bottom border.
///
/// The picker uses this to fold its `> filter hosts...` prompt into the
/// table's chrome, so the operator reads the whole picker as one boxed
/// widget instead of "table + loose line below".
#[derive(Debug, Clone, Copy)]
pub struct InputRow<'a> {
    /// Operator-supplied query so far. Empty string + `placeholder`
    /// renders the placeholder; non-empty renders the query verbatim.
    pub query: &'a str,
    /// Muted italic placeholder shown when `query.is_empty()`.
    pub placeholder: &'a str,
    /// Prompt prefix to display before the query (e.g. `> `).
    pub prompt: &'a str,
}

/// Render the table as a vector of ratatui `Line`s suitable for a TUI
/// `Paragraph` or inline viewport. Shares the same layout pass as
/// [`render`] (column fit, truncation, padding, ALL-CAPS dim headers)
/// but emits native ratatui spans so a TUI widget can compose the
/// table without round-tripping through ANSI bytes.
///
/// `highlight_row` overrides every span on the i-th data row with a
/// cyan background + black foreground, matching the picker's selected-
/// row affordance (fzf-style). Index is into the data rows (the top
/// border, header, and header rule are NOT countable indices). The
/// highlight stops at the rightmost data `│`: the box's closing edge
/// keeps the dim border style so the selection band does not bleed
/// past the table.
///
/// `input_row`: when `Some`, the renderer emits a `┴`-joined divider
/// followed by a full-width input row and the box's bottom border.
/// When `None`, the table closes with the usual `╰─...─╯` bottom border.
///
/// Returns the rendered lines plus, when an input row was emitted,
/// the cursor position as `(row, col)` (0-indexed). `row` is the index
/// of the input row inside the returned `Vec<Line>`; `col` is the cell
/// column where the cursor should sit (one space cell-padding + prompt
/// width + query width, all offset by 1 for the leading `│`).
pub fn render_to_lines(
    table: &Table,
    term_width: usize,
    highlight_row: Option<usize>,
    input_row: Option<InputRow<'_>>,
) -> (Vec<ratatui::text::Line<'static>>, Option<(u16, u16)>) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};

    let widths = fit_widths(table, term_width);
    let mut lines: Vec<Line<'static>> = Vec::new();

    let border_style = Style::default().add_modifier(Modifier::DIM);
    let header_style = Style::default().add_modifier(Modifier::DIM);

    // Top border.
    lines.push(Line::from(Span::styled(
        border_line(&widths, TL, T_DOWN, TR),
        border_style,
    )));

    // Header row.
    let mut header_spans: Vec<Span<'static>> = Vec::new();
    header_spans.push(Span::styled(V.to_string(), border_style));
    for (i, col) in table.columns.iter().enumerate() {
        let truncated = truncate_cell(col.header, widths[i], false);
        let padded = pad(&truncated, widths[i], col.align);
        header_spans.push(Span::styled(format!(" {padded} "), header_style));
        header_spans.push(Span::styled(V.to_string(), border_style));
    }
    lines.push(Line::from(header_spans));

    // Header rule.
    lines.push(Line::from(Span::styled(
        border_line(&widths, T_RIGHT, CROSS, T_LEFT),
        border_style,
    )));

    // Data rows.
    let hl_style = Style::default().bg(Color::Cyan).fg(Color::Black);
    let n_cols = table.columns.len();
    for (row_idx, row) in table.rows.iter().enumerate() {
        let highlighted = highlight_row == Some(row_idx);
        let mut row_spans: Vec<Span<'static>> = Vec::new();
        // Leading border: when highlighted, swap the dim border for
        // the highlight background so the selection band reads as one
        // continuous strip on the left edge of the row.
        if highlighted {
            row_spans.push(Span::styled(V.to_string(), hl_style));
        } else {
            row_spans.push(Span::styled(V.to_string(), border_style));
        }
        for (i, col) in table.columns.iter().enumerate() {
            let is_image = col.header == "IMAGE";
            let truncated = truncate_cell(&row[i], widths[i], is_image);
            let padded = pad(&truncated, widths[i], col.align);
            let cell_text = format!(" {padded} ");
            let cell_style = if highlighted {
                hl_style
            } else {
                cell_style_to_ratatui(col.style, &row[i])
            };
            row_spans.push(Span::styled(cell_text, cell_style));
            // Inner separators between columns stay highlighted on the
            // selected row so the band reads as one strip. The LAST
            // `│` (right edge of the box) keeps the dim border style
            // even on the highlighted row: the highlight stops AT the
            // box edge, never past it.
            let is_last_sep = i + 1 == n_cols;
            let sep_style = if highlighted && !is_last_sep {
                hl_style
            } else {
                border_style
            };
            row_spans.push(Span::styled(V.to_string(), sep_style));
        }
        lines.push(Line::from(row_spans));
    }

    let cursor = match input_row {
        Some(spec) => {
            // Divider that closes the inner column separators with `┴`
            // and joins to the box's outer border via `├`/`┤`.
            lines.push(Line::from(Span::styled(
                border_line(&widths, T_RIGHT, T_UP, T_LEFT),
                border_style,
            )));

            // Inner content width: same as a normal row's content (sum
            // of column widths + 2 per column for cell padding + 1 per
            // inner separator). Reuse the divider's display width minus
            // the two outer tees to stay in lock-step with the table
            // chrome regardless of column-count math.
            let chrome_w =
                console::measure_text_width(&border_line(&widths, T_RIGHT, T_UP, T_LEFT));
            // chrome_w includes the leading `├` and trailing `┤`; the
            // interior available for our input content is everything
            // between them.
            let interior_w = chrome_w.saturating_sub(2);

            let prompt = spec.prompt.to_string();
            let prompt_w = console::measure_text_width(&prompt);
            let is_empty = spec.query.is_empty();
            let body_text: String = if is_empty {
                spec.placeholder.to_string()
            } else {
                spec.query.to_string()
            };
            let body_w = console::measure_text_width(&body_text);

            // Build: " " (left cell padding) + prompt + body + spaces
            // to fill remaining interior + " " (right cell padding)?
            // The table's data rows use ` cell ` (one space each side)
            // INSIDE the column. The input row spans the full interior
            // and we want one space of left padding before the prompt
            // and trailing pad to the right edge. Concretely:
            //   interior = " " + prompt + body + filler
            //   |interior| == interior_w
            let inner_used = 1 + prompt_w + body_w;
            let filler = interior_w.saturating_sub(inner_used);

            let mut input_spans: Vec<Span<'static>> = Vec::new();
            input_spans.push(Span::styled(V.to_string(), border_style));
            // Leading pad + prompt rendered with default style (the
            // prompt glyph itself is not the placeholder).
            input_spans.push(Span::raw(format!(" {prompt}")));
            // Body: italic dim placeholder when empty, plain query when not.
            if is_empty {
                input_spans.push(Span::styled(
                    body_text,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ));
            } else {
                input_spans.push(Span::raw(body_text));
            }
            // Trailing filler to the right edge of the interior.
            if filler > 0 {
                input_spans.push(Span::raw(" ".repeat(filler)));
            }
            input_spans.push(Span::styled(V.to_string(), border_style));

            // Cursor row index: the input row's index in `lines` after
            // we push it. We push divider above; the input row is
            // about to be pushed at lines.len().
            let cursor_row = lines.len() as u16;
            lines.push(Line::from(input_spans));

            // Bottom border: no `┴`s, the divider already closed them.
            lines.push(Line::from(Span::styled(
                format!("{BL}{}{BR}", H.to_string().repeat(interior_w)),
                border_style,
            )));

            // Cursor column: leading `│` (1) + leading pad (1) +
            // prompt + query (for non-empty). For an empty query the
            // cursor sits over the first placeholder character (right
            // after `> `).
            let cursor_col = 1u16 + 1u16 + prompt_w as u16 + body_w as u16;
            // When empty, body_w is the placeholder width: we don't
            // want the cursor at the placeholder's end. Park it on the
            // first placeholder cell instead.
            let cursor_col = if is_empty {
                1u16 + 1u16 + prompt_w as u16
            } else {
                cursor_col
            };
            Some((cursor_row, cursor_col))
        }
        None => {
            // Normal bottom border.
            lines.push(Line::from(Span::styled(
                border_line(&widths, BL, T_UP, BR),
                border_style,
            )));
            None
        }
    };

    (lines, cursor)
}

/// Translate a [`CellStyle`] (plus the raw cell text, for `State`
/// classification) into a ratatui `Style`. Kept private to the module:
/// every consumer renders through `render` or `render_to_lines`, not
/// directly off the style enum.
fn cell_style_to_ratatui(style: CellStyle, raw: &str) -> ratatui::style::Style {
    use ratatui::style::{Color, Modifier, Style};
    match style {
        CellStyle::Dim => Style::default().add_modifier(Modifier::DIM),
        CellStyle::Plain => Style::default(),
        CellStyle::Emphasis => Style::default().add_modifier(Modifier::BOLD),
        CellStyle::State => match classify_status(raw) {
            StatusColor::Green => Style::default().fg(Color::Green),
            StatusColor::Yellow => Style::default().fg(Color::Yellow),
            StatusColor::Red => Style::default().fg(Color::Red),
            StatusColor::Grey => Style::default().add_modifier(Modifier::DIM),
        },
        CellStyle::Cyan => Style::default().fg(Color::Cyan),
    }
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
        let widths: Vec<usize> = out.lines().map(console::measure_text_width).collect();
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
    }

    #[test]
    fn truncate_image_strips_path_segments() {
        // Final segment fits in max: strip registry + namespace, keep
        // `name:tag` intact (no ellipsis needed).
        assert_eq!(
            truncate_cell("lscr.io/linuxserver/bazarr:latest", 20, true),
            "bazarr:latest"
        );
        // The intermediate `namespace/name:tag` form fits: keep it
        // (only the registry gets stripped).
        assert_eq!(
            truncate_cell("ghcr.io/weavers/isengard-agent:next", 30, true),
            "weavers/isengard-agent:next"
        );
        // Already-short refs pass through.
        assert_eq!(truncate_cell("nginx:latest", 20, true), "nginx:latest");
    }

    #[test]
    fn truncate_image_falls_back_to_middle_when_final_segment_is_too_long() {
        // Final segment is still too long for max: middle-truncate it.
        let out = truncate_cell("ghcr.io/weavers/isengard-agent-very-long:next", 15, true);
        assert!(out.contains("..."), "should middle-truncate: {out:?}");
        assert!(
            console::measure_text_width(&out) <= 15,
            "must respect max width: {out:?}"
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
    fn render_to_lines_returns_shape_matching_render() {
        let table = Table {
            columns: ps_columns(),
            rows: ps_rows(),
        };
        let (lines, cursor) = render_to_lines(&table, 200, None, None);
        // Total lines = top border + header + header rule + N rows +
        // bottom border = N + 4.
        assert_eq!(lines.len(), ps_rows().len() + 4);
        assert!(cursor.is_none(), "no input row -> no cursor");
        // Top + bottom borders carry the rounded corners.
        let top = lines.first().unwrap().to_string();
        let bot = lines.last().unwrap().to_string();
        assert!(top.starts_with('╭') && top.ends_with('╮'), "top: {top:?}");
        assert!(bot.starts_with('╰') && bot.ends_with('╯'), "bot: {bot:?}");
        // Header rule is the only line that starts with ├.
        let rules: Vec<&ratatui::text::Line<'_>> = lines
            .iter()
            .filter(|l| l.to_string().starts_with('├'))
            .collect();
        assert_eq!(rules.len(), 1, "exactly one header rule");
        // Header carries the spec column names.
        let header = lines[1].to_string();
        assert!(header.contains("CONTAINER ID"));
        assert!(header.contains("NAMES"));
    }

    #[test]
    fn render_to_lines_highlight_paints_selected_row_but_not_right_edge() {
        use ratatui::style::Color;
        let table = Table {
            columns: ps_columns(),
            rows: ps_rows(),
        };
        let (lines, _) = render_to_lines(&table, 200, Some(1), None);
        // Selected row sits at index: top border (0) + header (1) +
        // header rule (2) + row 0 (3) + row 1 (4).
        let selected = &lines[4];
        // Every span EXCEPT the trailing right-edge `│` carries the
        // cyan-bg / black-fg override. The last span is the box's
        // closing edge and must keep the dim border style: the
        // highlight stops AT the rightmost `│`, never past it.
        let n = selected.spans.len();
        assert!(n >= 2, "row must have at least leading + trailing borders");
        for (idx, span) in selected.spans.iter().enumerate() {
            if idx + 1 == n {
                // Trailing `│`: dim border, no highlight.
                assert_eq!(
                    span.content.as_ref(),
                    "│",
                    "last span must be the right-edge border glyph"
                );
                assert_ne!(
                    span.style.bg,
                    Some(Color::Cyan),
                    "right-edge `│` must NOT carry highlight bg: {span:?}"
                );
            } else {
                assert_eq!(
                    span.style.bg,
                    Some(Color::Cyan),
                    "interior span missing highlight bg at idx {idx}: {span:?}"
                );
                assert_eq!(
                    span.style.fg,
                    Some(Color::Black),
                    "interior span missing highlight fg at idx {idx}: {span:?}"
                );
            }
        }
        // A non-highlighted row keeps the dim border style.
        let other = &lines[3];
        assert!(
            other.spans.iter().all(|s| s.style.bg != Some(Color::Cyan)),
            "non-selected row should not be highlighted"
        );
    }

    #[test]
    fn render_to_lines_with_no_rows_emits_just_chrome() {
        let table = Table {
            columns: ps_columns(),
            rows: vec![],
        };
        let (lines, cursor) = render_to_lines(&table, 200, None, None);
        // Top border + header + header rule + bottom border = 4.
        assert_eq!(lines.len(), 4);
        assert!(cursor.is_none());
        assert!(lines[0].to_string().starts_with('╭'));
        assert!(lines[2].to_string().starts_with('├'));
        assert!(lines[3].to_string().starts_with('╰'));
    }

    #[test]
    fn render_to_lines_with_input_row_empty_query_shows_italic_placeholder() {
        use ratatui::style::{Color, Modifier};
        let table = Table {
            columns: ps_columns(),
            rows: ps_rows(),
        };
        let input = InputRow {
            query: "",
            placeholder: "filter hosts...",
            prompt: "> ",
        };
        let (lines, cursor) = render_to_lines(&table, 200, None, Some(input));
        // top + header + rule + N rows + divider + input row + bottom.
        assert_eq!(lines.len(), ps_rows().len() + 6);
        // Divider closes inner columns with `┴` and joins outer borders.
        let divider = &lines[lines.len() - 3];
        let dtxt = divider.to_string();
        assert!(dtxt.starts_with('├'), "divider starts with ├: {dtxt:?}");
        assert!(dtxt.ends_with('┤'), "divider ends with ┤: {dtxt:?}");
        assert!(
            dtxt.contains('┴'),
            "divider has at least one ┴ join: {dtxt:?}"
        );
        // Input row carries the placeholder text in italic + DarkGray.
        let input_line = &lines[lines.len() - 2];
        let body_span = input_line
            .spans
            .iter()
            .find(|s| s.content.contains("filter hosts..."))
            .expect("placeholder span present");
        assert!(
            body_span.style.add_modifier.contains(Modifier::ITALIC),
            "placeholder must be italic: {body_span:?}"
        );
        assert_eq!(
            body_span.style.fg,
            Some(Color::DarkGray),
            "placeholder must be DarkGray: {body_span:?}"
        );
        // Bottom border is plain `╰─...─╯` with NO `┴` joins (divider
        // already closed them).
        let bottom = &lines[lines.len() - 1];
        let btxt = bottom.to_string();
        assert!(btxt.starts_with('╰'));
        assert!(btxt.ends_with('╯'));
        assert!(
            !btxt.contains('┴'),
            "bottom border under input row should not carry ┴ joins: {btxt:?}"
        );
        // Cursor parked on the first placeholder char (right after `> `).
        let (row, col) = cursor.expect("input row must yield cursor");
        assert_eq!(row, (lines.len() - 2) as u16, "cursor on the input row");
        // Column = 1 (leading `│`) + 1 (pad) + 2 (`> `) = 4.
        assert_eq!(col, 4, "cursor sits over first placeholder cell: {col}");
    }

    #[test]
    fn render_to_lines_with_input_row_non_empty_query_hides_placeholder() {
        use ratatui::style::{Color, Modifier};
        let table = Table {
            columns: ps_columns(),
            rows: ps_rows(),
        };
        let input = InputRow {
            query: "lau",
            placeholder: "filter hosts...",
            prompt: "> ",
        };
        let (lines, cursor) = render_to_lines(&table, 200, None, Some(input));
        let input_line = &lines[lines.len() - 2];
        let txt = input_line.to_string();
        assert!(txt.contains("> lau"), "input row shows `> lau`: {txt:?}");
        assert!(
            !txt.contains("filter hosts..."),
            "placeholder must vanish once the query is non-empty: {txt:?}"
        );
        // No span on the input row may be italic/DarkGray now.
        for span in &input_line.spans {
            let is_placeholder_style = span.style.add_modifier.contains(Modifier::ITALIC)
                || span.style.fg == Some(Color::DarkGray);
            assert!(
                !is_placeholder_style,
                "non-empty query must not carry placeholder styling: {span:?}"
            );
        }
        // Cursor sits at the end of `> lau`: 1 (`│`) + 1 (pad) + 2 (`> `) + 3 (`lau`) = 7.
        let (_, col) = cursor.expect("input row must yield cursor");
        assert_eq!(col, 7, "cursor sits at the end of the query: {col}");
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
        assert_eq!(
            classify_status("Restarting (1) 4 seconds ago"),
            StatusColor::Yellow
        );
        assert_eq!(
            classify_status("Exited (0) 12 minutes ago"),
            StatusColor::Grey
        );
        assert_eq!(classify_status("Exited (137) 1 hour ago"), StatusColor::Red);
        assert_eq!(classify_status("Dead"), StatusColor::Red);
        assert_eq!(classify_status("Created"), StatusColor::Grey);
        assert_eq!(classify_status("Paused"), StatusColor::Grey);
        assert_eq!(classify_status(""), StatusColor::Grey);
    }
}
