#!/bin/sh
# Builds the second-core firmware (needs out/ps1gpu/libps1gpu.a from
# scripts/build-ps1gpu.sh):
#   out/avp/syscore.bin     raw image at 0x8bda4000 (for the Linux driver)
#   out/avp/syscore.uImage  as cubegm/avp.uImage for the NOR bootloader
# objcopy and mkimage come from the kernel build's Docker image.
set -eu
cd "$(dirname "$0")"
cargo build --release
root=$(cd ../.. && pwd)
mkdir -p "$root/out/avp"
docker image inspect gb350-kernel-build >/dev/null 2>&1 ||
    docker build -q -t gb350-kernel-build "$root/kernel/docker" >/dev/null
docker run --rm -v "$root":/src gb350-kernel-build sh -ec '
    o=/src/out/avp
    mipsel-linux-gnu-objcopy -O binary -j .text -j .rodata -j .data -j .got \
        /src/avp/syscore/target/mipsel-unknown-none/release/syscore $o/syscore.bin
    gzip -9 -n -f -k $o/syscore.bin
    mkimage -A mips -O u-boot -T standalone -C gzip -a 0x8bda4000 -e 0x8bda4000 -n gb350-syscore \
        -d $o/syscore.bin.gz $o/syscore.uImage >/dev/null'
ls -l "$root/out/avp/syscore.bin" "$root/out/avp/syscore.uImage"
