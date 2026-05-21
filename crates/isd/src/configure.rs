//! `isd configure` operator surface (controller-wide configuration).
//!
//! Bare `isd configure` (no sub-verb) opens a two-level interactive
//! menu: pick a category, then pick a key inside that category, then
//! edit. `isd configure setup` is an explicit alias for the same menu.
//!
//! Five sub-verbs:
//!
//!  - `isd configure get <key> [--show-secret]`: print one key.
//!  - `isd configure set <key> [<value>|--stdin|--from-file <path>]`:
//!    write one key. Secret-typed keys refuse inline values at the
//!    parser-side level via clap `conflicts_with`, and again at runtime
//!    via a schema fetch before the PUT.
//!  - `isd configure unset <key>`: clear one key (falls back to the
//!    schema default if any).
//!  - `isd configure list [--show-secrets]`: print every key with its
//!    current value (secrets redacted by default).
//!  - `isd configure schema`: print the static schema (key, type,
//!    default, description).
//!
//! All verbs hit the dashboard's `/api/v1/config*` routes through
//! a [`Session`] (same pattern as `secret.rs` and `ssh/mod.rs`).
//!
//! There is no rotation verb: operators just `set` again. See the
//! `2026-05-21-isd-configure-design` spec for the locked decisions.

use std::io::{IsTerminal, Read};
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow};
use clap::{Args, Subcommand};
use comfy_table::{ContentArrangement, Table, presets::NOTHING};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::Session;

/// Type of a schema entry. Mirrors the controller's `KeyType` over the
/// wire: serialised as a lower-snake-case string.
///
/// Defined locally so the `isd` crate does not need to depend on
/// `isengard-controller` (which pulls in sqlx, the docker plugins, the
/// secrets store, etc.). The wire format is the source of truth; this
/// enum is just the deserialisation target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyType {
    /// Plain UTF-8 string. Lands in the `settings` table.
    String,
    /// String persisted to the encrypted secrets store. The CLI refuses
    /// inline values for these.
    Secret,
    /// Signed integer.
    Int,
    /// Boolean.
    Bool,
}

/// CLI flags for `isd configure`.
///
/// `command` is optional: bare `isd configure` dispatches to the
/// interactive two-level menu (`run_menu`). Same pattern as `SshArgs`
/// from PR #237 (picker-by-default).
#[derive(Debug, Args)]
pub struct ConfigureArgs {
    /// Resolved sub-verb. `None` means bare `isd configure`: open the
    /// interactive menu.
    #[command(subcommand)]
    pub command: Option<ConfigureCommand>,
}

/// Sub-verbs under `isd configure`.
#[derive(Debug, Subcommand)]
pub enum ConfigureCommand {
    /// Print one key's current value.
    Get(GetArgs),
    /// Set a key. Use the inline positional for plain types; pass
    /// `--stdin` or `--from-file` for secrets.
    Set(SetArgs),
    /// Remove a key. Falls back to the schema default if one exists.
    Unset(UnsetArgs),
    /// Print every key with its current value (secrets redacted).
    List(ListArgs),
    /// Print the schema (key, type, default, description).
    Schema,
    /// Open the interactive two-level configure menu. Explicit alias
    /// for bare `isd configure`.
    Setup,
}

/// CLI flags for `isd configure get`.
#[derive(Debug, Args)]
pub struct GetArgs {
    /// Schema key, e.g. `acme.directory` or `cloudflare.api_token`.
    pub key: String,
    /// Print secret-typed values in cleartext. Off by default.
    #[arg(long)]
    pub show_secret: bool,
}

/// CLI flags for `isd configure set`. Exactly one value-source must be
/// supplied; clap's `conflicts_with_all` enforces that at parse time.
#[derive(Debug, Args)]
pub struct SetArgs {
    /// Schema key.
    pub key: String,
    /// Inline value for plain-typed keys (rejected for secret-typed
    /// keys at runtime; see [`SetArgs::stdin`] / [`SetArgs::from_file`]).
    pub value: Option<String>,
    /// Read the value from stdin (refused when stdin is a TTY).
    #[arg(long, conflicts_with_all = ["value", "from_file"])]
    pub stdin: bool,
    /// Read the value from a file.
    #[arg(long = "from-file", conflicts_with_all = ["value", "stdin"])]
    pub from_file: Option<PathBuf>,
}

/// CLI flags for `isd configure unset`.
#[derive(Debug, Args)]
pub struct UnsetArgs {
    /// Schema key.
    pub key: String,
}

/// CLI flags for `isd configure list`.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Print secret-typed values in cleartext. Off by default.
    #[arg(long)]
    pub show_secrets: bool,
}

/// JSON shape returned by `GET /api/v1/config/{key}`.
#[derive(Debug, Deserialize)]
struct GetResponse {
    /// Echoed schema key.
    #[allow(dead_code)]
    key: String,
    /// Schema-declared type.
    #[serde(rename = "type")]
    ty: KeyType,
    /// Current value. Stored, default, or `<redacted>`.
    value: Value,
    /// `"set"` (stored) or `"default"` (from schema).
    source: String,
    /// `true` when a backing-store row exists.
    #[allow(dead_code)]
    is_set: bool,
}

/// JSON shape returned by `GET /api/v1/config`.
#[derive(Debug, Deserialize)]
struct ListRow {
    /// Schema key.
    key: String,
    /// Schema-declared type.
    #[serde(rename = "type")]
    ty: KeyType,
    /// Current value: stored, default, redacted, or null.
    value: Value,
    /// `"set"`, `"default"`, or `"unset"`.
    source: String,
}

/// JSON shape for one row in `GET /api/v1/config/schema`.
#[derive(Debug, Deserialize, Clone)]
struct SchemaEntry {
    /// Schema key.
    key: String,
    /// Operator-facing label used in menu rows and prompts.
    #[serde(default)]
    display_name: String,
    /// Top-level menu bucket: `Certificates`, `Routing`, `SSH bastion`.
    #[serde(default)]
    category: String,
    /// Schema-declared type.
    #[serde(rename = "type")]
    ty: KeyType,
    /// Optional default value.
    default: Option<Value>,
    /// One-line description.
    doc: String,
}

/// PUT body for `/api/v1/config/{key}`.
#[derive(Debug, Serialize)]
struct PutBody {
    /// New value. The controller validates against the schema.
    value: Value,
}

/// Dispatch to the matching `configure` sub-verb.
///
/// # Errors
///
/// Propagates the sub-verb's error.
pub async fn run(args: ConfigureArgs, context: Option<&str>) -> Result<()> {
    let session = Session::open(context).await?;
    let base = session.require_controller()?.to_owned();
    match args.command {
        None | Some(ConfigureCommand::Setup) => run_menu(&session, &base).await,
        Some(ConfigureCommand::Get(a)) => run_get(&session, &base, a).await,
        Some(ConfigureCommand::Set(a)) => run_set(&session, &base, a).await,
        Some(ConfigureCommand::Unset(a)) => run_unset(&session, &base, a).await,
        Some(ConfigureCommand::List(a)) => run_list(&session, &base, a).await,
        Some(ConfigureCommand::Schema) => run_schema(&session, &base).await,
    }
}

/// `GET /api/v1/config/{key}` then render.
async fn run_get(session: &Session, base: &str, args: GetArgs) -> Result<()> {
    let mut url = format!("{base}/api/v1/config/{}", args.key);
    if args.show_secret {
        url.push_str("?show_secret=1");
    }
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "{}: {}",
            args.key,
            parse_error_message(&body, "not found")
        ));
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("GET {url} -> {status}: {body}"));
    }
    let r: GetResponse = resp.json().await.context("decoding /config/{key} body")?;
    print_get_value(&r, args.show_secret);
    Ok(())
}

/// Render a single-key fetch. Pulled out so the formatter can be unit
/// tested without spinning up a fake controller.
fn print_get_value(r: &GetResponse, show_secret: bool) {
    let rendered = json_to_display(&r.value);
    if r.source == "default" {
        println!("{rendered}  (default)");
    } else if r.ty == KeyType::Secret && !show_secret {
        println!("<redacted>  (use --show-secret to print)");
    } else {
        println!("{rendered}");
    }
}

/// `PUT /api/v1/config/{key}` after resolving the value source.
async fn run_set(session: &Session, base: &str, args: SetArgs) -> Result<()> {
    // 1. Fetch the schema so we can refuse inline values for secret keys
    //    BEFORE we put them on the wire. Also lets us echo the right
    //    confirmation line ("secret; value not echoed" vs the value).
    let schema = fetch_schema(session, base).await?;
    let entry = schema.iter().find(|e| e.key == args.key);
    if let Some(e) = entry {
        if e.ty == KeyType::Secret && args.value.is_some() {
            return Err(anyhow!(
                "{} is a secret-typed key; pass via --stdin or --from-file (shell history is not safe)",
                args.key
            ));
        }
    }

    // 2. Resolve the value: exactly one of inline / stdin / from-file
    //    must be set. clap's conflicts_with_all enforces "at most one";
    //    we enforce "at least one" here.
    let value_str = resolve_set_value(&args)?;

    // 3. Wire the typed value. Secrets and strings ride as JSON strings.
    //    Int and bool keys parse the input so the controller validates
    //    against the right JSON type.
    let value_json = encode_value(entry.map(|e| e.ty), &value_str)?;
    put_value(session, base, &args.key, value_json).await?;
    match entry.map(|e| e.ty) {
        Some(KeyType::Secret) => println!("Set {} (secret; value not echoed)", args.key),
        _ => println!("Set {} = {}", args.key, value_str),
    }
    Ok(())
}

/// `PUT /api/v1/config/{key}` shared helper. Returns the parsed error
/// envelope on non-2xx so callers can surface the controller's hint.
async fn put_value(session: &Session, base: &str, key: &str, value: Value) -> Result<()> {
    let url = format!("{base}/api/v1/config/{key}");
    let resp = session
        .client
        .put(&url)
        .json(&PutBody { value })
        .send()
        .await
        .with_context(|| format!("PUT {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let detail = parse_error_message(&body, &body);
        return Err(anyhow!("{key}: {detail}"));
    }
    Ok(())
}

/// Resolve the `set` value from one of three sources. Returns the
/// raw string; type-aware parsing happens in [`encode_value`].
fn resolve_set_value(args: &SetArgs) -> Result<String> {
    if let Some(v) = args.value.as_deref() {
        return Ok(v.to_string());
    }
    if args.stdin {
        if std::io::stdin().is_terminal() {
            return Err(anyhow!(
                "stdin is a TTY; pipe a value or pass --from-file <path>"
            ));
        }
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading value from stdin")?;
        return Ok(buf.trim_end_matches('\n').to_string());
    }
    if let Some(p) = args.from_file.as_deref() {
        return std::fs::read_to_string(p)
            .with_context(|| format!("reading {}", p.display()))
            .map(|s| s.trim_end_matches('\n').to_string());
    }
    Err(anyhow!(
        "no value provided; pass an inline value, --stdin, or --from-file <path>"
    ))
}

/// Encode the raw string for the controller's PUT body. Strings and
/// secrets ride as JSON strings; int / bool parse so the controller
/// validates against the right JSON type. Unknown keys fall through as
/// JSON strings so the server's did-you-mean path still fires.
fn encode_value(ty: Option<KeyType>, raw: &str) -> Result<Value> {
    match ty {
        Some(KeyType::Int) => {
            let n: i64 = raw
                .parse()
                .with_context(|| format!("expected integer, got {raw:?}"))?;
            Ok(Value::from(n))
        }
        Some(KeyType::Bool) => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" | "on" => Ok(Value::Bool(true)),
            "false" | "no" | "0" | "off" => Ok(Value::Bool(false)),
            other => Err(anyhow!(
                "expected boolean (true/false/yes/no/1/0/on/off), got {other:?}"
            )),
        },
        _ => Ok(Value::String(raw.to_string())),
    }
}

/// `DELETE /api/v1/config/{key}` then render the outcome.
async fn run_unset(session: &Session, base: &str, args: UnsetArgs) -> Result<()> {
    let url = format!("{base}/api/v1/config/{}", args.key);
    let resp = session
        .client
        .delete(&url)
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        println!("{} was not set", args.key);
        return Ok(());
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("DELETE {url} -> {status}: {body}"));
    }
    // 204: row removed. Mention the default fallback if the schema has
    // one, so the operator sees what the next read will return.
    let schema = fetch_schema(session, base).await.unwrap_or_default();
    let entry = schema.iter().find(|e| e.key == args.key);
    match entry.and_then(|e| e.default.clone()) {
        Some(default) => println!(
            "Unset {} (will fall back to default: {})",
            args.key,
            json_to_display(&default)
        ),
        None => println!("Unset {}", args.key),
    }
    Ok(())
}

/// `GET /api/v1/config` then render the snapshot as a table.
async fn run_list(session: &Session, base: &str, args: ListArgs) -> Result<()> {
    let rows = fetch_list(session, base, args.show_secrets).await?;
    if rows.is_empty() {
        println!("No config keys.");
        return Ok(());
    }
    println!("{}", render_list_table(&rows));
    Ok(())
}

/// `GET /api/v1/config` shared helper. `list` and the wizard both need
/// the current snapshot keyed by `key`.
async fn fetch_list(session: &Session, base: &str, show_secrets: bool) -> Result<Vec<ListRow>> {
    let mut url = format!("{base}/api/v1/config");
    if show_secrets {
        url.push_str("?show_secrets=1");
    }
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let rows: Vec<ListRow> = resp.error_for_status()?.json().await?;
    Ok(rows)
}

/// Build the `KEY / TYPE / VALUE / SOURCE` table. Pulled out so the
/// formatter stays testable without an HTTP stub.
fn render_list_table(rows: &[ListRow]) -> Table {
    let mut table = Table::new();
    table
        .load_preset(NOTHING)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_header(vec!["KEY", "TYPE", "VALUE", "SOURCE"]);
    for r in rows {
        table.add_row(vec![
            r.key.clone(),
            key_type_label(r.ty).to_string(),
            json_to_display(&r.value),
            r.source.clone(),
        ]);
    }
    table
}

/// `GET /api/v1/config/schema` then render `KEY / TYPE / DEFAULT / DESCRIPTION`.
async fn run_schema(session: &Session, base: &str) -> Result<()> {
    let entries = fetch_schema(session, base).await?;
    println!("{}", render_schema_table(&entries));
    Ok(())
}

/// Build the schema table. Pulled out so the formatter stays testable.
fn render_schema_table(entries: &[SchemaEntry]) -> Table {
    let mut table = Table::new();
    table
        .load_preset(NOTHING)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_header(vec!["KEY", "TYPE", "DEFAULT", "DESCRIPTION"]);
    for e in entries {
        let default = match &e.default {
            Some(v) => json_to_display(v),
            None => "(none)".to_string(),
        };
        table.add_row(vec![
            e.key.clone(),
            key_type_label(e.ty).to_string(),
            default,
            e.doc.clone(),
        ]);
    }
    table
}

/// `GET /api/v1/config/schema` shared helper. `set` and `unset` call this
/// to drive their client-side guards and default-fallback messaging.
async fn fetch_schema(session: &Session, base: &str) -> Result<Vec<SchemaEntry>> {
    let url = format!("{base}/api/v1/config/schema");
    let resp = session
        .client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let entries: Vec<SchemaEntry> = resp.error_for_status()?.json().await?;
    Ok(entries)
}

/// Interactive two-level menu. Outer level picks a category; inner
/// level picks a key inside the chosen category; selecting a key opens
/// the type-aware edit prompt. Esc / `[back]` / `[done]` unwind cleanly.
///
/// Replaces the sequential walking wizard from PR #244: operators get
/// a menu where every key is reachable in two hops, and the rendering
/// groups by feature (Certificates, Routing, SSH bastion) so future
/// DNS-01 providers nest under one bucket.
async fn run_menu(session: &Session, base: &str) -> Result<()> {
    let schema = fetch_schema(session, base).await?;
    println!("isd configure: {} keys.", schema.len());

    let mut updated: usize = 0;

    loop {
        // Refresh on every outer iteration so the operator sees their
        // edits reflected immediately.
        let list = fetch_list(session, base, false).await.unwrap_or_default();
        let groups = group_by_category(&schema);
        let rows = build_category_rows(&groups, &list);

        let picked = match inquire::Select::new("Pick a category:", rows)
            .with_page_size(8)
            .prompt_skippable()
        {
            Ok(Some(row)) => row,
            Ok(None) => break, // Esc
            Err(e) => return Err(anyhow!("category picker failed: {e}")),
        };

        let CategoryRow { kind, .. } = picked;
        let category = match kind {
            CategoryRowKind::Done => break,
            CategoryRowKind::Category(name) => name,
        };

        // Inner loop for the chosen category. Refresh the list before
        // each prompt so values update after edits.
        loop {
            let list = fetch_list(session, base, false).await.unwrap_or_default();
            let groups = group_by_category(&schema);
            let entries = match groups.get(category.as_str()) {
                Some(v) => v.clone(),
                None => break, // category vanished (shouldn't happen)
            };
            let rows = build_key_rows(&entries, &list);

            let inner_picked = match inquire::Select::new("Pick a key:", rows)
                .with_page_size(8)
                .with_help_message(&format!("isd configure > {category}"))
                .prompt_skippable()
            {
                Ok(Some(row)) => row,
                Ok(None) => break, // Esc -> back to outer
                Err(e) => return Err(anyhow!("key picker failed: {e}")),
            };

            let KeyRow { kind, .. } = inner_picked;
            let entry = match kind {
                KeyRowKind::Back => break,
                KeyRowKind::Key(e) => e,
            };

            match edit_key(session, base, &entry).await {
                EditOutcome::Saved => updated += 1,
                EditOutcome::Cancelled => {}
                EditOutcome::Error(msg) => println!("Skipped {}: {msg}", entry.key),
            }
        }
    }

    println!();
    println!("{updated} keys updated.");
    Ok(())
}

/// Outcome of one edit attempt inside the inner menu.
enum EditOutcome {
    /// PUT succeeded.
    Saved,
    /// Operator hit Esc on the value prompt: no write attempted.
    Cancelled,
    /// PUT failed. Carry the server's hint so the caller can print it.
    Error(String),
}

/// Run the type-aware edit prompt for one key and PUT the result.
///
/// Esc on the prompt returns [`EditOutcome::Cancelled`]. PUT errors
/// surface as [`EditOutcome::Error`] without crashing the menu so a
/// single bad entry doesn't blow up the whole session.
async fn edit_key(session: &Session, base: &str, entry: &SchemaEntry) -> EditOutcome {
    let label = key_label(entry);
    let value_json = match prompt_for_value(entry, &label) {
        Ok(v) => v,
        Err(EditExit::Cancelled) => return EditOutcome::Cancelled,
    };
    match put_value(session, base, &entry.key, value_json.clone()).await {
        Ok(()) => {
            if entry.ty == KeyType::Secret {
                println!("Set {} (secret; value not echoed)", entry.key);
            } else {
                println!("Set {} = {}", entry.key, json_to_display(&value_json));
            }
            EditOutcome::Saved
        }
        Err(e) => EditOutcome::Error(format!("{e}")),
    }
}

/// Sentinel for the value prompt: Esc / Ctrl-C cancels the edit but
/// stays inside the menu (unlike the old wizard, where Ctrl-C aborted
/// the whole flow).
enum EditExit {
    /// Operator hit Esc; back out to the inner menu without a write.
    Cancelled,
}

/// Type-aware value prompt. Returns the encoded JSON value ready for
/// the PUT body. `label` is shown in front of the input (e.g.
/// `Cloudflare API token:`).
fn prompt_for_value(entry: &SchemaEntry, label: &str) -> Result<Value, EditExit> {
    let secret_label = format!("{label}:");
    let value_label = format!("{label}:");
    let bool_label = format!("{label}?");
    loop {
        let attempt: Result<Value, inquire::InquireError> = match entry.ty {
            KeyType::Secret => inquire::Password::new(&secret_label)
                .with_display_mode(inquire::PasswordDisplayMode::Masked)
                .without_confirmation()
                .prompt()
                .map(Value::String),
            KeyType::String => inquire::Text::new(&value_label).prompt().map(Value::String),
            KeyType::Int => inquire::CustomType::<i64>::new(&value_label)
                .prompt()
                .map(Value::from),
            KeyType::Bool => inquire::Confirm::new(&bool_label).prompt().map(Value::Bool),
        };
        match attempt {
            Ok(Value::String(s)) if s.is_empty() => {
                println!("Empty value rejected. Try again, or Esc to cancel.");
                continue;
            }
            Ok(v) => return Ok(v),
            Err(inquire::InquireError::OperationCanceled)
            | Err(inquire::InquireError::OperationInterrupted) => return Err(EditExit::Cancelled),
            Err(e) => {
                println!("prompt error: {e}");
                return Err(EditExit::Cancelled);
            }
        }
    }
}

/// Group schema entries by category. Alphabetical order on the outer
/// keys via `BTreeMap`. Inside a category we preserve schema declaration
/// order so related keys (e.g. `cloudflare.api_token`,
/// `cloudflare.zone_id`) stay adjacent.
///
/// Pulled out so the grouping behavior is unit-testable without
/// spinning up a fake controller.
fn group_by_category<'a>(
    entries: &'a [SchemaEntry],
) -> std::collections::BTreeMap<&'a str, Vec<&'a SchemaEntry>> {
    let mut out: std::collections::BTreeMap<&'a str, Vec<&'a SchemaEntry>> =
        std::collections::BTreeMap::new();
    for e in entries {
        // Defensive: skip entries with an empty category. Should not
        // happen against a v0.1+ controller but guards against stale
        // mocks in tests.
        if e.category.is_empty() {
            continue;
        }
        out.entry(e.category.as_str()).or_default().push(e);
    }
    out
}

/// Operator-facing label for a key. Falls back to the dotted key when
/// the schema's display_name is empty (older controllers).
fn key_label(entry: &SchemaEntry) -> String {
    if entry.display_name.is_empty() {
        entry.key.clone()
    } else {
        entry.display_name.clone()
    }
}

/// One row in the outer category picker.
#[derive(Clone, Debug)]
struct CategoryRow {
    /// Pre-rendered label (`Certificates  4 keys  2 set`).
    label: String,
    /// What this row resolves to when picked.
    kind: CategoryRowKind,
}

/// Variant carried by a [`CategoryRow`]: either a real category or the
/// `[done]` sentinel that exits the menu.
#[derive(Clone, Debug)]
enum CategoryRowKind {
    /// Pick a category by name.
    Category(String),
    /// Sentinel last row that exits the menu.
    Done,
}

impl std::fmt::Display for CategoryRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// Build the outer category rows. One row per category followed by a
/// `[done]` sentinel.
fn build_category_rows(
    groups: &std::collections::BTreeMap<&str, Vec<&SchemaEntry>>,
    list: &[ListRow],
) -> Vec<CategoryRow> {
    let mut rows: Vec<CategoryRow> = Vec::with_capacity(groups.len() + 1);
    for (cat, entries) in groups {
        if entries.is_empty() {
            continue;
        }
        let n = entries.len();
        let set = entries
            .iter()
            .filter(|e| {
                list.iter()
                    .find(|r| r.key == e.key)
                    .map(|r| r.source == "set")
                    .unwrap_or(false)
            })
            .count();
        let key_word = if n == 1 { "key" } else { "keys" };
        let set_part = if set == 0 {
            "0 set".to_string()
        } else {
            format!("{set} set")
        };
        rows.push(CategoryRow {
            label: format!("{cat}  ({n} {key_word}, {set_part})"),
            kind: CategoryRowKind::Category((*cat).to_string()),
        });
    }
    rows.push(CategoryRow {
        label: "[done]".to_string(),
        kind: CategoryRowKind::Done,
    });
    rows
}

/// One row in the inner key picker.
#[derive(Clone, Debug)]
struct KeyRow {
    /// Pre-rendered label.
    label: String,
    /// What this row resolves to when picked.
    kind: KeyRowKind,
}

/// Variant carried by a [`KeyRow`]: either a schema entry to edit or
/// the `[back]` sentinel that returns to the outer category menu.
#[derive(Clone, Debug)]
enum KeyRowKind {
    /// Pick a key for editing.
    Key(SchemaEntry),
    /// Sentinel last row that returns to the outer menu.
    Back,
}

impl std::fmt::Display for KeyRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// Build the inner key rows. One per entry followed by `[back]`.
fn build_key_rows(entries: &[&SchemaEntry], list: &[ListRow]) -> Vec<KeyRow> {
    // Pad the display name column so the type + value line up.
    let max_label = entries
        .iter()
        .map(|e| key_label(e).chars().count())
        .max()
        .unwrap_or(0);
    let mut rows: Vec<KeyRow> = Vec::with_capacity(entries.len() + 1);
    for entry in entries {
        let label = key_label(entry);
        let padded = format!("{label:<max_label$}");
        let ty = key_type_label(entry.ty);
        let current = list.iter().find(|r| r.key == entry.key);
        let value = menu_value_display(entry, current);
        rows.push(KeyRow {
            label: format!("{padded}  {ty:<6}  {value}"),
            kind: KeyRowKind::Key((*entry).clone()),
        });
    }
    rows.push(KeyRow {
        label: "[back]".to_string(),
        kind: KeyRowKind::Back,
    });
    rows
}

/// Render the value column for an inner key row. Secrets become
/// `<set>` / `<unset>`; defaults get the `(default)` marker; plain
/// values render via [`json_to_display`].
fn menu_value_display(entry: &SchemaEntry, current: Option<&ListRow>) -> String {
    let source = current.map(|r| r.source.as_str()).unwrap_or("unset");
    match (entry.ty, source) {
        (KeyType::Secret, "set") => "<set>".to_string(),
        (KeyType::Secret, _) => "<unset>".to_string(),
        (_, "unset") => "<unset>".to_string(),
        _ => match current.map(|r| &r.value) {
            Some(v) => {
                let rendered = json_to_display(v);
                if source == "default" {
                    format!("(default) {rendered}")
                } else {
                    rendered
                }
            }
            None => "<unset>".to_string(),
        },
    }
}

/// Lower-snake-case label for a [`KeyType`]. Used in table rendering.
fn key_type_label(ty: KeyType) -> &'static str {
    match ty {
        KeyType::String => "string",
        KeyType::Secret => "secret",
        KeyType::Int => "int",
        KeyType::Bool => "bool",
    }
}

/// Render a JSON value for terminal display. Strings drop their quotes
/// so `acme.directory` prints as `https://...` (not `"https://..."`).
/// `null` renders as `(unset)` so list rows stay readable.
fn json_to_display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "(unset)".to_string(),
        other => other.to_string(),
    }
}

/// Extract the `error` field from a controller error envelope
/// (`{"error": "..."}`). Falls back to `default` when the body is not
/// JSON or carries no `error` field.
fn parse_error_message(body: &str, default: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        if let Some(s) = v.get("error").and_then(Value::as_str) {
            return s.to_string();
        }
    }
    default.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct Wrap {
        #[command(subcommand)]
        c: ConfigureCommand,
    }

    /// Outer wrapper for [`ConfigureArgs`] (subcommand is optional, so
    /// it parses bare `isd configure` without arguments).
    #[derive(Parser, Debug)]
    struct WrapArgs {
        #[command(flatten)]
        a: ConfigureArgs,
    }

    #[test]
    fn configure_get_parses() {
        let w = Wrap::try_parse_from(["x", "get", "acme.directory"]).unwrap();
        match w.c {
            ConfigureCommand::Get(a) => {
                assert_eq!(a.key, "acme.directory");
                assert!(!a.show_secret);
            }
            other => panic!("expected Get, got {other:?}"),
        }
    }

    #[test]
    fn configure_get_with_show_secret_parses() {
        let w =
            Wrap::try_parse_from(["x", "get", "cloudflare.api_token", "--show-secret"]).unwrap();
        match w.c {
            ConfigureCommand::Get(a) => assert!(a.show_secret),
            other => panic!("expected Get, got {other:?}"),
        }
    }

    #[test]
    fn configure_set_inline_parses() {
        let w = Wrap::try_parse_from(["x", "set", "routing.default_zone", "weavers.engineering"])
            .unwrap();
        match w.c {
            ConfigureCommand::Set(a) => {
                assert_eq!(a.key, "routing.default_zone");
                assert_eq!(a.value.as_deref(), Some("weavers.engineering"));
                assert!(!a.stdin);
                assert!(a.from_file.is_none());
            }
            other => panic!("expected Set, got {other:?}"),
        }
    }

    #[test]
    fn configure_set_stdin_parses() {
        let w = Wrap::try_parse_from(["x", "set", "cloudflare.api_token", "--stdin"]).unwrap();
        match w.c {
            ConfigureCommand::Set(a) => {
                assert_eq!(a.key, "cloudflare.api_token");
                assert!(a.value.is_none());
                assert!(a.stdin);
                assert!(a.from_file.is_none());
            }
            other => panic!("expected Set, got {other:?}"),
        }
    }

    #[test]
    fn configure_set_from_file_parses() {
        let w = Wrap::try_parse_from([
            "x",
            "set",
            "cloudflare.api_token",
            "--from-file",
            "/tmp/cf.token",
        ])
        .unwrap();
        match w.c {
            ConfigureCommand::Set(a) => {
                assert!(a.value.is_none());
                assert!(!a.stdin);
                assert_eq!(
                    a.from_file.as_deref().and_then(|p| p.to_str()),
                    Some("/tmp/cf.token")
                );
            }
            other => panic!("expected Set, got {other:?}"),
        }
    }

    #[test]
    fn configure_set_value_conflicts_with_stdin() {
        // Inline value AND --stdin must be rejected by clap's
        // conflicts_with_all.
        let err = Wrap::try_parse_from(["x", "set", "k", "v", "--stdin"]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("conflict") || msg.contains("cannot be used") || msg.contains("cannot"),
            "expected conflict error, got: {msg}"
        );
    }

    #[test]
    fn configure_set_value_conflicts_with_from_file() {
        let err =
            Wrap::try_parse_from(["x", "set", "k", "v", "--from-file", "/tmp/x"]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("conflict") || msg.contains("cannot be used") || msg.contains("cannot"),
            "expected conflict error, got: {msg}"
        );
    }

    #[test]
    fn configure_set_stdin_and_from_file_conflict() {
        let err = Wrap::try_parse_from(["x", "set", "k", "--stdin", "--from-file", "/tmp/x"])
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("conflict") || msg.contains("cannot be used") || msg.contains("cannot"),
            "expected conflict error, got: {msg}"
        );
    }

    #[test]
    fn configure_set_refuses_three_positional_args() {
        // The third positional should be rejected: `set` accepts at most
        // `<key> [value]`.
        let err = Wrap::try_parse_from(["x", "set", "k", "v", "extra"]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unexpected")
                || msg.contains("extra")
                || msg.contains("argument")
                || msg.contains("found"),
            "expected extra-arg error, got: {msg}"
        );
    }

    #[test]
    fn configure_unset_parses() {
        let w = Wrap::try_parse_from(["x", "unset", "acme.directory"]).unwrap();
        match w.c {
            ConfigureCommand::Unset(a) => assert_eq!(a.key, "acme.directory"),
            other => panic!("expected Unset, got {other:?}"),
        }
    }

    #[test]
    fn configure_list_parses_default() {
        let w = Wrap::try_parse_from(["x", "list"]).unwrap();
        match w.c {
            ConfigureCommand::List(a) => assert!(!a.show_secrets),
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn configure_list_with_show_secrets_parses() {
        let w = Wrap::try_parse_from(["x", "list", "--show-secrets"]).unwrap();
        match w.c {
            ConfigureCommand::List(a) => assert!(a.show_secrets),
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn configure_schema_parses() {
        let w = Wrap::try_parse_from(["x", "schema"]).unwrap();
        assert!(matches!(w.c, ConfigureCommand::Schema));
    }

    #[test]
    fn json_to_display_drops_string_quotes() {
        assert_eq!(json_to_display(&Value::String("hi".into())), "hi");
        assert_eq!(json_to_display(&serde_json::json!(42)), "42");
        assert_eq!(json_to_display(&serde_json::json!(true)), "true");
        assert_eq!(json_to_display(&Value::Null), "(unset)");
    }

    #[test]
    fn key_type_label_renders_snake_case() {
        assert_eq!(key_type_label(KeyType::String), "string");
        assert_eq!(key_type_label(KeyType::Secret), "secret");
        assert_eq!(key_type_label(KeyType::Int), "int");
        assert_eq!(key_type_label(KeyType::Bool), "bool");
    }

    #[test]
    fn encode_value_parses_int_for_int_type() {
        let v = encode_value(Some(KeyType::Int), "3600").unwrap();
        assert_eq!(v, Value::from(3600i64));
    }

    #[test]
    fn encode_value_rejects_non_int_for_int_type() {
        assert!(encode_value(Some(KeyType::Int), "not-a-number").is_err());
    }

    #[test]
    fn encode_value_parses_bool_for_bool_type() {
        assert_eq!(
            encode_value(Some(KeyType::Bool), "true").unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            encode_value(Some(KeyType::Bool), "no").unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            encode_value(Some(KeyType::Bool), "on").unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            encode_value(Some(KeyType::Bool), "0").unwrap(),
            Value::Bool(false)
        );
    }

    #[test]
    fn encode_value_strings_for_unknown_type() {
        // Unknown key: fall through as a JSON string so the server's
        // did-you-mean path still fires.
        let v = encode_value(None, "abc").unwrap();
        assert_eq!(v, Value::String("abc".into()));
    }

    #[test]
    fn parse_error_message_extracts_envelope() {
        let body = r#"{"error": "unknown key foo"}"#;
        assert_eq!(parse_error_message(body, "fallback"), "unknown key foo");
    }

    #[test]
    fn parse_error_message_falls_back_for_non_json() {
        assert_eq!(parse_error_message("not-json", "fallback"), "fallback");
    }

    #[test]
    fn render_list_table_includes_header_and_rows() {
        let rows = vec![
            ListRow {
                key: "acme.directory".into(),
                ty: KeyType::String,
                value: Value::String("https://example.com".into()),
                source: "default".into(),
            },
            ListRow {
                key: "cloudflare.api_token".into(),
                ty: KeyType::Secret,
                value: Value::String("<redacted>".into()),
                source: "set".into(),
            },
        ];
        let rendered = render_list_table(&rows).to_string();
        assert!(rendered.contains("KEY"), "rendered: {rendered}");
        assert!(rendered.contains("TYPE"), "rendered: {rendered}");
        assert!(rendered.contains("VALUE"), "rendered: {rendered}");
        assert!(rendered.contains("SOURCE"), "rendered: {rendered}");
        assert!(rendered.contains("acme.directory"), "rendered: {rendered}");
        assert!(
            rendered.contains("https://example.com"),
            "rendered: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "rendered: {rendered}");
    }

    #[test]
    fn render_schema_table_renders_none_for_missing_default() {
        let entries = vec![
            SchemaEntry {
                key: "cloudflare.api_token".into(),
                display_name: "Cloudflare API token".into(),
                category: "Certificates".into(),
                ty: KeyType::Secret,
                default: None,
                doc: "CF token".into(),
            },
            SchemaEntry {
                key: "acme.directory".into(),
                display_name: "ACME directory".into(),
                category: "Certificates".into(),
                ty: KeyType::String,
                default: Some(Value::String("https://example.com".into())),
                doc: "ACME dir".into(),
            },
        ];
        let rendered = render_schema_table(&entries).to_string();
        assert!(rendered.contains("DEFAULT"), "rendered: {rendered}");
        assert!(rendered.contains("DESCRIPTION"), "rendered: {rendered}");
        assert!(rendered.contains("(none)"), "rendered: {rendered}");
        assert!(
            rendered.contains("https://example.com"),
            "rendered: {rendered}"
        );
    }

    #[test]
    fn configure_bare_parses_as_none() {
        // `isd configure` with no sub-verb must parse, with
        // `command = None`. That is what dispatches to the menu.
        let w = WrapArgs::try_parse_from(["x"]).unwrap();
        assert!(
            w.a.command.is_none(),
            "expected None, got {:?}",
            w.a.command
        );
    }

    #[test]
    fn configure_setup_alias_parses() {
        // `isd configure setup` must parse as the explicit Setup
        // alias for the menu.
        let w = WrapArgs::try_parse_from(["x", "setup"]).unwrap();
        assert!(matches!(w.a.command, Some(ConfigureCommand::Setup)));
    }

    #[test]
    fn configure_args_get_still_parses() {
        // Sanity: ConfigureArgs (the outer struct) still routes
        // explicit verbs correctly after flipping `command` to optional.
        let w = WrapArgs::try_parse_from(["x", "get", "acme.directory"]).unwrap();
        match w.a.command {
            Some(ConfigureCommand::Get(a)) => assert_eq!(a.key, "acme.directory"),
            other => panic!("expected Some(Get), got {other:?}"),
        }
    }

    // --- menu helpers ------------------------------------------------------

    /// Build the six v0.1 entries inline so the menu helpers can be
    /// tested without a fake controller round-trip.
    fn v01_schema_entries() -> Vec<SchemaEntry> {
        vec![
            SchemaEntry {
                key: "cloudflare.api_token".into(),
                display_name: "Cloudflare API token".into(),
                category: "Certificates".into(),
                ty: KeyType::Secret,
                default: None,
                doc: "CF token".into(),
            },
            SchemaEntry {
                key: "cloudflare.zone_id".into(),
                display_name: "Cloudflare zone ID".into(),
                category: "Certificates".into(),
                ty: KeyType::String,
                default: None,
                doc: "CF zone".into(),
            },
            SchemaEntry {
                key: "routing.default_zone".into(),
                display_name: "Default zone".into(),
                category: "Routing".into(),
                ty: KeyType::String,
                default: None,
                doc: "Default routing zone".into(),
            },
            SchemaEntry {
                key: "acme.contact_email".into(),
                display_name: "Contact email".into(),
                category: "Certificates".into(),
                ty: KeyType::String,
                default: None,
                doc: "ACME contact".into(),
            },
            SchemaEntry {
                key: "acme.directory".into(),
                display_name: "ACME directory (prod vs staging)".into(),
                category: "Certificates".into(),
                ty: KeyType::String,
                default: Some(Value::String("https://example.com".into())),
                doc: "ACME dir".into(),
            },
            SchemaEntry {
                key: "ssh.max_ttl_seconds".into(),
                display_name: "Max cert TTL (seconds)".into(),
                category: "SSH bastion".into(),
                ty: KeyType::Int,
                default: Some(Value::from(2_592_000i64)),
                doc: "TTL ceiling".into(),
            },
        ]
    }

    #[test]
    fn group_by_category_buckets_v01_keys() {
        let entries = v01_schema_entries();
        let groups = group_by_category(&entries);
        // BTreeMap so iteration is alphabetical.
        let names: Vec<&str> = groups.keys().copied().collect();
        assert_eq!(names, vec!["Certificates", "Routing", "SSH bastion"]);
        assert_eq!(groups["Certificates"].len(), 4);
        assert_eq!(groups["Routing"].len(), 1);
        assert_eq!(groups["SSH bastion"].len(), 1);
        // Declaration order inside a category is preserved.
        let certs_keys: Vec<&str> = groups["Certificates"]
            .iter()
            .map(|e| e.key.as_str())
            .collect();
        assert_eq!(
            certs_keys,
            vec![
                "cloudflare.api_token",
                "cloudflare.zone_id",
                "acme.contact_email",
                "acme.directory",
            ]
        );
    }

    #[test]
    fn group_by_category_skips_empty_category() {
        // Defensive: an entry from an older controller with no category
        // serializes as empty string via `#[serde(default)]`. Make sure
        // the menu still draws.
        let entries = vec![SchemaEntry {
            key: "legacy.key".into(),
            display_name: "Legacy".into(),
            category: String::new(),
            ty: KeyType::String,
            default: None,
            doc: "no category".into(),
        }];
        let groups = group_by_category(&entries);
        assert!(groups.is_empty(), "expected legacy key dropped: {groups:?}");
    }

    #[test]
    fn key_label_falls_back_to_dotted_key() {
        let mut entry = v01_schema_entries().remove(0);
        assert_eq!(key_label(&entry), "Cloudflare API token");
        entry.display_name.clear();
        assert_eq!(key_label(&entry), "cloudflare.api_token");
    }

    #[test]
    fn build_category_rows_counts_set_keys() {
        let entries = v01_schema_entries();
        let groups = group_by_category(&entries);
        let list = vec![
            ListRow {
                key: "cloudflare.api_token".into(),
                ty: KeyType::Secret,
                value: Value::String("<redacted>".into()),
                source: "set".into(),
            },
            ListRow {
                key: "acme.directory".into(),
                ty: KeyType::String,
                value: Value::String("https://example.com".into()),
                source: "default".into(),
            },
        ];
        let rows = build_category_rows(&groups, &list);
        // 3 categories + 1 [done] row.
        assert_eq!(rows.len(), 4);
        // Last row is [done].
        assert!(matches!(rows.last().unwrap().kind, CategoryRowKind::Done));
        assert_eq!(rows.last().unwrap().label, "[done]");
        // First (alphabetical) is Certificates with 4 keys, 1 set.
        let certs = &rows[0];
        assert!(
            certs.label.contains("Certificates"),
            "label: {}",
            certs.label
        );
        assert!(certs.label.contains("4 keys"), "label: {}", certs.label);
        assert!(certs.label.contains("1 set"), "label: {}", certs.label);
        match &certs.kind {
            CategoryRowKind::Category(n) => assert_eq!(n, "Certificates"),
            other => panic!("expected Category, got {other:?}"),
        }
        // Routing: 1 key, 0 set.
        let routing = rows
            .iter()
            .find(|r| r.label.starts_with("Routing"))
            .unwrap();
        assert!(routing.label.contains("1 key,"), "label: {}", routing.label);
        assert!(routing.label.contains("0 set"), "label: {}", routing.label);
    }

    #[test]
    fn build_key_rows_renders_back_sentinel_last() {
        let entries = v01_schema_entries();
        let groups = group_by_category(&entries);
        let certs: Vec<&SchemaEntry> = groups["Certificates"].clone();
        let list: Vec<ListRow> = vec![ListRow {
            key: "cloudflare.api_token".into(),
            ty: KeyType::Secret,
            value: Value::String("<redacted>".into()),
            source: "set".into(),
        }];
        let rows = build_key_rows(&certs, &list);
        assert_eq!(rows.len(), 5); // 4 keys + [back]
        assert_eq!(rows.last().unwrap().label, "[back]");
        assert!(matches!(rows.last().unwrap().kind, KeyRowKind::Back));
        // Secret-set rendered as <set>.
        let token = rows
            .iter()
            .find(|r| r.label.contains("Cloudflare API token"))
            .expect("api token row present");
        assert!(token.label.contains("secret"), "label: {}", token.label);
        assert!(token.label.contains("<set>"), "label: {}", token.label);
    }

    #[test]
    fn menu_value_display_redacts_set_secret() {
        let entry = v01_schema_entries().remove(0); // cloudflare.api_token
        let row = ListRow {
            key: "cloudflare.api_token".into(),
            ty: KeyType::Secret,
            value: Value::String("<redacted>".into()),
            source: "set".into(),
        };
        assert_eq!(menu_value_display(&entry, Some(&row)), "<set>");
    }

    #[test]
    fn menu_value_display_unset_secret_no_row() {
        let entry = v01_schema_entries().remove(0);
        assert_eq!(menu_value_display(&entry, None), "<unset>");
    }

    #[test]
    fn menu_value_display_default_marks_default() {
        let entry = v01_schema_entries()
            .into_iter()
            .find(|e| e.key == "acme.directory")
            .unwrap();
        let row = ListRow {
            key: "acme.directory".into(),
            ty: KeyType::String,
            value: Value::String("https://example.com".into()),
            source: "default".into(),
        };
        assert_eq!(
            menu_value_display(&entry, Some(&row)),
            "(default) https://example.com"
        );
    }

    #[test]
    fn menu_value_display_set_string_value() {
        let entry = v01_schema_entries()
            .into_iter()
            .find(|e| e.key == "routing.default_zone")
            .unwrap();
        let row = ListRow {
            key: "routing.default_zone".into(),
            ty: KeyType::String,
            value: Value::String("weavers.engineering".into()),
            source: "set".into(),
        };
        assert_eq!(
            menu_value_display(&entry, Some(&row)),
            "weavers.engineering"
        );
    }

    #[test]
    fn menu_value_display_unset_string_no_default() {
        let entry = v01_schema_entries()
            .into_iter()
            .find(|e| e.key == "routing.default_zone")
            .unwrap();
        let row = ListRow {
            key: "routing.default_zone".into(),
            ty: KeyType::String,
            value: Value::Null,
            source: "unset".into(),
        };
        assert_eq!(menu_value_display(&entry, Some(&row)), "<unset>");
    }
}
