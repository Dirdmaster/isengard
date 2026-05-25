---
seo:
  title: Isengard documentation
  description: Docker-native orchestration for personal infrastructure. One binary, one declarative stack file, fleet-wide deploys.
---

::u-page-hero
#title
Docker-native orchestration for personal infrastructure.

#description
Isengard ships as a single `isd` binary. Declare your fleet in a stack file, deploy across hosts, route traffic with labels, observe everything from one controller.

#links
  :::u-button
  ---
  color: neutral
  size: xl
  to: /getting-started/install
  trailing-icon: i-lucide-arrow-right
  ---
  Get started
  :::

  :::u-button
  ---
  color: neutral
  icon: simple-icons-github
  size: xl
  to: https://github.com/Weavers-Engineering/Isengard
  variant: outline
  ---
  Star on GitHub
  :::
::

::u-page-section
#title
What you get

#features
  :::u-page-feature
  ---
  icon: i-lucide-package
  ---
  #title
  One binary, one install
  
  #description
  `isd` is the CLI, the LSP server (`isd lsp`), the MCP server (`isd mcp`), and the embedded docs and skills. Install once, no extra services.
  :::

  :::u-page-feature
  ---
  icon: i-lucide-network
  ---
  #title
  Declarative fleet stacks
  
  #description
  Compose files plus an `isengard.toml` overlay describe routing, policy, and per-host placement. Apply the same file from any operator workstation.
  :::

  :::u-page-feature
  ---
  icon: i-lucide-shield-check
  ---
  #title
  Trust on first use, by fingerprint
  
  #description
  Agents enroll via a short-lived join token that pins the controller CA by SHA-256. No long-lived secrets, no shared PKI to babysit.
  :::

  :::u-page-feature
  ---
  icon: i-lucide-route
  ---
  #title
  Label-driven routing
  
  #description
  Add `isengard.expose=whoami.isengard.app` to a service and the agent reports the route to the controller. The common case needs one label; doctor asks only when the port is ambiguous.
  :::

  :::u-page-feature
  ---
  icon: i-lucide-bot
  ---
  #title
  AI-ready out of the box
  
  #description
  The local `isd mcp` server exposes the embedded docs and skill library to any MCP-capable LLM host. Same markdown that ships to the website.
  :::

  :::u-page-feature
  ---
  icon: i-lucide-code-2
  ---
  #title
  Rust core, plugin-first
  
  #description
  Backup, notifications, dashboards, and networking adapters are all plugins. The core stays small; the surface you actually need is opt-in.
  :::
::
