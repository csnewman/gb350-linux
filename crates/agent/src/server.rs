//! Serves `gb350-proto` requests over the USB bulk pipe (FunctionFS
//! endpoint files, see `usb`).

use crate::{kexec, log, sys, usb};
use gb350_proto::{req, resp, Frame, FrameReader, Payload, CHUNK, FLAG_END};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const READ_BUF: usize = 16 * 1024;
const MAX_FILE_READ: u64 = 64 << 20;
const MAX_MEM_READ: u32 = 4 << 20;

struct Conn {
    rx: File,
    tx: File,
    /// The file a streamed WRITE_FILE is filling (path, handle).
    upload: Option<(String, File)>,
    /// First error of the current streamed upload, reported on its last
    /// chunk (the others get no reply).
    upload_err: Option<String>,
}

impl Conn {
    fn send(&mut self, kind: u16, flags: u16, id: u32, payload: Vec<u8>) -> io::Result<()> {
        let bytes = Frame::new(kind, flags, id, payload).encode();
        self.tx.write_all(&bytes)?;
        // Terminate transfers that are a multiple of the packet size with a
        // zero-length packet (FunctionFS does not do it by itself).
        if bytes.len() % 512 == 0 {
            self.tx.write(&[])?;
        }
        Ok(())
    }

    fn ok(&mut self, id: u32, payload: Vec<u8>) -> io::Result<()> {
        self.send(resp::OK, FLAG_END, id, payload)
    }

    fn err(&mut self, id: u32, msg: &str) -> io::Result<()> {
        self.send(resp::ERR, FLAG_END, id, msg.as_bytes().to_vec())
    }
}

pub fn run() -> ! {
    let mut announced_wait = false;
    let mut quiet = 0u32;
    loop {
        // Off (agent.conf): nothing to serve until it is switched on.
        if !usb::control_on() {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        let (rx, tx, name) = match usb::open_transport() {
            Ok(t) => t,
            Err(e) => {
                if !announced_wait {
                    log::warn(&format!("server: transport: {e} (retrying)"));
                    announced_wait = true;
                }
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        announced_wait = false;
        if quiet == 0 {
            log::info(&format!("server: listening on {name}"));
        }
        let mut conn = Conn { rx, tx, upload: None, upload_err: None };
        let opened = std::time::Instant::now();
        if let Err(e) = serve(&mut conn) {
            // While the host is gone, reads fail immediately; log the first
            // failure only and back off.
            if quiet == 0 {
                log::warn(&format!("server: connection ended: {e}"));
            }
            quiet = if opened.elapsed() < Duration::from_secs(1) { quiet + 1 } else { 0 };
        }
        drop(conn);
        std::thread::sleep(Duration::from_millis(if quiet > 0 { 1000 } else { 200 }));
    }
}

fn serve(conn: &mut Conn) -> io::Result<()> {
    let mut reader = FrameReader::default();
    let mut buf = vec![0u8; READ_BUF];
    loop {
        let n = match conn.rx.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if n == 0 {
            continue; // zero-length packet
        }
        reader.push(&buf[..n]);
        loop {
            match reader.next_frame() {
                Ok(Some(frame)) => handle(conn, frame)?,
                Ok(None) => break,
                Err(e) => {
                    log::warn(&format!("server: {e}; resyncing"));
                    break;
                }
            }
        }
    }
}

fn handle(conn: &mut Conn, f: Frame) -> io::Result<()> {
    let id = f.id;
    match f.kind {
        req::PING => conn.ok(id, f.payload),
        req::INFO => conn.ok(id, sys::info().into_bytes()),
        req::EXEC => exec(conn, id, &f.payload),
        req::READ_FILE => read_file(conn, id, &f.payload),
        req::WRITE_FILE => write_file(conn, id, &f.payload),
        req::READ_MEM => read_mem(conn, id, &f.payload),
        req::WRITE_MEM => write_mem(conn, id, &f.payload),
        req::SAY => {
            log::remote(&String::from_utf8_lossy(&f.payload));
            conn.ok(id, vec![])
        }
        req::TONE => tone(conn, id, &f.payload),
        req::HOST_TEST => {
            let secs = Payload::new(&f.payload).u32().unwrap_or(60).clamp(5, 600) as u64;
            usb::host_test(secs);
            conn.ok(id, format!("host mode in 5 s for {secs} s").into_bytes())
        }
        req::PLAY_WAV => {
            let vol = f.payload.first().copied().unwrap_or(0);
            let atten = f.payload.get(1).copied().unwrap_or(30);
            let path = String::from_utf8_lossy(f.payload.get(2..).unwrap_or(&[])).into_owned();
            log::remote(&format!("play {path} vol {vol} -{atten}dB"));
            match crate::audio::play_wav(&path, vol, atten) {
                Ok(r) => conn.ok(id, r.into_bytes()),
                Err(e) => conn.err(id, &e),
            }
        }
        req::KEYS => {
            let mut s = crate::buttons::held().join(" ");
            if crate::buttons::headphones() {
                s.push_str(" [headphones]");
            }
            conn.ok(id, s.into_bytes())
        }
        req::SCREEN => screen(conn, id, &f.payload),
        req::UPGRADE => {
            let path = String::from_utf8_lossy(&f.payload).into_owned();
            if let Err(e) = std::fs::metadata(&path) {
                return conn.err(id, &format!("{path}: {e}"));
            }
            log::warn(&format!("server: upgrading to {path}"));
            conn.ok(id, vec![])?;
            let e = sys::exec_self(&path);
            log::error(&format!("server: exec {path} failed: {e}"));
            Ok(())
        }
        req::REBOOT => {
            // Payload byte 1 = hard reset (watchdog; powers the GB350 off).
            if f.payload.first() == Some(&1) {
                log::warn("server: hard reboot requested");
                let card = sys::release_sdcard();
                conn.ok(id, format!("hard reset (watchdog, through the bootloader); {card}").into_bytes())?;
                sys::reboot();
            }
            // Payload byte 2 + "kernel\0dtb" = kexec into those files instead.
            let (kernel, dtb) = match f.payload.first() {
                Some(&2) => {
                    let args = String::from_utf8_lossy(&f.payload[1..]).into_owned();
                    let mut it = args.splitn(2, '\0');
                    (it.next().unwrap_or_default().to_string(), it.next().unwrap_or_default().to_string())
                }
                _ => (kexec::KERNEL.to_string(), kexec::DTB.to_string()),
            };
            let img = match kexec::prepare(&kernel, &dtb).and_then(|img| kexec::load(&img).map(|_| img)) {
                Ok(img) => img,
                Err(e) => return conn.err(id, &format!("kexec: {e} (use `reboot --hard`)")),
            };
            log::warn(&format!("server: kexec into {}", img.name));
            let card = sys::release_sdcard();
            conn.ok(id, format!("kexec into {}; {card}", img.name).into_bytes())?;
            // Let the reply reach the host before USB goes away.
            std::thread::sleep(Duration::from_millis(200));
            let e = kexec::exec();
            log::error(&format!("server: {e}"));
            Ok(())
        }
        other => conn.err(id, &format!("unknown request kind {other:#06x}")),
    }
}

/// Malformed payloads become `ERR` replies rather than connection errors.
macro_rules! field {
    ($conn:expr, $id:expr, $e:expr, $what:literal) => {
        match $e {
            Some(v) => v,
            None => return $conn.err($id, concat!("malformed request: ", $what)),
        }
    };
}

fn exec(conn: &mut Conn, id: u32, payload: &[u8]) -> io::Result<()> {
    let mut p = Payload::new(payload);
    let timeout = field!(conn, id, p.u32(), "timeout");
    let cmd = String::from_utf8_lossy(p.rest()).into_owned();
    log::file_only(&format!("exec: {cmd}"));

    let (mut rx, tx) = match io::pipe() {
        Ok(p) => p,
        Err(e) => return conn.err(id, &format!("pipe: {e}")),
    };
    let tx2 = match tx.try_clone() {
        Ok(t) => t,
        Err(e) => return conn.err(id, &format!("pipe: {e}")),
    };
    let mut child = match Command::new("/bin/sh")
        .arg("-c")
        .arg(&cmd)
        .stdin(Stdio::null())
        .stdout(tx)
        .stderr(tx2)
        .env("PATH", sys::PATH)
        .current_dir("/")
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return conn.err(id, &format!("spawn: {e}")),
    };

    // Kill the child if it outlives its timeout.
    let done = Arc::new(AtomicBool::new(false));
    if timeout > 0 {
        let done = done.clone();
        let pid = child.id() as libc::pid_t;
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(timeout as u64);
            while Instant::now() < deadline {
                if done.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if !done.load(Ordering::Relaxed) {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        });
    }

    // Stream output until EOF, or until the child has exited and its pipe is
    // drained (background grandchildren may keep the pipe open forever).
    let fd = rx.as_raw_fd();
    let mut buf = vec![0u8; CHUNK];
    let mut exited = None;
    loop {
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let ready = unsafe { libc::poll(&mut pfd, 1, 100) };
        if ready > 0 {
            match rx.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => conn.send(resp::OUT, 0, id, buf[..n].to_vec())?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
            continue;
        }
        if exited.is_some() {
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            exited = Some(status);
        }
    }
    done.store(true, Ordering::Relaxed);
    let status = match exited {
        Some(s) => s,
        None => child.wait()?,
    };
    use std::os::unix::process::ExitStatusExt;
    let code = status.code().unwrap_or_else(|| -status.signal().unwrap_or(0));
    conn.send(resp::EXIT, FLAG_END, id, code.to_le_bytes().to_vec())
}

fn read_file(conn: &mut Conn, id: u32, payload: &[u8]) -> io::Result<()> {
    let mut p = Payload::new(payload);
    let offset = field!(conn, id, p.u64(), "offset");
    let max_len = field!(conn, id, p.u32(), "len") as u64;
    let path = String::from_utf8_lossy(p.rest()).into_owned();

    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => return conn.err(id, &format!("open {path}: {e}")),
    };
    // Size is reported for regular files; procfs/sysfs files report 0.
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    if offset > 0 {
        if let Err(e) = file.seek(SeekFrom::Start(offset)) {
            return conn.err(id, &format!("seek {path}: {e}"));
        }
    }
    let mut remaining = max_len.min(MAX_FILE_READ);
    let mut buf = vec![0u8; CHUNK];
    while remaining > 0 {
        let want = (remaining as usize).min(buf.len());
        let n = match file.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return conn.err(id, &format!("read {path}: {e}")),
        };
        conn.send(resp::DATA, 0, id, buf[..n].to_vec())?;
        remaining -= n as u64;
    }
    conn.ok(id, size.to_le_bytes().to_vec())
}

fn write_file(conn: &mut Conn, id: u32, payload: &[u8]) -> io::Result<()> {
    let mut p = Payload::new(payload);
    let offset = field!(conn, id, p.u64(), "offset");
    let flags = field!(conn, id, p.u8(), "flags");
    let path_len = field!(conn, id, p.u16(), "path_len") as usize;
    let path = String::from_utf8_lossy(field!(conn, id, p.bytes(path_len), "path")).into_owned();
    let data = p.rest();
    let (truncate, last, streamed) = (flags & 1 != 0, flags & 2 != 0, flags & 0x80 != 0);

    // Streamed uploads keep the file open and sync once at the end: a sync
    // per 16 KiB chunk (exFAT metadata included) held pushes to ~1.5 MB/s.
    if truncate {
        conn.upload_err = None;
    }
    if streamed && conn.upload_err.is_some() {
        // Already failed: swallow the rest, report on the last chunk.
        return match last {
            true => {
                let e = conn.upload_err.take().unwrap_or_default();
                conn.err(id, &e)
            }
            false => Ok(()),
        };
    }
    let cached = conn.upload.take().filter(|(p, _)| *p == path && !truncate);
    let result = match cached {
        Some((_, f)) => Ok(f),
        None => {
            let mut opts = OpenOptions::new();
            opts.write(true).create(true);
            if truncate {
                opts.truncate(true);
            }
            opts.open(&path)
        }
    }
    .and_then(|mut f| {
        if f.stream_position()? != offset {
            f.seek(SeekFrom::Start(offset))?;
        }
        f.write_all(data)?;
        if !streamed || last {
            f.sync_data()?;
        } else {
            conn.upload = Some((path.clone(), f));
        }
        Ok(())
    });
    match (result, streamed && !last) {
        (Ok(()), true) => Ok(()),
        (Err(e), true) => {
            conn.upload_err = Some(format!("write {path}: {e}"));
            Ok(())
        }
        (Ok(()), false) => conn.ok(id, vec![]),
        (Err(e), false) => conn.err(id, &format!("write {path}: {e}")),
    }
}

fn tone(conn: &mut Conn, id: u32, payload: &[u8]) -> io::Result<()> {
    let mut p = Payload::new(payload);
    let freq = field!(conn, id, p.u32(), "freq");
    let ms = field!(conn, id, p.u32(), "ms").min(10_000);
    let vol = field!(conn, id, p.u8(), "volume");
    let amp = p.u16().unwrap_or(1000);
    let square = p.u8().unwrap_or(0) != 0;
    log::remote(&format!("tone {freq} Hz {ms} ms vol {vol} amp {amp}{}", if square { " square" } else { "" }));
    match crate::audio::tone(freq, ms, vol, amp, square) {
        Ok(r) => conn.ok(id, r.into_bytes()),
        Err(e) => conn.err(id, &e),
    }
}

/// SCREEN: no payload = report; one byte 1 = release the display, 2 = take
/// it back.
fn screen(conn: &mut Conn, id: u32, payload: &[u8]) -> io::Result<()> {
    if let [cmd] = payload {
        let r = log::with_screen(|s| if *cmd == 1 { s.release() } else { s.take() });
        return match r {
            None => conn.err(id, "no console"),
            Some(Err(e)) => conn.err(id, &e),
            Some(Ok(())) => conn.ok(id, format!("display {}\n", if *cmd == 1 { "released" } else { "taken" }).into_bytes()),
        };
    }
    let report = log::with_screen(|s| s.describe()).unwrap_or_else(|| "no console".into());
    conn.ok(id, (report + "\n").into_bytes())
}

fn read_mem(conn: &mut Conn, id: u32, payload: &[u8]) -> io::Result<()> {
    let mut p = Payload::new(payload);
    let addr = field!(conn, id, p.u32(), "addr");
    let len = field!(conn, id, p.u32(), "len");
    if addr % 4 != 0 || len % 4 != 0 || len == 0 || len > MAX_MEM_READ {
        return conn.err(id, "addr/len must be 4-byte aligned, len 1..=4MiB");
    }
    match sys::mem_op(addr, len, None) {
        Ok(data) => {
            for chunk in data.chunks(CHUNK) {
                conn.send(resp::DATA, 0, id, chunk.to_vec())?;
            }
            conn.ok(id, vec![])
        }
        Err(e) => conn.err(id, &e),
    }
}

fn write_mem(conn: &mut Conn, id: u32, payload: &[u8]) -> io::Result<()> {
    let mut p = Payload::new(payload);
    let addr = field!(conn, id, p.u32(), "addr");
    let width = field!(conn, id, p.u8(), "width");
    let value = field!(conn, id, p.u32(), "value");
    if !matches!(width, 1 | 2 | 4) || addr % width as u32 != 0 {
        return conn.err(id, "width must be 1, 2 or 4 and addr aligned to it");
    }
    log::file_only(&format!("poke {addr:#010x} <- {value:#x} (w{width})"));
    match sys::mem_op(addr, width as u32, Some((width, value))) {
        Ok(_) => conn.ok(id, vec![]),
        Err(e) => conn.err(id, &e),
    }
}
