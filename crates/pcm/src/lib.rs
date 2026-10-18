//! Minimal ALSA playback over raw ioctls (no alsa-lib on the device):
//! interleaved S16LE PCM on `/dev/snd/pcmC0D0p` and integer/enum mixer
//! controls on `/dev/snd/controlC0`. Layouts: include/uapi/sound/asound.h,
//! 32-bit MIPS (long = 4 bytes).

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;

// MIPS _IOC: dir << 29 (none 1, read 2, write 4) | size << 16 | type << 8 | nr
const fn ioc(dir: u32, ty: u8, nr: u32, size: usize) -> u32 {
    (dir << 29) | ((size as u32 & 0x1fff) << 16) | ((ty as u32) << 8) | nr
}

const HW_PARAMS_SIZE: usize = 604;
const SW_PARAMS_SIZE: usize = 104;
const ELEM_VALUE_SIZE: usize = 712;

const PCM_HW_PARAMS: u32 = ioc(6, b'A', 0x11, HW_PARAMS_SIZE);
const PCM_SW_PARAMS: u32 = ioc(6, b'A', 0x13, SW_PARAMS_SIZE);
const PCM_PREPARE: u32 = ioc(1, b'A', 0x40, 0);
const PCM_DROP: u32 = ioc(1, b'A', 0x43, 0);
const PCM_DRAIN: u32 = ioc(1, b'A', 0x44, 0);
const PCM_WRITEI: u32 = ioc(4, b'A', 0x50, 12);
const PCM_DELAY: u32 = ioc(2, b'A', 0x21, 4);
const CTL_ELEM_READ: u32 = ioc(6, b'U', 0x12, ELEM_VALUE_SIZE);
const CTL_ELEM_WRITE: u32 = ioc(6, b'U', 0x13, ELEM_VALUE_SIZE);

// hw_params layout: flags, 3 masks of 32 bytes, 5 reserved masks, then
// intervals of 12 bytes (min, max, flags) from parameter 8 on.
const MASK_ACCESS: usize = 0;
const MASK_FORMAT: usize = 1;
const MASK_SUBFORMAT: usize = 2;
const I_SAMPLE_BITS: usize = 8;
const I_FRAME_BITS: usize = 9;
const I_CHANNELS: usize = 10;
const I_RATE: usize = 11;
const I_PERIOD_SIZE: usize = 13;
const I_PERIODS: usize = 15;
const I_BUFFER_SIZE: usize = 17;
const I_TICK_TIME: usize = 19;
const MASKS_OFF: usize = 4;
const INTERVALS_OFF: usize = 4 + 8 * 32;
const RMASK_OFF: usize = INTERVALS_OFF + 21 * 12;
const ACCESS_RW_INTERLEAVED: u32 = 3;
const FORMAT_S16_LE: u32 = 2;
const INTERVAL_INTEGER: u32 = 1 << 2;

fn ioctl(f: &File, req: u32, arg: *mut u8) -> std::io::Result<i32> {
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), req as _, arg) };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}

fn get32(b: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(b[off..off + 4].try_into().unwrap())
}

/// A playback stream: S16LE interleaved.
pub struct Pcm {
    f: File,
    pub rate: u32,
    pub channels: u32,
    pub period: u32,
    pub buffer: u32,
    /// Underruns recovered from by `write`.
    pub xruns: u32,
}

impl Pcm {
    /// Opens `/dev/snd/pcmC{card}D0p` for `rate`/`channels`, asking for
    /// `period` frames per period and `periods` periods.
    pub fn open(card: u32, rate: u32, channels: u32, period: u32, periods: u32) -> Result<Pcm, String> {
        let path = format!("/dev/snd/pcmC{card}D0p");
        let f = OpenOptions::new().read(true).write(true).open(&path).map_err(|e| format!("{path}: {e}"))?;
        let mut hw = [0u8; HW_PARAMS_SIZE];
        // Everything allowed, then narrow down.
        for m in 0..8 {
            for w in 0..8 {
                put32(&mut hw, MASKS_OFF + m * 32 + w * 4, u32::MAX);
            }
        }
        for i in 0..21 {
            put32(&mut hw, INTERVALS_OFF + i * 12 + 4, u32::MAX);
        }
        let mask = |hw: &mut [u8], m: usize, bit: u32| {
            for w in 0..8 {
                put32(hw, MASKS_OFF + m * 32 + w * 4, 0);
            }
            put32(hw, MASKS_OFF + m * 32 + (bit as usize / 32) * 4, 1 << (bit % 32));
        };
        let set = |hw: &mut [u8], i: usize, v: u32| {
            let o = INTERVALS_OFF + (i - I_SAMPLE_BITS) * 12;
            put32(hw, o, v);
            put32(hw, o + 4, v);
            put32(hw, o + 8, INTERVAL_INTEGER);
        };
        mask(&mut hw, MASK_ACCESS, ACCESS_RW_INTERLEAVED);
        mask(&mut hw, MASK_FORMAT, FORMAT_S16_LE);
        mask(&mut hw, MASK_SUBFORMAT, 0);
        set(&mut hw, I_SAMPLE_BITS, 16);
        set(&mut hw, I_FRAME_BITS, 16 * channels);
        set(&mut hw, I_CHANNELS, channels);
        set(&mut hw, I_RATE, rate);
        set(&mut hw, I_PERIOD_SIZE, period);
        set(&mut hw, I_PERIODS, periods);
        let _ = I_TICK_TIME;
        put32(&mut hw, RMASK_OFF, u32::MAX);
        ioctl(&f, PCM_HW_PARAMS, hw.as_mut_ptr()).map_err(|e| format!("hw_params ({rate} Hz, {channels} ch, {period}x{periods}): {e}"))?;
        let got = |i: usize| get32(&hw, INTERVALS_OFF + (i - I_SAMPLE_BITS) * 12);
        let (period, buffer) = (got(I_PERIOD_SIZE), got(I_BUFFER_SIZE));

        let pcm = Pcm { f, rate, channels, period, buffer, xruns: 0 };
        // Start once the buffer is full; stop (underrun) when it runs dry.
        pcm.set_start(buffer)?;
        ioctl(&pcm.f, PCM_PREPARE, std::ptr::null_mut()).map_err(|e| format!("prepare: {e}"))?;
        Ok(pcm)
    }

    /// Playback starts (and restarts after an underrun) once `frames` are
    /// queued.
    pub fn set_start(&self, frames: u32) -> Result<(), String> {
        let mut sw = [0u8; SW_PARAMS_SIZE];
        let mut boundary = self.buffer;
        while boundary <= (i32::MAX as u32 - self.buffer) / 2 {
            boundary *= 2;
        }
        put32(&mut sw, 12, self.period); // avail_min
        put32(&mut sw, 20, frames.clamp(1, self.buffer)); // start_threshold
        put32(&mut sw, 24, self.buffer); // stop_threshold
        put32(&mut sw, 36, boundary);
        put32(&mut sw, 40, 2 << 16 | 18); // proto 2.0.18
        ioctl(&self.f, PCM_SW_PARAMS, sw.as_mut_ptr()).map(|_| ()).map_err(|e| format!("sw_params: {e}"))
    }

    /// Writes interleaved samples (blocking), recovering from underruns.
    pub fn write(&mut self, samples: &[i16]) -> Result<(), String> {
        let frame = self.channels as usize;
        let mut done = 0;
        while done < samples.len() / frame {
            let rest = &samples[done * frame..];
            let mut x = [0u8; 12];
            put32(&mut x, 4, rest.as_ptr() as u32);
            put32(&mut x, 8, (rest.len() / frame) as u32);
            match ioctl(&self.f, PCM_WRITEI, x.as_mut_ptr()) {
                Ok(_) => done += get32(&x, 0) as usize,
                Err(e) if e.raw_os_error() == Some(libc::EPIPE) => {
                    // Underrun: start over from a prepared state.
                    self.xruns += 1;
                    ioctl(&self.f, PCM_PREPARE, std::ptr::null_mut()).map_err(|e| format!("prepare: {e}"))?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("write: {e}")),
            }
        }
        Ok(())
    }

    /// Frames queued and not yet played (0 when stopped).
    pub fn delay(&self) -> u32 {
        let mut d = [0u8; 4];
        match ioctl(&self.f, PCM_DELAY, d.as_mut_ptr()) {
            Ok(_) => i32::from_ne_bytes(d).max(0) as u32,
            Err(_) => 0,
        }
    }

    /// Plays out what is queued, then stops.
    pub fn drain(&mut self) -> Result<(), String> {
        ioctl(&self.f, PCM_DRAIN, std::ptr::null_mut()).map(|_| ()).map_err(|e| format!("drain: {e}"))
    }
}

impl Drop for Pcm {
    fn drop(&mut self) {
        let _ = ioctl(&self.f, PCM_DROP, std::ptr::null_mut());
    }
}

/// Mixer controls by name on `/dev/snd/controlC{card}`.
pub struct Mixer {
    f: File,
}

impl Mixer {
    pub fn open(card: u32) -> Result<Mixer, String> {
        let path = format!("/dev/snd/controlC{card}");
        OpenOptions::new().read(true).write(true).open(&path).map(|f| Mixer { f }).map_err(|e| format!("{path}: {e}"))
    }

    fn elem(name: &str) -> [u8; ELEM_VALUE_SIZE] {
        let mut v = [0u8; ELEM_VALUE_SIZE];
        put32(&mut v, 4, 2); // iface MIXER
        let n = name.as_bytes();
        v[16..16 + n.len().min(43)].copy_from_slice(&n[..n.len().min(43)]);
        v
    }

    /// First value of an integer, boolean or enumerated control.
    pub fn get(&self, name: &str) -> Result<i32, String> {
        let mut v = Self::elem(name);
        ioctl(&self.f, CTL_ELEM_READ, v.as_mut_ptr()).map_err(|e| format!("{name}: {e}"))?;
        Ok(get32(&v, 72) as i32)
    }

    pub fn set(&self, name: &str, value: i32) -> Result<(), String> {
        let mut v = Self::elem(name);
        put32(&mut v, 72, value as u32);
        put32(&mut v, 76, value as u32); // second channel, if any
        ioctl(&self.f, CTL_ELEM_WRITE, v.as_mut_ptr()).map(|_| ()).map_err(|e| format!("{name}: {e}"))
    }
}

/// Sets the volume as the user sees it, 0-100 %: the hardware gain (x 255 /
/// 100, as the stock firmware maps it), and at 0 % the output muted and the
/// speaker amplifier off; above 0 the speaker follows the second core's
/// automatic policy (off while headphones are plugged in).
pub fn set_volume(pct: u8) -> Result<(), String> {
    let pct = pct.min(100) as i32;
    let m = Mixer::open(0)?;
    m.set("Master Playback Volume", (pct * 255 + 50) / 100)?;
    m.set("Master Playback Switch", (pct > 0) as i32)?;
    // Speaker Mode: 0 Auto, 1 Off, 2 On.
    m.set("Speaker Mode", if pct == 0 { 1 } else { 0 })
}

/// Volume steps of the keys and menus.
pub const VOLUME_STEP: u8 = 5;
