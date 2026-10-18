//! retro: a minimal libretro frontend for the GB350.
//!
//!   retro <game> [secs]
//!
//! The core (PCSX-ReARMed for now) is linked in statically. Video goes
//! straight to KMS in the core's own pixel format (0RGB1555, RGB565 and
//! XRGB8888 are all native formats of the display driver) and the display
//! hardware scales it to the panel at the core's aspect ratio; audio goes to
//! ALSA, whose blocking writes pace emulation; input comes from evdev (the
//! gb350-buttons device). Hold Select + Start for the menu (resume, save /
//! load state, reset, volume, brightness, scaling, quit), drawn on the
//! overlay plane; the volume keys show a slider.
//!
//! Needs the display: `gbctl screen release` first while the agent holds it.
//!
//! Environment: RETRO_OPTS="key=value,..." core options, RETRO_FILTER=nearest
//! (pixel-exact scaling), RETRO_SCALE=aspect|integer|stretch (how the
//! picture fills the panel), RETRO_VOLUME=0..100 (percent, default 10; 0
//! mutes and switches the speaker off; the volume keys change it while
//! playing; RETRO_GAIN=0..255, the hardware gain, also works).



mod content;
mod libretro;
mod menu;
#[cfg(core_pcsx)]
mod ps1split;
mod resample;

use gb350_kms::{Card, Dumb, SetPlane, OBJ_PLANE, RGB565, ROTATE_0, ROTATE_180, ROTATE_270, ROTATE_90, XBGR1555, XRGB1555, XRGB8888};
use gb350_ui::input::Input;
use gb350_pcm::Pcm;
use libretro as lr;
use std::cell::RefCell;
use std::ffi::{c_char, c_uint, c_void, CStr, CString};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

/// The panel as shown, without the overlay: the frame scaled into place
/// (nearest), black around it. For test screenshots.
fn screen_rgb(v: &mut Video) -> Vec<u32> {
    let mut out = vec![0u32; 640 * 480];
    let (w, h) = v.geometry;
    if w == 0 {
        return out;
    }
    let (dw, dh) = placement(w, h, shape(w, h, v.aspect, v.rot & 1 == 1), v.scale);
    let (x0, y0) = ((640 - dw) / 2, (480 - dh) / 2);
    let (format, pitch) = (v.format, v.bufs[v.front].pitch as usize);
    let px = v.bufs[v.front].pixels();
    let c5 = |c: u32| (c & 31) << 3 | (c & 31) >> 2;
    for y in 0..dh {
        let sy = (y * h / dh) as usize;
        for x in 0..dw {
            let sx = (x * w / dw) as usize;
            let rgb = if format == XRGB8888 {
                let o = sy * pitch + sx * 4;
                u32::from_le_bytes([px[o], px[o + 1], px[o + 2], 0])
            } else {
                let o = sy * pitch + sx * 2;
                let p = u16::from_le_bytes([px[o], px[o + 1]]) as u32;
                match format {
                    f if f == XBGR1555 => c5(p) << 16 | c5(p >> 5) << 8 | c5(p >> 10),
                    f if f == XRGB1555 => c5(p >> 10) << 16 | c5(p >> 5) << 8 | c5(p),
                    _ => c5(p >> 11) << 16 | ((p >> 5 & 63) << 2) << 8 | c5(p),
                }
            };
            out[((y0 + y) * 640 + x0 + x) as usize] = rgb;
        }
    }
    out
}

fn write_ppm(path: &str, px: &[u32]) {
    let mut data = b"P6\n640 480\n255\n".to_vec();
    for p in px {
        data.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
    }
    match std::fs::write(path, data) {
        Ok(()) => eprintln!("retro: screen {path}"),
        Err(e) => eprintln!("retro: {path}: {e}"),
    }
}

/// The core linked into this binary (scripts/build-emulators.sh builds one
/// binary per core).
const CORE: &str = env!("RETRO_CORE_NAME");

/// Default core options for speed on this CPU; RETRO_OPTS overrides them.
fn core_options() -> &'static [(&'static str, &'static str)] {
    match CORE {
        "pcsx_rearmed" => &[
        ("pcsx_rearmed_drc", "enabled"),
        // Read the disc ahead in a thread: SD card latency stalled frames.
        ("pcsx_rearmed_cd_readahead", "256"),
        // Skip rendering early (audio buffer under ~half full) rather than
        // let the sound gap: measured on Crash 1 in-level, 55-60 fps
        // emulated with no underruns (auto mode reacted too late).
        ("pcsx_rearmed_frameskip_type", "auto_threshold"),
        ("pcsx_rearmed_frameskip_threshold", "51"),
        ("pcsx_rearmed_frameskip_interval", "3"),
        // Speed settings in the spirit of the stock firmware's pcsx4all
        // config (reverb/interpolation off, slower emulated CPU, fast
        // lighting, no dithering). unai's new renderer: as fast as the old
        // one here (Crash 1 in-level benchmark) and more accurate; the old
        // one ignores fast lighting and dithering.
        ("pcsx_rearmed_psxclock", "50"),
        ("pcsx_rearmed_spu_reverb", "disabled"),
        ("pcsx_rearmed_spu_interpolation", "off"),
        ("pcsx_rearmed_gpu_unai_fast_lighting", "enabled"),
        ("pcsx_rearmed_dithering", "disabled"),
        ],
        _ => &[],
    }
}

/// Options the game itself calls for. WonderSwan cartridges say in their
/// footer (flags byte, 4 from the end, bit 0) whether they're played
/// upright; the core leaves rotation to the Select button unless told.
fn header_options(c: &content::Content) -> Option<(&'static str, &'static str)> {
    let rom = c.data.as_deref()?;
    match CORE {
        "mednafen_wswan" if rom.len() >= 10 && rom[rom.len() - 4] & 1 != 0 => Some(("wswan_rotate_display", "portrait")),
        _ => None,
    }
}

/// Hold Select + Start this long for the menu.
const MENU_HOLD_MS: u64 = 600;

static QUIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

const SYSTEM_DIR: &str = "/mnt/sdcard/gb350/retro/system";
const SAVE_DIR: &str = "/mnt/sdcard/gb350/retro/saves";
/// Frame buffers big enough for any PS1 mode (and most other systems).
const MAX_W: u32 = 1024;
const MAX_H: u32 = 512;

struct Video {
    card: Card,
    bufs: Vec<Dumb>,
    format: u32,
    bpp: u32,
    /// Three buffers: on screen (`front`), flipped to at the next vsync
    /// (`queued`), drawn into (`back`). A finished frame that can't be
    /// queued yet waits in `back` (`ready`) instead of being dropped; only a
    /// newer frame replaces it.
    front: usize,
    queued: Option<usize>,
    back: usize,
    ready: bool,
    geometry: (u32, u32),
    aspect: f32,
    shown: u32,
    dropped: u32,
    /// Time spent in video_refresh (copy + flip), for the report.
    time: Duration,
    scale: Scale,
    /// Nearest-neighbour scaling (RETRO_FILTER=nearest).
    sharp: bool,
    /// Menu and volume slider (the overlay plane), if available.
    overlay: Option<menu::Overlay>,
    /// Frames put on screen since the start (the stats overlay).
    drawn: u64,
    /// Quarter turns counter-clockwise applied to each frame.
    rot: u32,
    /// The panel's refresh rate (from the mode timings).
    hz: f64,
    /// Completed flips: count, first and last timestamp (the measured rate).
    flips: (u32, f64, f64),
    /// Turned games: the core's frame, turned by the 2D engine into `back`.
    stage: Option<Dumb>,
    /// The 2D engine works (off after a failure: the CPU turns frames).
    ge_ok: bool,
}

/// How the picture fills the panel (RETRO_SCALE).
#[derive(Clone, Copy, PartialEq)]
enum Scale {
    Aspect,
    Integer,
    Stretch,
}

impl Scale {
    fn from_env() -> Scale {
        match std::env::var("RETRO_SCALE").as_deref() {
            Ok("integer") => Scale::Integer,
            Ok("stretch") => Scale::Stretch,
            _ => Scale::Aspect,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Scale::Aspect => "aspect",
            Scale::Integer => "integer",
            Scale::Stretch => "stretch",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Scale::Aspect => "Keep aspect",
            Scale::Integer => "Integer",
            Scale::Stretch => "Stretch",
        }
    }
}

impl Video {
    /// The buffer neither on screen nor queued (other than `back`).
    fn spare(&self) -> usize {
        (0..self.bufs.len()).find(|&i| i != self.front && Some(i) != self.queued && i != self.back).unwrap_or(self.back)
    }

    fn flipped(&mut self, at: f64) {
        let f = &mut self.flips;
        if f.0 == 0 {
            f.1 = at;
        }
        f.0 += 1;
        f.2 = at;
    }

    /// Refreshes per second measured from the flips since the last call.
    fn measured_hz(&mut self) -> Option<f64> {
        let (n, first, last) = std::mem::take(&mut self.flips);
        // Only back-to-back flips (display pacing) give the refresh rate.
        (n > 60 && last > first).then(|| (n - 1) as f64 / (last - first))
    }

    /// Notes a completed flip and queues a waiting frame.
    fn service(&mut self) {
        if let Some(q) = self.queued {
            if let Ok(Some(at)) = self.card.poll_flip(0) {
                self.flipped(at);
                self.front = q;
                self.queued = None;
            }
        }
        if self.queued.is_none() && self.ready {
            if self.card.page_flip(&self.bufs[self.back]).is_ok() {
                self.queued = Some(self.back);
                self.back = self.spare();
                self.shown += 1;
                self.drawn += 1;
            }
            self.ready = false;
        }
    }

    /// With the display pacing the emulator: once a finished frame waits
    /// behind the queued one, waits for the vsync that takes the queued one
    /// (so the next frame has a buffer and none is dropped).
    fn make_room(&mut self) {
        if self.ready {
            if let Some(q) = self.queued {
                // No event within ~3 refreshes: taken as done (not lost forever).
                if let Ok(Some(at)) = self.card.poll_flip(50) {
                    self.flipped(at);
                }
                self.front = q;
                self.queued = None;
            }
            self.service();
        }
    }

    /// Starts a new frame in `back`: a finished one still waiting there is
    /// dropped.
    fn begin_frame(&mut self) {
        self.service();
        if self.ready {
            self.ready = false;
            self.dropped += 1;
        }
    }

    /// Places the frame on screen again (after a scale or filter change).
    fn replace(&mut self) {
        let filter = if self.sharp { "Nearest Neighbor" } else { "Default" };
        let _ = self.card.set_enum(self.card.planes[0], OBJ_PLANE, "SCALING_FILTER", filter);
        let (w, h) = self.geometry;
        if w == 0 {
            return;
        }
        let (dw, dh) = placement(w, h, shape(w, h, self.aspect, self.rot & 1 == 1), self.scale);
        let mut sp = SetPlane {
            plane_id: self.card.planes[0],
            crtc_id: self.card.crtc,
            fb_id: self.bufs[self.front].id,
            crtc_x: ((640 - dw) / 2) as i32,
            crtc_y: ((480 - dh) / 2) as i32,
            crtc_w: dw,
            crtc_h: dh,
            src_w: w << 16,
            src_h: h << 16,
            ..Default::default()
        };
        if let Err(e) = self.card.set_plane(&mut sp) {
            eprintln!("retro: setplane: {e}");
        }
    }
}

struct State {
    video: Option<Video>,
    pixel_format: c_uint,
    audio: Vec<i16>,
    buttons: u16,
    inputs: Vec<std::fs::File>,
    options: Vec<(CString, CString)>,
    system_dir: CString,
    save_dir: CString,
    /// The core's audio buffer status callback (drives its auto frameskip).
    audio_status: Option<lr::AudioBufferStatusFn>,
    /// The core sends PlayStation-native BGR555 as "RGB565" (cores/patches).
    native_bgr: bool,
    /// Buttons held by RETRO_SCRIPT (OR'ed with the real ones).
    scripted: u16,
    /// Volume, percent.
    volume: i32,
    /// The volume keys were used (show the slider).
    volume_changed: bool,
    /// Rotation the core asks for (vertical games): quarter turns counter-
    /// clockwise to show the picture upright.
    rotation: u32,
    /// "Turn the console" (settings `vertical=sideways`): the picture stays
    /// as it comes and the D-pad turns with it.
    sideways: bool,
}

thread_local! {
    static ST: RefCell<State> = RefCell::new(State {
        video: None,
        pixel_format: lr::PIXEL_FORMAT_0RGB1555,
        audio: Vec::with_capacity(4096),
        buttons: 0,
        inputs: Vec::new(),
        options: Vec::new(),
        system_dir: CString::new(SYSTEM_DIR).unwrap(),
        save_dir: CString::new(SAVE_DIR).unwrap(),
        audio_status: None,
        native_bgr: false,
        scripted: 0,
        volume: 10,
        volume_changed: false,
        rotation: 0,
        sideways: false,
    });
}

// ---- callbacks ----

extern "C" {
    fn vsnprintf(buf: *mut c_char, n: usize, fmt: *const c_char, ap: std::ffi::VaList) -> i32;
}

unsafe extern "C" fn log_cb(level: c_uint, fmt: *const c_char, args: ...) {
    if level == lr::LOG_DEBUG {
        return;
    }
    let mut buf = [0u8; 512];
    vsnprintf(buf.as_mut_ptr() as *mut c_char, buf.len(), fmt, args);
    let s = CStr::from_bytes_until_nul(&buf).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    eprint!("core: {s}");
}

unsafe extern "C" fn environment(cmd: c_uint, data: *mut c_void) -> bool {
    ST.with(|st| {
        let mut st = st.borrow_mut();
        match cmd {
            lr::ENV_SET_ROTATION => {
                st.rotation = *(data as *const c_uint) & 3;
                let rot = if st.sideways { 0 } else { st.rotation };
                if let Some(v) = st.video.as_mut() {
                    v.rot = rot;
                    v.geometry = (0, 0);
                }
                eprintln!("retro: rotation {} ({})", st.rotation * 90, if st.sideways { "turn the console" } else { "upright" });
                true
            }
            lr::ENV_GET_CAN_DUPE => {
                *(data as *mut bool) = true;
                true
            }
            lr::ENV_GET_SYSTEM_DIRECTORY => {
                *(data as *mut *const c_char) = st.system_dir.as_ptr();
                true
            }
            lr::ENV_GET_SAVE_DIRECTORY => {
                *(data as *mut *const c_char) = st.save_dir.as_ptr();
                true
            }
            lr::ENV_SET_PIXEL_FORMAT => {
                let f = *(data as *const c_uint);
                if f > lr::PIXEL_FORMAT_RGB565 {
                    return false;
                }
                st.pixel_format = f;
                true
            }
            lr::ENV_GET_VARIABLE => {
                let var = &mut *(data as *mut lr::Variable);
                let key = CStr::from_ptr(var.key);
                match st.options.iter().find(|(k, _)| k.as_c_str() == key) {
                    Some((_, v)) => {
                        var.value = v.as_ptr();
                        true
                    }
                    None => false,
                }
            }
            lr::ENV_GET_VARIABLE_UPDATE => {
                *(data as *mut bool) = false;
                true
            }
            lr::ENV_GET_LOG_INTERFACE => {
                (*(data as *mut lr::LogCallback)).log = log_cb;
                true
            }
            lr::ENV_GET_CORE_OPTIONS_VERSION => {
                *(data as *mut c_uint) = 0; // legacy SET_VARIABLES
                true
            }
            lr::ENV_GET_INPUT_BITMASKS => true,
            lr::ENV_GET_AUDIO_VIDEO_ENABLE => {
                *(data as *mut i32) = 3;
                true
            }
            lr::ENV_SET_GEOMETRY | lr::ENV_SET_SYSTEM_AV_INFO => {
                if let Some(v) = st.video.as_mut() {
                    let g = if cmd == lr::ENV_SET_GEOMETRY {
                        *(data as *const lr::GameGeometry)
                    } else {
                        (*(data as *const lr::SystemAvInfo)).geometry
                    };
                    v.aspect = aspect(&g);
                    v.geometry = (0, 0); // re-place the plane on the next frame
                }
                true
            }
            lr::ENV_SET_PERFORMANCE_LEVEL | lr::ENV_SET_INPUT_DESCRIPTORS | lr::ENV_SET_VARIABLES => true,
            lr::ENV_SET_MESSAGE => true,
            lr::ENV_SET_AUDIO_BUFFER_STATUS_CALLBACK => {
                st.audio_status = if data.is_null() { None } else { (*(data as *const lr::AudioBufferStatusCallback)).callback };
                true
            }
            lr::ENV_SET_MINIMUM_AUDIO_LATENCY => true,
            lr::ENV_GB350_NATIVE_BGR555 => {
                *(data as *mut bool) = true;
                st.native_bgr = true;
                true
            }
            lr::ENV_GET_CURRENT_SOFTWARE_FRAMEBUFFER => {
                // Let the core draw straight into the next KMS buffer.
                let Some(v) = st.video.as_mut() else { return false };
                let turned = v.rot != 0;
                if turned && !(v.ge_ok && v.stage.is_some()) {
                    return false; // the CPU turns frames as it copies them
                }
                v.begin_frame();
                let fb = &mut *(data as *mut lr::Framebuffer);
                if fb.width > MAX_W || fb.height > MAX_H || v.bpp != 16 {
                    return false;
                }
                let buf = if turned { v.stage.as_mut().unwrap() } else { &mut v.bufs[v.back] };
                fb.pitch = buf.pitch as usize;
                fb.data = buf.pixels().as_mut_ptr() as *mut c_void;
                fb.format = st.pixel_format;
                fb.memory_flags = lr::MEMORY_TYPE_CACHED;
                true
            }
            _ => false,
        }
    })
}

/// The aspect ratio oriented like the `w` x `h` frame when the frontend turned
/// the picture (`turned`: a quarter turn): cores differ on whether a turned
/// picture's ratio is given before or after the turn (WonderSwan and arcade:
/// after, e.g. 9:14 upright). Unturned pictures keep the core's ratio: a
/// frame narrower than tall in pixels is often 4:3 on screen (PlayStation
/// 320 x 480, Atari 2600 160 x 250).
fn shape(w: u32, h: u32, aspect: f32, turned: bool) -> f32 {
    if !turned || (w < h) == (aspect < 1.0) { aspect } else { 1.0 / aspect }
}

fn aspect(g: &lr::GameGeometry) -> f32 {
    if g.aspect_ratio > 0.0 {
        g.aspect_ratio
    } else {
        g.base_width as f32 / g.base_height.max(1) as f32
    }
}

unsafe extern "C" fn video_refresh(data: *const c_void, width: c_uint, height: c_uint, pitch: usize) {
    if data.is_null() {
        return; // duplicate frame
    }
    let t0 = Instant::now();
    ST.with(|st| {
        let mut guard = st.borrow_mut();
        video_frame(&mut guard, data, width, height, pitch);
        if let Some(v) = guard.video.as_mut() {
            v.time += t0.elapsed();
        }
    });
}

unsafe fn video_frame(st: &mut State, data: *const c_void, width: c_uint, height: c_uint, pitch: usize) {
    {
        let Some(v) = st.video.as_mut() else { return };
        // (The core may have begun this frame already, drawing into `back`.)
        if v.ready {
            v.begin_frame();
        } else {
            v.service();
        }
        let (mut w, mut h) = (width.min(MAX_W), height.min(MAX_H));
        let bpp = v.bpp as usize / 8;
        let rot = v.rot;
        // Turned by the 2D engine (staging buffer -> back buffer). Its
        // turns need the destination width a multiple of 8.
        let dst_w = if rot & 1 == 1 { h } else { w };
        if rot != 0 && v.ge_ok && dst_w % 8 == 0 && h <= MAX_W && w <= MAX_H {
            if let Some(stage) = v.stage.as_mut() {
                let spitch = stage.pitch as usize;
                let sp = stage.pixels();
                if data as *const u8 != sp.as_ptr() {
                    let src = std::slice::from_raw_parts(data as *const u8, pitch * h as usize);
                    for y in 0..h as usize {
                        sp[y * spitch..y * spitch + w as usize * bpp].copy_from_slice(&src[y * pitch..y * pitch + w as usize * bpp]);
                    }
                }
                let r = [ROTATE_0, ROTATE_90, ROTATE_180, ROTATE_270][rot as usize & 3];
                let (format, back) = (v.format, v.back);
                match v.card.blit(v.stage.as_ref().unwrap(), format, w, h, &v.bufs[back], format, r) {
                    Ok(()) => {
                        if rot & 1 == 1 {
                            (w, h) = (h, w);
                        }
                        return present(v, w, h);
                    }
                    Err(e) => {
                        eprintln!("retro: 2D engine: {e}; turning frames on the CPU");
                        v.ge_ok = false;
                        if data as *const u8 == v.stage.as_mut().unwrap().pixels().as_ptr() {
                            return; // this frame is lost; the next goes the CPU way
                        }
                    }
                }
            }
        }
        let buf = &mut v.bufs[v.back];
        let dpitch = buf.pitch as usize;
        let px = buf.pixels();
        if rot != 0 && h <= MAX_W && w <= MAX_H {
            let src = std::slice::from_raw_parts(data as *const u8, pitch * h as usize);
            rotate(src, pitch, w as usize, h as usize, px, dpitch, bpp, rot);
            if rot & 1 == 1 {
                (w, h) = (h, w);
            }
        } else if data as *const u8 != px.as_ptr() {
            // (Unless the core drew into this buffer itself.)
            let src = std::slice::from_raw_parts(data as *const u8, pitch * h as usize);
            for y in 0..h as usize {
                px[y * dpitch..y * dpitch + w as usize * bpp].copy_from_slice(&src[y * pitch..y * pitch + w as usize * bpp]);
            }
        }
        present(v, w, h);
    }
}

/// Shows the frame now in `back` (`w` x `h`): a new size places the plane
/// (synchronously), otherwise it is queued for the next vsync.
unsafe fn present(v: &mut Video, w: u32, h: u32) {
    if v.geometry != (w, h) {
        // A synchronous plane update: let a queued flip land first.
        if let Some(q) = v.queued.take() {
            let _ = v.card.poll_flip(100);
            v.front = q;
        }
        let fb = &v.bufs[v.back];
        // New resolution: place the primary plane (fit 4:3 or whatever
        // the core asks for into 640x480; the hardware scales).
        let (dw, dh) = placement(w, h, shape(w, h, v.aspect, v.rot & 1 == 1), v.scale);
        let mut sp = SetPlane {
            plane_id: v.card.planes[0],
            crtc_id: v.card.crtc,
            fb_id: fb.id,
            crtc_x: ((640 - dw) / 2) as i32,
            crtc_y: ((480 - dh) / 2) as i32,
            crtc_w: dw,
            crtc_h: dh,
            src_w: w << 16,
            src_h: h << 16,
            ..Default::default()
        };
        let r = v.card.set_plane(&mut sp);
        if r.is_ok() {
            v.geometry = (w, h);
            v.front = v.back;
            v.back = v.spare();
            v.shown += 1;
            v.drawn += 1;
            eprintln!("retro: video {w}x{h} -> {dw}x{dh} on screen");
        } else {
            eprintln!("retro: setplane {w}x{h}: {r:?}");
        }
    } else {
        v.ready = true;
        v.service();
    }
}

/// On-screen size of a `w` x `h` frame (aspect ratio `aspect`) on the
/// 640x480 panel: `Aspect` fits it keeping the aspect ratio; `Integer` uses
/// the largest whole multiples of the frame that fit, across and down
/// separately so non-square pixels keep their shape (PlayStation 320 x 480
/// at 4:3: 2 x 1; 512 x 240: 1 x 2), within 25 % of the aspect ratio (else
/// it falls back to aspect); `Stretch` fills the panel.
fn placement(w: u32, h: u32, aspect: f32, scale: Scale) -> (u32, u32) {
    let (pw, ph) = (640u32, 480u32);
    let fit = || {
        let (mut dw, mut dh) = (ph as f32 * aspect, ph as f32);
        if dw > pw as f32 {
            dw = pw as f32;
            dh = pw as f32 / aspect;
        }
        (dw.round() as u32, dh.round() as u32)
    };
    match scale {
        Scale::Stretch => (pw, ph),
        Scale::Integer => {
            let (w, h) = (w.max(1), h.max(1));
            // A pixel's width over its height.
            let par = aspect * h as f32 / w as f32;
            for ny in (1..=ph / h).rev() {
                let want = ny as f32 * par;
                for nx in [want.round(), want.floor()] {
                    let nx = nx as u32;
                    if nx >= 1 && w * nx <= pw && ((w * nx) as f32 / (h * ny) as f32 / aspect - 1.0).abs() <= 0.25 {
                        return (w * nx, h * ny);
                    }
                }
            }
            fit()
        }
        _ => fit(),
    }
}

unsafe extern "C" fn audio_sample(l: i16, r: i16) {
    ST.with(|st| st.borrow_mut().audio.extend_from_slice(&[l, r]));
}

unsafe extern "C" fn audio_sample_batch(data: *const i16, frames: usize) -> usize {
    ST.with(|st| st.borrow_mut().audio.extend_from_slice(std::slice::from_raw_parts(data, frames * 2)));
    frames
}

/// evdev key code -> RetroPad bit.
fn retropad_bit(code: u16) -> Option<c_uint> {
    Some(match code {
        0x130 => lr::DEVICE_ID_JOYPAD_B,      // BTN_SOUTH
        0x131 => lr::DEVICE_ID_JOYPAD_A,      // BTN_EAST
        0x133 => lr::DEVICE_ID_JOYPAD_X,      // BTN_NORTH
        0x134 => lr::DEVICE_ID_JOYPAD_Y,      // BTN_WEST
        0x136 => lr::DEVICE_ID_JOYPAD_L,      // BTN_TL
        0x137 => lr::DEVICE_ID_JOYPAD_R,      // BTN_TR
        0x138 => lr::DEVICE_ID_JOYPAD_L2,     // BTN_TL2
        0x139 => lr::DEVICE_ID_JOYPAD_R2,     // BTN_TR2
        0x13a => lr::DEVICE_ID_JOYPAD_SELECT, // BTN_SELECT
        0x13b => lr::DEVICE_ID_JOYPAD_START,  // BTN_START
        0x220 => lr::DEVICE_ID_JOYPAD_UP,
        0x221 => lr::DEVICE_ID_JOYPAD_DOWN,
        0x222 => lr::DEVICE_ID_JOYPAD_LEFT,
        0x223 => lr::DEVICE_ID_JOYPAD_RIGHT,
        _ => return None,
    })
}

unsafe extern "C" fn input_poll() {
    ST.with(|st| {
        let mut st = st.borrow_mut();
        let mut buttons = st.buttons;
        let mut volume = 0;
        let mut ev = [0u8; 16 * 32];
        for f in st.inputs.iter_mut() {
            while let Ok(n) = f.read(&mut ev) {
                if n == 0 {
                    break;
                }
                for e in ev[..n].chunks_exact(16) {
                    let (ty, code) = (u16::from_ne_bytes([e[8], e[9]]), u16::from_ne_bytes([e[10], e[11]]));
                    let value = i32::from_ne_bytes([e[12], e[13], e[14], e[15]]);
                    // Volume keys (KEY_VOLUMEDOWN/UP): a step per press (or repeat).
                    if ty == 1 && value != 0 && (code == 114 || code == 115) {
                        volume += if code == 115 { menu::VOLUME_STEP } else { -menu::VOLUME_STEP };
                    }
                    if ty == 1 && value != 2 {
                        if let Some(b) = retropad_bit(code) {
                            if value != 0 {
                                buttons |= 1 << b;
                            } else {
                                buttons &= !(1 << b);
                            }
                        }
                    }
                }
            }
        }
        st.buttons = buttons;
        if volume != 0 {
            st.volume = (st.volume + volume).clamp(0, 100);
            let _ = gb350_pcm::set_volume(st.volume as u8);
            st.volume_changed = true;
        }
    });
}

unsafe extern "C" fn input_state(port: c_uint, device: c_uint, _index: c_uint, id: c_uint) -> i16 {
    if port != 0 || device != lr::DEVICE_JOYPAD {
        return 0;
    }
    ST.with(|st| {
        let st = st.borrow();
        let mut b = st.buttons | st.scripted;
        if st.sideways && st.rotation & 1 == 1 {
            b = turn_dpad(b, st.rotation);
        }
        if id == lr::DEVICE_ID_JOYPAD_MASK {
            b as i16
        } else {
            (b >> id & 1) as i16
        }
    })
}

/// The D-pad as the game sees it with the console turned on its side
/// ("turn the console" for a vertical game): with `rotation` 1 the screen's
/// right edge is up, so the button pointing up is Right; 3 the other way.
fn turn_dpad(b: u16, rotation: u32) -> u16 {
    let bit = |id: c_uint| b >> id & 1;
    let (u, d, l, r) = (bit(lr::DEVICE_ID_JOYPAD_UP), bit(lr::DEVICE_ID_JOYPAD_DOWN), bit(lr::DEVICE_ID_JOYPAD_LEFT), bit(lr::DEVICE_ID_JOYPAD_RIGHT));
    let (gu, gd, gl, gr) = if rotation == 1 { (r, l, u, d) } else { (l, r, d, u) };
    let dirs = 1 << lr::DEVICE_ID_JOYPAD_UP | 1 << lr::DEVICE_ID_JOYPAD_DOWN | 1 << lr::DEVICE_ID_JOYPAD_LEFT | 1 << lr::DEVICE_ID_JOYPAD_RIGHT;
    (b & !dirs)
        | gu << lr::DEVICE_ID_JOYPAD_UP
        | gd << lr::DEVICE_ID_JOYPAD_DOWN
        | gl << lr::DEVICE_ID_JOYPAD_LEFT
        | gr << lr::DEVICE_ID_JOYPAD_RIGHT
}

/// Copies a `w` x `h` frame into `dst` turned `rot` quarter turns
/// counter-clockwise (for vertical games shown upright).
#[allow(clippy::too_many_arguments)]
fn rotate(src: &[u8], pitch: usize, w: usize, h: usize, dst: &mut [u8], dpitch: usize, bpp: usize, rot: u32) {
    for y in 0..h {
        for x in 0..w {
            let (nx, ny) = match rot {
                1 => (y, w - 1 - x),
                2 => (w - 1 - x, h - 1 - y),
                _ => (h - 1 - y, x),
            };
            let s = y * pitch + x * bpp;
            let d = ny * dpitch + nx * bpp;
            dst[d..d + bpp].copy_from_slice(&src[s..s + bpp]);
        }
    }
}

// ---- setup ----

fn open_inputs() -> Vec<std::fs::File> {
    let mut out = vec![];
    for e in std::fs::read_dir("/dev/input").into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with("event") {
            if let Ok(f) = std::fs::OpenOptions::new().read(true).custom_flags_nonblock().open(e.path()) {
                out.push(f);
            }
        }
    }
    out
}

trait NonBlock {
    fn custom_flags_nonblock(&mut self) -> &mut Self;
}

impl NonBlock for std::fs::OpenOptions {
    fn custom_flags_nonblock(&mut self) -> &mut Self {
        std::os::unix::fs::OpenOptionsExt::custom_flags(self, libc::O_NONBLOCK)
    }
}

fn open_video(av: &lr::SystemAvInfo, pixel_format: c_uint, native_bgr: bool) -> Result<Video, String> {
    let card = Card::open("/dev/dri/card0")?;
    card.set_master(true).map_err(|e| format!("{e} (is the display released? gbctl screen release)"))?;
    let (format, bpp) = match pixel_format {
        lr::PIXEL_FORMAT_XRGB8888 => (XRGB8888, 32),
        lr::PIXEL_FORMAT_RGB565 if native_bgr => (XBGR1555, 16),
        lr::PIXEL_FORMAT_RGB565 => (RGB565, 16),
        _ => (XRGB1555, 16),
    };
    let mut bufs = (0..3).map(|_| card.dumb(MAX_W, MAX_H, format, bpp)).collect::<Result<Vec<_>, _>>()?;
    for b in bufs.iter_mut() {
        b.pixels().fill(0);
    }
    // The panel mode whose refresh rate matches the game's (the driver
    // offers exact rates for the emulated systems): every frame shown once.
    // The launcher mode-sets the native one again when it takes over.
    // RETRO_RATE=native keeps the native 60.58 Hz.
    let native = card.native_mode().ok_or("no display mode")?;
    let mode = match std::env::var("RETRO_RATE").as_deref() {
        Ok("native") => native,
        _ => card.mode_for_rate(av.timing.fps).filter(|m| (m.refresh() - av.timing.fps).abs() / av.timing.fps < 0.02).unwrap_or(native),
    };
    card.set_crtc(&bufs[2], &mode)?;
    let hz = mode.refresh();
    eprintln!("retro: display mode {}x{} at {hz:.3} Hz", mode.hdisplay, mode.vdisplay);
    // A staging buffer for turned games: the core draws there, the 2D
    // engine turns it into the next display buffer.
    let stage = card.dumb(MAX_W, MAX_H, format, bpp).ok();
    let sharp = std::env::var("RETRO_FILTER").as_deref() == Ok("nearest");
    let _ = card.set_enum(card.planes[0], OBJ_PLANE, "SCALING_FILTER", if sharp { "Nearest Neighbor" } else { "Default" });
    let perf = menu::read_setting("perf").as_deref() == Some("on");
    let overlay = menu::Overlay::new(&card, perf).map_err(|e| eprintln!("retro: no menu: {e}")).ok();
    Ok(Video {
        card,
        bufs,
        format,
        bpp,
        front: 2,
        queued: None,
        back: 0,
        ready: false,
        geometry: (0, 0),
        aspect: aspect(&av.geometry),
        shown: 0,
        dropped: 0,
        time: Duration::ZERO,
        scale: Scale::from_env(),
        sharp,
        overlay,
        drawn: 0,
        rot: 0,
        hz,
        flips: (0, 0.0, 0.0),
        stage,
        ge_ok: std::env::var("RETRO_GE").as_deref() != Ok("off"),
    })
}

fn save_path(game: &str) -> String {
    let stem = std::path::Path::new(game).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    format!("{SAVE_DIR}/{stem}.srm")
}

/// Battery RAM / memory card: (pointer, size).
fn save_ram() -> Option<&'static mut [u8]> {
    unsafe {
        let p = lr::retro_get_memory_data(lr::MEMORY_SAVE_RAM) as *mut u8;
        let n = lr::retro_get_memory_size(lr::MEMORY_SAVE_RAM);
        (!p.is_null() && n > 0).then(|| std::slice::from_raw_parts_mut(p, n))
    }
}

fn write_save(path: &str) {
    if let Some(ram) = save_ram() {
        match std::fs::write(path, &*ram) {
            Ok(()) => eprintln!("retro: saved {} bytes to {path}", ram.len()),
            Err(e) => eprintln!("retro: {path}: {e}"),
        }
    }
}

fn run(game: &str, secs: Option<u64>) -> Result<(), String> {
    // PCSX-ReARMed's recompiler maps the PS1 address space at 0 (fastest
    // code); Linux refuses mappings below vm.mmap_min_addr (4096).
    if CORE == "pcsx_rearmed" && std::fs::write("/proc/sys/vm/mmap_min_addr", "0").is_err() {
        eprintln!("retro: could not lower vm.mmap_min_addr (slower recompiled code)");
    }
    for d in [SYSTEM_DIR, SAVE_DIR] {
        std::fs::create_dir_all(d).map_err(|e| format!("{d}: {e}"))?;
    }
    let mut options: Vec<(String, String)> = core_options().iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    if let Ok(o) = std::env::var("RETRO_OPTS") {
        for kv in o.split(',').filter_map(|kv| kv.split_once('=')) {
            options.retain(|(k, _)| k != kv.0);
            options.push((kv.0.into(), kv.1.into()));
        }
    }
    ST.with(|st| {
        let mut st = st.borrow_mut();
        st.options = options.iter().map(|(k, v)| (CString::new(k.as_str()).unwrap(), CString::new(v.as_str()).unwrap())).collect();
        st.sideways = menu::read_setting("vertical").as_deref() == Some("sideways");
        st.inputs = open_inputs();
    });

    let content;
    unsafe {
        lr::retro_set_environment(environment);
        lr::retro_set_video_refresh(video_refresh);
        lr::retro_set_audio_sample(audio_sample);
        lr::retro_set_audio_sample_batch(audio_sample_batch);
        lr::retro_set_input_poll(input_poll);
        lr::retro_set_input_state(input_state);
        // PlayStation: VRAM the second core can draw into too.
        #[cfg(core_pcsx)]
        ps1split::install();
        lr::retro_init();
        let mut si: lr::SystemInfo = std::mem::zeroed();
        lr::retro_get_system_info(&mut si);
        let cs = |p: *const c_char| if p.is_null() { String::new() } else { CStr::from_ptr(p).to_string_lossy().into_owned() };
        eprintln!("retro: core {} {} (api {})", cs(si.library_name), cs(si.library_version), lr::retro_api_version());

        // Kept for the whole run (cores may keep pointing into the data).
        content = content::prepare(game, &si)?;
        if let Some((k, v)) = header_options(&content) {
            if !options.iter().any(|(o, _)| o == k) {
                eprintln!("retro: {k}={v} (from the cartridge header)");
                ST.with(|st| st.borrow_mut().options.push((CString::new(k).unwrap(), CString::new(v).unwrap())));
            }
        }
        if !lr::retro_load_game(&content.game_info()) {
            return Err(format!("{game}: the core could not load it"));
        }
    }
    // PlayStation: drawing shared with the second core unless the
    // launcher's setting says main core only (RETRO_PS1_SPLIT = 1 | 0
    // overrides); the in-game menu switches it.
    #[cfg(core_pcsx)]
    let ps1_cores = match std::env::var("RETRO_PS1_SPLIT").as_deref() {
        Ok(v) => v == "1",
        Err(_) => menu::read_setting("ps1_cores").as_deref() != Some("1"),
    };
    #[cfg(core_pcsx)]
    if ps1_cores {
        ps1split::start();
    }
    let srm = save_path(game);
    stuck::watch(srm.clone());
    if let (Some(ram), Ok(data)) = (save_ram(), std::fs::read(&srm)) {
        let n = ram.len().min(data.len());
        ram[..n].copy_from_slice(&data[..n]);
        eprintln!("retro: loaded {srm}");
    }
    let mut av = lr::SystemAvInfo::default();
    unsafe { lr::retro_get_system_av_info(&mut av) };
    eprintln!("retro: {:?}", av);
    unsafe { lr::retro_set_controller_port_device(0, lr::DEVICE_JOYPAD) };

    let (pixel_format, native_bgr) = ST.with(|st| (st.borrow().pixel_format, st.borrow().native_bgr));
    let mut video = open_video(&av, pixel_format, native_bgr)?;
    // A rotation asked for while loading (before there was a display).
    video.rot = ST.with(|st| if st.borrow().sideways { 0 } else { st.borrow().rotation });
    eprintln!("retro: video format {:#x}, {} bpp", video.format, video.bpp);
    ST.with(|st| st.borrow_mut().video = Some(video));

    let env_num = |k: &str| std::env::var(k).ok().and_then(|g| g.parse::<i32>().ok());
    let volume = env_num("RETRO_VOLUME").or_else(|| env_num("RETRO_GAIN").map(|g| (g.clamp(0, 255) * 100 + 127) / 255)).unwrap_or(10).clamp(0, 100);
    let _ = gb350_pcm::set_volume(volume as u8);
    ST.with(|st| st.borrow_mut().volume = volume);
    let rate = av.timing.sample_rate.round() as u32;
    let dev_rate = resample::device_rate(rate);
    // Pacing. By the display (one emulated frame per refresh, sound rate
    // nudged to keep its buffer half full) when the game's frame rate is
    // within 2% of the panel's: every frame is shown once, none dropped or
    // doubled. Otherwise (PAL 50 Hz, WonderSwan 75 Hz) by the sound card.
    // PCSX stays on the sound card: its frameskip watches the buffer level.
    // RETRO_SYNC=video|audio overrides.
    let hz = ST.with(|st| st.borrow().video.as_ref().map_or(60.0, |v| v.hz));
    let fps = av.timing.fps;
    let by_display = match std::env::var("RETRO_SYNC").as_deref() {
        Ok("video") => true,
        Ok("audio") => false,
        _ => CORE != "pcsx_rearmed" && (fps - hz).abs() / hz < 0.02,
    };
    // Input frames per output frame when running at `hz` instead of `fps`.
    let base_ratio = av.timing.sample_rate * hz / fps / dev_rate as f64;
    let mut resampler = (by_display || dev_rate != rate).then(|| {
        let mut r = resample::Resampler::new(rate, dev_rate);
        if by_display {
            r.set_ratio(base_ratio);
        }
        r
    });
    eprintln!("retro: game {fps:.2} fps, display {hz:.2} Hz: paced by the {}", if by_display { "display" } else { "sound card" });
    let (mut skew_sum, mut skew_n) = (0f64, 0u32);
    // The steady part of the clock difference, learnt slowly (so the
    // buffer settles at half full instead of wherever the correction
    // balances it).
    let mut drift = 0f64;
    let mut resampled: Vec<i16> = Vec::with_capacity(8192);
    // ~140 ms in all: rides out the recompiler's compile bursts.
    // RETRO_PERIOD / RETRO_PERIODS: the chunking (frames per period, count).
    // Small periods keep the core's frames evenly spaced: with 1024 frames
    // the audio write blocks for ~21 ms at a time and frames come in bursts.
    let period = env_num("RETRO_PERIOD").unwrap_or(256).max(64) as u32;
    let periods = env_num("RETRO_PERIODS").unwrap_or(6144 / period as i32).max(2) as u32;
    let mut pcm = Pcm::open(0, dev_rate, 2, period, periods)?;
    if by_display {
        // Start (and restart after an underrun) at the level kept.
        pcm.set_start(pcm.buffer / 2)?;
    }
    eprintln!("retro: audio {rate} Hz (device {dev_rate}), period {} buffer {}", pcm.period, pcm.buffer);

    let script = automation::script();
    let shots = automation::frames("RETRO_SHOT");
    // RETRO_SCREEN="frame,...": the whole panel, overlay included.
    let screens = automation::frames("RETRO_SCREEN");
    let save_at: Option<(u64, String)> = std::env::var("RETRO_SAVE_AT")
        .ok()
        .and_then(|v| v.split_once(':').map(|(f, p)| (f.parse().ok(), p.to_string())))
        .and_then(|(f, p)| Some((f?, p)));
    // Loaded once the core has run a few frames (right after load_game the
    // recompiler crashed on its first block after the restore).
    let load_state = std::env::var("RETRO_LOAD").ok();
    // RETRO_LOAD_FRAME: later for discs that boot through a loader program
    // (the HLE BIOS state across that hand-over is not in the state).
    let load_frame: u64 = std::env::var("RETRO_LOAD_FRAME").ok().and_then(|f| f.parse().ok()).unwrap_or(5);
    let mut frame_no: u64 = 0;
    let profile_path = std::env::var("RETRO_PROFILE").ok();
    if profile_path.is_some() {
        profile::start();
    }
    let start = Instant::now();
    let (mut frames, mut last_report, mut last_save) = (0u32, Instant::now(), Instant::now());
    let mut save_snapshot: Vec<u8> = save_ram().map(|r| r.to_vec()).unwrap_or_default();
    let mut quit_since: Option<Instant> = None;
    let system = std::env::var("RETRO_SYSTEM").unwrap_or_else(|_| "PlayStation".into());
    let mut menu = menu::Menu::new(game, &system);
    #[cfg(core_pcsx)]
    if ps1split::available() {
        menu.with_cores(ps1_cores);
    }
    // A separate handle on the buttons for the menu (the game's are polled by
    // the core).
    let mut menu_input = Input::open();
    let (mut perf_frames, mut perf_at, mut perf_drawn) = (0u32, Instant::now(), 0u64);
    let mut perf_sampler = gb350_ui::sysstat::Sampler::default();
    perf_sampler.sample();
    // SIGTERM/SIGINT: quit like Select + Start (saves are written).
    extern "C" fn on_quit(_: i32) {
        QUIT.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    unsafe {
        libc::signal(libc::SIGTERM, on_quit as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_quit as *const () as libc::sighandler_t);
    }
    let (mut t_run, mut t_audio) = (Duration::ZERO, Duration::ZERO);
    // RETRO_SLOW_DETAIL: CPU time / switches / faults for each slow frame.
    let slow_detail = std::env::var("RETRO_SLOW_DETAIL").is_ok();
    let mut samples = 0usize;
    loop {
        // Tell the core how full the audio buffer is (auto frameskip).
        if let Some(cb) = ST.with(|st| st.borrow().audio_status) {
            let occupancy = (pcm.delay() as u64 * 100 / pcm.buffer.max(1) as u64).min(100) as c_uint;
            unsafe { cb(true, occupancy, occupancy < 25) };
        }
        if frame_no == load_frame {
            if let Some(p) = &load_state {
                let _ = automation::load_state(p);
            }
        }
        if let Some(&(_, b)) = script.iter().rev().find(|e| e.0 <= frame_no) {
            ST.with(|st| st.borrow_mut().scripted = b);
        }
        if by_display {
            ST.with(|st| {
                if let Some(v) = st.borrow_mut().video.as_mut() {
                    v.make_room();
                }
            });
        }
        let t0 = Instant::now();
        // getrusage every frame cost ~2 % (PS1 profile): only on request.
        let r0 = slow_detail.then(rusage);
        stuck::enter();
        unsafe { lr::retro_run() };
        stuck::leave();
        #[cfg(core_pcsx)]
        ps1split::frame();
        frame_no += 1;

        if shots.contains(&frame_no) {
            ST.with(|st| {
                let mut st = st.borrow_mut();
                if let Some(v) = st.video.as_mut() {
                    let shown = v.front;
                    let (w, h) = v.geometry;
                    let (pitch, format) = (v.bufs[shown].pitch, v.format);
                    let px = v.bufs[shown].pixels().to_vec();
                    automation::screenshot(&format!("/tmp/shot-{frame_no}.ppm"), &px, w, h, pitch, format);
                }
            });
        }
        if screens.contains(&frame_no) {
            ST.with(|st| {
                if let Some(v) = st.borrow_mut().video.as_mut() {
                    let mut screen = screen_rgb(v);
                    if let Some(ov) = v.overlay.as_mut() {
                        ov.composite(&mut screen);
                    }
                    write_ppm(&format!("/tmp/screen-{frame_no}.ppm"), &screen);
                }
            });
        }
        if save_at.as_ref().is_some_and(|(f, _)| *f == frame_no) {
            automation::save_state(&save_at.as_ref().unwrap().1);
        }
        let dt = t0.elapsed();
        t_run += dt;
        frames += 1;
        let audio = ST.with(|st| std::mem::take(&mut st.borrow_mut().audio));
        samples += audio.len() / 2;
        if dt > Duration::from_millis(30) {
            match r0 {
                Some(r0) => {
                    let r1 = rusage();
                    eprintln!(
                        "retro: slow frame {:.1} ms: user {:.1} sys {:.1} ms, {} voluntary / {} forced switches, {} major faults",
                        dt.as_secs_f32() * 1000.0,
                        (r1.0 - r0.0) as f32 / 1000.0,
                        (r1.1 - r0.1) as f32 / 1000.0,
                        r1.2 - r0.2,
                        r1.3 - r0.3,
                        r1.4 - r0.4
                    );
                }
                None => eprintln!("retro: slow frame {:.1} ms", dt.as_secs_f32() * 1000.0),
            }
        }
        let t1 = Instant::now();
        if let (true, Some(r)) = (by_display, &mut resampler) {
            // Up to 1% faster or slower sound for the buffer level (half
            // full = as is), plus the learnt clock difference (up to 2%).
            const MAX_SKEW: f64 = 0.01;
            let error = (pcm.delay() as f64 / pcm.buffer as f64 - 0.5) / 0.5;
            drift = (drift + 2e-5 * error).clamp(-0.02, 0.02);
            let skew = (MAX_SKEW * error).clamp(-MAX_SKEW, MAX_SKEW) + drift;
            r.set_ratio(base_ratio * (1.0 + skew));
            skew_sum += skew;
            skew_n += 1;
        }
        match &mut resampler {
            Some(r) => {
                resampled.clear();
                r.process(&audio, &mut resampled);
                pcm.write(&resampled)?;
            }
            None => pcm.write(&audio)?,
        }
        t_audio += t1.elapsed();
        ST.with(|st| {
            let mut st = st.borrow_mut();
            let mut a = audio;
            a.clear();
            st.audio = a;
            // A vsync may have passed while the sound blocked: queue a frame
            // that was waiting for it.
            if let Some(v) = st.video.as_mut() {
                v.service();
            }
        });

        // Stats overlay, once a second: emulated and drawn frames, CPU use.
        perf_frames += 1;
        if perf_at.elapsed() >= Duration::from_secs(1) {
            let secs = perf_at.elapsed().as_secs_f32();
            let sys = perf_sampler.sample();
            ST.with(|st| {
                let mut st = st.borrow_mut();
                if let Some(v) = st.video.as_mut() {
                    let drawn = v.drawn - perf_drawn;
                    perf_drawn = v.drawn;
                    if let Some(ov) = v.overlay.as_mut().filter(|o| o.perf_on) {
                        let pct = |p: Option<u32>| p.map_or("-".to_string(), |p| format!("{p}%"));
                        let fps = perf_frames as f32 / secs;
                        ov.set_perf(
                            &v.card,
                            format!(
                                "{fps:.0} fps · {:.0} drawn · CPU {} · 2nd core {} · RAM {} MB",
                                drawn as f32 / secs,
                                pct(sys.cpu),
                                pct(sys.second_core),
                                sys.mem_used_mb
                            ),
                        );
                    }
                }
            });
            perf_frames = 0;
            perf_at = Instant::now();
        }

        // Volume keys: show the slider; hide it when its time is up.
        let volume_shown = ST.with(|st| {
            let mut st = st.borrow_mut();
            std::mem::take(&mut st.volume_changed).then_some(st.volume)
        });
        ST.with(|st| {
            let mut st = st.borrow_mut();
            if let Some(v) = st.video.as_mut() {
                if let Some(ov) = v.overlay.as_mut() {
                    if let Some(g) = volume_shown {
                        let kicker = if g == 0 { "VOLUME · MUTED" } else { "VOLUME" };
                        ov.osd(&v.card, kicker, "", Some(g as u32));
                    }
                    ov.tick(&v.card);
                }
            }
        });

        // Hold Select + Start: the menu (pauses the game).
        let buttons = ST.with(|st| st.borrow().buttons | st.borrow().scripted);
        let combo = 1 << lr::DEVICE_ID_JOYPAD_SELECT | 1 << lr::DEVICE_ID_JOYPAD_START;
        if buttons & combo == combo {
            if quit_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_millis(MENU_HOLD_MS) {
                quit_since = None;
                let Some(mut video) = ST.with(|st| st.borrow_mut().video.take()) else { break };
                let mut volume = ST.with(|st| st.borrow().volume);
                let headphones = menu_input.headphones_now();
                let action = if video.overlay.is_some() {
                    menu.run(&mut video, &mut menu_input, &mut volume, headphones)
                } else {
                    menu::Action::Quit // no overlay plane: the old behaviour
                };
                ST.with(|st| {
                    let mut st = st.borrow_mut();
                    st.volume = volume;
                    // Buttons pressed in the menu are not the game's.
                    let mut ev = [0u8; 16 * 32];
                    for f in st.inputs.iter_mut() {
                        while matches!(f.read(&mut ev), Ok(n) if n > 0) {}
                    }
                    st.buttons = 0;
                });
                #[cfg(core_pcsx)]
                if let Some(on) = menu.cores {
                    ps1split::set_enabled(on);
                }
                let note = match action {
                    menu::Action::Resume => None,
                    menu::Action::Quit => {
                        ST.with(|st| st.borrow_mut().video = Some(video));
                        eprintln!("retro: quit from the menu");
                        break;
                    }
                    menu::Action::Save(slot) => {
                        automation::save_state(&menu::state_path(menu.stem(), slot));
                        Some(format!("Saved to slot {slot}"))
                    }
                    menu::Action::Load(slot) => {
                        // A bad state can take the emulator down: keep the
                        // memory card safe first.
                        write_save(&srm);
                        if automation::load_state(&menu::state_path(menu.stem(), slot)) {
                            Some(format!("Loaded slot {slot}"))
                        } else {
                            Some(format!("Slot {slot} can't be loaded"))
                        }
                    }
                    menu::Action::Reset => {
                        unsafe { lr::retro_reset() };
                        Some("Game restarted".to_string())
                    }
                };
                if let (Some(n), Some(ov)) = (&note, video.overlay.as_mut()) {
                    ov.osd(&video.card, "STATE", n, None);
                }
                ST.with(|st| st.borrow_mut().video = Some(video));
                (perf_frames, perf_at) = (0, Instant::now());
                last_report = Instant::now();
                frames = 0;
                samples = 0;
                t_run = Duration::ZERO;
                t_audio = Duration::ZERO;
                continue;
            }
        } else {
            quit_since = None;
        }
        if secs.is_some_and(|s| start.elapsed() > Duration::from_secs(s)) {
            break;
        }
        if QUIT.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("retro: asked to quit");
            break;
        }
        if last_report.elapsed() >= Duration::from_secs(5) {
            let el = last_report.elapsed().as_secs_f32();
            let (shown, dropped, t_video) = ST.with(|st| {
                let mut st = st.borrow_mut();
                let v = st.video.as_mut().unwrap();
                let r = (v.shown, v.dropped, v.time);
                v.shown = 0;
                v.dropped = 0;
                v.time = Duration::ZERO;
                r
            });
            let per = |d: Duration| d.as_secs_f32() * 1000.0 / frames.max(1) as f32;
            eprintln!(
                "retro: {:.1} fps emulated ({:.0} samples/s), {:.1} shown, {dropped} dropped, {} underruns; per frame: core {:.1} ms (video {:.1}), audio wait {:.1} ms",
                frames as f32 / el,
                samples as f32 / el,
                shown as f32 / el,
                pcm.xruns,
                per(t_run),
                per(t_video),
                per(t_audio)
            );
            #[cfg(core_pcsx)]
            if let Some(r) = ps1split::report() {
                eprintln!("{r}");
            }
            if skew_n > 0 {
                let hz = ST.with(|st| st.borrow_mut().video.as_mut().and_then(|v| v.measured_hz()));
                eprintln!(
                    "retro: sound rate adjusted {:+.2}% on average ({:+.2}% clock difference), display {}, buffer {}% full",
                    skew_sum / skew_n as f64 * 100.0,
                    drift * 100.0,
                    hz.map_or("-".into(), |h| format!("{h:.3} Hz")),
                    pcm.delay() * 100 / pcm.buffer
                );
                (skew_sum, skew_n) = (0.0, 0);
            }
            t_run = Duration::ZERO;
            t_audio = Duration::ZERO;
            samples = 0;
            frames = 0;
            last_report = Instant::now();
        }
        if last_save.elapsed() >= Duration::from_secs(30) {
            last_save = Instant::now();
            if let Some(ram) = save_ram() {
                if ram[..] != save_snapshot[..] {
                    save_snapshot = ram.to_vec();
                    write_save(&srm);
                }
            }
        }
    }
    if let Some(p) = &profile_path {
        profile::save(p);
    }
    write_save(&srm);
    // The volume keys may have changed it: the launcher reads it back.
    let volume = ST.with(|st| st.borrow().volume);
    menu::save_settings(menu.settings_path(), &[("volume_pct", volume.to_string())]);
    unsafe {
        lr::retro_unload_game();
        lr::retro_deinit();
    }
    let _ = pcm.drain();
    Ok(())
}

/// Test automation (for driving games without hands on the device):
///   RETRO_SCRIPT="frame:buttons,..."  from `frame` on hold `buttons`
///                 ("start+right", "a", or "-" for none)
///   RETRO_SHOT="frame,..."            save the shown frame as
///                 /tmp/shot-<frame>.ppm
///   RETRO_SAVE_AT="frame:file"        save a state; RETRO_LOAD=file loads one
mod automation {
    use super::lr;

    pub fn parse_buttons(s: &str) -> u16 {
        s.split('+')
            .filter_map(|b| {
                Some(match b {
                    "b" => lr::DEVICE_ID_JOYPAD_B,
                    "y" => lr::DEVICE_ID_JOYPAD_Y,
                    "select" => lr::DEVICE_ID_JOYPAD_SELECT,
                    "start" => lr::DEVICE_ID_JOYPAD_START,
                    "up" => lr::DEVICE_ID_JOYPAD_UP,
                    "down" => lr::DEVICE_ID_JOYPAD_DOWN,
                    "left" => lr::DEVICE_ID_JOYPAD_LEFT,
                    "right" => lr::DEVICE_ID_JOYPAD_RIGHT,
                    "a" => lr::DEVICE_ID_JOYPAD_A,
                    "x" => lr::DEVICE_ID_JOYPAD_X,
                    "l" => lr::DEVICE_ID_JOYPAD_L,
                    "r" => lr::DEVICE_ID_JOYPAD_R,
                    _ => return None,
                })
            })
            .fold(0u16, |m, b| m | 1 << b)
    }

    /// (frame, buttons) steps, sorted.
    pub fn script() -> Vec<(u64, u16)> {
        let mut v: Vec<(u64, u16)> = std::env::var("RETRO_SCRIPT")
            .unwrap_or_default()
            .split(',')
            .filter_map(|e| e.split_once(':'))
            .filter_map(|(f, b)| Some((f.trim().parse().ok()?, parse_buttons(b.trim()))))
            .collect();
        v.sort_by_key(|e| e.0);
        v
    }

    pub fn frames(var: &str) -> Vec<u64> {
        std::env::var(var).unwrap_or_default().split(',').filter_map(|f| f.trim().parse().ok()).collect()
    }

    /// Writes a binary PPM from the shown buffer (16-bit 1555/565 or 32-bit).
    pub fn screenshot(path: &str, px: &[u8], w: u32, h: u32, pitch: u32, format: u32) {
        let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
        for y in 0..h as usize {
            for x in 0..w as usize {
                let rgb = if format == gb350_kms::XRGB8888 {
                    let o = y * pitch as usize + x * 4;
                    [px[o + 2], px[o + 1], px[o]]
                } else {
                    let o = y * pitch as usize + x * 2;
                    let v = u16::from_le_bytes([px[o], px[o + 1]]) as u32;
                    let c5 = |c: u32| ((c & 31) << 3 | (c & 31) >> 2) as u8;
                    match format {
                        f if f == gb350_kms::XBGR1555 => [c5(v), c5(v >> 5), c5(v >> 10)],
                        f if f == gb350_kms::XRGB1555 => [c5(v >> 10), c5(v >> 5), c5(v)],
                        _ => [c5(v >> 11), ((v >> 5 & 63) << 2) as u8, c5(v)],
                    }
                };
                out.extend_from_slice(&rgb);
            }
        }
        match std::fs::write(path, out) {
            Ok(()) => eprintln!("retro: screenshot {path} ({w}x{h})"),
            Err(e) => eprintln!("retro: {path}: {e}"),
        }
    }

    pub fn save_state(path: &str) {
        unsafe {
            let n = lr::retro_serialize_size();
            let mut buf = vec![0u8; n];
            if lr::retro_serialize(buf.as_mut_ptr() as *mut _, n) {
                match std::fs::write(path, &buf) {
                    Ok(()) => eprintln!("retro: state saved to {path} ({n} bytes)"),
                    Err(e) => eprintln!("retro: {path}: {e}"),
                }
            } else {
                eprintln!("retro: serialize failed");
            }
        }
    }

    /// Whether the state was loaded.
    pub fn load_state(path: &str) -> bool {
        match std::fs::read(path) {
            Ok(buf) => {
                // RETRO_LOAD_FORCE: load anyway (testing the stuck-frame exit).
                if let Some(why) = damaged(&buf).filter(|_| std::env::var_os("RETRO_LOAD_FORCE").is_none()) {
                    eprintln!("retro: state {path}: REJECTED ({why})");
                    return false;
                }
                let ok = unsafe { lr::retro_unserialize(buf.as_ptr() as *const _, buf.len()) };
                #[cfg(core_pcsx)]
                super::ps1split::vram_loaded();

                eprintln!("retro: state {path}: {}", if ok { "loaded" } else { "REJECTED" });
                ok
            }
            Err(e) => {
                eprintln!("retro: {path}: {e}");
                false
            }
        }
    }

    /// States that would hang the core instead of resuming. PCSX-ReARMed
    /// ("STv4 PCSX"): header 37 bytes, picture 36864, RAM 2 MiB, BIOS 512
    /// KiB, hardware 64 KiB, then the CPU registers (34 GPRs, 3 x 32 COP
    /// registers, pc, code, cycle). A Ridge Racer state from an older build
    /// had them all zero (and zero root counters): the core spun forever in
    /// its first frame.
    fn damaged(buf: &[u8]) -> Option<&'static str> {
        if !buf.starts_with(b"STv4 PCSX") {
            return None;
        }
        let regs = 37 + 128 * 96 * 3 + 0x20_0000 + 0x8_0000 + 0x1_0000;
        let word = |i: usize| buf.get(regs + 4 * i..regs + 4 * i + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
        match word(130) {
            None => Some("too short"),
            Some(0) => Some("damaged: no CPU state"),
            _ => None,
        }
    }
}

/// A frame that never ends (the core stuck in a loop, e.g. after a damaged
/// state) keeps the menu from ever running: once a frame has taken 3 s,
/// holding Select + Start for 1.5 s saves the memory card / battery RAM and
/// leaves (the launcher comes back). Watched from a thread of its own, the
/// buttons read with EVIOCGKEY (their current state).
mod stuck {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    static BASE: OnceLock<Instant> = OnceLock::new();
    /// When the running frame started (ms since BASE, + 1; MIPS32 has no
    /// 64-bit atomics); 0 between frames.
    static IN_FRAME: AtomicU32 = AtomicU32::new(0);
    const STUCK: Duration = Duration::from_secs(3);
    const HOLD: Duration = Duration::from_millis(1500);

    fn now_ms() -> u32 {
        (BASE.get_or_init(Instant::now).elapsed().as_millis() as u32).wrapping_add(1).max(1)
    }

    pub fn enter() {
        IN_FRAME.store(now_ms(), Ordering::Relaxed);
    }

    pub fn leave() {
        IN_FRAME.store(0, Ordering::Relaxed);
    }

    /// Whether `code` is down on the device (EVIOCGKEY: 96 bytes of key bits).
    fn down(f: &std::fs::File, code: usize) -> bool {
        use std::os::fd::AsRawFd;
        let mut bits = [0u8; 96];
        const EVIOCGKEY: u32 = (2 << 29) | (96 << 16) | (0x45 << 8) | 0x18; // MIPS _IOR('E', 0x18, 96)
        // Returns the bytes copied.
        unsafe { libc::ioctl(f.as_raw_fd(), EVIOCGKEY as _, bits.as_mut_ptr()) >= 0 && bits[code / 8] & (1 << (code % 8)) != 0 }
    }

    pub fn watch(srm: String) {
        let _ = std::thread::Builder::new().name("stuck".into()).spawn(move || {
            let inputs = super::open_inputs();
            let mut held: Option<Instant> = None;
            loop {
                std::thread::sleep(Duration::from_millis(200));
                let start = IN_FRAME.load(Ordering::Relaxed);
                if start == 0 || now_ms().wrapping_sub(start) < STUCK.as_millis() as u32 {
                    held = None;
                    continue;
                }
                // BTN_SELECT, BTN_START.
                if inputs.iter().any(|f| down(f, 0x13a) && down(f, 0x13b)) {
                    if held.get_or_insert_with(Instant::now).elapsed() >= HOLD {
                        eprintln!("retro: the game is stuck in a frame ({} s): leaving on Select + Start", now_ms().wrapping_sub(start) / 1000);
                        super::write_save(&srm);
                        unsafe { libc::_exit(3) };
                    }
                } else {
                    held = None;
                }
            }
        });
    }
}

/// RETRO_PROFILE=<file>: a sampling profiler - SIGPROF at ~1 kHz of CPU
/// time records the interrupted PC; written as little-endian u32s on exit
/// (resolve them against an unstripped build).
mod profile {
    use std::sync::atomic::{AtomicUsize, Ordering};

    const MAX: usize = 1 << 18;
    static mut SAMPLES: [u32; MAX] = [0; MAX];
    static COUNT: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn on_prof(_sig: i32, _info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
        // o32 ucontext: flags, link, stack_t (12 bytes), then sigcontext
        // (8-aligned, at 24): regmask, status, then pc as a u64 at 32.
        let pc = unsafe { *((ctx as *const u8).add(32) as *const u32) };
        let i = COUNT.fetch_add(1, Ordering::Relaxed);
        if i < MAX {
            unsafe { SAMPLES[i] = pc };
        }
    }

    pub fn start() {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_prof as *const () as usize;
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigaction(libc::SIGPROF, &sa, std::ptr::null_mut());
            let t = libc::itimerval {
                it_interval: libc::timeval { tv_sec: 0, tv_usec: 1000 },
                it_value: libc::timeval { tv_sec: 0, tv_usec: 1000 },
            };
            libc::setitimer(libc::ITIMER_PROF, &t, std::ptr::null_mut());
        }
    }

    #[allow(static_mut_refs)]
    pub fn save(path: &str) {
        unsafe {
            let t: libc::itimerval = std::mem::zeroed();
            libc::setitimer(libc::ITIMER_PROF, &t, std::ptr::null_mut());
            let n = COUNT.load(Ordering::Relaxed).min(MAX);
            let bytes: Vec<u8> = SAMPLES[..n].iter().flat_map(|v| v.to_le_bytes()).collect();
            let _ = std::fs::write(path, bytes);
            eprintln!("retro: {n} profile samples -> {path}");
        }
    }
}

/// Process user/system microseconds, voluntary/involuntary switches, major faults.
fn rusage() -> (i64, i64, i64, i64, i64) {
    let mut r: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r) };
    let us = |t: libc::timeval| t.tv_sec as i64 * 1_000_000 + t.tv_usec as i64;
    (us(r.ru_utime), us(r.ru_stime), r.ru_nvcsw as i64, r.ru_nivcsw as i64, r.ru_majflt as i64)
}

/// `retro --memtest`: checks the linked memcpy/memset against byte loops
/// (all small sizes and alignments) and times large copies.
fn memtest() {
    extern "C" {
        fn memcpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void;
        fn memset(d: *mut c_void, c: i32, n: usize) -> *mut c_void;
    }
    let mut src = vec![0u8; 1 << 21];
    for (i, b) in src.iter_mut().enumerate() {
        *b = (i * 7 + i / 251) as u8;
    }
    let mut dst = vec![0u8; 1 << 21];
    let mut bad = 0;
    for n in 0..300usize {
        for so in 0..8usize {
            for d_o in 0..8usize {
                dst[..400].fill(0xaa);
                unsafe { memcpy(dst.as_mut_ptr().add(d_o).cast(), src.as_ptr().add(so).cast(), n) };
                let ok = dst[..d_o].iter().all(|&b| b == 0xaa)
                    && dst[d_o..d_o + n] == src[so..so + n]
                    && dst[d_o + n..400].iter().all(|&b| b == 0xaa);
                dst[..400].fill(0x55);
                unsafe { memset(dst.as_mut_ptr().add(d_o).cast(), 0x3c, n) };
                let ok2 = dst[..d_o].iter().all(|&b| b == 0x55)
                    && dst[d_o..d_o + n].iter().all(|&b| b == 0x3c)
                    && dst[d_o + n..400].iter().all(|&b| b == 0x55);
                if !ok || !ok2 {
                    bad += 1;
                    if bad < 5 {
                        eprintln!("memtest: FAIL n {n} src+{so} dst+{d_o} memcpy {ok} memset {ok2}");
                    }
                }
            }
        }
    }
    for size in [4096usize, 65536, 1 << 20] {
        let reps = (64 << 20) / size;
        let t = Instant::now();
        for _ in 0..reps {
            unsafe { memcpy(dst.as_mut_ptr().cast(), src.as_ptr().cast(), size) };
        }
        let cp = (reps * size) as f64 / t.elapsed().as_secs_f64() / 1e6;
        let t = Instant::now();
        for _ in 0..reps {
            unsafe { memset(dst.as_mut_ptr().cast(), 0, size) };
        }
        let st = (reps * size) as f64 / t.elapsed().as_secs_f64() / 1e6;
        eprintln!("memtest: {size} B: memcpy {cp:.0} MB/s, memset {st:.0} MB/s");
    }
    eprintln!("memtest: {} failures", bad);
}

/// Fatal signals (a core or its recompiler going wrong): one line with
/// where, to the log, then the default action (the launcher reports it).
mod crash {
    extern "C" fn on_crash(sig: i32, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
        // o32 ucontext: sigcontext at 24: regmask, status, pc (u64 at 32),
        // then 32 general registers as u64 from 40.
        let reg = |i: usize| unsafe { *((ctx as *const u8).add(40 + 8 * i) as *const u32) };
        let pc = unsafe { *((ctx as *const u8).add(32) as *const u32) };
        let addr = unsafe { (*info).si_addr() } as usize;
        let mut line = [0u8; 160];
        let n = {
            use std::io::Write;
            let mut c = std::io::Cursor::new(&mut line[..]);
            let _ = write!(c, "retro: signal {sig} at pc {pc:#010x} ra {:#010x} sp {:#010x} addr {addr:#010x}\n", reg(31), reg(29));
            c.position() as usize
        };
        unsafe {
            libc::write(2, line.as_ptr() as *const _, n);
        }
        // Dying anyway: allocation is fine now. RETRO_CRASH_DETAIL adds the
        // registers, the code before ra and the mappings of pc / ra.
        if std::env::var("RETRO_CRASH_DETAIL").is_ok() {
            let names = ["zero", "at", "v0", "v1", "a0", "a1", "a2", "a3", "t0", "t1", "t2", "t3", "t4", "t5", "t6", "t7", "s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7", "t8", "t9", "k0", "k1", "gp", "sp", "fp", "ra"];
            let regs: Vec<String> = (1..32).map(|i| format!("{}={:08x}", names[i], reg(i))).collect();
            eprintln!("retro: regs {}", regs.join(" "));
            let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
            for (what, a) in [("pc", pc as usize), ("ra", reg(31) as usize), ("sp", reg(29) as usize)] {
                let m = maps.lines().find(|l| {
                    let r = l.split_whitespace().next().unwrap_or("");
                    let (lo, hi) = r.split_once('-').unwrap_or(("0", "0"));
                    let (lo, hi) = (usize::from_str_radix(lo, 16).unwrap_or(0), usize::from_str_radix(hi, 16).unwrap_or(0));
                    a >= lo && a < hi
                });
                eprintln!("retro: {what} {a:#x} in: {}", m.unwrap_or("(unmapped)"));
            }
            let ra = reg(31) as usize & !3;
            if maps.lines().any(|l| l.starts_with("00800000")) || ra >= 0x10000 {
                let words: Vec<String> = (0..12).map(|i| format!("{:08x}", unsafe { *((ra - 32 + 4 * i) as *const u32) })).collect();
                eprintln!("retro: code at ra-32: {}", words.join(" "));
            }
        }
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }

    pub fn install() {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_crash as *const () as usize;
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESETHAND;
            for sig in [libc::SIGILL, libc::SIGSEGV, libc::SIGBUS, libc::SIGFPE] {
                libc::sigaction(sig, &sa, std::ptr::null_mut());
            }
        }
    }
}

fn main() {
    crash::install();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--memtest") {
        memtest();
        return;
    }
    let Some(game) = args.first() else {
        eprintln!("usage: retro <game> [secs]");
        std::process::exit(2);
    };
    // Seconds to run (tests): the second argument, or RETRO_SECS.
    let secs = args.get(1).cloned().or_else(|| std::env::var("RETRO_SECS").ok()).and_then(|s| s.parse().ok());
    if let Err(e) = run(game, secs) {
        eprintln!("retro: {e}");
        std::process::exit(1);
    }
    let _ = std::io::stderr().flush_now();
}

trait FlushNow {
    fn flush_now(&mut self) -> std::io::Result<()>;
}

impl FlushNow for std::io::Stderr {
    fn flush_now(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(self)
    }
}

#[allow(dead_code)]
fn raw_fd(f: &std::fs::File) -> i32 {
    f.as_raw_fd()
}
