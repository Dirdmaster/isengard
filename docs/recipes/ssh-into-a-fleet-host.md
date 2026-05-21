---
title: SSH into a fleet host
description: Enroll a host, wait for sshd to pick up the controller CA, then dial in with `isd ssh`.
---

# SSH into a fleet host

End-to-end walkthrough: from a fresh agent enrollment to a working `isd ssh` shell on the host.

Prerequisites:

- A running controller you can reach as the operator (you can already run `isd ps`).
- A target host where you can install the agent (linux, not macOS: macOS hosts skip the SSH CA install).
- An ed25519 keypair at `~/.ssh/id_ed25519` on your operator laptop. Generate one with `ssh-keygen -t ed25519` if missing.

## 1. Enroll the agent

On the controller host:

```sh
isd join-token --role agent
```

Copy the printed `isd join` command, paste it on the target host, wait for `agent enrolled`.

## 2. Confirm the agent installed the CA

The agent writes the controller's SSH CA pubkey to `/etc/isengard/ssh_ca.pub` and a sshd drop-in at `/etc/ssh/sshd_config.d/40-isengard-ca.conf`, then reloads sshd. From the target host:

```sh
cat /etc/isengard/ssh_ca.pub
cat /etc/ssh/sshd_config.d/40-isengard-ca.conf
sudo sshd -T | grep -i trustedusercakeys
```

The third command should print the path matching the drop-in. If it doesn't, the reload failed; run `sudo systemctl reload sshd` and try again.

## 3. Verify the host shows up

From your operator laptop:

```sh
isd ssh hosts
```

The new host appears with a recent `LAST SEEN` timestamp.

## 4. Dial in

```sh
isd ssh <hostname>
```

First call auto-mints a 1h cert (`issued cert ~/.ssh/id_ed25519-cert.pub ...`) and exec's `ssh isengard@<hostname>`. You land in the `isengard` user's shell. Subsequent calls within the cert's validity window skip the mint.

## 5. Check what you've got

```sh
isd ssh status
```

Confirms the cert path, fingerprint, principals, validity window, and remaining seconds. Exits non-zero when the cert is expired or missing: useful in scripts.

## Common knobs

**Longer-lived certs.** The default cap is 1 hour. To raise it, set `ISENGARD_SSH_CERT_MAX_TTL` (seconds) on the controller and re-mint:

```sh
# on the controller host, in the agent compose env
ISENGARD_SSH_CERT_MAX_TTL=14400  # 4 hours

# on your laptop
isd ssh mint --ttl 14400
```

The absolute ceiling is 24h (86400 seconds) regardless of the env value.

**Opt one host out.** Set `ISENGARD_SSH_BASTION_DISABLED=1` in the agent's environment to skip the CA install on that host. Pre-existing `~/.ssh/authorized_keys` still works.

**Trust the CA on a non-Isengard host.** Pipe `isd ssh ca pubkey` into a hand-managed box's `TrustedUserCAKeys` file. See the [`isd ssh` reference](/reference/cli/ssh) for the exact one-liner.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Permission denied (publickey)` after a fresh mint | sshd didn't reload after the agent wrote the drop-in | `sudo systemctl reload sshd` on the target host |
| `isd ssh status` reports `expired` | Cert is older than the requested TTL | `isd ssh mint` for a fresh one |
| `no SSH cert found in ~/.ssh/id_*-cert.pub` | Never minted on this laptop | `isd ssh mint` |
| Target host missing from `isd ssh hosts` | Agent not enrolled or last-seen lapsed | Re-run `isd join` on the host |
| macOS host doesn't trust the cert | macOS agents skip the CA install (no `/host/` mount) | Use `~/.ssh/authorized_keys` for macOS targets |
