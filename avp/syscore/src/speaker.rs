//! Speaker amplifier: GPIO L19, high = muted. Policy
//! (CONFIG key 8): 0 = auto - muted while headphones are plugged in (L15,
//! from the button scan), as the stock firmware does; 1 = always muted;
//! 2 = always on.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::hw::{self, GPIO_DIR, GPIO_L, GPIO_OUT};
use crate::shm;

const SPEAKER_MUTE: u32 = 1 << 19;
const HEADPHONES: u64 = 1 << (32 + 15);

pub const AUTO: u32 = 0;
pub const MUTED: u32 = 1;
pub const ON: u32 = 2;

pub static POLICY: AtomicU32 = AtomicU32::new(AUTO);

pub struct Speaker {
    muted: Option<bool>,
}

impl Speaker {
    pub const fn new() -> Speaker {
        Speaker { muted: None }
    }

    /// `input`: the debounced input state.
    pub fn poll(&mut self, input: u64) {
        let want = match POLICY.load(Ordering::Relaxed) {
            MUTED => true,
            ON => false,
            _ => input & HEADPHONES != 0,
        };
        if self.muted == Some(want) {
            return;
        }
        hw::gpio_update(GPIO_L, GPIO_OUT, SPEAKER_MUTE, if want { SPEAKER_MUTE } else { 0 });
        hw::gpio_update(GPIO_L, GPIO_DIR, SPEAKER_MUTE, SPEAKER_MUTE);
        self.muted = Some(want);
        shm::write(shm::H_SPEAKER, want as u32);
        log!("speaker {}", if want { "muted" } else { "on" });
    }
}
