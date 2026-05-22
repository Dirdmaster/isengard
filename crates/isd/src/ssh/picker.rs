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
//! Frontend: native ratatui inline TUI (no alt-screen takeover),
//! fuzzy-matched via `nucleo-matcher`. The earlier fzf + inquire dance
//! was removed because fzf opens `/dev/tty` with raw ioctls that
//! bypass capture tools (cheese, asciinema's pty wrap). The native
//! picker keeps all output inside crossterm so screen capture works
//! and the visual language matches `isd configure`. Inline rendering
//! reserves a small viewport at the bottom of the current shell and
//! clears it silently on exit, so the surrounding shell flow stays
//! visible (gh/gum aesthetic).
//!
//! Visual: the picker IS the `isd ssh hosts` table. Same chrome from
//! the unified renderer (rounded corners, vertical separators between
//! columns, ALL CAPS dim headers, one header rule, `#` dim, NAME bold,
//! DIAL TARGET cyan, LAST SEEN dim). The selected row is painted with
//! a cyan background and black foreground inside the renderer. The
//! filter input sits BELOW the table as a `> <query>` line (fzf
//! default layout). The intent: an operator who knows `isd ssh hosts`
//! recognises the picker at a glance and the columns line up so the
//! `#` they read off the listing matches the `#` in the picker.

use std::collections::HashSet;
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
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::Paragraph,
};

use crate::render::{Align, CellStyle, Column, Table as RenderTable, render_to_lines};

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
/// the controller has never heard from. `dial_target` is the
/// operator-facing dial string (`user@host[:port]`) when known.
#[derive(Debug, Clone)]
pub struct PickerRow {
    /// Dial target as the operator typed it (or as the agent reported).
    pub hostname: String,
    /// Source bucket. Constructed by callers but not currently surfaced
    /// in the minimalist render. Kept for future visual cues (e.g., a
    /// glyph column distinguishing isengard from trusted hosts).
    #[allow(dead_code)]
    pub class: HostClass,
    /// RFC3339 timestamp the agent last heartbeated, when known.
    pub last_seen: Option<String>,
    /// Operator-facing dial target. `None` when the controller never
    /// captured one (pre-PR-B enroll) or the trusted entry omitted
    /// `ssh_user`.
    pub dial_target: Option<String>,
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

/// Score a row against the filter query. Empty query returns `Some(0)`
/// for every row (so the unfiltered list preserves original order
/// after a stable sort). Non-empty queries return the nucleo fuzzy
/// score or `None` when the row does not match.
fn score_row(matcher: &mut Matcher, row: &PickerRow, needle: &str) -> Option<u16> {
    if needle.is_empty() {
        return Some(0);
    }
    let haystack_str = match row.dial_target.as_deref() {
        Some(t) => format!("{} {}", row.hostname, t),
        None => row.hostname.clone(),
    };
    let mut nbuf = Vec::new();
    let mut hbuf = Vec::new();
    let needle_u32 = Utf32Str::new(needle, &mut nbuf);
    let hay_u32 = Utf32Str::new(&haystack_str, &mut hbuf);
    matcher.fuzzy_match(hay_u32, needle_u32)
}

/// Filter + rank rows against `query`. Empty query: original order,
/// every row kept. Non-empty: only matched rows, sorted by descending
/// nucleo score. Stable sort preserves original order between ties so
/// the Isengard-first ordering from `merge_sources` survives.
pub fn score_rows_by_filter(rows: &[PickerRow], query: &str) -> Vec<PickerRow> {
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut scored: Vec<(u16, usize, PickerRow)> = rows
        .iter()
        .enumerate()
        .filter_map(|(idx, r)| score_row(&mut matcher, r, query).map(|s| (s, idx, r.clone())))
        .collect();
    // Sort by score desc, then original index asc. `sort_by` is stable
    // but we encode the tie-breaker explicitly so the order is total.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, r)| r).collect()
}

/// Column layout for the picker table. Mirrors `isd ssh hosts` so the
/// operator sees the same chrome they get from the list command:
/// `#` dim + right-aligned, `NAME` emphasized (bold), `DIAL TARGET`
/// cyan (the action verb), `LAST SEEN` dim. The picker upgrades the
/// dial-target style from Plain (default in `ssh hosts`) to Cyan
/// because the picker's primary affordance is "pick a thing to dial".
fn picker_columns() -> Vec<Column> {
    vec![
        Column::new("#", Align::Right, CellStyle::Dim, 9, 1),
        Column::new("NAME", Align::Left, CellStyle::Emphasis, 6, 8),
        Column::new("DIAL TARGET", Align::Left, CellStyle::Cyan, 5, 14),
        Column::new("LAST SEEN", Align::Left, CellStyle::Dim, 1, 8),
    ]
}

/// Build the cells for one picker row. `index` is the row's position
/// in the CURRENT filtered list (0..N-1), so the operator can type the
/// visible number to jump-pick once that wiring lands.
fn picker_row_cells(index: usize, row: &PickerRow) -> Vec<String> {
    let dial = row.dial_target.clone().unwrap_or_else(|| "(unset)".into());
    let last = relative_time(row.last_seen.as_deref());
    vec![index.to_string(), row.hostname.clone(), dial, last]
}

/// Build the picker's `RenderTable` from the currently visible rows.
/// The `#` column carries the filtered-list index (not the source-list
/// index) so the displayed numbers always start at 0 and stay dense.
fn picker_table(visible: &[PickerRow]) -> RenderTable {
    let rows: Vec<Vec<String>> = visible
        .iter()
        .enumerate()
        .map(|(i, r)| picker_row_cells(i, r))
        .collect();
    RenderTable {
        columns: picker_columns(),
        rows,
    }
}

/// Format an RFC3339 timestamp as a coarse relative-time string
/// ("2m ago", "3h ago", "5d ago"). Falls back to "-" on missing / bad
/// input, "just now" for under 60s. Pure function so the picker stays
/// testable without freezing the system clock.
fn relative_time(rfc3339: Option<&str>) -> String {
    let Some(raw) = rfc3339 else {
        return "-".into();
    };
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) else {
        return raw.to_string();
    };
    let now = chrono::Utc::now();
    let delta = now.signed_duration_since(parsed.with_timezone(&chrono::Utc));
    let secs = delta.num_seconds();
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Compute the inline viewport height (rows) for `n_rows` source items.
///
/// Layout budget for the boxed picker: top border (1) + header (1) +
/// header rule (1) + N data rows + bottom border (1) + filter line (1)
/// = N + 6. Cap at 18 so a long fleet does not eat the operator's
/// scrollback. Floor at 8 so the picker still has a parseable shape
/// (chrome + at least two data slots) with an empty / tiny fleet.
pub fn picker_height(n_rows: usize) -> u16 {
    let raw = (n_rows as u16).saturating_add(6);
    raw.clamp(8, 18)
}

/// Picker public entry. Opens an inline ratatui viewport at the bottom
/// of the current shell, lets the operator filter + pick, returns the
/// chosen hostname (or `Ok(None)` on Esc / Ctrl-C / empty filter
/// dismiss). On exit the viewport is silently cleared so the next
/// shell prompt or command output starts cleanly.
///
/// # Errors
///
/// Returns `Err` on terminal init/draw failure. Empty-list early-out
/// is the caller's job (see `ssh::run_picker`).
pub async fn pick(rows: Vec<PickerRow>) -> Result<Option<String>> {
    // Install panic hook BEFORE switching to raw mode so a panic
    // between enable_raw_mode and the guard's construction still
    // restores the terminal.
    install_panic_hook();

    enable_raw_mode().context("enabling raw mode")?;
    let stdout_handle = stdout();
    let backend = CrosstermBackend::new(stdout_handle);
    let height = picker_height(rows.len());
    let opts = TerminalOptions {
        viewport: Viewport::Inline(height),
    };
    let mut terminal =
        Terminal::with_options(backend, opts).context("creating inline terminal backend")?;
    let _guard = TermGuard;

    let outcome = event_loop(&mut terminal, rows).await;

    // Silent clear: wipe the inline viewport area so the shell prompt
    // / subsequent command output starts on a clean line. The guard's
    // Drop is the safety net for panics; this is the happy path.
    let _ = terminal.clear();
    drop(terminal);
    outcome
}

/// RAII guard that disables raw mode and restores cursor visibility
/// on drop. Stack-allocated so unwinding through the TUI still
/// restores the terminal. No alt-screen exit needed: inline rendering
/// never entered one.
struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let mut out = stdout();
        let _ = execute!(out, cursor::Show);
        let _ = disable_raw_mode();
    }
}

/// Install a panic hook that restores the terminal before delegating
/// to the previous hook. Called once per `pick`; subsequent calls
/// chain so multiple TUIs in one process still restore correctly.
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
/// cancels. Returns the picked hostname on Enter, `None` on Esc /
/// Ctrl-C.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    rows: Vec<PickerRow>,
) -> Result<Option<String>> {
    let mut state = PickerState::new(rows);
    let mut events = EventStream::new();
    loop {
        terminal.draw(|f| draw(f, &mut state))?;
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
            KeyOutcome::Pick(host) => return Ok(Some(host)),
        }
    }
    Ok(None)
}

/// Picker TUI state. Filter is mutated as the operator types;
/// `visible` is recomputed every keystroke from `all` + `filter`.
struct PickerState {
    /// Full source list, unchanged after construction. Acts as the
    /// pool every filter pass scores against.
    all: Vec<PickerRow>,
    /// Currently-visible subset, ranked by nucleo score. Recomputed
    /// from `all` + `filter` on every keystroke via [`Self::refresh`].
    visible: Vec<PickerRow>,
    /// Operator-typed filter buffer.
    filter: String,
    /// Selection cursor over `visible`. `None` when the filter has
    /// no matches.
    selected: Option<usize>,
}

impl PickerState {
    /// Seed picker state from the merged source list. Selects the
    /// first row (or `None` when the list is empty).
    fn new(rows: Vec<PickerRow>) -> Self {
        let selected = if rows.is_empty() { None } else { Some(0) };
        Self {
            visible: rows.clone(),
            all: rows,
            filter: String::new(),
            selected,
        }
    }

    /// Recompute `visible` from `all` + `filter` and reset the
    /// selection cursor to the top match. Called after every filter
    /// edit so the first row is always highlighted; Enter on an
    /// untouched filter picks the first row in source order.
    fn refresh(&mut self) {
        self.visible = score_rows_by_filter(&self.all, &self.filter);
        self.selected = if self.visible.is_empty() {
            None
        } else {
            Some(0)
        };
    }

    /// Currently highlighted hostname (`None` when no row matches).
    fn selected_hostname(&self) -> Option<String> {
        let idx = self.selected?;
        self.visible.get(idx).map(|r| r.hostname.clone())
    }

    /// Move the cursor by `delta` rows, wrapping at the visible
    /// list's ends. No-op when no row matches the current filter.
    fn move_selection(&mut self, delta: i32) {
        let len = self.visible.len();
        if len == 0 {
            return;
        }
        let cur = self.selected.unwrap_or(0) as i32;
        let next = (cur + delta).rem_euclid(len as i32) as usize;
        self.selected = Some(next);
    }
}

/// Outcome of one keypress in the picker.
enum KeyOutcome {
    /// Stay in the loop, re-draw.
    Continue,
    /// Operator hit Esc / Ctrl-C: caller returns `None`.
    Cancel,
    /// Operator hit Enter on a row: caller returns the hostname.
    Pick(String),
}

/// Translate one key event into a `KeyOutcome`. Pure function over
/// state mutation: no IO, no draws, no terminal calls. Lets the event
/// loop stay thin.
fn handle_key(state: &mut PickerState, key: KeyEvent) -> KeyOutcome {
    // Global Ctrl-C exits.
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return KeyOutcome::Cancel;
    }
    // Ctrl-U clears the filter (readline parity).
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('u') | KeyCode::Char('U'))
    {
        state.filter.clear();
        state.refresh();
        return KeyOutcome::Continue;
    }
    match key.code {
        KeyCode::Esc => KeyOutcome::Cancel,
        KeyCode::Enter => match state.selected_hostname() {
            Some(h) => KeyOutcome::Pick(h),
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
            // Reject control chars not handled above; only printable
            // characters extend the filter.
            if !key.modifiers.contains(KeyModifiers::CONTROL) {
                state.filter.push(c);
                state.refresh();
            }
            KeyOutcome::Continue
        }
        _ => KeyOutcome::Continue,
    }
}

/// Render the picker frame. The viewport is split into the boxed
/// hosts table on top and a single `> <query>` filter line at the
/// bottom: fzf default layout. The table is built via the shared
/// `crate::render::render_to_lines` so the chrome (rounded corners,
/// vertical separators, ALL-CAPS dim headers, one header rule) and
/// per-column styling (`#` dim, NAME bold, DIAL TARGET cyan, LAST
/// SEEN dim) match `isd ssh hosts` exactly. The selected row is
/// painted with a cyan background + black foreground inside the
/// renderer so the highlight survives the same ratatui paint pass.
fn draw(f: &mut ratatui::Frame<'_>, state: &mut PickerState) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),    // boxed hosts table
            Constraint::Length(1), // filter input at the bottom
        ])
        .split(area);

    // Build the table from the currently visible rows. The highlight
    // index is the picker's `selected` cursor: when the filter
    // excludes everything (`selected == None`), no row is painted.
    let table = picker_table(&state.visible);
    let lines = render_to_lines(&table, chunks[0].width as usize, state.selected);
    let table_paragraph = Paragraph::new(Text::from(lines));
    f.render_widget(table_paragraph, chunks[0]);

    // Filter line at the bottom. Yellow `>` prompt + raw query text;
    // ratatui draws the terminal cursor at the end of this line so
    // the operator sees a familiar input affordance.
    let prompt = Span::styled("> ", Style::default().fg(Color::Yellow));
    let query = Span::raw(state.filter.clone());
    let filter_line = Paragraph::new(Line::from(vec![prompt, query]));
    f.render_widget(filter_line, chunks[1]);

    // Position the visible cursor at the end of the filter line.
    // `2` accounts for the "> " prompt prefix.
    let cursor_x = chunks[1].x + 2 + state.filter.chars().count() as u16;
    f.set_cursor_position((cursor_x, chunks[1].y));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isen(host: &str, dial: Option<&str>, seen: Option<&str>) -> PickerRow {
        PickerRow {
            hostname: host.into(),
            class: HostClass::Isengard,
            last_seen: seen.map(|s| s.into()),
            dial_target: dial.map(|s| s.into()),
        }
    }

    fn trust(host: &str) -> PickerRow {
        PickerRow {
            hostname: host.into(),
            class: HostClass::Trusted,
            last_seen: None,
            dial_target: None,
        }
    }

    #[test]
    fn merge_sources_dedups_by_hostname_isengard_wins() {
        let isengard = vec![isen("edge-1", None, Some("2026-05-21T10:00:00Z"))];
        let trusted = vec![trust("edge-1")];
        let merged = merge_sources(isengard, trusted);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].class, HostClass::Isengard);
        assert_eq!(merged[0].last_seen.as_deref(), Some("2026-05-21T10:00:00Z"));
    }

    #[test]
    fn merge_sources_appends_unique_trusted() {
        let isengard = vec![isen("edge-1", None, None)];
        let trusted = vec![trust("media.local")];
        let merged = merge_sources(isengard, trusted);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].hostname, "edge-1");
        assert_eq!(merged[1].hostname, "media.local");
        assert_eq!(merged[1].class, HostClass::Trusted);
    }

    #[test]
    fn merge_sources_preserves_isengard_order() {
        let isengard = vec![isen("a", None, None), isen("b", None, None)];
        let trusted = vec![trust("c")];
        let merged = merge_sources(isengard, trusted);
        let names: Vec<_> = merged.iter().map(|r| r.hostname.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn score_rows_empty_query_keeps_original_order() {
        let rows = vec![isen("a", None, None), isen("b", None, None), trust("c")];
        let out = score_rows_by_filter(&rows, "");
        let names: Vec<_> = out.iter().map(|r| r.hostname.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn score_rows_filters_and_ranks() {
        let rows = vec![
            isen("lausanne", Some("dirdmaster@10.17.0.125"), None),
            isen("edge-fra", Some("dirdmaster@10.0.0.42"), None),
            trust("media"),
        ];
        let out = score_rows_by_filter(&rows, "lau");
        assert!(!out.is_empty(), "lausanne must match 'lau'");
        assert_eq!(out[0].hostname, "lausanne");
    }

    #[test]
    fn score_rows_no_match_returns_empty() {
        let rows = vec![isen("lausanne", None, None), trust("media")];
        let out = score_rows_by_filter(&rows, "x9z");
        assert!(out.is_empty());
    }

    #[test]
    fn score_rows_matches_against_dial_target() {
        // Operator types part of the IP; nucleo should find the host
        // by dial_target even when the hostname does not match.
        let rows = vec![
            isen("lausanne", Some("dirdmaster@10.17.0.125"), None),
            isen("edge-fra", Some("ops@10.0.0.42"), None),
        ];
        let out = score_rows_by_filter(&rows, "10.17");
        assert!(!out.is_empty());
        assert_eq!(out[0].hostname, "lausanne");
    }

    #[test]
    fn picker_state_arrow_keys_wrap() {
        let rows = vec![isen("a", None, None), isen("b", None, None)];
        let mut st = PickerState::new(rows);
        assert_eq!(st.selected, Some(0));
        st.move_selection(1);
        assert_eq!(st.selected, Some(1));
        st.move_selection(1);
        assert_eq!(st.selected, Some(0));
        st.move_selection(-1);
        assert_eq!(st.selected, Some(1));
    }

    #[test]
    fn picker_state_typing_filter_refreshes_visible() {
        let rows = vec![isen("lausanne", None, None), isen("edge-fra", None, None)];
        let mut st = PickerState::new(rows);
        st.filter.push_str("lau");
        st.refresh();
        assert_eq!(st.visible.len(), 1);
        assert_eq!(st.visible[0].hostname, "lausanne");
        assert_eq!(st.selected, Some(0));
    }

    #[test]
    fn picker_state_no_match_clears_selection() {
        let rows = vec![isen("lausanne", None, None)];
        let mut st = PickerState::new(rows);
        st.filter.push_str("x9z");
        st.refresh();
        assert!(st.visible.is_empty());
        assert_eq!(st.selected, None);
        // Selecting on an empty list returns None, doesn't panic.
        assert_eq!(st.selected_hostname(), None);
    }

    #[test]
    fn picker_height_floor_at_eight_rows() {
        // N + 6 clamped to (8, 18). Empty / tiny lists clamp to 8 so
        // the chrome still has room to breathe.
        assert_eq!(picker_height(0), 8);
        assert_eq!(picker_height(1), 8);
        assert_eq!(picker_height(2), 8);
    }

    #[test]
    fn picker_height_scales_with_row_count() {
        // N + 6 past the floor: 3 -> 9, 5 -> 11, 10 -> 16.
        assert_eq!(picker_height(3), 9);
        assert_eq!(picker_height(5), 11);
        assert_eq!(picker_height(10), 16);
        assert_eq!(picker_height(12), 18);
    }

    #[test]
    fn picker_height_caps_at_eighteen() {
        // Large fleets cap at 18 so we never eat the operator's
        // scrollback.
        assert_eq!(picker_height(50), 18);
        assert_eq!(picker_height(1_000), 18);
    }

    #[test]
    fn picker_row_cells_uses_filtered_list_index() {
        let row = isen("edge-1", Some("ops@10.0.0.1"), None);
        let cells = picker_row_cells(3, &row);
        // Index column carries the filtered-list index, not the
        // source-list index. The operator's mental model is "the
        // number you see is the number you type".
        assert_eq!(cells[0], "3");
        assert_eq!(cells[1], "edge-1");
        assert_eq!(cells[2], "ops@10.0.0.1");
        assert_eq!(cells[3], "-");
    }

    #[test]
    fn picker_table_renders_selected_row_with_highlight() {
        use ratatui::style::Color;
        let rows = vec![
            isen("lausanne", Some("dirdmaster@10.17.0.125"), None),
            isen("edge-fra", Some("dirdmaster@10.0.0.42"), None),
        ];
        let table = picker_table(&rows);
        let lines = render_to_lines(&table, 80, Some(1));
        // Expected line layout: top border + header + header rule +
        // row 0 + row 1 + bottom border. Selected (row 1) is at idx 4.
        let selected = &lines[4];
        assert!(
            selected
                .spans
                .iter()
                .all(|s| s.style.bg == Some(Color::Cyan)),
            "every span on the selected row carries the highlight bg"
        );
        // Sanity: the un-selected row keeps its native styling, no
        // cyan background bleed.
        let other = &lines[3];
        assert!(
            other.spans.iter().all(|s| s.style.bg != Some(Color::Cyan)),
            "non-selected row stays un-highlighted"
        );
    }

    #[test]
    fn draw_places_filter_at_the_bottom_with_query_visible() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let rows = vec![
            isen("lausanne", Some("dirdmaster@10.17.0.125"), None),
            isen("edge-fra", Some("dirdmaster@10.0.0.42"), None),
        ];
        let mut st = PickerState::new(rows);
        st.filter.push_str("lau");
        st.refresh();
        terminal.draw(|f| draw(f, &mut st)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // The bottom row of the viewport carries the filter prompt
        // and the live query text. fzf-style: query lives below the
        // table, never above.
        let last_line: String = (0..80).map(|x| buf[(x, 11)].symbol()).collect();
        assert!(
            last_line.starts_with("> lau"),
            "filter line must sit at the bottom: {last_line:?}"
        );
        // Top of the viewport carries the boxed table's top border.
        let first_line: String = (0..80).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(
            first_line.starts_with('╭'),
            "table top border at the top of the viewport: {first_line:?}"
        );
    }

    #[test]
    fn picker_table_with_empty_visible_shows_only_chrome() {
        let table = picker_table(&[]);
        let lines = render_to_lines(&table, 80, None);
        // Top border, header, header rule, bottom border = 4 lines.
        assert_eq!(lines.len(), 4);
        assert!(lines[0].to_string().starts_with('╭'));
        assert!(lines[1].to_string().contains("NAME"));
        assert!(lines[1].to_string().contains("DIAL TARGET"));
        assert!(lines[1].to_string().contains("LAST SEEN"));
        assert!(lines[2].to_string().starts_with('├'));
        assert!(lines[3].to_string().starts_with('╰'));
    }

    #[test]
    fn relative_time_handles_known_shapes() {
        assert_eq!(relative_time(None), "-");
        assert_eq!(relative_time(Some("not-a-timestamp")), "not-a-timestamp");
        let ten_min_ago = (chrono::Utc::now() - chrono::Duration::minutes(10))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        assert_eq!(relative_time(Some(&ten_min_ago)), "10m ago");
        let three_hours_ago = (chrono::Utc::now() - chrono::Duration::hours(3))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        assert_eq!(relative_time(Some(&three_hours_ago)), "3h ago");
    }

    #[test]
    fn picker_state_clear_filter_via_ctrl_u_restores_all() {
        let rows = vec![isen("a", None, None), isen("b", None, None)];
        let mut st = PickerState::new(rows);
        st.filter.push('a');
        st.refresh();
        assert_eq!(st.visible.len(), 1);
        st.filter.clear();
        st.refresh();
        assert_eq!(st.visible.len(), 2);
        assert_eq!(st.selected, Some(0));
    }
}
