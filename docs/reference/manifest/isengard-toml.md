---
title: isengard.toml
description: Schema for the repo-root fleet manifest.
---

# `isengard.toml`

Optional fleet-level defaults for a repository of stacks.

Place `isengard.toml` at the repo root:

```toml
fleet = "lab"
context = "prod"
```

Fields:

| Field | Meaning |
|---|---|
| `fleet` | Default fleet name for stack manifests that do not set one. |
| `context` | Default Docker or controller context for `isd` commands run inside the repo. |

`isd` walks up from the current directory until it finds `isengard.toml` or a `.git` boundary. Both fields are optional, so an empty file is valid but rarely useful.
