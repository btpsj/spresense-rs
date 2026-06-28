//! Single-core LED chase — drives all four Spresense main-board LEDs in order.
//!
//! The four on-board LEDs are wired to the I2S1 pins (`gp_i2s1_bck` = LED0,
//! `gp_i2s1_lrck` = LED1, `gp_i2s1_data_in` = LED2, `gp_i2s1_data_out` = LED3).
//! This runs entirely on `Core0` (no multicore) and lights each LED in turn,
//! cycling 0 → 1 → 2 → 3 forever. It exists to confirm that every board LED is
//! individually drivable through the HAL before the multicore example relies on
//! a second LED pin.

#![no_std]
#![no_main]

use core::convert::Infallible;

use cortex_m::Peripherals;
use cortex_m_rt::entry;
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::OutputPin;
use panic_halt as _;

use cxd56_hal::clocks::{Config, RccExt};
use cxd56_hal::delay::Delay;
use cxd56_hal::gpio::{pins, Level};
use cxd56_hal::pac;

/// How long each LED stays lit before the chase advances, in milliseconds.
const STEP_MS: u32 = 250;

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();
    let core = Peripherals::take().unwrap();

    let clock = pac.crg.constrain(Config::default()).into_clock();

    // All four board LEDs are on the I2S1 pins; configure each as a low output.
    let pins = pins::Parts::new(pac.topreg);
    let mut led0 = pins.gp_i2s1_bck.into_output(Level::Low);
    let mut led1 = pins.gp_i2s1_lrck.into_output(Level::Low);
    let mut led2 = pins.gp_i2s1_data_in.into_output(Level::Low);
    let mut led3 = pins.gp_i2s1_data_out.into_output(Level::Low);

    let mut delay = Delay::new(core.SYST, &clock);

    // Walk LED0 → LED1 → LED2 → LED3 (the physical order across the board).
    let mut leds: [&mut dyn OutputPin<Error = Infallible>; 4] =
        [&mut led0, &mut led1, &mut led2, &mut led3];

    loop {
        for led in leds.iter_mut() {
            led.set_high().unwrap();
            delay.delay_ms(STEP_MS);
            led.set_low().unwrap();
        }
    }
}
