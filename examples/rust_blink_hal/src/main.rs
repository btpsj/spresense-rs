#![no_std]
#![no_main]

use cortex_m::Peripherals;
use cortex_m_rt::entry;
use embedded_hal::delay::DelayNs;
use panic_halt as _;

use cxd56_hal::clocks::{Config, RccExt};
use cxd56_hal::delay::Delay;
use cxd56_hal::gpio::{pins, Level};
use cxd56_hal::pac;

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();
    let core = Peripherals::take().unwrap();

    let clock = pac.crg.constrain(Config::default()).into_clock();

    let pins = pins::Parts::new(pac.topreg);
    let mut led = pins.gp_i2s1_bck.into_output(Level::Low);
    let mut delay = Delay::new(core.SYST, &clock);

    loop {
        led.set_high();
        delay.delay_ms(500);
        led.set_low();
        delay.delay_ms(500);
    }
}
