//! The panel through KMS: two XRGB8888 buffers at the native mode, drawn in
//! software and page-flipped on vsync. Can be released (DRM master dropped)
//! so another program (a game) takes the display, and taken back.

use crate::{Canvas, Ui};
use gb350_kms::{Card, Dumb, ModeInfo, XRGB8888};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

pub struct Display {
    card: Card,
    bufs: [Dumb; 2],
    mode: ModeInfo,
    /// The buffer not on screen.
    back: usize,
    flip_pending: bool,
    /// These buffers are on screen (the first frame mode-sets).
    crtc_set: bool,
    pub w: i32,
    pub h: i32,
}

impl Display {
    pub fn open() -> Result<Display, String> {
        let card = Card::open("/dev/dri/card0")?;
        // Whoever had it (the agent, a launcher that just exited) may still
        // be letting go.
        let t = Instant::now();
        while let Err(e) = card.set_master(true) {
            if t.elapsed() > Duration::from_secs(5) {
                return Err(format!("{e} (display not released?)"));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let mode = card.native_mode().ok_or("no display mode")?;
        let (w, h) = (mode.hdisplay as u32, mode.vdisplay as u32);
        let bufs = [card.dumb(w, h, XRGB8888, 32)?, card.dumb(w, h, XRGB8888, 32)?];
        // No mode-set yet: whatever is on screen (the boot logo, the
        // previous program's last frame) stays until the first frame here is
        // ready - mode-setting an empty buffer here flashed black.
        Ok(Display { card, bufs, mode, back: 0, flip_pending: false, crtc_set: false, w: w as i32, h: h as i32 })
    }

    pub fn fd(&self) -> RawFd {
        self.card.as_raw_fd()
    }

    pub fn busy(&self) -> bool {
        self.flip_pending
    }

    /// Draws a frame into the back buffer and queues it for the next vsync.
    /// False (nothing drawn) while the previous flip is still pending.
    pub fn draw(&mut self, ui: &mut Ui, f: impl FnOnce(&mut Canvas)) -> bool {
        if self.flip_pending {
            return false;
        }
        let (w, h) = (self.w, self.h);
        let buf = &mut self.bufs[self.back];
        let stride = buf.pitch as usize / 4;
        let bytes = buf.pixels();
        let px = unsafe { std::slice::from_raw_parts_mut(bytes.as_mut_ptr() as *mut u32, bytes.len() / 4) };
        let mut c = Canvas::new(px, w, h, stride, ui);
        f(&mut c);
        if !self.crtc_set {
            match self.card.set_crtc(&self.bufs[self.back], &self.mode) {
                Ok(()) => {
                    self.crtc_set = true;
                    self.back ^= 1;
                }
                Err(e) => eprintln!("display: mode-set: {e}"),
            }
            return true;
        }
        match self.card.page_flip(&self.bufs[self.back]) {
            Ok(()) => self.flip_pending = true,
            Err(e) => {
                // Show it anyway (mode-set), e.g. if a flip was refused.
                eprintln!("launcher: page flip: {e}");
                let _ = self.card.set_crtc(&self.bufs[self.back], &self.mode);
                self.back ^= 1;
            }
        }
        true
    }

    /// Call when the display fd is readable: a flip completed.
    pub fn flip_done(&mut self) {
        if let Ok(Some(_)) = self.card.poll_flip(0) {
            if self.flip_pending {
                self.flip_pending = false;
                self.back ^= 1;
            }
        }
    }

    /// Waits (briefly) for a pending flip, so the frame is on screen.
    pub fn settle(&mut self) {
        if self.flip_pending {
            let _ = self.card.poll_flip(200);
            self.flip_pending = false;
            self.back ^= 1;
        }
    }

    /// A copy of what is on screen (development screenshots).
    pub fn front(&mut self) -> Vec<u32> {
        self.settle();
        let (w, h) = (self.w as usize, self.h as usize);
        let buf = &mut self.bufs[self.back ^ 1];
        let stride = buf.pitch as usize / 4;
        let bytes = buf.pixels();
        let px = unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const u32, bytes.len() / 4) };
        (0..h).flat_map(|y| px[y * stride..y * stride + w].iter().copied()).collect()
    }

    /// Gives the display up (the last frame stays on screen).
    pub fn release(&mut self) {
        self.settle();
        if let Err(e) = self.card.set_master(false) {
            eprintln!("launcher: drop master: {e}");
        }
    }

    /// Takes the display back and shows the front buffer again.
    pub fn take(&mut self) -> Result<(), String> {
        self.card.set_master(true)?;
        let _ = self.card.set_enum(self.card.planes[0], gb350_kms::OBJ_PLANE, "SCALING_FILTER", "Default");
        if !self.crtc_set {
            return Ok(()); // nothing drawn yet: the next draw mode-sets
        }
        self.card.set_crtc(&self.bufs[self.back ^ 1], &self.mode)
    }
}
