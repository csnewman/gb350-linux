//! Minimal KMS bindings over raw DRM ioctls (no libdrm on the device), for
//! the GB350's `hc16xx-drm` driver: one CRTC, one connector, two planes.
//! Legacy mode-setting calls (SETCRTC, PAGE_FLIP, SETPLANE), which the
//! kernel maps onto atomic commits.

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;

// ---- ioctl plumbing (MIPS: dir bits 29-31, none = 1, read = 2, write = 4) ----

const fn iowr(nr: u32, size: usize) -> u32 {
    (6 << 29) | ((size as u32 & 0x1fff) << 16) | (0x64 << 8) | nr
}

const fn io(nr: u32) -> u32 {
    (1 << 29) | (0x64 << 8) | nr
}

fn ioctl<T>(fd: &File, req: u32, arg: &mut T) -> Result<(), String> {
    loop {
        let rc = unsafe { libc::ioctl(fd.as_raw_fd(), req as _, arg as *mut T) };
        if rc == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(format!("drm ioctl {:#x}: {e}", req & 0xff));
        }
    }
}

#[repr(C)]
#[derive(Default)]
struct CardRes {
    fb_ptr: u64,
    crtc_ptr: u64,
    conn_ptr: u64,
    enc_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_conns: u32,
    count_encs: u32,
    min_w: u32,
    max_w: u32,
    min_h: u32,
    max_h: u32,
}

#[repr(C)]
#[derive(Default)]
struct GetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

/// `struct drm_mode_modeinfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ModeInfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub typ: u32,
    pub name: [u8; 32],
}

impl ModeInfo {
    pub fn name(&self) -> String {
        CStr::from_bytes_until_nul(&self.name).map(|s| s.to_string_lossy().into()).unwrap_or_default()
    }

    pub fn preferred(&self) -> bool {
        self.typ & (1 << 3) != 0
    }

    /// Exact refresh rate (Hz) from the clock and totals.
    pub fn refresh(&self) -> f64 {
        self.clock as f64 * 1000.0 / (self.htotal as f64 * self.vtotal as f64)
    }
}

/// Rotations for `Card::blit` (counter-clockwise, as DRM_MODE_ROTATE_*).
pub const ROTATE_0: u32 = 1;
pub const ROTATE_90: u32 = 2;
pub const ROTATE_180: u32 = 4;
pub const ROTATE_270: u32 = 8;

/// `struct drm_hc16xx_blit` (kernel/include/uapi/drm/hc16xx_drm.h).
#[repr(C)]
#[derive(Default)]
struct Blit {
    src_handle: u32,
    src_offset: u32,
    src_pitch: u32,
    src_format: u32,
    width: u32,
    height: u32,
    dst_handle: u32,
    dst_offset: u32,
    dst_pitch: u32,
    dst_format: u32,
    rotation: u32,
    pad: u32,
}

/// DRM_IOW(DRM_COMMAND_BASE + 0, struct drm_hc16xx_blit); MIPS write = 4.
const HC16XX_BLIT: u32 = (4 << 29) | ((std::mem::size_of::<Blit>() as u32) << 16) | (0x64 << 8) | 0x40;

#[repr(C)]
struct SetCrtc {
    connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: ModeInfo,
}

#[repr(C)]
#[derive(Default)]
struct CreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
#[derive(Default)]
struct MapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

#[repr(C)]
#[derive(Default)]
struct FbCmd2 {
    fb_id: u32,
    width: u32,
    height: u32,
    format: u32,
    flags: u32,
    handles: [u32; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
    pad: u32,
    modifier: [u64; 4],
}

#[repr(C)]
#[derive(Default)]
struct PlaneRes {
    planes_ptr: u64,
    count: u32,
    pad: u32,
}

/// `struct drm_mode_set_plane` (source rectangle in 16.16 fixed point).
#[repr(C)]
#[derive(Default)]
pub struct SetPlane {
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub flags: u32,
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_h: u32,
    pub src_w: u32,
}

#[repr(C)]
#[derive(Default)]
struct PageFlip {
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    reserved: u32,
    user_data: u64,
}

#[repr(C)]
#[derive(Default)]
struct ObjProps {
    props_ptr: u64,
    values_ptr: u64,
    count: u32,
    obj_id: u32,
    obj_type: u32,
    pad: u32,
}

#[repr(C)]
struct GetProperty {
    values_ptr: u64,
    enum_ptr: u64,
    prop_id: u32,
    flags: u32,
    name: [u8; 32],
    count_values: u32,
    count_enums: u32,
}

#[repr(C)]
#[derive(Default)]
struct SetProperty {
    value: u64,
    prop_id: u32,
    obj_id: u32,
    obj_type: u32,
    pad: u32,
}

#[repr(C)]
struct PropEnum {
    value: u64,
    name: [u8; 32],
}

#[repr(C)]
#[derive(Default)]
struct ClientCap {
    cap: u64,
    value: u64,
}

const GETRESOURCES: u32 = iowr(0xa0, std::mem::size_of::<CardRes>());
const SETCRTC: u32 = iowr(0xa2, std::mem::size_of::<SetCrtc>());
const GETCONNECTOR: u32 = iowr(0xa7, std::mem::size_of::<GetConnector>());
const GETPROPERTY: u32 = iowr(0xaa, std::mem::size_of::<GetProperty>());
const RMFB: u32 = iowr(0xaf, 4);
const PAGE_FLIP: u32 = iowr(0xb0, std::mem::size_of::<PageFlip>());
const CREATE_DUMB: u32 = iowr(0xb2, std::mem::size_of::<CreateDumb>());
const MAP_DUMB: u32 = iowr(0xb3, std::mem::size_of::<MapDumb>());
const DESTROY_DUMB: u32 = iowr(0xb4, 4);
const GETPLANERESOURCES: u32 = iowr(0xb5, std::mem::size_of::<PlaneRes>());
const SETPLANE: u32 = iowr(0xb7, std::mem::size_of::<SetPlane>());
const ADDFB2: u32 = iowr(0xb8, std::mem::size_of::<FbCmd2>());
const OBJ_GETPROPERTIES: u32 = iowr(0xb9, std::mem::size_of::<ObjProps>());
const OBJ_SETPROPERTY: u32 = iowr(0xba, std::mem::size_of::<SetProperty>());
const SET_CLIENT_CAP: u32 = (4 << 29) | (16 << 16) | (0x64 << 8) | 0x0d;
const SET_MASTER: u32 = io(0x1e);
const DROP_MASTER: u32 = io(0x1f);
const PAGE_FLIP_EVENT: u32 = 1;
pub const OBJ_CRTC: u32 = 0xcccccccc;
pub const OBJ_CONNECTOR: u32 = 0xc0c0c0c0;
pub const OBJ_PLANE: u32 = 0xeeeeeeee;

pub const fn fourcc(s: &[u8; 4]) -> u32 {
    s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}

pub const XRGB8888: u32 = fourcc(b"XR24");
pub const ARGB8888: u32 = fourcc(b"AR24");
pub const RGB565: u32 = fourcc(b"RG16");
pub const XRGB1555: u32 = fourcc(b"XR15");
pub const XRGB4444: u32 = fourcc(b"XR12");
pub const XBGR1555: u32 = fourcc(b"XB15");
pub const XBGR8888: u32 = fourcc(b"XB24");
pub const BGR565: u32 = fourcc(b"BG16");

/// A property of a KMS object: name, id, current value, enum values.
pub struct Property {
    pub name: String,
    pub id: u32,
    pub value: u64,
    pub enums: Vec<(u64, String)>,
}

/// The display device: its single CRTC/connector and planes.
pub struct Card {
    f: File,
    pub crtc: u32,
    pub conn: u32,
    pub modes: Vec<ModeInfo>,
    /// Primary first, then the overlay.
    pub planes: Vec<u32>,
}

/// A dumb buffer with a framebuffer object, mapped (cached on this driver:
/// the kernel flushes it when it is committed).
pub struct Dumb {
    f: File,
    pub id: u32,
    handle: u32,
    pub w: u32,
    pub h: u32,
    pub pitch: u32,
    map: *mut u8,
    size: usize,
}

unsafe impl Send for Dumb {}

impl Card {
    pub fn open(path: &str) -> Result<Card, String> {
        let f = OpenOptions::new().read(true).write(true).open(path).map_err(|e| format!("{path}: {e}"))?;
        let mut cap = ClientCap { cap: 2, value: 1 }; // universal planes
        ioctl(&f, SET_CLIENT_CAP, &mut cap)?;
        let mut res = CardRes::default();
        ioctl(&f, GETRESOURCES, &mut res)?;
        let mut crtcs = vec![0u32; res.count_crtcs as usize];
        let mut conns = vec![0u32; res.count_conns as usize];
        let mut res2 = CardRes {
            crtc_ptr: crtcs.as_mut_ptr() as u64,
            conn_ptr: conns.as_mut_ptr() as u64,
            count_crtcs: res.count_crtcs,
            count_conns: res.count_conns,
            ..Default::default()
        };
        ioctl(&f, GETRESOURCES, &mut res2)?;
        let conn = *conns.first().ok_or("no connector")?;
        let mut gc = GetConnector { connector_id: conn, ..Default::default() };
        ioctl(&f, GETCONNECTOR, &mut gc)?;
        let mut modes = vec![unsafe { std::mem::zeroed::<ModeInfo>() }; gc.count_modes as usize];
        let mut gc2 = GetConnector { connector_id: conn, modes_ptr: modes.as_mut_ptr() as u64, count_modes: gc.count_modes, ..Default::default() };
        ioctl(&f, GETCONNECTOR, &mut gc2)?;
        let mut pr = PlaneRes::default();
        ioctl(&f, GETPLANERESOURCES, &mut pr)?;
        let mut planes = vec![0u32; pr.count as usize];
        let mut pr2 = PlaneRes { planes_ptr: planes.as_mut_ptr() as u64, count: pr.count, pad: 0 };
        ioctl(&f, GETPLANERESOURCES, &mut pr2)?;
        Ok(Card { f, crtc: *crtcs.first().ok_or("no crtc")?, conn, modes, planes })
    }

    /// Becomes the display's master (needed to set modes), or gives it up so
    /// another program (or the kernel console) can show.
    pub fn set_master(&self, on: bool) -> Result<(), String> {
        let mut nothing = 0u32;
        ioctl(&self.f, if on { SET_MASTER } else { DROP_MASTER }, &mut nothing)
    }

    pub fn native_mode(&self) -> Option<ModeInfo> {
        self.modes.iter().find(|m| m.preferred()).or(self.modes.first()).copied()
    }

    /// The panel-size mode whose refresh rate is closest to `hz` (the
    /// driver offers the native one plus exact rates for the emulated
    /// systems: 50.007, 59.73, 59.92, 60.00, 60.10, 75.49 Hz).
    pub fn mode_for_rate(&self, hz: f64) -> Option<ModeInfo> {
        let native = self.native_mode()?;
        self.modes
            .iter()
            .filter(|m| m.hdisplay == native.hdisplay && m.vdisplay == native.vdisplay)
            .min_by(|a, b| (a.refresh() - hz).abs().total_cmp(&(b.refresh() - hz).abs()))
            .copied()
    }

    /// Copies `src` (w x h from its top left) to the top left of `dst` with
    /// the SoC's 2D engine, turned by `rotation` (ROTATE_*) and converted
    /// between the buffers' formats. Waits for the engine.
    pub fn blit(&self, src: &Dumb, src_format: u32, w: u32, h: u32, dst: &Dumb, dst_format: u32, rotation: u32) -> Result<(), String> {
        let mut b = Blit {
            src_handle: src.handle,
            src_pitch: src.pitch,
            src_format,
            width: w,
            height: h,
            dst_handle: dst.handle,
            dst_pitch: dst.pitch,
            dst_format,
            rotation,
            ..Default::default()
        };
        ioctl(&self.f, HC16XX_BLIT, &mut b)
    }

    pub fn mode(&self, w: u32, h: u32) -> Result<ModeInfo, String> {
        self.modes.iter().find(|m| m.hdisplay as u32 == w && m.vdisplay as u32 == h).copied().ok_or(format!("no {w}x{h} mode"))
    }

    pub fn props(&self, obj: u32, typ: u32) -> Result<Vec<Property>, String> {
        let mut op = ObjProps { obj_id: obj, obj_type: typ, ..Default::default() };
        ioctl(&self.f, OBJ_GETPROPERTIES, &mut op)?;
        let mut ids = vec![0u32; op.count as usize];
        let mut vals = vec![0u64; op.count as usize];
        let mut op2 = ObjProps { props_ptr: ids.as_mut_ptr() as u64, values_ptr: vals.as_mut_ptr() as u64, count: op.count, obj_id: obj, obj_type: typ, pad: 0 };
        ioctl(&self.f, OBJ_GETPROPERTIES, &mut op2)?;
        let mut out = vec![];
        for (id, value) in ids.into_iter().zip(vals) {
            let mut gp = GetProperty { values_ptr: 0, enum_ptr: 0, prop_id: id, flags: 0, name: [0; 32], count_values: 0, count_enums: 0 };
            ioctl(&self.f, GETPROPERTY, &mut gp)?;
            let mut enums: Vec<PropEnum> = (0..gp.count_enums).map(|_| PropEnum { value: 0, name: [0; 32] }).collect();
            let mut values = vec![0u64; gp.count_values as usize];
            // enum or bitmask properties
            if gp.flags & (1 << 3 | 1 << 5) != 0 && gp.count_enums > 0 {
                gp.enum_ptr = enums.as_mut_ptr() as u64;
                gp.values_ptr = values.as_mut_ptr() as u64;
                ioctl(&self.f, GETPROPERTY, &mut gp)?;
            }
            let cstr = |b: &[u8]| CStr::from_bytes_until_nul(b).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            out.push(Property {
                name: cstr(&gp.name),
                id,
                value,
                enums: enums.iter().map(|e| (e.value, cstr(&e.name))).collect(),
            });
        }
        Ok(out)
    }

    /// Sets an enum property by value name.
    pub fn set_enum(&self, obj: u32, typ: u32, prop: &str, value: &str) -> Result<(), String> {
        let p = self.props(obj, typ)?.into_iter().find(|p| p.name == prop).ok_or(format!("no property {prop}"))?;
        let v = p.enums.iter().find(|(_, n)| n == value).ok_or(format!("{prop}: no value {value}"))?.0;
        let mut sp = SetProperty { value: v, prop_id: p.id, obj_id: obj, obj_type: typ, pad: 0 };
        ioctl(&self.f, OBJ_SETPROPERTY, &mut sp)
    }

    pub fn dumb(&self, w: u32, h: u32, format: u32, bpp: u32) -> Result<Dumb, String> {
        let f = self.f.try_clone().map_err(|e| e.to_string())?;
        let mut cd = CreateDumb { width: w, height: h, bpp, ..Default::default() };
        ioctl(&f, CREATE_DUMB, &mut cd)?;
        // From here on Drop cleans up whatever exists.
        let mut d = Dumb { f, id: 0, handle: cd.handle, w, h, pitch: cd.pitch, map: std::ptr::null_mut(), size: cd.size as usize };
        let mut md = MapDumb { handle: cd.handle, ..Default::default() };
        ioctl(&d.f, MAP_DUMB, &mut md)?;
        // mmap64: the offset can exceed 32 bits. (Plain mmap elsewhere, so
        // the crate builds on a development host.)
        #[cfg(target_os = "linux")]
        let map = unsafe {
            libc::mmap64(std::ptr::null_mut(), d.size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, d.f.as_raw_fd(), md.offset as i64)
        };
        #[cfg(not(target_os = "linux"))]
        let map = unsafe {
            libc::mmap(std::ptr::null_mut(), d.size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, d.f.as_raw_fd(), md.offset as libc::off_t)
        };
        if map == libc::MAP_FAILED {
            return Err(format!("mmap dumb: {}", std::io::Error::last_os_error()));
        }
        d.map = map as *mut u8;
        let mut fc = FbCmd2 { width: w, height: h, format, ..Default::default() };
        fc.handles[0] = cd.handle;
        fc.pitches[0] = cd.pitch;
        ioctl(&d.f, ADDFB2, &mut fc)?;
        d.id = fc.fb_id;
        Ok(d)
    }

    /// Mode-sets the CRTC to `mode`, showing `fb` on the primary plane.
    pub fn set_crtc(&self, fb: &Dumb, mode: &ModeInfo) -> Result<(), String> {
        let conns = [self.conn];
        let mut sc = SetCrtc {
            connectors_ptr: conns.as_ptr() as u64,
            count_connectors: 1,
            crtc_id: self.crtc,
            fb_id: fb.id,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 1,
            mode: *mode,
        };
        ioctl(&self.f, SETCRTC, &mut sc)
    }

    /// Queues a flip to `fb` at the next vblank; completion is an event (see
    /// `wait_flip`).
    pub fn page_flip(&self, fb: &Dumb) -> Result<(), String> {
        let mut pf = PageFlip { crtc_id: self.crtc, fb_id: fb.id, flags: PAGE_FLIP_EVENT, ..Default::default() };
        ioctl(&self.f, PAGE_FLIP, &mut pf)
    }

    /// Waits for the next DRM event (a completed flip); returns its time in
    /// seconds (CLOCK_MONOTONIC).
    pub fn wait_flip(&self) -> Result<f64, String> {
        let mut ev = [0u8; 32];
        (&self.f).read_exact(&mut ev).map_err(|e| format!("drm event: {e}"))?;
        let sec = u32::from_ne_bytes(ev[16..20].try_into().unwrap());
        let usec = u32::from_ne_bytes(ev[20..24].try_into().unwrap());
        Ok(sec as f64 + usec as f64 / 1e6)
    }

    /// A completed flip within `timeout_ms` (0 = just check), if any.
    pub fn poll_flip(&self, timeout_ms: i32) -> Result<Option<f64>, String> {
        let mut p = libc::pollfd { fd: self.f.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut p, 1, timeout_ms) } <= 0 {
            return Ok(None);
        }
        self.wait_flip().map(Some)
    }

    pub fn set_plane(&self, sp: &mut SetPlane) -> Result<(), String> {
        ioctl(&self.f, SETPLANE, sp)
    }
}

/// The display driver's sysfs controls (`hc16xx-drm`): picture colour,
/// the background shown around the layers, panel power.
pub mod control {
    const DIR: &str = "/sys/devices/platform/1883a000.display-controller";

    fn write(name: &str, v: &str) -> Result<(), String> {
        std::fs::write(format!("{DIR}/{name}"), v).map_err(|e| format!("{name}: {e}"))
    }

    /// The vendor's neutral BT.709 words: RGB -> YCbCr (limited range,
    /// x512) rows Y, Cb, Cr with an offset each, then sharpness.
    const Y: [f64; 3] = [93.0, 314.0, 32.0];
    const CB: [f64; 3] = [-52.0, -173.0, 225.0];
    const CR: [f64; 3] = [225.0, -204.0, -21.0];

    /// Picture adjustments, 0..100 with 50 neutral (sharpness 0..10, 5
    /// neutral). Brightness moves black up to 64 levels; contrast scales
    /// around mid grey; saturation scales the colour; hue turns it (0 / 100
    /// = half a turn either way).
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Picture {
        pub brightness: u32,
        pub contrast: u32,
        pub saturation: u32,
        pub hue: u32,
        pub sharpness: u32,
    }

    impl Default for Picture {
        fn default() -> Picture {
            Picture { brightness: 50, contrast: 50, saturation: 50, hue: 50, sharpness: 5 }
        }
    }

    impl Picture {
        /// The 13 descriptor words.
        pub fn words(&self) -> [i32; 13] {
            let gain = |v: u32| if v < 50 { v as f64 / 50.0 } else { 1.0 + (v as f64 - 50.0) / 50.0 };
            let c = gain(self.contrast.min(100));
            let sat = gain(self.saturation.min(100));
            // Level offset in Y (8-bit limited-range levels), ~440 per level.
            let level = (self.brightness.min(100) as f64 - 50.0) * 64.0 / 50.0 + (1.0 - c) * 64.0;
            let a = (self.hue.min(100) as f64 / 50.0 - 1.0) * std::f64::consts::PI;
            let (sin, cos) = a.sin_cos();
            let r = |v: f64| v.round() as i32;
            let mut w = [0i32; 13];
            for i in 0..3 {
                w[i] = r(Y[i] * c);
                w[4 + i] = r((CB[i] * cos - CR[i] * sin) * c * sat);
                w[8 + i] = r((CR[i] * cos + CB[i] * sin) * c * sat);
            }
            w[3] = r(level * 512.0 * 219.0 / 255.0);
            w[12] = (self.sharpness.min(10) as i32 - 5) & 0xf;
            w
        }
    }

    pub fn set_picture(p: &Picture) -> Result<(), String> {
        let w = p.words();
        write("enhance", &w.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" "))
    }

    /// The colour around and under the layers (scaled games' borders).
    pub fn set_background(rgb: u32) -> Result<(), String> {
        write("background", &format!("{:06x}", rgb & 0xffffff))
    }

    /// Panel on / off (off: display off + sleep; the backlight is separate).
    pub fn set_panel(on: bool) -> Result<(), String> {
        write("panel", if on { "on" } else { "off" })
    }
}

impl std::os::fd::AsRawFd for Card {
    /// For `poll` alongside other inputs; readable when a flip completed.
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.f.as_raw_fd()
    }
}

impl Dumb {
    pub fn pixels(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.map, self.size) }
    }
}

impl Drop for Dumb {
    fn drop(&mut self) {
        if !self.map.is_null() {
            unsafe { libc::munmap(self.map as *mut libc::c_void, self.size) };
        }
        if self.id != 0 {
            let _ = ioctl(&self.f, RMFB, &mut self.id);
        }
        let _ = ioctl(&self.f, DESTROY_DUMB, &mut self.handle);
    }
}
