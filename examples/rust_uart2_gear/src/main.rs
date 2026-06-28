//! UART2 with a reconfigured gear divider.
//!
//! UART2 (IMG_UART) lives in the APP_SUB domain and derives its base clock
//! from the APP CPU clock through an M/N "gear" divider: base = appsmp / M.
//! The HAL default is M=4 (≈39 MHz at HP); this example selects M=2 (≈78 MHz)
//! via [`Config::gear`], doubling the maximum reachable baud rate (PL011 max
//! baud = base / 16). The configured divisor is programmed into hardware when
//! `Uart::new` enables the peripheral, and the `Clock` snapshot reflects it
//! from the start — no set-before-snapshot ordering to get wrong.
//!
//! Wiring: UART2 is on the JP1 extension header — TXD = P1n_00 (JP1 pin 2),
//! RXD = P1n_01 (JP1 pin 3). Attach a 3.3 V serial adapter at 921 600 baud to
//! see the stream and echo. The USB console (UART1) prints the clock report,
//! so the gear reconfiguration is observable without any extra hardware.

#![no_std]
#![no_main]

use core::fmt::Write;

use cortex_m::Peripherals;
use cortex_m_rt::entry;
use embedded_hal::delay::DelayNs;
use panic_halt as _;

use cxd56_hal::{clocks::{Config, GearConfig, PeripheralId, RccExt}, uart::UartConfig};
use cxd56_hal::delay::Delay;
use cxd56_hal::gpio::pins::Parts;
use cxd56_hal::pac;
use cxd56_hal::uart::{Uart, Uart1Pins, Uart2Pins};

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();
    let core = Peripherals::take().unwrap();

    let gear_reset = PeripheralId::ImgUart.gear_divisor();

    // UART2 base clock = appsmp / 2 (≈78 MHz at HP) instead of the default
    // appsmp / 4. The divisor is written to hardware when the driver enables
    // the peripheral; the Clock snapshot below already reflects it.
    let cfg = Config {
        gear: GearConfig { img_uart: 2, ..Default::default() },
        ..Default::default()
    };
    let crg = pac.crg.constrain(cfg);

    let clocks = crg.into_clock();

    let mut delay = Delay::new(core.SYST, &clocks);

    let parts = Parts::new(pac.topreg);
    let uart1_pins = Uart1Pins { tx: parts.gp_spi0_cs_x, rx: parts.gp_spi0_sck };
    let uart2_pins = Uart2Pins { tx: parts.gp_uart2_txd, rx: parts.gp_uart2_rxd };

    // UART1: USB console at the default 115 200 baud, for the clock report.
    let mut console = Uart::new(pac.uart1, uart1_pins, Default::default(), &clocks).unwrap();

    let appsmp = clocks.appsmp().hz().to_Hz();
    let img_uart = clocks.img_uart().hz().to_Hz();
    let _ = writeln!(console, "appsmp:        {appsmp} Hz");
    let _ = writeln!(console, "gear at reset: {gear_reset:?}");
    let _ = writeln!(console, "img_uart:      {img_uart} Hz (appsmp / 2, from Config::gear)");
    let _ = writeln!(console, "max baud:      {} (img_uart / 16)", img_uart / 16);

    // UART2 on JP1 at 921 600 baud, clocked from the reconfigured base.
    let config = UartConfig { baud_rate: 921_600, ..Default::default() };
    let mut uart2 = Uart::new(pac.uart2, uart2_pins, config, &clocks).unwrap();

    // enable() inside Uart::new programmed the configured divisor — read the
    // live register back to show hardware now matches the configuration.
    let _ = writeln!(
        console,
        "gear in hw:    {:?} (programmed at enable)",
        PeripheralId::ImgUart.gear_divisor()
    );

    let mut n: u32 = 0;
    loop {
        let _ = writeln!(uart2, "hello from uart2 at 921600 baud, n={n}");
        while let Some(byte) = uart2.read_byte() {
            uart2.write_byte(byte);
        }
        let _ = writeln!(console, "uart2 tick n={n}");
        n = n.wrapping_add(1);
        delay.delay_ms(500);
    }
}
