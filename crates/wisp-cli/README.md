# wisp-cli

Operator front-end for the [`wisp`](../wisp/README.md) runtime. See the wisp crate README for the runtime, the dev loop, and the demo. This README is just the subcommand cheat sheet.

State-dir defaults to `/var/lib/wisp`. Override with `--state-dir <PATH>` or the `WISP_STATE_DIR` env var. The CLI needs root for the `run` path (the runtime's create step writes mounts + cgroups).

## Subcommands

| Command | Description |
|---------|-------------|
| `wisp run <BUNDLE> [--id <ID>] [--detach]` | Create + start the bundle and (without `--detach`) wait for PID 1 to exit, then clean up. ID defaults to the bundle's basename. |
| `wisp ps` | List containers in the state-dir as a small table (ID, STATE, PID, AGE). |
| `wisp state <ID>` | Print one container's handle as JSON (state, pid, bundle, created_at). Refreshes `Running -> Stopped` from `/proc`. |
| `wisp kill <ID> [--signal <SIG>]` | Send a signal to PID 1. `--signal` accepts `SIGTERM`, `TERM`, `KILL`, `INT`, etc. Default `SIGTERM`. |
| `wisp delete <ID> [--force]` | Remove the cgroup + state-dir entry. Refuses a `Running` container without `--force`. |

Logs to stderr via `tracing`. Set `WISP_LOG=debug` (or any `EnvFilter` directive) for verbose output. The binary is deliberately synchronous (no tokio): `Runtime::start` calls `clone3` and the process must be single-threaded at that point.
