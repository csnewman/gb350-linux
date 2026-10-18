//! Power supervision, independent of Linux being alive:
//! - power button held 2 s: ask Linux to power off (cleanly); held 8 s: cut
//!   power regardless;
//! - SD card removed: ask Linux to power off, cut power after a grace period;
//! - battery critically low (and not charging) for 30 s: the same.
//!
//! Power off = drop the power latch, GPIO L1. Linux asks
//! for it with POWER_OFF (its power-off handler), so the latch is driven by
//! one core only.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::hw::{self, GPIO_DIR, GPIO_L, GPIO_OUT, GPIO_T};
use crate::input::POWER_BIT;
use crate::shm;

const POWER_LATCH: u32 = 1 << 1; // L1
const CARD_DETECT: u32 = 1 << 13; // T13, low = card present

pub const CHECK_POWER_HOLD: u32 = 1 << 0;
pub const CHECK_CARD: u32 = 1 << 1;
pub const CHECK_BATTERY: u32 = 1 << 2;

/// POWER_REQUEST reasons (Linux driver: same values).
pub const REASON_CARD: u32 = 1;
pub const REASON_BATTERY: u32 = 2;
pub const REASON_BUTTON: u32 = 3;

/// Power button held this long: ask Linux to power off.
const POWER_ASK_MS: u32 = 2000;
const POWER_ASK_GRACE_MS: u32 = 10_000;
/// Held this long: power off whatever Linux is doing.
const POWER_HOLD_MS: u32 = 8000;
const CARD_DEBOUNCE_MS: u32 = 200;
const CARD_GRACE_MS: u32 = 10_000;
/// Battery: stock thresholds put "low" below 1020 mV at the ADC pin and full
/// at ~1320 mV; charging when the charge input reads over 400 mV.
const BATTERY_CRITICAL_MV: u32 = 990;
const CHARGING_MV: u32 = 400;
const BATTERY_LOW_SECONDS: u32 = 30;
const BATTERY_GRACE_MS: u32 = 30_000;

/// Enabled checks (CONFIG message).
pub static CHECKS: AtomicU32 = AtomicU32::new(CHECK_POWER_HOLD | CHECK_CARD | CHECK_BATTERY);

pub struct Supervisor {
    power_since: Option<u32>,
    /// This press already asked Linux to power off.
    power_asked: bool,
    card: Option<bool>,
    card_raw: bool,
    card_since: u32,
    low_seconds: u32,
    last_second: u32,
    /// Pending power-off: reason, deadline (ms).
    request: Option<(u32, u32)>,
}

pub enum Action {
    None,
    /// Ask Linux to shut down (reason, grace ms).
    Request(u32, u32),
}

impl Supervisor {
    pub const fn new() -> Supervisor {
        Supervisor {
            power_since: None,
            power_asked: false,
            card: None,
            card_raw: false,
            card_since: 0,
            low_seconds: 0,
            last_second: 0,
            request: None,
        }
    }

    /// Called from the main loop; `now` in ms, `input` the debounced state.
    pub fn poll(&mut self, now: u32, input: u64) -> Action {
        let checks = CHECKS.load(Ordering::Relaxed);
        let mut action = Action::None;

        if input & POWER_BIT == 0 {
            self.power_since = None;
            self.power_asked = false;
        } else {
            let since = *self.power_since.get_or_insert(now);
            let held = now.wrapping_sub(since);
            if checks & CHECK_POWER_HOLD != 0 {
                if held >= POWER_ASK_MS && !self.power_asked {
                    self.power_asked = true;
                    log!("power button held {} s: asking Linux to power off", POWER_ASK_MS / 1000);
                    action = self.request(now, REASON_BUTTON, POWER_ASK_GRACE_MS);
                }
                if held >= POWER_HOLD_MS {
                    log!("power button held {} s: powering off", POWER_HOLD_MS / 1000);
                    power_off();
                }
            }
        }

        let raw = hw::gpio_in(GPIO_T) & CARD_DETECT == 0;
        if raw != self.card_raw {
            self.card_raw = raw;
            self.card_since = now;
        }
        if now.wrapping_sub(self.card_since) >= CARD_DEBOUNCE_MS && self.card != Some(raw) {
            if self.card == Some(true) && !raw {
                log!("SD card removed");
                if checks & CHECK_CARD != 0 {
                    action = self.request(now, REASON_CARD, CARD_GRACE_MS);
                }
            }
            self.card = Some(raw);
            shm::write(shm::H_CARD, raw as u32);
        }

        let second = now / 1000;
        if second != self.last_second {
            self.last_second = second;
            let battery = hw::adc_mv(1);
            let charger = hw::adc_mv(5);
            shm::write(shm::H_BATTERY_MV, battery.unwrap_or(0));
            shm::write(shm::H_CHARGER_MV, charger.unwrap_or(0));
            let charging = charger.is_some_and(|mv| mv > CHARGING_MV);
            match battery {
                Some(mv) if mv < BATTERY_CRITICAL_MV && !charging => self.low_seconds += 1,
                _ => self.low_seconds = 0,
            }
            if self.low_seconds == BATTERY_LOW_SECONDS && checks & CHECK_BATTERY != 0 {
                log!("battery critical ({} mV): requesting power-off", battery.unwrap_or(0));
                action = self.request(now, REASON_BATTERY, BATTERY_GRACE_MS);
            }
        }

        if let Some((reason, deadline)) = self.request {
            if (now.wrapping_sub(deadline) as i32) >= 0 {
                log!("Linux did not power off (reason {reason}): cutting power");
                power_off();
            }
        }
        shm::write(shm::H_SUPERVISOR, self.request.map_or(0, |r| r.0) << 16 | checks);
        action
    }

    fn request(&mut self, now: u32, reason: u32, grace: u32) -> Action {
        if self.request.is_some() {
            return Action::None;
        }
        self.request = Some((reason, now.wrapping_add(grace)));
        Action::Request(reason, grace)
    }
}

/// Drops the power latch. Does not return: should power stay on, park (so
/// Linux can still reload the firmware).
pub fn power_off() -> ! {
    hw::gpio_update(GPIO_L, GPIO_OUT, POWER_LATCH, 0);
    hw::gpio_update(GPIO_L, GPIO_DIR, POWER_LATCH, POWER_LATCH);
    crate::park()
}
