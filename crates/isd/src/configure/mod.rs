//! `isd configure` operator surface (controller-wide configuration).
//!
//! Bare `isd configure` (no sub-verb) opens a full ratatui TUI on the
//! alternate screen with a two-pane layout (categories left, keys right)
//! and a modal editor on Enter. `isd configure setup` is an explicit
//! alias. The TUI restores the terminal cleanly on exit and panics.
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
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::render::{Align, CellStyle, Column, Table, render, render_plain};
use crate::session::Session;

mod tui;

/// Type of a schema entry. Mirrors the controller's `KeyType` over the
/// wire: serialised as a lower-snake-case string.
///
/// Defined locally so the `isd` crate does not need to depend on
/// `isengard-controller` (which pulls in sqlx, the docker plugins, the
/// secrets store, etc.). The wire format is the source of truth; this
/// enum is just the deserialisation target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum KeyType {
    /// Plain UTF-8 string. Lands in the `settings` table.
    String,
    /// String persisted to the encrypted secrets store. The CLI refuses
    /// inline values for these.
    Secret,
    /// Signed integer.
    Int,
    /// Boolean.
    Bool,
    /// Ordered list of non-empty UTF-8 strings.
    ///
    /// Persisted on the wire as a JSON array. Reserved for future
    /// operator-managed multi-value keys that do not need per-row
    /// metadata; the CLI `set` verb accepts a comma-separated string.
    StringList,
    /// Ordered list of zone entries: `{ name, wildcard }` per element.
    ///
    /// Persisted on the wire as a JSON array of objects. The TUI renders
    /// one row per zone with a wildcard toggle; the `set` verb accepts
    /// a comma-separated list of names where each name may be suffixed
    /// with `*` to mark the wildcard intent
    /// (e.g. `weavers.engineering*,vallee.casa`).
    ZoneList,
}

/// CLI flags for `isd configure`.
///
/// `command` is optional: bare `isd configure` dispatches to the
/// ratatui TUI ([`tui::run`]). Same pattern as `SshArgs`
/// from PR #237 (picker-by-default).
#[derive(Debug, Args)]
pub struct ConfigureArgs {
    /// Resolved sub-verb. `None` means bare `isd configure`: open the
    /// interactive menu.
    #[command(subcommand)]
    pub command: Option<ConfigureCommand>,
}

/// Sub-verbs under `isd configure`. Canonical verbs follow the lexicon
/// spec (`3 Resources/Superpowers/specs/2026-05-22-isd-cli-lexicon-design.md`):
/// `get` / `set` / `rm` / `ls` / `schema`. `unset` and `list` are kept
/// as deprecated aliases for one minor version.
#[derive(Debug, Subcommand)]
pub enum ConfigureCommand {
    /// Print one key's current value.
    Get(GetArgs),
    /// Set a key. Use the inline positional for plain types; pass
    /// `--stdin` or `--from-file` for secrets.
    Set(SetArgs),
    /// Remove a key. Falls back to the schema default if one exists.
    #[command(alias = "unset")]
    Rm(RmArgs),
    /// Print every key with its current value (secrets redacted).
    #[command(alias = "list")]
    Ls(LsArgs),
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
pub struct RmArgs {
    /// Schema key.
    pub key: String,
}

/// CLI flags for `isd configure list`.
#[derive(Debug, Args)]
pub struct LsArgs {
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
#[derive(Debug, Deserialize, Clone)]
pub(crate) struct ListRow {
    /// Schema key.
    pub(crate) key: String,
    /// Schema-declared type.
    #[serde(rename = "type")]
    pub(crate) ty: KeyType,
    /// Current value: stored, default, redacted, or null.
    pub(crate) value: Value,
    /// `"set"`, `"default"`, or `"unset"`.
    pub(crate) source: String,
}

/// JSON shape for one row in `GET /api/v1/config/schema`.
#[derive(Debug, Deserialize, Clone)]
pub(crate) struct SchemaEntry {
    /// Schema key.
    pub(crate) key: String,
    /// Operator-facing label used in menu rows and prompts.
    #[serde(default)]
    pub(crate) display_name: String,
    /// Top-level menu bucket: `Certificates`, `Routing`, `SSH bastion`.
    #[serde(default)]
    pub(crate) category: String,
    /// Schema-declared type.
    #[serde(rename = "type")]
    pub(crate) ty: KeyType,
    /// Optional default value.
    pub(crate) default: Option<Value>,
    /// One-line description.
    pub(crate) doc: String,
    /// Optional fixed choice set. When present, the TUI shows a select
    /// widget instead of a free-text input regardless of [`Self::ty`].
    #[serde(default)]
    pub(crate) choices: Option<Vec<Choice>>,
}

/// One option in a fixed [`SchemaEntry::choices`] set.
#[derive(Debug, Deserialize, Clone)]
pub(crate) struct Choice {
    /// Wire value persisted when this choice is selected.
    pub(crate) value: String,
    /// Operator-facing label.
    pub(crate) label: String,
}

/// One row in a [`KeyType::ZoneList`] value.
///
/// Mirrors the controller's `Zone` over the wire (`{ name, wildcard }`).
/// Declared locally so the `isd` crate stays clean of the controller's
/// sqlx / secrets dependencies; the wire shape is the source of truth.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Zone {
    /// Apex zone name (e.g. `weavers.engineering`).
    pub(crate) name: String,
    /// Operator intent: request wildcard cert for this zone.
    #[serde(default)]
    pub(crate) wildcard: bool,
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
        None | Some(ConfigureCommand::Setup) => {
            let updated = tui::run(&session, &base).await?;
            if updated > 0 {
                let s = if updated == 1 { "" } else { "s" };
                println!("isd configure: {updated} key{s} updated.");
            }
            Ok(())
        }
        Some(ConfigureCommand::Get(a)) => run_get(&session, &base, a).await,
        Some(ConfigureCommand::Set(a)) => run_set(&session, &base, a).await,
        Some(ConfigureCommand::Rm(a)) => run_rm(&session, &base, a).await,
        Some(ConfigureCommand::Ls(a)) => run_ls(&session, &base, a).await,
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
pub(crate) async fn put_value(
    session: &Session,
    base: &str,
    key: &str,
    value: Value,
) -> Result<()> {
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
        Some(KeyType::StringList) => {
            // Inline values for list-typed keys come from a CLI argument,
            // so accept the conventional comma-separated form. Empty
            // tokens are dropped so trailing commas do not crash the
            // controller's element-non-empty check.
            let items: Vec<Value> = raw
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_string()))
                .collect();
            Ok(Value::Array(items))
        }
        Some(KeyType::ZoneList) => {
            // Comma-separated zone names. A trailing `*` on a name marks
            // the wildcard intent: `weavers.engineering*,vallee.casa`
            // means wildcard=true for the first, false for the second.
            // The TUI is the richer surface; this is a minimal inline
            // shape so `isd configure set routing.zones ...` still works.
            let items: Vec<Value> = raw
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| {
                    let (name, wildcard) = if let Some(stripped) = s.strip_suffix('*') {
                        (stripped.trim().to_string(), true)
                    } else {
                        (s.to_string(), false)
                    };
                    serde_json::json!({
                        "name": name,
                        "wildcard": wildcard,
                    })
                })
                .collect();
            Ok(Value::Array(items))
        }
        _ => Ok(Value::String(raw.to_string())),
    }
}

/// `DELETE /api/v1/config/{key}` then render the outcome.
async fn run_rm(session: &Session, base: &str, args: RmArgs) -> Result<()> {
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
            "Removed {} (will fall back to default: {})",
            args.key,
            json_to_display(&default)
        ),
        None => println!("Removed {}", args.key),
    }
    Ok(())
}

/// `GET /api/v1/config` then render the snapshot as a table.
async fn run_ls(session: &Session, base: &str, args: LsArgs) -> Result<()> {
    let rows = fetch_list(session, base, args.show_secrets).await?;
    if rows.is_empty() {
        println!("No config keys.");
        return Ok(());
    }
    print_list_table(&rows);
    Ok(())
}

/// `GET /api/v1/config` shared helper. `list` and the wizard both need
/// the current snapshot keyed by `key`.
pub(crate) async fn fetch_list(
    session: &Session,
    base: &str,
    show_secrets: bool,
) -> Result<Vec<ListRow>> {
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

/// Column layout for `isd configure list`. Columns in spec order:
/// `KEY`, `TYPE`, `VALUE`, `SOURCE`.
fn list_columns() -> Vec<Column> {
    vec![
        Column::new("KEY", Align::Left, CellStyle::Emphasis, 7, 12),
        Column::new("TYPE", Align::Left, CellStyle::Plain, 4, 6),
        Column::new("VALUE", Align::Left, CellStyle::Plain, 1, 12),
        Column::new("SOURCE", Align::Left, CellStyle::Dim, 5, 6),
    ]
}

/// Build the row matrix for `isd configure list`.
fn build_list_row_cells(rows: &[ListRow]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| {
            vec![
                r.key.clone(),
                key_type_label(r.ty).to_string(),
                json_to_display(&r.value),
                r.source.clone(),
            ]
        })
        .collect()
}

/// Render `isd configure list` to stdout: boxed table on a TTY, tab-
/// separated plain text on a pipe.
fn print_list_table(rows: &[ListRow]) {
    let table = Table {
        columns: list_columns(),
        rows: build_list_row_cells(rows),
    };
    let term = console::Term::stdout();
    if term.is_term() {
        let width = term.size().1 as usize;
        let color = console::colors_enabled();
        println!("{}", render(&table, width, color));
    } else {
        println!("{}", render_plain(&table));
    }
}

/// `GET /api/v1/config/schema` then render `KEY / TYPE / DEFAULT / DESCRIPTION`.
async fn run_schema(session: &Session, base: &str) -> Result<()> {
    let entries = fetch_schema(session, base).await?;
    print_schema_table(&entries);
    Ok(())
}

/// Column layout for `isd configure schema`. Columns in spec order:
/// `KEY`, `TYPE`, `DEFAULT`, `DESCRIPTION`. DESCRIPTION is the longest
/// column and the safest to truncate when the terminal is narrow.
fn schema_columns() -> Vec<Column> {
    vec![
        Column::new("KEY", Align::Left, CellStyle::Emphasis, 7, 12),
        Column::new("TYPE", Align::Left, CellStyle::Plain, 5, 6),
        Column::new("DEFAULT", Align::Left, CellStyle::Plain, 4, 8),
        Column::new("DESCRIPTION", Align::Left, CellStyle::Plain, 1, 16),
    ]
}

/// Build the row matrix for `isd configure schema`. A missing default
/// renders as the literal `(none)` so the column never collapses.
fn build_schema_row_cells(entries: &[SchemaEntry]) -> Vec<Vec<String>> {
    entries
        .iter()
        .map(|e| {
            let default = match &e.default {
                Some(v) => json_to_display(v),
                None => "(none)".to_string(),
            };
            vec![
                e.key.clone(),
                key_type_label(e.ty).to_string(),
                default,
                e.doc.clone(),
            ]
        })
        .collect()
}

/// Render `isd configure schema` to stdout: boxed table on a TTY,
/// tab-separated plain text on a pipe.
fn print_schema_table(entries: &[SchemaEntry]) {
    let table = Table {
        columns: schema_columns(),
        rows: build_schema_row_cells(entries),
    };
    let term = console::Term::stdout();
    if term.is_term() {
        let width = term.size().1 as usize;
        let color = console::colors_enabled();
        println!("{}", render(&table, width, color));
    } else {
        println!("{}", render_plain(&table));
    }
}

/// `POST /api/v1/config/zones/cloudflare-fetch` shared helper. Returns
/// the zone names the dashboard's Cloudflare client found for the
/// configured token. The TUI calls this when the operator presses `F`
/// inside the `routing.zones` editor.
pub(crate) async fn fetch_zones_from_cloudflare(
    session: &Session,
    base: &str,
) -> Result<Vec<String>> {
    let url = format!("{base}/api/v1/config/zones/cloudflare-fetch");
    let resp = session
        .client
        .post(&url)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "cloudflare fetch: {}",
            parse_error_message(&body, &body)
        ));
    }
    let body: ZoneFetchResponse = resp
        .json()
        .await
        .context("decoding cloudflare-fetch body")?;
    Ok(body.zones)
}

/// JSON shape returned by `POST /api/v1/config/zones/cloudflare-fetch`.
#[derive(Debug, Deserialize)]
struct ZoneFetchResponse {
    /// Zone names the controller discovered via the Cloudflare API.
    zones: Vec<String>,
}

/// `GET /api/v1/config/schema` shared helper. `set` and `unset` call this
/// to drive their client-side guards and default-fallback messaging.
pub(crate) async fn fetch_schema(session: &Session, base: &str) -> Result<Vec<SchemaEntry>> {
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

/// Group schema entries by category. Alphabetical order on the outer
/// keys via `BTreeMap`. Inside a category we preserve schema declaration
/// order so related keys (e.g. `cloudflare.api_token`,
/// `cloudflare.zone_id`) stay adjacent.
///
/// Pulled out so the grouping behavior is unit-testable without
/// spinning up a fake controller.
pub(crate) fn group_by_category<'a>(
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
pub(crate) fn key_label(entry: &SchemaEntry) -> String {
    if entry.display_name.is_empty() {
        entry.key.clone()
    } else {
        entry.display_name.clone()
    }
}

/// Render the value column for an inner key row. Secrets become
/// `<set>` / `<unset>`; defaults get the `(default)` marker; plain
/// values render via [`json_to_display`].
pub(crate) fn menu_value_display(entry: &SchemaEntry, current: Option<&ListRow>) -> String {
    let source = current.map(|r| r.source.as_str()).unwrap_or("unset");
    match (entry.ty, source) {
        (KeyType::Secret, "set") => "<set>".to_string(),
        (KeyType::Secret, _) => "<unset>".to_string(),
        (_, "unset") => "<unset>".to_string(),
        _ => match current.map(|r| &r.value) {
            Some(v) => {
                let rendered = if entry.ty == KeyType::ZoneList {
                    zone_list_display(v)
                } else {
                    json_to_display(v)
                };
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

/// Render a [`KeyType::ZoneList`] JSON value as a compact, operator-readable
/// line: `weavers.engineering*, vallee.casa` where `*` marks the wildcard
/// intent. Falls back to [`json_to_display`] when the shape drifts.
pub(crate) fn zone_list_display(v: &Value) -> String {
    let Some(items) = v.as_array() else {
        return json_to_display(v);
    };
    if items.is_empty() {
        return "(empty)".to_string();
    }
    let parts: Vec<String> = items
        .iter()
        .map(|item| {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("(?)");
            let wildcard = item
                .get("wildcard")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if wildcard {
                format!("{name}*")
            } else {
                name.to_string()
            }
        })
        .collect();
    parts.join(", ")
}

/// Lower-snake-case label for a [`KeyType`]. Used in table rendering.
pub(crate) fn key_type_label(ty: KeyType) -> &'static str {
    match ty {
        KeyType::String => "string",
        KeyType::Secret => "secret",
        KeyType::Int => "int",
        KeyType::Bool => "bool",
        KeyType::StringList => "string_list",
        KeyType::ZoneList => "zone_list",
    }
}

/// Render a JSON value for terminal display. Strings drop their quotes
/// so `acme.directory` prints as `https://...` (not `"https://..."`).
/// `null` renders as `(unset)` so list rows stay readable.
pub(crate) fn json_to_display(v: &Value) -> String {
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
            ConfigureCommand::Rm(a) => assert_eq!(a.key, "acme.directory"),
            other => panic!("expected Rm, got {other:?}"),
        }
    }

    #[test]
    fn configure_list_parses_default() {
        let w = Wrap::try_parse_from(["x", "list"]).unwrap();
        match w.c {
            ConfigureCommand::Ls(a) => assert!(!a.show_secrets),
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn configure_list_with_show_secrets_parses() {
        let w = Wrap::try_parse_from(["x", "list", "--show-secrets"]).unwrap();
        match w.c {
            ConfigureCommand::Ls(a) => assert!(a.show_secrets),
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
    fn encode_value_parses_comma_separated_for_string_list() {
        let v = encode_value(Some(KeyType::StringList), "a.com, b.com,c.com").unwrap();
        assert_eq!(
            v,
            Value::Array(vec![
                Value::String("a.com".into()),
                Value::String("b.com".into()),
                Value::String("c.com".into()),
            ])
        );
    }

    #[test]
    fn encode_value_drops_empty_tokens_for_string_list() {
        let v = encode_value(Some(KeyType::StringList), "a.com,,b.com,").unwrap();
        assert_eq!(
            v,
            Value::Array(vec![
                Value::String("a.com".into()),
                Value::String("b.com".into()),
            ])
        );
    }

    #[test]
    fn key_type_label_renders_string_list() {
        assert_eq!(key_type_label(KeyType::StringList), "string_list");
    }

    #[test]
    fn key_type_label_renders_zone_list() {
        assert_eq!(key_type_label(KeyType::ZoneList), "zone_list");
    }

    #[test]
    fn encode_value_parses_comma_separated_for_zone_list() {
        let v = encode_value(
            Some(KeyType::ZoneList),
            "weavers.engineering*, vallee.casa,another.dev*",
        )
        .unwrap();
        assert_eq!(
            v,
            Value::Array(vec![
                serde_json::json!({"name": "weavers.engineering", "wildcard": true}),
                serde_json::json!({"name": "vallee.casa", "wildcard": false}),
                serde_json::json!({"name": "another.dev", "wildcard": true}),
            ])
        );
    }

    #[test]
    fn encode_value_drops_empty_tokens_for_zone_list() {
        let v = encode_value(Some(KeyType::ZoneList), "a.com,,b.com*,").unwrap();
        assert_eq!(
            v,
            Value::Array(vec![
                serde_json::json!({"name": "a.com", "wildcard": false}),
                serde_json::json!({"name": "b.com", "wildcard": true}),
            ])
        );
    }

    #[test]
    fn zone_list_display_marks_wildcard_entries() {
        let v = serde_json::json!([
            {"name": "weavers.engineering", "wildcard": true},
            {"name": "vallee.casa", "wildcard": false},
        ]);
        assert_eq!(zone_list_display(&v), "weavers.engineering*, vallee.casa");
    }

    #[test]
    fn zone_list_display_handles_empty_array() {
        let v = serde_json::json!([]);
        assert_eq!(zone_list_display(&v), "(empty)");
    }

    #[test]
    fn zone_list_display_falls_back_for_non_array() {
        let v = serde_json::json!("scalar");
        assert_eq!(zone_list_display(&v), "scalar");
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

    /// `isd configure list` renders through the unified boxed renderer.
    /// The non-TTY decay path emits ALL CAPS headers in spec order and
    /// every row's text.
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
        let table = Table {
            columns: list_columns(),
            rows: build_list_row_cells(&rows),
        };
        let plain = render_plain(&table);
        let header = plain.lines().next().unwrap();
        assert_eq!(header, "KEY\tTYPE\tVALUE\tSOURCE");
        assert!(plain.contains("acme.directory"), "plain: {plain}");
        assert!(plain.contains("https://example.com"), "plain: {plain}");
        assert!(plain.contains("<redacted>"), "plain: {plain}");
        // Boxed render carries the rounded-corner glyphs.
        let boxed = render(&table, 200, false);
        assert!(boxed.contains('╭'));
        assert!(boxed.contains("KEY"));
    }

    /// Empty input still renders the spec headers so pipeline consumers
    /// (`cut -f`) keep a stable shape.
    #[test]
    fn render_list_table_renders_header_on_empty_input() {
        let table = Table {
            columns: list_columns(),
            rows: build_list_row_cells(&[]),
        };
        let plain = render_plain(&table);
        assert_eq!(plain.lines().next().unwrap(), "KEY\tTYPE\tVALUE\tSOURCE");
    }

    /// `isd configure schema` renders through the unified boxed
    /// renderer; missing defaults fall back to `(none)`.
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
                choices: None,
            },
            SchemaEntry {
                key: "acme.directory".into(),
                display_name: "ACME directory".into(),
                category: "Certificates".into(),
                ty: KeyType::String,
                default: Some(Value::String("https://example.com".into())),
                doc: "ACME dir".into(),
                choices: None,
            },
        ];
        let table = Table {
            columns: schema_columns(),
            rows: build_schema_row_cells(&entries),
        };
        let plain = render_plain(&table);
        let header = plain.lines().next().unwrap();
        assert_eq!(header, "KEY\tTYPE\tDEFAULT\tDESCRIPTION");
        assert!(plain.contains("(none)"), "plain: {plain}");
        assert!(plain.contains("https://example.com"), "plain: {plain}");
        // Boxed render carries the rounded-corner glyphs.
        let boxed = render(&table, 200, false);
        assert!(boxed.contains('╭'));
    }

    /// Empty schema still renders the spec headers.
    #[test]
    fn render_schema_table_renders_header_on_empty_input() {
        let table = Table {
            columns: schema_columns(),
            rows: build_schema_row_cells(&[]),
        };
        let plain = render_plain(&table);
        assert_eq!(
            plain.lines().next().unwrap(),
            "KEY\tTYPE\tDEFAULT\tDESCRIPTION"
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
                choices: None,
            },
            SchemaEntry {
                key: "cloudflare.zone_id".into(),
                display_name: "Cloudflare zone ID".into(),
                category: "Certificates".into(),
                ty: KeyType::String,
                default: None,
                doc: "CF zone".into(),
                choices: None,
            },
            SchemaEntry {
                key: "routing.default_zone".into(),
                display_name: "Default zone".into(),
                category: "Routing".into(),
                ty: KeyType::String,
                default: None,
                doc: "Default routing zone".into(),
                choices: None,
            },
            SchemaEntry {
                key: "acme.contact_email".into(),
                display_name: "Contact email".into(),
                category: "Certificates".into(),
                ty: KeyType::String,
                default: None,
                doc: "ACME contact".into(),
                choices: None,
            },
            SchemaEntry {
                key: "acme.directory".into(),
                display_name: "ACME directory (prod vs staging)".into(),
                category: "Certificates".into(),
                ty: KeyType::String,
                default: Some(Value::String("https://example.com".into())),
                doc: "ACME dir".into(),
                choices: Some(vec![
                    Choice {
                        value: "https://acme-v02.api.letsencrypt.org/directory".into(),
                        label: "Let's Encrypt production".into(),
                    },
                    Choice {
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
                default: Some(Value::from(2_592_000i64)),
                doc: "TTL ceiling".into(),
                choices: None,
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
            choices: None,
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
