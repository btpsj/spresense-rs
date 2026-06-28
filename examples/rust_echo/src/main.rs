#![no_std]
#![no_main]

use cortex_m_rt::entry;
use embedded_io::{Read, Write};
use panic_halt as _;

use cxd56_hal::pac;
use cxd56_hal::{
    clocks::{Config, RccExt},
    gpio::pins::Parts,
    uart::{Uart, Uart1Pins},
};

// Read a single byte at a time from the uart and store it in the buf until we
// get a line ending character '\r' or '\n'. Handles backspaces as well
fn read_line<R: Read>(uart: &mut R, buf: &mut [u8]) -> Result<usize, R::Error> {
    let mut pos: usize = 0;
    loop {
        let mut byte = [0u8; 1];
        uart.read(&mut byte)?;

        match byte[0] {
            b'\n' | b'\r' => break,

            // Backspace (0x08) or DEL (0x7F)
            0x08 | 0x7F => pos = pos.saturating_sub(1usize),

            b => {
                if pos < buf.len() {
                    buf[pos] = b;
                    pos += 1;
                }
            }
        }
    }
    Ok(pos)
}

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();

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

    let mut buf = [0u8; 256];
    loop {
        read_line(&mut uart, &mut buf).unwrap();
        uart.write_all(&buf).unwrap();
        uart.write_all(b"\r\n").unwrap();
        buf.fill(0);
    }
}
