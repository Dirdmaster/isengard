# Phase 5a: Dashboard Scaffold + Bundle Pipeline

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** End state: `cargo build --workspace` produces an isengard binary that, when run as `controller`, loads a `dashboard` plugin which spawns an axum server on `127.0.0.1:9418` serving an embedded Nuxt 3 static bundle. Visiting `http://localhost:9418` shows a placeholder Nuxt page with "Isengard Dashboard" + the iso-* design tokens loaded. No API yet, no real UI yet — just the pipeline working end-to-end and producing a single binary.

**Architecture:** A new Nuxt 3 project lives at `crates/isengard-plugins/dashboard/web/`. `bun run build` produces a static SPA at `web/.output/public/`. A `build.rs` in the dashboard Rust crate runs the bun build when web sources are newer. `rust-embed` 8 bakes the bundle into the `.rlib`. The dashboard plugin's `start()` mounts an axum router serving the embedded files (with SPA fallback for client-side routes) and binds it to `0.0.0.0:9418` (configurable). Plugin lifecycle integrates with the controller plugin host (Phase 4d).

**Tech stack additions:** `axum` 0.8, `rust-embed` 8.5, `mime_guess` 2.0, `tower` 0.5 (already a workspace dep). Bun 1.x as a build-time dependency. Nuxt 3.14, Tailwind CSS 4, Vue 3.5, Pinia.

**Branch:** `next`. Lefthook pre-push runs full gates.

**Spec:** `docs/superpowers/specs/2026-05-01-phase-5-dashboard-design.md` §2.

---

## Scope

**In:**
- `crates/isengard-plugins/dashboard/` becomes a real crate (was an empty Phase 0 stub)
- Workspace deps: `axum`, `rust-embed`, `mime_guess`
- Bun-managed Nuxt 3 project at `dashboard/web/` with Tailwind 4
- `nuxt.config.ts` configured for static-site generation (`nitro.preset: 'static'`)
- Single placeholder page with iso-* design tokens visible (just the brand mark + a colored test grid proving Tailwind config works)
- `tailwind.config.ts` mirrors the 35 design tokens from `design/app.pen` (color palette + spacing + radii + type scale)
- Inter + JetBrains Mono fonts via @fontsource (or CDN fallback)
- `build.rs` in dashboard crate that runs `bun install && bun run build` on cargo build (with timestamp gate to skip if not needed)
- `rust-embed` derive on a `WebAssets` struct embedding `web/.output/public/`
- Axum router with two routes: `/_nuxt/*path` (asset serving) and `/*` (SPA fallback to embedded `index.html`)
- Dashboard plugin's `start()` binds to `bind_addr` from config (default `127.0.0.1:9418`), spawns axum server, stores `JoinHandle` for shutdown
- Dashboard plugin's `stop()` aborts the server task
- `inventory::submit!` registration with `Capability::Controller`
- Wired into the `isengard` binary (force-link via `use isengard_plugin_dashboard as _`)
- CI installs Bun via `oven-sh/setup-bun@v1` step before `cargo build`
- Manual smoke documented: build + run controller + open `http://localhost:9418` + see placeholder page

**Out (deferred to 5b–5e):**
- Any actual API endpoints (5b)
- WebSocket (5b)
- Real Vue pages beyond placeholder (5c)
- Pinia stores (5b)
- Authentication (v1.x)

**Done when:**
1. `cargo build --workspace` clean
2. `cargo nextest run --workspace` ≥ 142 baseline (plus any new dashboard unit tests; no regressions)
3. `just ci-local` clean (cargo-deny mandatory)
4. `bun --cwd crates/isengard-plugins/dashboard/web run build` succeeds and produces `web/.output/public/index.html`
5. Manual smoke: `ISENGARD_TOKEN=test ./target/debug/isengard controller --listen 127.0.0.1:9417 --state-dir /tmp/isengard-5a` runs without crash. In another terminal: `curl -s http://localhost:9418/ | grep "Isengard Dashboard"` returns a match.
6. Tag `v0.1.0-alpha.phase5a` set locally
7. **Not pushed** until user confirms

---

## File Structure

```
Cargo.toml                                          # MODIFY: + axum, rust-embed, mime_guess workspace deps

crates/isengard-plugins/dashboard/
├── Cargo.toml                                      # MODIFY: real deps (was empty)
├── build.rs                                        # NEW: runs `bun install && bun run build` if web is newer
├── README.md                                       # NEW: dev workflow docs (terminal 1: bun run dev; terminal 2: cargo run)
├── src/
│   └── lib.rs                                      # NEW: Plugin impl + axum router + WebAssets embed
└── web/
    ├── package.json                                # NEW: bun-managed
    ├── bun.lock                                    # NEW (generated)
    ├── nuxt.config.ts                              # NEW: static preset
    ├── tailwind.config.ts                          # NEW: iso-* tokens
    ├── app.vue                                     # NEW: root layout
    ├── pages/
    │   └── index.vue                               # NEW: placeholder home
    ├── assets/
    │   └── css/
    │       └── main.css                            # NEW: tailwind imports + base styles
    └── public/
        └── (favicon, etc — minimal)

crates/isengard/
├── Cargo.toml                                      # MODIFY: + isengard-plugin-dashboard dep
└── src/main.rs                                     # MODIFY: + use isengard_plugin_dashboard as _;

.github/workflows/ci.yml                            # MODIFY: + bun setup step before cargo build

.gitignore                                          # MODIFY: + dashboard/web/node_modules + .output
```

---

## Task 1: Workspace deps + dashboard crate Cargo.toml

**Files:**
- Modify: `Cargo.toml` (workspace `[workspace.dependencies]`)
- Modify: `crates/isengard-plugins/dashboard/Cargo.toml`

- [ ] **Step 1: Add axum + rust-embed + mime_guess to workspace deps**

In `Cargo.toml`, append below the existing `chrono` line (around line 70):

```toml
# HTTP server (dashboard)
axum = { version = "0.8.1", default-features = false, features = ["http1", "http2", "tokio", "json", "ws", "macros"] }

# embedded static assets (dashboard bundle)
rust-embed = { version = "8.5.0", features = ["compression"] }

# mime type detection for embedded asset serving
mime_guess = "2.0.5"
```

- [ ] **Step 2: Replace dashboard Cargo.toml**

Replace the existing minimal `crates/isengard-plugins/dashboard/Cargo.toml` with:

```toml
[package]
name = "isengard-plugin-dashboard"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
authors.workspace = true
repository.workspace = true
description = "Dashboard plugin for Isengard — Nuxt 3 SPA + axum"

[dependencies]
anyhow.workspace = true
async-trait.workspace = true
axum.workspace = true
inventory.workspace = true
isengard-controller.workspace = true
isengard-core.workspace = true
mime_guess.workspace = true
rust-embed.workspace = true
serde.workspace = true
serde_json.workspace = true
tokio = { workspace = true }
tower.workspace = true
tracing.workspace = true
```

Add the dashboard crate as an internal workspace dep in root `Cargo.toml` `[workspace.dependencies]` (mirror the pattern from notifier/updater):

```toml
isengard-plugin-dashboard = { path = "crates/isengard-plugins/dashboard", version = "0.1.0-alpha" }
```

- [ ] **Step 3: Build (Rust side will fail until we add the build.rs + lib.rs in later tasks; just confirm deps resolve)**

```bash
cd ~/Projects/isengard && cargo check -p isengard-plugin-dashboard 2>&1 | tail -10
```

Expected: errors about missing `lib.rs`, NOT errors about missing crates. If a crate isn't found, fix the workspace dep entry.

- [ ] **Step 4: Commit**

```bash
cd ~/Projects/isengard && git add Cargo.toml Cargo.lock crates/isengard-plugins/dashboard/Cargo.toml
cd ~/Projects/isengard && git commit -m "chore(deps): add axum + rust-embed + mime_guess for dashboard plugin"
```

**Self-review checklist:**
- [ ] Workspace deps + dashboard Cargo.toml updated
- [ ] `Cargo.lock` staged
- [ ] No `Co-Authored-By` trailer

---

## Task 2: Nuxt 3 + Tailwind 4 scaffold

**Files:**
- Create: `crates/isengard-plugins/dashboard/web/package.json`
- Create: `crates/isengard-plugins/dashboard/web/nuxt.config.ts`
- Create: `crates/isengard-plugins/dashboard/web/tailwind.config.ts`
- Create: `crates/isengard-plugins/dashboard/web/app.vue`
- Create: `crates/isengard-plugins/dashboard/web/pages/index.vue`
- Create: `crates/isengard-plugins/dashboard/web/assets/css/main.css`
- Create: `crates/isengard-plugins/dashboard/web/.gitignore`

- [ ] **Step 1: Initialize Nuxt with bun**

```bash
cd ~/Projects/isengard/crates/isengard-plugins/dashboard
bun create nuxt@latest web --no-install
cd web
bun install
```

This creates a baseline Nuxt 3 project. Now overwrite the generated configs with our specific setup.

- [ ] **Step 2: Replace package.json**

```json
{
  "name": "isengard-dashboard",
  "private": true,
  "type": "module",
  "scripts": {
    "build": "nuxt generate",
    "dev": "nuxt dev --port 3000",
    "preview": "nuxt preview"
  },
  "dependencies": {
    "@pinia/nuxt": "^0.7.0",
    "nuxt": "^3.14.0",
    "pinia": "^2.2.0",
    "vue": "^3.5.0",
    "vue-router": "^4.4.0"
  },
  "devDependencies": {
    "@iconify-json/lucide": "^1.2.0",
    "@nuxtjs/tailwindcss": "^6.12.0",
    "@nuxt/icon": "^1.6.0",
    "@fontsource/inter": "^5.1.0",
    "@fontsource/jetbrains-mono": "^5.1.0",
    "tailwindcss": "^3.4.0"
  }
}
```

(Note: Tailwind 4 alpha had migration friction at the time of writing. Pinning to `tailwindcss 3.4.0` for stability. Migration to v4 in v1.x.)

```bash
cd ~/Projects/isengard/crates/isengard-plugins/dashboard/web
bun install
```

- [ ] **Step 3: Replace nuxt.config.ts**

```typescript
export default defineNuxtConfig({
  compatibilityDate: '2026-05-01',
  ssr: false,  // SPA mode — Rust serves the static bundle
  modules: [
    '@nuxtjs/tailwindcss',
    '@nuxt/icon',
    '@pinia/nuxt',
  ],
  css: [
    '@fontsource/inter/400.css',
    '@fontsource/inter/500.css',
    '@fontsource/inter/600.css',
    '@fontsource/jetbrains-mono/400.css',
    '@fontsource/jetbrains-mono/500.css',
    '~/assets/css/main.css',
  ],
  app: {
    head: {
      title: 'Isengard Dashboard',
      meta: [
        { name: 'viewport', content: 'width=device-width, initial-scale=1' },
        { name: 'description', content: 'Container fleet management' },
      ],
    },
  },
  nitro: {
    preset: 'static',
  },
  // Dev server proxies /api and /ws to the Rust backend on 9418
  vite: {
    server: {
      proxy: {
        '/api': 'http://localhost:9418',
        '/ws': { target: 'ws://localhost:9418', ws: true },
      },
    },
  },
})
```

- [ ] **Step 4: Create tailwind.config.ts with iso-* tokens**

```typescript
import type { Config } from 'tailwindcss'

export default {
  content: [
    './components/**/*.{vue,js,ts}',
    './layouts/**/*.vue',
    './pages/**/*.vue',
    './plugins/**/*.{js,ts}',
    './app.vue',
    './error.vue',
  ],
  theme: {
    extend: {
      colors: {
        iso: {
          'bg-base': '#0b0d0f',
          'bg-elevated': '#0e1114',
          'bg-overlay': '#15181b',
          'bg-row-hover': '#11151a',
          'bg-selected': '#0f1a12',
          'border-subtle': '#1c2024',
          'border-strong': '#2a2f35',
          'text-primary': '#e6e8eb',
          'text-secondary': '#d8dde2',
          'text-muted': '#8a9099',
          'text-faint': '#6f7680',
          success: '#4ade80',
          'success-soft': '#1e3826',
          warn: '#fbbf24',
          error: '#f87171',
          info: '#c084fc',
          neutral: '#94a3b8',
        },
        terminal: {
          bg: '#050505',
        },
      },
      fontFamily: {
        sans: ['Inter', 'system-ui', 'sans-serif'],
        mono: ['JetBrains Mono', 'ui-monospace', 'monospace'],
      },
      fontSize: {
        'iso-xs': ['11px', { lineHeight: '14px' }],
        'iso-sm': ['12px', { lineHeight: '16px' }],
        'iso-base': ['13px', { lineHeight: '18px' }],
        'iso-md': ['14px', { lineHeight: '20px' }],
        'iso-lg': ['16px', { lineHeight: '22px' }],
      },
      spacing: {
        'iso-1': '4px',
        'iso-2': '8px',
        'iso-3': '12px',
        'iso-4': '16px',
        'iso-5': '20px',
        'iso-6': '24px',
      },
      borderRadius: {
        'iso-sm': '4px',
        'iso-md': '6px',
        'iso-lg': '8px',
      },
    },
  },
  plugins: [],
} satisfies Config
```

- [ ] **Step 5: Create app.vue**

```vue
<template>
  <div class="min-h-screen bg-iso-bg-base text-iso-text-primary font-sans antialiased">
    <NuxtPage />
  </div>
</template>
```

- [ ] **Step 6: Create pages/index.vue (placeholder)**

```vue
<template>
  <div class="p-8 max-w-4xl mx-auto">
    <header class="flex items-center gap-3 mb-8">
      <div class="w-3 h-3 rounded-full bg-iso-success"></div>
      <h1 class="text-2xl font-semibold tracking-tight">Isengard Dashboard</h1>
      <span class="text-iso-text-faint text-iso-sm">Phase 5a · scaffold</span>
    </header>

    <section class="space-y-4 mb-12">
      <p class="text-iso-text-secondary">
        Bundle pipeline working. This page is served by the embedded Nuxt 3 SPA.
      </p>
      <p class="text-iso-text-muted text-iso-sm">
        Real UI lands in 5b/5c/5d/5e. This is the foundation.
      </p>
    </section>

    <section>
      <h2 class="text-iso-sm uppercase tracking-wider text-iso-text-faint mb-3">Design tokens</h2>
      <div class="grid grid-cols-6 gap-2">
        <div v-for="t in tokens" :key="t.name" class="rounded-iso-md p-3 border border-iso-border-subtle" :style="{ backgroundColor: t.color }">
          <div class="text-iso-xs font-mono" :class="t.darkText ? 'text-black' : 'text-iso-text-primary'">{{ t.name }}</div>
        </div>
      </div>
    </section>
  </div>
</template>

<script setup lang="ts">
const tokens = [
  { name: 'success', color: '#4ade80', darkText: true },
  { name: 'warn', color: '#fbbf24', darkText: true },
  { name: 'error', color: '#f87171', darkText: true },
  { name: 'info', color: '#c084fc', darkText: true },
  { name: 'neutral', color: '#94a3b8', darkText: true },
  { name: 'overlay', color: '#15181b', darkText: false },
]
</script>
```

- [ ] **Step 7: Create assets/css/main.css**

```css
@tailwind base;
@tailwind components;
@tailwind utilities;

html, body {
  background: #0b0d0f;
  color: #e6e8eb;
  font-family: Inter, system-ui, sans-serif;
}
```

- [ ] **Step 8: Create web/.gitignore**

```
node_modules
.output
.nuxt
dist
*.log
```

- [ ] **Step 9: Test the build**

```bash
cd ~/Projects/isengard/crates/isengard-plugins/dashboard/web
bun run build 2>&1 | tail -10
```

Expected: success, generated `.output/public/index.html` exists.

```bash
ls -la .output/public/index.html
```

Should show the file.

- [ ] **Step 10: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/dashboard/web
cd ~/Projects/isengard && git commit -m "feat(dashboard): scaffold Nuxt 3 + Tailwind 4 with iso-* design tokens"
```

**Self-review checklist:**
- [ ] `bun run build` succeeds
- [ ] `.output/public/index.html` exists
- [ ] node_modules + .output ignored from git
- [ ] No `Co-Authored-By` trailer

---

## Task 3: build.rs — auto-build the bundle when sources change

**Files:**
- Create: `crates/isengard-plugins/dashboard/build.rs`

- [ ] **Step 1: Create build.rs**

```rust
//! Build script: run `bun run build` in `web/` if web sources are newer than
//! `web/.output/public/index.html`. Skip silently if `bun` is not installed
//! (CI is expected to install it; local dev can build the bundle manually with
//! `bun run build` if needed).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let web_dir = manifest_dir.join("web");
    let output_index = web_dir.join(".output").join("public").join("index.html");

    // Tell cargo when to re-run this build script.
    println!("cargo:rerun-if-changed=web/package.json");
    println!("cargo:rerun-if-changed=web/nuxt.config.ts");
    println!("cargo:rerun-if-changed=web/tailwind.config.ts");
    println!("cargo:rerun-if-changed=web/app.vue");
    rerun_dir(&web_dir.join("pages"));
    rerun_dir(&web_dir.join("components"));
    rerun_dir(&web_dir.join("composables"));
    rerun_dir(&web_dir.join("stores"));
    rerun_dir(&web_dir.join("assets"));

    // Skip if bun isn't installed. The bundle must already exist (CI built
    // it, or developer ran `bun run build` manually).
    let bun_available = Command::new("bun").arg("--version").output().is_ok();
    if !bun_available {
        if !output_index.exists() {
            panic!(
                "bun not found AND no pre-built bundle at {}. Install bun (https://bun.sh) or run `bun run build` in {}.",
                output_index.display(),
                web_dir.display()
            );
        }
        println!("cargo:warning=bun not found; using pre-built bundle at {}", output_index.display());
        return;
    }

    // Compare timestamps: skip rebuild if output is newer than all watched sources.
    if let Some(out_ts) = mtime(&output_index) {
        let needs_rebuild = walk_for_newer(&web_dir.join("pages"), out_ts)
            || walk_for_newer(&web_dir.join("components"), out_ts)
            || walk_for_newer(&web_dir.join("composables"), out_ts)
            || walk_for_newer(&web_dir.join("stores"), out_ts)
            || walk_for_newer(&web_dir.join("assets"), out_ts)
            || mtime(&web_dir.join("package.json")).map_or(false, |t| t > out_ts)
            || mtime(&web_dir.join("nuxt.config.ts")).map_or(false, |t| t > out_ts)
            || mtime(&web_dir.join("tailwind.config.ts")).map_or(false, |t| t > out_ts)
            || mtime(&web_dir.join("app.vue")).map_or(false, |t| t > out_ts);
        if !needs_rebuild {
            println!("cargo:warning=dashboard bundle up to date; skipping bun build");
            return;
        }
    }

    // Install if node_modules missing.
    if !web_dir.join("node_modules").exists() {
        run("bun", &["install"], &web_dir);
    }
    run("bun", &["run", "build"], &web_dir);

    // Verify output exists.
    if !output_index.exists() {
        panic!("bun run build completed but {} doesn't exist", output_index.display());
    }
}

fn rerun_dir(dir: &Path) {
    if !dir.exists() {
        return;
    }
    println!("cargo:rerun-if-changed={}", dir.display());
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

fn walk_for_newer(dir: &Path, ts: SystemTime) -> bool {
    if !dir.exists() {
        return false;
    }
    fn walk(dir: &Path, ts: SystemTime) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if walk(&path, ts) {
                    return true;
                }
            } else if mtime(&path).map_or(false, |t| t > ts) {
                return true;
            }
        }
        false
    }
    walk(dir, ts)
}

fn run(cmd: &str, args: &[&str], cwd: &Path) {
    let status = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn `{cmd} {}`: {e}", args.join(" ")));
    if !status.success() {
        panic!("`{cmd} {}` failed with {status}", args.join(" "));
    }
}
```

- [ ] **Step 2: Test build.rs runs**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-dashboard 2>&1 | tail -10
```

Expected: build runs (might produce warnings about no `lib.rs` yet — that's fine, build.rs runs first). The bundle should NOT rebuild (it was just built in Task 2).

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/dashboard/build.rs
cd ~/Projects/isengard && git commit -m "feat(dashboard): build.rs auto-runs bun build when web sources change"
```

---

## Task 4: src/lib.rs — Plugin impl + axum router + WebAssets embed

**Files:**
- Create: `crates/isengard-plugins/dashboard/src/lib.rs`

- [ ] **Step 1: Create lib.rs**

```rust
//! Isengard `dashboard` plugin (controller-side).
//!
//! Embeds a Nuxt 3 SPA bundle and serves it on a configurable HTTP port via
//! axum. v1 ships scaffold + bundle pipeline; API + WebSocket land in 5b.

#![allow(clippy::result_large_err)]

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::Path;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use isengard_core::{
    Capability, CoreError, Plugin, PluginContext, PluginRegistration, Result,
};
use rust_embed::RustEmbed;
use serde::Deserialize;
use tokio::task::JoinHandle;
use tracing::{info, warn};

const PLUGIN_NAME: &str = "dashboard";
const DEFAULT_BIND_ADDR: &str = "127.0.0.1:9418";

#[derive(RustEmbed)]
#[folder = "web/.output/public/"]
struct WebAssets;

#[derive(Debug, Deserialize, Default)]
struct DashboardConfig {
    #[serde(default)]
    bind_addr: Option<String>,
}

pub struct Dashboard {
    bind_addr: SocketAddr,
    server_task: Option<JoinHandle<()>>,
}

impl Dashboard {
    pub fn new() -> Self {
        Self {
            bind_addr: DEFAULT_BIND_ADDR.parse().unwrap(),
            server_task: None,
        }
    }
}

impl Default for Dashboard {
    fn default() -> Self {
        Self::new()
    }
}

fn init_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::InitFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

fn start_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::StartFailed {
        name: PLUGIN_NAME.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

fn build_router() -> Router {
    Router::new()
        .route("/_nuxt/{*path}", get(serve_asset_nuxt))
        .route("/", get(serve_index))
        .fallback(get(fallback_handler))
}

async fn serve_index() -> Response {
    serve_embedded("index.html").await
}

async fn serve_asset_nuxt(Path(path): Path<String>) -> Response {
    // Nuxt assets live under _nuxt/ in the bundle. Reconstruct path.
    let asset_path = format!("_nuxt/{path}");
    serve_embedded(&asset_path).await
}

async fn fallback_handler(uri: Uri) -> Response {
    // Try literal path first (favicon, robots.txt, etc), then SPA fallback.
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty() && WebAssets::get(path).is_some() {
        return serve_embedded(path).await;
    }
    // SPA fallback — any unknown route serves index.html so client-side routing handles it.
    serve_embedded("index.html").await
}

async fn serve_embedded(path: &str) -> Response {
    match WebAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .header(header::CACHE_CONTROL, cache_control_for(path))
                .body(Body::from(content.data.into_owned()))
                .unwrap()
        }
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from(format!("not found: {path}")))
            .unwrap(),
    }
}

fn cache_control_for(path: &str) -> &'static str {
    if path.starts_with("_nuxt/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

#[async_trait]
impl Plugin for Dashboard {
    fn name(&self) -> &'static str {
        PLUGIN_NAME
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, ctx: &PluginContext) -> Result<()> {
        let cfg: DashboardConfig = serde_json::from_value(ctx.config.clone())
            .map_err(|e| init_err(format!("parsing dashboard config: {e}")))?;

        let addr_str = cfg.bind_addr.as_deref().unwrap_or(DEFAULT_BIND_ADDR);
        self.bind_addr = addr_str
            .parse()
            .map_err(|e| init_err(format!("invalid bind_addr {addr_str}: {e}")))?;

        info!(addr = %self.bind_addr, "dashboard initialised");
        Ok(())
    }

    async fn start(&mut self, _ctx: &PluginContext) -> Result<()> {
        let app = build_router();
        let addr = self.bind_addr;

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| start_err(format!("bind {addr}: {e}")))?;

        let task = tokio::spawn(async move {
            info!(%addr, "dashboard server listening");
            if let Err(e) = axum::serve(listener, app).await {
                warn!(error = %e, "dashboard server ended");
            }
        });

        self.server_task = Some(task);
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(t) = self.server_task.take() {
            t.abort();
        }
        info!("dashboard stopped");
        Ok(())
    }
}

inventory::submit! {
    PluginRegistration {
        name: PLUGIN_NAME,
        capabilities: &[Capability::Controller],
        constructor: || Box::new(Dashboard::new()) as Box<dyn Plugin>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_new_has_default_bind_addr() {
        let d = Dashboard::new();
        assert_eq!(d.bind_addr.to_string(), "127.0.0.1:9418");
    }

    #[test]
    fn router_builds_without_panic() {
        let _ = build_router();
    }
}
```

- [ ] **Step 2: Build + test**

```bash
cd ~/Projects/isengard && cargo build -p isengard-plugin-dashboard 2>&1 | tail -10
cd ~/Projects/isengard && cargo test -p isengard-plugin-dashboard 2>&1 | tail -10
cd ~/Projects/isengard && cargo clippy -p isengard-plugin-dashboard --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: clean. 2 tests pass.

- [ ] **Step 3: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard-plugins/dashboard/src/lib.rs
cd ~/Projects/isengard && git commit -m "feat(dashboard): Plugin impl + axum router + rust-embed of Nuxt bundle"
```

---

## Task 5: Wire dashboard into binary + CI bun setup

**Files:**
- Modify: `crates/isengard/Cargo.toml`
- Modify: `crates/isengard/src/main.rs`
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Add dashboard dep to binary**

In `crates/isengard/Cargo.toml`, add to `[dependencies]`:

```toml
isengard-plugin-dashboard.workspace = true
```

- [ ] **Step 2: Force-link dashboard plugin in main.rs**

In `crates/isengard/src/main.rs`, alongside the existing `use isengard_plugin_notifier as _;` add:

```rust
#[allow(unused_imports)]
use isengard_plugin_dashboard as _;
```

- [ ] **Step 3: Add Bun setup to CI**

In `.github/workflows/ci.yml`, add a step BEFORE `cargo build`:

```yaml
- name: Setup Bun
  uses: oven-sh/setup-bun@v1
  with:
    bun-version: latest
```

- [ ] **Step 4: Build workspace**

```bash
cd ~/Projects/isengard && cargo build --workspace 2>&1 | tail -10
```

Expected: clean build. The dashboard plugin is now linked into the binary.

- [ ] **Step 5: Commit**

```bash
cd ~/Projects/isengard && git add crates/isengard/Cargo.toml crates/isengard/src/main.rs .github/workflows/ci.yml
cd ~/Projects/isengard && git commit -m "feat(bin): include dashboard plugin in agent binary; CI installs Bun"
```

---

## Task 6: Manual smoke + CI gate + tag

- [ ] **Step 1: `just ci-local`**

```bash
cd ~/Projects/isengard && just ci-local 2>&1 | tail -10
```

If `cargo fmt --check` fails: `cargo fmt`, commit as `style: cargo fmt across phase 5a`, re-run.

- [ ] **Step 2: Manual smoke**

```bash
cd ~/Projects/isengard
mkdir -p /tmp/isengard-5a-ctrl

ISENGARD_TOKEN=test ./target/debug/isengard controller --listen 127.0.0.1:9417 --state-dir /tmp/isengard-5a-ctrl &
CTRL=$!
sleep 1

# Dashboard should be on 9418 (default)
curl -s http://localhost:9418/ | grep "Isengard Dashboard"
curl -s http://localhost:9418/ | head -50

# Also try a deep route (SPA fallback)
curl -s http://localhost:9418/hosts | grep "Isengard Dashboard"

kill $CTRL 2>/dev/null
wait $CTRL 2>/dev/null || true
```

Expected: both curls return HTML containing "Isengard Dashboard". The `/hosts` route also returns the same index.html (SPA fallback), letting client-side routing handle it.

- [ ] **Step 3: Confirm test count**

```bash
cd ~/Projects/isengard && cargo test --workspace 2>&1 | grep -E "^test result" | awk '{sum+=$4; fails+=$6} END {print "Total passing:", sum, "| failures:", fails}'
```

Expected: ≥ 142 baseline + 2 dashboard tests = 144+. Critical: zero failures.

- [ ] **Step 4: Tag**

```bash
cd ~/Projects/isengard && git tag -a v0.1.0-alpha.phase5a -m "phase 5a: dashboard scaffold — Nuxt 3 + Tailwind + rust-embed + axum"
cd ~/Projects/isengard && git tag -l | grep phase5a
```

Don't push.

- [ ] **Step 5: Confirm done**

- [ ] `cargo build --workspace` clean
- [ ] `cargo test --workspace` ≥ 142 baseline + new tests, zero failures
- [ ] `just ci-local` clean
- [ ] Manual smoke: dashboard responds on `:9418`
- [ ] Tag `v0.1.0-alpha.phase5a` exists locally
- [ ] Nothing pushed

---

## Self-review

| Spec requirement (§2) | Plan task |
|---|---|
| Nuxt 3 + Tailwind + bun build pipeline | Task 2 |
| Static SSG (no SSR runtime) | Task 2 (`nitro.preset: 'static'`, `ssr: false`) |
| build.rs auto-runs bun build on Rust build | Task 3 |
| rust-embed bakes bundle into .rlib | Task 4 |
| Axum router serving embedded files + SPA fallback | Task 4 |
| Dashboard plugin lifecycle (init/start/stop) | Task 4 |
| Wired into binary | Task 5 |
| CI installs Bun | Task 5 |
| Manual smoke confirms end-to-end | Task 6 |

No API or WebSocket — explicitly deferred to 5b.

---

## Execution Handoff

Plan saved at `docs/superpowers/plans/2026-05-01-phase-5a-dashboard-scaffold.md`. Subagent-driven execution.
