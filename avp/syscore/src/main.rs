//! gb350-syscore: firmware for the GB350's second core (MIPS 24KE, the core
//! the stock firmware used for its "AVP"). It boots as `cubegm/avp.uImage`
//! or is (re)loaded at runtime by the Linux `gb350-syscore` driver, and talks
//! to Linux through shared memory (`shm`) and mailbox doorbells (`hw`).
//!
//! - Runtime: exceptions with a crash record, one SoC timer for all
//!   deadlines (1 kHz tick, button-scan steps), doorbells, message rings, log.
//! - Buttons (`input`): scanned here, debounced, sent to Linux as INPUT.
//! - Power supervision (`supervisor`): power-button hold, card removal,
//!   critical battery; powers off for Linux too (POWER_OFF).
//! - Speaker amplifier mute vs headphones (`speaker`).

#![no_std]
#![no_main]
#![feature(asm_experimental_arch)]

mod boot;
mod hw;
#[macro_use]
mod log;
mod corelink;
mod i2cscan;
mod input;
mod ps1gpu;
mod shm;
mod speaker;
mod supervisor;

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const VERSION: u32 = 0x00_03_00; // 0.3.0: corelink
const BUILD: &str = concat!("gb350-syscore ", env!("CARGO_PKG_VERSION"));

/// Message kinds (Linux: include/linux/soc/hichip/gb350-syscore.h).
mod kind {
    pub const PING: u16 = 1; // Linux -> core, payload echoed
    pub const PONG: u16 = 2; // core -> Linux
    pub const GET_INFO: u16 = 3; // Linux -> core
    pub const INFO: u16 = 4; // core -> Linux: see `handle`
    pub const RESTART: u16 = 5; // Linux -> core: park for a firmware reload
    pub const INPUT: u16 = 6; // core -> Linux: [state lo, state hi, tick]
    pub const GET_INPUT: u16 = 7; // Linux -> core: INPUT reply
    pub const POWER_OFF: u16 = 8; // Linux -> core: drop the power latch now
    pub const POWER_REQUEST: u16 = 9; // core -> Linux: [reason, grace ms]
    pub const CONFIG: u16 = 10; // Linux -> core: [key, value or !0 = read] -> [key, value]
    pub const CORELINK: u16 = 11; // Linux -> core: [op, ...] -> [status, channel] (corelink.rs)
    pub const SCAN_TEST: u16 = 0x80; // [load, release, low, high, bits] us -> [raw, us]
    pub const PEEK: u16 = 0x81; // [addr (KSEG1), n] -> n words
    pub const POKE: u16 = 0x82; // [addr, value] -> [value read back]
}

/// CONFIG keys.
mod key {
    pub const SCAN_LOAD: u32 = 1;
    pub const SCAN_RELEASE: u32 = 2;
    pub const SCAN_LOW: u32 = 3;
    pub const SCAN_HIGH: u32 = 4;
    pub const SCAN_PERIOD: u32 = 5;
    pub const DEBOUNCE: u32 = 6;
    pub const CHECKS: u32 = 7;
    pub const SPEAKER: u32 = 8;
}

static TICKS: AtomicU32 = AtomicU32::new(0);
/// SB timer count at tick 0; tick n is due at TICK_BASE + n * 1687.5.
static TICK_BASE: AtomicU32 = AtomicU32::new(0);
static DOORBELL: AtomicBool = AtomicBool::new(false);
static RX: AtomicU32 = AtomicU32::new(0);
static TX: AtomicU32 = AtomicU32::new(0);
/// Button scanning on (off while SCAN_TEST experiments run).
static SCANNING: AtomicBool = AtomicBool::new(true);

/// Timer state, interrupt context only.
struct Timer {
    tick: u32,
    scan_due: u32,
    scanner: input::Scanner,
}
static mut TIMER: Timer = Timer { tick: 0, scan_due: 0, scanner: input::Scanner::new() };

#[no_mangle]
extern "C" fn syscore_main() -> ! {
    shm::reset();
    shm::reset_locks();
    shm::write(shm::H_STATE, 0);
    shm::write(shm::H_PARKED, 0);
    shm::write(shm::H_PARK_GO, 0);
    for off in [shm::H_TICKS, shm::H_EXCEPTIONS, shm::H_LAST_CAUSE, shm::H_LAST_STATUS, shm::H_LOOPS] {
        shm::write(off, 0);
    }
    for off in (shm::H_INPUT..=shm::H_CYCLES).step_by(4) {
        shm::write(off, 0);
    }
    shm::write(shm::H_ABI, shm::ABI);
    shm::write(shm::H_VERSION, VERSION);
    shm::write(shm::H_INFO, hw::ebase());
    shm::write(shm::H_INFO + 4, hw::prid());
    shm::write(shm::H_INFO + 8, hw::config());
    shm::write(shm::H_INFO + 12, hw::config1());
    for i in 0..5 {
        shm::write(shm::H_CRASH + 4 * i, 0);
    }
    shm::set_build(BUILD);

    let per_ms = hw::calibrate_count();
    shm::write(shm::H_COUNT_PER_MS, per_ms);
    hw::set_count_per_us((per_ms + 500) / 1000);
    shm::write(shm::H_TICK_HZ, 1000);
    let base = hw::sb_count();
    TICK_BASE.store(base, Ordering::Relaxed);
    unsafe {
        TIMER.tick = 0;
        TIMER.scan_due = tick_deadline(base, 1);
        TIMER.scanner = input::Scanner::new();
    }
    // Far enough ahead to still be in the future when interrupts come on
    // (the compare matches on equality only).
    hw::timer_init(tick_deadline(base, 1));
    hw::mbox_init();
    hw::enable_interrupts(hw::IM_INTC | hw::IM_MBOX);

    shm::write(shm::H_STATE, shm::STATE_RUNNING);
    shm::barrier();
    shm::write(shm::H_MAGIC, shm::MAGIC);
    log!("{BUILD} up: PRId {:08x}, core clock {} MHz", hw::prid(), per_ms * 2 / 1000);

    let mut input = input::Input::new();
    let mut supervisor = supervisor::Supervisor::new();
    let mut speaker = speaker::Speaker::new();
    let mut loops = 0u32;
    let mut idle = 0u32;
    loop {
        loops = loops.wrapping_add(1);
        shm::write(shm::H_LOOPS, loops);
        while let Some(msg) = shm::recv() {
            RX.fetch_add(1, Ordering::Relaxed);
            handle(msg, &input);
        }
        if let Some(state) = input.update() {
            shm::write(shm::H_INPUT, state as u32);
            shm::write(shm::H_INPUT + 4, (state >> 32) as u32);
            send_input(state, 0);
        }
        if input::Input::scans() > 2 {
            speaker.poll(input.state); // once the first scans are in
        }
        shm::write(shm::H_SCANS, input::Input::scans());
        shm::write(shm::H_SCAN_US, input::Input::last_scan_us());
        if let supervisor::Action::Request(reason, grace) = supervisor.poll(TICKS.load(Ordering::Relaxed), input.state) {
            let mut buf = [0u8; 8];
            buf[..4].copy_from_slice(&reason.to_le_bytes());
            buf[4..].copy_from_slice(&grace.to_le_bytes());
            reply(kind::POWER_REQUEST, 0, &buf);
        }
        // Work rings (/dev/corelink): about 1 ms at a time, so messages and
        // the rest of the loop keep going; no sleep while work remains.
        let busy = corelink::poll(shm::read(shm::H_COUNT_PER_MS));
        if DOORBELL.swap(false, Ordering::Relaxed) || busy {
            continue; // re-check the ring after a doorbell
        }
        shm::write(shm::H_LAST_STATUS, hw::status());
        // Idle time: the wait, less the interrupt handlers that ran in it
        // (the one that wakes the core runs before `wait` returns).
        let (c0, irq0) = (hw::count(), IRQ_CYCLES.load(Ordering::Relaxed));
        hw::wait();
        let c1 = hw::count();
        let irq = IRQ_CYCLES.load(Ordering::Relaxed).wrapping_sub(irq0);
        idle = idle.wrapping_add(c1.wrapping_sub(c0).saturating_sub(irq));
        shm::write(shm::H_IDLE, idle);
        shm::write(shm::H_CYCLES, c1);
    }
}

/// SB timer count when tick `n` is due (exactly 1 kHz: 1687.5 counts each).
fn tick_deadline(base: u32, n: u32) -> u32 {
    base.wrapping_add(((n as u64 * hw::SB_HZ as u64) / 1000) as u32)
}

fn reply(kind: u16, seq: u32, payload: &[u8]) {
    if shm::send(&shm::Msg::new(kind, seq, payload)) {
        TX.fetch_add(1, Ordering::Relaxed);
    }
    hw::ring_linux();
}

fn words(w: &[u32]) -> ([u8; shm::MSG_DATA], usize) {
    let mut buf = [0u8; shm::MSG_DATA];
    for (i, v) in w.iter().enumerate() {
        buf[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
    }
    (buf, 4 * w.len())
}

fn send_input(state: u64, seq: u32) {
    let (buf, n) = words(&[state as u32, (state >> 32) as u32, TICKS.load(Ordering::Relaxed)]);
    reply(kind::INPUT, seq, &buf[..n]);
}

fn config(key: u32, value: u32) -> Option<u32> {
    let c = &input::CONFIG;
    let slot = match key {
        key::SCAN_LOAD => &c.load,
        key::SCAN_RELEASE => &c.release,
        key::SCAN_LOW => &c.low,
        key::SCAN_HIGH => &c.high,
        key::SCAN_PERIOD => &c.period,
        key::DEBOUNCE => &c.debounce,
        key::CHECKS => &supervisor::CHECKS,
        key::SPEAKER => &speaker::POLICY,
        _ => return None,
    };
    let limit = match key {
        key::CHECKS => u32::MAX - 1,
        key::SPEAKER => speaker::ON,
        _ => 100_000,
    };
    if value != u32::MAX && value <= limit {
        slot.store(value, Ordering::Relaxed);
    }
    Some(slot.load(Ordering::Relaxed))
}

fn handle(msg: shm::Msg, input: &input::Input) {
    let w = |i: usize| u32::from_le_bytes(msg.data[4 * i..4 * i + 4].try_into().unwrap());
    match msg.kind {
        kind::PING => reply(kind::PONG, msg.seq, msg.payload()),
        kind::GET_INFO => {
            let (buf, n) = words(&[
                VERSION,
                shm::ABI,
                TICKS.load(Ordering::Relaxed),
                shm::read(shm::H_COUNT_PER_MS),
                RX.load(Ordering::Relaxed),
                TX.load(Ordering::Relaxed),
                hw::prid(),
            ]);
            reply(kind::INFO, msg.seq, &buf[..n]);
        }
        kind::RESTART => {
            log!("parking for a firmware reload");
            park()
        }
        kind::GET_INPUT => send_input(input.state, msg.seq),
        kind::POWER_OFF => {
            log!("powering off for Linux");
            supervisor::power_off()
        }
        kind::CONFIG => match config(w(0), if msg.len >= 8 { w(1) } else { u32::MAX }) {
            Some(v) => {
                let (buf, n) = words(&[w(0), v]);
                reply(kind::CONFIG, msg.seq, &buf[..n]);
            }
            None => reply(kind::CONFIG, msg.seq, &[]),
        },
        kind::SCAN_TEST => {
            // Pause the scanner for a blocking scan with these timings.
            SCANNING.store(false, Ordering::Relaxed);
            hw::delay_us(3000);
            let n = if msg.len >= 20 { w(4).clamp(1, 32) } else { 16 };
            let s0 = hw::sb_count();
            let raw = input::scan_bits(w(0), w(1), w(2), w(3), n);
            let us = hw::sb_count().wrapping_sub(s0) * 16 / 27;
            SCANNING.store(true, Ordering::Relaxed);
            let (buf, n) = words(&[raw, us]);
            reply(kind::SCAN_TEST, msg.seq, &buf[..n]);
        }
        kind::PEEK => {
            let (addr, n) = (w(0) as usize & !3, (w(1) as usize).clamp(1, shm::MSG_DATA / 4));
            let mut buf = [0u8; shm::MSG_DATA];
            for i in 0..n {
                let v = unsafe { core::ptr::read_volatile((addr + 4 * i) as *const u32) };
                buf[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
            }
            reply(kind::PEEK, msg.seq, &buf[..4 * n]);
        }
        kind::POKE => {
            let addr = w(0) as usize & !3;
            let v = unsafe {
                core::ptr::write_volatile(addr as *mut u32, w(1));
                core::ptr::read_volatile(addr as *const u32)
            };
            reply(kind::POKE, msg.seq, &v.to_le_bytes());
        }
        kind::CORELINK => {
            let n = (msg.len as usize / 4).min(shm::MSG_DATA / 4);
            let mut req = [0u32; shm::MSG_DATA / 4];
            for (i, r) in req.iter_mut().enumerate().take(n) {
                *r = w(i);
            }
            let (buf, len) = words(&corelink::request(&req[..n]));
            reply(kind::CORELINK, msg.seq, &buf[..len]);
        }
        other => log!("unknown message kind {other} (seq {})", msg.seq),
    }
}

/// Timer interrupt: the 1 kHz tick and the button-scan steps share one
/// compare register; run whatever is due, then arm the earliest deadline.
#[allow(static_mut_refs)]
fn timer_irq() {
    let t = unsafe { &mut TIMER };
    let base = TICK_BASE.load(Ordering::Relaxed);
    loop {
        let mut tick_due = tick_deadline(base, t.tick + 1);
        if hw::sb_due(tick_due) {
            // Skip deadlines already passed (the core was busy), keeping the grid.
            while hw::sb_due(tick_deadline(base, t.tick + 2)) {
                t.tick += 1;
            }
            t.tick += 1;
            TICKS.store(t.tick, Ordering::Relaxed);
            shm::write(shm::H_TICKS, t.tick);
            tick_due = tick_deadline(base, t.tick + 1);
        }
        if hw::sb_due(t.scan_due) {
            t.scan_due = if SCANNING.load(Ordering::Relaxed) {
                t.scanner.step(hw::sb_count())
            } else {
                hw::sb_count().wrapping_add(hw::sb_counts(1000))
            };
        }
        let next = if (t.scan_due.wrapping_sub(tick_due) as i32) < 0 { t.scan_due } else { tick_due };
        hw::timer_set(next);
        if !hw::sb_due(next) {
            break;
        }
    }
}

/// Called from the exception vector with EXL set: interrupts, or a crash.
#[no_mangle]
extern "C" fn rust_exception(cause: u32, epc: u32, badvaddr: u32) {
    let t0 = hw::count();
    handle_exception(cause, epc, badvaddr);
    IRQ_CYCLES.fetch_add(hw::count().wrapping_sub(t0), Ordering::Relaxed);
}

/// CP0 count cycles spent in exception handlers (for the idle figure).
static IRQ_CYCLES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn handle_exception(cause: u32, epc: u32, badvaddr: u32) {
    shm::write(shm::H_EXCEPTIONS, shm::read(shm::H_EXCEPTIONS).wrapping_add(1));
    shm::write(shm::H_LAST_CAUSE, cause);
    let code = (cause >> 2) & 0x1f;
    if code != 0 {
        crash(0x300, cause, epc, badvaddr);
    }
    let pending = cause & hw::status() & 0xff00;
    if pending & hw::IM_INTC != 0 && hw::timer_pending() {
        timer_irq();
    }
    if pending & hw::IM_MBOX != 0 && hw::mbox_ack() {
        DOORBELL.store(true, Ordering::Relaxed);
    }
}

fn crash(vector: u32, cause: u32, epc: u32, badvaddr: u32) -> ! {
    for (i, v) in [vector, cause, epc, badvaddr, hw::status()].into_iter().enumerate() {
        shm::write(shm::H_CRASH + 4 * i, v);
    }
    shm::write(shm::H_STATE, shm::STATE_CRASHED);
    hw::ring_linux();
    park()
}

/// Spins in uncached code in shared memory (outside the image, which Linux
/// may now overwrite) until Linux writes an entry address to H_PARK_GO, then
/// jumps there. No `wait`: the reset bit does not seem to take a core out of
/// it, so a parked core must stay awake.
pub fn park() -> ! {
    // lui t0, 0xabd0; 1: lw t1, 0x60(t0); beqz t1, 1b; nop; jr t1; nop
    const CODE: [u32; 6] = [0x3c08_abd0, 0x8d09_0060, 0x1120_fffe, 0, 0x0120_0008, 0];
    hw::disable_interrupts();
    shm::reset_locks();
    shm::write(shm::H_PARK_GO, 0);
    for (i, w) in CODE.iter().enumerate() {
        shm::write(shm::PARK_CODE + 4 * i, *w);
    }
    shm::write(shm::H_PARKED, 1);
    shm::barrier();
    unsafe {
        core::arch::asm!(".set noat", "jr {0}", "nop", ".set at", in(reg) shm::BASE + shm::PARK_CODE, options(noreturn));
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    log!("panic: {}", info.message());
    crash(0xbad, 0, info.location().map_or(0, |l| l.line()), 0)
}
