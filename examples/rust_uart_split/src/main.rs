#![no_std]
#![no_main]

//! UART split demo: build UART1, split it into independent TX and RX halves,
//! and run a byte-at-a-time echo where the read and write paths are owned by
//! separate values. The halves implement only their own direction's
//! `embedded-io` trait — `rx` is `Read`-only, `tx` is `Write`-only — so this
//! example would not compile if the directions were mixed up.

use cortex_m_rt::entry;
use embedded_io::{Read, Write};
use panic_halt as _;

use cxd56_hal::clocks::{Config, RccExt};
use cxd56_hal::gpio::pins::Parts;
use cxd56_hal::pac;
use cxd56_hal::uart::{Uart, Uart1Pins};

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();
    let clocks = pac.crg.constrain(Config::default()).into_clock();

    // Build the full UART1 driver (CP2102N USB-serial bridge on the main board).
    let parts = Parts::new(pac.topreg);
    let uart1_pins = Uart1Pins {
        tx: parts.gp_spi0_cs_x,
        rx: parts.gp_spi0_sck,
    };
    let uart = Uart::new(pac.uart1, uart1_pins, Default::default(), &clocks).unwrap();

    // Split into independent halves. `tx` keeps the transmit path, `rx` the
    // receive path; the UART stays enabled across the split.
    let (mut tx, mut rx) = uart.split();

    let _ = tx.write_all(b"uart split echo ready\r\n");

    // Echo loop: pull one byte through the RX half, push it through the TX half.
    let mut byte = [0u8; 1];
    loop {
        if rx.read(&mut byte).is_ok() {
            let _ = tx.write_all(&byte);
        }
    }
}
