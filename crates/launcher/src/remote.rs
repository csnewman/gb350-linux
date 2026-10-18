//! Development control: a FIFO at /tmp/launcher.ctl takes lines
//! `key <name>` (press a button: up, a, l2, start, ...), `shot <file>` (save
//! the screen as a PPM) and `launch <game path>|KEY=value ...` (start a game
//! with extra emulator settings, e.g. RETRO_SCREEN), e.g. over USB:
//! `gbctl sh 'echo "key down" > /tmp/launcher.ctl'`.

use std::ffi::CString;
use std::os::fd::RawFd;

pub const PATH: &str = "/tmp/launcher.ctl";

pub struct Remote {
    fd: RawFd,
    buf: Vec<u8>,
}

impl Remote {
    pub fn open() -> Option<Remote> {
        let p = CString::new(PATH).ok()?;
        unsafe {
            libc::unlink(p.as_ptr());
            if libc::mkfifo(p.as_ptr(), 0o600) != 0 {
                return None;
            }
            // Read-write, so it never reports end-of-file between writers.
            let fd = libc::open(p.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC);
            (fd >= 0).then_some(Remote { fd, buf: vec![] })
        }
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// Complete lines received so far.
    pub fn lines(&mut self) -> Vec<String> {
        let mut b = [0u8; 512];
        loop {
            let n = unsafe { libc::read(self.fd, b.as_mut_ptr() as *mut _, b.len()) };
            if n <= 0 {
                break;
            }
            self.buf.extend_from_slice(&b[..n as usize]);
        }
        let mut out = vec![];
        while let Some(i) = self.buf.iter().position(|&c| c == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            out.push(String::from_utf8_lossy(&line).trim().to_string());
        }
        out
    }
}

/// Writes XRGB8888 pixels as a binary PPM.
pub fn write_ppm(path: &str, px: &[u32], w: usize, h: usize) -> std::io::Result<()> {
    let mut data = format!("P6\n{w} {h}\n255\n").into_bytes();
    data.reserve(w * h * 3);
    for p in &px[..w * h] {
        data.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
    }
    std::fs::write(path, data)
}
