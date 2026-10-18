#!/bin/sh
# Runs the pinned zig (C compiler, musl and linker for the device builds),
# downloading it into out/tools/ on first use and checking its SHA-256.
# ZIG=<path> in the environment overrides it in every build script.
set -eu
V=0.14.1
d=$(cd "$(dirname "$0")/.." && pwd)/out/tools/zig-$V
if [ ! -x "$d/zig" ]; then
    case "$(uname -m)-$(uname -s)" in
    x86_64-Linux) p=x86_64-linux; sum=24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c ;;
    aarch64-Linux) p=aarch64-linux; sum=f7a654acc967864f7a050ddacfaa778c7504a0eca8d2b678839c21eea47c992b ;;
    arm64-Darwin) p=aarch64-macos; sum=39f3dc5e79c22088ce878edc821dedb4ca5a1cd9f5ef915e9b3cc3053e8faefa ;;
    x86_64-Darwin) p=x86_64-macos; sum=b0f8bdfb9035783db58dd6c19d7dea89892acc3814421853e5752fe4573e5f43 ;;
    *) echo "zig.sh: no pinned zig for $(uname -m)-$(uname -s); set ZIG" >&2; exit 1 ;;
    esac
    t=$d.tmp.$$
    rm -rf "$t" && mkdir -p "$t"
    curl -fsSL -o "$t/zig.tar.xz" "https://ziglang.org/download/$V/zig-$p-$V.tar.xz"
    if command -v sha256sum >/dev/null; then have=$(sha256sum "$t/zig.tar.xz"); else have=$(shasum -a 256 "$t/zig.tar.xz"); fi
    [ "${have%% *}" = "$sum" ] || { echo "zig.sh: checksum mismatch" >&2; rm -rf "$t"; exit 1; }
    tar -xJf "$t/zig.tar.xz" -C "$t" --strip-components=1 && rm "$t/zig.tar.xz"
    # Another build may have got there first.
    if [ -e "$d" ]; then rm -rf "$t"; else mv "$t" "$d"; fi
fi
exec "$d/zig" "$@"
