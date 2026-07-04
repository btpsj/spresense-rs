//! Blocking UART1 console demo on the `embassy-cxd56` hand-rolled PL011 driver.
//!
//! Prints a counting greeting on UART1, which is wired to the on-board CP2102N
//! USB-serial bridge. Connect at 115200 8N1.
//!
//! Uses `cortex-m-rt` + a busy-wait delay — no async executor.

#![no_std]
#![no_main]

use core::fmt::Write;

use cortex_m::asm;
use cortex_m_rt::entry;
use panic_halt as _;

use embassy_cxd56::uart::{Config, Uart};

/// UART1 base (COM) clock in Hz — the baud-divisor reference.
///
/// COM is derived at runtime (`sys / (ckdiv_com + 1)`), so this crate's
/// "quickest" clock story has the caller supply it here rather than computing
/// it. **Confirm/adjust this for your board** — read the real value via
/// `cxd56-hal`'s `Clocks::com`, or scope the TX line. A wrong value yields the
/// wrong baud rate (garbled output); zero yields `UartError::BadBaud`.
const COM_HZ: u32 = 48_000_000;

#[entry]
fn main() -> ! {
    let p = embassy_cxd56::init();

    let mut uart = Uart::new(
        p.UART1,
        p.SPI0_CS_X,
        p.SPI0_SCK,
        Config {
            baud_rate: 115_200,
            src_clk_hz: COM_HZ,
            ..Default::default()
        },
    )
    .unwrap();

    let mut n: u32 = 0;
    loop {
        let _ = writeln!(uart, "hello from embassy-cxd56 uart, n={n}");
        n = n.wrapping_add(1);
        asm::delay(76_800_000); // ~0.5 s at 153.6 MHz
    }
}
