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
    KeyType, ListRow, SchemaEntry, fetch_list, fetch_schema, fetch_zones_from_cloudflare,
    group_by_category, json_to_display, key_label, key_type_label, menu_value_display, put_value,
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
    modal: Option<ModalKind>,
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
pub(crate) struct Modal {
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
                // StringList editing goes through `ZonesModal`, not the
                // generic key editor. If we ever land here it means the
                // dispatcher misrouted; fall back to plain string input so
                // the operator can still escape with Esc.
                KeyType::StringList => Mode::String,
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

/// One of the two modal editors the TUI knows how to drive. Most schema
/// keys go through the generic single-value [`Modal`]; the
/// `routing.zones` StringList key gets its own list editor.
pub(crate) enum ModalKind {
    /// Generic single-value editor (string / secret / int / bool /
    /// choices).
    Key(Modal),
    /// Operator-managed DNS zones list editor with optional Cloudflare
    /// fetch.
    Zones(ZonesModal),
}

/// State for the `routing.zones` list editor.
///
/// The editor renders the current zones as a vertical list and exposes:
///
///  * `a` / `i`: open the inline input to add a new zone.
///  * `d` / `x`: remove the selected zone.
///  * `F`: trigger a Cloudflare fetch; the response opens a confirmation
///    overlay with `[Merge] [Replace] [Cancel]`.
///  * `Enter`: PUT the current list to `/api/v1/config/routing.zones`.
///  * `Esc`: cancel without writing.
pub(crate) struct ZonesModal {
    /// Working zone list. Mutates as the operator adds/removes/merges.
    zones: Vec<String>,
    /// Selection state for the list (highlights the row that `d` /`x`
    /// removes).
    list_state: ListState,
    /// Inline input mode flag. `Some` when the operator pressed `a` /
    /// `i`; carries the in-progress text.
    add_buffer: Option<String>,
    /// Cursor index into [`Self::add_buffer`] (chars).
    add_cursor: usize,
    /// Confirmation overlay state. `Some` after a successful Cloudflare
    /// fetch; carries the zones the server returned plus the highlight
    /// among `[Merge] [Replace] [Cancel]`.
    fetch_confirm: Option<FetchConfirm>,
    /// Latest server-side error, rendered red at the bottom.
    error: Option<String>,
}

/// Confirmation overlay shown after a successful Cloudflare fetch.
struct FetchConfirm {
    /// Zones the server returned. Read-only inside this overlay.
    fetched: Vec<String>,
    /// Currently highlighted action: 0 = Merge, 1 = Replace, 2 = Cancel.
    action_idx: usize,
}

impl ZonesModal {
    /// Build a fresh editor from the current `routing.zones` row.
    fn new(current: Option<&ListRow>) -> Self {
        let zones = zones_from_list_row(current);
        let mut list_state = ListState::default();
        if !zones.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            zones,
            list_state,
            add_buffer: None,
            add_cursor: 0,
            fetch_confirm: None,
            error: None,
        }
    }
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
                    app.modal = Some(open_modal_for(entry, &app.list, row.as_ref()));
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

/// Handle one key press inside whichever modal is currently open.
async fn handle_modal_key(
    app: &mut AppState,
    session: &Session,
    base: &str,
    key: KeyEvent,
) -> Result<ModalControl> {
    match app.modal.as_ref() {
        Some(ModalKind::Key(_)) => handle_key_modal_key(app, session, base, key).await,
        Some(ModalKind::Zones(_)) => handle_zones_modal_key(app, session, base, key).await,
        None => Ok(ModalControl::Close),
    }
}

/// Handle one key press inside the generic single-value editor.
async fn handle_key_modal_key(
    app: &mut AppState,
    session: &Session,
    base: &str,
    key: KeyEvent,
) -> Result<ModalControl> {
    let Some(ModalKind::Key(modal)) = app.modal.as_mut() else {
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
                    if let Some(ModalKind::Key(m)) = app.modal.as_mut() {
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

/// Handle one key press inside the zones list editor.
async fn handle_zones_modal_key(
    app: &mut AppState,
    session: &Session,
    base: &str,
    key: KeyEvent,
) -> Result<ModalControl> {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return Ok(ModalControl::Close);
    }
    // Fetch-confirmation overlay swallows all input until the operator
    // picks Merge / Replace / Cancel.
    if matches!(
        app.modal.as_ref(),
        Some(ModalKind::Zones(z)) if z.fetch_confirm.is_some()
    ) {
        return handle_zones_fetch_confirm_key(app, session, base, key).await;
    }
    // Inline add buffer is a stricter text-input mode; only Esc / Enter
    // / printable chars touch it.
    if matches!(
        app.modal.as_ref(),
        Some(ModalKind::Zones(z)) if z.add_buffer.is_some()
    ) {
        return Ok(handle_zones_add_buffer_key(app, key));
    }
    let cf_token_set = cloudflare_token_is_set(&app.list);
    let Some(ModalKind::Zones(zones_modal)) = app.modal.as_mut() else {
        return Ok(ModalControl::Close);
    };
    match key.code {
        KeyCode::Esc => return Ok(ModalControl::Close),
        KeyCode::Enter => {
            let payload: Vec<Value> = zones_modal
                .zones
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect();
            app.busy = true;
            app.status = "Saving zones...".to_string();
            let result = put_value(session, base, "routing.zones", Value::Array(payload)).await;
            app.busy = false;
            match result {
                Ok(()) => {
                    app.updated += 1;
                    app.status = "Set routing.zones".to_string();
                    refresh_list(app, session, base).await;
                    return Ok(ModalControl::Close);
                }
                Err(e) => {
                    if let Some(ModalKind::Zones(z)) = app.modal.as_mut() {
                        z.error = Some(format!("{e}"));
                    }
                    return Ok(ModalControl::Stay);
                }
            }
        }
        KeyCode::Up => {
            zones_step_selection(zones_modal, -1);
        }
        KeyCode::Down => {
            zones_step_selection(zones_modal, 1);
        }
        KeyCode::Char('a') | KeyCode::Char('i') => {
            zones_modal.add_buffer = Some(String::new());
            zones_modal.add_cursor = 0;
            zones_modal.error = None;
        }
        KeyCode::Char('d') | KeyCode::Char('x') => {
            if let Some(idx) = zones_modal.list_state.selected() {
                if idx < zones_modal.zones.len() {
                    zones_modal.zones.remove(idx);
                    let new_len = zones_modal.zones.len();
                    if new_len == 0 {
                        zones_modal.list_state.select(None);
                    } else {
                        zones_modal.list_state.select(Some(idx.min(new_len - 1)));
                    }
                }
            }
        }
        KeyCode::Char('F') => {
            if !cf_token_set {
                zones_modal.error =
                    Some("Set cloudflare.api_token first to enable fetch.".to_string());
                return Ok(ModalControl::Stay);
            }
            app.busy = true;
            app.status = "Fetching zones from Cloudflare...".to_string();
            let result = fetch_zones_from_cloudflare(session, base).await;
            app.busy = false;
            match result {
                Ok(fetched) => {
                    if let Some(ModalKind::Zones(z)) = app.modal.as_mut() {
                        z.fetch_confirm = Some(FetchConfirm {
                            fetched,
                            action_idx: 0,
                        });
                        z.error = None;
                    }
                    app.status.clear();
                }
                Err(e) => {
                    if let Some(ModalKind::Zones(z)) = app.modal.as_mut() {
                        z.error = Some(format!("fetch failed: {e}"));
                    }
                }
            }
        }
        _ => {}
    }
    Ok(ModalControl::Stay)
}

/// Handle a key press while the inline "add zone" text input is active.
///
/// Pulled into a sync helper because it does not need to await: keeps
/// the event loop's main async path readable.
fn handle_zones_add_buffer_key(app: &mut AppState, key: KeyEvent) -> ModalControl {
    let Some(ModalKind::Zones(z)) = app.modal.as_mut() else {
        return ModalControl::Close;
    };
    let Some(buf) = z.add_buffer.as_mut() else {
        return ModalControl::Stay;
    };
    match key.code {
        KeyCode::Esc => {
            z.add_buffer = None;
            z.add_cursor = 0;
        }
        KeyCode::Enter => {
            let candidate = buf.trim().to_string();
            if !candidate.is_empty() && !z.zones.iter().any(|s| s == &candidate) {
                z.zones.push(candidate);
                // Highlight the newly added row.
                z.list_state.select(Some(z.zones.len() - 1));
            }
            z.add_buffer = None;
            z.add_cursor = 0;
        }
        KeyCode::Backspace if z.add_cursor > 0 => {
            let mut chars: Vec<char> = buf.chars().collect();
            let i = z.add_cursor - 1;
            if i < chars.len() {
                chars.remove(i);
            }
            *buf = chars.into_iter().collect();
            z.add_cursor -= 1;
        }
        KeyCode::Left if z.add_cursor > 0 => {
            z.add_cursor -= 1;
        }
        KeyCode::Right if z.add_cursor < buf.chars().count() => {
            z.add_cursor += 1;
        }
        KeyCode::Home => z.add_cursor = 0,
        KeyCode::End => z.add_cursor = buf.chars().count(),
        // Drop control modifiers; accept printable ASCII + common
        // hostname characters (letters, digits, dot, dash). The
        // controller validates the final value, this is just a UX
        // filter so a stray `Ctrl+R` does not corrupt the buffer.
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            let mut chars: Vec<char> = buf.chars().collect();
            let i = z.add_cursor.min(chars.len());
            chars.insert(i, ch);
            *buf = chars.into_iter().collect();
            z.add_cursor = i + 1;
        }
        _ => {}
    }
    ModalControl::Stay
}

/// Handle a key press while the post-fetch confirmation overlay is
/// active. Operator picks one of Merge / Replace / Cancel; the first
/// two call `PUT /config/routing.zones` with the resulting list.
async fn handle_zones_fetch_confirm_key(
    app: &mut AppState,
    session: &Session,
    base: &str,
    key: KeyEvent,
) -> Result<ModalControl> {
    match key.code {
        KeyCode::Esc => {
            if let Some(ModalKind::Zones(z)) = app.modal.as_mut() {
                z.fetch_confirm = None;
            }
            return Ok(ModalControl::Stay);
        }
        KeyCode::Left => {
            if let Some(ModalKind::Zones(z)) = app.modal.as_mut() {
                if let Some(fc) = z.fetch_confirm.as_mut() {
                    fc.action_idx = (fc.action_idx + 2) % 3;
                }
            }
            return Ok(ModalControl::Stay);
        }
        KeyCode::Right => {
            if let Some(ModalKind::Zones(z)) = app.modal.as_mut() {
                if let Some(fc) = z.fetch_confirm.as_mut() {
                    fc.action_idx = (fc.action_idx + 1) % 3;
                }
            }
            return Ok(ModalControl::Stay);
        }
        KeyCode::Enter => {
            // Resolve target list from the chosen action without
            // mutating state yet so we can drop the &mut before await.
            let (action_idx, resolved) = {
                let Some(ModalKind::Zones(z)) = app.modal.as_mut() else {
                    return Ok(ModalControl::Stay);
                };
                let Some(fc) = z.fetch_confirm.as_ref() else {
                    return Ok(ModalControl::Stay);
                };
                let resolved = match fc.action_idx {
                    0 => merge_zones(&z.zones, &fc.fetched),
                    1 => {
                        // Replace = exactly the fetched list, deduped
                        // and sorted for stable wire shape.
                        merge_zones(&[], &fc.fetched)
                    }
                    _ => Vec::new(),
                };
                (fc.action_idx, resolved)
            };
            if action_idx == 2 {
                // Cancel: drop the overlay without touching anything.
                if let Some(ModalKind::Zones(z)) = app.modal.as_mut() {
                    z.fetch_confirm = None;
                }
                return Ok(ModalControl::Stay);
            }
            let payload = Value::Array(
                resolved
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect::<Vec<_>>(),
            );
            app.busy = true;
            app.status = "Writing zones...".to_string();
            let result = put_value(session, base, "routing.zones", payload).await;
            app.busy = false;
            match result {
                Ok(()) => {
                    app.updated += 1;
                    if let Some(ModalKind::Zones(z)) = app.modal.as_mut() {
                        z.zones = resolved.clone();
                        if z.zones.is_empty() {
                            z.list_state.select(None);
                        } else {
                            z.list_state.select(Some(0));
                        }
                        z.fetch_confirm = None;
                    }
                    refresh_list(app, session, base).await;
                    app.status = format!("Wrote {} zone(s) to routing.zones", resolved.len());
                }
                Err(e) => {
                    if let Some(ModalKind::Zones(z)) = app.modal.as_mut() {
                        z.error = Some(format!("write failed: {e}"));
                    }
                }
            }
            return Ok(ModalControl::Stay);
        }
        _ => {}
    }
    Ok(ModalControl::Stay)
}

/// Step the highlighted row in the zones list (wraps).
fn zones_step_selection(modal: &mut ZonesModal, delta: i32) {
    let len = modal.zones.len();
    if len == 0 {
        modal.list_state.select(None);
        return;
    }
    let i = modal.list_state.selected().unwrap_or(0) as i32;
    let next = (i + delta).rem_euclid(len as i32) as usize;
    modal.list_state.select(Some(next));
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

/// Render whichever modal is currently open.
fn draw_modal(f: &mut ratatui::Frame<'_>, app: &AppState) {
    match app.modal.as_ref() {
        Some(ModalKind::Key(modal)) => draw_key_modal(f, app, modal),
        Some(ModalKind::Zones(zones)) => draw_zones_modal(f, app, zones),
        None => {}
    }
}

/// Render the generic single-value editor modal.
fn draw_key_modal(f: &mut ratatui::Frame<'_>, app: &AppState, modal: &Modal) {
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

/// Render the `routing.zones` list editor on top of the main view.
fn draw_zones_modal(f: &mut ratatui::Frame<'_>, app: &AppState, zones: &ZonesModal) {
    let area = centered_rect(70, 70, f.area());
    f.render_widget(Clear, area);

    let category = app
        .categories
        .get(app.cat_state.selected().unwrap_or(0))
        .map(|s| s.as_str())
        .unwrap_or("Routing");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            format!(" {category} > DNS zones "),
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
            Constraint::Min(3),    // zone list
            Constraint::Length(3), // inline add input (when active)
            Constraint::Length(1), // error
            Constraint::Length(1), // hints
        ])
        .split(inner);

    let cf_token_set = cloudflare_token_is_set(&app.list);
    let doc_text = if cf_token_set {
        "DNS zones you manage. Press F to fetch from Cloudflare."
    } else {
        "DNS zones you manage. Set cloudflare.api_token to enable Cloudflare fetch."
    };
    f.render_widget(
        Paragraph::new(doc_text)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(Color::Gray)),
        layout[0],
    );

    // Zone list.
    let items: Vec<ListItem> = if zones.zones.is_empty() {
        vec![ListItem::new(Span::styled(
            "(no zones yet)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        zones
            .zones
            .iter()
            .map(|z| ListItem::new(z.as_str()))
            .collect()
    };
    let zone_block = Block::default().borders(Borders::ALL).title(" Zones ");
    let zone_list = List::new(items).block(zone_block).highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    let mut state = zones.list_state.clone();
    f.render_stateful_widget(zone_list, layout[1], &mut state);

    // Inline add input.
    if let Some(buf) = zones.add_buffer.as_ref() {
        let add_block = Block::default().borders(Borders::ALL).title(" Add zone ");
        let para = Paragraph::new(buf.as_str()).block(add_block);
        f.render_widget(para, layout[2]);
        let inner_add = Block::default().borders(Borders::ALL).inner(layout[2]);
        let col = inner_add.x + zones.add_cursor as u16;
        let row = inner_add.y;
        f.set_cursor_position((col, row));
    }

    if let Some(err) = &zones.error {
        f.render_widget(
            Paragraph::new(err.as_str()).style(Style::default().fg(Color::Red)),
            layout[3],
        );
    }

    let hint = if zones.add_buffer.is_some() {
        "[Enter] add  *  [Esc] cancel input"
    } else if cf_token_set {
        "[a] add  *  [d] remove  *  [F] fetch CF  *  [Enter] save  *  [Esc] cancel"
    } else {
        "[a] add  *  [d] remove  *  [Enter] save  *  [Esc] cancel"
    };
    f.render_widget(
        Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
        layout[4],
    );

    // Fetch-confirmation overlay sits over the editor.
    if let Some(fc) = zones.fetch_confirm.as_ref() {
        draw_zones_fetch_confirm(f, fc);
    }
}

/// Render the post-fetch confirmation overlay on top of the zones
/// editor.
fn draw_zones_fetch_confirm(f: &mut ratatui::Frame<'_>, fc: &FetchConfirm) {
    let area = centered_rect(50, 60, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            " Cloudflare fetch result ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    f.render_widget(block.clone(), area);
    let inner = block.inner(area);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // summary
            Constraint::Min(1),    // fetched list
            Constraint::Length(1), // actions
        ])
        .split(inner);
    let summary = if fc.fetched.is_empty() {
        "Cloudflare returned no zones for the configured token.".to_string()
    } else {
        format!(
            "Cloudflare returned {} zone(s). Pick how to apply.",
            fc.fetched.len()
        )
    };
    f.render_widget(
        Paragraph::new(summary)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(Color::Gray)),
        layout[0],
    );

    let items: Vec<ListItem> = fc
        .fetched
        .iter()
        .map(|z| ListItem::new(z.as_str()))
        .collect();
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(" Fetched "));
    f.render_widget(list, layout[1]);

    let labels = ["[Merge]", "[Replace]", "[Cancel]"];
    let mut spans: Vec<Span<'_>> = Vec::new();
    for (i, label) in labels.iter().enumerate() {
        let style = if i == fc.action_idx {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(*label, style));
        if i + 1 < labels.len() {
            spans.push(Span::raw("  "));
        }
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
        layout[2],
    );
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

/// Open the right modal for `entry` given the current snapshot.
///
/// Routing rules:
///
///  * `routing.zones` (StringList) → [`ModalKind::Zones`].
///  * `routing.default_zone` when `routing.zones` is non-empty → key
///    modal with the zones promoted into `entry.choices` so the picker
///    renders a select widget.
///  * Anything else → generic [`ModalKind::Key`].
pub(crate) fn open_modal_for(
    entry: SchemaEntry,
    list: &[ListRow],
    current: Option<&ListRow>,
) -> ModalKind {
    if entry.key == "routing.zones" || entry.ty == KeyType::StringList {
        let zones_row = list.iter().find(|r| r.key == "routing.zones");
        return ModalKind::Zones(ZonesModal::new(zones_row));
    }
    if entry.key == "routing.default_zone" {
        let zones_row = list.iter().find(|r| r.key == "routing.zones");
        let zones = zones_from_list_row(zones_row);
        if !zones.is_empty() {
            let mut promoted = entry.clone();
            promoted.choices = Some(
                zones
                    .into_iter()
                    .map(|z| super::Choice {
                        value: z.clone(),
                        label: z,
                    })
                    .collect(),
            );
            return ModalKind::Key(Modal::new(promoted, current));
        }
    }
    ModalKind::Key(Modal::new(entry, current))
}

/// True when a backing-store row exists for `cloudflare.api_token`.
///
/// The TUI uses this to gate the Cloudflare fetch action: a default
/// row (or no row at all) means the operator never set the token, so
/// the `F` binding is disabled with an inline hint.
pub(crate) fn cloudflare_token_is_set(list: &[ListRow]) -> bool {
    list.iter()
        .any(|r| r.key == "cloudflare.api_token" && r.source == "set")
}

/// Combine `current` and `fetched` into a stable, deduped, sorted
/// list. Used by both the Merge and Replace branches of the fetch
/// confirmation overlay (Replace passes an empty `current`).
pub(crate) fn merge_zones(current: &[String], fetched: &[String]) -> Vec<String> {
    let mut out: Vec<String> = current.iter().chain(fetched.iter()).cloned().collect();
    out.sort();
    out.dedup();
    out
}

/// Extract the zone list out of a `routing.zones` list row. Returns
/// the empty vector when the row is missing, unset, or carries the
/// wrong JSON shape (the controller validates incoming writes; a stray
/// row here means the wire shape drifted and we surface a clean
/// fallback).
pub(crate) fn zones_from_list_row(row: Option<&ListRow>) -> Vec<String> {
    match row.map(|r| &r.value) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
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
        app.modal = Some(ModalKind::Key(Modal::new(acme, None)));
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

    fn routing_zones_entry() -> SchemaEntry {
        SchemaEntry {
            key: "routing.zones".into(),
            display_name: "DNS zones".into(),
            category: "Routing".into(),
            ty: KeyType::StringList,
            default: None,
            doc: "DNS zones you manage.".into(),
            choices: None,
        }
    }

    fn default_zone_entry() -> SchemaEntry {
        SchemaEntry {
            key: "routing.default_zone".into(),
            display_name: "Default zone".into(),
            category: "Routing".into(),
            ty: KeyType::String,
            default: None,
            doc: "Default routing zone.".into(),
            choices: None,
        }
    }

    #[test]
    fn cloudflare_token_is_set_true_only_when_source_set() {
        let list_set = vec![ListRow {
            key: "cloudflare.api_token".into(),
            ty: KeyType::Secret,
            value: Value::String("<redacted>".into()),
            source: "set".into(),
        }];
        assert!(cloudflare_token_is_set(&list_set));

        let list_unset = vec![ListRow {
            key: "cloudflare.api_token".into(),
            ty: KeyType::Secret,
            value: Value::Null,
            source: "unset".into(),
        }];
        assert!(!cloudflare_token_is_set(&list_unset));

        assert!(!cloudflare_token_is_set(&[]));
    }

    #[test]
    fn merge_zones_dedups_and_sorts() {
        let current = vec!["vallee.casa".to_string(), "weavers.engineering".to_string()];
        let fetched = vec!["weavers.engineering".to_string(), "another.dev".to_string()];
        assert_eq!(
            merge_zones(&current, &fetched),
            vec![
                "another.dev".to_string(),
                "vallee.casa".to_string(),
                "weavers.engineering".to_string(),
            ]
        );
    }

    #[test]
    fn merge_zones_replace_with_empty_current_returns_sorted_fetched() {
        let fetched = vec![
            "b.com".to_string(),
            "a.com".to_string(),
            "b.com".to_string(),
        ];
        assert_eq!(
            merge_zones(&[], &fetched),
            vec!["a.com".to_string(), "b.com".to_string()]
        );
    }

    #[test]
    fn zones_from_list_row_returns_empty_for_missing_or_null() {
        assert!(zones_from_list_row(None).is_empty());
        let unset = ListRow {
            key: "routing.zones".into(),
            ty: KeyType::StringList,
            value: Value::Null,
            source: "unset".into(),
        };
        assert!(zones_from_list_row(Some(&unset)).is_empty());
    }

    #[test]
    fn zones_from_list_row_parses_string_array() {
        let row = ListRow {
            key: "routing.zones".into(),
            ty: KeyType::StringList,
            value: json!(["a.com", "b.com"]),
            source: "set".into(),
        };
        assert_eq!(
            zones_from_list_row(Some(&row)),
            vec!["a.com".to_string(), "b.com".to_string()]
        );
    }

    #[test]
    fn open_modal_for_routing_zones_returns_zones_kind() {
        let entry = routing_zones_entry();
        let list = vec![ListRow {
            key: "routing.zones".into(),
            ty: KeyType::StringList,
            value: json!(["one.dev", "two.dev"]),
            source: "set".into(),
        }];
        let kind = open_modal_for(entry, &list, list.first());
        match kind {
            ModalKind::Zones(z) => {
                assert_eq!(z.zones, vec!["one.dev".to_string(), "two.dev".to_string()]);
            }
            _ => panic!("expected ModalKind::Zones"),
        }
    }

    #[test]
    fn open_modal_for_default_zone_promotes_zones_to_choices() {
        let entry = default_zone_entry();
        let list = vec![ListRow {
            key: "routing.zones".into(),
            ty: KeyType::StringList,
            value: json!(["one.dev", "two.dev"]),
            source: "set".into(),
        }];
        let kind = open_modal_for(entry, &list, None);
        match kind {
            ModalKind::Key(modal) => {
                let choices = modal
                    .entry
                    .choices
                    .as_ref()
                    .expect("choices promoted from routing.zones");
                let values: Vec<&str> = choices.iter().map(|c| c.value.as_str()).collect();
                assert_eq!(values, vec!["one.dev", "two.dev"]);
                assert_eq!(modal.mode, Mode::Choices);
            }
            _ => panic!("expected ModalKind::Key with promoted choices"),
        }
    }

    #[test]
    fn open_modal_for_default_zone_falls_back_to_string_input_when_no_zones() {
        let entry = default_zone_entry();
        let kind = open_modal_for(entry, &[], None);
        match kind {
            ModalKind::Key(modal) => {
                assert!(modal.entry.choices.is_none());
                assert_eq!(modal.mode, Mode::String);
            }
            _ => panic!("expected ModalKind::Key"),
        }
    }

    #[test]
    fn zones_modal_seeds_from_current_row() {
        let row = ListRow {
            key: "routing.zones".into(),
            ty: KeyType::StringList,
            value: json!(["a.com", "b.com", "c.com"]),
            source: "set".into(),
        };
        let modal = ZonesModal::new(Some(&row));
        assert_eq!(modal.zones, vec!["a.com", "b.com", "c.com"]);
        assert_eq!(modal.list_state.selected(), Some(0));
    }

    #[test]
    fn zones_modal_empty_when_no_row() {
        let modal = ZonesModal::new(None);
        assert!(modal.zones.is_empty());
        assert_eq!(modal.list_state.selected(), None);
    }

    #[test]
    fn zones_step_selection_wraps_around() {
        let row = ListRow {
            key: "routing.zones".into(),
            ty: KeyType::StringList,
            value: json!(["a", "b", "c"]),
            source: "set".into(),
        };
        let mut modal = ZonesModal::new(Some(&row));
        modal.list_state.select(Some(0));
        zones_step_selection(&mut modal, -1);
        assert_eq!(modal.list_state.selected(), Some(2));
        zones_step_selection(&mut modal, 1);
        assert_eq!(modal.list_state.selected(), Some(0));
    }

    #[test]
    fn draw_zones_modal_renders_list_and_hint() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let entries = vec![routing_zones_entry()];
        let list = vec![
            ListRow {
                key: "cloudflare.api_token".into(),
                ty: KeyType::Secret,
                value: Value::String("<redacted>".into()),
                source: "set".into(),
            },
            ListRow {
                key: "routing.zones".into(),
                ty: KeyType::StringList,
                value: json!(["weavers.engineering", "vallee.casa"]),
                source: "set".into(),
            },
        ];
        let mut app = AppState::new(entries, list.clone());
        app.modal = Some(ModalKind::Zones(ZonesModal::new(list.last())));
        terminal.draw(|f| draw(f, &app)).unwrap();
        let rendered = buffer_to_string(&terminal.backend().buffer().clone());
        assert!(rendered.contains("DNS zones"), "rendered: {rendered}");
        assert!(
            rendered.contains("weavers.engineering"),
            "rendered: {rendered}"
        );
        assert!(rendered.contains("vallee.casa"), "rendered: {rendered}");
        // Fetch hint is enabled because the CF token is set in the
        // fixture above.
        assert!(rendered.contains("[F] fetch CF"), "rendered: {rendered}");
    }

    #[test]
    fn draw_zones_modal_hides_fetch_hint_when_token_unset() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let entries = vec![routing_zones_entry()];
        let list: Vec<ListRow> = Vec::new();
        let mut app = AppState::new(entries, list);
        app.modal = Some(ModalKind::Zones(ZonesModal::new(None)));
        terminal.draw(|f| draw(f, &app)).unwrap();
        let rendered = buffer_to_string(&terminal.backend().buffer().clone());
        assert!(rendered.contains("DNS zones"), "rendered: {rendered}");
        assert!(rendered.contains("(no zones yet)"), "rendered: {rendered}");
        assert!(
            !rendered.contains("[F] fetch CF"),
            "fetch hint must be hidden without token: {rendered}"
        );
    }

    #[test]
    fn draw_zones_modal_shows_fetch_confirm_actions() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let entries = vec![routing_zones_entry()];
        let mut app = AppState::new(entries, Vec::new());
        let mut zones_modal = ZonesModal::new(None);
        zones_modal.fetch_confirm = Some(FetchConfirm {
            fetched: vec!["x.com".to_string(), "y.com".to_string()],
            action_idx: 0,
        });
        app.modal = Some(ModalKind::Zones(zones_modal));
        terminal.draw(|f| draw(f, &app)).unwrap();
        let rendered = buffer_to_string(&terminal.backend().buffer().clone());
        assert!(
            rendered.contains("Cloudflare fetch result"),
            "rendered: {rendered}"
        );
        assert!(rendered.contains("[Merge]"), "rendered: {rendered}");
        assert!(rendered.contains("[Replace]"), "rendered: {rendered}");
        assert!(rendered.contains("[Cancel]"), "rendered: {rendered}");
        assert!(rendered.contains("x.com"), "rendered: {rendered}");
        assert!(rendered.contains("y.com"), "rendered: {rendered}");
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
        app.modal = Some(ModalKind::Key(modal));
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
