#!/bin/sh
# Builds the emulator binaries: the retro frontend linked with each core
# (out/cores/lib<core>.a from scripts/build-cores.sh), as out/emulators/
# retro-<core>. The launcher maps each system to one (library.rs SYSTEMS).
#   usage: scripts/build-emulators.sh [core...]   (default: all)
set -eu
cd "$(dirname "$0")/.."
cores=${*:-"pcsx_rearmed gpsp snes9x2005 fceumm gambatte picodrive mednafen_pce_fast mednafen_ngp mednafen_wswan stella2014 fbalpha2012"}
mkdir -p out/emulators
log=out/emulators/build.log
failed=""
for core in $cores; do
    rm -f out/emulators/retro-$core
    if RETRO_CORE=$core cargo build -q -p gb350-retro --release --target mipsel-unknown-linux-musl \
        -Z build-std=std,panic_abort >"$log" 2>&1; then
        cp target/mipsel-unknown-linux-musl/release/retro out/emulators/retro-$core
        ls -l out/emulators/retro-$core
    else
        echo "retro-$core: FAILED" >&2
        grep -E "^error|ld.lld: error" "$log" | head -8 >&2
        failed="$failed $core"
    fi
done
[ -z "$failed" ] || { echo "failed:$failed" >&2; exit 1; }
