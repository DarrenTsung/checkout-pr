#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

echo "Building checkout..."
cargo build

cp target/debug/checkout ~/.local/bin/

echo "Installed to ~/.local/bin/checkout"
