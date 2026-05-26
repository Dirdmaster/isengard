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
bun run generate  # outputs to .output/public/
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

Cloudflare Pages deploys this site from `website/.output/public/`. This is the
current public deploy target for `docs.isengard.dev`.

The GitHub Actions Pages workflow installs dependencies in `website`, runs
`bun run generate`, and deploys the generated static artifact on pushes to
`main` and `next`.
