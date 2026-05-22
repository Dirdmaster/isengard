//! Generic single-column ratatui picker for the "no positional arg"
//! interactive fallback. Distinct from [`crate::ssh::picker`] which
//! keeps its own purpose-built `NAME / DIAL TARGET / LAST SEEN`
//! chrome.
//!
//! Same visual language as the ssh picker (boxed table, `┴`-joined
//! divider, folded `> filter ...` input row, vim modal with `NOR`/`INS`
//! badge) so the operator sees one consistent picker shape everywhere.
//! Built on `crate::render::render_to_lines` so the shared layout +
//! highlight code lives in exactly one place.
//!
//! Callers supply the column header (e.g. `KEY` for `isd configure
//! get`, `NAME` for `isd secret rm`) and a list of row strings. The
//! picker returns the picked string (or `Ok(None)` on Esc / Ctrl-C).

use std::io::{Stdout, stdout};

use anyhow::{Context as _, Result};
use crossterm::{
    cursor,
    event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt as _;
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::{
    Terminal, TerminalOptions, Viewport, backend::CrosstermBackend, text::Text, widgets::Paragraph,
};

use crate::render::{
    Align, CellStyle, Column, InputRow, ModeBadge, Table as RenderTable, render_to_lines,
};

/// Compute the inline viewport height (rows) for `n_rows` source items.
/// Mirrors `ssh::picker::picker_height` so the generic picker shares
/// the same layout budget (chrome + filtered rows + divider + input).
pub fn picker_height(n_rows: usize) -> u16 {
    let raw = (n_rows as u16).saturating_add(6);
    raw.clamp(8, 18)
}

/// Score one row against the filter query. Empty needle keeps every
/// row in the original order (so the unfiltered list draws as the
/// caller supplied it).
fn score_row(matcher: &mut Matcher, row: &str, needle: &str) -> Option<u16> {
    if needle.is_empty() {
        return Some(0);
    }
    let mut nbuf = Vec::new();
    let mut hbuf = Vec::new();
    let needle_u32 = Utf32Str::new(needle, &mut nbuf);
    let hay_u32 = Utf32Str::new(row, &mut hbuf);
    matcher.fuzzy_match(hay_u32, needle_u32)
}

/// Filter + rank rows against `query`. Empty query keeps the original
/// order; non-empty returns matches sorted by descending nucleo score,
/// breaking ties on the original index for stability.
pub fn score_rows_by_filter(rows: &[String], query: &str) -> Vec<String> {
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut scored: Vec<(u16, usize, String)> = rows
        .iter()
        .enumerate()
        .filter_map(|(idx, r)| score_row(&mut matcher, r, query).map(|s| (s, idx, r.clone())))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, r)| r).collect()
}

/// Column layout: `#` (dim, right) + caller-supplied header (emphasis,
/// left). Mirrors the ssh picker's first two columns so the operator
/// reads the same affordance everywhere.
fn picker_columns(header: &'static str) -> Vec<Column> {
    vec![
        Column::new("#", Align::Right, CellStyle::Dim, 9, 1),
        Column::new(header, Align::Left, CellStyle::Emphasis, 1, 8),
    ]
}

/// Build the table for the currently visible rows.
fn picker_table(header: &'static str, visible: &[String]) -> RenderTable {
    let rows: Vec<Vec<String>> = visible
        .iter()
        .enumerate()
        .map(|(i, r)| vec![i.to_string(), r.clone()])
        .collect();
    RenderTable {
        columns: picker_columns(header),
        rows,
    }
}

/// Picker entry point. Opens an inline ratatui viewport, lets the
/// operator filter + pick, returns the picked row (or `Ok(None)` on
/// Esc / Ctrl-C).
///
/// `header` is the column label rendered above the data column
/// (e.g. `"KEY"`, `"NAME"`).
/// `placeholder` is the muted italic hint shown in the empty filter
/// input (e.g. `"filter keys..."`).
///
/// # Errors
///
/// Returns `Err` on terminal init / draw failure. Empty-list early-out
/// is the caller's job.
pub async fn pick(
    rows: Vec<String>,
    header: &'static str,
    placeholder: &'static str,
) -> Result<Option<String>> {
    install_panic_hook();

    enable_raw_mode().context("enabling raw mode")?;
    let backend = CrosstermBackend::new(stdout());
    let height = picker_height(rows.len());
    let opts = TerminalOptions {
        viewport: Viewport::Inline(height),
    };
    let mut terminal =
        Terminal::with_options(backend, opts).context("creating inline terminal backend")?;
    let _guard = TermGuard;

    let outcome = event_loop(&mut terminal, rows, header, placeholder).await;

    let _ = terminal.clear();
    drop(terminal);
    outcome
}

/// RAII: disable raw mode + restore cursor visibility on drop.
struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let mut out = stdout();
        let _ = execute!(out, cursor::Show);
        let _ = disable_raw_mode();
    }
}

/// Restore the terminal in the panic path too.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut out = stdout();
        let _ = execute!(out, cursor::Show);
        let _ = disable_raw_mode();
        previous(info);
    }));
}

/// Drive the filter+pick event loop until the operator hits Enter or
/// cancels.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    rows: Vec<String>,
    header: &'static str,
    placeholder: &'static str,
) -> Result<Option<String>> {
    let mut state = State::new(rows);
    let mut events = EventStream::new();
    loop {
        terminal.draw(|f| draw(f, &mut state, header, placeholder))?;
        let evt = match events.next().await {
            Some(Ok(e)) => e,
            Some(Err(_)) => continue,
            None => break,
        };
        let Event::Key(key) = evt else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match handle_key(&mut state, key) {
            KeyOutcome::Continue => {}
            KeyOutcome::Cancel => return Ok(None),
            KeyOutcome::Pick(s) => return Ok(Some(s)),
        }
    }
    Ok(None)
}

/// Vim modal state: NORMAL by default, INSERT after `/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Navigate the visible list; printable chars are inert.
    Normal,
    /// Filter input active; cursor visible.
    Insert,
}

/// Picker TUI state. Filter mutates in INSERT mode; `visible` is
/// recomputed from `all` + `filter` on every keystroke.
struct State {
    /// Full source list, unchanged after construction.
    all: Vec<String>,
    /// Currently-visible subset, ranked by nucleo score.
    visible: Vec<String>,
    /// Operator-typed filter buffer.
    filter: String,
    /// Selection cursor over `visible`.
    selected: Option<usize>,
    /// Vim modal state.
    mode: Mode,
}

impl State {
    /// Seed picker state from the source list. Selects the first row
    /// when available; opens in NORMAL mode.
    fn new(rows: Vec<String>) -> Self {
        let selected = if rows.is_empty() { None } else { Some(0) };
        Self {
            visible: rows.clone(),
            all: rows,
            filter: String::new(),
            selected,
            mode: Mode::Normal,
        }
    }

    /// Recompute `visible` from `all` + `filter` and reset the
    /// selection cursor to the top match.
    fn refresh(&mut self) {
        self.visible = score_rows_by_filter(&self.all, &self.filter);
        self.selected = if self.visible.is_empty() {
            None
        } else {
            Some(0)
        };
    }

    /// Currently highlighted row, if any.
    fn selected_row(&self) -> Option<String> {
        let idx = self.selected?;
        self.visible.get(idx).cloned()
    }

    /// Move the cursor by `delta` rows, clamped at the visible list's
    /// ends (no wraparound).
    fn move_selection(&mut self, delta: i32) {
        let len = self.visible.len();
        if len == 0 {
            return;
        }
        let cur = self.selected.unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, len as i32 - 1) as usize;
        self.selected = Some(next);
    }

    /// Jump to the first visible row.
    fn jump_first(&mut self) {
        if !self.visible.is_empty() {
            self.selected = Some(0);
        }
    }

    /// Jump to the last visible row.
    fn jump_last(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        self.selected = Some(self.visible.len() - 1);
    }

    /// Half-page step for vim `Ctrl-d`/`Ctrl-u` motions.
    fn half_page(&self) -> i32 {
        let len = self.visible.len();
        if len <= 1 {
            1
        } else {
            (len.div_ceil(2)) as i32
        }
    }
}

/// Outcome of one keypress.
enum KeyOutcome {
    /// Stay in the loop, re-draw.
    Continue,
    /// Operator hit Esc / Ctrl-C: caller returns `None`.
    Cancel,
    /// Operator hit Enter on a row: caller returns the row text.
    Pick(String),
}

/// Translate one key event into a [`KeyOutcome`]. Dispatches to the
/// mode-specific handler so NORMAL and INSERT bindings stay readable.
fn handle_key(state: &mut State, key: KeyEvent) -> KeyOutcome {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return KeyOutcome::Cancel;
    }
    match state.mode {
        Mode::Normal => apply_key_normal(state, key),
        Mode::Insert => apply_key_insert(state, key),
    }
}

/// NORMAL-mode bindings: vim-style navigation + `/` to enter INSERT.
fn apply_key_normal(state: &mut State, key: KeyEvent) -> KeyOutcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D')) {
        let step = state.half_page();
        state.move_selection(step);
        return KeyOutcome::Continue;
    }
    if ctrl && matches!(key.code, KeyCode::Char('u') | KeyCode::Char('U')) {
        let step = state.half_page();
        state.move_selection(-step);
        return KeyOutcome::Continue;
    }
    match key.code {
        KeyCode::Esc => KeyOutcome::Cancel,
        KeyCode::Enter => match state.selected_row() {
            Some(s) => KeyOutcome::Pick(s),
            None => KeyOutcome::Continue,
        },
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
            state.move_selection(-1);
            KeyOutcome::Continue
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
            state.move_selection(1);
            KeyOutcome::Continue
        }
        KeyCode::Char('g') => {
            state.jump_first();
            KeyOutcome::Continue
        }
        KeyCode::Char('G') => {
            state.jump_last();
            KeyOutcome::Continue
        }
        KeyCode::Char('/') => {
            state.mode = Mode::Insert;
            KeyOutcome::Continue
        }
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyOutcome::Cancel,
        KeyCode::Char('c') | KeyCode::Char('C') => {
            state.filter.clear();
            state.refresh();
            KeyOutcome::Continue
        }
        _ => KeyOutcome::Continue,
    }
}

/// INSERT-mode bindings: filter input + cursor motions via Ctrl-N/P.
fn apply_key_insert(state: &mut State, key: KeyEvent) -> KeyOutcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && matches!(key.code, KeyCode::Char('u') | KeyCode::Char('U')) {
        state.filter.clear();
        state.refresh();
        return KeyOutcome::Continue;
    }
    if ctrl && matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N')) {
        state.move_selection(1);
        return KeyOutcome::Continue;
    }
    if ctrl && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P')) {
        state.move_selection(-1);
        return KeyOutcome::Continue;
    }
    if ctrl && matches!(key.code, KeyCode::Char('h') | KeyCode::Char('H')) {
        state.filter.pop();
        state.refresh();
        return KeyOutcome::Continue;
    }
    match key.code {
        KeyCode::Esc => {
            state.mode = Mode::Normal;
            KeyOutcome::Continue
        }
        KeyCode::Enter => match state.selected_row() {
            Some(s) => KeyOutcome::Pick(s),
            None => KeyOutcome::Continue,
        },
        KeyCode::Up => {
            state.move_selection(-1);
            KeyOutcome::Continue
        }
        KeyCode::Down => {
            state.move_selection(1);
            KeyOutcome::Continue
        }
        KeyCode::Backspace => {
            state.filter.pop();
            state.refresh();
            KeyOutcome::Continue
        }
        KeyCode::Char(c) => {
            if !ctrl {
                state.filter.push(c);
                state.refresh();
            }
            KeyOutcome::Continue
        }
        _ => KeyOutcome::Continue,
    }
}

/// Draw the picker frame: boxed table with a folded `> ...` input row.
fn draw(
    f: &mut ratatui::Frame<'_>,
    state: &mut State,
    header: &'static str,
    placeholder: &'static str,
) {
    use ratatui::style::Color;
    let area = f.area();
    let table = picker_table(header, &state.visible);
    let badge = match state.mode {
        Mode::Normal => ModeBadge {
            label: "NOR",
            color: Color::Yellow,
        },
        Mode::Insert => ModeBadge {
            label: "INS",
            color: Color::Green,
        },
    };
    let show_cursor = state.mode == Mode::Insert;
    let input = InputRow {
        query: &state.filter,
        placeholder,
        prompt: "> ",
        mode: Some(badge),
        show_cursor,
    };
    let (lines, cursor) = render_to_lines(&table, area.width as usize, state.selected, Some(input));
    let paragraph = Paragraph::new(Text::from(lines));
    f.render_widget(paragraph, area);
    if let Some((row, col)) = cursor {
        f.set_cursor_position((area.x + col, area.y + row));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_height_clamps_to_eight_and_eighteen() {
        assert_eq!(picker_height(0), 8);
        assert_eq!(picker_height(2), 8);
        assert_eq!(picker_height(3), 9);
        assert_eq!(picker_height(12), 18);
        assert_eq!(picker_height(100), 18);
    }

    #[test]
    fn score_rows_empty_query_preserves_order() {
        let rows = vec!["alpha".into(), "beta".into(), "gamma".into()];
        let out = score_rows_by_filter(&rows, "");
        assert_eq!(out, rows);
    }

    #[test]
    fn score_rows_filters_and_ranks() {
        let rows = vec![
            "cloudflare.api_token".into(),
            "acme.directory".into(),
            "routing.default_zone".into(),
        ];
        let out = score_rows_by_filter(&rows, "acme");
        assert!(!out.is_empty());
        assert_eq!(out[0], "acme.directory");
    }

    #[test]
    fn score_rows_no_match_returns_empty() {
        let rows = vec!["a".into(), "b".into()];
        let out = score_rows_by_filter(&rows, "x9z");
        assert!(out.is_empty());
    }

    #[test]
    fn state_arrow_keys_clamp_at_ends() {
        let mut st = State::new(vec!["a".into(), "b".into()]);
        assert_eq!(st.selected, Some(0));
        st.move_selection(1);
        assert_eq!(st.selected, Some(1));
        st.move_selection(1);
        assert_eq!(st.selected, Some(1));
        st.move_selection(-1);
        assert_eq!(st.selected, Some(0));
        st.move_selection(-1);
        assert_eq!(st.selected, Some(0));
    }

    #[test]
    fn state_typing_filter_refreshes_visible() {
        let mut st = State::new(vec!["alpha".into(), "beta".into()]);
        st.filter.push_str("alp");
        st.refresh();
        assert_eq!(st.visible.len(), 1);
        assert_eq!(st.visible[0], "alpha");
        assert_eq!(st.selected, Some(0));
    }

    #[test]
    fn state_no_match_clears_selection() {
        let mut st = State::new(vec!["alpha".into()]);
        st.filter.push_str("x9z");
        st.refresh();
        assert!(st.visible.is_empty());
        assert_eq!(st.selected, None);
        assert_eq!(st.selected_row(), None);
    }
}
