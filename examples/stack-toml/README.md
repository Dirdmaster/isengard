# `stack.toml` example layout

End-to-end fixture for Phase 0.13 (`stack.toml` manifest + `isd deploy --all`).
Two stacks under a parent dir, one fleet manifest at the top. Useful for:

- Smoke-testing `isd deploy` and `isd deploy --all` against a controller.
- Showing operators the minimum viable repo shape for managed stacks.
- Catching regressions in the manifest pipeline (parse, overlay, hooks).

See [`docs/RELEASE_NOTES_PHASE_0_13.md`](../../docs/RELEASE_NOTES_PHASE_0_13.md)
for the full schema.

## Layout

```
examples/stack-toml/
├── README.md             # this file
├── isengard.toml         # optional fleet config (fleet + context)
├── hello/
│   ├── stack.toml        # name, fleet, compose, hooks
│   └── compose.toml      # one nginx service on :18080
└── monitoring/
    ├── stack.toml        # name, fleet, compose, secrets, hooks
    └── compose.toml      # grafana on :3000
```

### What each file does

- `isengard.toml`: top-level fleet manifest. `isd deploy` walks upward
  from the stack dir (stopping at the nearest `.git` boundary) to find
  one. Provides a default fleet for stacks that don't pin one.
- `<stack>/stack.toml`: orchestration manifest. Required: `name` and a
  non-empty `compose` list. Optional: `fleet`, `strategy`, `secrets`,
  `[[hooks]]`, `[overlays.<name>]`.
- `<stack>/compose.toml`: flat-shape TOML compose. Every top-level
  table is a service. `isd` converts to YAML before shipping (the
  agent only ever stores YAML on disk).

## Deploy

The parent dir is the cwd. From here:

```sh
# Single stack from its directory:
isd deploy ./hello

# Or from inside it:
cd hello && isd deploy

# Every immediate subdir that has a stack.toml:
isd deploy --all
```

Or use the just recipes from the repo root:

```sh
just example-deploy-hello       # ships only hello
just example-deploy              # ships both via --all
```

The recipes assume you have a controller running locally and an
`isd` context that points to it. Set one up once with:

```sh
isd context create local --http http://127.0.0.1:9418 --use
```

The `monitoring` stack binds to a `grafana_admin_password` secret;
create it first or the POST returns 422:

```sh
isd secret put grafana_admin_password --from-stdin <<< "change-me"
```

## Verify

After deploy:

```sh
isd ps                                       # both stacks present
isd hosts list                               # which host ships each
curl -fsS http://127.0.0.1:18080/            # hello: nginx welcome page
curl -fsS http://127.0.0.1:3000/api/health   # grafana healthcheck
```

Hook output flows back to the controller as `lifecycle_hook.*` audit
events. Tail the agent log for the live view:

```sh
docker logs -f iso-agent | grep lifecycle_hook
```

## Clean up

```sh
isd stack rm hello
isd stack rm monitoring
```

(Or `just down` if you used `just dev` to bring the control plane up.)
