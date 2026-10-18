//! PlayStation drawing shared with the second core (PCSX-ReARMed only).
//!
//! The VRAM lives in corelink memory both cores address (`gb350_vram_map`,
//! core patch 0006). VRAM rows go in 16-row stripes: the second core's unai
//! (avp/syscore service 2, PS1GPU) draws some of them, the main core's the
//! rest, from the same commands: unai copies every command it draws into a buffer (core
//! patch 0007) and hands it over in ~512-word batches, sent as records as
//! they fill so the second core works alongside. A row always has the same
//! owner, so the two (non-coherent) data caches never hold the same line
//! dirty.
//!
//! Synchronised (the second core finishes, writes its cache back and drops
//! it; this core does the same over the VRAM, so memory holds everything and
//! neither cache anything stale) when:
//! - gpulib accesses VRAM itself (`renderer_flush_queues`): uploads, copies,
//!   reads, scan-out at the end of a frame, save states;
//! - after gpulib wrote VRAM (`renderer_update_caches`), and after a state's
//!   VRAM was loaded ([`vram_loaded`]);
//! - before a textured primitive whose texture page or CLUT overlaps VRAM
//!   drawn since the last synchronisation (render to texture): drawn
//!   rectangles against the texels the primitive's coordinates span.
//!
//! On any failure the main core goes back to drawing every row.

use gb350_corelink::{Buffer, Caching, Link, Ring};
use std::ffi::{c_int, c_uint, c_void};
use std::time::{Duration, Instant};

const SERVICE_PS1GPU: u32 = 2;
const RING_BYTES: usize = 256 * 1024;
/// The second core's largest CMD record (avp/syscore ps1gpu.rs).
const MAX_CMD_WORDS: usize = 4096;
/// Stripes of 2^4 = 16 rows, 32 to a pattern; the second core draws `share`
/// of every 32 (spread evenly), this core the rest. The second core is about
/// half as fast, and its share follows how long this core waits for it (see
/// [`Split::adapt`]). RETRO_PS1_SPLIT_SKIP=<hex>: a fixed pattern instead
/// (bit s & 31 set: stripe s is the second core's).
const SHIFT: u32 = 4;
const START_SHARE: u32 = 10;
const MIN_SHARE: u32 = 4;
const MAX_SHARE: u32 = 16;
/// The share is reconsidered every this many syncs, from the average wait:
/// a sync always waits for the last batch sent (~0.3 ms); much more means
/// the second core has more than it keeps up with (Crash Bandicoot: 0.33
/// ms at 10/32 of the rows, 1 ms at 12/32 and 4.5 ms at 14/32; 10-12 was
/// fastest).
const WINDOW: u32 = 32;
/// More texture syncs than frames over this many frames (a game drawing
/// into what it then reads, like Suikoden 2's title effect, which waits for
/// the second core several times a frame): everything back on this core
/// for PAUSE_FRAMES, then try again.
const TEX_WINDOW: u32 = 30;
const PAUSE_FRAMES: u32 = 600;
const WAIT_LESS: Duration = Duration::from_micros(900);
const WAIT_MORE: Duration = Duration::from_micros(450);

fn pattern(share: u32) -> u32 {
    (0..32).filter(|i| (i + 1) * share / 32 > i * share / 32).fold(0, |m, i| m | 1 << i)
}

fn fixed_skip() -> Option<u32> {
    std::env::var("RETRO_PS1_SPLIT_SKIP").ok().and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
}
const WAIT: Duration = Duration::from_secs(2);

// Records (avp/syscore/src/ps1gpu.rs).
const INIT: u16 = 1;
const CMD: u16 = 3;
const FRAME: u16 = 6;
const SPLIT: u16 = 8;
const FLUSH: u16 = 9;
const STATE: u16 = 10;

/// Core patches 0006 / 0007 (gpulib_if.cpp `gb350_split_ops`).
#[repr(C)]
struct Ops {
    texture: unsafe extern "C" fn(*const u32, u32, u32),
    track: unsafe extern "C" fn(*const u32, c_int),
    batch: unsafe extern "C" fn(*const u32, c_int),
    flush: extern "C" fn(),
    written: extern "C" fn(),
    state: extern "C" fn(),
}

static OPS: Ops = Ops { texture: on_texture, track: on_track, batch: on_batch, flush: on_flush, written: on_written, state: on_state };

/// What unai's hooks read before calling here (`gb350_split_shared`): the
/// drawing area still to be marked as drawn, and the bounding box of VRAM
/// drawn since the last sync (x1 < x0: nothing), for a cheap texture check.
#[repr(C)]
struct Shared {
    area_unmarked: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

extern "C" {
    static mut gb350_split: *const Ops;
    static mut gb350_split_shared: Shared;
    /// Hands over unai's buffered commands (to `on_batch`).
    fn gb350_split_drain();
    static mut gb350_vram_map: Option<unsafe extern "C" fn(c_uint) -> *mut c_void>;
    static mut gb350_vram_unmap: Option<unsafe extern "C" fn(*mut c_void, c_uint) -> c_int>;
    // C wrappers in libretro.c (core patch 0006) for the renderer's
    // renderer_set_split & co.
    fn gb350_split_set(shift: c_int, skip: c_uint);
    fn gb350_split_state(w: *mut u32) -> c_int;
    fn gb350_split_resync();
    fn gb350_split_vram() -> *mut c_void;
}

/// A VRAM rectangle, inclusive.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Rect {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

impl Rect {
    fn overlaps(&self, o: &Rect) -> bool {
        self.x0 <= o.x1 && o.x0 <= self.x1 && self.y0 <= o.y1 && o.y0 <= self.y1
    }

    /// `w` x `h` from (x, y), wrapping at VRAM's edges (1024 x 512): one or
    /// two rectangles each way.
    fn wrapped(x: u32, y: u32, w: u32, h: u32, mut f: impl FnMut(Rect)) {
        let (x, y) = (x % 1024, y % 512);
        let xs = if x + w > 1024 { [(x, 1023), (0, x + w - 1025)] } else { [(x, x + w - 1), (1, 0)] };
        let ys = if y + h > 512 { [(y, 511), (0, y + h - 513)] } else { [(y, y + h - 1), (1, 0)] };
        for &(x0, x1) in xs.iter().filter(|r| r.0 <= r.1) {
            for &(y0, y1) in ys.iter().filter(|r| r.0 <= r.1) {
                f(Rect { x0, y0, x1, y1 });
            }
        }
    }
}

const MAX_DRAWN: usize = 16;

struct Split {
    ring: Ring,
    /// Records sent since the last synchronisation.
    dirty: bool,
    /// VRAM rectangles drawn since the last synchronisation (inclusive);
    /// past MAX_DRAWN, their bounding box. Their bounding box is also in
    /// `gb350_split_shared`.
    drawn: Vec<Rect>,
    /// Drawing area, inclusive.
    area: Rect,
    syncs: u32,
    texture_syncs: u32,
    waited: Duration,
    /// The second core's stripes of 32 (adapted unless `fixed`); syncs and
    /// their waits in the current window.
    share: u32,
    skip: u32,
    fixed: bool,
    window: (u32, Duration),
    /// Frames left with everything on this core (0: sharing); frames and
    /// texture syncs in the current TEX_WINDOW; pauses since the report.
    paused: u32,
    tex_window: (u32, u32),
    pauses: u32,
    /// Measuring: sending batches, cache maintenance here; batches sent.
    t_send: Duration,
    t_cache: Duration,
    batches: u32,
}

// The emulator runs on one thread; these are only touched from it (the
// hooks run inside retro_run / retro_unserialize).
static mut VRAM: Option<Buffer> = None;
static mut SPLIT_STATE: Option<Split> = None;

#[allow(static_mut_refs)]
fn split() -> Option<&'static mut Split> {
    unsafe { SPLIT_STATE.as_mut() }
}

#[allow(static_mut_refs)]
fn vram() -> Option<&'static Buffer> {
    unsafe { VRAM.as_ref() }
}

/// Before retro_init: VRAM from corelink memory (costs nothing measurable),
/// so the drawing can be shared from the start or later ([`set_enabled`]).
pub fn install() {
    unsafe {
        gb350_vram_map = Some(vram_map);
        gb350_vram_unmap = Some(vram_unmap);
    }
}

/// Whether the drawing can be shared (the VRAM is corelink memory).
pub fn available() -> bool {
    vram().is_some()
}

/// Between frames (the in-game menu): share the drawing, or draw it all here.
pub fn set_enabled(on: bool) {
    match (on, split().is_some()) {
        (true, false) => start(),
        (false, true) => stop("switched off in the menu"),
        _ => {}
    }
}

unsafe extern "C" fn vram_map(size: c_uint) -> *mut c_void {
    let buf = match Link::open().and_then(|l| l.alloc(size as usize, Caching::Cached)) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("retro: second core drawing off: no corelink memory ({e})");
            return std::ptr::null_mut();
        }
    };
    let p = buf.as_ptr() as *mut c_void;
    unsafe { VRAM = Some(buf) };
    p
}

unsafe extern "C" fn vram_unmap(p: *mut c_void, _size: c_uint) -> c_int {
    if vram().is_none_or(|b| b.as_ptr() as *mut c_void != p) {
        return 0;
    }
    stop("the game ended");
    unsafe { VRAM = None };
    1
}

/// After retro_load_game: starts sharing the drawing if the VRAM can be shared.
pub fn start() {
    let Some(buf) = vram() else { return };
    let mut state = [0u32; 4];
    let n = unsafe { gb350_split_state(state.as_mut_ptr()) };
    if n < 0 {
        eprintln!("retro: second core drawing off: the old unai renderer is selected");
        return;
    }
    let off = unsafe { gb350_split_vram() } as usize - buf.as_ptr() as usize;
    if off >= buf.len {
        return;
    }
    let mut ring = match Ring::new(SERVICE_PS1GPU, RING_BYTES) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("retro: second core drawing off: no PS1 GPU service there ({e})");
            return;
        }
    };
    // Memory holds this core's VRAM; nothing of it stays cached here.
    let _ = buf.sync_both(0, buf.len);
    let phys = buf.phys + off as u32;
    let skip = fixed_skip().unwrap_or(pattern(START_SHARE));
    let words = |w: &[u32]| w.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
    let ok = ring.push(INIT, &words(&[phys])).is_ok()
        && ring.push(SPLIT, &words(&[SHIFT, !skip])).is_ok()
        && ring.push(STATE, &words(&state[..n as usize])).is_ok();
    if !ok {
        return;
    }
    ring.flush();
    unsafe {
        SPLIT_STATE = Some(Split {
            ring,
            dirty: true,
            drawn: Vec::with_capacity(MAX_DRAWN),
            area: Rect { x0: 0, y0: 0, x1: 0, y1: 0 },
            syncs: 0,
            texture_syncs: 0,
            waited: Duration::ZERO,
            share: skip.count_ones(),
            skip,
            paused: 0,
            tex_window: (0, 0),
            pauses: 0,
            fixed: fixed_skip().is_some(),
            window: (0, Duration::ZERO),
            t_send: Duration::ZERO,
            t_cache: Duration::ZERO,
            batches: 0,
        });
        clear_drawn();
        // Drop anything unai still holds from an earlier session (the
        // hooks are off, so this only empties its buffer).
        gb350_split_drain();
        gb350_split = &OPS;
        // The drawing registers, through the hooks (to the second core).
        gb350_split_resync();
        gb350_split_set(SHIFT as c_int, skip);
    }
    eprintln!("retro: drawing shared with the second core (16-row stripes, {}/32 there{})", skip.count_ones(), if fixed_skip().is_some() { ", fixed" } else { "" });
}

/// Back to drawing everything here (after synchronising).
fn stop(why: &str) {
    if split().is_none() {
        return;
    }
    sync();
    unsafe {
        gb350_split = std::ptr::null();
        gb350_split_set(0, 0);
        SPLIT_STATE = None; // detaches the ring
    }
    eprintln!("retro: second core drawing stopped: {why}");
}

impl Split {
    /// After a sync (nothing cached on either side, so rows can change
    /// owner): the second core's share from how long this core waited.
    fn adapt(&mut self, waited: Duration) {
        if self.fixed {
            return;
        }
        self.window.0 += 1;
        self.window.1 += waited;
        if self.window.0 < WINDOW {
            return;
        }
        let avg = self.window.1 / self.window.0;
        self.window = (0, Duration::ZERO);
        let old = self.share;
        if avg > WAIT_LESS {
            self.share = (self.share - 1).max(MIN_SHARE);
        } else if avg < WAIT_MORE {
            self.share = (self.share + 1).min(MAX_SHARE);
        }
        if self.share != old {
            self.skip = pattern(self.share);
            let ok = self.ring.push_words(SPLIT, &[SHIFT, !self.skip]).is_ok();
            unsafe { gb350_split_set(SHIFT as c_int, if ok { self.skip } else { 0 }) };
            if !ok {
                self.fixed = true;
            }
        }
    }

    fn mark(&mut self, x: u32, y: u32, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        let drawn = &mut self.drawn;
        let sh = unsafe { &mut *std::ptr::addr_of_mut!(gb350_split_shared) };
        Rect::wrapped(x, y, w, h, |r| {
            if drawn.contains(&r) {
                return;
            }
            if drawn.len() == MAX_DRAWN {
                let b = drawn.iter().fold(r, |b, d| Rect { x0: b.x0.min(d.x0), y0: b.y0.min(d.y0), x1: b.x1.max(d.x1), y1: b.y1.max(d.y1) });
                drawn.clear();
                drawn.push(b);
            } else {
                drawn.push(r);
            }
            if sh.x1 < sh.x0 {
                (sh.x0, sh.y0, sh.x1, sh.y1) = (r.x0 as i32, r.y0 as i32, r.x1 as i32, r.y1 as i32);
            } else {
                (sh.x0, sh.y0) = (sh.x0.min(r.x0 as i32), sh.y0.min(r.y0 as i32));
                (sh.x1, sh.y1) = (sh.x1.max(r.x1 as i32), sh.y1.max(r.y1 as i32));
            }
        });
    }

    fn marked(&self, x: u32, y: u32, w: u32, h: u32) -> bool {
        let mut hit = false;
        Rect::wrapped(x, y, w, h, |r| hit |= self.drawn.iter().any(|d| d.overlaps(&r)));
        hit
    }

    /// Whether a textured primitive reads VRAM drawn since the last sync:
    /// the texels its coordinates span (a texel more each way for the
    /// rasteriser's rounding; the whole page under a texture window or
    /// flipped sprites) and its CLUT.
    fn texture_hazard(&self, w: &[u32], e1: u32, e2: u32) -> bool {
        if self.drawn.is_empty() {
            return false;
        }
        let c = w[0] >> 24;
        let poly = c & 0xe0 == 0x20;
        let clut = w[2] >> 16;
        let tpage = if poly { w[if c & 0x10 != 0 { 5 } else { 4 }] >> 16 } else { e1 };
        let depth = (tpage >> 7) & 3;
        let (tx, ty) = ((tpage & 0xf) * 64, ((tpage >> 4) & 1) * 256);
        // Texture coordinates: (u, v) in the low half of these words.
        let uv_at: &[usize] = match (poly, c & 0x18) {
            (true, 0x00) => &[2, 4, 6],
            (true, 0x08) => &[2, 4, 6, 8],
            (true, 0x10) => &[2, 5, 8],
            (true, _) => &[2, 5, 8, 11],
            (false, _) => &[2],
        };
        let (mut u0, mut u1, mut v0, mut v1) = (255u32, 0u32, 255u32, 0u32);
        for &i in uv_at {
            let (u, v) = (w[i] & 0xff, (w[i] >> 8) & 0xff);
            (u0, u1, v0, v1) = (u0.min(u), u1.max(u), v0.min(v), v1.max(v));
        }
        let flipped = !poly && e1 & 0x3000 != 0;
        if !poly {
            let (sw, sh) = match c & 0x18 {
                0x00 => (w[3] & 0x3ff, (w[3] >> 16) & 0x1ff),
                0x08 => (1, 1),
                0x10 => (8, 8),
                _ => (16, 16),
            };
            (u1, v1) = (u0 + sw.max(1) - 1, v0 + sh.max(1) - 1);
        }
        if e2 & 0xfffff != 0 || flipped || u1 > 255 || v1 > 255 {
            (u0, u1, v0, v1) = (0, 255, 0, 255);
        } else {
            (u0, u1, v0, v1) = (u0.saturating_sub(1), (u1 + 1).min(255), v0.saturating_sub(1), (v1 + 1).min(255));
        }
        // Texels to VRAM pixels: 4 or 2 per pixel at 4 / 8 bits.
        let shift = [2, 1, 0, 0][depth as usize];
        if self.marked(tx + (u0 >> shift), ty + v0, (u1 >> shift) - (u0 >> shift) + 1, v1 - v0 + 1) {
            return true;
        }
        depth < 2 && self.marked((clut & 0x3f) * 16, (clut >> 6) & 0x1ff, if depth == 0 { 16 } else { 256 }, 1)
    }

    /// Tracks what a command draws (unai calls this for fills, drawing-area
    /// changes, and the first drawing command after either or a sync).
    fn track(&mut self, w: &[u32]) {
        let sh = unsafe { &mut *std::ptr::addr_of_mut!(gb350_split_shared) };
        match w[0] >> 24 {
            0x02 if w.len() >= 3 => {
                let (x, y) = (w[1] & 0x3f0, (w[1] >> 16) & 0x1ff);
                let (fw, fh) = (((w[2] & 0x3ff) + 15) & !15, (w[2] >> 16) & 0x1ff);
                self.mark(x, y, fw, fh);
            }
            0x20..=0x7f => {
                let a = self.area;
                if a.x1 >= a.x0 && a.y1 >= a.y0 {
                    self.mark(a.x0, a.y0, a.x1 - a.x0 + 1, a.y1 - a.y0 + 1);
                }
                sh.area_unmarked = 0;
            }
            0xe3 => {
                (self.area.x0, self.area.y0) = (w[0] & 0x3ff, ((w[0] >> 10) & 0x3ff).min(511));
                sh.area_unmarked = 1;
            }
            0xe4 => {
                (self.area.x1, self.area.y1) = (w[0] & 0x3ff, ((w[0] >> 10) & 0x3ff).min(511));
                sh.area_unmarked = 1;
            }
            _ => {}
        }
    }
}

/// Nothing drawn since the last sync (and the drawing area to be marked by
/// the next drawing command).
fn clear_drawn() {
    if let Some(s) = split() {
        s.drawn.clear();
    }
    unsafe {
        gb350_split_shared = Shared { area_unmarked: 1, x0: 1, y0: 1, x1: 0, y1: 0 };
    }
}

/// unai's buffered commands to the ring (through `on_batch`).
fn drain() {
    if split().is_some() {
        unsafe { gb350_split_drain() };
    }
}

/// Waits for the second core (its cache written back and dropped), then
/// writes back and drops this core's VRAM lines.
fn sync() {
    drain();
    let Some(s) = split() else { return };
    if !s.dirty {
        return;
    }
    let mut waited = Duration::ZERO;
    let ok = match s.ring.push(FLUSH, &[]) {
            Ok(f) => {
                // Whether the second core still had work beyond the flush.
                let t = Instant::now();
                let busy = s.ring.completed().wrapping_add(1) != f;
                let ok = s.ring.wait(f, WAIT).is_ok();
                if busy {
                    waited = t.elapsed();
                }
                ok
            }
            Err(_) => false,
        };
    s.waited += waited;
    s.syncs += 1;
    s.dirty = false;
    clear_drawn();
    let t = Instant::now();
    if let Some(v) = vram() {
        let _ = v.sync_both(0, v.len);
    }
    s.t_cache += t.elapsed();
    if ok {
        s.adapt(waited);
    }
    if !ok || s.ring.error() != 0 {
        let e = s.ring.error();
        unsafe {
            gb350_split = std::ptr::null();
            gb350_split_set(0, 0);
            SPLIT_STATE = None;
        }
        eprintln!("retro: second core drawing stopped: it did not answer (error {e:#x})");
    }
}

unsafe extern "C" fn on_texture(cmd: *const u32, e1: u32, e2: u32) {
    let Some(s) = split() else { return };
    // The whole primitive (gpulib_if.cpp checked it is all there): 3 to 12
    // words.
    let c = unsafe { *cmd } >> 24;
    let n = match c & 0xfc {
        0x24 => 7,
        0x2c | 0x34 => 9,
        0x3c => 12,
        0x64 => 4,
        _ => 3,
    };
    let w = unsafe { std::slice::from_raw_parts(cmd, n) };
    if s.texture_hazard(w, e1, e2) {
        s.texture_syncs += 1;
        s.tex_window.1 += 1;
        static mut LOGGED: u32 = 0;
        if std::env::var_os("RETRO_PS1_SPLIT_LOG").is_some() && unsafe { LOGGED } < 40 {
            unsafe { LOGGED += 1 };
            let d: Vec<String> = s.drawn.iter().map(|r| format!("({},{})-({},{})", r.x0, r.y0, r.x1, r.y1)).collect();
            eprintln!("retro: texture sync: {w:08x?} e1 {e1:08x} e2 {e2:08x}; drawn {}", d.join(" "));
        }
        sync();
    }
}

unsafe extern "C" fn on_track(cmd: *const u32, words: c_int) {
    let Some(s) = split() else { return };
    s.track(unsafe { std::slice::from_raw_parts(cmd, words as usize) });
}

unsafe extern "C" fn on_batch(words: *const u32, n: c_int) {
    let Some(s) = split() else { return };
    let w = unsafe { std::slice::from_raw_parts(words, n as usize) };
    if w.len() > MAX_CMD_WORDS {
        stop("a command too long for the second core");
        return;
    }
    let t = Instant::now();
    let ok = s.ring.push_words(CMD, w).is_ok();
    s.ring.flush();
    s.t_send += t.elapsed();
    s.batches += 1;
    s.dirty = true;
    if !ok {
        stop("ring error");
    }
}

extern "C" fn on_flush() {
    sync();
}

extern "C" fn on_written() {
    sync();
    if let Some(v) = vram() {
        let _ = v.sync_both(0, v.len);
    }
}

extern "C" fn on_state() {
    let Some(s) = split() else { return };
    let mut w = [0u32; 4];
    let n = unsafe { gb350_split_state(w.as_mut_ptr()) };
    if n < 0 {
        stop("the old unai renderer was selected");
        return;
    }
    drain();
    let Some(s) = split() else { return };
    if s.ring.push_words(STATE, &w[..n as usize]).is_err() {
        stop("ring error");
        return;
    }
    s.ring.flush();
}

/// After retro_unserialize: the state's VRAM was written here.
pub fn vram_loaded() {
    if split().is_some_and(|s| s.paused == 0) {
        on_written();
    }
}

/// After each retro_run (the second core counts its frames' drawing time).
pub fn frame() {
    let Some(s) = split() else { return };
    if s.paused > 0 {
        s.paused -= 1;
        if s.paused == 0 {
            resume();
        }
        return;
    }
    drain();
    let Some(s) = split() else { return };
    if s.ring.push(FRAME, &[]).is_err() {
        stop("ring error");
        return;
    }
    s.ring.flush();
    s.tex_window.0 += 1;
    if s.tex_window.0 >= TEX_WINDOW {
        let busy = s.tex_window.1 > TEX_WINDOW;
        s.tex_window = (0, 0);
        if busy {
            pause();
        }
    }
}

/// Everything on this core for PAUSE_FRAMES (unai's hooks off: nothing
/// copied, sent or synced).
fn pause() {
    sync();
    let Some(s) = split() else { return };
    unsafe {
        gb350_split = std::ptr::null();
        gb350_split_set(0, 0);
    }
    s.paused = PAUSE_FRAMES;
    s.pauses += 1;
}

/// Sharing again: memory holds this core's VRAM; the second core gets the
/// pattern, the options and the drawing registers again.
fn resume() {
    let Some(s) = split() else { return };
    if let Some(v) = vram() {
        let _ = v.sync_both(0, v.len);
    }
    let mut st = [0u32; 4];
    let n = unsafe { gb350_split_state(st.as_mut_ptr()) };
    if n < 0 || s.ring.push_words(SPLIT, &[SHIFT, !s.skip]).is_err() || s.ring.push_words(STATE, &st[..n as usize]).is_err() {
        stop("ring error");
        return;
    }
    s.dirty = true;
    s.window = (0, Duration::ZERO);
    clear_drawn();
    unsafe {
        gb350_split = &OPS;
        gb350_split_resync();
        gb350_split_set(SHIFT as c_int, s.skip);
    }
    s.ring.flush();
}

/// Since the last report: the second core's drawing per frame, and the
/// synchronisations.
pub fn report() -> Option<String> {
    let s = split()?;
    let (n, cycles, worst) = (s.ring.result(0), s.ring.result(4), s.ring.result(8));
    // Results are cumulative since INIT: report differences.
    static mut LAST: (u32, u32) = (0, 0);
    let (ln, lc) = unsafe { LAST };
    unsafe { LAST = (n, cycles) };
    let frames = n.wrapping_sub(ln).max(1);
    const COUNT_HZ: f64 = 297_000_000.0;
    let r = format!(
        "retro: second core drawing {:.1} ms per frame (worst {:.1}), {}/32 rows{}; {} syncs ({} for textures), waited {:.2} ms per frame; {} batches sending {:.2} ms per frame, cache {:.2} ms per frame",
        cycles.wrapping_sub(lc) as f64 / frames as f64 / COUNT_HZ * 1000.0,
        worst as f64 / COUNT_HZ * 1000.0,
        s.share,
        if s.paused > 0 || s.pauses > 0 { format!(" ({} pauses for render to texture{})", s.pauses, if s.paused > 0 { ", paused" } else { "" }) } else { String::new() },
        s.syncs,
        s.texture_syncs,
        s.waited.as_secs_f64() * 1000.0 / frames as f64,
        s.batches,
        s.t_send.as_secs_f64() * 1000.0 / frames as f64,
        s.t_cache.as_secs_f64() * 1000.0 / frames as f64
    );
    s.batches = 0;
    s.pauses = 0;
    s.t_send = Duration::ZERO;
    s.t_cache = Duration::ZERO;
    s.syncs = 0;
    s.texture_syncs = 0;
    s.waited = Duration::ZERO;
    Some(r)
}
