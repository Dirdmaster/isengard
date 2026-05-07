# Tracing Polish: Operator-Friendly Logs

> **Status:** approved 2026-05-07. Polish PR (no tracked phase). Goal: `docker logs isengard-controller` should be readable at a glance.

## Goal

Make Isengard's stderr trace output look polished in `docker logs` and `journalctl`, without replacing the `tracing` framework.

Today every line is white-on-black with a full RFC3339 timestamp, full module path, and ANSI is silently disabled the moment stdout is not a TTY. That's exactly the case under Docker, which means production operators see the worst version.

## Non-goals

- Replacing `tracing` / `tracing_subscriber` with a different framework.
- Custom formatter implementations: stick to what the crate provides (`compact`, `json`).
- Log shipping, log rotation, or log aggregation. Operators forward stderr themselves.
- Per-module color customization. One palette per level is enough.

## Behavior

| Mode | Trigger | Output |
| --- | --- | --- |
| pretty (default) | none | compact formatter, ANSI on, short HH:MM:SS, trimmed module path |
| no-color | `RUST_LOG_STYLE=never` or `NO_COLOR` set | compact formatter, ANSI off |
| forced color | `RUST_LOG_STYLE=always` (default for non-TTY) | compact formatter, ANSI on even in pipes |
| machine | `LOG_FORMAT=json` | one JSON object per line, ANSI off, full timestamps |

Color choice is decided once at startup. `RUST_LOG_STYLE` overrides everything except `LOG_FORMAT=json` (json is always uncolored).

The `RUST_LOG` filter behavior is unchanged. `ISENGARD_LOG` (the existing CLI flag's env) still wins over `RUST_LOG` if set.

## Ready banner

After init, a single colored banner line goes through `tracing::info!`:

```
isengard 0.1.0-alpha · controller · ready
```

That gives operators an obvious "init finished, real logs follow" marker, includes version, includes mode. Build SHA is omitted: not currently surfaced through any build script, and adding `vergen` is out of scope.

## Module path trim

`isengard_agent::enroll` reads as `agent::enroll`. The compact formatter has `with_target(true)` and a `.fmt_fields(...)` hook, but the cleanest knob is `with_target(false)` plus emitting the trimmed target via the `Format` API. Path trim happens via a tiny custom `MakeWriter`-style helper or, simpler: keep `with_target(true)` and accept the full path. The user spec asks for trim; we'll implement by post-processing the target in a thin formatter wrapper.

## JSON mode

`tracing-subscriber`'s `fmt::Subscriber` ships a built-in JSON formatter behind the `json` feature. We add the feature in workspace deps and gate via `LOG_FORMAT=json`.

## Pretty error exit

`anyhow::Error` chain is dumped on `Result::Err` return from `main`. We add a thin wrapper at the top of `main` that, on `Err`, prints a one-line red `error: <top>` followed by indented `caused by: <next>` lines, then exits 1. No new crate dependency needed: `anyhow::Error::chain()` plus `nu-ansi-term` (already pulled by `tracing-subscriber`'s `ansi` feature) gives the colors.

## Out of scope, on the radar

- `tracing-error` `SpanTrace` integration: nice-to-have for real backtraces, but adds a dep and an `.in_current_span()` discipline. Skip for now; revisit if we hit a debugging case where the span chain matters.
- `vergen` build SHA: defer. Banner stays pkg-version + mode.
