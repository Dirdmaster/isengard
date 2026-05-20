Join-token format: `TK<base32(32-bytes)>.<base32(sha256(ca_pem))>`.

Used end-to-end:

- `isengard controller token mint` packs `(bytes, ca_fingerprint)` into a
  single string for the operator.
- `isd join-token` prints `isd join --token <packed>` so operators paste
  one line.
- `isengard-agent::enroll` parses the packed token: extracts the
  fingerprint for pre-enroll CA verify, sends the bytes portion to the
  controller's `Enroll` RPC for token validation.

The fingerprint half is what closes the trust loop when the agent fetches
the controller's CA over an unverified channel. The bytes half is the
shared secret the controller validates against its mint records.
