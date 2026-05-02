// Tiny tokenizer for the docker run install command shown in the wizard.
// Hand-rolled instead of pulling in shiki/prism — this is one short multi-line
// command with a very predictable shape, and a 100KB syntax library would
// dwarf the use case.
//
// Tokens map to Tailwind classes in the consumer. Categories chosen for
// distinction in our iso-* palette, not for general shell-script accuracy:
//
//   binary   -> command name itself (e.g. "docker")
//   verb     -> docker subcommands (run)
//   flag     -> -d, --name, --restart=always, -v, -e, --group-add
//   string   -> value after a flag (isengard-agent, always)
//   path     -> filesystem paths and URL hosts (/var/run/...:/var/run/...)
//   env-key  -> ENV_VAR_NAME=
//   env-val  -> the value of an env var (URL, token)
//   image    -> ghcr.io/owner/name:tag
//   subshell -> $(stat -c %g /var/run/docker.sock)
//   continue -> trailing backslash + newline
//   text     -> punctuation + whitespace fallback

export type ShellToken = {
  type:
    | 'binary'
    | 'verb'
    | 'flag'
    | 'string'
    | 'path'
    | 'env-key'
    | 'env-val'
    | 'image'
    | 'subshell'
    | 'continue'
    | 'text'
  text: string
}

const FLAG_RE = /^(?:--?[a-zA-Z][\w-]*)/
const ENV_RE = /^([A-Z][A-Z0-9_]+)=(\S+)/
const SUBSHELL_RE = /^\$\([^)]+\)/
const PATH_RE = /^(?:\/[\w./:_-]+(?::\/[\w./:_-]+)?)/
const IMAGE_RE = /^[\w.\-]+\.[\w.\-]+\/[\w./_-]+(?::[\w.\-]+)?/
const WORD_RE = /^[\w./@:-]+/

export function tokenizeDockerCommand(src: string): ShellToken[] {
  const out: ShellToken[] = []
  let i = 0
  let atLineStart = true

  while (i < src.length) {
    const c = src[i]

    // Whitespace + newlines pass through as text tokens.
    if (c === '\n') {
      out.push({ type: 'text', text: '\n' })
      i += 1
      atLineStart = true
      continue
    }
    if (c === ' ' || c === '\t') {
      let j = i
      while (j < src.length && (src[j] === ' ' || src[j] === '\t')) j += 1
      out.push({ type: 'text', text: src.slice(i, j) })
      i = j
      continue
    }

    // Trailing line continuation: '\' at end of line.
    if (c === '\\' && (src[i + 1] === '\n' || i === src.length - 1)) {
      out.push({ type: 'continue', text: '\\' })
      i += 1
      continue
    }

    const rest = src.slice(i)

    // First token on first line is the binary; second is the verb.
    if (atLineStart && i === 0) {
      const m = WORD_RE.exec(rest)
      if (m) {
        out.push({ type: 'binary', text: m[0] })
        i += m[0].length
        atLineStart = false
        // Skip whitespace and grab the verb.
        while (src[i] === ' ') {
          out.push({ type: 'text', text: ' ' })
          i += 1
        }
        const v = WORD_RE.exec(src.slice(i))
        if (v) {
          out.push({ type: 'verb', text: v[0] })
          i += v[0].length
        }
        continue
      }
    }
    atLineStart = false

    // $(subshell)
    let m = SUBSHELL_RE.exec(rest)
    if (m) {
      out.push({ type: 'subshell', text: m[0] })
      i += m[0].length
      continue
    }

    // Flags. --restart=always splits into flag + '=' + string.
    m = FLAG_RE.exec(rest)
    if (m) {
      const flag = m[0]
      // '=value' pattern in long flags
      const after = rest.slice(flag.length)
      if (after.startsWith('=')) {
        out.push({ type: 'flag', text: flag })
        out.push({ type: 'text', text: '=' })
        i += flag.length + 1
        const valMatch = WORD_RE.exec(src.slice(i))
        if (valMatch) {
          out.push({ type: 'string', text: valMatch[0] })
          i += valMatch[0].length
        }
        continue
      }
      out.push({ type: 'flag', text: flag })
      i += flag.length
      continue
    }

    // Env var assignment KEY=VALUE
    m = ENV_RE.exec(rest)
    if (m) {
      out.push({ type: 'env-key', text: m[1] + '=' })
      out.push({ type: 'env-val', text: m[2] })
      i += m[0].length
      continue
    }

    // Image refs (registry/owner/name:tag)
    m = IMAGE_RE.exec(rest)
    if (m && m[0].includes('/')) {
      out.push({ type: 'image', text: m[0] })
      i += m[0].length
      continue
    }

    // Filesystem paths
    m = PATH_RE.exec(rest)
    if (m) {
      out.push({ type: 'path', text: m[0] })
      i += m[0].length
      continue
    }

    // Bare word
    m = WORD_RE.exec(rest)
    if (m) {
      out.push({ type: 'string', text: m[0] })
      i += m[0].length
      continue
    }

    // Anything else: pass through one char at a time.
    out.push({ type: 'text', text: c })
    i += 1
  }

  return out
}

export function shellTokenClass(type: ShellToken['type']): string {
  // Map each token type to a Tailwind text-color class. iso-* palette.
  switch (type) {
    case 'binary':
      return 'text-iso-text-primary font-semibold'
    case 'verb':
      return 'text-iso-success'
    case 'flag':
      return 'text-iso-info'
    case 'string':
      return 'text-iso-text-secondary'
    case 'path':
      return 'text-iso-warn'
    case 'env-key':
      return 'text-iso-info'
    case 'env-val':
      return 'text-iso-success'
    case 'image':
      return 'text-iso-warn'
    case 'subshell':
      return 'text-iso-text-muted italic'
    case 'continue':
      return 'text-iso-text-faint'
    case 'text':
    default:
      return 'text-iso-text-faint'
  }
}
