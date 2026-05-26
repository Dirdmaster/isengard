# Showcase Public Entrypoint Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the repo and deployed site tell one current Isengard product story.

**Architecture:** Keep runtime behavior unchanged. Rewrite public entrypoint copy, point Cloudflare Pages at the current Docus site, update local site recipes, and broaden the docs guard so stale public language cannot return.

**Tech Stack:** Markdown, GitHub Actions, Docus/Nuxt, Bun, Justfile, shell.

---

## File Structure

- Modify `README.md`: current product story and quick start.
- Modify `.github/workflows/pages.yml`: build/deploy `website` instead of `www`.
- Modify `package.json`: make `docs:build` use static generation.
- Modify `website/README.md`: update deployment docs now that Pages deploys it.
- Modify `Justfile`: rename site recipes to `site` and `site-build`, keep `www` aliases as hidden compatibility if useful.
- Modify `scripts/ci/check_public_docs_vocabulary.sh`: expand guarded paths and patterns.

## Task 1: README Rewrite

- [ ] Replace the current README body with current Isengard positioning, install, quick start, example compose, architecture sketch, maturity note, and links.
- [ ] Verify README has no old org/image references, stale login flow, missing legacy links, or consumer examples.
- [ ] Commit with `docs: rewrite public readme`.

## Task 2: Deploy Current Website

- [ ] Update `.github/workflows/pages.yml` to trigger on `website/**`, `docs/**`, `README.md`, root package files, and the workflow file.
- [ ] Make PRs run `bun install` and `bun run generate` in `website`.
- [ ] Make deploy run only when not a pull request.
- [ ] Deploy `website/.output/public`.
- [ ] Update `package.json`, `Justfile`, and `website/README.md` to match.
- [ ] Commit with `ci: deploy current docs site`.

## Task 3: Expand Public Docs Guard

- [ ] Add `website`, `install/README.md`, and current public docs paths to the guard.
- [ ] Add patterns for old image orgs, stale login flow, personal route domains, consumer examples, and placeholder text.
- [ ] Run `just docs-check` and fix guarded public files until it passes.
- [ ] Commit with `ci: expand public docs guard`.

## Task 4: Verification

- [ ] Run `cargo test -p isd help_render::tests`.
- [ ] Run `just docs-check`.
- [ ] Run `bun install && bun run generate` in `website`.
- [ ] Run `cargo fmt --check`.
- [ ] Scan changed files for Unicode dash characters.
- [ ] Push PR and enable auto-merge after checks pass.

## Self-Review

- Scope matches PR 1 only: README, site deployment, site commands, docs guard.
- Demo stack, placeholder doc cleanup, screenshots, and video script remain follow-up PRs.
- No runtime behavior changes.
