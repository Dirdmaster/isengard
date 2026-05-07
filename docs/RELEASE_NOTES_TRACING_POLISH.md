# Tracing Polish

Operator-facing logs got a once-over. Same `tracing` framework, same `RUST_LOG` filter behavior — just dramatically nicer to read in `docker logs` and `journalctl`.

## What changed

- Compact formatter with short `HH:MM:SS` timestamps instead of full RFC3339.
- Levels are colored: blue `INFO`, yellow `WARN`, red `ERROR`. Span paths show in bold, target paths in dim.
- ANSI is now on by default even when stderr is not a TTY (the Docker case). Opt out with `RUST_LOG_STYLE=never` or `NO_COLOR`.
- `LOG_FORMAT=json` switches to JSON-lines output for log aggregators (Loki, Vector, Datadog).
- New colored ready banner: `isengard 0.1.0-alpha controller ready` after init, so restarts are obvious.
- Errors returned from `main` now print as a colored chain instead of an `anyhow` debug dump.

## Env vars

| Var | Values | Effect |
| --- | --- | --- |
| `RUST_LOG` | filter syntax | unchanged: standard `tracing` filter |
| `ISENGARD_LOG` | filter syntax | unchanged: CLI-flag form, wins over `RUST_LOG` |
| `RUST_LOG_STYLE` | `always`, `auto`, `never` | force / disable ANSI |
| `NO_COLOR` | any | disables ANSI (https://no-color.org) |
| `LOG_FORMAT` | `json` | switch to JSON lines |

## Before / after

Before:
```
2026-05-06T14:01:02.123456Z  INFO isengard: controller mode listen_addr=0.0.0.0:9417 state_dir="/var/lib/isengard"
2026-05-06T14:01:02.301245Z  INFO run_controller: isengard_controller: starting controller listen=0.0.0.0:9417
```

After (pretty):
```
isengard 0.1.0-alpha controller ready
14:01:02  INFO isengard: controller mode listen_addr=0.0.0.0:9417 state_dir="/var/lib/isengard"
14:01:02  INFO run_controller: isengard_controller: starting controller listen=0.0.0.0:9417
```

After (`LOG_FORMAT=json`):
```
{"timestamp":"2026-05-06T14:01:02.123456Z","level":"INFO","fields":{"message":"isengard ready","version":"0.1.0-alpha","mode":"controller"},"target":"isengard::tracing_init"}
{"timestamp":"2026-05-06T14:01:02.301245Z","level":"INFO","fields":{"message":"controller mode","listen_addr":"0.0.0.0:9417","state_dir":"\"/var/lib/isengard\""},"target":"isengard"}
```

## Risk

Cosmetic only. No protocol, storage, or scheduler changes. Filter syntax and event payloads are unchanged.
