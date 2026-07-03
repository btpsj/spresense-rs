//! LPADC read demo — blob-free, two-channel (A2=SEN_AIN4, A3=SEN_AIN5).
//!
//! Prints raw 10-bit ADC codes over UART1 in a loop. Full-scale ≈ 0.7 V
//! (VDDA_LPADC). Do NOT exceed 0.7 V on the A2/A3 pins.
//!
//! # Wiring
//!
//! Connect a signal (≤ 0.7 V) or leave floating:
//!
//! | Arduino | Spresense pad | JP2 pin |
//! |---------|---------------|---------|
//! | A2      | SEN_AIN4      | JP2-33  |
//! | A3      | SEN_AIN5      | JP2-35  |
//!
//! # Run
//!
//! ```
//! cargo run --release
//! ```
//!
//! Expected output (serial monitor at 115200 baud):
//!
//! ```text
//! LPADC ready — A2(SEN_AIN4) A3(SEN_AIN5); full-scale ~0.7 V
//! A2=0000 A3=0000
//! A2=0512 A3=0768
//! ...
//! ```

#![no_std]
#![no_main]

use core::fmt::Write;

use cortex_m_rt::entry;
use panic_halt as _;

use cxd56_hal::adc::{Adc, AdcConfig, LpAdc, LpAdcChannel};
use cxd56_hal::clocks::{Config, RccExt};
use cxd56_hal::gpio::pins::Parts;
use cxd56_hal::pac;
use cxd56_hal::uart_alt::{Uart, Uart1Pins};

#[entry]
fn main() -> ! {
    let dp = pac::Peripherals::take().unwrap();

    let crg = dp.crg.constrain(Config::default());
    let clock = crg.into_clock();

    // UART1 for console output. COM is a Dyn clock → the UART borrows `clock`.
    let parts = Parts::new(dp.topreg);
    let uart1_pins = Uart1Pins { tx: parts.gp_spi0_cs_x, rx: parts.gp_spi0_sck };
    let mut uart = Uart::new(dp.uart1, uart1_pins, Default::default(), &clock)
        .expect("uart1 init failed");

    // Bring up LPADC (channels 2 and 3). The iSoP firmware is never released
    // from reset; reads go straight to the ADCIF hardware FIFO Read Ports.
    let mut adc = match Adc::new(LpAdc, dp.scu_adcif, AdcConfig::default(), &clock) {
        Ok(a) => a,
        Err(_) => {
            let _ = writeln!(uart, "LPADC init failed");
            loop {
                cortex_m::asm::wfe();
            }
        }
    };

    let _ = writeln!(
        uart,
        "LPADC ready — A2(SEN_AIN4) A3(SEN_AIN5); full-scale ~0.7 V"
    );

    loop {
        let ch2 = adc.read(LpAdcChannel::Ch2);
        let ch3 = adc.read(LpAdcChannel::Ch3);
        let _ = writeln!(uart, "A2={ch2:04} A3={ch3:04}");
    }
}
