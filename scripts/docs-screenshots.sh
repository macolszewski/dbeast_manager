#!/usr/bin/env bash
# Copies the screenshots referenced by the documentation from the UI test
# output (docs/screenshots/, not committed) into docs/images/ (committed).
# Run the UI tests first, with the test databases up: ./scripts/test-dbs.sh && cargo test
set -euo pipefail
cd "$(dirname "$0")/.."

referenced=$(grep -rhoE 'images/[A-Za-z0-9._-]+\.png' docs README.md --include='*.md' | sed 's#.*images/##' | sort -u)
missing=0
for name in $referenced; do
  case "$name" in favicon.png) continue ;; esac
  if [ -f "docs/screenshots/$name" ]; then
    cp "docs/screenshots/$name" "docs/images/$name"
  elif [ ! -f "docs/images/$name" ]; then
    echo "missing screenshot: $name (no UI test writes it)" >&2
    missing=1
  fi
done
# Smaller repository and faster pages; the screenshots stay visually identical.
if command -v pngquant >/dev/null; then
  for name in $referenced; do
    [ -f "docs/images/$name" ] && [ "$name" != favicon.png ] && \
      pngquant --quality 85-98 --skip-if-larger --force --ext .png "docs/images/$name" 2>/dev/null || true
  done
fi
# Images no page references any more.
for file in docs/images/*.png; do
  name=$(basename "$file")
  case "$name" in favicon.png) continue ;; esac
  grep -qx "$name" <<<"$referenced" || { echo "removing unused docs/images/$name"; rm "$file"; }
done
exit "$missing"
