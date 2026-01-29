#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

echo "Building checkout..."
cargo build --release 2>/dev/null || cargo build

if [[ -f target/release/checkout ]]; then
    cp target/release/checkout ~/.local/bin/
else
    cp target/debug/checkout ~/.local/bin/
fi

echo "Installed to ~/.local/bin/checkout"
