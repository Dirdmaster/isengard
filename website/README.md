# Isengard documentation site

Docus 5.10 / Nuxt 4 static site for `docs.isengard.dev`. Renders the same
markdown trees that the `isd` binary embeds for the LSP and MCP servers,
so what you read on the web matches what ships with the binary.

## Local dev

```bash
cd website
bun install
bun run dev    # http://localhost:3000
```

## Build

```bash
bun run build  # outputs to .output/
```

## Content sources

The site mounts two trees from the repo, configured in `content.config.ts`:

- `../docs/**`: operator-facing guides at `/`.
- `../crates/<crate>/docs/**`: per-crate API reference at `/api/`. Populated
  by Phase 2 of the docs+AI plan.

The Docus landing collection (`content/index.md`) ships the homepage hero
in-tree because it depends on Docus MDC blocks rather than plain prose.

## Brand polish

- `app.config.ts`: theme tokens (primary colour, header title, GitHub link,
  disabled built-in AI assistant).
- `app/components/IsoCallout.vue`: Isengard-flavoured callout for asides
  that need a coloured left border.

## Deploy

Cloudflare Pages deploy lands in Phase 7 of the docs+AI plan. The current
build artifact (`.output/public/`) is hosted-static-ready.
