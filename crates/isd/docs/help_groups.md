Subcommand groups rendered by `isd --help`.

clap has no native multi-group subcommand help (issue clap-rs/clap#1553
is open; PRs #5816 closed and #5819 still open as of clap 4.6.1). The
root `--help` renders through this table instead so the operator sees
a categorised list instead of a flat 20-entry blob.

Every top-level subcommand must appear in exactly one group. A unit
test in `help_render.rs` (`every_subcommand_is_in_exactly_one_group`)
walks clap's introspection and fails CI if a new variant lands in
`main::Command` without being slotted here, or if a stale entry refers
to a removed subcommand.

Group order is the operator's mental order: the verbs they reach for
first sit at the top.

| Group | Members |
|---|---|
| Containers | `ps`, `logs`, `stop`, `start`, `restart`, `rm`, `kill` |
| Stacks | `stack` (with sub-verbs `ls`, `ps`, `deploy`, `diff`, `edit`, `manifest`), `open` |
| Cluster | `hosts`, `service`, `route`, `secret`, `placement`, `join`, `join-token` |
| Setup | `init`, `uninit`, `upgrade`, `context`, `update` |
| Backup | `backup`, `restore` |
| Editor | `lsp`, `mcp` |

Sub-verbs inside `stack`, `secret`, `route`, `placement`, `hosts`,
`service`, `context` keep clap's stock auto-help. Only the root is
custom.
