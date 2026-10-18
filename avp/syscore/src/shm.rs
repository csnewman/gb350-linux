//! Shared memory with Linux: physical 0x0bd00000, 64 KiB, accessed uncached
//! (KSEG1) on both sides - the cores are not cache-coherent. Layout (ABI 1;
//! the Linux driver `gb350-syscore.c` has the same definitions):
//!
//! | offset  | contents                                              |
//! |---------|-------------------------------------------------------|
//! | 0x0000  | header: see `H_*`                                     |
//! | 0x0100  | Linux -> core ring: head (Linux), 0x0140 tail (core)   |
//! | 0x0200  | ... 64 slots of 64 bytes                               |
//! | 0x1200  | core -> Linux ring: head (core), 0x1240 tail (Linux)   |
//! | 0x1300  | ... 64 slots of 64 bytes                               |
//! | 0x3f00  | cross-core locks: 16 bytes each, see `lock`             |
//! | 0x4000  | log ring: write index (core), 0x4004 read index (Linux)|
//! | 0x4100  | ... 16 KiB of text                                     |
//!
//! Ring indices are free-running u32 counters; slot = index % 64. A message
//! slot is `kind: u16, len: u16, seq: u32, data: [u8; 56]`.

use core::ptr::{read_volatile, write_volatile};

pub const BASE: usize = 0xabd0_0000;

pub const MAGIC: u32 = 0x4353_5953; // "SYSC"
pub const ABI: u32 = 1;

pub const H_MAGIC: usize = 0x00;
pub const H_ABI: usize = 0x04;
pub const H_VERSION: usize = 0x08;
pub const H_STATE: usize = 0x0c;
pub const H_TICKS: usize = 0x10;
pub const H_TICK_HZ: usize = 0x14;
pub const H_CRASH: usize = 0x18; // vector, cause, epc, badvaddr, status
pub const H_INFO: usize = 0x30; // ebase, prid, config, config1
pub const H_COUNT_PER_MS: usize = 0x40;
pub const H_EXCEPTIONS: usize = 0x44; // exceptions taken (incl. interrupts)
pub const H_LAST_CAUSE: usize = 0x48;
pub const H_LAST_STATUS: usize = 0x4c;
pub const H_LOOPS: usize = 0x50; // main loop iterations
/// Restart handshake: the core parks in code copied to PARK_CODE, spinning
/// until Linux writes an entry address here, then jumps to it.
pub const H_PARK_GO: usize = 0x60;
pub const H_PARKED: usize = 0x64; // 1 while parked
pub const PARK_CODE: usize = 0xf000;
pub const H_BUILD: usize = 0x80; // NUL-terminated build string, 64 bytes
/// Status for debugging (Linux sysfs `debug`).
pub const H_INPUT: usize = 0xc0; // u64 input state (see input.rs)
pub const H_SCANS: usize = 0xc8; // shift-register scans done
pub const H_SCAN_US: usize = 0xcc; // last scan's duration
pub const H_BATTERY_MV: usize = 0xd0;
pub const H_CHARGER_MV: usize = 0xd4;
pub const H_CARD: usize = 0xd8; // 1 = SD card present
pub const H_SUPERVISOR: usize = 0xdc; // pending power-off reason << 16 | enabled checks
pub const H_SPEAKER: usize = 0xe0; // 1 = speaker amplifier muted
/// Load: CP0 count cycles spent idle (in `wait`, interrupts excluded) and
/// the count when it was last written, both wrapping. Busy share between two
/// samples = 1 - d(idle) / d(cycles).
pub const H_IDLE: usize = 0xe4;
pub const H_CYCLES: usize = 0xe8;

pub const STATE_RUNNING: u32 = 1;
pub const STATE_CRASHED: u32 = 0xdead;

const SLOTS: u32 = 64;
const SLOT_SIZE: usize = 64;
pub const MSG_DATA: usize = SLOT_SIZE - 8;

struct RingLayout {
    head: usize,
    tail: usize,
    slots: usize,
}

const TO_CORE: RingLayout = RingLayout { head: 0x0100, tail: 0x0140, slots: 0x0200 };
const TO_LINUX: RingLayout = RingLayout { head: 0x1200, tail: 0x1240, slots: 0x1300 };

const LOG_WR: usize = 0x4000;
const LOG_DATA: usize = 0x4100;
const LOG_SIZE: u32 = 0x4000;

pub fn read(offset: usize) -> u32 {
    unsafe { read_volatile((BASE + offset) as *const u32) }
}

pub fn write(offset: usize, value: u32) {
    unsafe { write_volatile((BASE + offset) as *mut u32, value) }
}

fn write_byte(offset: usize, value: u8) {
    unsafe { write_volatile((BASE + offset) as *mut u8, value) }
}

/// Orders uncached stores before the following ones (MIPS `sync`).
pub fn barrier() {
    unsafe { core::arch::asm!("sync", options(nostack)) }
}

pub struct Msg {
    pub kind: u16,
    pub len: u16,
    pub seq: u32,
    pub data: [u8; MSG_DATA],
}

impl Msg {
    pub fn new(kind: u16, seq: u32, payload: &[u8]) -> Msg {
        let len = payload.len().min(MSG_DATA);
        let mut data = [0u8; MSG_DATA];
        data[..len].copy_from_slice(&payload[..len]);
        Msg { kind, len: len as u16, seq, data }
    }

    pub fn payload(&self) -> &[u8] {
        &self.data[..(self.len as usize).min(MSG_DATA)]
    }
}

/// Next message from Linux, if any.
pub fn recv() -> Option<Msg> {
    let r = &TO_CORE;
    let tail = read(r.tail);
    if read(r.head) == tail {
        return None;
    }
    let slot = r.slots + (tail % SLOTS) as usize * SLOT_SIZE;
    let w0 = read(slot);
    let mut msg = Msg { kind: w0 as u16, len: (w0 >> 16) as u16, seq: read(slot + 4), data: [0; MSG_DATA] };
    for (i, chunk) in msg.data.chunks_exact_mut(4).enumerate() {
        chunk.copy_from_slice(&read(slot + 8 + 4 * i).to_le_bytes());
    }
    barrier();
    write(r.tail, tail.wrapping_add(1));
    Some(msg)
}

/// Queues a message for Linux; false if the ring is full.
pub fn send(msg: &Msg) -> bool {
    let r = &TO_LINUX;
    let head = read(r.head);
    if head.wrapping_sub(read(r.tail)) >= SLOTS {
        return false;
    }
    let slot = r.slots + (head % SLOTS) as usize * SLOT_SIZE;
    write(slot, msg.kind as u32 | (msg.len as u32) << 16);
    write(slot + 4, msg.seq);
    for (i, chunk) in msg.data.chunks_exact(4).enumerate() {
        write(slot + 8 + 4 * i, u32::from_le_bytes(chunk.try_into().unwrap()));
    }
    barrier();
    write(r.head, head.wrapping_add(1));
    true
}

/// Resets both rings and the log (at boot, before Linux is told the firmware runs).
pub fn reset() {
    for off in [TO_CORE.head, TO_CORE.tail, TO_LINUX.head, TO_LINUX.tail, LOG_WR, LOG_WR + 4] {
        write(off, 0);
    }
}

/// Appends text to the log ring; drops it if Linux has not drained enough.
pub fn log_bytes(bytes: &[u8]) {
    let wr = read(LOG_WR);
    let rd = read(LOG_WR + 4);
    if wr.wrapping_sub(rd) + bytes.len() as u32 > LOG_SIZE {
        return;
    }
    for (i, &b) in bytes.iter().enumerate() {
        write_byte(LOG_DATA + (wr.wrapping_add(i as u32) % LOG_SIZE) as usize, b);
    }
    barrier();
    write(LOG_WR, wr.wrapping_add(bytes.len() as u32));
}

pub fn set_build(s: &str) {
    for (i, &b) in s.as_bytes().iter().take(63).enumerate() {
        write_byte(H_BUILD + i, b);
    }
    write_byte(H_BUILD + s.len().min(63), 0);
}

/// Cross-core locks (Peterson's algorithm on uncached memory: plain loads
/// and stores, `sync` between them). Lock i at 0x3f00 + 16 i: +0 Linux
/// wants it, +4 the core wants it, +8 turn (0 = Linux may go first, 1 = the
/// core). Linux implements them as hwspinlocks (gb350-syscore.c).
pub const LOCK_GPIO: usize = 0;
const LOCKS: usize = 0x3f00;
pub const NUM_LOCKS: usize = 4;

pub fn lock(i: usize) {
    let l = LOCKS + 16 * i;
    write(l + 4, 1);
    write(l + 8, 0);
    barrier();
    // Linux holds a lock for a few register accesses at most; never hang on
    // a Linux that died holding one (~1 ms at most here).
    for _ in 0..20_000 {
        if read(l) == 0 || read(l + 8) != 0 {
            return;
        }
    }
}

pub fn unlock(i: usize) {
    barrier();
    write(LOCKS + 16 * i + 4, 0);
}

/// This core's side of all locks released (at start-up).
pub fn reset_locks() {
    for i in 0..NUM_LOCKS {
        write(LOCKS + 16 * i + 4, 0);
    }
}
