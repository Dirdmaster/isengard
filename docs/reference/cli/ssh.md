---
title: isd ssh
description: Reference for the `isd ssh` subcommand group (auto-dial, mint, status, hosts, ca).
---

# `isd ssh`

Mint and use short-lived SSH user certs against the fleet. See the [SSH bastion concept page](/concepts/ssh-bastion) for the model.

Five sub-verbs:

| Verb | Summary |
|---|---|
| `isd ssh <host> [args...]` | Auto-mint a cert if stale, then exec `ssh isengard@<host>`. |
| `isd ssh mint` | Explicit cert mint. Writes the cert next to the pubkey. |
| `isd ssh status` | Print the current local cert. Exits non-zero when missing or expired. |
| `isd ssh hosts` | List fleet hosts the controller knows about. |
| `isd ssh ca pubkey` | Print the controller's SSH CA pubkey in OpenSSH wire format. |

## `isd ssh <host> [args...]`

Connect to a fleet host. Auto-mints a fresh cert if the local one is missing, expired, or has less than 5 minutes remaining, then exec's `ssh isengard@<host>` with any extra args passed through verbatim.

The `cert_is_stale` heuristic re-mints when any of these is true:

- `~/.ssh/id_ed25519-cert.pub` does not exist.
- `ssh-keygen -L` fails to parse the cert.
- The cert's `Valid: ... to <when>` is within 5 minutes of now (or already past).

The auto-mint path uses defaults: `~/.ssh/id_ed25519.pub`, 1h TTL, principal `isengard`, comment `operator@<hostname> <UTC ISO8601>`.

Example:

```sh
# basic dial
isd ssh edge-1

# pass extra ssh args (port, command)
isd ssh edge-1 -p 2222 -- uptime

# verbose ssh
isd ssh edge-1 -v
```

Exit code is `ssh`'s exit code. Mint failures exit non-zero before `ssh` runs.

## `isd ssh mint`

Explicit cert re-mint. Useful when you want a fresh window without dialing, or when you want to override the defaults.

```
isd ssh mint [--pubkey <path>] [--ttl <secs>] [--principal <p>]... [--comment <text>]
```

| Flag | Default | What it does |
|---|---|---|
| `--pubkey <path>` | `~/.ssh/id_ed25519.pub` | Path to the SSH public key to sign. |
| `--ttl <secs>` | `3600` | Requested TTL in seconds. Server caps via `ISENGARD_SSH_CERT_MAX_TTL` (default 1h, hard cap 24h). |
| `--principal <p>` | `isengard` | Principal baked into the cert. Repeatable: `--principal isengard --principal deploy`. |
| `--comment <text>` | `operator@<hostname> <UTC ISO8601>` | Free-form key-id. Shows up in `auditd` and `last` on the agent host. |

Example:

```sh
# default mint (1h, principal=isengard)
isd ssh mint

# 15 minute cert with a custom key id
isd ssh mint --ttl 900 --comment "deploy-job-1234"

# sign a non-default key
isd ssh mint --pubkey ~/.ssh/id_ed25519_isengard.pub
```

On success: prints `issued cert <path> (fingerprint <sha256>, ttl <secs>s)` and writes the signed cert next to the pubkey (`id_ed25519.pub` -> `id_ed25519-cert.pub`).

Exit codes:

| Code | When |
|---|---|
| `0` | Cert minted and written. |
| `1` | Missing pubkey, controller-less context, HTTP error, or disk write failure. |

## `isd ssh status`

Print the current local cert in the standard isd table format. Reads the freshest `~/.ssh/id_*-cert.pub` by mtime, parses it via `ssh-keygen -L`, prints path / fingerprint / principals / key id / validity window / remaining seconds.

```
isd ssh status
```

Example output:

```
 FIELD        VALUE
 PATH         /home/op/.ssh/id_ed25519-cert.pub
 FINGERPRINT  SHA256:abc...
 PRINCIPALS   isengard
 KEY ID       operator@laptop 2026-05-21T10:00:00Z
 VALID FROM   2026-05-21T10:00:00Z
 VALID TO     2026-05-21T11:00:00Z
 REMAINING    1734s
```

Exit codes:

| Code | When |
|---|---|
| `0` | Cert present and not expired. |
| `1` | No cert found, `ssh-keygen` failed, or cert is expired. |

## `isd ssh hosts`

List the fleet hosts the controller knows about. Renders four columns: host, ssh user, principal, last-seen timestamp.

```
isd ssh hosts
```

Example output:

```
 HOST        SSH USER  PRINCIPAL  LAST SEEN
 edge-1      isengard  isengard   2026-05-21T10:32:11Z
 edge-2      isengard  isengard   -
```

The `SSH USER` and `PRINCIPAL` columns are both `isengard` today (the bastion uses a single default user). They diverge in a future phase when `isengard.ssh.principals` runtime enforcement lands.

Exit codes:

| Code | When |
|---|---|
| `0` | List rendered (zero or more rows). |
| `1` | HTTP failure or controller-less context. |

## `isd ssh ca pubkey`

Print the controller's SSH user CA pubkey in OpenSSH wire format. Drop the output into a non-Isengard host's `TrustedUserCAKeys` file to extend the bastion to that host without enrolling it.

```
isd ssh ca pubkey
```

Example:

```sh
# trust the controller's CA on a hand-managed box
isd ssh ca pubkey | ssh root@legacy-1 'cat >> /etc/ssh/trusted_user_ca_keys'
ssh root@legacy-1 'echo "TrustedUserCAKeys /etc/ssh/trusted_user_ca_keys" >> /etc/ssh/sshd_config && systemctl reload sshd'
```

Exit codes:

| Code | When |
|---|---|
| `0` | Pubkey printed. |
| `1` | HTTP failure or controller-less context. |
