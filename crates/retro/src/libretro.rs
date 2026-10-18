//! The libretro API (libretro.h), as far as this frontend uses it. The core
//! is linked in statically.

#![allow(dead_code)]

use std::ffi::{c_char, c_uint, c_void};

pub const API_VERSION: c_uint = 1;

pub const DEVICE_JOYPAD: c_uint = 1;
pub const DEVICE_ID_JOYPAD_B: c_uint = 0;
pub const DEVICE_ID_JOYPAD_Y: c_uint = 1;
pub const DEVICE_ID_JOYPAD_SELECT: c_uint = 2;
pub const DEVICE_ID_JOYPAD_START: c_uint = 3;
pub const DEVICE_ID_JOYPAD_UP: c_uint = 4;
pub const DEVICE_ID_JOYPAD_DOWN: c_uint = 5;
pub const DEVICE_ID_JOYPAD_LEFT: c_uint = 6;
pub const DEVICE_ID_JOYPAD_RIGHT: c_uint = 7;
pub const DEVICE_ID_JOYPAD_A: c_uint = 8;
pub const DEVICE_ID_JOYPAD_X: c_uint = 9;
pub const DEVICE_ID_JOYPAD_L: c_uint = 10;
pub const DEVICE_ID_JOYPAD_R: c_uint = 11;
pub const DEVICE_ID_JOYPAD_L2: c_uint = 12;
pub const DEVICE_ID_JOYPAD_R2: c_uint = 13;
pub const DEVICE_ID_JOYPAD_MASK: c_uint = 256;

pub const MEMORY_SAVE_RAM: c_uint = 0;

// Environment commands
pub const ENV_SET_ROTATION: c_uint = 1;
pub const ENV_GET_CAN_DUPE: c_uint = 3;
pub const ENV_SET_MESSAGE: c_uint = 6;
pub const ENV_SET_PERFORMANCE_LEVEL: c_uint = 8;
pub const ENV_GET_SYSTEM_DIRECTORY: c_uint = 9;
pub const ENV_SET_PIXEL_FORMAT: c_uint = 10;
pub const ENV_SET_INPUT_DESCRIPTORS: c_uint = 11;
pub const ENV_GET_VARIABLE: c_uint = 15;
pub const ENV_SET_VARIABLES: c_uint = 16;
pub const ENV_GET_VARIABLE_UPDATE: c_uint = 17;
pub const ENV_SET_SUPPORT_NO_GAME: c_uint = 18;
pub const ENV_GET_LOG_INTERFACE: c_uint = 27;
pub const ENV_GET_SAVE_DIRECTORY: c_uint = 31;
pub const ENV_SET_SYSTEM_AV_INFO: c_uint = 32;
pub const ENV_SET_GEOMETRY: c_uint = 37;
pub const ENV_GET_LANGUAGE: c_uint = 39;
pub const ENV_GET_AUDIO_VIDEO_ENABLE: c_uint = 47 | 0x10000;
pub const ENV_GET_INPUT_BITMASKS: c_uint = 51 | 0x10000;
pub const ENV_GET_CORE_OPTIONS_VERSION: c_uint = 52;
pub const ENV_SET_AUDIO_BUFFER_STATUS_CALLBACK: c_uint = 62;
pub const ENV_SET_MINIMUM_AUDIO_LATENCY: c_uint = 63;
pub const ENV_EXPERIMENTAL: c_uint = 0x10000;
pub const ENV_GET_CURRENT_SOFTWARE_FRAMEBUFFER: c_uint = 40 | 0x10000;
/// GB350 addition (cores/patches): the frontend takes PlayStation-native BGR555.
pub const ENV_GB350_NATIVE_BGR555: c_uint = 0x7f35_0001;
pub const MEMORY_TYPE_CACHED: c_uint = 1;

#[repr(C)]
pub struct Framebuffer {
    pub data: *mut c_void,
    pub width: c_uint,
    pub height: c_uint,
    pub pitch: usize,
    pub format: c_uint,
    pub access_flags: c_uint,
    pub memory_flags: c_uint,
}

pub type AudioBufferStatusFn = unsafe extern "C" fn(active: bool, occupancy: c_uint, underrun_likely: bool);

#[repr(C)]
pub struct AudioBufferStatusCallback {
    pub callback: Option<AudioBufferStatusFn>,
}

pub const PIXEL_FORMAT_0RGB1555: c_uint = 0;
pub const PIXEL_FORMAT_XRGB8888: c_uint = 1;
pub const PIXEL_FORMAT_RGB565: c_uint = 2;

pub const LOG_DEBUG: c_uint = 0;

#[repr(C)]
pub struct SystemInfo {
    pub library_name: *const c_char,
    pub library_version: *const c_char,
    pub valid_extensions: *const c_char,
    pub need_fullpath: bool,
    pub block_extract: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct GameGeometry {
    pub base_width: c_uint,
    pub base_height: c_uint,
    pub max_width: c_uint,
    pub max_height: c_uint,
    pub aspect_ratio: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct SystemTiming {
    pub fps: f64,
    pub sample_rate: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct SystemAvInfo {
    pub geometry: GameGeometry,
    pub timing: SystemTiming,
}

#[repr(C)]
pub struct GameInfo {
    pub path: *const c_char,
    pub data: *const c_void,
    pub size: usize,
    pub meta: *const c_char,
}

#[repr(C)]
pub struct Variable {
    pub key: *const c_char,
    pub value: *const c_char,
}

pub type LogFn = unsafe extern "C" fn(level: c_uint, fmt: *const c_char, ...);

#[repr(C)]
pub struct LogCallback {
    pub log: LogFn,
}

pub type EnvironmentFn = unsafe extern "C" fn(cmd: c_uint, data: *mut c_void) -> bool;
pub type VideoRefreshFn = unsafe extern "C" fn(data: *const c_void, width: c_uint, height: c_uint, pitch: usize);
pub type AudioSampleFn = unsafe extern "C" fn(left: i16, right: i16);
pub type AudioSampleBatchFn = unsafe extern "C" fn(data: *const i16, frames: usize) -> usize;
pub type InputPollFn = unsafe extern "C" fn();
pub type InputStateFn = unsafe extern "C" fn(port: c_uint, device: c_uint, index: c_uint, id: c_uint) -> i16;

extern "C" {
    pub fn retro_set_environment(cb: EnvironmentFn);
    pub fn retro_set_video_refresh(cb: VideoRefreshFn);
    pub fn retro_set_audio_sample(cb: AudioSampleFn);
    pub fn retro_set_audio_sample_batch(cb: AudioSampleBatchFn);
    pub fn retro_set_input_poll(cb: InputPollFn);
    pub fn retro_set_input_state(cb: InputStateFn);
    pub fn retro_init();
    pub fn retro_deinit();
    pub fn retro_api_version() -> c_uint;
    pub fn retro_get_system_info(info: *mut SystemInfo);
    pub fn retro_get_system_av_info(info: *mut SystemAvInfo);
    pub fn retro_set_controller_port_device(port: c_uint, device: c_uint);
    pub fn retro_reset();
    pub fn retro_run();
    pub fn retro_load_game(game: *const GameInfo) -> bool;
    pub fn retro_unload_game();
    pub fn retro_get_memory_data(id: c_uint) -> *mut c_void;
    pub fn retro_get_memory_size(id: c_uint) -> usize;
    pub fn retro_serialize_size() -> usize;
    pub fn retro_serialize(data: *mut c_void, size: usize) -> bool;
    pub fn retro_unserialize(data: *const c_void, size: usize) -> bool;
}
