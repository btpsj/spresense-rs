#![no_std]
#![no_main]

//! Brings up I2S0 as an I2S master at 48 kHz on the Spresense main board and
//! prints a heartbeat over the UART1 console.
//!
//! Pins (consumed by the driver): I2S0_BCK = JP1-6 / Arduino D26,
//! I2S0_LRCK = JP1-7 / D25, I2S0_DATA_IN = JP2-6 / D19,
//! I2S0_DATA_OUT = JP2-7 / D18.
//!
//! In master mode the audio block drives BCK/LRCK from the external audio MCLK,
//! so the clocks can be observed on D26/D25. For sample I/O and an external
//! `DATA_OUT`→`DATA_IN` (D18→D19) loopback test see `rust_i2s0_loopback`.

use core::fmt::Write;

use cortex_m::Peripherals;
use cortex_m_rt::entry;
use embedded_hal::delay::DelayNs;
use panic_halt as _;

use cxd56_hal::clocks::{Config, RccExt};
use cxd56_hal::delay::Delay;
use cxd56_hal::gpio::pins::Parts;
use cxd56_hal::i2s::{I2s, I2s0, I2s0Pins, I2sConfig};
use cxd56_hal::pac;
use cxd56_hal::uart::{Uart, Uart1Pins};

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();
    let core = Peripherals::take().unwrap();

    let crg = pac.crg.constrain(Config::default());
    let clocks = crg.into_hp_clock().expect("lock Hp");

    let mut delay = Delay::new(core.SYST, &clocks);
    let parts = Parts::new(pac.topreg);

    // UART1 console for status output.
    let uart1_pins = Uart1Pins {
        tx: parts.gp_spi0_cs_x,
        rx: parts.gp_spi0_sck,
    };
    let mut uart = Uart::new(pac.uart1, uart1_pins, Default::default(), &clocks).unwrap();

    // Bring up I2S0 as master @ 48 kHz, duplex. Consumes the four I2S0 pads and
    // the audio block. The MCLK is the external audio crystal (fixed), so `I2s`
    // holds no `&clocks` borrow once `new` returns.
    let i2s0_pins = I2s0Pins {
        bck: parts.gp_i2s0_bck,
        lrck: parts.gp_i2s0_lrck,
        data_in: parts.gp_i2s0_data_in,
        data_out: parts.gp_i2s0_data_out,
    };

    let _i2s = match I2s::<I2s0>::new(pac.audio, i2s0_pins, &clocks, I2sConfig::default()) {
        Ok(i2s) => {
            let _ = writeln!(uart, "I2S0 master @ 48kHz up — scope BCK=D26, LRCK=D25");
            i2s
        }
        Err(e) => {
            let _ = writeln!(uart, "I2S0 bring-up failed: {e:?}");
            loop {
                delay.delay_ms(1000);
            }
        }
    };

    let mut n: u32 = 0;
    loop {
        let _ = writeln!(uart, "i2s0 alive, n={n}");
        n = n.wrapping_add(1);
        delay.delay_ms(1000);
    }
}
