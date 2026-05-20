SQLite-backed persistence for the Isengard controller.

The crate owns one `.db` file per controller and migrates it forward on
open. Higher crates (`isengard-controller`, `isengard-agent`, plugins) do
not own SQL: they call into [`Inventory`] and [`Journal`] and trade in
the typed rows defined here.

# The two facades

[`Inventory`] is the wide CRUD surface. It wraps a `sqlx::SqlitePool` and
exposes one DAO per entity (hosts, services, stacks, routing rules,
deployments, policies, secrets, certs, webhooks, placements, container
hooks, host actions, settings, the singleton CA, the ACME account, the
per-host adapter config, the agent leaf certs).

[`Journal`] is the append-only event log. The same pool migrates the
same file: in production the controller opens both against the same
path so an `Inventory::pool()` borrow and a `Journal::insert` write into
the same WAL.

# Identifiers

[`HostId`] is a 16-byte ULID stored as BLOB. Every entity that references
a host carries `host_id BLOB(16)` and decodes through
[`HostId::from_db_bytes`]. Other entities mint surrogate keys
([`StackId`], [`ServiceId`], [`RoutingRuleId`]) as autoincrement
integers because no natural key exists.

[`PendingApprovalRow::action_id`] is a ULID rendered as text. Approval
rows live in `host_actions` alongside agent-pull actions but their
external id is the string ULID, not the integer `host_actions.id`.

# Migrations

The numbered `migrations/<NNNN>_<name>.sql` files are frozen history.
`sqlx::migrate!()` plays them on every `Inventory::open` (and `Journal::open`),
which is why the two facades share a pool: the migrator is idempotent
and one file holds both schemas. The DAO module owning a table's first
appearance documents the migration that introduced it (see
[`containers`] for `0029`, [`secret`] for `0025`, [`webhook`] for
`0020`+, [`placements`] for `0030`).

# Encryption surface

The DAO trades in raw ciphertext bytes for [`SecretMeta`]. The
controller crate owns the master-key file and the ChaCha20-Poly1305
seal; the storage layer never touches plaintext and never writes
plaintext through any list endpoint. [`CaRow`] and [`AgentCert`] hold
PEM material directly: filesystem permissions on the SQLite file
(chmod 600 in `state-dir`) are the only protection.
