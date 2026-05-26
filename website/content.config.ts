import { existsSync, readdirSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { defineCollection, defineContentConfig, z } from '@nuxt/content'

/**
 * Isengard docs site overrides the default Docus content collections.
 *
 * Two trees are mounted from outside the website root:
 *
 * - `docs`: operator-facing guides at `../docs/**`, served at `/`. Overrides
 *   the Docus default `docs` collection so the bundled theme picks it up.
 * - `api`: per-crate API reference at `../crates/<crate>/docs/**`, served
 *   at `/api/`. A custom page renderer for `/api/*` lands in a later PR.
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

const ignoredApiDocDirs = new Set([
  '.git',
  '.nuxt',
  '.output',
  'dist',
  'node_modules',
  'target',
])

const discoverApiDocRoots = (root: string, prefix = ''): string[] => {
  const current = resolve(root, prefix)
  const roots: string[] = []

  for (const entry of readdirSync(current, { withFileTypes: true })) {
    if (!entry.isDirectory() || ignoredApiDocDirs.has(entry.name)) {
      continue
    }

    const relativePath = prefix ? `${prefix}/${entry.name}` : entry.name
    const absolutePath = resolve(root, relativePath)

    if (existsSync(resolve(absolutePath, 'docs'))) {
      roots.push(relativePath)
    }

    roots.push(...discoverApiDocRoots(root, relativePath))
  }

  return roots
}

const apiDocSources = discoverApiDocRoots(cratesRoot).map((cratePath) => ({
  cwd: resolve(cratesRoot, cratePath),
  include: 'docs/**/*.md',
  prefix: `/api/${cratePath}/docs`,
}))

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
        exclude: ['PLACEMENT.md', 'RELEASE_NOTES_*.md', 'RELEASES.md'],
      },
      schema: createDocsSchema(),
    }),
    api: defineCollection({
      type: 'page',
      source: apiDocSources,
      schema: createDocsSchema(),
    }),
  },
})
