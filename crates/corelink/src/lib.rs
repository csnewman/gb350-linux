//! Memory and work rings shared with the GB350's second core (avp/syscore),
//! through `/dev/corelink` (kernel/drivers/soc/hichip/gb350-corelink.c,
//! interface in kernel/include/uapi/linux/gb350-corelink.h).
//!
//! - [`Link`]: the device; allocates [`Buffer`]s on demand (physically
//!   contiguous, freed with the buffer or the process).
//! - [`Buffer`]: mapped memory the core can address by its physical address.
//!   The cores' caches are not coherent: `Cached` buffers need
//!   [`Buffer::sync_to_core`] / [`Buffer::sync_from_core`]; `Uncached` and
//!   `WriteCombine` ones do not.
//! - [`Ring`]: records (`tag`, payload) for a service on the core, in a
//!   write-combining buffer, with a fence counting the records done.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::ptr::{read_volatile, write_volatile};
use std::sync::atomic::{fence, Ordering};
use std::time::Duration;

/// Services on the core.
pub const SERVICE_TEST: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caching {
    Uncached = 0,
    Cached = 1,
    WriteCombine = 2,
}

#[repr(C)]
#[derive(Default, Debug)]
pub struct Info {
    pub version: u32,
    pub allocated: u32,
    pub channels: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct Alloc {
    size: u32,
    caching: u32,
    phys: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct Sync {
    phys: u32,
    len: u32,
    dir: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct Wait {
    fence: u32,
    target: u32,
    timeout_ms: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct Attach {
    service: u32,
    header: u32,
    data: u32,
    size: u32,
    channel: u32,
    pad: [u32; 3],
}

// MIPS ioctl numbers: dir << 29 (none 1, read 2, write 4) | size << 16 | 'C' << 8 | nr.
const fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
    (dir << 29) | ((size as u32) << 16) | (0x43 << 8) | nr
}
const IOC_INFO: u32 = ioc(2, 0, std::mem::size_of::<Info>());
const IOC_ALLOC: u32 = ioc(6, 1, std::mem::size_of::<Alloc>());
const IOC_FREE: u32 = ioc(4, 2, 4);
const IOC_SYNC: u32 = ioc(4, 3, std::mem::size_of::<Sync>());
const IOC_KICK: u32 = ioc(1, 4, 0);
const IOC_WAIT: u32 = ioc(4, 5, std::mem::size_of::<Wait>());
const IOC_ATTACH: u32 = ioc(6, 6, std::mem::size_of::<Attach>());
const IOC_DETACH: u32 = ioc(4, 7, 4);

// Ring header offsets (uapi CORELINK_RING_*).
const HEAD: usize = 0x00;
const TAIL: usize = 0x04;
const IDLE: usize = 0x08;
const FENCE: usize = 0x10;
const ERROR: usize = 0x18;
/// Service results in the header (service specific).
pub const RESULT: usize = 0x20;

fn ioctl<T>(f: &File, req: u32, arg: *mut T) -> std::io::Result<()> {
    loop {
        if unsafe { libc::ioctl(f.as_raw_fd(), req as _, arg) } == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

pub struct Link {
    f: File,
}

impl Link {
    pub fn open() -> std::io::Result<Link> {
        Ok(Link { f: OpenOptions::new().read(true).write(true).open("/dev/corelink")? })
    }

    pub fn info(&self) -> std::io::Result<Info> {
        let mut i = Info::default();
        ioctl(&self.f, IOC_INFO, &mut i)?;
        Ok(i)
    }

    /// A new buffer of at least `size` bytes, zeroed, mapped.
    pub fn alloc(&self, size: usize, caching: Caching) -> std::io::Result<Buffer> {
        let mut a = Alloc { size: size as u32, caching: caching as u32, ..Default::default() };
        ioctl(&self.f, IOC_ALLOC, &mut a)?;
        let len = (size + 4095) & !4095;
        // MAP_SHARED at offset = physical address: the kernel picks an
        // address of the same cache colour (the data cache aliases).
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, self.f.as_raw_fd(), a.phys as libc::off_t)
        };
        if ptr == libc::MAP_FAILED {
            let e = std::io::Error::last_os_error();
            let mut p = a.phys;
            let _ = ioctl(&self.f, IOC_FREE, &mut p);
            return Err(e);
        }
        Ok(Buffer { f: self.f.try_clone()?, phys: a.phys, len, ptr: ptr as *mut u8, caching })
    }

    /// Wakes the core (it looks at all rings).
    pub fn kick(&self) -> std::io::Result<()> {
        ioctl(&self.f, IOC_KICK, std::ptr::null_mut::<u8>())
    }
}

/// Memory shared with the core. Freed when dropped (the kernel keeps it while
/// a ring on the core still uses it).
pub struct Buffer {
    f: File,
    pub phys: u32,
    pub len: usize,
    ptr: *mut u8,
    pub caching: Caching,
}

unsafe impl Send for Buffer {}

impl Buffer {
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// The memory as bytes. For `Uncached` / `WriteCombine` buffers prefer
    /// word-sized volatile accesses ([`Buffer::read32`] / [`Buffer::write32`]).
    pub fn bytes(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    pub fn read32(&self, off: usize) -> u32 {
        assert!(off + 4 <= self.len);
        unsafe { read_volatile(self.ptr.add(off) as *const u32) }
    }

    pub fn write32(&self, off: usize, v: u32) {
        assert!(off + 4 <= self.len);
        unsafe { write_volatile(self.ptr.add(off) as *mut u32, v) }
    }

    fn sync(&self, off: usize, len: usize, dir: u32) -> std::io::Result<()> {
        assert!(self.caching == Caching::Cached && off + len <= self.len);
        let mut s = Sync { phys: self.phys + off as u32, len: len as u32, dir, pad: 0 };
        ioctl(&self.f, IOC_SYNC, &mut s)
    }

    /// Writes what this CPU wrote in [off, off + len) back for the core.
    pub fn sync_to_core(&self, off: usize, len: usize) -> std::io::Result<()> {
        self.sync(off, len, 1)
    }

    /// Drops this CPU's cached copy of [off, off + len): the core wrote it.
    /// Unwritten changes there are lost; [`Buffer::sync_both`] keeps them.
    pub fn sync_from_core(&self, off: usize, len: usize) -> std::io::Result<()> {
        self.sync(off, len, 2)
    }

    /// Writes back and drops this CPU's cached copy of [off, off + len):
    /// memory holds this CPU's changes, and later reads see the core's.
    /// Ranges of at least the data cache's size (32 KiB) cost one pass
    /// over the cache.
    pub fn sync_both(&self, off: usize, len: usize) -> std::io::Result<()> {
        self.sync(off, len, 3)
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr as *mut _, self.len) };
        let mut p = self.phys;
        let _ = ioctl(&self.f, IOC_FREE, &mut p);
    }
}

/// A ring of records for one service on the core.
pub struct Ring {
    link: Link,
    header: Buffer,
    data: Buffer,
    pub channel: u32,
    head: u32,
    /// Records submitted (not counting padding): the fence target for the
    /// last one.
    submitted: u32,
}

#[derive(Debug)]
pub enum PushError {
    /// The record does not fit in the ring at all.
    TooBig,
    /// The core consumed nothing for [`Ring::STALL`]: it is stuck.
    Stalled,
}

impl Ring {
    /// Attaches a ring of `size` bytes (a power of two) to `service`.
    pub fn new(service: u32, size: usize) -> std::io::Result<Ring> {
        assert!(size.is_power_of_two() && size >= 64);
        let link = Link::open()?;
        let header = link.alloc(4096, Caching::Uncached)?;
        let data = link.alloc(size, Caching::WriteCombine)?;
        let mut a = Attach { service, header: header.phys, data: data.phys, size: size as u32, ..Default::default() };
        ioctl(&link.f, IOC_ATTACH, &mut a)?;
        Ok(Ring { link, header, data, channel: a.channel, head: 0, submitted: 0 })
    }

    fn size(&self) -> u32 {
        self.data.len as u32
    }

    /// Bytes free right now.
    pub fn free(&self) -> u32 {
        self.size() - self.head.wrapping_sub(self.header.read32(TAIL))
    }

    /// Queues one record; returns its fence value (wait for it with
    /// [`Ring::wait`]). Waits for space if the ring is full (up to
    /// [`Ring::STALL`] without progress: [`PushError::Stalled`]). The core sees
    /// it after [`Ring::flush`] (or the next push that flushes).
    pub fn push(&mut self, tag: u16, payload: &[u8]) -> Result<u32, PushError> {
        let total = 8 + ((payload.len() as u32 + 7) & !7);
        let size = self.size();
        if total > size / 2 {
            return Err(PushError::TooBig);
        }
        let mask = size - 1;
        // Padding to the end if the record would cross it (records never wrap).
        let room_to_end = size - (self.head & mask);
        let need = if total > room_to_end { total + room_to_end } else { total };
        self.wait_room(need)?;
        if total > room_to_end {
            self.write_record(0, &[], room_to_end - 8);
        }
        self.write_record(tag, payload, payload.len() as u32);
        self.submitted = self.submitted.wrapping_add(1);
        Ok(self.submitted)
    }

    /// [`Ring::push`] for a payload of words (copied in one go).
    pub fn push_words(&mut self, tag: u16, words: &[u32]) -> Result<u32, PushError> {
        let len = words.len() as u32 * 4;
        let total = 8 + ((len + 7) & !7);
        let size = self.size();
        if total > size / 2 {
            return Err(PushError::TooBig);
        }
        let mask = size - 1;
        let room_to_end = size - (self.head & mask);
        let need = if total > room_to_end { total + room_to_end } else { total };
        self.wait_room(need)?;
        if total > room_to_end {
            self.write_record(0, &[], room_to_end - 8);
        }
        let off = (self.head & mask) as usize;
        self.data.write32(off, (tag as u32) << 16);
        self.data.write32(off + 4, len);
        // Write-combining memory: plain stores, published by flush()'s
        // barrier. Records start 8-byte aligned and never cross the end.
        unsafe { std::ptr::copy_nonoverlapping(words.as_ptr(), self.data.as_ptr().add(off + 8) as *mut u32, words.len()) };
        self.head = self.head.wrapping_add(total);
        self.submitted = self.submitted.wrapping_add(1);
        Ok(self.submitted)
    }

    /// How long a push waits for room before giving up on the core.
    pub const STALL: Duration = Duration::from_secs(2);

    fn wait_room(&mut self, need: u32) -> Result<(), PushError> {
        if self.free() >= need {
            return Ok(());
        }
        let start = std::time::Instant::now();
        let mut tail = self.header.read32(TAIL);
        let mut moved = start;
        while self.free() < need {
            self.flush();
            std::thread::yield_now();
            let t = self.header.read32(TAIL);
            if t != tail {
                (tail, moved) = (t, std::time::Instant::now());
            } else if moved.elapsed() > Self::STALL {
                return Err(PushError::Stalled);
            }
        }
        Ok(())
    }

    fn write_record(&mut self, tag: u16, payload: &[u8], len: u32) {
        let off = (self.head & (self.size() - 1)) as usize;
        self.data.write32(off, (tag as u32) << 16);
        self.data.write32(off + 4, len);
        // Whole words (the core reads the padding too).
        for (i, chunk) in payload.chunks(4).enumerate() {
            let mut w = [0u8; 4];
            w[..chunk.len()].copy_from_slice(chunk);
            self.data.write32(off + 8 + 4 * i, u32::from_le_bytes(w));
        }
        self.head = self.head.wrapping_add(8 + ((len + 7) & !7));
    }

    /// Publishes queued records (drains the write-combining buffer first)
    /// and wakes the core if it is asleep.
    pub fn flush(&mut self) {
        fence(Ordering::SeqCst); // MIPS `sync`: the records reach memory first
        self.header.write32(HEAD, self.head);
        fence(Ordering::SeqCst);
        if self.header.read32(IDLE) != 0 {
            self.header.write32(IDLE, 0);
            let _ = self.link.kick();
        }
    }

    /// Records completed by the core.
    pub fn completed(&self) -> u32 {
        self.header.read32(FENCE)
    }

    /// Flushes and waits until the record with fence value `target` is done.
    pub fn wait(&mut self, target: u32, timeout: Duration) -> std::io::Result<()> {
        self.flush();
        if (self.completed().wrapping_sub(target) as i32) >= 0 {
            return Ok(());
        }
        let mut w = Wait { fence: self.header.phys + FENCE as u32, target, timeout_ms: timeout.as_millis() as u32, pad: 0 };
        ioctl(&self.link.f, IOC_WAIT, &mut w)
    }

    /// Waits for everything submitted.
    pub fn finish(&mut self, timeout: Duration) -> std::io::Result<()> {
        self.wait(self.submitted, timeout)
    }

    pub fn error(&self) -> u32 {
        self.header.read32(ERROR)
    }

    /// A word of the service's results in the header (RESULT + off).
    pub fn result(&self, off: usize) -> u32 {
        self.header.read32(RESULT + off)
    }

    pub fn link(&self) -> &Link {
        &self.link
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        let mut c = self.channel;
        let _ = ioctl(&self.link.f, IOC_DETACH, &mut c);
    }
}
