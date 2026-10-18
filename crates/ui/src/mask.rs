//! Coverage masks: anti-aliased shapes rendered once (float maths, slow
//! without an FPU) and then blitted in any colour.

/// An 8-bit coverage bitmap.
pub struct Mask {
    pub w: i32,
    pub h: i32,
    pub a: Vec<u8>,
}

/// Builds a mask by combining shapes (each pixel keeps its highest
/// coverage). Coordinates are in pixels, pixel centres at +0.5.
pub struct MaskBuilder {
    w: i32,
    h: i32,
    cov: Vec<f32>,
}

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

/// Signed distance from (px, py) to a rounded rectangle.
fn sd_rrect(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let (hw, hh) = (w / 2.0, h / 2.0);
    let qx = (px - x - hw).abs() - (hw - r);
    let qy = (py - y - hh).abs() - (hh - r);
    let (ox, oy) = (qx.max(0.0), qy.max(0.0));
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

impl MaskBuilder {
    pub fn new(w: i32, h: i32) -> MaskBuilder {
        MaskBuilder { w, h, cov: vec![0.0; (w * h) as usize] }
    }

    fn each(&mut self, f: impl Fn(f32, f32) -> f32) {
        for y in 0..self.h {
            for x in 0..self.w {
                let c = f(x as f32 + 0.5, y as f32 + 0.5);
                let p = &mut self.cov[(y * self.w + x) as usize];
                if c > *p {
                    *p = c;
                }
            }
        }
    }

    /// A filled disc.
    pub fn disc(mut self, cx: f32, cy: f32, r: f32) -> Self {
        self.each(|x, y| clamp01(r - ((x - cx) * (x - cx) + (y - cy) * (y - cy)).sqrt() + 0.5));
        self
    }

    /// A circle outline `t` thick, inside radius `r`.
    pub fn ring(mut self, cx: f32, cy: f32, r: f32, t: f32) -> Self {
        self.each(|x, y| {
            let d = ((x - cx) * (x - cx) + (y - cy) * (y - cy)).sqrt();
            clamp01(r - d + 0.5) - clamp01(r - t - d + 0.5)
        });
        self
    }

    /// A filled rounded rectangle.
    pub fn rrect(mut self, x: f32, y: f32, w: f32, h: f32, r: f32) -> Self {
        self.each(|px, py| clamp01(0.5 - sd_rrect(px, py, x, y, w, h, r)));
        self
    }

    /// A rounded rectangle outline `t` thick, inside the shape.
    pub fn rrect_stroke(mut self, x: f32, y: f32, w: f32, h: f32, r: f32, t: f32) -> Self {
        self.each(|px, py| {
            let d = sd_rrect(px, py, x, y, w, h, r);
            clamp01(0.5 - d) - clamp01(0.5 - (d + t))
        });
        self
    }

    /// A line `t` thick with round caps.
    pub fn line(mut self, x0: f32, y0: f32, x1: f32, y1: f32, t: f32) -> Self {
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len2 = (dx * dx + dy * dy).max(1e-6);
        self.each(|x, y| {
            let s = (((x - x0) * dx + (y - y0) * dy) / len2).clamp(0.0, 1.0);
            let (ex, ey) = (x - x0 - s * dx, y - y0 - s * dy);
            clamp01(t / 2.0 - (ex * ex + ey * ey).sqrt() + 0.5)
        });
        self
    }

    /// A filled polygon (non-zero winding), 4x4 supersampled.
    pub fn poly(mut self, pts: &[(f32, f32)]) -> Self {
        let inside = |x: f32, y: f32| {
            let mut wind = 0i32;
            for i in 0..pts.len() {
                let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                if (a.1 <= y) != (b.1 <= y) {
                    let t = (y - a.1) / (b.1 - a.1);
                    if x < a.0 + t * (b.0 - a.0) {
                        wind += if b.1 > a.1 { 1 } else { -1 };
                    }
                }
            }
            wind != 0
        };
        self.each(|x, y| {
            let mut n = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    if inside(x - 0.5 + (sx as f32 + 0.5) / 4.0, y - 0.5 + (sy as f32 + 0.5) / 4.0) {
                        n += 1;
                    }
                }
            }
            n as f32 / 16.0
        });
        self
    }

    pub fn build(self) -> Mask {
        Mask { w: self.w, h: self.h, a: self.cov.iter().map(|c| (c * 255.0 + 0.5) as u8).collect() }
    }
}

impl Mask {
    /// Quarter-circle coverage for a rounded corner of radius `r` (top-left
    /// orientation; mirrored for the others).
    pub fn corner(r: i32) -> Mask {
        let rf = r as f32;
        MaskBuilder::new(r, r).disc(rf, rf, rf).build()
    }
}
