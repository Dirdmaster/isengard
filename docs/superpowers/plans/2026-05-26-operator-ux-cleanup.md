# Operator UX Cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship Phase A of the operator UX cleanup: coherent route vocabulary, filled onboarding docs, root help guidance, and a public-doc stale-vocabulary guard.

**Architecture:** Keep runtime behavior unchanged. Treat docs, website copy, CLI help rendering, and guard scripts as separate units so each can be reviewed and verified independently. The only Rust change is the root help renderer plus tests.

**Tech Stack:** Rust, clap, shell, Justfile, Markdown, Nuxt content markdown.

---

## File Structure

- Modify `website/content/index.md`: replace stale route feature copy with `isengard.expose=<hostname>`.
- Modify `README.md`: rewrite current operator CLI section around Docker contexts and controller discovery.
- Modify `docs/getting-started/install.md`: first useful install and bootstrap path.
- Modify `docs/getting-started/first-stack.md`: tech/dev example stack using `whoami.isengard.app`.
- Modify `docs/getting-started/adding-a-route.md`: canonical label path plus imperative route escape hatch.
- Modify `docs/reference/cli/route.md`: route command mental model and examples.
- Modify `docs/reference/cli/stack.md`: stack command mental model, with doctor section.
- Modify `docs/concepts/labels.md`: use `*.isengard.app` tech/dev examples.
- Modify `crates/isd/src/help_render.rs`: add `Start here` block to root help.
- Create `scripts/ci/check_public_docs_vocabulary.sh`: fail when current public docs reintroduce stale routing or auth vocabulary, personal route domains, or consumer app examples.
- Modify `Justfile`: add `docs-check` recipe for the guard.
- Modify `.github/workflows/ci.yml`: run the docs guard in CI.

## Task 1: Root Help Start Block

**Files:**
- Modify: `crates/isd/src/help_render.rs`

- [ ] **Step 1: Extend help renderer tests first**

Add assertions to `render_contains_every_group_and_command`:

```rust
assert!(out.contains("Start here"));
assert!(out.contains("isd init"));
assert!(out.contains("isd stack deploy"));
assert!(out.contains("isd stack doctor"));
assert!(out.contains("isd route ls"));
```

- [ ] **Step 2: Run focused test and verify failure**

Run: `cargo test -p isd help_render::tests::render_contains_every_group_and_command`

Expected: FAIL because `Start here` is not rendered yet.

- [ ] **Step 3: Add the start block implementation**

Insert this helper near `render`:

```rust
fn start_here_block(color: bool) -> String {
    let cmd_name = Style::new().bold();
    let mut out = String::new();
    out.push_str("Start here\n");
    for (cmd, about) in [
        ("isd init", "bootstrap controller and first agent"),
        ("isd stack deploy", "deploy a compose stack"),
        ("isd stack doctor", "check expose labels before routing"),
        ("isd route ls", "inspect installed public routes"),
    ] {
        out.push_str("  ");
        out.push_str(&style(&cmd_name, cmd, color));
        for _ in cmd.len()..18 {
            out.push(' ');
        }
        out.push_str(about);
        out.push('\n');
    }
    out
}
```

Then call it in `render` after usage and before grouped commands:

```rust
out.push_str(&start_here_block(color));
out.push('\n');
```

- [ ] **Step 4: Run focused test and verify pass**

Run: `cargo test -p isd help_render::tests`

Expected: PASS, 2 tests.

- [ ] **Step 5: Commit root help**

Run:

```bash
git add crates/isd/src/help_render.rs
git commit -m "docs: add isd start here help"
```

## Task 2: Public Docs Rewrite

**Files:**
- Modify: `website/content/index.md`
- Modify: `README.md`
- Modify: `docs/getting-started/install.md`
- Modify: `docs/getting-started/first-stack.md`
- Modify: `docs/getting-started/adding-a-route.md`
- Modify: `docs/reference/cli/route.md`
- Modify: `docs/reference/cli/stack.md`
- Modify: `docs/concepts/labels.md`

- [ ] **Step 1: Update website feature copy**

Replace the routing card description with:

```markdown
Add `isengard.expose=whoami.isengard.app` to a service and the agent reports the route to the controller. The common case needs one label; doctor asks only when the port is ambiguous.
```

- [ ] **Step 2: Rewrite README operator CLI section**

Replace the stale `## Operator CLI (`isd`)` paragraph with:

```markdown
## Operator CLI (`isd`)

`isd` is the terminal companion to the dashboard. Docker contexts are the target selector and credential source: `isd --context prod ps` talks to the Docker host named `prod`, discovers the controller container by `io.isengard.role=controller`, and uses the controller REST API for orchestration commands.

For SSH-backed Docker contexts, `isd` reuses Docker's SSH transport and opens a local forward to the controller. No separate login step is required.

Start with:

```sh
isd init
isd ps
isd stack doctor
isd route ls
```
```

- [ ] **Step 3: Fill getting-started docs**

Use tech/dev examples only. The first-stack example should deploy `traefik/whoami` with:

```yaml
services:
  whoami:
    image: traefik/whoami:v1.10
    labels:
      isengard.expose: whoami.isengard.app
```

The docs must use `*.isengard.app` hostnames and avoid personal or consumer app examples.

- [ ] **Step 4: Fill route and stack reference pages**

`route.md` must cover:

```text
isd route ls
isd route add whoami.isengard.app --service whoami --port 80
isd route rm <id>
```

`stack.md` must cover:

```text
isd stack ls
isd stack deploy ./compose.yaml
isd stack doctor ./compose.yaml
isd stack doctor labels
```

- [ ] **Step 5: Update label concept examples**

Use this route table vocabulary:

```markdown
| `isengard.expose` | Default rule: public hostname | e.g. `whoami.isengard.app` |
```

Use this named-rule example:

```yaml
labels:
  isengard.expose.web: grafana.isengard.app
  isengard.expose.web.port: "3000"
  isengard.expose.admin: admin.isengard.app
  isengard.expose.admin.port: "9090"
```

- [ ] **Step 6: Check docs for dash and stale public examples manually**

Run:

```bash
grep -R "vallee\.casa\|jellyfin\|qbittorrent\|isengard\.route\.public\|expose\.host\|isd login\|credentials\.toml" README.md website/content docs/getting-started docs/reference/cli docs/concepts || true
```

Expected: no matches in current public docs.

- [ ] **Step 7: Commit public docs**

Run:

```bash
git add README.md website/content/index.md docs/getting-started/install.md docs/getting-started/first-stack.md docs/getting-started/adding-a-route.md docs/reference/cli/route.md docs/reference/cli/stack.md docs/concepts/labels.md
git commit -m "docs: refresh operator onboarding"
```

## Task 3: Public Docs Vocabulary Guard

**Files:**
- Create: `scripts/ci/check_public_docs_vocabulary.sh`
- Modify: `Justfile`
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Create the guard script**

Create `scripts/ci/check_public_docs_vocabulary.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

paths=(
  README.md
  website/content
  docs/getting-started
  docs/reference/cli
  docs/concepts
)

pattern='isengard\.route\.public|expose\.host|isd login|credentials\.toml|vallee\.casa|jellyfin|qbittorrent'

if grep -RInE "$pattern" "${paths[@]}"; then
  echo "public docs contain stale routing/auth vocabulary or non-generic examples" >&2
  exit 1
fi
```

- [ ] **Step 2: Make the script executable**

Run: `chmod +x scripts/ci/check_public_docs_vocabulary.sh`

- [ ] **Step 3: Add Justfile recipe**

Add near lint recipes:

```just
# Check current public docs for stale operator vocabulary
docs-check:
    bash scripts/ci/check_public_docs_vocabulary.sh
```

- [ ] **Step 4: Add CI step**

Add to the `lint` job after `cargo fmt --check`:

```yaml
      - name: public docs vocabulary
        run: bash scripts/ci/check_public_docs_vocabulary.sh
```

- [ ] **Step 5: Verify guard passes**

Run: `just docs-check`

Expected: no output, exit 0.

- [ ] **Step 6: Commit guard**

Run:

```bash
git add scripts/ci/check_public_docs_vocabulary.sh Justfile .github/workflows/ci.yml
git commit -m "ci: guard public docs vocabulary"
```

## Task 4: Final Verification

**Files:**
- Verify all modified files.

- [ ] **Step 1: Run focused tests**

Run:

```bash
cargo test -p isd help_render::tests
just docs-check
```

Expected: Rust tests pass and docs guard exits 0.

- [ ] **Step 2: Run formatting checks**

Run: `cargo fmt --check`

Expected: exit 0.

- [ ] **Step 3: Check forbidden Unicode dashes in changed files**

Run:

```bash
grep -RIn $'\u2014\|\u2013' README.md website/content docs/getting-started docs/reference/cli docs/concepts crates/isd/src/help_render.rs scripts/ci/check_public_docs_vocabulary.sh Justfile .github/workflows/ci.yml || true
```

Expected: no matches.

- [ ] **Step 4: Review git diff**

Run: `git diff --stat origin/next...HEAD`

Expected: spec, plan, docs, help renderer, guard, CI, and Justfile changes only.

- [ ] **Step 5: Push branch and open PR when requested**

Run only when ready to publish:

```bash
git push -u origin ux-operator-onboarding
gh pr create --repo Weavers-Engineering/Isengard --base next --head ux-operator-onboarding --title "docs: refresh operator onboarding" --body-file <body-file>
```

## Self-Review

- Spec coverage: Phase A website, README, getting-started docs, route reference, stack doctor reference, root help, and stale-vocabulary guard are covered.
- Placeholders: none.
- Scope: Phase B route wizard and Phase C scriptable doctor fixes remain follow-up behavior PRs, not part of this plan.
