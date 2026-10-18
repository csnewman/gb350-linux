//! Hardware used by the firmware: CP0, the mailbox doorbells, this core's
//! interrupt controller and the timer.
//!
//! Mailbox (probed 2026-10-16): channel n has a block per core,
//! Linux 0x18800800 + 0x20 n and this core 0x18866800 + 0x20 n. A sender sets bit 0
//! of +0x00 in its own block ("busy" until released); the receiver's block
//! then shows +0x08 bit 8 (request pending, write 1 to clear) and, if +0x08
//! bit 0 is set, raises its mailbox interrupt (hwirq 66). The receiver writes
//! +0x04 bit 0 to release, which clears the sender's busy bit and sets the
//! sender's +0x08 bit 16 (release pending). The sender's +0x08 bit 1 must be
//! set for its requests to reach the receiver at all (it also enables the
//! release interrupt) - the vendor's send-channel value 0x10002.
//! Channel 0 = Linux -> core doorbell, channel 4 = core -> Linux.

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};

const MBOX_OURS: usize = 0xb886_6800;
const DOORBELL_IN: usize = 0; // Linux -> this core
const DOORBELL_OUT: usize = 4; // this core -> Linux

/// This core's interrupt controller: +0x38.. enable words for hwirq 0-95.
const INTC_ENABLE: usize = 0xb886_6038;
const HWIRQ_MBOX: u32 = 66;

/// SoC timers at 0x18818a00: 8 channels of +0 count, +4 compare, +8 control
/// (bit 4 run, bit 2 compare interrupt enable, bit 3 pending - write 1 to
/// clear), all on hwirq 22 (the stock AVP's FUN_8bdec220/074). Channel 5
/// counts at 27 MHz / 16 = 1.6875 MHz (measured) and was the AVP's 1 ms tick;
/// the firmware uses it for all its deadlines (tick and button-scan steps).
/// CP0 Count cannot be used: it stops while the core sleeps in `wait`.
const SB_TICK: usize = 0xb881_8a50; // channel 5
const SB_OTHER: usize = 0xb881_8a30; // channel 3, left running by the bootloader
const SB_CTL_RUN: u32 = 0x10;
const SB_CTL_PENDING: u32 = 0x08;
const SB_CTL_IRQ: u32 = 0x04;
pub const SB_HZ: u32 = 1_687_500;
const HWIRQ_TIMER: u32 = 22;

/// CP0 Status/Cause bits.
pub const IM_MBOX: u32 = 1 << 10; // IP2 (hwirq 66, as the stock AVP)
pub const IM_INTC: u32 = 1 << 11; // IP3 (other hwirqs, e.g. the timer)

fn rd(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

fn wr(addr: usize, v: u32) {
    unsafe { write_volatile(addr as *mut u32, v) }
}

macro_rules! mfc0 {
    ($reg:literal, $sel:literal) => {{
        let v: u32;
        unsafe {
            asm!(concat!(".set noat\n", "mfc0 {0}, $", $reg, ", ", $sel, "\n.set at"), out(reg) v, options(nomem, nostack))
        };
        v
    }};
}

macro_rules! mtc0 {
    ($reg:literal, $sel:literal, $v:expr) => {
        unsafe {
            asm!(concat!(".set noat\n", "mtc0 {0}, $", $reg, ", ", $sel, "\n.set at"), "ehb", in(reg) $v, options(nomem, nostack))
        }
    };
}

pub fn ebase() -> u32 {
    mfc0!("15", "1")
}
pub fn prid() -> u32 {
    mfc0!("15", "0")
}
pub fn config() -> u32 {
    mfc0!("16", "0")
}
pub fn config1() -> u32 {
    mfc0!("16", "1")
}
pub fn status() -> u32 {
    mfc0!("12", "0")
}
pub fn count() -> u32 {
    mfc0!("9", "0")
}

pub fn enable_interrupts(mask: u32) {
    let s = status() | mask | 1;
    mtc0!("12", "0", s);
}

pub fn disable_interrupts() {
    let s = status() & !1;
    mtc0!("12", "0", s);
}

pub fn wait() {
    unsafe { asm!("wait", options(nomem, nostack)) }
}

/// Doorbell interrupt from Linux on, any stale state cleared.
/// Clearing a request also releases it: an unreleased sender stays busy and
/// never rings again.
pub fn mbox_init() {
    wr(MBOX_OURS + 0x20 * DOORBELL_IN + 8, 0x101);
    let rel = MBOX_OURS + 0x20 * DOORBELL_IN + 4;
    wr(rel, rd(rel) | 1);
    wr(MBOX_OURS + 0x20 * DOORBELL_OUT + 8, 0x10002);
    intc_enable(HWIRQ_MBOX);
}

fn intc_enable(hwirq: u32) {
    let w = INTC_ENABLE + 4 * (hwirq / 32) as usize;
    wr(w, rd(w) | 1 << (hwirq % 32));
}

pub fn sb_count() -> u32 {
    rd(SB_TICK)
}

/// Starts the timer: first interrupt at `first` (an SB count).
pub fn timer_init(first: u32) {
    wr(SB_OTHER + 8, rd(SB_OTHER + 8) & !SB_CTL_IRQ | SB_CTL_PENDING);
    wr(SB_TICK + 4, first);
    wr(SB_TICK + 8, SB_CTL_RUN | SB_CTL_IRQ | SB_CTL_PENDING);
    intc_enable(HWIRQ_TIMER);
}

pub fn timer_pending() -> bool {
    rd(SB_TICK + 8) & SB_CTL_PENDING != 0
}

/// Acknowledges the timer and sets the next deadline (an SB count). The
/// compare matches on equality only: the caller re-checks that `next` is
/// still ahead afterwards.
pub fn timer_set(next: u32) {
    wr(SB_TICK + 4, next);
    wr(SB_TICK + 8, rd(SB_TICK + 8) | SB_CTL_PENDING);
    // Read back so the acknowledge reaches the timer before returning
    // (otherwise the still-asserted line re-enters the handler once).
    rd(SB_TICK + 8);
}

/// True once SB count `deadline` has been reached.
pub fn sb_due(deadline: u32) -> bool {
    sb_count().wrapping_sub(deadline) as i32 >= 0
}

/// Microseconds to SB timer counts (27 MHz / 16).
pub fn sb_counts(us: u32) -> u32 {
    (us * 27 / 16).max(1)
}

/// Acknowledges a doorbell from Linux; true if there was one. Also clears the
/// release flag of this core's outgoing doorbell (Linux took it).
pub fn mbox_ack() -> bool {
    let out = MBOX_OURS + 0x20 * DOORBELL_OUT + 8;
    let v = rd(out);
    if v & 0x10000 != 0 {
        wr(out, (v & 0xfffe_feff) | 0x10000);
    }
    let reg = MBOX_OURS + 0x20 * DOORBELL_IN + 8;
    let v = rd(reg);
    if v & 0x100 == 0 {
        return false;
    }
    wr(reg, (v & 0xfffe_feff) | 0x100);
    let rel = MBOX_OURS + 0x20 * DOORBELL_IN + 4;
    wr(rel, rd(rel) | 1);
    true
}

/// Rings Linux's doorbell unless it has not yet taken the previous ring.
pub fn ring_linux() {
    let reg = MBOX_OURS + 0x20 * DOORBELL_OUT;
    let v = rd(reg);
    if v & 1 == 0 {
        wr(reg, v | 1);
    }
}

/// CP0 Count ticks per millisecond (= core clock / 2), measured against the
/// SoC timer over 4 ms. The timer must be running (it is, from boot).
pub fn calibrate_count() -> u32 {
    let s0 = sb_count();
    while sb_count().wrapping_sub(s0) < 100 {}
    let (s1, c1) = (sb_count(), count());
    while sb_count().wrapping_sub(s1) < SB_HZ / 250 {}
    let (s2, c2) = (sb_count(), count());
    let us = s2.wrapping_sub(s1) as u64 * 1_000_000 / SB_HZ as u64;
    (c2.wrapping_sub(c1) as u64 * 1000 / us) as u32
}

/// GPIO banks (vendor gpio-hc): +0x0c input, +0x10 output,
/// +0x14 direction (1 = output). Shared with Linux: output and direction
/// changes go through `gpio_update` (read-modify-write).
pub const GPIO_L: usize = 0xb880_0044;
pub const GPIO_T: usize = 0xb880_0344;
pub const GPIO_IN: usize = 0x0c;
pub const GPIO_OUT: usize = 0x10;
pub const GPIO_DIR: usize = 0x14;

pub fn gpio_in(bank: usize) -> u32 {
    rd(bank + GPIO_IN)
}

/// Read-modify-write of a GPIO register, under the cross-core lock that the
/// Linux GPIO driver takes for the same registers (hwspinlock 0), with this core's
/// interrupts off.
pub fn gpio_update(bank: usize, reg: usize, mask: u32, bits: u32) {
    let a = bank + reg;
    critical(|| {
        crate::shm::lock(crate::shm::LOCK_GPIO);
        wr(a, rd(a) & !mask | bits & mask);
        crate::shm::unlock(crate::shm::LOCK_GPIO);
    })
}

/// Runs `f` with interrupts disabled (R1 has no `di`: clear Status.IE).
pub fn critical<R>(f: impl FnOnce() -> R) -> R {
    let s = status();
    mtc0!("12", "0", s & !1);
    let r = f();
    mtc0!("12", "0", s);
    r
}

/// SAR ADC (0x18818400, Linux hc16xx-adc): latest 8-bit
/// sample at +0xd0 + ch (ch 4/5 at +0xd6/+0xd7), calibrated against channel
/// 0 as the vendor does; returns millivolts at the pin.
pub fn adc_mv(ch: usize) -> Option<u32> {
    const ADC: usize = 0xb881_8400;
    let sample = |c: usize| unsafe { read_volatile((ADC + 0xd0 + c + if c > 3 { 2 } else { 0 }) as *const u8) } as u32;
    if rd(ADC + 0x8c) & 1 << (16 + ch) == 0 {
        return None;
    }
    let reference = sample(0);
    if reference == 0 {
        return None;
    }
    Some((sample(ch) * 241 / reference).min(255) * 1890 / 241)
}

static COUNT_PER_US: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(297);

pub fn set_count_per_us(n: u32) {
    COUNT_PER_US.store(n, core::sync::atomic::Ordering::Relaxed);
}

/// Busy-waits (CP0 Count runs while the core is awake).
pub fn delay_us(us: u32) {
    let n = us * COUNT_PER_US.load(core::sync::atomic::Ordering::Relaxed);
    let c0 = count();
    while count().wrapping_sub(c0) < n {}
}
