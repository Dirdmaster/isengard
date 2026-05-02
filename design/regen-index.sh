#!/usr/bin/env bash
#
# Regenerate design/concepts/_index.html — a CSS Grid of all concept HTML files,
# scaled to thumbnails. Click a thumbnail to open the concept full-size.
#
# Run after adding or removing a concept:
#   bash design/regen-index.sh
#
# Or via just:
#   just design-index

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONCEPTS_DIR="$SCRIPT_DIR/concepts"
OUT="$CONCEPTS_DIR/_index.html"

# Find concept files (excluding underscored utility files), sort newest-first by name
CONCEPTS=$(find "$CONCEPTS_DIR" -maxdepth 1 -name "*.html" ! -name "_*.html" | sort -r)

cat > "$OUT" <<'HEAD'
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <title>Isengard concepts — index</title>
  <link rel="stylesheet" href="../tokens.css">
  <style>
    * { box-sizing: border-box; }
    body {
      margin: 0; padding: 32px;
      background: var(--iso-bg-base);
      color: var(--iso-text-primary);
      font-family: var(--iso-font-sans);
    }
    h1 { font-size: 22px; margin: 0 0 8px; font-weight: 600; }
    .meta { color: var(--iso-text-muted); font-size: 12px; margin-bottom: 32px; }
    .grid {
      display: grid;
      grid-template-columns: repeat(auto-fill, minmax(580px, 1fr));
      gap: 24px;
    }
    .card {
      background: var(--iso-bg-elevated);
      border: 1px solid var(--iso-border-subtle);
      border-radius: 8px;
      overflow: hidden;
      transition: border-color 0.15s;
    }
    .card:hover { border-color: var(--iso-border-strong); }
    .card .preview {
      width: 100%;
      aspect-ratio: 16 / 10;
      overflow: hidden;
      position: relative;
      background: var(--iso-bg-base);
      border-bottom: 1px solid var(--iso-border-subtle);
      display: block;
    }
    .card iframe {
      position: absolute; top: 0; left: 0;
      width: 1500px; height: 940px;
      border: 0;
      transform: scale(0.4);
      transform-origin: top left;
      pointer-events: none;
    }
    .card .info {
      display: flex; align-items: center; justify-content: space-between;
      padding: 12px 14px;
    }
    .card .name {
      font-size: 13px; font-weight: 500;
      color: var(--iso-text-primary);
    }
    .card .open {
      font-size: 11px;
      color: var(--iso-accent-info);
      text-decoration: none;
    }
    .card .open:hover { text-decoration: underline; }
  </style>
</head>
<body>
  <h1>Isengard concepts</h1>
  <p class="meta">Click any thumbnail to open the full-size concept.</p>
  <div class="grid">
HEAD

count=0
while IFS= read -r f; do
  [ -z "$f" ] && continue
  name=$(basename "$f")
  cat >> "$OUT" <<EOF
    <div class="card">
      <a href="$name" target="_blank" class="preview">
        <iframe src="$name"></iframe>
      </a>
      <div class="info">
        <span class="name">$name</span>
        <a href="$name" target="_blank" class="open">open →</a>
      </div>
    </div>
EOF
  count=$((count + 1))
done <<< "$CONCEPTS"

cat >> "$OUT" <<'TAIL'
  </div>
</body>
</html>
TAIL

echo "Wrote $OUT"
echo "Found $count concept(s)."
