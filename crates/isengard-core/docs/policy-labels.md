Parse `isengard.policy.*` Docker labels into a [`super::Policy`] struct.

See spec §"Label parser" of
Container labels are the bridge between Compose services and policy records.

Pure module: `HashMap<String, String>` in, [`super::Policy`] out (or
[`ParseLabelError`] on a malformed value). The ingest caller decides what
to do with errors (logs at warn and skips the upsert).

The five recognized labels match the [`super::Policy`] struct fields
one-to-one:

| Label                              | Field             | Values                                |
|------------------------------------|-------------------|---------------------------------------|
| `isengard.policy.strategy`         | `strategy`        | `pinned` / `tag-only` / `minor` / `any` |
| `isengard.policy.gate`             | `gate`            | `auto` / `approval` / `never`         |
| `isengard.policy.paused_until`     | `paused_until`    | RFC 3339 timestamp                    |
| `isengard.policy.on_failure`       | `on_failure`      | `rollback` / `keep` / `notify`        |
| `isengard.policy.approver_channel` | `approver_channel`| free-form string (notifier channel id) |

Enum values accept both kebab-case and snake_case for ergonomics; the
canonical form is kebab-case (matches [`super::Policy`] serde).
