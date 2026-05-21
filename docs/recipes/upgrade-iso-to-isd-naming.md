---
title: Upgrade iso-* to isd-* Naming (v0.5.x to v0.6.0)
description: Migrate an existing deployment to the new isd-* container and volume names without losing state.
---

# Upgrade iso-* to isd-* Naming (v0.5.x to v0.6.0)

Starting in v0.6.0 the production compose recipe renames every container and
named volume from `iso-*` to `isd-*` so the runtime artefacts line up with the
`isd` CLI binary the operator already types. The `iso-` stem was historical:
it came from "isolation", the working name before the daemon got the
`isengard` / `isd` split. The rename is cosmetic (no schema changes, no wire
changes, no image tag bumps), but the docker volumes are content-addressed by
name, so an existing v0.5.x host cannot just pull the new compose.yaml and
restart: docker would create empty `isd-controller-state` / `isd-agent-state`
/ `isd-stacks` volumes alongside the old `iso-*` ones, and the controller
would boot with no master key.

This recipe walks one host through the rename. Expect a few minutes of
downtime per host while the named volumes get copied to their new names.

## Pre-flight

Take a backup before touching anything. If something goes sideways during
the copy step the restore path is `isd restore`.

```sh
isd backup
```

## Stop the stack

```sh
docker compose -f /etc/isengard/compose.yaml down
```

The compose project goes down but the named volumes persist by default.

## Copy volumes to their new names

The compose `name:` field is content-addressed, so renaming requires
copying into a fresh volume. One-shot alpine handles it:

```sh
for old in iso-controller-state iso-agent-state iso-stacks; do
  new="${old/iso-/isd-}"
  docker volume create "$new"
  docker run --rm -v "$old:/from" -v "$new:/to" alpine \
    sh -c "cd /from && cp -a . /to"
  docker volume rm "$old"
done
```

Each iteration creates the new volume, copies content with `cp -a` (preserves
permissions, timestamps, symlinks), then drops the old volume. The
`iso-controller-state` -> `isd-controller-state` copy is the load-bearing one:
master.key and the controller's sqlite + CA state live there.

## Refresh the compose recipe

The new compose.yaml ships in the v0.6.0 release artefact bundle. Either
pull it down explicitly, or let `isd init --force` regenerate it from the
binary's embedded recipe.

```sh
curl -sSfL https://github.com/Weavers-Engineering/Isengard/releases/download/v0.6.0/compose.yaml \
  > /etc/isengard/compose.yaml
```

## Bring the stack back up

```sh
docker compose -f /etc/isengard/compose.yaml up -d
```

The controller boots against `isd-controller-state` (which now holds the
copied master.key, CA, sqlite) and the agent rejoins against its preserved
client cert in `isd-agent-state`. No re-enrolment, no token mint, no fleet
churn.

## Verify

```sh
isd ps --all-system
docker volume ls | grep isd-
```

You should see `isd-controller` and `isd-agent` running, and the three
`isd-*` named volumes. The old `iso-*` volumes are gone.
