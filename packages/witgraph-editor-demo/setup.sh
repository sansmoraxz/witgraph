#!/usr/bin/env bash
# Copies test components into components/ so the demo page can fetch them.
#
# Prerequisites:
#   cargo build -p test-components
#   cd packages/witgraph-loader && pnpm build:wasm
#
# Usage (from packages/witgraph-editor-demo):
#   bash setup.sh
#   # or: pnpm setup

set -euo pipefail
cd "$(dirname "$0")"

TARGET=$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
  | grep -o '"target_directory":"[^"]*"' \
  | head -1 \
  | cut -d'"' -f4)

if [ -z "$TARGET" ]; then
  echo "error: could not determine cargo target directory" >&2
  echo "       is Rust / Cargo installed?" >&2
  exit 1
fi

find_guest() {
  local name="$1"
  local found
  found=$(find "$TARGET/debug/build" \
    -path "*/test-components-*/out/${name}.wasm" \
    -printf '%T@ %p\n' 2>/dev/null \
    | sort -rn \
    | head -1 \
    | cut -d' ' -f2-)
  if [ -z "$found" ]; then
    echo "error: ${name}.wasm not found in $TARGET/debug/build/" >&2
    echo "       build the guests first:  cargo build -p test-components" >&2
    exit 1
  fi
  echo "$found"
}

mkdir -p components

for guest in relay echo stream-producer stream-consumer future-writer future-reader; do
  src=$(find_guest "$guest")
  cp "$src" "components/${guest}.wasm"
  echo "Copied $src"
  echo "  -> components/${guest}.wasm"
done

LOADER_WASM="../witgraph-loader/pkg/witgraph_web_bg.wasm"
if [ ! -f "$LOADER_WASM" ]; then
  echo ""
  echo "warning: $LOADER_WASM not found"
  echo "         build it:  cd ../witgraph-loader && pnpm build:wasm"
fi
