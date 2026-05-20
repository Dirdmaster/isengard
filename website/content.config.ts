import { fileURLToPath } from 'node:url'
import { dirname, resolve } from 'node:path'
import { defineCollection, defineContentConfig, z } from '@nuxt/content'

/**
 * Isengard docs site overrides the default Docus content collections.
 *
 * Two trees are mounted from outside the website root:
 *
 * - `docs`: operator-facing guides at `../docs/**`, served at `/`. Overrides
 *   the Docus default `docs` collection so the bundled theme picks it up.
 * - `api`: per-crate API reference at `../crates/<crate>/docs/**`, served
 *   at `/api/`. Populated by Phase 2 of the docs+AI plan. A custom page
 *   renderer for `/api/*` lands in a later PR.
 *
 * The `landing` collection stays pointed at `content/index.md` so the
 * homepage hero ships with the scaffold and can be edited in-tree.
 *
 * `cwd` is computed as an absolute path here because pathe.normalize()
 * collapses `~~/../docs` to `docs` before Nuxt Content's own `~~/` token
 * substitution runs. Computing the absolute path up front sidesteps that.
 */
const websiteRoot = dirname(fileURLToPath(import.meta.url))
const repoRoot = resolve(websiteRoot, '..')
const operatorDocsRoot = resolve(repoRoot, 'docs')
const cratesRoot = resolve(repoRoot, 'crates')

const createDocsSchema = () =>
  z.object({
    links: z
      .array(
        z.object({
          label: z.string(),
          icon: z.string(),
          to: z.string(),
          target: z.string().optional(),
        }),
      )
      .optional(),
  })

export default defineContentConfig({
  collections: {
    landing: defineCollection({
      type: 'page',
      source: {
        include: 'index.md',
      },
    }),
    docs: defineCollection({
      type: 'page',
      source: {
        cwd: operatorDocsRoot,
        include: '**/*.md',
        prefix: '/',
        exclude: ['PLACEMENT.md', 'RELEASE_NOTES_*.md', 'RELEASES.md', 'superpowers/**'],
      },
      schema: createDocsSchema(),
    }),
    api: defineCollection({
      type: 'page',
      source: {
        cwd: cratesRoot,
        include: '**/docs/**/*.md',
        prefix: '/api',
      },
      schema: createDocsSchema(),
    }),
  },
})
