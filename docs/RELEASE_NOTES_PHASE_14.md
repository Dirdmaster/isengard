# Phase 14: Auth & Identity (BREAKING CHANGE)

The shared `ISENGARD_TOKEN` bearer secret is gone. Auth now uses an internal CA + per-agent mTLS + short-lived enrollment tokens.

> **2026-05-06 update:** `controller token mint --role agent` now prints a Docker Swarm-style `docker run` join command with the token + CA pre-baked in. Steps 6 and 7 below are no longer needed for typical setups; see [`RELEASE_NOTES_ENROLLMENT_UX.md`](./RELEASE_NOTES_ENROLLMENT_UX.md). The recipe below still works as the advanced fallback.

## Migration

There is no in-place migration. To upgrade:

1. Stop the controller and all agents.
2. Wipe state-dir on the controller (`/var/lib/isengard`).
3. Wipe state-dir on every agent (`/var/lib/isengard`).
4. Drop `ISENGARD_TOKEN` from your env / compose / docker run.
5. Start the controller (no env var changes; on first boot it generates a CA).
6. Run `isengard controller ca export > ca.pem` and copy `ca.pem` to each agent host.
7. For each agent: `isengard controller token mint`, then `docker run -e ISENGARD_ENROLL_TOKEN=... -e ISENGARD_CONTROLLER_CA_PEM_PATH=/etc/isengard/ca.pem -v ./ca.pem:/etc/isengard/ca.pem ...`

(Or, recommended since 2026-05-06: `docker exec controller isengard controller token mint --role agent` and paste the printed `docker run` block on the agent host. CA is inlined as `ISENGARD_CONTROLLER_CA_PEM_BASE64`. No side file required.)

## What changed

- Controller no longer requires `ISENGARD_TOKEN` at startup.
- New CLI: `isengard controller token mint`, `isengard controller agent revoke <id>`, `isengard controller agent list`, `isengard controller ca export`.
- New env vars (agent first boot): `ISENGARD_ENROLL_TOKEN`, `ISENGARD_CONTROLLER_CA_PEM_PATH` (or `ISENGARD_CONTROLLER_CA_PEM` inline; or `ISENGARD_CONTROLLER_CA_PEM_BASE64`, added 2026-05-06).
- Agent persists `state-dir/certs/` (ca.pem + agent.crt + agent.key, key chmod 600).
- Cert TTL: 30 days, auto-renewed at 50% TTL via the new `RenewCert` RPC.
- Per-cert revocation via dashboard (Settings → Enrollment + per-host Revoke button) or CLI.
- Dashboard: new Enrollment tab for minting tokens + per-host revoke button on inspector.

## Known limitations (deferred to follow-ups)

- CA private key not encrypted at rest (file permissions only).
- No CA rotation story; rotating the CA requires re-enrolling everyone.
- Dashboard HTTP still unauthenticated (Cloudflare Access integration is the planned answer).
- Bootstrap-trust during the Enroll RPC: agent trusts whatever cert the controller presents during initial enrollment if `ISENGARD_CONTROLLER_CA_PEM_BASE64` / `_PEM` / `_PEM_PATH` is provided (which is correct), or relies on native roots if not (only works for LE-signed controllers; the agent logs a warning when falling back).

Historical implementation notes are no longer kept in the public repository.
