#!/bin/sh
# ps1gpu (cores/ps1gpu): PCSX-ReARMed's unai rasteriser behind C entry points,
# for the second core (24KE: MIPS32 release 1, soft-float, no libc), from the
# checked-out core (scripts/build-cores.sh pcsx_rearmed first):
#   out/ps1gpu/libps1gpu.a   linked into the firmware (avp/syscore), which
#                            provides memcpy & co.
set -eu
cd "$(dirname "$0")/.."
Z=${ZIG:-$PWD/scripts/zig.sh}
P=out/build/pcsx_rearmed
O=out/ps1gpu
mkdir -p $O/core
DEFS="-DGPU_UNAI -DREARMED -DUSE_GPULIB=1 -DGPU_UNAI_NO_OLD"
INC="-I$P -I$P/include"
CXXF="${PS1GPU_OPT:--O2} -fno-exceptions -fno-rtti -Wno-everything $DEFS $INC"
C="-target mipsel-linux-musleabi -mcpu=mips32 -msoft-float -fomit-frame-pointer -fno-pic -mno-abicalls -G0 -ffreestanding -fno-stack-protector -fno-builtin-printf ${PS1GPU_CORE_FLAGS:-}"
$Z c++ $C $CXXF -c $P/plugins/gpu_unai/gpulib_if.cpp -o $O/core/gpulib_if.o
$Z c++ $C $CXXF -c cores/ps1gpu/ps1gpu.cpp -o $O/core/ps1gpu.o
$Z cc $C -O2 -Wno-everything $DEFS $INC -c $P/plugins/gpulib/prim.c -o $O/core/prim.o
rm -f $O/libps1gpu.a
$Z ar rcs $O/libps1gpu.a $O/core/*.o
ls -l $O/libps1gpu.a
