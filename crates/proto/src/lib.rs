//! Wire protocol spoken over the USB bulk pipe between `gbctl` (host) and
//! `gb350-agent` (device).
//!
//! The transport is a byte stream: USB transfer boundaries carry no meaning, so
//! both sides accumulate bytes in a [`FrameReader`] and split them into frames.
//!
//! Frame layout (little-endian):
//!
//! ```text
//! 0  u32 magic  "GB35"
//! 4  u16 kind   request (< 0x8000) or response (>= 0x8000)
//! 6  u16 flags  FLAG_END on the last response frame for a request
//! 8  u32 id     request id, echoed in every response frame
//! 12 u32 len    payload length
//! 16 ...        payload
//! ```

pub const MAGIC: u32 = u32::from_le_bytes(*b"GB35");
pub const HEADER_LEN: usize = 16;
/// Upper bound on a single frame's payload; larger data is split across frames.
pub const MAX_PAYLOAD: usize = 1 << 20;
/// Payload size used when splitting bulk data (file/memory reads, exec output).
pub const CHUNK: usize = 16 * 1024 - HEADER_LEN;

pub const FLAG_END: u16 = 1;

/// USB identity of the agent's gadget (pid.codes test VID/PID).
pub const USB_VID: u16 = 0x1209;
pub const USB_PID: u16 = 0x0001;

/// Requests (host -> device).
pub mod req {
    /// Payload echoed back in an `OK` response.
    pub const PING: u16 = 0x0001;
    /// Returns a text summary of the device state.
    pub const INFO: u16 = 0x0002;
    /// `u32 timeout_secs` (0 = none) + shell command (UTF-8), run via `/bin/sh -c`.
    /// Responds with `OUT` frames, then an `EXIT` frame.
    pub const EXEC: u16 = 0x0003;
    /// `u64 offset, u32 max_len` + path. Responds with `DATA` frames, then `OK`.
    pub const READ_FILE: u16 = 0x0004;
    /// `u64 offset, u8 flags, u16 path_len` + path + data. Responds `OK`.
    /// Flags: bit 0 truncate (first chunk); bit 7 streamed: the file stays
    /// open between chunks, only the chunk with bit 1 (last) is answered
    /// (with the first error of the stream, if any) and synced. Without bit
    /// 7 every chunk is answered and synced (slow on the SD card).
    pub const WRITE_FILE: u16 = 0x0005;
    /// `u32 phys_addr, u32 len` (both 4-byte aligned). Word reads via /dev/mem.
    pub const READ_MEM: u16 = 0x0006;
    /// `u32 phys_addr, u8 width (1|2|4), u32 value`.
    pub const WRITE_MEM: u16 = 0x0007;
    /// Text (UTF-8) printed on the device screen.
    pub const SAY: u16 = 0x0008;
    /// Sync filesystems and restart: empty payload = kexec into the kernel on
    /// the SD card (replies `OK` with the image name first); `[1]` = hard
    /// reset (watchdog; the GB350 powers off); `[2]` + "kernel\0dtb" = kexec
    /// into those files.
    pub const REBOOT: u16 = 0x0009;
    // 0x000a: retired (raw ioctl, for the vendor kernel's drivers).
    /// No payload: responds `OK` with text describing the console's display.
    /// `[1]`: give the display up (DRM master) for another program; `[2]`:
    /// take it back.
    pub const SCREEN: u16 = 0x000b;
    /// Path of a new agent binary on the device; the agent execs it in place.
    /// Responds `OK` before exec'ing.
    pub const UPGRADE: u16 = 0x000c;
    /// Buttons held now: responds `OK` with their names, space-separated
    /// (plus `[headphones]` while plugged in).
    pub const KEYS: u16 = 0x000d;
    /// `u32 freq_hz, u32 duration_ms, u8 volume (0-100)[, u16 amplitude[, u8 square]]`:
    /// play a sine (or square) tone; amplitude is the peak sample value.
    /// Responds `OK` with a text report once played.
    pub const TONE: u16 = 0x000e;
    /// `u8 volume, u8 attenuation_db` + path of a PCM S16LE WAV file on the
    /// device: play it.
    pub const PLAY_WAV: u16 = 0x000f;
    /// `u32 seconds`: after 5 s, switch the USB-C port to host mode for that
    /// long, logging devices that enumerate, then restore the gadget. The USB
    /// link to the host drops meanwhile.
    pub const HOST_TEST: u16 = 0x0010;
}

/// Responses (device -> host).
pub mod resp {
    pub const OK: u16 = 0x8000;
    /// UTF-8 error message.
    pub const ERR: u16 = 0x8001;
    pub const DATA: u16 = 0x8002;
    /// Combined stdout/stderr chunk from `EXEC`.
    pub const OUT: u16 = 0x8003;
    /// `i32` exit status (negative = killed by signal / timeout).
    pub const EXIT: u16 = 0x8004;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: u16,
    pub flags: u16,
    pub id: u32,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(kind: u16, flags: u16, id: u32, payload: Vec<u8>) -> Self {
        Self { kind, flags, id, payload }
    }

    pub fn is_end(&self) -> bool {
        self.flags & FLAG_END != 0
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&self.kind.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }
}

#[derive(Debug)]
pub enum DecodeError {
    BadMagic(u32),
    TooLarge(usize),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::BadMagic(m) => write!(f, "bad frame magic {m:#010x}"),
            DecodeError::TooLarge(n) => write!(f, "frame payload too large ({n} bytes)"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Reassembles frames from arbitrarily chunked input.
#[derive(Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Returns the next complete frame, if one is buffered.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, DecodeError> {
        if self.buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let magic = u32_at(&self.buf, 0);
        if magic != MAGIC {
            // Drop the garbage so the caller can resynchronise if it wants to.
            self.buf.clear();
            return Err(DecodeError::BadMagic(magic));
        }
        let len = u32_at(&self.buf, 12) as usize;
        if len > MAX_PAYLOAD {
            self.buf.clear();
            return Err(DecodeError::TooLarge(len));
        }
        if self.buf.len() < HEADER_LEN + len {
            return Ok(None);
        }
        let frame = Frame {
            kind: u16_at(&self.buf, 4),
            flags: u16_at(&self.buf, 6),
            id: u32_at(&self.buf, 8),
            payload: self.buf[HEADER_LEN..HEADER_LEN + len].to_vec(),
        };
        self.buf.drain(..HEADER_LEN + len);
        Ok(Some(frame))
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

/// Cursor for decoding request/response payload fields.
pub struct Payload<'a> {
    data: &'a [u8],
}

impl<'a> Payload<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.data.len() < n {
            return None;
        }
        let (head, tail) = self.data.split_at(n);
        self.data = tail;
        Some(head)
    }

    pub fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    pub fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }

    pub fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        self.take(n)
    }

    pub fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.data)
    }
}

fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_split_input() {
        let a = Frame::new(req::PING, 0, 7, b"hello".to_vec());
        let b = Frame::new(resp::OK, FLAG_END, 7, vec![0; 40_000]);
        let mut wire = a.encode();
        wire.extend(b.encode());

        let mut r = FrameReader::default();
        let mut got = Vec::new();
        for chunk in wire.chunks(511) {
            r.push(chunk);
            while let Some(f) = r.next_frame().unwrap() {
                got.push(f);
            }
        }
        assert_eq!(got, vec![a, b]);
    }

    #[test]
    fn bad_magic() {
        let mut r = FrameReader::default();
        r.push(&[0u8; 16]);
        assert!(r.next_frame().is_err());
    }
}
