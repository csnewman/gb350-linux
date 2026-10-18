#!/bin/sh
# Linker for the device target: zig cc (scripts/zig.sh, or $ZIG) against its
# bundled musl, soft-float MIPS32r2 to match Rust's mipsel-unknown-linux-musl.
# zig refuses .rlib inputs (which build-std passes for compiler_builtins), so
# those are handed over as .a copies.
for arg do
    shift
    case "$arg" in
        *.rlib) cp -f "$arg" "$arg.a"; set -- "$@" "$arg.a" ;;
        *) set -- "$@" "$arg" ;;
    esac
done
exec "${ZIG:-$(dirname "$0")/zig.sh}" cc -target mipsel-linux-musleabi -mcpu=mips32r2 "$@"
