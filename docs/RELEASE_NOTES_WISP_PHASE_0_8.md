# Wisp Phase 0.8: drop docker from the host install

> Phase 0.8 of the wisp arc. Branch: `wisp/phase-0-8`. Stacked on `wisp/phase-0-7` (the static-binary release pipeline). The host now runs the controller + agent as systemd services from the raw musl binaries Phase 0.7 publishes. No dockerd, no docker compose, no docker network create.

## What this is

The systemd-native install replaces docker-compose as the default. A host with the new install has only:

- `/usr/local/bin/isengard` (the static musl binary)
- `/etc/systemd/system/isd-controller.service`, `isd-agent.service`, `isd-agent.target`
- `/etc/isengard/` (master.key, isengard.env, agent-token.env, master-key.env, ca.pem)
- `/var/lib/isengard/` (controller SQLite, agent state, per-stack compose files)
- `/var/lib/wisp/` (wisp content store + bundles + IPAM)

The agent uses wisp (clone3 + cgroup v2 + iptables, via `wisp` + `wisp-image` + `wisp-net` from Phase 0.1-0.6) to manage workload containers. dockerd is not running. The legacy docker-compose path stays around as `install/install-docker.sh` for operators who haven't migrated; Phase 0.10+ removes it.

## What changed

### New systemd unit files

`install/systemd/isd-controller.service`

```ini
[Unit]
After=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/isengard controller --state-dir /var/lib/isengard
Restart=on-failure
EnvironmentFile=/etc/isengard/isengard.env
EnvironmentFile=-/etc/isengard/master-key.env
User=root
StateDirectory=isengard
ConfigurationDirectory=isengard
LogsDirectory=isengard
ReadWritePaths=/var/lib/isengard /etc/isengard
ProtectSystem=strict
ProtectHome=true
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

`install/systemd/isd-agent.service`

```ini
[Unit]
After=network-online.target isd-controller.service
PartOf=isd-controller.service

[Service]
Type=simple
ExecStart=/usr/local/bin/isengard agent --controller https://controller.local:9417 --state-dir /var/lib/isengard/agent
Restart=on-failure
EnvironmentFile=/etc/isengard/isengard.env
EnvironmentFile=/etc/isengard/agent-token.env
EnvironmentFile=-/etc/isengard/master-key.env
User=root
ReadWritePaths=/var/lib/wisp /var/lib/isengard /sys/fs/cgroup /etc/isengard
ProtectSystem=full
NoNewPrivileges=false
PrivateTmp=false
```

`install/systemd/isd-agent.target` is a convenience wrapper (`Wants= isd-controller isd-agent`).

`PartOf=isd-controller.service` on the agent means a controller restart cycles the agent automatically. Agents bind their mTLS cert to the running controller's CA, so a fresh CA needs a fresh handshake.

### Capability story

Phase 0.8 runs both services as root with no AppArmor profile. The agent needs SYS_ADMIN (mount, namespace, pivot_root for clone3), NET_ADMIN (iptables, bridge, veth), SYS_PTRACE (`/proc/<pid>/ns/*` for nsenter healthchecks), SYS_RESOURCE (cgroup v2 writes outside its own slice), and CHOWN/SETUID/SETGID/DAC_OVERRIDE/FOWNER/SETPCAP for workload bootstrap (nginx-style images that drop privs).

Holding all of that explicitly via `Capabilities=` + `AmbientCapabilities=` is doable but pulls in subtle interactions with `NoNewPrivileges=` and the agent's per-container cap drops. Phase 0.10+ tightens this to a dedicated `isd-agent` user + AmbientCapabilities + an AppArmor profile.

### install.sh rewrite

`install/install.sh` is a top-to-bottom rewrite. Highlights:

- Detects host arch (`uname -m` -> x86_64 / aarch64), constructs the release URL, downloads `isengard-<target>-unknown-linux-musl` + `<asset>.sha256`.
- Verifies sha256 before installing to `/usr/local/bin/isengard`.
- Same secrets bootstrap flow as the legacy script: master key in `/etc/isengard/master.key`, hidden-input prompts for individual secrets, encrypted SQLite, plaintext never on disk.
- Writes `/etc/isengard/isengard.env` with `ISENGARD_RUNTIME=wisp` baked in (the systemd flow is wisp-only by design).
- Installs the systemd units, `daemon-reload`, `enable --now isd-controller`, waits up to 30s for the CA to initialize.
- Mints a 15-minute enrollment token via `isengard controller --state-dir <dir> token mint --role agent --format token` and writes it to `/etc/isengard/agent-token.env`.
- Exports the controller CA via `isengard controller --state-dir <dir> ca export` to `/etc/isengard/ca.pem`.
- `enable --now isd-agent`. The agent picks up the token + CA path from `agent-token.env`, enrolls, persists its mTLS cert under `/var/lib/isengard/agent`, and ignores the env var on subsequent restarts.

The reinstall menu now offers refresh-binary | refresh-config | wipe | abort:

| Action | What it does |
|---|---|
| refresh-binary | Re-downloads the binary at the requested version, restarts services. Keeps master.key, isengard.env, and the secrets DB. |
| refresh-config | Re-downloads the binary AND re-prompts for ACME config. Backs up the old env to `isengard.env.bak`. Keeps secrets. |
| wipe | DESTRUCTIVE. Stops + disables the units, removes /var/lib/isengard, /var/lib/wisp, /etc/isengard, /usr/local/bin/isengard. Then runs the full first-time path. |

### Binary self-update

A new module `crates/isengard-agent/src/self_update.rs` and a new CLI subcommand `isengard self-update`:

```sh
isengard self-update \
  --url https://github.com/Weavers-Engineering/Isengard/releases/download/v0.4.1/isengard-x86_64-unknown-linux-musl \
  --sha256 <expected-hex>
```

Flow:
1. Resolve `current_exe()` to find the running binary path.
2. Download to `<target>.new` next to it (same fs guarantees an atomic rename).
3. Verify sha256 (size capped at 512 MiB so a corrupt URL can't OOM the agent).
4. `chmod 0755` and `fs::rename` onto the running binary's path. On the same filesystem, `rename(2)` is atomic.
5. Spawn `systemctl restart isd-agent.service` (default; pass `--no-restart` to skip). The current process catches SIGTERM; systemd's Type=simple unit ExecStarts the new binary.

The legacy docker-coupled rename-and-recreate flow in `crates/isengard-plugins/updater/src/self_update.rs` stays around for the docker-compose path. It's not wired into the binary today (no force-link in main.rs); Phase 0.10+ removes both the legacy plugin path and `install/install-docker.sh`.

The remote-trigger story (controller pushes a `host.update_binary` HostAction with URL + digest, agent picks it up via the existing sync stream and calls `run_self_update`) is a Phase 0.9+ follow-up. This commit lands the safe, replicable foundation.

## Migration

From a docker-compose-installed host to the systemd-native install:

1. Drain workload stacks first: `isd stack down <stack>` for everything you don't want orphaned.
2. Tear down: `sudo bash /etc/isengard/uninstall.sh --purge`.
3. Run the new `install/install.sh`. Master key + secrets DB do NOT carry over; you'll re-enter the bootstrap secrets.
4. `isd stack up <stack>` for each stack. Wisp pulls the images and creates the bundles fresh.

Going back to docker is the inverse: stop the systemd units, run `install/install-docker.sh`, accept that wisp-managed bundles won't carry over.

## What's NOT in 0.8

- **End-to-end test on a docker-less host.** We don't have an automated way to provision one. Phase 0.9 (init/join verbs) will exercise the new flow against a fresh Linux VM.
- **Tightened capabilities.** Agent runs as root for now. `Capabilities=` + AppArmor lands in 0.10+.
- **Controller self-update.** Operators run install.sh again to refresh the controller binary. Auto-update for the controller lands later.
- **Removal of the docker path.** `install/install-docker.sh` and `install/compose.{yaml,wisp.yaml}` stay around. 0.10+ removes them.
- **Signed binaries.** SHA256 only, same as Phase 0.7. Signing is a 0.9+ decision once we know whether we want sigstore vs a key we control.
- **Auto-token refresh.** Enrollment tokens have a 15-minute TTL. install.sh mints one at install time; if the agent fails to enroll within that window, the operator runs `isengard controller --state-dir <dir> token mint` and overwrites `/etc/isengard/agent-token.env`.

## Done bar

- `cargo build --workspace` clean.
- `cargo fmt --check` clean.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace --lib` all green (7 new self_update unit tests cover staging path, digest validation, chmod, atomic rename).
- `cargo deny check` clean.
- `bash -n install/install.sh` clean (full shellcheck not run; not installed locally).
- The systemd unit files parse as valid INI shape; `systemd-analyze verify` not run locally (Mac dev box). The unit ExecStart commands match the binary's actual CLI surface (verified via `isengard --help`).

## Phase 0.9 hooks

- `install.sh init|join` verbs: replace the curl-bash one-liner with operator-typed `isengard init` (controller) and `isengard join <token>` (agent) on a fresh host. Self-bootstraps the systemd units.
- Remote `host.update_binary` HostAction so the controller can publish a new release URL + digest and the agent's sync loop calls `run_self_update`.
- An end-to-end test that provisions a fresh Linux VM (no docker installed), runs install.sh, brings up a workload stack, and asserts the agent is talking via wisp.
