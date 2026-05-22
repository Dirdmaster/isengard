//! Full-screen ratatui TUI for `isd configure`.
//!
//! Replaces the inline inquire two-level menu from PR #245 with a real
//! terminal application: alternate screen, raw mode, two-pane layout
//! (categories on the left, keys on the right), modal editor on Enter.
//!
//! The TUI owns the terminal for the duration of the session and
//! restores it cleanly on every exit path:
//!
//! * Normal exit (`q`, `Esc` from the main view) drops a [`TermGuard`]
//!   that disables raw mode and leaves the alternate screen.
//! * Panics route through a panic hook installed in [`run`] that
//!   performs the same restore before delegating to the previous hook.
//! * Errors bubble out as `anyhow::Result`; the guard's `Drop` still
//!   runs because it's stack-allocated.
//!
//! HTTP calls are synchronous against the event loop: a "Loading..."
//! status flag is set, the future is awaited inline, then the loop
//! resumes. Keyboard input during a load is discarded.
//!
//! Tests at the bottom use `ratatui::backend::TestBackend` to render
//! the two-pane and modal views without spawning a real terminal.

use std::io::{Stdout, stdout};

use anyhow::{Context as _, Result};
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt as _;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::Value;

use super::{
    KeyType, ListRow, SchemaEntry, fetch_list, fetch_schema, group_by_category, json_to_display,
    key_label, key_type_label, menu_value_display, put_value,
};
use crate::session::Session;

/// Run the configure TUI against `session` / `base`. Returns the number
/// of keys the operator successfully updated during the session.
///
/// # Errors
///
/// Returns any HTTP error from the initial schema/list fetch (before
/// the alternate screen is entered) and any unrecoverable rendering
/// error. PUT errors during the session render inline inside the modal
/// and never escape.
pub(crate) async fn run(session: &Session, base: &str) -> Result<usize> {
    // Fetch once up-front: if the controller is unreachable, surface
    // the error on the main screen instead of flashing an empty TUI.
    let schema = fetch_schema(session, base)
        .await
        .context("fetching schema for configure TUI")?;
    let list = fetch_list(session, base, false).await.unwrap_or_default();

    // Install panic hook BEFORE entering the alternate screen so a
    // panic between EnterAlternateScreen and the guard's construction
    // still restores the terminal.
    install_panic_hook();

    enable_raw_mode().context("enabling raw mode")?;
    let mut stdout_handle = stdout();
    execute!(stdout_handle, EnterAlternateScreen, EnableMouseCapture)
        .context("entering alternate screen")?;
    let backend = CrosstermBackend::new(stdout_handle);
    let mut terminal = Terminal::new(backend).context("creating terminal backend")?;
    let _guard = TermGuard;

    let mut app = AppState::new(schema, list);
    let outcome = event_loop(&mut terminal, &mut app, session, base).await;

    // Explicit cleanup. _guard's Drop is a safety net for panic paths.
    drop(terminal);
    outcome
}

/// RAII guard that disables raw mode and leaves the alternate screen
/// on drop. Stack-allocated so unwinding through the TUI still
/// restores the terminal.
struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let mut out = stdout();
        let _ = execute!(out, DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// Install a panic hook that restores the terminal before delegating
/// to the previous hook. Called once per [`run`]; subsequent calls
/// chain so multiple TUIs in one process still restore correctly.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut out = stdout();
        let _ = execute!(out, DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
        previous(info);
    }));
}

/// Top-level TUI state.
struct AppState {
    /// All schema entries (unchanged after construction).
    schema: Vec<SchemaEntry>,
    /// Latest snapshot of stored values. Refreshed after every PUT.
    list: Vec<ListRow>,
    /// Category names in alphabetical order. Indexes the left pane.
    categories: Vec<String>,
    /// State for the categories list.
    cat_state: ListState,
    /// State for the keys list.
    key_state: ListState,
    /// Which pane has focus.
    focus: Focus,
    /// Modal state, when open.
    modal: Option<Modal>,
    /// Help overlay shown via `?`.
    help_open: bool,
    /// Latest user-facing status line.
    status: String,
    /// Number of keys updated this session.
    updated: usize,
    /// True while an HTTP call is in flight (block input).
    busy: bool,
}

impl AppState {
    /// Seed the TUI state from a fresh schema + list snapshot.
    fn new(schema: Vec<SchemaEntry>, list: Vec<ListRow>) -> Self {
        let groups = group_by_category(&schema);
        let categories: Vec<String> = groups.keys().map(|s| s.to_string()).collect();
        let mut cat_state = ListState::default();
        if !categories.is_empty() {
            cat_state.select(Some(0));
        }
        let mut key_state = ListState::default();
        key_state.select(Some(0));
        Self {
            schema,
            list,
            categories,
            cat_state,
            key_state,
            focus: Focus::Categories,
            modal: None,
            help_open: false,
            status: String::new(),
            updated: 0,
            busy: false,
        }
    }

    /// Schema entries for the currently selected category, in
    /// declaration order. Empty when no category is selected (which
    /// only happens against a controller that has zero settable keys).
    fn current_entries(&self) -> Vec<SchemaEntry> {
        let Some(idx) = self.cat_state.selected() else {
            return Vec::new();
        };
        let Some(name) = self.categories.get(idx) else {
            return Vec::new();
        };
        let groups = group_by_category(&self.schema);
        groups
            .get(name.as_str())
            .map(|v| v.iter().map(|e| (*e).clone()).collect())
            .unwrap_or_default()
    }

    /// Currently highlighted key entry, if any.
    fn current_entry(&self) -> Option<SchemaEntry> {
        let entries = self.current_entries();
        let idx = self.key_state.selected().unwrap_or(0);
        entries.get(idx).cloned()
    }

    /// Find a [`ListRow`] for `key` in the latest snapshot.
    fn row_for(&self, key: &str) -> Option<&ListRow> {
        self.list.iter().find(|r| r.key == key)
    }

    /// Clamp the key selection so the right pane never points past the
    /// end of the current category's entry list.
    fn clamp_key_selection(&mut self) {
        let len = self.current_entries().len();
        if len == 0 {
            self.key_state.select(None);
            return;
        }
        let i = self.key_state.selected().unwrap_or(0).min(len - 1);
        self.key_state.select(Some(i));
    }
}

/// Which pane the keyboard targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// Left pane: categories list.
    Categories,
    /// Right pane: keys inside the highlighted category.
    Keys,
}

/// Modal editor state.
struct Modal {
    /// Schema entry being edited.
    entry: SchemaEntry,
    /// Text buffer (used for free-text / int input).
    buffer: String,
    /// Cursor index into `buffer` (in chars, not bytes).
    cursor: usize,
    /// Choice index (used for [`Mode::Choices`] / [`Mode::Bool`]).
    choice_idx: usize,
    /// Choice list state.
    choice_state: ListState,
    /// Mode-specific input.
    mode: Mode,
    /// Latest server-side error, rendered red at the bottom.
    error: Option<String>,
}

impl Modal {
    /// Seed a modal for `entry`, biasing the input toward the operator's
    /// current value when one is set.
    fn new(entry: SchemaEntry, current: Option<&ListRow>) -> Self {
        let mode = if entry.choices.is_some() {
            Mode::Choices
        } else {
            match entry.ty {
                KeyType::Bool => Mode::Bool,
                KeyType::Secret => Mode::Secret,
                KeyType::Int => Mode::Int,
                KeyType::String => Mode::String,
            }
        };
        // Seed the buffer with the current value when sensible so the
        // operator sees what they're replacing.
        let buffer = match (&mode, current.map(|r| &r.value)) {
            (Mode::Choices | Mode::Bool | Mode::Secret, _) => String::new(),
            (_, Some(Value::String(s))) => s.clone(),
            (_, Some(Value::Number(n))) => n.to_string(),
            (_, _) => String::new(),
        };
        let cursor = buffer.chars().count();
        // Seed the choice index to the current value when it matches a
        // declared choice; same for bool (true -> 0, false -> 1).
        let choice_idx = match (&mode, &entry.choices, current.map(|r| &r.value)) {
            (Mode::Choices, Some(choices), Some(Value::String(v))) => {
                choices.iter().position(|c| &c.value == v).unwrap_or(0)
            }
            (Mode::Bool, _, Some(Value::Bool(true))) => 0,
            (Mode::Bool, _, Some(Value::Bool(false))) => 1,
            _ => 0,
        };
        let mut choice_state = ListState::default();
        choice_state.select(Some(choice_idx));
        Self {
            entry,
            buffer,
            cursor,
            choice_idx,
            choice_state,
            mode,
            error: None,
        }
    }
}

/// Input mode of a modal editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Free-text string input.
    String,
    /// Masked text input (secret).
    Secret,
    /// Numeric text input.
    Int,
    /// Two-row Yes/No select.
    Bool,
    /// Fixed select from [`SchemaEntry::choices`].
    Choices,
}

/// Drive the event loop until the operator quits. Returns the count
/// of successful writes.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppState,
    session: &Session,
    base: &str,
) -> Result<usize> {
    let mut events = EventStream::new();
    loop {
        terminal.draw(|f| draw(f, app))?;
        let evt = match events.next().await {
            Some(Ok(e)) => e,
            Some(Err(e)) => {
                app.status = format!("input error: {e}");
                continue;
            }
            None => break,
        };
        let Event::Key(key) = evt else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        // Discard input during a network call.
        if app.busy {
            continue;
        }
        if app.help_open {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter) {
                app.help_open = false;
            }
            continue;
        }
        if app.modal.is_some() {
            match handle_modal_key(app, session, base, key).await? {
                ModalControl::Stay => {}
                ModalControl::Close => app.modal = None,
            }
            continue;
        }
        match handle_main_key(app, key) {
            MainControl::Quit => break,
            MainControl::Continue => {}
            MainControl::OpenModal => {
                if let Some(entry) = app.current_entry() {
                    let row = app.row_for(&entry.key).cloned();
                    app.modal = Some(Modal::new(entry, row.as_ref()));
                }
            }
            MainControl::Refresh => {
                refresh_list(app, session, base).await;
            }
        }
    }
    Ok(app.updated)
}

/// Outcome of a key press in the main view.
enum MainControl {
    /// Stay in the main view; nothing to do.
    Continue,
    /// Open the modal for the currently highlighted key.
    OpenModal,
    /// Refresh the list snapshot (after explicit operator request).
    Refresh,
    /// Quit the TUI.
    Quit,
}

/// Outcome of a key press inside the modal.
enum ModalControl {
    /// Keep the modal open.
    Stay,
    /// Close the modal.
    Close,
}

/// Handle one key press in the main view.
fn handle_main_key(app: &mut AppState, key: KeyEvent) -> MainControl {
    // Global Ctrl-C exits.
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return MainControl::Quit;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => MainControl::Quit,
        KeyCode::Char('?') => {
            app.help_open = true;
            MainControl::Continue
        }
        KeyCode::Char('r') | KeyCode::Char('R') => MainControl::Refresh,
        KeyCode::Tab | KeyCode::BackTab => {
            app.focus = match app.focus {
                Focus::Categories => Focus::Keys,
                Focus::Keys => Focus::Categories,
            };
            MainControl::Continue
        }
        KeyCode::Right => {
            app.focus = Focus::Keys;
            MainControl::Continue
        }
        KeyCode::Left => {
            app.focus = Focus::Categories;
            MainControl::Continue
        }
        KeyCode::Up => {
            move_selection(app, -1);
            MainControl::Continue
        }
        KeyCode::Down => {
            move_selection(app, 1);
            MainControl::Continue
        }
        KeyCode::Enter => {
            // Enter in left pane shifts focus to keys; Enter in keys
            // opens the modal.
            match app.focus {
                Focus::Categories => {
                    app.focus = Focus::Keys;
                    MainControl::Continue
                }
                Focus::Keys => MainControl::OpenModal,
            }
        }
        _ => MainControl::Continue,
    }
}

/// Step the highlighted row in the focused pane.
fn move_selection(app: &mut AppState, delta: i32) {
    match app.focus {
        Focus::Categories => {
            let len = app.categories.len();
            if len == 0 {
                return;
            }
            let i = app.cat_state.selected().unwrap_or(0) as i32;
            let next = (i + delta).rem_euclid(len as i32) as usize;
            app.cat_state.select(Some(next));
            // When categories change, reset key selection to the top.
            app.key_state.select(Some(0));
            app.clamp_key_selection();
        }
        Focus::Keys => {
            let len = app.current_entries().len();
            if len == 0 {
                return;
            }
            let i = app.key_state.selected().unwrap_or(0) as i32;
            let next = (i + delta).rem_euclid(len as i32) as usize;
            app.key_state.select(Some(next));
        }
    }
}

/// Refresh `app.list` from the controller. Renders a busy flag while
/// the call is in flight; restores the previous status on success.
async fn refresh_list(app: &mut AppState, session: &Session, base: &str) {
    app.busy = true;
    app.status = "Loading...".to_string();
    match fetch_list(session, base, false).await {
        Ok(rows) => {
            app.list = rows;
            app.status.clear();
        }
        Err(e) => {
            app.status = format!("refresh failed: {e}");
        }
    }
    app.busy = false;
}

/// Handle one key press inside the modal.
async fn handle_modal_key(
    app: &mut AppState,
    session: &Session,
    base: &str,
    key: KeyEvent,
) -> Result<ModalControl> {
    let Some(modal) = app.modal.as_mut() else {
        return Ok(ModalControl::Close);
    };
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return Ok(ModalControl::Close);
    }
    match key.code {
        KeyCode::Esc => return Ok(ModalControl::Close),
        KeyCode::Enter => {
            let attempt = build_value(modal);
            let value = match attempt {
                Ok(v) => v,
                Err(msg) => {
                    modal.error = Some(msg);
                    return Ok(ModalControl::Stay);
                }
            };
            let entry_key = modal.entry.key.clone();
            let entry_ty = modal.entry.ty;
            app.busy = true;
            app.status = "Saving...".to_string();
            let result = put_value(session, base, &entry_key, value).await;
            app.busy = false;
            match result {
                Ok(()) => {
                    app.updated += 1;
                    app.status = if entry_ty == KeyType::Secret {
                        format!("Set {entry_key} (secret; value not echoed)")
                    } else {
                        format!("Set {entry_key}")
                    };
                    refresh_list(app, session, base).await;
                    return Ok(ModalControl::Close);
                }
                Err(e) => {
                    // Re-borrow the modal because the await above
                    // crossed the &mut boundary.
                    if let Some(m) = app.modal.as_mut() {
                        m.error = Some(format!("{e}"));
                    }
                    return Ok(ModalControl::Stay);
                }
            }
        }
        KeyCode::Up => {
            modal_step_choice(modal, -1);
        }
        KeyCode::Down => {
            modal_step_choice(modal, 1);
        }
        KeyCode::Backspace
            if matches!(modal.mode, Mode::String | Mode::Secret | Mode::Int)
                && modal.cursor > 0
                && !modal.buffer.is_empty() =>
        {
            let new_cursor = modal.cursor - 1;
            let mut chars: Vec<char> = modal.buffer.chars().collect();
            if new_cursor < chars.len() {
                chars.remove(new_cursor);
            }
            modal.buffer = chars.into_iter().collect();
            modal.cursor = new_cursor;
        }
        KeyCode::Left
            if matches!(modal.mode, Mode::String | Mode::Secret | Mode::Int)
                && modal.cursor > 0 =>
        {
            modal.cursor -= 1;
        }
        KeyCode::Right
            if matches!(modal.mode, Mode::String | Mode::Secret | Mode::Int)
                && modal.cursor < modal.buffer.chars().count() =>
        {
            modal.cursor += 1;
        }
        KeyCode::Home if matches!(modal.mode, Mode::String | Mode::Secret | Mode::Int) => {
            modal.cursor = 0;
        }
        KeyCode::End if matches!(modal.mode, Mode::String | Mode::Secret | Mode::Int) => {
            modal.cursor = modal.buffer.chars().count();
        }
        KeyCode::Char(ch) => {
            // Plain ASCII typing only for free-text modes. For Int,
            // accept digits and a leading sign character.
            match modal.mode {
                Mode::String | Mode::Secret => {
                    insert_char(modal, ch);
                }
                Mode::Int => {
                    if ch.is_ascii_digit() || (ch == '-' && modal.cursor == 0) {
                        insert_char(modal, ch);
                    }
                }
                Mode::Bool | Mode::Choices => {}
            }
        }
        _ => {}
    }
    Ok(ModalControl::Stay)
}

/// Insert `ch` at the current cursor position and advance the cursor.
fn insert_char(modal: &mut Modal, ch: char) {
    let mut chars: Vec<char> = modal.buffer.chars().collect();
    let i = modal.cursor.min(chars.len());
    chars.insert(i, ch);
    modal.buffer = chars.into_iter().collect();
    modal.cursor = i + 1;
}

/// Step the highlighted choice or bool option.
fn modal_step_choice(modal: &mut Modal, delta: i32) {
    let len = match &modal.mode {
        Mode::Choices => modal.entry.choices.as_ref().map(|c| c.len()).unwrap_or(0),
        Mode::Bool => 2,
        _ => 0,
    };
    if len == 0 {
        return;
    }
    let i = modal.choice_idx as i32;
    let next = (i + delta).rem_euclid(len as i32) as usize;
    modal.choice_idx = next;
    modal.choice_state.select(Some(next));
}

/// Encode the current modal buffer / selection into a JSON value.
/// Returns an error message rendered red inside the modal on failure.
fn build_value(modal: &Modal) -> Result<Value, String> {
    match &modal.mode {
        Mode::Choices => {
            let choices = modal
                .entry
                .choices
                .as_ref()
                .ok_or_else(|| "no choices declared".to_string())?;
            let c = choices
                .get(modal.choice_idx)
                .ok_or_else(|| "choice out of range".to_string())?;
            Ok(Value::String(c.value.clone()))
        }
        Mode::Bool => Ok(Value::Bool(modal.choice_idx == 0)),
        Mode::String | Mode::Secret => {
            if modal.buffer.is_empty() {
                Err("value cannot be empty".to_string())
            } else {
                Ok(Value::String(modal.buffer.clone()))
            }
        }
        Mode::Int => {
            let n: i64 = modal
                .buffer
                .trim()
                .parse()
                .map_err(|_| format!("expected integer, got {:?}", modal.buffer))?;
            Ok(Value::from(n))
        }
    }
}

// -- rendering ---------------------------------------------------------

/// Draw one frame.
fn draw(f: &mut ratatui::Frame<'_>, app: &AppState) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(outer[0]);

    draw_categories(f, app, panes[0]);
    draw_keys(f, app, panes[1]);
    draw_status_bar(f, app, outer[1]);

    if app.modal.is_some() {
        draw_modal(f, app);
    }
    if app.help_open {
        draw_help(f);
    }
}

/// Left pane: categories with set/total counts.
fn draw_categories(f: &mut ratatui::Frame<'_>, app: &AppState, area: Rect) {
    let groups = group_by_category(&app.schema);
    let items: Vec<ListItem> = app
        .categories
        .iter()
        .map(|cat| {
            let entries = groups
                .get(cat.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            ListItem::new(category_summary_label(cat, entries, &app.list))
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            " Categories ",
            block_title_style(app.focus == Focus::Categories),
        ));
    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    let mut state = app.cat_state.clone();
    f.render_stateful_widget(list, area, &mut state);
}

/// Right pane: keys inside the current category.
fn draw_keys(f: &mut ratatui::Frame<'_>, app: &AppState, area: Rect) {
    let entries = app.current_entries();
    let max_label = entries
        .iter()
        .map(|e| key_label(e).chars().count())
        .max()
        .unwrap_or(0);
    let items: Vec<ListItem> = entries
        .iter()
        .map(|e| {
            let label = key_label(e);
            let padded = format!("{label:<max_label$}");
            let ty = key_type_label(e.ty);
            let current = app.row_for(&e.key);
            let value = menu_value_display(e, current);
            ListItem::new(format!("{padded}  {ty:<6}  {value}"))
        })
        .collect();
    let title = match app.categories.get(app.cat_state.selected().unwrap_or(0)) {
        Some(name) => format!(" Keys: {name} "),
        None => " Keys ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            title,
            block_title_style(app.focus == Focus::Keys),
        ));
    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    let mut state = app.key_state.clone();
    f.render_stateful_widget(list, area, &mut state);
}

/// Bottom status bar with key bindings.
fn draw_status_bar(f: &mut ratatui::Frame<'_>, app: &AppState, area: Rect) {
    let bindings = "up/down nav | Enter edit | Tab switch pane | r refresh | q quit | ? help";
    let text = if app.status.is_empty() {
        bindings.to_string()
    } else {
        format!("{}  |  {bindings}", app.status)
    };
    let para = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    f.render_widget(para, area);
}

/// Title styling: bright when the pane has focus.
fn block_title_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// Render the modal editor on top of the main view.
fn draw_modal(f: &mut ratatui::Frame<'_>, app: &AppState) {
    let Some(modal) = app.modal.as_ref() else {
        return;
    };
    let area = centered_rect(60, 50, f.area());
    f.render_widget(Clear, area);

    let category = app
        .categories
        .get(app.cat_state.selected().unwrap_or(0))
        .map(|s| s.as_str())
        .unwrap_or("");
    let title = format!(" {category} > {} ", key_label(&modal.entry));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    f.render_widget(block.clone(), area);

    let inner = block.inner(area);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // doc
            Constraint::Length(2), // current state
            Constraint::Min(1),    // input
            Constraint::Length(1), // error
            Constraint::Length(1), // buttons
        ])
        .split(inner);

    let doc = Paragraph::new(modal.entry.doc.as_str())
        .wrap(Wrap { trim: true })
        .style(Style::default().fg(Color::Gray));
    f.render_widget(doc, layout[0]);

    let current = app.row_for(&modal.entry.key);
    let current_text = format!(
        "Current: {}  ({})",
        format_value(&modal.entry, current),
        current.map(|r| r.source.as_str()).unwrap_or("unset")
    );
    let cur = Paragraph::new(current_text).style(Style::default().fg(Color::DarkGray));
    f.render_widget(cur, layout[1]);

    draw_modal_input(f, modal, layout[2]);

    if let Some(err) = &modal.error {
        let p = Paragraph::new(err.as_str()).style(Style::default().fg(Color::Red));
        f.render_widget(p, layout[3]);
    }

    let buttons = Paragraph::new("[Enter] Save  *  [Esc] Cancel")
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(buttons, layout[4]);
}

/// Render the input region of the modal (varies by [`Mode`]).
fn draw_modal_input(f: &mut ratatui::Frame<'_>, modal: &Modal, area: Rect) {
    match &modal.mode {
        Mode::Choices => {
            let choices = modal.entry.choices.as_ref();
            let items: Vec<ListItem> = choices
                .map(|cs| {
                    cs.iter()
                        .map(|c| ListItem::new(format!("{}  ->  {}", c.label, c.value)))
                        .collect()
                })
                .unwrap_or_default();
            let block = Block::default().borders(Borders::ALL).title(" Choices ");
            let list = List::new(items).block(block).highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
            let mut state = modal.choice_state.clone();
            f.render_stateful_widget(list, area, &mut state);
        }
        Mode::Bool => {
            let items: Vec<ListItem> = vec![ListItem::new("Yes"), ListItem::new("No")];
            let block = Block::default().borders(Borders::ALL).title(" Value ");
            let list = List::new(items).block(block).highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
            let mut state = modal.choice_state.clone();
            f.render_stateful_widget(list, area, &mut state);
        }
        Mode::String | Mode::Int => {
            let block = Block::default().borders(Borders::ALL).title(" Value ");
            let para = Paragraph::new(Text::from(Line::from(modal.buffer.as_str()))).block(block);
            f.render_widget(para, area);
            // Position the cursor.
            let inner = Block::default().borders(Borders::ALL).inner(area);
            let col = inner.x + modal.cursor as u16;
            let row = inner.y;
            f.set_cursor_position((col, row));
        }
        Mode::Secret => {
            let mask: String = "*".repeat(modal.buffer.chars().count());
            let block = Block::default()
                .borders(Borders::ALL)
                .title(" Secret value ");
            let para = Paragraph::new(Text::from(Line::from(mask.as_str()))).block(block);
            f.render_widget(para, area);
            let inner = Block::default().borders(Borders::ALL).inner(area);
            let col = inner.x + modal.cursor as u16;
            let row = inner.y;
            f.set_cursor_position((col, row));
        }
    }
}

/// Help overlay rendered when `?` is pressed.
fn draw_help(f: &mut ratatui::Frame<'_>) {
    let area = centered_rect(50, 40, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Help ");
    let body = vec![
        Line::from("up/down: navigate within the focused pane"),
        Line::from("Tab / Shift-Tab: switch pane"),
        Line::from("Enter: focus keys / open editor"),
        Line::from("r: refresh from controller"),
        Line::from("q / Esc: quit"),
        Line::from("Inside the modal:"),
        Line::from("  up/down: change choice"),
        Line::from("  Enter: save"),
        Line::from("  Esc: cancel"),
    ];
    let p = Paragraph::new(body).block(block).wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

/// Compute a centered rect with the given percentage sizes inside
/// `parent`.
fn centered_rect(percent_x: u16, percent_y: u16, parent: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(parent);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(v[1])[1]
}

// -- pure helpers (unit-testable) -------------------------------------

/// Render the left-pane row label for one category. Format:
/// `<name>  <set>/<total>`.
pub(crate) fn category_summary_label(
    name: &str,
    entries: &[&SchemaEntry],
    list: &[ListRow],
) -> String {
    let total = entries.len();
    let set = entries
        .iter()
        .filter(|e| {
            list.iter()
                .find(|r| r.key == e.key)
                .map(|r| r.source == "set")
                .unwrap_or(false)
        })
        .count();
    format!("{name}  {set}/{total}")
}

/// Render the current-value display for the modal header.
pub(crate) fn format_value(entry: &SchemaEntry, current: Option<&ListRow>) -> String {
    match (entry.ty, current) {
        (KeyType::Secret, Some(r)) if r.source == "set" => "<set>".to_string(),
        (KeyType::Secret, _) => "<unset>".to_string(),
        (_, Some(r)) => json_to_display(&r.value),
        (_, None) => match &entry.default {
            Some(v) => json_to_display(v),
            None => "<unset>".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    fn entries_fixture() -> Vec<SchemaEntry> {
        vec![
            SchemaEntry {
                key: "cloudflare.api_token".into(),
                display_name: "Cloudflare API token".into(),
                category: "Certificates".into(),
                ty: KeyType::Secret,
                default: None,
                doc: "CF token".into(),
                choices: None,
            },
            SchemaEntry {
                key: "acme.directory".into(),
                display_name: "ACME directory".into(),
                category: "Certificates".into(),
                ty: KeyType::String,
                default: Some(json!("https://acme-v02.api.letsencrypt.org/directory")),
                doc: "ACME dir".into(),
                choices: Some(vec![
                    super::super::Choice {
                        value: "https://acme-v02.api.letsencrypt.org/directory".into(),
                        label: "Let's Encrypt production".into(),
                    },
                    super::super::Choice {
                        value: "https://acme-staging-v02.api.letsencrypt.org/directory".into(),
                        label: "Let's Encrypt staging".into(),
                    },
                ]),
            },
            SchemaEntry {
                key: "ssh.max_ttl_seconds".into(),
                display_name: "Max cert TTL (seconds)".into(),
                category: "SSH bastion".into(),
                ty: KeyType::Int,
                default: Some(json!(2_592_000i64)),
                doc: "TTL ceiling".into(),
                choices: None,
            },
        ]
    }

    fn list_fixture() -> Vec<ListRow> {
        vec![ListRow {
            key: "cloudflare.api_token".into(),
            ty: KeyType::Secret,
            value: Value::String("<redacted>".into()),
            source: "set".into(),
        }]
    }

    #[test]
    fn category_summary_label_counts_set_keys() {
        let entries = entries_fixture();
        let list = list_fixture();
        let groups = group_by_category(&entries);
        let certs = groups.get("Certificates").unwrap().as_slice();
        let label = category_summary_label("Certificates", certs, &list);
        assert_eq!(label, "Certificates  1/2");
    }

    #[test]
    fn category_summary_label_zero_when_no_rows() {
        let entries = entries_fixture();
        let groups = group_by_category(&entries);
        let ssh = groups.get("SSH bastion").unwrap().as_slice();
        let label = category_summary_label("SSH bastion", ssh, &[]);
        assert_eq!(label, "SSH bastion  0/1");
    }

    #[test]
    fn format_value_redacts_set_secret() {
        let entries = entries_fixture();
        let secret = entries.iter().find(|e| e.ty == KeyType::Secret).unwrap();
        let row = ListRow {
            key: secret.key.clone(),
            ty: KeyType::Secret,
            value: Value::String("<redacted>".into()),
            source: "set".into(),
        };
        assert_eq!(format_value(secret, Some(&row)), "<set>");
    }

    #[test]
    fn format_value_falls_back_to_default_when_unset() {
        let entries = entries_fixture();
        let acme = entries.iter().find(|e| e.key == "acme.directory").unwrap();
        assert_eq!(
            format_value(acme, None),
            "https://acme-v02.api.letsencrypt.org/directory"
        );
    }

    #[test]
    fn format_value_unset_string_no_default() {
        let entry = SchemaEntry {
            key: "routing.default_zone".into(),
            display_name: "Default zone".into(),
            category: "Routing".into(),
            ty: KeyType::String,
            default: None,
            doc: "".into(),
            choices: None,
        };
        assert_eq!(format_value(&entry, None), "<unset>");
    }

    #[test]
    fn appstate_seeds_focus_on_categories() {
        let app = AppState::new(entries_fixture(), list_fixture());
        assert_eq!(app.focus, Focus::Categories);
        assert_eq!(app.categories, vec!["Certificates", "SSH bastion"]);
        assert_eq!(app.cat_state.selected(), Some(0));
    }

    #[test]
    fn modal_seeds_choice_index_to_current_value() {
        let entries = entries_fixture();
        let acme = entries.iter().find(|e| e.key == "acme.directory").unwrap();
        let row = ListRow {
            key: acme.key.clone(),
            ty: KeyType::String,
            value: Value::String("https://acme-staging-v02.api.letsencrypt.org/directory".into()),
            source: "set".into(),
        };
        let modal = Modal::new(acme.clone(), Some(&row));
        assert_eq!(modal.mode, Mode::Choices);
        assert_eq!(modal.choice_idx, 1);
    }

    #[test]
    fn modal_choices_save_emits_value_not_label() {
        let entries = entries_fixture();
        let acme = entries.iter().find(|e| e.key == "acme.directory").unwrap();
        let mut modal = Modal::new(acme.clone(), None);
        modal.choice_idx = 1;
        let v = build_value(&modal).unwrap();
        assert_eq!(
            v,
            Value::String("https://acme-staging-v02.api.letsencrypt.org/directory".into())
        );
    }

    #[test]
    fn modal_int_rejects_non_numeric_buffer() {
        let entries = entries_fixture();
        let ssh = entries
            .iter()
            .find(|e| e.key == "ssh.max_ttl_seconds")
            .unwrap();
        let mut modal = Modal::new(ssh.clone(), None);
        modal.mode = Mode::Int;
        modal.buffer = "abc".into();
        assert!(build_value(&modal).is_err());
    }

    #[test]
    fn modal_int_accepts_digits() {
        let entries = entries_fixture();
        let ssh = entries
            .iter()
            .find(|e| e.key == "ssh.max_ttl_seconds")
            .unwrap();
        let mut modal = Modal::new(ssh.clone(), None);
        modal.mode = Mode::Int;
        modal.buffer = "3600".into();
        let v = build_value(&modal).unwrap();
        assert_eq!(v, Value::from(3600i64));
    }

    #[test]
    fn modal_bool_maps_first_choice_to_true() {
        let entry = SchemaEntry {
            key: "feat.enabled".into(),
            display_name: "Enabled".into(),
            category: "Feature".into(),
            ty: KeyType::Bool,
            default: None,
            doc: "".into(),
            choices: None,
        };
        let mut modal = Modal::new(entry, None);
        modal.choice_idx = 0;
        assert_eq!(build_value(&modal).unwrap(), Value::Bool(true));
        modal.choice_idx = 1;
        assert_eq!(build_value(&modal).unwrap(), Value::Bool(false));
    }

    #[test]
    fn draw_main_renders_titles_and_categories() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = AppState::new(entries_fixture(), list_fixture());
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let rendered = buffer_to_string(&buf);
        assert!(rendered.contains("Categories"), "rendered: {rendered}");
        assert!(rendered.contains("Certificates"), "rendered: {rendered}");
        assert!(rendered.contains("SSH bastion"), "rendered: {rendered}");
        // The right pane should show the current category's keys.
        assert!(
            rendered.contains("Cloudflare API token"),
            "rendered: {rendered}"
        );
        // Secret-set row redacts as <set>.
        assert!(rendered.contains("<set>"), "rendered: {rendered}");
        // Status bar bindings.
        assert!(rendered.contains("Enter edit"), "rendered: {rendered}");
        assert!(rendered.contains("? help"), "rendered: {rendered}");
    }

    #[test]
    fn draw_modal_renders_choices_for_acme_directory() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let entries = entries_fixture();
        let acme = entries
            .iter()
            .find(|e| e.key == "acme.directory")
            .unwrap()
            .clone();
        let mut app = AppState::new(entries.clone(), list_fixture());
        // Force the highlighted key to acme.directory.
        let groups = group_by_category(&app.schema);
        let certs = groups.get("Certificates").unwrap();
        let idx = certs
            .iter()
            .position(|e| e.key == "acme.directory")
            .unwrap();
        app.key_state.select(Some(idx));
        app.modal = Some(Modal::new(acme, None));
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let rendered = buffer_to_string(&buf);
        // Modal title shows the category > display name.
        assert!(
            rendered.contains("Certificates > ACME directory"),
            "rendered: {rendered}"
        );
        // Both Let's Encrypt rows visible.
        assert!(
            rendered.contains("Let's Encrypt production"),
            "rendered: {rendered}"
        );
        assert!(
            rendered.contains("Let's Encrypt staging"),
            "rendered: {rendered}"
        );
        // Current value: the default URL.
        assert!(
            rendered.contains("https://acme-v02.api.letsencrypt.org/directory"),
            "rendered: {rendered}"
        );
        // Save / Cancel hint.
        assert!(rendered.contains("Save"), "rendered: {rendered}");
        assert!(rendered.contains("Cancel"), "rendered: {rendered}");
    }

    #[test]
    fn draw_modal_masks_secret_buffer() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let entries = entries_fixture();
        let secret = entries
            .iter()
            .find(|e| e.ty == KeyType::Secret)
            .unwrap()
            .clone();
        let mut app = AppState::new(entries.clone(), list_fixture());
        let mut modal = Modal::new(secret, None);
        modal.buffer = "supersecret".into();
        modal.cursor = modal.buffer.chars().count();
        app.modal = Some(modal);
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let rendered = buffer_to_string(&buf);
        // The literal secret must NEVER appear in the buffer.
        assert!(!rendered.contains("supersecret"), "rendered: {rendered}");
        // Stars stand in.
        assert!(rendered.contains("***********"), "rendered: {rendered}");
    }

    /// Dump a `Buffer` to a single string for substring assertions.
    fn buffer_to_string(buf: &ratatui::buffer::Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                out.push_str(cell.symbol());
            }
            out.push('\n');
        }
        out
    }
}
