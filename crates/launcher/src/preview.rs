//! Screenshots for the preview panel: decoded and scaled on a worker thread
//! (a 320x240 PNG takes a few tens of ms here), newest request wins, and a
//! byte on a pipe wakes the UI loop when one is ready.

use std::collections::HashMap;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// A scaled image, XRGB8888.
pub struct Image {
    pub w: i32,
    pub h: i32,
    pub px: Vec<u32>,
}

pub struct Previews {
    req: Sender<PathBuf>,
    done: Arc<Mutex<Vec<(PathBuf, Option<Arc<Image>>)>>>,
    wake_rd: RawFd,
    cache: HashMap<PathBuf, Option<Arc<Image>>>,
    order: Vec<PathBuf>,
    pending: Option<PathBuf>,
}

const CACHE: usize = 48;

impl Previews {
    /// Images are scaled to fit `max_w` x `max_h`.
    pub fn new(max_w: u32, max_h: u32) -> Previews {
        let (tx, rx) = channel::<PathBuf>();
        let done = Arc::new(Mutex::new(vec![]));
        let mut fds = [0; 2];
        unsafe {
            libc::pipe(fds.as_mut_ptr());
            for fd in fds {
                libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        let (wake_rd, wake_wr) = (fds[0], fds[1]);
        let d = done.clone();
        std::thread::Builder::new()
            .name("preview".into())
            .spawn(move || worker(rx, d, wake_wr, max_w, max_h))
            .expect("preview thread");
        Previews { req: tx, done, wake_rd, cache: HashMap::new(), order: vec![], pending: None }
    }

    /// Forgets cached images (the card's files may have changed).
    pub fn clear(&mut self) {
        self.cache.clear();
        self.order.clear();
    }

    /// A screenshot is still loading.
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Readable when results arrived (then call `collect`).
    pub fn fd(&self) -> RawFd {
        self.wake_rd
    }

    /// Takes finished images into the cache; true if any arrived.
    pub fn collect(&mut self) -> bool {
        let mut b = [0u8; 64];
        while unsafe { libc::read(self.wake_rd, b.as_mut_ptr() as *mut _, b.len()) } > 0 {}
        let got: Vec<_> = std::mem::take(&mut *self.done.lock().unwrap());
        let any = !got.is_empty();
        for (p, img) in got {
            if self.pending.as_ref() == Some(&p) {
                self.pending = None;
            }
            self.order.push(p.clone());
            self.cache.insert(p, img);
        }
        while self.order.len() > CACHE {
            let old = self.order.remove(0);
            self.cache.remove(&old);
        }
        any
    }

    /// The image for `path`: Some(Some) ready, Some(None) none exists, None
    /// still loading (a request is made).
    pub fn get(&mut self, path: &PathBuf) -> Option<Option<Arc<Image>>> {
        if let Some(i) = self.cache.get(path) {
            return Some(i.clone());
        }
        if self.pending.as_ref() != Some(path) {
            self.pending = Some(path.clone());
            let _ = self.req.send(path.clone());
        }
        None
    }
}

fn worker(rx: Receiver<PathBuf>, done: Arc<Mutex<Vec<(PathBuf, Option<Arc<Image>>)>>>, wake: RawFd, max_w: u32, max_h: u32) {
    while let Ok(mut path) = rx.recv() {
        // Only the newest request matters (the cursor moved on).
        while let Ok(p) = rx.try_recv() {
            path = p;
        }
        let img = load(&path, max_w, max_h).map(Arc::new);
        done.lock().unwrap().push((path, img));
        unsafe { libc::write(wake, b"x".as_ptr() as *const _, 1) };
    }
}

fn load(path: &PathBuf, max_w: u32, max_h: u32) -> Option<Image> {
    let file = std::fs::File::open(path).ok()?;
    let mut dec = png::Decoder::new(std::io::BufReader::new(file));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let ch = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Indexed => return None,
    };
    let px = |x: u32, y: u32| -> [u32; 3] {
        let i = ((y * w + x) * ch) as usize;
        if ch < 3 {
            let v = buf[i] as u32;
            [v, v, v]
        } else {
            [buf[i] as u32, buf[i + 1] as u32, buf[i + 2] as u32]
        }
    };
    // Fit, keeping the aspect ratio; box-filter down (area average).
    let (dw, dh) = if w * max_h > h * max_w { (max_w, (h * max_w / w).max(1)) } else { ((w * max_h / h).max(1), max_h) };
    let mut out = Vec::with_capacity((dw * dh) as usize);
    for y in 0..dh {
        let (y0, y1) = (y * h / dh, ((y + 1) * h / dh).max(y * h / dh + 1));
        for x in 0..dw {
            let (x0, x1) = (x * w / dw, ((x + 1) * w / dw).max(x * w / dw + 1));
            let mut s = [0u32; 3];
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let p = px(sx, sy);
                    s[0] += p[0];
                    s[1] += p[1];
                    s[2] += p[2];
                }
            }
            let n = (y1 - y0) * (x1 - x0);
            out.push((s[0] / n) << 16 | (s[1] / n) << 8 | s[2] / n);
        }
    }
    Some(Image { w: dw as i32, h: dh as i32, px: out })
}
