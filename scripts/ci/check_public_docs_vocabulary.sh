#!/usr/bin/env bash
set -euo pipefail

public_paths=(
  README.md
  website
  docs
  install/README.md
  ':(exclude)docs/superpowers/**'
  ':(exclude)docs/PLACEMENT.md'
  ':(exclude)docs/RELEASES.md'
  ':(exclude)docs/RELEASE_NOTES_*.md'
  ':(exclude)website/.output'
  ':(exclude)website/node_modules'
  ':(exclude)website/bun.lock'
)

stale_pattern='isengard\.route\.public|expose\.host|isengard\.expose\.host|isd login|credentials\.toml|join-token --role|vallee\.casa|dirdmaster|jellyfin|qbittorrent|(^|[^[:alnum:]_-])plex([^[:alnum:]_-]|$)|sonarr|radarr|servarr|overseer|(^|[^[:alnum:]_-])torrent([^[:alnum:]_-]|$)|immich|paperless|nextcloud|ghcr\.io/weavers-engineering/isengard-(controller|agent)|ghcr\.io/weavers-engineering/isengard(:|@)'

check_forbidden() {
  local message="$1"
  local pattern="$2"
  shift 2

  local output
  local status
  set +e
  output=$(git grep -niIE -e "$pattern" -- "$@" 2>&1)
  status=$?
  set -e

  if [[ $status -eq 0 ]]; then
    printf '%s\n' "$output"
    echo "$message" >&2
    exit 1
  fi

  if [[ $status -eq 1 ]]; then
    return 0
  fi

  printf '%s\n' "$output" >&2
  echo "public docs guard failed while scanning" >&2
  exit "$status"
}

check_forbidden \
  "public docs contain stale routing/auth vocabulary or non-generic examples" \
  "$stale_pattern" \
  "${public_paths[@]}"

entrypoint_paths=(
  "${public_paths[@]}"
)

placeholder_pattern='\[\[[^]]*([[:space:]]|/)[^]]*\]\]|TODO|TBD|Placeholder\.|placeholder text|lorem ipsum|coming soon|replace me'

check_forbidden \
  "public entrypoints contain placeholders or internal wikilinks" \
  "$placeholder_pattern" \
  "${entrypoint_paths[@]}"
