#!/bin/sh
# Builds the libretro cores the `retro` frontend links in, as static
# libraries out/cores/lib<name>.a (MIPS32r2, soft-float, musl, via zig cc),
# and the frontend's malloc and memcpy/memset:
#   pcsx_rearmed        PlayStation (lightrec recompiler, unai GPU)
#   snes9x2005          Super Famicom
#   gpsp                Game Boy Advance (MIPS recompiler)
#   fceumm              Famicom
#   picodrive           Mega Drive, Master System, Game Gear
#   gambatte            Game Boy (Color)
#   mednafen_pce_fast   PC Engine
#   mednafen_ngp        Neo Geo Pocket (Color)
#   mednafen_wswan      WonderSwan (Color)
#   stella2014          Atari 2600
#   fbalpha2012         Arcade
#   mimalloc            malloc replacement for the frontend (musl's mallocng maps and
#                       unmaps memory constantly under the recompiler: ~4.5 % of time)
#   mipsstring          memcpy/memset: newlib's MIPS assembly (musl only has generic C)
#   usage: scripts/build-cores.sh [name...]   (default: all)
# Most cores are LLVM bitcode (full LTO): the frontend's final link (zig's
# lld) optimises across them. Not thin LTO: lld's ThinLTO back end drops the
# soft-float ABI (links fail with "'-mdouble-float' is incompatible").
# zig keeps frame pointers by default; the cores don't need them. fceumm, stella2014 and fbalpha2012 are plain
# objects, as they have been built so far.
# Sources: pinned upstream commits in out/build/<core>, plus the patches
# from cores/patches/<core>/ (applied on a clean checkout each build).
set -eu
cd "$(dirname "$0")/.."
root=$PWD
Z=${ZIG:-$root/scripts/zig.sh}
"$Z" version >/dev/null  # fetched once, before the parallel makes
# zig cc traps undefined behaviour by default; the cores have plenty of it.
F="-target mipsel-linux-musleabi -mcpu=mips32r2 -msoft-float -fomit-frame-pointer -fno-sanitize=undefined -Wno-date-time -Wno-error"
LTO=-flto=full
NOLTO=-fno-lto
mkdir -p out/build out/cores

checkout() { # name url commit
    d=out/build/$1
    [ -d "$d/.git" ] || git clone -q --recurse-submodules "$2" "$d"
    (cd "$d" && { git fetch -q origin "$3" 2>/dev/null || true; } && git checkout -q -f "$3" &&
        git submodule -q update --init --recursive && git clean -q -fdx)
    for p in "$root"/cores/patches/"$1"/*.patch; do
        if [ -f "$p" ]; then (cd "$d" && git apply "$p"); fi
    done
}

fail() { # name log
    echo "$1: FAILED" >&2
    tail -20 "$2" >&2
    exit 1
}

# Cores whose makefiles only build a shared library: build that (its link
# fails, hence -k) and archive every object, libretro-common included.
objects() { # name dir lto makefile [make args...]
    n=$1; d=$2; l=$3; mf=$4; shift 4
    (cd "$d" && touch .buildstart && make -k -s -j8 -f "$mf" platform=unix fpic= \
        CC="$Z cc $F $l" CXX="$Z c++ $F $l" AR="$Z ar" "$@" >build.log 2>&1) || true
    rm -f out/cores/lib$n.a
    (cd "$d" && find . -name '*.o' -newer .buildstart -print0 | xargs -0 "$Z" ar rcs "$root/out/cores/lib$n.a")
    grep -E " error:" "$d/build.log" | grep -v "ld.lld\|linker command" | head -3 >&2 || true
    [ -s out/cores/lib$n.a ] || fail "$n" "$d/build.log"
}

build() {
    case $1 in
    pcsx_rearmed)
        # LIGHTREC_CUSTOM_MAP: emulated memory at fixed addresses (needs
        # vm.mmap_min_addr = 0 and the frontend linked high); fpic= : no PIC,
        # which costs on MIPS; unai: the fast GPU plugin. Without
        # -fno-sanitize: built this way since the start.
        checkout pcsx_rearmed https://github.com/libretro/pcsx_rearmed.git c8816799b50388e61cfe237fe2cdbb7d8175f20a
        P="-target mipsel-linux-musleabi -mcpu=mips32r2 -msoft-float -fomit-frame-pointer -Wno-date-time -Wno-error"
        (cd out/build/pcsx_rearmed && make -s -f Makefile.libretro -j8 platform=unix STATIC_LINKING=1 \
            TARGET=pcsx_rearmed_libretro.a ARCH=mips DYNAREC=lightrec LIGHTREC_CUSTOM_MAP=1 \
            LIGHTREC_THREADED_COMPILER=0 HAVE_PHYSICAL_CDROM=0 BUILTIN_GPU=unai fpic= \
            CC="$Z cc $P $LTO" CXX="$Z c++ $P $LTO" AR="$Z ar" >build.log 2>&1) ||
            fail pcsx_rearmed out/build/pcsx_rearmed/build.log
        cp out/build/pcsx_rearmed/pcsx_rearmed_libretro.a out/cores/libpcsx_rearmed.a ;;
    snes9x2005)
        checkout snes9x2005 https://github.com/libretro/snes9x2005.git a79dfe9047e7fec58808aefe48ad2bf499c7af11
        objects snes9x2005 out/build/snes9x2005 "$LTO" Makefile ;;
    gpsp)
        # The MIPS recompiler, with release 2 instructions.
        checkout gpsp https://github.com/libretro/gpsp.git 5819380c2ffb0900219d700a382ee68c464ebb99
        (cd out/build/gpsp && make -s -j8 platform=unix STATIC_LINKING=1 TARGET=gpsp_libretro.a \
            HAVE_DYNAREC=1 CPU_ARCH=mips MMAP_JIT_CACHE=1 fpic= \
            CC="$Z cc $F -DMIPS_HAS_R2_INSTS $LTO" CXX="$Z c++ $F -DMIPS_HAS_R2_INSTS $LTO" AR="$Z ar" >build.log 2>&1) ||
            fail gpsp out/build/gpsp/build.log
        cp out/build/gpsp/gpsp_libretro.a out/cores/libgpsp.a ;;
    fceumm)
        checkout fceumm https://github.com/libretro/libretro-fceumm.git ac0d00a7fc01166f84c7bac01d1084ebfd2b865e
        objects fceumm out/build/fceumm "$NOLTO" Makefile.libretro ;;
    picodrive)
        checkout picodrive https://github.com/libretro/picodrive.git 1890c2932234c9d30f4cd3851d02228baae8f09e
        objects picodrive out/build/picodrive "$LTO" Makefile.libretro use_libchdr=0 ;;
    gambatte)
        checkout gambatte https://github.com/libretro/gambatte-libretro.git d9d6cd06382d1ced30de34d56d3609452323dab1
        objects gambatte out/build/gambatte "$LTO" Makefile.libretro ;;
    mednafen_pce_fast)
        checkout mednafen_pce_fast https://github.com/libretro/beetle-pce-fast-libretro.git 3f946f277aef3aa99a95551618bbcd1dd2bda0d9
        objects mednafen_pce_fast out/build/mednafen_pce_fast "$LTO" Makefile ;;
    mednafen_ngp)
        checkout mednafen_ngp https://github.com/libretro/beetle-ngp-libretro.git a50d5ac288a81f2104ddf43195a4efdd15c72227
        objects mednafen_ngp out/build/mednafen_ngp "$LTO" Makefile ;;
    mednafen_wswan)
        checkout mednafen_wswan https://github.com/libretro/beetle-wswan-libretro.git 4b01295838ea89e3f1355bbe4cb5cf98aa6108cd
        objects mednafen_wswan out/build/mednafen_wswan "$LTO" Makefile ;;
    stella2014)
        checkout stella2014 https://github.com/libretro/stella2014-libretro.git 7d1361e407e63f29e52892655069e5fb4096e691
        objects stella2014 out/build/stella2014 "$NOLTO" Makefile ;;
    fbalpha2012)
        checkout fbalpha2012 https://github.com/libretro/fbalpha2012.git 0ce31536bef3162fe7e69ff5f555334ec4913cef
        d=out/build/fbalpha2012/svn-current/trunk
        A="$F -Wno-narrowing -Wno-register -Wno-c++11-narrowing $NOLTO"
        (cd $d && make -j8 -f makefile.libretro platform=unix STATIC_LINKING=1 \
            TARGET=fbalpha2012_libretro.a fpic= CC="$Z cc $A" CXX="$Z c++ $A" AR="$Z ar" >build.log 2>&1) ||
            fail fbalpha2012 $d/build.log
        cp $d/fbalpha2012_libretro.a out/cores/libfbalpha2012.a ;;
    mimalloc)
        # Static override of malloc & co. (musl supports replacing malloc in a
        # static program when every allocation function is replaced).
        checkout mimalloc https://github.com/microsoft/mimalloc.git v2.1.7
        M="-target mipsel-linux-musleabi -mcpu=mips32r2 -msoft-float -fomit-frame-pointer -Wno-date-time -Wno-error"
        (cd out/build/mimalloc && $Z cc $M $LTO -O3 -DNDEBUG -DMI_MALLOC_OVERRIDE -DMI_SECURE=0 \
            -DMI_STAT=0 -Wno-atomic-alignment -Iinclude -c src/static.c -o mimalloc.o &&
            $Z ar rcs libmimalloc.a mimalloc.o) || exit 1
        cp out/build/mimalloc/libmimalloc.a out/cores/libmimalloc.a ;;
    mipsstring)
        M="-target mipsel-linux-musleabi -mcpu=mips32r2 -msoft-float"
        for f in memcpy memset; do
            $Z cc $M -Icores/libc-mips/include -DPREFETCH_LOAD_HINT=PREFETCH_HINT_LOAD_STREAMED \
                -DPREFETCH_STORE_HINT=PREFETCH_HINT_PREPAREFORSTORE -c cores/libc-mips/$f.S \
                -o out/build/$f.o
        done
        rm -f out/cores/libmipsstring.a
        $Z ar rcs out/cores/libmipsstring.a out/build/memcpy.o out/build/memset.o ;;
    *)
        echo "unknown core: $1" >&2; exit 2 ;;
    esac
    ls -l out/cores/lib$1.a
}

for c in ${*:-pcsx_rearmed snes9x2005 gpsp fceumm picodrive gambatte mednafen_pce_fast mednafen_ngp \
    mednafen_wswan stella2014 fbalpha2012 mimalloc mipsstring}; do
    build "$c"
done
