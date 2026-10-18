//! Test tones and WAV playback through the GB350 ALSA driver (`snd-hc16xx`, card
//! 0) with `gb350-pcm`; the volume (0-100) maps onto the hardware gain as the
//! stock firmware does (x 255 / 100).

use std::time::Instant;

const RATE: u32 = 44_100;
const CHANNELS: u8 = 2;

/// Plays a sine (or, with `square`, a +-amplitude square) tone; `volume` is
/// 0-100.
pub fn tone(freq: u32, ms: u32, volume: u8, amplitude: u16, square: bool) -> Result<String, String> {
    let frames = (RATE as u64 * ms as u64 / 1000) as usize;
    let a = amplitude.min(32767) as f32;
    let mut pcm = Vec::with_capacity(frames * 2);
    for n in 0..frames {
        let t = n as f32 / RATE as f32;
        let phase = (t * freq as f32 * std::f32::consts::TAU).sin();
        let s = if square { if phase >= 0.0 { a } else { -a } } else { phase * a } as i16;
        pcm.push(s);
        pcm.push(s);
    }
    play(pcm, RATE, CHANNELS, volume)
}

/// Plays a PCM S16LE WAV file from the device's filesystem, attenuated by
/// `atten_db`. (The stock menu mixes its sounds at about -6 dB.)
pub fn play_wav(path: &str, volume: u8, atten_db: u8) -> Result<String, String> {
    let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let u16_at = |o: usize| u16::from_le_bytes([data[o], data[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
    if data.len() < 44 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err(format!("{path}: not a WAV file"));
    }
    // Walk chunks for "fmt " and "data".
    let (mut fmt, mut body) = (None, None);
    let mut off = 12;
    while off + 8 <= data.len() {
        let id = &data[off..off + 4];
        let len = u32_at(off + 4) as usize;
        let end = (off + 8 + len).min(data.len());
        match id {
            b"fmt " => fmt = Some(off + 8),
            b"data" => body = Some(&data[off + 8..end]),
            _ => {}
        }
        off += 8 + len + (len & 1);
    }
    let (f, body) = match (fmt, body) {
        (Some(f), Some(b)) => (f, b),
        _ => return Err(format!("{path}: missing fmt/data chunk")),
    };
    let (format, channels, rate, bits) = (u16_at(f), u16_at(f + 2), u32_at(f + 4), u16_at(f + 14));
    if format != 1 || bits != 16 || !(channels == 1 || channels == 2) {
        return Err(format!("{path}: only PCM 16-bit mono/stereo supported (format {format}, {bits} bits, {channels} ch)"));
    }
    let gain = 10f32.powf(-(atten_db as f32) / 20.0);
    let pcm: Vec<i16> = body.chunks_exact(2).map(|b| (i16::from_le_bytes([b[0], b[1]]) as f32 * gain) as i16).collect();
    let r = play(pcm, rate, channels as u8, volume)?;
    Ok(format!("{path}: {rate} Hz {channels} ch, -{atten_db} dB; {r}"))
}

/// Plays interleaved S16 samples.
fn play(mut samples: Vec<i16>, rate: u32, channels: u8, volume: u8) -> Result<String, String> {
    let gain = (volume.min(100) as i32 * 255 + 50) / 100;
    let mixer = gb350_pcm::Mixer::open(0)?;
    mixer.set("Master Playback Volume", gain)?;
    mixer.set("Master Playback Switch", 1)?;
    let mut pcm = gb350_pcm::Pcm::open(0, rate, channels as u32, 1024, 4)?;
    // A little silence after, so the end is not cut by the drain.
    samples.resize(samples.len() + (rate as usize / 10) * channels as usize, 0);
    let t0 = Instant::now();
    pcm.write(&samples)?;
    pcm.drain()?;
    Ok(format!(
        "alsa: {} frames at {rate} Hz, period {} buffer {}, gain {gain}/255, {:.2} s",
        samples.len() / channels as usize,
        pcm.period,
        pcm.buffer,
        t0.elapsed().as_secs_f32()
    ))
}
