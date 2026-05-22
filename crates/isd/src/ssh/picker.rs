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
//! fuzzy-matched via `nucleo-matcher`. The earlier fzf + inquire
//! dance was removed because fzf opens `/dev/tty` with raw ioctls that
//! bypass capture tools (cheese, asciinema's pty wrap). The native
//! picker keeps all output inside crossterm so screen capture works
//! and the visual language matches `isd configure`. Inline rendering
//! reserves a small viewport at the bottom of the current shell and
//! clears it silently on exit, so the surrounding shell flow stays
//! visible (gh/gum aesthetic).

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
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};

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

/// Render a row as the picker row's display Line. Hostname bold, dial
/// target cyan when set, last-seen dim and relative ("2m ago"). Class
/// tag dropped: single-fleet operators do not need the disambiguation,
/// and the trusted-vs-isengard distinction is informational at best.
fn row_line(row: &PickerRow) -> Line<'static> {
    let last = relative_time(row.last_seen.as_deref());
    let dial = row.dial_target.clone().unwrap_or_else(|| "(unset)".into());
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            row.hostname.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(dial, Style::default().fg(Color::Cyan)),
        Span::raw("  "),
        Span::styled(last, Style::default().fg(Color::DarkGray)),
    ])
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
/// Layout budget: filter (1) + list (>=1) = 2 min. Cap at 12 so a long
/// fleet does not eat the operator's scrollback. Floor at 3 so the
/// picker still has a usable list area with a tiny fleet.
pub fn picker_height(n_rows: usize) -> u16 {
    let raw = (n_rows as u16).saturating_add(1);
    raw.clamp(3, 12)
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
    list_state: ListState,
}

impl PickerState {
    /// Seed picker state from the merged source list. Selects the
    /// first row (or `None` when the list is empty).
    fn new(rows: Vec<PickerRow>) -> Self {
        let mut list_state = ListState::default();
        if !rows.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            visible: rows.clone(),
            all: rows,
            filter: String::new(),
            list_state,
        }
    }

    /// Recompute `visible` from `all` + `filter` and reset the
    /// selection cursor to the top match. Called after every filter
    /// edit so the first row is always highlighted; Enter on an
    /// untouched filter picks the first row in source order.
    fn refresh(&mut self) {
        self.visible = score_rows_by_filter(&self.all, &self.filter);
        if self.visible.is_empty() {
            self.list_state.select(None);
        } else {
            self.list_state.select(Some(0));
        }
    }

    /// Currently highlighted hostname (`None` when no row matches).
    fn selected_hostname(&self) -> Option<String> {
        let idx = self.list_state.selected()?;
        self.visible.get(idx).map(|r| r.hostname.clone())
    }

    /// Move the cursor by `delta` rows, clamped to the visible range.
    fn move_selection(&mut self, delta: i32) {
        let len = self.visible.len();
        if len == 0 {
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0) as i32;
        let next = (cur + delta).rem_euclid(len as i32) as usize;
        self.list_state.select(Some(next));
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

/// Render the picker frame. Minimalist by design: a single `>` filter
/// line on top, ranked rows below, nothing else. No bordered chrome, no
/// title, no keybind footer. Esc / Enter are universal; operators know
/// them. Selection is highlighted via background color, not a leading
/// arrow.
fn draw(f: &mut ratatui::Frame<'_>, state: &mut PickerState) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // filter input
            Constraint::Min(1),    // host list
        ])
        .split(area);

    let filter_line = Paragraph::new(Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Yellow)),
        Span::raw(state.filter.clone()),
    ]));
    f.render_widget(filter_line, chunks[0]);

    let items: Vec<ListItem> = state
        .visible
        .iter()
        .map(|r| ListItem::new(row_line(r)))
        .collect();
    let list = List::new(items).highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    f.render_stateful_widget(list, chunks[1], &mut state.list_state);
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
        assert_eq!(st.list_state.selected(), Some(0));
        st.move_selection(1);
        assert_eq!(st.list_state.selected(), Some(1));
        st.move_selection(1);
        assert_eq!(st.list_state.selected(), Some(0));
        st.move_selection(-1);
        assert_eq!(st.list_state.selected(), Some(1));
    }

    #[test]
    fn picker_state_typing_filter_refreshes_visible() {
        let rows = vec![isen("lausanne", None, None), isen("edge-fra", None, None)];
        let mut st = PickerState::new(rows);
        st.filter.push_str("lau");
        st.refresh();
        assert_eq!(st.visible.len(), 1);
        assert_eq!(st.visible[0].hostname, "lausanne");
        assert_eq!(st.list_state.selected(), Some(0));
    }

    #[test]
    fn picker_state_no_match_clears_selection() {
        let rows = vec![isen("lausanne", None, None)];
        let mut st = PickerState::new(rows);
        st.filter.push_str("x9z");
        st.refresh();
        assert!(st.visible.is_empty());
        assert_eq!(st.list_state.selected(), None);
        // Selecting on an empty list returns None, doesn't panic.
        assert_eq!(st.selected_hostname(), None);
    }

    #[test]
    fn picker_height_floor_at_three_rows() {
        assert_eq!(picker_height(0), 3);
        assert_eq!(picker_height(1), 3);
        assert_eq!(picker_height(2), 3);
    }

    #[test]
    fn picker_height_scales_with_row_count() {
        // n+1 once we're past the floor: 5 rows + 1 = 6.
        assert_eq!(picker_height(5), 6);
        assert_eq!(picker_height(10), 11);
    }

    #[test]
    fn picker_height_caps_at_twelve() {
        assert_eq!(picker_height(50), 12);
        assert_eq!(picker_height(1_000), 12);
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
        assert_eq!(st.list_state.selected(), Some(0));
    }
}
