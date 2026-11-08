#!/bin/sh
# Build a release binary for this machine: frontend first, then Rust (the order matters)
set -eu

# Always work from the project root, wherever this is run from
cd "$(dirname "$0")"

npm --prefix frontend ci
npm --prefix frontend run build
cargo build --release

echo "done: target/release/manorcloud"
