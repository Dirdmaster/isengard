# Style and voice

Isengard speaks in one voice across every surface: the README, the website,
doc comments, hover popups, error messages, CLI output, skill bodies. The
voice is taste-loaded but never loud, direct without being curt, declarative
without being preachy. This document is the rule sheet.

When in doubt: cut a word, name a thing, ship the sentence.

## Twelve rules

1. **State things, don't promise them.** `Isengard does X.` Not `Isengard can do
   X` or `Isengard helps you X` or `Isengard tries to X`.
2. **Specific things over abstract things.** `Plex on port 32400.` Not `your
   media server`. `20 containers across 3 hosts.` Not `many services`.
3. **No softeners.** Banned: `just`, `simply`, `easily`, `intuitive`, `seamless`,
   `lightning`, `blazing`, `modern` (when used as a marketing claim),
   `effortless`, `friction-free`, `next-generation`, `revolutionary`.
4. **Direct address.** Speak to `you`, not `users`. Use `operator` only when
   naming the role on a UI element or in a label.
5. **Tradeoffs explicit.** A `Not for you if...` paragraph earns more trust
   than glossing. Name what Isengard isn't good at. Name who should reach
   for k3s or k8s or coolify instead.
6. **Imperatives are fine.** `Run isd init.` `Read the spec.` No `please`,
   no `you might want to`, no `feel free to`.
7. **Colons, periods, parens, commas. Never em dashes (U+2014) or en
   dashes (U+2013).** Lefthook rejects them. This is a vault-wide rule.
8. **Headlines 4-7 words.** Body paragraphs short. Bullets when the points
   are parallel. No prose monoliths.
9. **Conviction words earned, not filler.** `Insane`, `fits in your head`,
   `no apologies`, `the whole thing`, `you'll wonder why you ever`. Reserve
   for moments where the claim is real and the reader will agree.
10. **Tolkien names stay as names.** Isengard. iso-controller. iso-agent.
    Don't extend the lore. Don't write "wielding the power of Saruman."
11. **Sentence rhythm punchy then longer then punchy.** Variable lengths.
    A four-word sentence next to a sixteen-word one reads with cadence.
12. **One emoji rule.** Zero in code, zero in commits, zero in static docs,
    zero in markdown bodies. Earned in CLI output where they mark a real
    transition: `✓` on init complete, `✗` on enrollment fail. Earned in
    chat / log output where they tag emotional state.

## Earned words

These are loaded. Use them sparingly. Each one signals a stance; if you
use them everywhere they stop meaning anything.

- `insane`, `wild`, `wrong`: a take, not a description.
- `no apologies`: the design choice was deliberate.
- `the whole thing`: closes a description that was complete.
- `fits in your head`: the surface is small.
- `the operator's lab`: the homelab framing.
- `state of the art`: only when it actually is.

## Common patterns

### Headlines

| Bad | Good |
|---|---|
| Easily deploy your containers | Deploy containers across hosts. One binary. |
| Modern container orchestration | Compose semantics, scaled out. |
| Lightning-fast routing | Pingora routes traffic on every host. |
| Powerful and intuitive | One CLI. Three concepts. |

### Body prose

Bad:
> Isengard provides a seamless way to orchestrate your containerized
> workloads across multiple hosts with intuitive configuration and modern
> tooling.

Good:
> Isengard orchestrates containers across hosts using compose semantics.
> One binary per host. Labels declare routing, policy, and lifecycle hooks.
> The CLI talks to the controller over the same SSH connection you already
> have to the host.

### Error messages (CLI)

Bad (the kind we scrubbed in PR #189):
> Track F fingerprint flow required; got a legacy token. Mint a new token
> with `isd join-token` and re-run `isd join`.

Good:
> Token is missing the controller CA fingerprint. Mint a fresh token with
> `isd join-token` and re-run `isd join`.

The fix: name the problem in operator vocabulary. Drop internal scaffolding
("Track F"). Tell them what to do next.

### Doc comments (Rust `///`)

Bad:
> /// Pre-enroll fingerprint verify. Calls the unauthenticated GetCaPem RPC
> /// over a skip-verify TLS channel, compares the served CA's SHA-256
> /// against the fingerprint embedded in the packed token, and returns
> /// the verified PEM bytes on match.

Good:
> /// Verifies the controller CA before any authenticated RPC.
> ///
> /// The agent has no trust anchor yet; this call gets one over an
> /// untrusted channel and checks its fingerprint against the join
> /// token. Returns the verified PEM the caller pins for `Enroll`.
> ///
> /// # Errors
> ///
> /// Returns `Err` when the fingerprint doesn't match. The mismatch
> /// is fatal: there's no recovery short of re-minting the token.

Same content, structured. Headline first sentence. Body explains the why.
`# Errors` when the error matters.

### Doc comments (Markdown `#[doc = include_str!("...")]`)

When a doc body exceeds ~15 lines, move it to a sibling `crates/<crate>/docs/<symbol>.md`.
The markdown follows the same rules as inline `///`: headline, body, `# Examples`,
`# Errors`, `# Panics` only when relevant.

### Recipes (operator playbooks)

Imperative, numbered, no exposition. Each step does one thing. The
operator opens a recipe to get unstuck; they read top to bottom and act.

Bad:
> ## Adding a new host
>
> Adding a new host to your Isengard cluster is a straightforward process.
> First, you'll need to make sure your existing controller is running and
> healthy. Then, on the new host you want to add...

Good:
> ## Add a host to the cluster
>
> 1. On the controller host: `isd join-token --role agent`. Copy the printed
>    `isd join` command.
> 2. On the new host: paste that command. Wait for `agent enrolled`.
> 3. `isd ps` from anywhere confirms the new host.

### Skills (AI playbooks)

Same imperative shape as recipes, but front-matter declares parameters
the LLM should ask for. The body assumes the LLM is doing the work, not
the operator; phrase steps for execution, not for reading.

```markdown
---
title: Add a route
parameters:
  service_name:
    description: Container name or compose service to route to.
    required: true
  public_hostname:
    description: Hostname the route exposes (e.g. plex.vallee.casa).
    required: true
returns: The created routing rule id.
---

# Add a route

1. Resolve the docker context via `isd context show`.
2. Confirm `service_name` exists in `isd ps`.
3. Run `isd route create <public_hostname> --service <service_name>`.
4. Verify with `isd route list` (the new row appears with `state=ready`).
```

## Banned word list (the explicit version)

`just`, `simply`, `easily`, `intuitive`, `seamless`, `lightning`, `blazing`,
`modern` (as marketing), `effortless`, `friction-free`, `next-generation`,
`revolutionary`, `cutting-edge`, `best-in-class`, `world-class`, `unleash`,
`empower`, `unlock`, `industry-leading`, `enterprise-grade`,
`production-ready` (overused; prove it instead).

## Reviewers' checklist

Before a doc PR merges, reviewer scans for:

- Em dashes (lefthook catches; double-check anyway).
- Words from the banned list.
- Sentences with `just`, `simply`, `easily` slipped in unconsciously.
- `# Examples` blocks with no actual example.
- Promotional adjectives stacked together (e.g. `powerful intuitive seamless`).
- Tolkien lore beyond names.
- More than zero emoji in static docs.

Comment with the rule number when calling out a violation.

## When to break the rules

A rule serves the voice; the voice serves the reader. If a sentence
reads better with `just` in it, and removing `just` breaks the rhythm,
keep `just` and move on. Don't apply the rules dogmatically.

The rules describe the floor, not the ceiling.
