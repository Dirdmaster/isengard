# Design

This directory holds Isengard's design system. Repo-native: every artifact lives here as a plain file (HTML, CSS, markdown). No Figma, no Penpot, no proprietary editor required.

> Methodology: see `1 Projects/Weave/` in the weavers-vault for the full convention and rationale.

## Map

| Path | What it is | When to touch it |
|---|---|---|
| `tokens.css` | CSS variables for colors, spacing, typography. Source of truth. | When the design language changes |
| `tw-config.js` | Tailwind config for concept HTML (CDN-friendly) | Almost never |
| `components.md` | Inventory of reusable Vue components (where they live, status) | When adding/removing/deprecating a component |
| `pages/*.md` | Page specs — what each page does, who uses it, key interactions | When defining a new page or its purpose changes |
| `concepts/*.html` | Throwaway visual mocks, date-prefixed | When sketching ideas |
| `concepts/_shell.html` | Template for new concepts | When the shell needs to change |
| `concepts/_index.html` | Auto-generated grid of all concepts | Re-run `regen-index.sh` after adding/removing concepts |
| `concepts/ARCHIVE/` | Old concepts kept for reference | When a concept is superseded but worth remembering |
| `decisions/*.md` | ADRs — why we chose X over Y | When making a significant design decision |
| `flows/*.md` | User journeys (markdown + mermaid) | When defining a multi-step user flow |
| `app.pen` | **Legacy** Pencil mocks (kept for archaeology) | Don't add new mocks here |

## Workflow

### New idea ("design a Y page")

1. Write `pages/Y.md` if it doesn't exist (intent first)
2. Sketch in `concepts/YYYY-MM-DD-Y-v1.html` using `concepts/_shell.html` as base
3. Iterate with `-v2.html`, `-v3.html` as needed
4. Compare via `concepts/_index.html` in the browser
5. When locked, write `decisions/YYYY-MM-DD-Y.md` (the why)
6. Build the real component in `crates/isengard-plugins/dashboard/web/components/`
7. Move losing concepts to `concepts/ARCHIVE/`

### Token change

1. Edit `tokens.css`
2. Tailwind config picks it up via the file
3. Every concept and every real component updates automatically

## Conventions

- **Date prefix everything time-bound**: concepts and ADRs use `YYYY-MM-DD-` prefix. Specs and component inventory don't.
- **Lowercase, hyphenated filenames**: `bottom-bar-cmdk.md`, not `BottomBarCmdk.md`.
- **One concept per file**: don't put 3 variants in one HTML; make 3 files.
- **Don't delete concepts**: archive them. The history of considered ideas is valuable.
- **Concepts use Tailwind iso-* classes**: same as the production Vue components, so promotion is copy-paste.

## Viewing concepts

```bash
# One-off (just open the file in your browser)
open design/concepts/_index.html

# Or serve it (port 4040 by convention)
cd design && python3 -m http.server 4040
# then visit http://localhost:4040/concepts/_index.html
```

## Promotion path (concept → real component)

The HTML body in `concepts/foo.html` uses the same Tailwind classes the production components use. To ship a concept:

1. Copy the body content from the concept HTML
2. Paste into a Vue component template at `crates/isengard-plugins/dashboard/web/components/Foo.vue`
3. Replace mock data with reactive props
4. Wire to the API or store

That's it. There is no "Figma export" or "design handoff".
