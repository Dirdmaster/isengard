# Enrollment UX: Swarm-style join command

`isengard controller token mint --role agent` now prints a complete,
copy-pasteable `docker run` block instead of a bare token. Inspired by
`docker swarm join`.

## Before

```sh
docker exec isd-controller isengard controller ca export > ca.pem
docker exec isd-controller isengard controller token mint --role agent
# 4N44VZMDJXGQ7PWPIIEOVP63ZVSOV5FJENS67K6N6RRXRUR3UCIQ
docker run -d --name isd-agent \
  -v $(pwd)/ca.pem:/etc/isengard/ca.pem:ro \
  -v isd-agent-state:/var/lib/isengard \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -e ISENGARD_CONTROLLER_CA_PEM_PATH=/etc/isengard/ca.pem \
  -e ISENGARD_ENROLL_TOKEN=4N44VZ... \
  ghcr.io/dirdmaster/isengard:next \
  agent --controller https://controller.example.com:9417 --state-dir /var/lib/isengard
```

Three commands, one side file, easy to typo.

## After

```sh
docker exec isd-controller isengard controller token mint --role agent
```

…prints:

```text
Token minted (expires in 15m).

To enroll an agent, run on the host where you want it to live:

    docker run -d \
      --name isd-agent \
      --restart unless-stopped \
      --platform linux/amd64 \
      -v isd-agent-state:/var/lib/isengard \
      -v /var/run/docker.sock:/var/run/docker.sock \
      -e ISENGARD_ENROLL_TOKEN=01HX... \
      -e ISENGARD_CONTROLLER_CA_PEM_BASE64=LS0t... \
      ghcr.io/dirdmaster/isengard:next \
      agent --controller https://controller.local:9417 --state-dir /var/lib/isengard

Token expires at 2026-05-07T11:30:00Z. Mint a new one with:
    isengard controller token mint --role agent
```

Paste the printed `docker run` block on the agent host. Done.

## What changed

### New env var: `ISENGARD_CONTROLLER_CA_PEM_BASE64`

The CA root cert is delivered as a single-line, standard-alphabet base64
string in an env var. The agent decodes it once at startup and pins it
for the bootstrap channel.

This avoids two pain points:

- `ISENGARD_CONTROLLER_CA_PEM_PATH` requires a side file (and a bind
  mount).
- `ISENGARD_CONTROLLER_CA_PEM` is multiline; passing multiline values
  through `docker run -e` works, but most shells mangle them.

The existing `_PEM_PATH` and `_PEM` env vars still work unchanged.
Resolution order: `_PATH` > `_BASE64` > `_PEM` > caller-supplied path
> caller-supplied inline > native roots.

### New flags on `controller token mint`

| Flag | Default | Purpose |
| --- | --- | --- |
| `--public-addr <host:port>` | `controller.local:9417` | Public address agents will dial; embedded in the join command. Also reads `ISENGARD_PUBLIC_ADDR`. |
| `--image <ref>` | `ghcr.io/dirdmaster/isengard:next` | Image reference to embed in the join command. |
| `--format <text\|token>` | `text` | `text` prints the join block (default). `token` prints just the bare token (legacy / scripts). |

### Compose stack

`docker/compose.yaml` ships a one-file controller + agent stack. The
controller has `ISENGARD_PUBLIC_ADDR=controller:9417` set so the join
command output works inside the compose network without editing.

## Backwards compatibility

- `ISENGARD_CONTROLLER_CA_PEM` and `ISENGARD_CONTROLLER_CA_PEM_PATH`
  continue to work.
- `controller ca export` still prints the CA root PEM unchanged
  (advanced flows: baking the CA into a custom image, distributing via
  config management).
- `controller token mint --role agent --format token` reproduces the
  pre-change output exactly (one-line bare token, nothing else).

## Why base64 over a path

- Path requires a side file, which requires a bind mount, which requires
  the operator to think about file permissions, paths, and SELinux/AppArmor.
- Inline PEM is multiline and breaks in many shells, especially CI
  systems that read env from `.env` files.
- Base64 is one line, transports cleanly through every config layer
  (env, compose, k8s secrets, vault), and the decode cost is negligible.

## Why not auto-detect the public address

The controller listens on `0.0.0.0:9417` by default; that's not a
dialable address. We could try to introspect the host's primary
interface, but that varies by deployment (LAN-only, behind reverse
proxy, Tailscale, public IP). Better to make the operator declare it
once (CLI flag or env) and emit a correct join command than to print a
plausible-looking but broken one.

## Files

- Spec: [`docs/superpowers/specs/2026-05-06-swarm-style-enrollment-design.md`](./superpowers/specs/2026-05-06-swarm-style-enrollment-design.md)
- Plan: [`docs/superpowers/plans/2026-05-06-swarm-style-enrollment.md`](./superpowers/plans/2026-05-06-swarm-style-enrollment.md)
- Compose stack: [`docker/`](../docker/)
