//! Audio rate conversion for cores whose rate the sound driver can't play
//! (it takes 16, 22.05, 24, 32, 44.1 and 48 kHz; the SNES core makes
//! ~32040 Hz, Game Boy 32768, ...): linear interpolation in 32.32 fixed
//! point (no FPU per sample), stereo. When the display paces the emulator
//! the ratio is also nudged each frame to keep the device buffer half full
//! (`set_ratio`).

pub const RATES: [u32; 6] = [16000, 22050, 24000, 32000, 44100, 48000];

/// The rate to open the device at for a core's `rate`: itself if the
/// driver takes it, else 48 kHz.
pub fn device_rate(rate: u32) -> u32 {
    RATES.into_iter().find(|&r| r.abs_diff(rate) <= 2).unwrap_or(48000)
}

pub struct Resampler {
    /// Input frames per output frame, 32.32.
    step: u64,
    /// Position between `prev` and the next input frame, 0.32.
    pos: u64,
    prev: [i32; 2],
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Resampler {
        Resampler { step: ((from as u64) << 32) / to as u64, pos: 0, prev: [0, 0] }
    }

    /// Input frames per output frame, e.g. 32040.5 / 48000.
    pub fn set_ratio(&mut self, ratio: f64) {
        self.step = (ratio * (1u64 << 32) as f64) as u64;
    }

    /// Appends `input` (interleaved stereo) converted, to `out`.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        const ONE: u64 = 1 << 32;
        for f in input.chunks_exact(2) {
            let cur = [f[0] as i32, f[1] as i32];
            while self.pos < ONE {
                let frac = self.pos as i64;
                for ch in 0..2 {
                    let d = (cur[ch] - self.prev[ch]) as i64;
                    out.push((self.prev[ch] as i64 + ((d * frac) >> 32)) as i16);
                }
                self.pos += self.step;
            }
            self.pos -= ONE;
            self.prev = cur;
        }
    }
}
