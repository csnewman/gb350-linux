//! `log!`: formatted lines into the shared log ring, which the Linux driver
//! prints to the kernel log ("syscore: ...").

use core::fmt::Write;

pub struct Line {
    buf: [u8; 160],
    len: usize,
}

impl Line {
    pub const fn new() -> Line {
        Line { buf: [0; 160], len: 0 }
    }

    pub fn finish(&mut self) {
        if self.len == self.buf.len() {
            self.len -= 1;
        }
        self.buf[self.len] = b'\n';
        crate::shm::log_bytes(&self.buf[..self.len + 1]);
        crate::hw::ring_linux();
    }
}

impl Write for Line {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            if self.len < self.buf.len() {
                self.buf[self.len] = b;
                self.len += 1;
            }
        }
        Ok(())
    }
}

macro_rules! log {
    ($($arg:tt)*) => {{
        let mut line = $crate::log::Line::new();
        let _ = core::fmt::Write::write_fmt(&mut line, format_args!($($arg)*));
        line.finish();
    }};
}
