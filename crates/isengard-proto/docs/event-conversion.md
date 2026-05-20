Converts an `isengard_core::Event` to its proto twin.

The wire shape mirrors the Rust struct one field at a time. Two fields
need translation:

- `occurred_at`: the Rust side is a `chrono::DateTime<Utc>`; the wire
  carries RFC3339 text. The conversion uses `to_rfc3339`, lossless
  round-trip.
- `metadata`: the Rust side is a `serde_json::Value`. A null value
  serializes to absent on the wire (`metadata_json = None`); anything
  else serializes to `Some(string)` via `Value::to_string`.

The reverse conversion ([`TryFrom<ProtoEvent>`]) parses both back.

The `host_id` field on `isengard_core::Event` is intentionally not on
the wire. The controller stamps it from the connection's mTLS identity
on receipt, which means an agent cannot lie about the host it speaks
for.

# Errors

The forward direction is infallible. The reverse direction (`TryFrom`)
returns `Err(String)` when `occurred_at` fails RFC3339 parsing or when
`metadata_json` is set but does not parse as JSON.
