#!/bin/sh
# Builds the mainline kernel + DTB for the GB350 in Docker.
#   out/kernel/vmlinux.uImage  (gzip legacy uImage, as the NOR bootloader expects)
#   out/kernel/dtb.bin
# Kernel source/build tree live in a Docker volume, gb350-kernel or
# $KERNEL_VOLUME (the kernel tree needs a case-sensitive filesystem); the
# source is downloaded from kernel.org on first use and checked against the
# pinned SHA-256.
set -eu
cd "$(dirname "$0")/.."
V=${KERNEL_VERSION:-7.2.11}
KSHA=${KERNEL_SHA256:-fd0fc479f983a8c05313f13e9c36330fe6651cf725396748c40d5cd93fa1e6a4}
docker image inspect gb350-kernel-build >/dev/null 2>&1 ||
    docker build -q -t gb350-kernel-build kernel/docker >/dev/null
# PID 1 is the agent (USB control channel via FunctionFS, screen console);
# BusyBox (Debian bookworm's busybox-static 1:1.35.0-4+deb12u1+b1, mipsel,
# from snapshot.debian.org, checked) provides the shell for `gbctl sh`.
init=target/mipsel-unknown-linux-musl/release/agent
./scripts/build-agent.sh >/dev/null
mkdir -p out/kernel
# GB350_PROBE=<name> applies kernel/probes/<name>.patch for this build only
# (bring-up diagnostics); it is reverted afterwards.
probe=${GB350_PROBE:-}
docker run --rm -v "${KERNEL_VOLUME:-gb350-kernel}":/work -v "$PWD":/src -e V="$V" -e KSHA="$KSHA" -e PROBE="$probe" gb350-kernel-build sh -ec '
    cd /work
    if [ ! -d linux-$V ]; then
        curl -fsSL -o linux-$V.tar.xz https://cdn.kernel.org/pub/linux/kernel/v${V%%.*}.x/linux-$V.tar.xz
        echo "$KSHA  linux-$V.tar.xz" | sha256sum -c --quiet
        tar -xJf linux-$V.tar.xz && rm linux-$V.tar.xz
    fi
    BB=b273e6e65483b7d68b684e951fb5498afb191f1d3c8fa637abd944b12e3169d0
    if ! echo "$BB  busybox.static" | sha256sum -c --status 2>/dev/null; then
        curl -fsSL -o busybox.deb https://snapshot.debian.org/file/06e02ee34a5cdf1e0531c07b09f6e6a09a7d786b
        dpkg-deb --fsys-tarfile busybox.deb | tar -xO ./bin/busybox > busybox.static && rm busybox.deb
        echo "$BB  busybox.static" | sha256sum -c --quiet
    fi
    cd /work/linux-$V
    # The GB350 drivers: kernel/drivers/<path> is copied into the tree; each new
    # directory is hooked into its parent Makefile once.
    rsync -a /src/kernel/drivers/ drivers/
    rsync -a /src/kernel/include/ include/
    rsync -a /src/kernel/sound/ sound/
    hook() { grep -qxF "$2" "$1" || echo "$2" >> "$1"; }
    hook drivers/soc/Makefile "obj-y += hichip/"
    hook drivers/irqchip/Makefile "obj-y += irq-hc16xx.o"
    hook drivers/usb/musb/Makefile "obj-\$(CONFIG_USB_MUSB_HDRC) += hc16xx.o"
    hook drivers/gpio/Makefile "obj-\$(CONFIG_GPIO_HC16XX) += gpio-hc16xx.o"
    hook drivers/gpio/Kconfig "source \"drivers/gpio/Kconfig.gb350\""
    hook drivers/input/keyboard/Makefile "obj-\$(CONFIG_KEYBOARD_GB350) += gb350-keys.o"
    hook drivers/input/keyboard/Makefile "obj-\$(CONFIG_KEYBOARD_GB350_SYSCORE) += gb350-syscore-keys.o"
    hook drivers/input/keyboard/Kconfig "source \"drivers/input/keyboard/Kconfig.gb350\""
    hook drivers/iio/adc/Makefile "obj-\$(CONFIG_HC16XX_ADC) += hc16xx-adc.o"
    hook drivers/iio/adc/Kconfig "source \"drivers/iio/adc/Kconfig.gb350\""
    hook drivers/power/supply/Makefile "obj-\$(CONFIG_BATTERY_GB350) += gb350-battery.o"
    hook drivers/power/supply/Kconfig "source \"drivers/power/supply/Kconfig.gb350\""
    hook drivers/gpu/drm/Makefile "obj-y += hichip/"
    hook drivers/gpu/drm/Kconfig "source \"drivers/gpu/drm/hichip/Kconfig\""
    hook drivers/pwm/Makefile "obj-\$(CONFIG_PWM_HC16XX) += pwm-hc16xx.o"
    hook drivers/pwm/Kconfig "source \"drivers/pwm/Kconfig.gb350\""
    hook sound/Makefile "obj-\$(CONFIG_SND) += hichip/"
    hook sound/Kconfig "source \"sound/hichip/Kconfig\""
    # kernel/patches/*.patch (fixes to upstream code) and the probe are
    # applied for this build and reverted afterwards (newest first), so the
    # tree in the volume stays pristine.
    undo=""
    trap "sh -c \"\$undo\"" EXIT
    for p in /src/kernel/patches/*.patch; do
        [ -f "$p" ] || continue
        patch -p1 -s --forward < "$p"
        undo="patch -p1 -s -R < $p; $undo"
    done
    if [ -f "/src/kernel/probes/$PROBE.patch" ]; then
        patch -p1 -s --forward < /src/kernel/probes/$PROBE.patch
        undo="patch -p1 -s -R < /src/kernel/probes/$PROBE.patch; $undo"
        echo "probe $PROBE applied"
    elif [ -f "/src/kernel/probes/$PROBE.py" ]; then
        python3 /src/kernel/probes/$PROBE.py apply
        undo="python3 /src/kernel/probes/$PROBE.py revert; $undo"
    fi
    install -m 0755 /src/'"$init"' /work/init
    install -m 0755 /work/busybox.static /work/busybox
    cat > /work/initramfs.list <<LIST
dir /dev 0755 0 0
nod /dev/console 0600 0 0 c 5 1
dir /proc 0755 0 0
dir /sys 0755 0 0
dir /tmp 1777 0 0
dir /bin 0755 0 0
file /bin/busybox /work/busybox 0755 0 0
slink /bin/sh busybox 0777 0 0
file /init /work/init 0755 0 0
LIST
    M="make -s ARCH=mips CROSS_COMPILE=mipsel-linux-gnu- O=/work/build"
    KCONFIG_ALLCONFIG=/src/kernel/config/gb350.config $M allnoconfig
    # Fail loudly if a requested option did not stick.
    grep -vE "^#|^$" /src/kernel/config/gb350.config | while read -r l; do
        grep -qx "$l" /work/build/.config || echo "WARNING: config not applied: $l"
    done
    $M -j"$(nproc)" vmlinux modules
    # Loadable modules -> out/kernel/modules.tar.gz (lib/modules/<release>/;
    # the card is exFAT, so no build/source symlinks).
    rm -rf /work/modroot
    $M INSTALL_MOD_PATH=/work/modroot INSTALL_MOD_STRIP=1 modules_install
    find /work/modroot -type l -delete
    tar -C /work/modroot -czf /src/out/kernel/modules.tar.gz lib/modules
    B=/work/build
    mipsel-linux-gnu-objcopy -O binary $B/vmlinux $B/vmlinux.bin
    gzip -9 -n -f -k $B/vmlinux.bin
    load=0x$(mipsel-linux-gnu-nm $B/vmlinux | awk "/ _text\$/ {print \$1}")
    entry=0x$(mipsel-linux-gnu-nm $B/vmlinux | awk "/ kernel_entry\$/ {print \$1}")
    mkimage -A mips -O linux -T kernel -C gzip -a $load -e $entry -n "Linux-$V-gb350" \
        -d $B/vmlinux.bin.gz /src/out/kernel/vmlinux.uImage >/dev/null
    # The DTS uses dt-bindings macros (KEY_*, GPIO_*): preprocess like Kbuild.
    cpp -nostdinc -I include -I scripts/dtc/include-prefixes -undef -D__DTS__ \
        -x assembler-with-cpp -P /src/kernel/dts/gb350.dts > /work/gb350.dts.pp
    dtc -q -I dts -O dtb -o /src/out/kernel/dtb.bin /work/gb350.dts.pp
    # Random boot seed in place of the 32-byte /chosen/rng-seed placeholder.
    python3 - /src/out/kernel/dtb.bin <<PY
import os, sys
p = sys.argv[1]; d = open(p, "rb").read()
m = b"GB350-RNG-SEED-PLACEHOLDER-32B!\0"
assert d.count(m) == 1, "rng-seed placeholder"
open(p, "wb").write(d.replace(m, os.urandom(32)))
PY
    cp $B/.config /src/out/kernel/config
    echo "load $load entry $entry"
'
ls -l out/kernel/
