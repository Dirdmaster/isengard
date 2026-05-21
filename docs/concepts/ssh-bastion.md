---
title: SSH Bastion
description: Short-lived SSH user certs minted by the controller, trusted by every agent host.
---

# SSH bastion

## What it is

The controller mints short-lived SSH user certificates. Every enrolled agent host trusts those certs automatically via a `TrustedUserCAKeys` drop-in. Operators run `isd ssh <host>` and land in a shell. No per-host accounts, no `~/.ssh/authorized_keys` to chase across the fleet, no shared bastion box.

The whole thing rides on the controller's SSH user CA: one keypair, generated at first boot, kept on the controller. Every agent gets the CA pubkey at enrollment and writes it to `/etc/isengard/ssh_ca.pub`. Sshd reads the drop-in at `/etc/ssh/sshd_config.d/40-isengard-ca.conf` and starts honoring certs signed by that CA.

## How it works

Four pieces talk to each other across the lifecycle:

1. **Controller**: owns an `SshAuthority` (the CA keypair). Exposes `GET /api/v1/ssh/ca` to hand out the public half, and `POST /api/v1/ssh/cert` to sign operator pubkeys into user certs.
2. **Agent**: fetches the CA pubkey during enrollment, writes it to the host's `/etc/isengard/ssh_ca.pub`, drops the sshd config snippet, reloads sshd.
3. **Dashboard**: hosts the two endpoints above. Caps TTL via `ISENGARD_SSH_CERT_MAX_TTL` (default 1h, absolute ceiling 24h) and writes an `ssh.cert.issued` event to the journal on every successful mint.
4. **`isd ssh`**: the operator CLI. Reads `~/.ssh/id_ed25519.pub`, POSTs it to the dashboard, writes the signed cert next to the pubkey as `id_ed25519-cert.pub`, and exec's `ssh isengard@<host>`. OpenSSH picks the cert up automatically because of the `-cert.pub` naming.

```
  operator laptop           controller                       agent host
  ---------------           ----------                       ----------
  isd ssh edge-1
    |
    | POST /api/v1/ssh/cert (pubkey, ttl, principals)
    +------------------------>  SshAuthority.sign_user_cert
                                journal: ssh.cert.issued
    | <------------------------ certificate, fingerprint, ttl
    |
    | ssh isengard@edge-1
    +----------------------------------------------------->  sshd reads
                                                             TrustedUserCAKeys
                                                             checks cert sig
                                                             matches principal
                                                             grants shell
```

The cert binds to the operator's pubkey: only the matching private key can use it. The agent never sees the operator's private key. The controller never sees the operator's private key either: only the pubkey it signs.

## Security model

Three primitives keep this tight.

**TTL caps.** Default cert lifetime is 1 hour. The controller's `ISENGARD_SSH_CERT_MAX_TTL` env caps how long any cert can live; an absolute 24h ceiling is baked into the dashboard code so a misconfigured env can't push past it. The CLI's auto-mint path requests 1h; explicit `isd ssh mint --ttl <secs>` can request shorter (the server enforces the upper bound).

**Key-pinning.** Each cert pins the operator's ephemeral pubkey. A stolen cert without the matching private key is useless. The fingerprint is echoed back in the issuance response and lands in the audit event.

**Audit trail.** Every mint writes an `ssh.cert.issued` event into the controller journal: principal list, requested TTL, effective TTL, pubkey fingerprint, free-form key-id (defaults to `operator@<laptop> <UTC ISO8601>`). The key-id also surfaces in `auditd` and `last` on the agent host, so you can pivot from "who logged into edge-1 at 14:32" back to the mint record.

What it does **not** provide today:

- **Revocation.** No KRL distribution. A leaked private key can use its cert for up to 24h. Rotate the SSH CA (deferred to v2) to cut every outstanding cert at once.
- **Per-host principal enforcement.** Phase 5 shipped the `isengard.ssh.*` label vocabulary (`principals`, `allowed_users`, `max_ttl_seconds`). The LSP knows the keys. Runtime enforcement (controller reads them at mint, agent reads them at install) is deferred.

## Limitations

- **macOS hosts skip the install.** The agent needs the host filesystem bind-mounted at `/host` to write the sshd drop-in. Linux hosts have that via the agent's compose; macOS hosts don't. The agent logs `host filesystem not mounted at /host` and continues. Use `ISENGARD_SSH_BASTION_DISABLED=1` to opt out explicitly.
- **Pre-existing auth keeps working.** The CA drop-in is additive: `TrustedUserCAKeys` extends what sshd already trusts. Your `~/.ssh/authorized_keys` entries still let you in. Removing the bastion doesn't lock you out.
- **One CA, no rotation.** The controller mints the CA at first boot and keeps it forever. There's no `isd ssh ca rotate` yet. When that lands (v2), transitional certs will cover the gap.
- **No WebSSH.** Browser-based shell access is a v2 feature. Today the surface is the CLI.
