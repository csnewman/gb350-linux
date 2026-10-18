//! corelink: work rings in memory Linux allocated (CMA, `/dev/corelink`),
//! each attached to a service here. Linux's side and the record format:
//! kernel/include/uapi/linux/gb350-corelink.h.
//!
//! A ring: an uncached 64-byte header (head = bytes written by Linux, tail =
//! bytes consumed here, idle, fence value / wanted, error, service results)
//! and a power-of-two data buffer of records (`tag << 16 | flags`, payload
//! length, payload padded to 8). Linux writes records straight to memory
//! (write-combining), so their cache lines are invalidated here before they
//! are read. Every record processed advances the fence; Linux's doorbell is
//! rung when a waiter's target is reached.

use crate::hw;
use core::ptr::{read_volatile, write_volatile};

const KSEG0: usize = 0x8000_0000;
const KSEG1: usize = 0xa000_0000;
/// RAM the rings may live in (physical).
const RAM_END: u32 = 0x1000_0000;
const MAX_CHANNELS: usize = 8;
const LINE: usize = 16; // the 24KE's data cache line

// Header offsets (uapi CORELINK_RING_*).
const HEAD: usize = 0x00;
const TAIL: usize = 0x04;
const IDLE: usize = 0x08;
const FENCE: usize = 0x10;
const WANTED: usize = 0x14;
const ERROR: usize = 0x18;
/// Service results (service specific).
const RESULT: usize = 0x20;

const OP_ATTACH: u32 = 1;
const OP_DETACH: u32 = 2;

// errno values for Linux.
const EINVAL: u32 = 22;
const ENOSPC: u32 = 28;
const ENOENT: u32 = 2;

/// Services.
pub const SERVICE_TEST: u32 = 1;
pub const SERVICE_PS1GPU: u32 = 2;

/// A channel's service results in its header (RESULT + 4 * i).
pub struct Results {
    header: usize,
}

impl Results {
    pub fn get(&self, i: usize) -> u32 {
        unsafe { read_volatile((self.header + RESULT + 4 * i) as *const u32) }
    }

    pub fn set(&self, i: usize, v: u32) {
        unsafe { write_volatile((self.header + RESULT + 4 * i) as *mut u32, v) }
    }
}

#[derive(Clone, Copy)]
struct Channel {
    service: u32,
    header: usize, // KSEG1
    data: usize,   // KSEG0
    mask: u32,
    fence: u32,
}

static mut CHANNELS: [Option<Channel>; MAX_CHANNELS] = [None; MAX_CHANNELS];

fn hdr(c: &Channel, off: usize) -> u32 {
    unsafe { read_volatile((c.header + off) as *const u32) }
}

fn set_hdr(c: &Channel, off: usize, v: u32) {
    unsafe { write_volatile((c.header + off) as *mut u32, v) }
}

/// Drops cached copies of [addr, addr + len) (KSEG0): memory is newer.
pub fn invalidate(addr: usize, len: usize) {
    let mut a = addr & !(LINE - 1);
    while a < addr + len {
        unsafe { core::arch::asm!("cache 0x11, 0({0})", in(reg) a) }; // Hit_Invalidate_D
        a += LINE;
    }
}

/// Writes cached data of [addr, addr + len) (KSEG0) to memory and drops it.
pub fn writeback(addr: usize, len: usize) {
    let mut a = addr & !(LINE - 1);
    while a < addr + len {
        unsafe { core::arch::asm!("cache 0x15, 0({0})", in(reg) a) }; // Hit_Writeback_Inv_D
        a += LINE;
    }
    unsafe { core::arch::asm!("sync") };
}

/// Writes back and drops the whole data cache (index operations, geometry
/// from Config1): no line of any buffer stays cached here.
pub fn flush_dcache() {
    let c1: u32;
    unsafe { core::arch::asm!("mfc0 {0}, $16, 1", out(reg) c1) };
    let dl = (c1 >> 10) & 7;
    if dl == 0 {
        return;
    }
    let line = 2usize << dl;
    let sets = 64usize << ((c1 >> 13) & 7);
    let ways = ((c1 >> 7) & 7) as usize + 1;
    let mut a = KSEG0;
    while a < KSEG0 + line * sets * ways {
        unsafe { core::arch::asm!("cache 0x01, 0({0})", in(reg) a) }; // Index_Writeback_Inv_D
        a += line;
    }
    unsafe { core::arch::asm!("sync") };
}

/// GB350_SYSCORE_CORELINK request: words in, reply words out.
#[allow(static_mut_refs)]
pub fn request(w: &[u32]) -> [u32; 2] {
    let chans = unsafe { &mut CHANNELS };
    match w.first().copied() {
        Some(OP_ATTACH) if w.len() >= 5 => {
            let (service, header, data, size) = (w[1], w[2], w[3], w[4]);
            let ok = (service == SERVICE_TEST || service == SERVICE_PS1GPU)
                && header % 64 == 0
                && header < RAM_END - 64
                && data % 16 == 0
                && size >= 64
                && size.is_power_of_two()
                && data < RAM_END
                && data.checked_add(size).is_some_and(|e| e <= RAM_END);
            if !ok {
                return [EINVAL, 0];
            }
            let Some(slot) = chans.iter().position(Option::is_none) else { return [ENOSPC, 0] };
            let c = Channel { service, header: KSEG1 | header as usize, data: KSEG0 | data as usize, mask: size - 1, fence: 0 };
            // Linux allocated the header zeroed; start from what it says.
            let c = Channel { fence: hdr(&c, FENCE), ..c };
            set_hdr(&c, IDLE, 1);
            chans[slot] = Some(c);
            log!("corelink: channel {slot}: service {service}, ring {size} bytes at {data:#x}");
            [0, slot as u32]
        }
        Some(OP_DETACH) if w.len() >= 2 => match chans.get_mut(w[1] as usize) {
            Some(c @ Some(_)) => {
                *c = None;
                log!("corelink: channel {} detached", w[1]);
                [0, 0]
            }
            _ => [ENOENT, 0],
        },
        _ => [EINVAL, 0],
    }
}

/// Processes queued records for up to `budget` CP0 count cycles. True if
/// work remains (do not sleep).
#[allow(static_mut_refs)]
pub fn poll(budget: u32) -> bool {
    let start = hw::count();
    let mut more = false;
    for slot in 0..MAX_CHANNELS {
        let Some(mut c) = (unsafe { CHANNELS[slot] }) else { continue };
        let fence0 = c.fence;
        let head = hdr(&c, HEAD);
        let mut tail = hdr(&c, TAIL);
        if head == tail {
            // Going idle: announce it, then look once more (Linux may have
            // written meanwhile and seen idle = 0).
            if hdr(&c, IDLE) == 0 {
                set_hdr(&c, IDLE, 1);
                crate::shm::barrier();
                if hdr(&c, HEAD) != tail {
                    set_hdr(&c, IDLE, 0);
                    more = true;
                }
            }
            continue;
        }
        set_hdr(&c, IDLE, 0);
        while tail != head {
            let rec = c.data + (tail & c.mask) as usize;
            invalidate(rec, 8);
            let (w0, len) = unsafe { (read_volatile(rec as *const u32), read_volatile((rec + 4) as *const u32)) };
            let total = 8 + ((len + 7) & !7);
            if total > c.mask + 1 - (tail & c.mask) {
                set_hdr(&c, ERROR, 0xbad0_0001); // record crosses the end: Linux bug
                tail = head;
                break;
            }
            let tag = (w0 >> 16) as u16;
            if tag != 0 {
                invalidate(rec + 8, len as usize);
                let payload = unsafe { core::slice::from_raw_parts((rec + 8) as *const u8, len as usize) };
                service(&c, tag, payload);
                c.fence = c.fence.wrapping_add(1);
            }
            tail = tail.wrapping_add(total);
            if hw::count().wrapping_sub(start) > budget {
                break;
            }
        }
        set_hdr(&c, TAIL, tail);
        if c.fence != fence0 {
            set_hdr(&c, FENCE, c.fence);
            crate::shm::barrier();
            // Ring once, as the fence passes what a waiter wants: Linux
            // leaves `wanted` set after its wait, and ringing whenever the
            // fence is past it interrupted Linux after every batch of work
            // from then on. (A waiter that set `wanted` after the fence had
            // passed sees that itself before sleeping.)
            let wanted = hdr(&c, WANTED);
            if wanted != 0 && (c.fence.wrapping_sub(wanted) as i32) >= 0 && (fence0.wrapping_sub(wanted) as i32) < 0 {
                hw::ring_linux();
            }
        }
        unsafe { CHANNELS[slot] = Some(c) };
        if tail != head || hdr(&c, HEAD) != tail {
            more = true;
        }
        if hw::count().wrapping_sub(start) > budget {
            return true;
        }
    }
    more
}

fn service(c: &Channel, tag: u16, payload: &[u8]) {
    match c.service {
        SERVICE_TEST => test_service(c, tag, payload),
        SERVICE_PS1GPU => {
            if let Err(e) = crate::ps1gpu::handle(&Results { header: c.header }, tag, payload) {
                set_hdr(c, ERROR, 0x9000_0000 | (tag as u32) << 8 | e);
            }
        }
        _ => set_hdr(c, ERROR, 0xbad0_0002),
    }
}

/// Test service: tag 1 SUM (result: wrapping sum of the payload's words,
/// bytes, records), tag 2 SPIN [microseconds], tag 3 COPY [dst, src, len]
/// physical, through the cache (bandwidth), tag 4 I2C_SCAN [scl, sda] of
/// bank-L pins (i2cscan.rs).
fn test_service(c: &Channel, tag: u16, p: &[u8]) {
    let w = |i: usize| p.get(4 * i..4 * i + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
    match tag {
        1 => {
            let mut sum = hdr(c, RESULT);
            for chunk in p.chunks_exact(4) {
                sum = sum.wrapping_add(u32::from_le_bytes(chunk.try_into().unwrap()));
            }
            set_hdr(c, RESULT, sum);
            set_hdr(c, RESULT + 4, hdr(c, RESULT + 4).wrapping_add(p.len() as u32));
            set_hdr(c, RESULT + 8, hdr(c, RESULT + 8).wrapping_add(1));
        }
        2 => hw::delay_us(w(0).min(1_000_000)),
        4 => {
            // I2C scan [scl pin, sda pin] of bank L (i2cscan.rs): results
            // status, bitmap x 4, first answer (address << 8 | byte).
            let (st, map, first) = crate::i2cscan::scan(w(0), w(1));
            set_hdr(c, RESULT, st);
            for (i, m) in map.iter().enumerate() {
                set_hdr(c, RESULT + 4 + 4 * i, *m);
            }
            set_hdr(c, RESULT + 20, first);
        }
        3 => {
            let (dst, src, len) = (w(0), w(1), w(2));
            if src.checked_add(len).is_some_and(|e| e <= RAM_END) && dst.checked_add(len).is_some_and(|e| e <= RAM_END) {
                let (d, s) = (KSEG0 | dst as usize, KSEG0 | src as usize);
                invalidate(s, len as usize);
                unsafe { core::ptr::copy_nonoverlapping(s as *const u8, d as *mut u8, len as usize) };
                writeback(d, len as usize);
            } else {
                set_hdr(c, ERROR, 0xbad0_0003);
            }
        }
        _ => set_hdr(c, ERROR, 0xbad0_0004),
    }
}
