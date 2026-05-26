# Showcase Public Entrypoint Design

## Problem

Isengard is strong enough to show, but the public entrypoints still disagree about what the product is.

The root README opens with rewrite history, old branch language, and a legacy Go auto-updater narrative. The deployed Cloudflare Pages workflow builds `www`, which is the old marketing site, while the current Docus docs live under `website`. The docs guard added in Phase A scans the current docs path, but not the deployed site workflow or website support files.

That creates a credibility problem: a visitor can land on the repo or site and learn the wrong product before they ever run `isd init`.

## Goals

- Make the root README tell one current product story: Docker-native orchestration for personal infrastructure.
- Make `website` the canonical deployed public site.
- Stop deploying stale `www` through the Pages workflow.
- Update local site commands to use `website`.
- Expand the public-doc guard to cover the current public entrypoints.
- Keep examples generic: tech/dev containers and `*.isengard.app` hostnames only.

## Non-Goals

- Do not remove the `www` tree in this PR.
- Do not fill every public docs placeholder in this PR.
- Do not add the public demo stack in this PR.
- Do not change Isengard runtime behavior.
- Do not harden install, backup, restore, upgrade, or route/TLS behavior in this PR.

## Canonical Public Entrypoints

The public entrypoints after this PR are:

- `README.md`: current product story, quick start, first stack, routing label, and useful links.
- `website/`: Docus site using `docs/**` plus `website/content/index.md`.
- `.github/workflows/pages.yml`: builds and deploys `website/.output/public`.
- `just site` and `just site-build`: local docs site commands.

`www/` remains in the repository for now, but it is no longer the public deploy target and is not linked from the current public path.

## README Shape

The README should lead with:

1. One-line positioning.
2. Short value bullets.
3. Install table.
4. Five-minute quick start with `isd init`, `isd stack doctor`, `isd stack deploy`, `isd route ls`, and a Host-header curl.
5. Minimal `traefik/whoami` compose example using `whoami.isengard.app`.
6. Architecture sketch in prose: operator CLI, controller, agents, Pingora proxy.
7. Links to getting-started docs, route docs, stack docs, backup/upgrade docs if present.
8. A blunt maturity note: showcase-ready, not yet hardened for unattended production upgrades.

The README should not include the legacy Go auto-updater guide, old `dirdmaster` image names, `isd login`, `credentials.toml`, missing `legacy-go` links, or `crates/wisp` as a current surface.

## Site Workflow

Pages should build the Docus site:

```text
cd website
bun install
bun run generate
wrangler pages deploy website/.output/public --project-name=isengard
```

On pull requests, the workflow should build only. Deploy should run only on push or manual dispatch where Cloudflare secrets are available.

The workflow should trigger on changes to `website/**`, `docs/**`, `README.md`, root package files, and itself.

## Docs Guard

The public-doc guard should cover current public entrypoints:

- `README.md`
- `website`
- `docs/getting-started`
- `docs/reference/cli`
- `docs/concepts`
- `install/README.md`

It should reject stale public vocabulary:

- old route labels
- old login and credential flow
- personal domains
- consumer-entertainment examples
- old org/image references
- placeholder text in the showcase path

Historical specs, plans, and release notes remain outside this guard.

## Verification

- `cargo test -p isd help_render::tests`
- `just docs-check`
- `bun install && bun run generate` in `website`
- `cargo fmt --check`
- no Unicode dash characters in changed files
- no stale public vocabulary in guarded paths
