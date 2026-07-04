#![no_std]
#![no_main]

use cortex_m::asm;
use cortex_m_rt::entry;
use panic_halt as _;

use embassy_cxd56::gpio::{Level, Output, OutputDrive};

// ~0.5 s at 153.6 MHz
const DELAY_CYCLES: u32 = 76_800_000;

#[entry]
fn main() -> ! {
    let p = embassy_cxd56::init();
    let mut led = Output::new(p.P1w_00, Level::Low, OutputDrive::Standard);

    loop {
        led.set_high();
        asm::delay(DELAY_CYCLES);
        led.set_low();
        asm::delay(DELAY_CYCLES);
    }
}
