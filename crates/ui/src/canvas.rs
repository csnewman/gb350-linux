//! A software canvas over an XRGB8888 pixel buffer, or a premultiplied
//! ARGB8888 one (an overlay plane: start from `clear`, colours are opaque,
//! `shade` makes translucent areas). Colours are 0xRRGGBB.

use crate::{blend, Face, Mask, MaskBuilder, Ui};

const OPAQUE: u32 = 0xff00_0000;

pub struct Canvas<'a> {
    px: &'a mut [u32],
    stride: usize,
    pub w: i32,
    pub h: i32,
    pub ui: &'a mut Ui,
}

impl<'a> Canvas<'a> {
    /// `px` holds `h` rows of `stride` pixels, `w` of them visible.
    pub fn new(px: &'a mut [u32], w: i32, h: i32, stride: usize, ui: &'a mut Ui) -> Canvas<'a> {
        Canvas { px, stride, w, h, ui }
    }

    /// Clips (x, y, w, h) to the canvas: (x0, y0, x1, y1), possibly empty.
    fn clip(&self, x: i32, y: i32, w: i32, h: i32) -> (i32, i32, i32, i32) {
        (x.max(0), y.max(0), (x + w).min(self.w), (y + h).min(self.h))
    }

    /// Fully transparent (overlay planes).
    pub fn clear(&mut self) {
        self.px.fill(0);
    }

    pub fn fill(&mut self, c: u32) {
        let (w, h) = (self.w, self.h);
        self.rect(0, 0, w, h, c);
    }

    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: u32) {
        let c = c | OPAQUE;
        let (x0, y0, x1, y1) = self.clip(x, y, w, h);
        for yy in y0..y1 {
            let row = yy as usize * self.stride;
            self.px[row + x0 as usize..row + x1.max(x0) as usize].fill(c);
        }
    }

    /// `c` over the area at opacity `a` (0..=255).
    pub fn shade(&mut self, x: i32, y: i32, w: i32, h: i32, c: u32, a: u32) {
        let c = c | OPAQUE;
        let (x0, y0, x1, y1) = self.clip(x, y, w, h);
        for yy in y0..y1 {
            let row = yy as usize * self.stride;
            for p in &mut self.px[row + x0 as usize..row + x1.max(x0) as usize] {
                *p = blend(*p, c, a);
            }
        }
    }

    /// Blits coverage `m` in colour `c` with its top-left at (x, y),
    /// optionally mirrored.
    fn draw_mask(&mut self, m: &Mask, x: i32, y: i32, c: u32, flip_x: bool, flip_y: bool) {
        for my in 0..m.h {
            let yy = y + my;
            if yy < 0 || yy >= self.h {
                continue;
            }
            let sy = if flip_y { m.h - 1 - my } else { my };
            for mx in 0..m.w {
                let xx = x + mx;
                if xx < 0 || xx >= self.w {
                    continue;
                }
                let sx = if flip_x { m.w - 1 - mx } else { mx };
                let a = m.a[(sy * m.w + sx) as usize] as u32;
                if a != 0 {
                    let p = &mut self.px[yy as usize * self.stride + xx as usize];
                    *p = blend(*p, c, a);
                }
            }
        }
    }

    /// The mask cached under `key` (built by `build` once), at (x, y).
    pub fn icon(&mut self, key: &str, x: i32, y: i32, c: u32, build: impl FnOnce() -> Mask) {
        let c = c | OPAQUE;
        let m = self.ui.mask(key, build) as *const Mask;
        // The mask lives in `ui`, which the blit does not touch.
        self.draw_mask(unsafe { &*m }, x, y, c, false, false);
    }

    /// A filled rectangle with corners of radius `r`.
    pub fn round_rect(&mut self, x: i32, y: i32, w: i32, h: i32, r: i32, c: u32) {
        let c = c | OPAQUE;
        let r = r.min(w / 2).min(h / 2);
        if r <= 0 {
            return self.rect(x, y, w, h, c);
        }
        self.rect(x + r, y, w - 2 * r, h, c);
        self.rect(x, y + r, r, h - 2 * r, c);
        self.rect(x + w - r, y + r, r, h - 2 * r, c);
        let m = self.ui.mask(&format!("corner{r}"), || Mask::corner(r)) as *const Mask;
        let m = unsafe { &*m };
        self.draw_mask(m, x, y, c, false, false);
        self.draw_mask(m, x + w - r, y, c, true, false);
        self.draw_mask(m, x, y + h - r, c, false, true);
        self.draw_mask(m, x + w - r, y + h - r, c, true, true);
    }

    /// A rounded-rectangle outline `t` thick (cached per size: for pills and
    /// small boxes).
    pub fn round_rect_stroke(&mut self, x: i32, y: i32, w: i32, h: i32, r: f32, t: f32, c: u32) {
        let key = format!("rrs{w}x{h}r{r}t{t}");
        self.icon(&key, x, y, c, || MaskBuilder::new(w, h).rrect_stroke(0.0, 0.0, w as f32, h as f32, r, t).build());
    }

    /// A filled circle of diameter `d` with its top-left at (x, y).
    pub fn disc(&mut self, x: i32, y: i32, d: i32, c: u32) {
        let r = d as f32 / 2.0;
        self.icon(&format!("disc{d}"), x, y, c, || MaskBuilder::new(d, d).disc(r, r, r).build());
    }

    /// A circle outline `t` thick, diameter `d`, top-left at (x, y).
    pub fn ring(&mut self, x: i32, y: i32, d: i32, t: f32, c: u32) {
        let r = d as f32 / 2.0;
        self.icon(&format!("ring{d}t{t}"), x, y, c, || MaskBuilder::new(d, d).ring(r, r, r, t).build());
    }

    /// Copies an image (`w` x `h` pixels, row-major) to (x, y).
    pub fn image(&mut self, x: i32, y: i32, w: i32, h: i32, img: &[u32]) {
        let (x0, y0, x1, y1) = self.clip(x, y, w, h);
        if x1 <= x0 {
            return;
        }
        for yy in y0..y1 {
            let src = ((yy - y) * w + (x0 - x)) as usize;
            let row = yy as usize * self.stride;
            for (d, p) in self.px[row + x0 as usize..row + x1 as usize].iter_mut().zip(&img[src..src + (x1 - x0) as usize]) {
                *d = p | OPAQUE;
            }
        }
    }

    /// Draws `s` with its baseline at `baseline`, `spacing` px after each
    /// character; returns the width drawn.
    pub fn text_ls(&mut self, face: Face, size: u16, x: i32, baseline: i32, c: u32, s: &str, spacing: i32) -> i32 {
        let c = c | OPAQUE;
        let mut pen = x as f32;
        let mut prev = None;
        for ch in s.chars() {
            if let Some(p) = prev {
                pen += self.ui.fonts.kern(face, size, p, ch);
            }
            prev = Some(ch);
            let g = self.ui.fonts.glyph(face, size, ch) as *const crate::text::Glyph;
            let g = unsafe { &*g };
            let (gx, gy) = (pen.round() as i32 + g.left, baseline - g.top);
            for my in 0..g.h {
                let yy = gy + my;
                if yy < 0 || yy >= self.h {
                    continue;
                }
                let row = yy as usize * self.stride;
                for mx in 0..g.w {
                    let xx = gx + mx;
                    if xx < 0 || xx >= self.w {
                        continue;
                    }
                    let a = g.bmp[(my * g.w + mx) as usize] as u32;
                    if a != 0 {
                        let p = &mut self.px[row + xx as usize];
                        *p = blend(*p, c, a);
                    }
                }
            }
            pen += g.adv + spacing as f32;
        }
        (pen - x as f32).round() as i32
    }

    pub fn text(&mut self, face: Face, size: u16, x: i32, baseline: i32, c: u32, s: &str) -> i32 {
        self.text_ls(face, size, x, baseline, c, s, 0)
    }

    /// Text vertically centred on `cy` (as a CSS flex row centres it).
    pub fn text_mid(&mut self, face: Face, size: u16, x: i32, cy: i32, c: u32, s: &str) -> i32 {
        let b = self.ui.baseline_center(face, size, cy);
        self.text(face, size, x, b, c, s)
    }

    /// Like `text_mid`, right-aligned to end at `right`.
    pub fn text_mid_right(&mut self, face: Face, size: u16, right: i32, cy: i32, c: u32, s: &str) -> i32 {
        let w = self.ui.measure(face, size, s, 0);
        self.text_mid(face, size, right - w, cy, c, s)
    }

    /// Like `text_mid`, centred horizontally on `cx`.
    pub fn text_mid_center(&mut self, face: Face, size: u16, cx: i32, cy: i32, c: u32, s: &str) -> i32 {
        let w = self.ui.measure(face, size, s, 0);
        self.text_mid(face, size, cx - w / 2, cy, c, s)
    }
}
