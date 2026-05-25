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
