---
title: isd secret
description: Reference for the `isd secret` subcommand.
---

# `isd secret`

Manage controller-stored secrets for stacks and automation.

## Set a secret

```sh
printf '%s' "$TOKEN" | isd secret set grafana-admin-token
isd secret set grafana-admin-token --from-file ./token.txt
```

By default the value is written to the current context. Fan out to every saved context with `--scope global`.

## List secrets

```sh
isd secret ls
```

The list shows names only, never plaintext values.

## Read a secret

```sh
isd secret get grafana-admin-token
```

`get` prints the plaintext value to stdout so scripts can pipe it into another command.

## Remove a secret

```sh
isd secret rm grafana-admin-token
```

Secret values are encrypted under the controller state. They are included in `isd backup` output.
