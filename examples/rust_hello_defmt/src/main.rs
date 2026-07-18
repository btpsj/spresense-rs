#![no_std]
#![no_main]

use cortex_m::Peripherals;
use cortex_m_rt::entry;
use cxd56_blink_debug::{sos, strobe};
use defmt::*;
use defmt_serial as _;
use embedded_hal::delay::DelayNs;
use panic_probe as _;
use static_cell::StaticCell;

use cxd56_hal::delay::Delay;
use cxd56_hal::gpio::Level;
use cxd56_hal::pac;
use cxd56_hal::{
    clocks::{Clock, Config, Hp, RccExt},
    gpio::pins::Parts,
    uart::{Uart, Uart1Pins},
};

static SERIAL: StaticCell<Uart<'static, pac::Uart1>> = StaticCell::new();
// UART1 now borrows the `Clock` for its lifetime (COM is a Dyn clock), so the
// `Clock` must outlive the `'static` UART stored in `SERIAL`.
static CLOCK: StaticCell<Clock<Hp>> = StaticCell::new();

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();
    let core = Peripherals::take().unwrap();

    let crg = pac.crg.constrain(Config::default());
    // Promote the clock to `'static` so the UART1 console (which borrows it)
    // can be stored in the `'static` `SERIAL` cell.
    let clock = CLOCK.init(crg.into_hp_clock().expect("lock Hp"));

    // UART1 for console output. COM is a Dyn clock → the UART borrows `clock`.
    let parts = Parts::new(pac.topreg);
    let uart1_pins = Uart1Pins {
        tx: parts.gp_spi0_cs_x,
        rx: parts.gp_spi0_sck,
    };
    let uart =
        Uart::new(pac.uart1, uart1_pins, Default::default(), clock).expect("uart1 init failed");

    let mut led = parts.gp_i2s1_bck.into_output(Level::Low);
    let mut delay = Delay::new(core.SYST, clock);

    sos(&mut led, &mut delay);

    defmt_serial::defmt_serial(SERIAL.init(uart));

    let mut n: u32 = 0;
    loop {
        info!("hello from spresense rust, n={}", n);
        n = n.wrapping_add(1);
        delay.delay_ms(500);
        strobe(&mut led, &mut delay);
    }
}
