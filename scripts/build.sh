#!/usr/bin/env bash
#
# Build all release targets and collect binaries into dist/.
# All targets are built inside Docker via cross, so the host only needs
# Docker and cross itself.
#
# Requirements:
#   - Docker (running)
#   - cross  (cargo install cross --locked)
#
# Usage:
#   ./scripts/build.sh
#   DIST=out ./scripts/build.sh   # override output directory

set -euo pipefail

DIST="${DIST:-dist}"
mkdir -p "$DIST"

command -v cross &>/dev/null || {
    echo "error: cross not found. Install with: cargo install cross --locked"
    exit 1
}

# rust_target|output_name
TARGETS=(
    "x86_64-unknown-linux-gnu|agent-browser-linux-x64"
    "aarch64-unknown-linux-gnu|agent-browser-linux-arm64"
    "x86_64-unknown-linux-musl|agent-browser-linux-musl-x64"
    "aarch64-unknown-linux-musl|agent-browser-linux-musl-arm64"
)

for entry in "${TARGETS[@]}"; do
    IFS='|' read -r target output_name <<< "$entry"
    echo "==> $output_name ($target)"
    cross build --release --manifest-path cli/Cargo.toml --target "$target"
    cp "cli/target/$target/release/agent-browser" "$DIST/$output_name"
    chmod +x "$DIST/$output_name"
    echo "   -> $DIST/$output_name"
done

echo ""
echo "Artifacts in $DIST/:"
ls -lh "$DIST"/agent-browser-*
