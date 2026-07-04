//! Blink LED0 (GP_I2S1_BCK / pin 97) and read GP_I2C4_BCK (pin 1).
//!
//! Build: cargo build --example gpio_blink --target thumbv7em-none-eabihf --features rt
//!
//! This example verifies the GP_* registers under TOPREG in patch.yml:
//!   - TOPREG baseAddress 0x04100000
//!   - GP_I2C4_BCK  at offset 0x2000  (TOPREG+0x2000, abs 0x04102000)
//!   - GP_I2S1_BCK  at offset 0x2168  (TOPREG+0x2168, abs 0x04102168)
//!   - DIR[16] active-low (0 = output enabled)
//!   - OUT[8]  data bit
//!   - IN[0]   sampled input (read)

#![no_std]
#![no_main]

use cortex_m::asm;
use cortex_m_rt::entry;
use panic_halt as _;

use cxd56_pac_svd2rust::*;

/// Cycles to spin between LED toggles at ~153.6 MHz (≈ 0.5 s).
const DELAY_CYCLES: u32 = 76_800_000;

#[entry]
fn main() -> ! {
    // Safety: no other code runs on this core before this point.
    let topreg = unsafe { Topreg::steal() };

    // Configure GP_I2S1_BCK (LED0) as output, initially low.
    // DIR is active-low: clear_bit() = 0 = drive output.
    topreg.gp_i2s1_bck().write(|w| w.dir().clear_bit().out().clear_bit());

    // Leave GP_I2C4_BCK (pin 1) as input (reset default: DIR=1, high-Z input).

    loop {
        // Read GP_I2C4_BCK input level.
        let pin1_high = topreg.gp_i2c4_bck().read().in_().bit_is_set();

        // Toggle LED to match the input level (high → LED on, low → LED off).
        topreg.gp_i2s1_bck().modify(|_, w| {
            if pin1_high {
                w.out().set_bit()
            } else {
                w.out().clear_bit()
            }
        });

        asm::delay(DELAY_CYCLES);

        // Toggle LED off regardless, so there is always a visible blink.
        topreg.gp_i2s1_bck().modify(|_, w| w.out().clear_bit());
        asm::delay(DELAY_CYCLES);
    }
}