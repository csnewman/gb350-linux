#!/bin/sh
# Assembles a GB350 card in out/sdcard/ (or $1) from the builds:
#   cubegm/vmlinux.uImage, dtb.bin   the kernel; its initramfs runs the agent
#                                    (scripts/build-kernel.sh)
#   cubegm/avp.uImage                second-core firmware (avp/syscore/build.sh)
#   cubegm/xgame-logo.bmp            the boot logo the NOR bootloader shows (the GB350 one,
#                                    crates/ui/examples/bootlogo.rs)
#   gb350/firmware/gb350-syscore.bin the same firmware, for reloading it at runtime
#   gb350/kmod/                      loadable kernel modules
#   gb350/bin/launcher               the launcher
#   gb350/bin/retro-<core>           the emulators (scripts/build-emulators.sh)
#   PS/, SFC/, ...                   games, one folder per system (the stock layout)
# Copy it to an exFAT card (MBR, one partition, as the stock card); add games
# later through USB storage mode. DEV=1: the USB control channel (gbctl) on
# from the first boot (gb350/agent.conf); otherwise it is off until switched
# on in Settings.
set -eu
cd "$(dirname "$0")/.."
out=${1:-out/sdcard}
bin=target/mipsel-unknown-linux-musl/release
# Without macOS extended attributes (they become ._ files on exFAT).
cp="cp"; [ "$(uname)" = Darwin ] && cp="cp -X"
# One emulator per core; the launcher maps systems to them (library.rs SYSTEMS).
emus="pcsx_rearmed snes9x2005 gpsp fceumm picodrive gambatte mednafen_pce_fast mednafen_ngp mednafen_wswan stella2014 fbalpha2012"
for f in out/kernel/vmlinux.uImage out/kernel/dtb.bin out/kernel/modules.tar.gz out/avp/syscore.uImage out/avp/syscore.bin \
    "$bin/launcher" $(for e in $emus; do echo out/emulators/retro-$e; done); do
    [ -f "$f" ] || { echo "missing $f: build the kernel, avp/syscore, the launcher and the emulators first" >&2; exit 1; }
done
rm -rf "$out"
mkdir -p "$out/cubegm" "$out/gb350/bin" "$out/gb350/firmware" "$out/gb350/kmod"
$cp out/kernel/vmlinux.uImage out/kernel/dtb.bin "$out/cubegm/"
$cp out/avp/syscore.uImage "$out/cubegm/avp.uImage"
cargo run -q --release -p gb350-ui --example bootlogo -- "$out/cubegm/xgame-logo.bmp"
$cp out/avp/syscore.bin "$out/gb350/firmware/gb350-syscore.bin"
tar -xzf out/kernel/modules.tar.gz -C "$out/gb350/kmod"
$cp "$bin/launcher" "$out/gb350/bin/"
for e in $emus; do $cp out/emulators/retro-$e "$out/gb350/bin/"; done
for s in PS SFC FC GBA GB GBC MD PCE MAME NGPC WSC GG SMS ATARI; do mkdir -p "$out/$s"; done
[ "${DEV:-0}" = 1 ] && echo "usb_control=on" > "$out/gb350/agent.conf"
find "$out" -type f | sort | xargs ls -l
