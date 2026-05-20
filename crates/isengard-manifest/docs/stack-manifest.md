A parsed `stack.toml`.

A `stack.toml` lives next to a stack's compose file(s) and carries the
orchestration metadata that compose itself can't express: stack name,
fleet binding, secret name list, deploy strategy, lifecycle hooks.

Fields are validated at parse time. `compose` is non-empty, every compose
path is relative (anchored at [`StackManifest::root`]), `strategy` is one
of the known [`Strategy`] variants, hook events are one of the known
[`HookEvent`] variants. Anything else is a [`ManifestError`].

The struct is plain data: `serde + toml` in, `serde + toml` out via
[`StackManifest::to_toml_string`]. Round-trips, byte-for-byte modulo
field ordering.

# Fields

| Field       | Required | Notes                                              |
|-------------|----------|----------------------------------------------------|
| `name`      | yes      | Stack identity.                                    |
| `compose`   | yes      | Relative paths, non-empty.                         |
| `fleet`     | no       | Defaults from [`FleetManifest`] when absent.       |
| `overlays`  | no       | Named groups of extra compose files.               |
| `strategy`  | no       | Defaults to [`Strategy::Auto`].                    |
| `secrets`   | no       | Secret names mounted into every service.           |
| `hooks`     | no       | Lifecycle hooks ([`HookSpec`]) run on the host.    |
| `root`      | derived  | Parent directory of `stack.toml` on disk.          |
