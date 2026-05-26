#!/usr/bin/env bash
set -euo pipefail

public_paths=(
  README.md
  website
  docs/getting-started
  docs/reference/cli
  docs/concepts
  install/README.md
  ':(exclude)website/.output'
  ':(exclude)website/node_modules'
  ':(exclude)website/bun.lock'
)

stale_pattern='isengard\.route\.public|expose\.host|isengard\.expose\.host|isd login|credentials\.toml|vallee\.casa|jellyfin|qbittorrent|plex|sonarr|radarr|immich|paperless|nextcloud|ghcr\.io/weavers-engineering/isengard-(controller|agent)|ghcr\.io/weavers-engineering/isengard(:|@)'

if git grep -nIE "$stale_pattern" -- "${public_paths[@]}"; then
  echo "public docs contain stale routing/auth vocabulary or non-generic examples" >&2
  exit 1
fi

entrypoint_paths=(
  README.md
  website
  docs/getting-started
  install/README.md
  ':(exclude)website/.output'
  ':(exclude)website/node_modules'
  ':(exclude)website/bun.lock'
)

placeholder_pattern='\[\[|TODO|TBD|Placeholder\.|placeholder text|lorem ipsum|coming soon|replace me'

if git grep -nIE "$placeholder_pattern" -- "${entrypoint_paths[@]}"; then
  echo "public entrypoints contain placeholders or internal wikilinks" >&2
  exit 1
fi
