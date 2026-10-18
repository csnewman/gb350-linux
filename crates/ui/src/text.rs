//! TrueType text: the launcher's typefaces, rasterised per (face, size,
//! character) on first use and cached. ab_glyph reads glyph outlines only
//! when needed (parsing whole fonts up front took over a second here).

use ab_glyph::{Font as _, FontRef, PxScale, ScaleFont};
use std::collections::HashMap;

/// The embedded faces: IBM Plex Sans for text, Chakra Petch for display,
/// IBM Plex Mono for logs.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Face {
    Sans,
    SansBold,
    Display,
    DisplayBold,
    Mono,
}

pub(crate) struct Glyph {
    pub w: i32,
    pub h: i32,
    /// Bitmap origin relative to the pen: x offset, and top above the
    /// baseline.
    pub left: i32,
    pub top: i32,
    pub adv: f32,
    pub bmp: Box<[u8]>,
}

pub(crate) struct Fonts {
    faces: [FontRef<'static>; 5],
    glyphs: HashMap<(Face, u16, char), Glyph>,
    lines: HashMap<(Face, u16), (i32, i32)>,
}

const SANS: &[u8] = include_bytes!("../fonts/IBMPlexSans-Regular.ttf");
const SANS_BOLD: &[u8] = include_bytes!("../fonts/IBMPlexSans-SemiBold.ttf");
const DISPLAY: &[u8] = include_bytes!("../fonts/ChakraPetch-SemiBold.ttf");
const DISPLAY_BOLD: &[u8] = include_bytes!("../fonts/ChakraPetch-Bold.ttf");
const MONO: &[u8] = include_bytes!("../fonts/IBMPlexMono-Regular.ttf");

fn load(data: &'static [u8]) -> FontRef<'static> {
    FontRef::try_from_slice(data).expect("embedded font")
}

impl Fonts {
    pub fn new() -> Fonts {
        Fonts { faces: [load(SANS), load(SANS_BOLD), load(DISPLAY), load(DISPLAY_BOLD), load(MONO)], glyphs: HashMap::new(), lines: HashMap::new() }
    }

    fn font(&self, face: Face) -> &FontRef<'static> {
        &self.faces[face as usize]
    }

    /// `size` is the em size in pixels (as CSS font-size).
    fn scale(&self, face: Face, size: u16) -> PxScale {
        let f = self.font(face);
        let em = f.units_per_em().unwrap_or(1000.0);
        PxScale::from(size as f32 * f.height_unscaled() / em)
    }

    pub fn glyph(&mut self, face: Face, size: u16, c: char) -> &Glyph {
        let key = (face, size, c);
        if !self.glyphs.contains_key(&key) {
            let scale = self.scale(face, size);
            let font = self.font(face);
            let id = font.glyph_id(c);
            let adv = font.as_scaled(scale).h_advance(id);
            let mut g = Glyph { w: 0, h: 0, left: 0, top: 0, adv, bmp: Box::new([]) };
            if let Some(o) = font.outline_glyph(id.with_scale(scale)) {
                let b = o.px_bounds();
                let (w, h) = (b.width() as i32, b.height() as i32);
                let mut bmp = vec![0u8; (w * h) as usize];
                o.draw(|x, y, cov| {
                    if let Some(p) = bmp.get_mut((y as i32 * w + x as i32) as usize) {
                        *p = (cov * 255.0 + 0.5).min(255.0) as u8;
                    }
                });
                g = Glyph { w, h, left: b.min.x as i32, top: -b.min.y as i32, adv, bmp: bmp.into_boxed_slice() };
            }
            self.glyphs.insert(key, g);
        }
        &self.glyphs[&key]
    }

    pub fn kern(&self, face: Face, size: u16, a: char, b: char) -> f32 {
        let font = self.font(face);
        font.as_scaled(self.scale(face, size)).kern(font.glyph_id(a), font.glyph_id(b))
    }

    /// (ascent, descent) in pixels; descent is negative.
    pub fn line(&mut self, face: Face, size: u16) -> (i32, i32) {
        if let Some(l) = self.lines.get(&(face, size)) {
            return *l;
        }
        let sf = self.font(face).as_scaled(self.scale(face, size));
        let l = (sf.ascent().round() as i32, sf.descent().round() as i32);
        self.lines.insert((face, size), l);
        l
    }

    pub fn measure(&mut self, face: Face, size: u16, s: &str, spacing: i32) -> i32 {
        let mut pen = 0.0f32;
        let mut prev = None;
        for c in s.chars() {
            if let Some(p) = prev {
                pen += self.kern(face, size, p, c);
            }
            pen += self.glyph(face, size, c).adv + spacing as f32;
            prev = Some(c);
        }
        pen.round() as i32
    }

    pub fn ellipsize(&mut self, face: Face, size: u16, s: &str, max_w: i32) -> String {
        if self.measure(face, size, s, 0) <= max_w {
            return s.to_string();
        }
        let ell = self.measure(face, size, "…", 0);
        let mut out = String::new();
        let mut w = 0.0f32;
        for c in s.chars() {
            let a = self.glyph(face, size, c).adv;
            if (w + a).round() as i32 + ell > max_w {
                break;
            }
            w += a;
            out.push(c);
        }
        out.truncate(out.trim_end().len());
        out.push('…');
        out
    }

    pub fn wrap(&mut self, face: Face, size: u16, s: &str, max_w: i32, max_lines: usize) -> Vec<String> {
        let mut lines: Vec<String> = vec![];
        let mut cur = String::new();
        let words: Vec<&str> = s.split(' ').filter(|w| !w.is_empty()).collect();
        let mut i = 0;
        while i < words.len() {
            let trial = if cur.is_empty() { words[i].to_string() } else { format!("{cur} {}", words[i]) };
            if self.measure(face, size, &trial, 0) <= max_w || cur.is_empty() {
                cur = trial;
                i += 1;
                continue;
            }
            lines.push(std::mem::take(&mut cur));
            if lines.len() == max_lines {
                break;
            }
        }
        if !cur.is_empty() && lines.len() < max_lines {
            lines.push(cur);
        } else if i < words.len() {
            // Out of lines: the rest goes on the last one, ellipsized.
            let last = lines.pop().unwrap_or_default();
            let rest = format!("{last} {}", words[i..].join(" "));
            lines.push(rest);
        }
        if let Some(last) = lines.last_mut() {
            *last = self.ellipsize(face, size, last, max_w);
        }
        lines
    }
}
