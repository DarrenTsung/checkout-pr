#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

echo "Building checkout-pr..."
cargo build --release 2>/dev/null || cargo build

if [[ -f target/release/checkout-pr ]]; then
    cp target/release/checkout-pr ~/.local/bin/
else
    cp target/debug/checkout-pr ~/.local/bin/
fi

echo "Installed to ~/.local/bin/checkout-pr"
