#![no_std]
#![no_main]

use core::fmt::Write;

use cortex_m::Peripherals;
use cortex_m_rt::entry;
use embedded_hal::delay::DelayNs;
use panic_halt as _;

use cxd56_hal::clocks::{Config, RccExt};
use cxd56_hal::gpio::pins::Parts;
use cxd56_hal::pac;
use cxd56_hal::uart::{Uart, Uart1Pins};
use cxd56_hal::{delay::Delay};

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();
    let core = Peripherals::take().unwrap();

    let crg = pac.crg.constrain(Config::default());
    let clocks = crg.into_clock();

    let mut delay = Delay::new(core.SYST, &clocks);

    let parts = Parts::new(pac.topreg);
    let uart1_pins = Uart1Pins { tx: parts.gp_spi0_cs_x, rx: parts.gp_spi0_sck };
    let mut uart = Uart::new(pac.uart1, uart1_pins, Default::default(), &clocks).unwrap();

    let mut n: u32 = 0;
    loop {
        let _ = writeln!(uart, "hello from spresense rust, n={n}");
        n = n.wrapping_add(1);
        delay.delay_ms(500);
    }
}
