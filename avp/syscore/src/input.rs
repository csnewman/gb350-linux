//! Buttons.
//!
//! - Shift register on GPIO L8 (clock) / L9 (data, doubles as the load
//!   input), 16 bits, active low. The chip only answers
//!   at ~60 us per step (probably a small MCU), so a scan is a sequence of
//!   timed steps run from the timer interrupt (`Scanner::step`), not a busy
//!   loop: the core sleeps in between.
//! - Direct inputs on bank L (active low): L2 Fn, L7 L2, L11 R2, L12/L13
//!   volume, L14 power, L15 headphone switch - sampled at the end of each
//!   scan.
//!
//! The state reported to Linux is a u64, 1 = pressed / active: bits 0-15 the
//! shift register, bit 32 + n bank L pin n. Changes are debounced (the same
//! reading in `debounce` consecutive scans).

use core::sync::atomic::{AtomicU32, Ordering};

use crate::hw::{self, GPIO_DIR, GPIO_L, GPIO_OUT};

const CLK: u32 = 1 << 8;
const DATA: u32 = 1 << 9;
pub const DIRECT: u32 = 1 << 2 | 1 << 7 | 1 << 11 | 1 << 12 | 1 << 13 | 1 << 14 | 1 << 15;
pub const POWER_BIT: u64 = 1 << (32 + 14);

/// Settings, changed by Linux (CONFIG message; values in microseconds).
pub struct Config {
    pub load: AtomicU32,
    pub release: AtomicU32,
    pub low: AtomicU32,
    pub high: AtomicU32,
    /// Scan start to scan start.
    pub period: AtomicU32,
    pub debounce: AtomicU32,
}

/// Defaults: the timing the Linux driver used (usleep_range(55, 75) steps).
pub static CONFIG: Config = Config {
    load: AtomicU32::new(60),
    release: AtomicU32::new(60),
    low: AtomicU32::new(60),
    high: AtomicU32::new(0),
    period: AtomicU32::new(4000),
    debounce: AtomicU32::new(2),
};

/// Latest completed scan, handed from the interrupt to the main loop.
static SCAN_SEQ: AtomicU32 = AtomicU32::new(0);
static SCAN_SHIFT: AtomicU32 = AtomicU32::new(0xffff);
static SCAN_BANK: AtomicU32 = AtomicU32::new(u32::MAX);
static SCAN_US: AtomicU32 = AtomicU32::new(0);

/// The scan state machine; runs in interrupt context only.
pub struct Scanner {
    /// 0 idle (next: load), 1 release, 2 read bit 0 + clock low, then
    /// 3 + 2 (i - 1) = clock high + read bit i, 4 + 2 (i - 1) = clock low.
    phase: u32,
    bits: u32,
    start: u32,
}

impl Scanner {
    pub const fn new() -> Scanner {
        Scanner { phase: 0, bits: 0, start: 0 }
    }

    /// Does the step that is due at SB count `now`; returns the SB count at
    /// which the next one is due.
    pub fn step(&mut self, now: u32) -> u32 {
        let c = &CONFIG;
        let us = |a: &AtomicU32| hw::sb_counts(a.load(Ordering::Relaxed));
        let data = || (hw::gpio_in(GPIO_L) & DATA != 0) as u32;
        match self.phase {
            0 => {
                self.start = now;
                hw::gpio_update(GPIO_L, GPIO_OUT, DATA | CLK, CLK);
                hw::gpio_update(GPIO_L, GPIO_DIR, DATA | CLK, DATA | CLK); // load
                self.phase = 1;
                now.wrapping_add(us(&c.load))
            }
            1 => {
                hw::gpio_update(GPIO_L, GPIO_DIR, DATA, 0);
                self.phase = 2;
                now.wrapping_add(us(&c.release))
            }
            2 => {
                self.bits = data();
                hw::gpio_update(GPIO_L, GPIO_OUT, CLK, 0);
                self.phase = 3;
                now.wrapping_add(us(&c.low))
            }
            p if p % 2 == 1 => {
                let i = (p - 1) / 2;
                hw::gpio_update(GPIO_L, GPIO_OUT, CLK, CLK);
                self.bits |= data() << i;
                if i == 15 {
                    let bank = hw::gpio_in(GPIO_L);
                    let end = hw::sb_count();
                    SCAN_SHIFT.store(self.bits, Ordering::Relaxed);
                    SCAN_BANK.store(bank, Ordering::Relaxed);
                    SCAN_US.store(end.wrapping_sub(self.start) * 16 / 27, Ordering::Relaxed);
                    SCAN_SEQ.fetch_add(1, Ordering::Release);
                    self.phase = 0;
                    let next = self.start.wrapping_add(us(&c.period));
                    return if (next.wrapping_sub(end) as i32) > 0 { next } else { end };
                }
                self.phase = p + 1;
                now.wrapping_add(us(&c.high))
            }
            p => {
                hw::gpio_update(GPIO_L, GPIO_OUT, CLK, 0);
                self.phase = p + 1;
                now.wrapping_add(us(&c.low))
            }
        }
    }
}

/// One blocking scan clocking `n` bits (experiments: SCAN_TEST).
pub fn scan_bits(load: u32, release: u32, low: u32, high: u32, n: u32) -> u32 {
    hw::gpio_update(GPIO_L, GPIO_OUT, DATA | CLK, CLK);
    hw::gpio_update(GPIO_L, GPIO_DIR, DATA | CLK, DATA | CLK);
    hw::delay_us(load);
    hw::gpio_update(GPIO_L, GPIO_DIR, DATA, 0);
    hw::delay_us(release);
    let mut bits = (hw::gpio_in(GPIO_L) & DATA != 0) as u32;
    for i in 1..n {
        hw::gpio_update(GPIO_L, GPIO_OUT, CLK, 0);
        hw::delay_us(low);
        hw::gpio_update(GPIO_L, GPIO_OUT, CLK, CLK);
        bits |= ((hw::gpio_in(GPIO_L) & DATA != 0) as u32) << i;
        hw::delay_us(high);
    }
    bits
}

/// Debounced state, main loop side.
pub struct Input {
    seen: u32,
    pub state: u64,
    candidate: u64,
    count: u32,
    reported: bool,
}

impl Input {
    pub const fn new() -> Input {
        Input { seen: 0, state: 0, candidate: 0, count: 0, reported: false }
    }

    pub fn scans() -> u32 {
        SCAN_SEQ.load(Ordering::Acquire)
    }

    pub fn last_scan_us() -> u32 {
        SCAN_US.load(Ordering::Relaxed)
    }

    /// Takes the latest scan; returns the new state when it changed (and
    /// once after start-up, so Linux learns the initial state).
    pub fn update(&mut self) -> Option<u64> {
        let seq = SCAN_SEQ.load(Ordering::Acquire);
        if seq == self.seen {
            return None;
        }
        self.seen = seq;
        let shift = !SCAN_SHIFT.load(Ordering::Relaxed) & 0xffff;
        let bank = !SCAN_BANK.load(Ordering::Relaxed) & DIRECT;
        let raw = shift as u64 | (bank as u64) << 32;
        if raw != self.candidate {
            self.candidate = raw;
            self.count = 0;
        }
        self.count += 1;
        let settled = self.count >= CONFIG.debounce.load(Ordering::Relaxed).max(1);
        if settled && (raw != self.state || !self.reported) {
            self.state = raw;
            self.reported = true;
            return Some(raw);
        }
        None
    }
}
