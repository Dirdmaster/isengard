Lifecycle state for a service as observed by an agent's heartbeat.

v0.5.3 extended this enum beyond `Running` / `Stopped` / `Restarting` /
`Unknown` so mid-startup states surface correctly in `isd ps` and
`isd deploy --watch` instead of collapsing to `Unknown`. Specifically,
wisp's `ContainerState::Created` (bundle staged + cgroup ready,
process not yet forked) used to map to `Unknown`; now it lands on
`Creating`.

# Wire format and back-compat

The on-disk representation is the lowercase variant name (`as_str`).
Old binaries reading new strings fall through `from_str`'s default arm
and decode as `Unknown` (forward-compatible by construction). New
binaries reading old `"unknown"` rows preserve them as `Unknown`.

`from_str` also accepts the docker-compatible aliases
(`created`, `exited`, `dead`) that pre-extension agents emit, so the
controller can ingest heartbeats from older agents without falsely
showing `Unknown`.

SQLite column type is TEXT, so no schema migration is needed.
