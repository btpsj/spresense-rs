//! Red/green dual-colour LED on the CommonSense board.
//!
//! The LED (LTST-C195KGJRKT) is driven by the **PCA9538ABSHP** 8-bit I2C I/O
//! expander at address `0x70` (A0=GND, A1=GND) on the I2C0 bus:
//!
//! | Expander port | Signal | Colour |
//! |:-------------:|:------:|:------:|
//! | P4            | LED_G  | Green  |
//! | P5            | LED_R  | Red    |
//!
//! Both outputs are **active-low**: driving the pin low turns the LED on.
//! All other PCA9538 ports (P0 = user button, P1 = ACC_INT2, P2/P3/P6/P7 =
//! unused) are left as inputs so this driver does not interfere with them.
//!
//! # Bus sharing
//!
//! `RgLed<I>` is generic over any `embedded_hal::i2c::I2c` implementation.
//! When the I2C0 bus must be shared with sensors, wrap the bus once in a
//! `core::cell::RefCell` and pass an
//! [`embedded_hal_bus::i2c::RefCellDevice`] to each driver:
//!
//! ```ignore
//! use core::cell::RefCell;
//! use embedded_hal_bus::i2c::RefCellDevice;
//! use cxd56_hal::i2c::{I2c0, I2cConfig};
//! use spresense_bsp::rg_led::RgLed;
//!
//! let i2c0 = I2c0::new(pac.i2c0, &clocks, I2cConfig::default())?;
//! let bus  = RefCell::new(i2c0);
//!
//! let mut rg = RgLed::new(RefCellDevice::new(&bus))?;
//! rg.green_on()?;
//! ```

use embedded_hal::i2c::I2c;

// ── PCA9538 register map ────────────────────────────────────────────────────
const ADDR: u8 = 0x70;

/// Output-port latch (R/W). 0 = drive low, 1 = drive high. POR = 0xFF.
const REG_OUTPUT: u8 = 0x01;

/// Configuration register. 0 = output, 1 = input. POR = 0xFF (all inputs).
const REG_CONFIG: u8 = 0x03;

// ── LED bit positions on the port byte ──────────────────────────────────────
/// P4 — green LED, active-low.
const LED_G: u8 = 1 << 4;
/// P5 — red LED, active-low.
const LED_R: u8 = 1 << 5;

/// Configuration mask: P4 and P5 as outputs, all others remain inputs.
const CONFIG_MASK: u8 = !(LED_G | LED_R);

// ── Driver ──────────────────────────────────────────────────────────────────

/// Red/green LED controller backed by the PCA9538 I2C I/O expander.
///
/// Construct with [`RgLed::new`].  Every mutating method returns
/// `Result<(), I::Error>` so I2C bus errors are visible to callers.
pub struct RgLed<I> {
    i2c: I,
    /// Cached copy of the PCA9538 output-port register.
    /// Each state change is a single 2-byte I2C write; the cache is only
    /// committed after a successful transaction, keeping software state
    /// consistent with the hardware latch on error.
    out: u8,
}

impl<I: I2c> RgLed<I> {
    /// Initialise the PCA9538, configure P4 (green) and P5 (red) as outputs,
    /// and leave both LEDs off.
    ///
    /// The latch is written before the direction register so there is no
    /// momentary glitch on power-up.
    pub fn new(mut i2c: I) -> Result<Self, I::Error> {
        // Pre-set output latch: both LEDs off (high = off for active-low).
        let out: u8 = 0xFF;
        i2c.write(ADDR, &[REG_OUTPUT, out])?;
        // Configure P4 + P5 as outputs; leave all other ports as inputs.
        i2c.write(ADDR, &[REG_CONFIG, CONFIG_MASK])?;
        Ok(Self { i2c, out })
    }

    // ── Red channel ─────────────────────────────────────────────────────────

    /// Turn the red LED on.
    pub fn red_on(&mut self) -> Result<(), I::Error> {
        self.set_red(true)
    }

    /// Turn the red LED off.
    pub fn red_off(&mut self) -> Result<(), I::Error> {
        self.set_red(false)
    }

    /// Set the red LED state: `true` = on, `false` = off.
    pub fn set_red(&mut self, on: bool) -> Result<(), I::Error> {
        self.set_bit(LED_R, on)
    }

    /// Returns `true` if the red LED is currently on.
    pub fn is_red_on(&self) -> bool {
        // Active-low: bit clear in cache means output is low means LED is on.
        self.out & LED_R == 0
    }

    // ── Green channel ────────────────────────────────────────────────────────

    /// Turn the green LED on.
    pub fn green_on(&mut self) -> Result<(), I::Error> {
        self.set_green(true)
    }

    /// Turn the green LED off.
    pub fn green_off(&mut self) -> Result<(), I::Error> {
        self.set_green(false)
    }

    /// Set the green LED state: `true` = on, `false` = off.
    pub fn set_green(&mut self, on: bool) -> Result<(), I::Error> {
        self.set_bit(LED_G, on)
    }

    /// Returns `true` if the green LED is currently on.
    pub fn is_green_on(&self) -> bool {
        self.out & LED_G == 0
    }

    // ── Combined ─────────────────────────────────────────────────────────────

    /// Turn off both LEDs in a single I2C transaction.
    pub fn both_off(&mut self) -> Result<(), I::Error> {
        let next = self.out | LED_G | LED_R;
        self.flush(next)
    }

    // ── Bus access ───────────────────────────────────────────────────────────

    /// Return the underlying I2C handle, releasing it for other drivers.
    pub fn free(self) -> I {
        self.i2c
    }

    // ── Private ──────────────────────────────────────────────────────────────

    /// Set or clear a single LED bit in the output latch.
    fn set_bit(&mut self, mask: u8, on: bool) -> Result<(), I::Error> {
        let next = if on {
            self.out & !mask // clear bit → output low → LED on  (active-low)
        } else {
            self.out | mask  // set   bit → output high → LED off
        };
        self.flush(next)
    }

    /// Write `next` to the PCA9538 output register and commit it to the cache
    /// only on success.
    fn flush(&mut self, next: u8) -> Result<(), I::Error> {
        self.i2c.write(ADDR, &[REG_OUTPUT, next])?;
        self.out = next;
        Ok(())
    }
}
