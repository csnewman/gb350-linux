//! Boot-time randomness: `/chosen/rng-seed` in the DTB (32 bytes), which the
//! kernel credits at boot (random.trust_bootloader, default on) - without it
//! the CRNG took ~80 s to initialise and anything using getrandom() blocked.
//!
//! The build fills the seed with random bytes (scripts/build-kernel.sh); the
//! agent replaces it in the card's dtb.bin early at every boot, so the next
//! boot never reuses one, and in each kexec image.

use std::io::{Seek, SeekFrom, Write};

pub const SEED_LEN: usize = 32;

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Offset of the `/chosen/rng-seed` value in a flattened device tree.
pub fn find_seed(dtb: &[u8]) -> Option<usize> {
    if be32(dtb, 0)? != 0xd00d_feed {
        return None;
    }
    let (structs, strings) = (be32(dtb, 8)? as usize, be32(dtb, 12)? as usize);
    let name_at = |off: usize| {
        let s = dtb.get(strings + off..)?;
        Some(&s[..s.iter().position(|&c| c == 0)?])
    };
    let (mut p, mut depth, mut in_chosen) = (structs, 0usize, false);
    loop {
        let tok = be32(dtb, p)?;
        p += 4;
        match tok {
            1 => {
                // FDT_BEGIN_NODE: NUL-terminated name, 4-aligned
                let rest = dtb.get(p..)?;
                let len = rest.iter().position(|&c| c == 0)?;
                depth += 1;
                if depth == 2 {
                    in_chosen = &rest[..len] == b"chosen";
                }
                p += (len + 4) & !3;
            }
            2 => {
                // FDT_END_NODE
                if depth == 2 {
                    in_chosen = false;
                }
                depth = depth.checked_sub(1)?;
            }
            3 => {
                // FDT_PROP: len, nameoff, value
                let (len, nameoff) = (be32(dtb, p)? as usize, be32(dtb, p + 4)? as usize);
                if in_chosen && depth == 2 && len == SEED_LEN && name_at(nameoff)? == b"rng-seed" {
                    return Some(p + 8);
                }
                p += 8 + ((len + 3) & !3);
            }
            4 => {} // FDT_NOP
            _ => return None,
        }
    }
}

fn fresh() -> [u8; SEED_LEN] {
    let mut seed = [0u8; SEED_LEN];
    // Blocks only until the CRNG is ready - immediate once a seed was used.
    unsafe { libc::getrandom(seed.as_mut_ptr() as *mut libc::c_void, SEED_LEN, 0) };
    seed
}

/// Puts a fresh seed into an in-memory DTB (a kexec image).
pub fn refresh(dtb: &mut [u8]) -> bool {
    match find_seed(dtb) {
        Some(at) => {
            dtb[at..at + SEED_LEN].copy_from_slice(&fresh());
            true
        }
        None => false,
    }
}

/// Replaces the seed in a DTB file in place (32 bytes written).
pub fn refresh_file(path: &str) -> Result<(), String> {
    let dtb = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let at = find_seed(&dtb).ok_or(format!("{path}: no /chosen/rng-seed"))?;
    let mut f = std::fs::OpenOptions::new().write(true).open(path).map_err(|e| format!("{path}: {e}"))?;
    f.seek(SeekFrom::Start(at as u64)).map_err(|e| e.to_string())?;
    f.write_all(&fresh()).map_err(|e| e.to_string())?;
    f.sync_data().map_err(|e| e.to_string())
}
