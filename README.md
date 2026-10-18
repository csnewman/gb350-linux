# gb350-linux

Replacement software for the GB350 v1.22 handheld (HiChip HC1600A: MIPS
74Kc at 1188 MHz for Linux, a second MIPS 24KE core, 256 MiB): mainline
Linux 7.2 with drivers for the board, a Rust userspace (agent, launcher,
emulator frontend) and Rust firmware for the second core. The stock bootloader in SPI NOR is used
as it is.

| Path | What |
|---|---|
| `kernel/` | Drivers, device tree, config and patches for mainline Linux |
| `avp/syscore` | Second-core firmware: buttons, power, battery, card, speaker, PS1 drawing |
| `crates/agent` | PID 1: system setup, console, USB control channel, power-off, USB storage mode, runs the launcher |
| `crates/launcher` | The game launcher |
| `crates/retro` | libretro frontend with one emulator core linked in, in-game menu |
| `crates/ui`, `crates/kms`, `crates/pcm`, `crates/corelink`, `crates/proto` | Drawing and text, display (DRM/KMS), sound (ALSA), the second-core link, the USB protocol |
| `cores/` | Patches and helpers for the emulator cores |

## Build

Needs Docker, rustup and curl. The Rust toolchain is pinned in
`rust-toolchain.toml`; zig (C compiler, musl, linker for the device) is
downloaded by `scripts/zig.sh`; the kernel source and BusyBox by
`scripts/build-kernel.sh` (all checked against pinned hashes).

```sh
scripts/build-cores.sh          # emulator cores -> out/cores/
scripts/build-ps1gpu.sh         # PS1 rasteriser for the second core -> out/ps1gpu/
avp/syscore/build.sh            # second-core firmware -> out/avp/
scripts/build-kernel.sh         # kernel + DT + modules (the agent in its initramfs) -> out/kernel/
cargo build -p gb350-launcher --release --target mipsel-unknown-linux-musl -Z build-std=std,panic_abort
scripts/build-emulators.sh      # one emulator per core -> out/emulators/
scripts/make-sdcard.sh          # the card's files -> out/sdcard/
```

Copy `out/sdcard/` to an exFAT card (MBR, one partition). Games go in one
folder per system at the card root (`PS/`, `SFC/`, `GBA/`, ...).

## Installing a release

https://csnewman.github.io/gb350-linux/ installs the latest release on a
GB350 card from Chrome or Edge on a computer (`web/index.html`). It moves
the card's current system into `backup/stock/` (the original firmware, kept
for good) or `backup/previous/` (the last GB350 Linux), writes and checks
the new files and creates the game folders. Releases are published there by
CI when a `v*` tag is pushed (`scripts/make-site.sh`).

## Licence

MIT (`LICENSE`), except `kernel/` (GPL-2.0, as Linux) and `cores/` (the
licences of the cores they patch or build on; PCSX-ReARMed is GPL-2.0).
The emulator binaries and the second-core firmware link GPL code.
