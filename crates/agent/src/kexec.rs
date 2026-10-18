//! Restart into the kernel on the SD card with kexec.
//!
//! A real reset can't restart the GB350: the watchdog reset releases the power
//! latch (GPIO L1) and the board powers off until the power button is pressed
//! (re-entering the boot ROM ends the same way). kexec skips the bootloader:
//! the running kernel copies the new one into place and jumps to it, with the
//! DTB handed over like the bootloader does (UHI: a0 = -2, a1 = DTB; mainline
//! MIPS kexec finds the DTB segment itself). The AVP keeps running throughout.
//!
//! Safety net: the watchdog is armed before the jump. The new kernel stops it
//! early in boot (hc16xx-board); if anything hangs first, the watchdog powers
//! the board off instead of leaving it stuck on.

use crate::sys;

pub const KERNEL: &str = "/mnt/sdcard/cubegm/vmlinux.uImage";
pub const DTB: &str = "/mnt/sdcard/cubegm/dtb.bin";

/// Where the DTB goes: well clear of the kernel image (loaded at 1 MiB) and of
/// the top of memory, where early boot allocations start.
const DTB_PHYS: u32 = 0x0400_0000;
const PAGE: usize = 4096;

const WDT: u32 = 0x1881_8500;
/// Watchdog control: run, reset on overflow, 750 kHz / 4.
const WDT_CTRL_RESET_DIV4: u32 = 0x26;
const WDT_HZ_DIV4: u32 = 187_500;
const SAFETY_SECS: u32 = 30;

#[repr(C)]
struct Segment {
    buf: *const u8,
    bufsz: usize,
    mem: usize,
    memsz: usize,
}

pub struct Image {
    kernel: Vec<u8>,
    dtb: Vec<u8>,
    load: u32,
    entry: u32,
    pub name: String,
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

/// Strips a gzip header and inflates the deflate stream behind it.
fn gunzip(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b || data[2] != 8 {
        return Err("not a gzip stream".into());
    }
    let flags = data[3];
    let mut at = 10;
    if flags & 0x04 != 0 {
        let xlen = u16::from_le_bytes([data[at], data[at + 1]]) as usize;
        at += 2 + xlen;
    }
    for bit in [0x08, 0x10] {
        if flags & bit != 0 {
            at += data[at..].iter().position(|&c| c == 0).ok_or("bad gzip header")? + 1;
        }
    }
    if flags & 0x02 != 0 {
        at += 2;
    }
    miniz_oxide::inflate::decompress_to_vec(&data[at..]).map_err(|e| format!("inflate: {e:?}"))
}

/// Reads and unpacks a legacy uImage (as the NOR bootloader takes) and a DTB.
pub fn prepare(kernel_path: &str, dtb_path: &str) -> Result<Image, String> {
    let raw = std::fs::read(kernel_path).map_err(|e| format!("{kernel_path}: {e}"))?;
    if raw.len() < 64 || be32(&raw, 0) != 0x2705_1956 {
        return Err(format!("{kernel_path}: not a uImage"));
    }
    let size = be32(&raw, 12) as usize;
    let load = be32(&raw, 16);
    let entry = be32(&raw, 20);
    let comp = raw[31];
    let name = String::from_utf8_lossy(&raw[32..64]).trim_end_matches('\0').to_string();
    let payload = raw.get(64..64 + size).ok_or("uImage truncated")?;
    let kernel = match comp {
        0 => payload.to_vec(),
        1 => gunzip(payload)?,
        c => return Err(format!("uImage compression {c} unsupported")),
    };
    let mut dtb = std::fs::read(dtb_path).map_err(|e| format!("{dtb_path}: {e}"))?;
    if dtb.len() < 40 || be32(&dtb, 0) != 0xd00d_feed {
        return Err(format!("{dtb_path}: not a DTB"));
    }
    // The next kernel gets fresh boot randomness.
    if !crate::rngseed::refresh(&mut dtb) {
        crate::log::warn(&format!("{dtb_path}: no /chosen/rng-seed to refresh"));
    }
    let phys = |a: u32| a & 0x1fff_ffff;
    if phys(load) as usize + kernel.len() >= DTB_PHYS as usize {
        return Err("kernel image overlaps the DTB slot".into());
    }
    Ok(Image { kernel, dtb, load: phys(load), entry: phys(entry), name })
}

fn round_up(n: usize) -> usize {
    n.div_ceil(PAGE) * PAGE
}

/// Hands the image to the kernel (kexec_load). Nothing happens until `exec`.
pub fn load(img: &Image) -> Result<(), String> {
    let segs = [
        Segment {
            buf: img.kernel.as_ptr(),
            bufsz: img.kernel.len(),
            mem: img.load as usize,
            memsz: round_up(img.kernel.len()),
        },
        Segment { buf: img.dtb.as_ptr(), bufsz: img.dtb.len(), mem: DTB_PHYS as usize, memsz: round_up(img.dtb.len()) },
    ];
    // flags 0 = KEXEC_ARCH_DEFAULT; the entry point is physical.
    let r = unsafe { libc::syscall(libc::SYS_kexec_load, img.entry as libc::c_ulong, segs.len(), segs.as_ptr(), 0) };
    if r != 0 {
        return Err(format!("kexec_load: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

fn wdt_write(off: u32, v: u32) -> Result<(), String> {
    sys::mem_op(WDT + off, 4, Some((4, v))).map(|_| ())
}

/// Syncs, arms the safety-net watchdog and jumps into the loaded kernel.
/// Returns only if the jump failed (the watchdog is disarmed again).
/// The SD card should already be released (`sys::release_sdcard`).
pub fn exec() -> String {
    unsafe { libc::sync() };
    let armed = wdt_write(4, 0)
        .and_then(|_| wdt_write(0, 0u32.wrapping_sub(SAFETY_SECS * WDT_HZ_DIV4)))
        .and_then(|_| wdt_write(4, WDT_CTRL_RESET_DIV4));
    if let Err(e) = &armed {
        crate::log::warn(&format!("kexec: no watchdog safety net: {e}"));
    }
    unsafe { libc::reboot(libc::RB_KEXEC) };
    let err = std::io::Error::last_os_error();
    let _ = wdt_write(4, 0);
    format!("reboot(KEXEC): {err}")
}
