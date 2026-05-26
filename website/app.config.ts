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
  // Disable Docus assistant widgets. Isengard exposes docs through
  // the local `isd mcp` integration instead of a website chat box.
  assistant: {
    floatingInput: false,
    explainWithAi: false,
  },
})
