#![no_std]
#![no_main]

use core::fmt::Write;

use cortex_m::Peripherals;
use cortex_m_rt::entry;
use embedded_hal::delay::DelayNs;
use panic_halt as _;

use cxd56_blink_debug::{sos, strobe};
use cxd56_hal::pac;
use cxd56_hal::uart::Uart;
use cxd56_hal::{
    clocks::{Config, RccExt},
    delay::Delay,
};
use cxd56_hal::{
    gpio::{Level, pins::Parts},
    uart::Uart1Pins,
};

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();
    let core = Peripherals::take().unwrap();

    let crg = pac.crg.constrain(Config::default());
    let clock = crg.into_clock();

    // UART1 for console output. COM is a Dyn clock → the UART borrows `clock`.
    let parts = Parts::new(pac.topreg);
    let uart1_pins = Uart1Pins {
        tx: parts.gp_spi0_cs_x,
        rx: parts.gp_spi0_sck,
    };
    let mut uart =
        Uart::new(pac.uart1, uart1_pins, Default::default(), &clock).expect("uart1 init failed");
    let mut led = parts.gp_i2s1_bck.into_output(Level::Low);
    let mut delay = Delay::new(core.SYST, &clock);

    sos(&mut led, &mut delay);

    let mut n: u32 = 0;
    loop {
        let _ = writeln!(uart, "hello from spresense rust, n={n}");
        n = n.wrapping_add(1);
        delay.delay_ms(500);
        strobe(&mut led, &mut delay);
    }
}
