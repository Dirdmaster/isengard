---
title: isd lsp
description: Reference for the `isd lsp` subcommand.
---

# `isd lsp`

Run the Isengard language server over stdio.

```sh
isd lsp
```

Editors start this command directly. It serves diagnostics and completion for Isengard manifests and label vocabulary using the same docs bundled with `isd`.

Typical editor configuration points the language server command at the installed binary:

```json
{
  "command": "isd",
  "args": ["lsp"]
}
```

Run it manually only when debugging editor integration. The command keeps stdin and stdout reserved for the language server protocol.
