Native placement verbs + label selectors.

The compose parser populates `DesiredService::placement` with one of the
[`Placement`] variants when a service uses `spread:`, `global:`, `on:`,
or `where:` keys (or the Swarm-compat `deploy:` block). The scheduler in
`isengard-controller` reads this enum to compute the target host(s).

Selector grammar (subset of k8s label-selector, comma-separated):

```text
selector = expr ("," expr)*
expr     = key (op value)?
op       = "==" | "!=" | "in (" v ("," v)* ")" | "notin (" v ("," v)* ")"
```

- Keys: ASCII `[a-z0-9._-]+`, max 63 chars.
- Values: any UTF-8 except `,` and `=` (matches the `agent_labels`
  ingest rule).
- A bare key (no op) means "exists" (`key == any`).

Selector parsing is intentionally simple: no nested parens, no regex, no
globbing. The flat string form keeps the YAML/TOML readable and covers
"give me a GPU host."
