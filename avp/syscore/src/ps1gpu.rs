//! PS1 GPU service (corelink service 2): PCSX-ReARMed's unai rasteriser
//! (out/ps1gpu/libps1gpu.a, scripts/build-ps1gpu.sh) drawing into a VRAM
//! buffer Linux allocated, fed records:
//!   1 INIT [vram phys]       2 EX_REGS [8 words]     3 CMD [words...]
//!   4 VRAM_WRITE [x, y, len, mask, pixels...]          5 VRAM_COPY [4 words]
//!   6 FRAME                  7 WRITEBACK (VRAM to memory, for Linux)
//!   8 SPLIT [shift, skip]    skip the stripes of 2^shift rows whose bit (stripe & 31) is set
//!   9 FLUSH                  the whole data cache to memory and dropped
//!                            (Linux then reads memory, and later reads here
//!                            see what Linux wrote)
//!  10 STATE [words...]       the emulator's unai options (renderer_split_state)
//! Results (ring header): frames, drawing cycles total, worst frame, cycles
//! in the current frame (CP0 count, 297 MHz).

use crate::corelink::{flush_dcache, writeback, Results};
use crate::hw;

extern "C" {
    fn ps1gpu_init(vram: *mut u16);
    fn ps1gpu_ex_regs(ex: *const u32);
    fn ps1gpu_cmd(list: *mut u32, n: i32) -> i32;
    fn ps1gpu_vram_write(x: i32, y: i32, pixels: *const u16, len: i32, r6: u32);
    fn ps1gpu_vram_copy(params: *const u32);
    fn ps1gpu_split(shift: i32, skip: u32);
    fn ps1gpu_state(words: *const u32, n: i32);
}

/// For the rasteriser (it has no libc).
#[no_mangle]
extern "C" fn abs(v: i32) -> i32 {
    v.wrapping_abs()
}

const KSEG0: usize = 0x8000_0000;
const VRAM_BYTES: usize = 1024 * 512 * 2;
/// Largest CMD record (words); Linux batches up to this.
pub const MAX_CMD_WORDS: usize = 4096;

static mut VRAM: usize = 0;
/// Command words copied out of the ring: the rasteriser may write to its
/// list, and a dirty cache line over ring memory could later be written back
/// over newer records.
static mut SCRATCH: [u32; MAX_CMD_WORDS] = [0; MAX_CMD_WORDS];

fn word(p: &[u8], i: usize) -> u32 {
    p.get(4 * i..4 * i + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
}

#[allow(static_mut_refs)]
pub fn handle(r: &Results, tag: u16, p: &[u8]) -> Result<(), u32> {
    let t0 = hw::count();
    match tag {
        1 => {
            let vram = word(p, 0) as usize;
            if vram == 0 || vram + VRAM_BYTES > 0x1000_0000 || vram % 16 != 0 {
                return Err(1);
            }
            flush_dcache(); // nothing stale from an earlier user
            unsafe {
                VRAM = KSEG0 | vram;
                ps1gpu_init(VRAM as *mut u16);
            }
            for i in 0..4 {
                r.set(i, 0);
            }
            return Ok(());
        }
        _ if unsafe { VRAM } == 0 => return Err(2),
        2 => {
            let mut ex = [0u32; 8];
            for (i, e) in ex.iter_mut().enumerate() {
                *e = word(p, i);
            }
            unsafe { ps1gpu_ex_regs(ex.as_ptr()) };
        }
        3 => {
            let n = (p.len() / 4).min(MAX_CMD_WORDS);
            unsafe {
                for i in 0..n {
                    SCRATCH[i] = word(p, i);
                }
                let mut pos = 0;
                while pos < n {
                    let used = ps1gpu_cmd(SCRATCH.as_mut_ptr().add(pos), (n - pos) as i32);
                    if used <= 0 {
                        break; // incomplete command: Linux split a list
                    }
                    pos += used as usize;
                }
            }
        }
        4 => {
            let (x, y, len, r6) = (word(p, 0), word(p, 1), word(p, 2), word(p, 3));
            if 16 + 2 * len as usize > p.len() || x + len > 1024 {
                return Err(3);
            }
            // Pixels are 16-bit; the payload is word aligned in the ring.
            unsafe { ps1gpu_vram_write(x as i32, y as i32, p.as_ptr().add(16) as *const u16, len as i32, r6) };
        }
        5 => {
            let w = [word(p, 0), word(p, 1), word(p, 2), word(p, 3)];
            unsafe { ps1gpu_vram_copy(w.as_ptr()) };
        }
        6 => {
            let cur = r.get(3);
            r.set(0, r.get(0).wrapping_add(1));
            r.set(1, r.get(1).wrapping_add(cur));
            if cur > r.get(2) {
                r.set(2, cur);
            }
            r.set(3, 0);
            return Ok(());
        }
        7 => {
            writeback(unsafe { VRAM }, VRAM_BYTES);
            return Ok(());
        }
        8 => {
            unsafe { ps1gpu_split(word(p, 0) as i32, word(p, 1)) };
            return Ok(());
        }
        9 => flush_dcache(),
        10 => {
            let mut w = [0u32; 16];
            let n = (p.len() / 4).min(w.len());
            for (i, v) in w[..n].iter_mut().enumerate() {
                *v = word(p, i);
            }
            unsafe { ps1gpu_state(w.as_ptr(), n as i32) };
            return Ok(());
        }
        _ => return Err(4),
    }
    r.set(3, r.get(3).wrapping_add(hw::count().wrapping_sub(t0)));
    Ok(())
}
