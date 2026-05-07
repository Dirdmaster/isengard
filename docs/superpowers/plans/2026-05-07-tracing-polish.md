# Tracing Polish: Implementation Plan

> Spec: [`2026-05-07-tracing-polish-design.md`](../specs/2026-05-07-tracing-polish-design.md). Single-file change, polish PR, no phase tag.

## Scope

Replace the inline `tracing_subscriber::fmt()` block in `crates/isengard/src/main.rs` with a small `tracing_init` module that:

- decides ANSI based on `RUST_LOG_STYLE` / `NO_COLOR` / TTY (in that order)
- supports `LOG_FORMAT=json` for log-aggregator mode
- uses the `compact` formatter with HH:MM:SS timestamps
- prints a colored ready banner
- installs a pretty error-chain printer for `anyhow::Error` returns from `main`

## Files touched

| File | Change |
| --- | --- |
| `Cargo.toml` (workspace) | enable `json` and `chrono` features on `tracing-subscriber` |
| `crates/isengard/src/main.rs` | factor tracing init out, add banner + pretty exit |
| `crates/isengard/src/tracing_init.rs` | new, ~80 lines |
| `docs/RELEASE_NOTES_TRACING_POLISH.md` | new release note |

No protocol or behavior changes outside log formatting.

## Steps

1. Workspace `Cargo.toml`: extend `tracing-subscriber` features to `["env-filter", "fmt", "json", "chrono"]`. The `chrono` feature lets the compact formatter emit a custom HH:MM:SS time without pulling `time` everywhere.
2. New file `crates/isengard/src/tracing_init.rs`:
   - `pub enum LogFormat { Pretty, Json }` decided from `LOG_FORMAT` env.
   - `pub fn ansi_enabled() -> bool`: reads `NO_COLOR`, `RUST_LOG_STYLE`, falls back to `stderr.is_terminal()`. Default for non-TTY when unset is `true` (Docker case).
   - `pub fn init(mode: &str)`: builds `EnvFilter` from `ISENGARD_LOG`/`RUST_LOG`, applies one of two formatter chains, prints the ready banner.
   - `pub fn print_error_chain(err: &anyhow::Error)`: red `error:` head, dim `caused by:` chain.
3. `main.rs` becomes:
   ```rust
   let mode = match &cli.command { Controller {..} => "controller", Agent {..} => "agent" };
   tracing_init::init(mode, cli.log.as_deref());
   if let Err(err) = run(cli).await { tracing_init::print_error_chain(&err); std::process::exit(1); }
   ```
4. Release note: 1-paragraph summary + before/after lines.

## Module path trim

Implement via `with_target(true)` on the compact formatter and accept "isengard_agent" in target text. The actual visual trim ("agent::enroll") is achieved by reading the target in a `FormatEvent` impl: too invasive. Instead, document that the prefix `isengard_` is stripped via a custom `Format` wrapper that delegates to the inner `Compact` formatter and rewrites the target string. Implementation uses `tracing_subscriber::fmt::format::Writer` and a thin `FormatEvent` newtype.

If implementing the wrapper turns out non-trivial against the closed `Compact` type, fall back to `with_target(true)` raw and skip the trim. Trim is nice-to-have.

## Validation

- `cargo build --workspace` succeeds.
- `cargo nextest run --workspace` (or `cargo test`) succeeds.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo fmt --check` clean.
- `cargo deny check` clean (no new advisories from the json feature).
- Manual: run `cargo run -p isengard -- controller --listen 127.0.0.1:9417 --state-dir /tmp/isengard-smoke` from a TTY, verify colored banner.
- Manual: pipe to `cat` (`... | cat`) without env vars, verify colors persist.
- Manual: with `RUST_LOG_STYLE=never`, verify no ANSI escapes.
- Manual: with `LOG_FORMAT=json`, verify each line parses as JSON via `... | jq -c '.'`.

## Risks

- New `chrono` feature on `tracing-subscriber`: workspace already depends on `chrono` separately, so no version conflict expected.
- Forced ANSI breaking a downstream pipe: mitigated by `RUST_LOG_STYLE=never` and `NO_COLOR`.
- Banner emitted before subscriber set: explicitly emitted after init.
