export default defineAppConfig({
  ui: {
    colors: {
      primary: 'emerald',
      neutral: 'slate',
    },
  },
  seo: {
    title: 'Isengard',
    description: 'Docker-native orchestration for personal infrastructure.',
  },
  header: {
    title: 'Isengard',
  },
  socials: {
    github: 'https://github.com/Weavers-Engineering/Isengard',
  },
  github: {
    url: 'https://github.com/Weavers-Engineering/Isengard',
    branch: 'next',
    rootDir: 'docs',
  },
  docus: {
    locale: 'en',
  },
  // Phase 3 ships the scaffold only. The AI assistant surface is provided
  // by `isd mcp` locally (see Phase 4), not the website. Disable the
  // built-in Docus assistant so we don't ship a half-wired chat box.
  assistant: {
    floatingInput: false,
    explainWithAi: false,
  },
})
