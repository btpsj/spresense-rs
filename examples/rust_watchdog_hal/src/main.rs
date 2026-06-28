//! SP805 watchdog demo.
//!
//! Phase 1 (healthy): blink LED0 and `feed()` the watchdog for ~10 s — the
//! board stays up. Phase 2 (fault): stop feeding and spin; ~4 s later the
//! watchdog resets the chip and the whole sequence restarts, so on hardware you
//! see the fast healthy blink, a stall, then a reboot back to phase 1.

#![no_std]
#![no_main]

use cortex_m::Peripherals;
use cortex_m_rt::entry;
use embedded_hal::delay::DelayNs;
use fugit::ExtU32; // brings `.millis()` into scope
use panic_halt as _;

use cxd56_hal::clocks::{Config, RccExt};
use cxd56_hal::delay::Delay;
use cxd56_hal::gpio::{Level, pins};
use cxd56_hal::pac;
use cxd56_hal::watchdog::Watchdog;

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();
    let core = Peripherals::take().unwrap();

    // Profile-aware clock (owns the CRG). The watchdog's reload is derived from
    // the perf-dependent CPU base clock, so we need the `Clock` (not a `Clocks`
    // snapshot) to construct the watchdog.
    let clock = pac.crg.constrain(Config::default()).into_clock();

    let pins = pins::Parts::new(pac.topreg);
    let mut led = pins.gp_i2s1_bck.into_output(Level::Low); // LED0
    let mut delay = Delay::new(core.SYST, &clock);

    // 4-second watchdog. Borrowing `clock` keeps `request_perf` locked out for
    // the watchdog's lifetime, so the computed timeout cannot go stale.
    let mut wdt = Watchdog::new(pac.wdog, 4000u32.millis(), &clock).unwrap();
    wdt.start();

    // Phase 1: feed every ~500 ms for ~10 s — well within the 4 s timeout.
    for _ in 0..10 {
        led.set_high();
        delay.delay_ms(250);
        led.set_low();
        delay.delay_ms(250);
        wdt.feed();
    }

    // Phase 2: stop feeding. After ~4 s the watchdog resets the chip.
    loop {
        led.set_high();
        delay.delay_ms(50);
        led.set_low();
        delay.delay_ms(50);
        // intentionally no wdt.feed() -> reset
    }
}
