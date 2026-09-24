#!/bin/bash
# Builds the Octos host-managed FFI dylib the app extension links.
#
#   build-octos-ffi.sh [octos-checkout]
#
# Defaults to the sibling checkout at ../octos. Prints the dylib path on
# success so it can be fed to the bundler (or ROBRIX_OCTOS_FFI_DYLIB).
set -euo pipefail

OCTOS_DIR="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../../../octos" && pwd)}"
cd "$OCTOS_DIR"
cargo build -p octos-ffi --no-default-features --release --target aarch64-apple-darwin
echo "$PWD/target/aarch64-apple-darwin/release/liboctos_ffi.dylib"
