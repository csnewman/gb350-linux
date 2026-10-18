//! A bit-banged I2C bus scan on two bank-L GPIOs (corelink test service,
//! tag 4): which 7-bit addresses answer. For finding what is wired to L17 /
//! L18 (the stock RTOS's `/dev/i2c0` pins). Open drain: a line is driven
//! low (output, 0) or released (input; the board's pull-ups take it high).
//! Bank L output / direction changes go through `gpio_update` (under the
//! lock Linux's GPIO driver takes too), touching only these two pins.

use crate::hw::{self, GPIO_DIR, GPIO_L, GPIO_OUT};

const HALF_US: u32 = 5; // ~100 kHz

struct Bus {
    scl: u32,
    sda: u32,
}

impl Bus {
    fn low(&self, m: u32) {
        hw::gpio_update(GPIO_L, GPIO_DIR, m, m);
    }

    fn release(&self, m: u32) {
        hw::gpio_update(GPIO_L, GPIO_DIR, m, 0);
    }

    fn read(&self, m: u32) -> bool {
        hw::gpio_in(GPIO_L) & m != 0
    }

    /// SCL released, waiting out a device stretching the clock (up to 1 ms).
    fn scl_high(&self) -> bool {
        self.release(self.scl);
        for _ in 0..200 {
            if self.read(self.scl) {
                return true;
            }
            hw::delay_us(HALF_US);
        }
        false
    }

    fn start(&self) -> bool {
        self.release(self.sda);
        if !self.scl_high() {
            return false;
        }
        hw::delay_us(HALF_US);
        self.low(self.sda);
        hw::delay_us(HALF_US);
        self.low(self.scl);
        true
    }

    fn stop(&self) {
        self.low(self.sda);
        hw::delay_us(HALF_US);
        self.scl_high();
        hw::delay_us(HALF_US);
        self.release(self.sda);
        hw::delay_us(HALF_US);
    }

    fn bit_out(&self, one: bool) -> bool {
        if one {
            self.release(self.sda)
        } else {
            self.low(self.sda)
        }
        hw::delay_us(HALF_US);
        let ok = self.scl_high();
        hw::delay_us(HALF_US);
        self.low(self.scl);
        ok
    }

    fn bit_in(&self) -> bool {
        self.release(self.sda);
        hw::delay_us(HALF_US);
        self.scl_high();
        hw::delay_us(HALF_US);
        let b = self.read(self.sda);
        self.low(self.scl);
        b
    }

    /// Sends a byte; true if the device acknowledged (pulled SDA low).
    fn write(&self, v: u8) -> bool {
        for i in (0..8).rev() {
            self.bit_out(v >> i & 1 != 0);
        }
        !self.bit_in()
    }

    /// Reads a byte and does not acknowledge it (the last byte).
    fn read_last(&self) -> u8 {
        let mut v = 0;
        for _ in 0..8 {
            v = v << 1 | self.bit_in() as u8;
        }
        self.bit_out(true); // NACK
        v
    }
}

/// Scans 0x08-0x77 with SCL on bank-L pin `scl` and SDA on `sda`: a read of
/// one byte (NACKed) at each address. Returns (status, bitmap of answering
/// addresses, the byte the first one returned): status 0 done, 1 bad pins,
/// 2 the bus was not idle (a line held low), 3 the clock stayed low.
pub fn scan(scl: u32, sda: u32) -> (u32, [u32; 4], u32) {
    let mut map = [0u32; 4];
    let mut first = 0xffff_ffff;
    if scl > 31 || sda > 31 || scl == sda {
        return (1, map, first);
    }
    let bus = Bus { scl: 1 << scl, sda: 1 << sda };
    let both = bus.scl | bus.sda;
    // Outputs drive 0 when enabled; start released (inputs).
    hw::gpio_update(GPIO_L, GPIO_DIR, both, 0);
    hw::gpio_update(GPIO_L, GPIO_OUT, both, 0);
    hw::delay_us(20);
    if hw::gpio_in(GPIO_L) & both != both {
        return (2, map, first);
    }
    let mut status = 0;
    for addr in 0x08u8..=0x77 {
        if !bus.start() {
            status = 3;
            break;
        }
        if bus.write(addr << 1 | 1) {
            map[addr as usize / 32] |= 1 << (addr % 32);
            let v = bus.read_last();
            if first == 0xffff_ffff {
                first = (addr as u32) << 8 | v as u32;
            }
        }
        bus.stop();
        hw::delay_us(50);
    }
    hw::gpio_update(GPIO_L, GPIO_DIR, both, 0);
    (status, map, first)
}
