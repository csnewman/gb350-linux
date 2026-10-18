#!/bin/sh
# Builds the on-device agent: static mipsel (MIPS32r2, soft-float) musl binary.
# The target is tier 3, so this needs nightly + build-std; zig provides musl
# and links (see scripts/mipsel-linker.sh).
set -eu
cd "$(dirname "$0")/.."
cargo build -p gb350-agent --release \
    --target mipsel-unknown-linux-musl \
    -Z build-std=std,panic_abort
ls -l target/mipsel-unknown-linux-musl/release/agent
