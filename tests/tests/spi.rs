//! On-hardware SPI5 loopback test (defmt-test framework).
//!
//! This is a `harness = false` integration test: build and run it with
//! `cargo test --release --test spi`. `defmt-test` only emits its entry point
//! under `cfg(test)`, which is why it lives here rather than in `src/`.
//!
//! Two test cases:
//!
//!   [1/2] **internal_lbm** (no wiring): `SSPCR1.LBM = 1` connects TX to RX
//!         inside the PL022 SSP — the EMMC pads are never driven.
//!
//!   [2/2] **external_loopback** (feature-gated): `SSPCR1.LBM = 0`; bytes
//!         travel out MOSI (JP2-9 / D16) and back in via MISO (JP2-8 / D17)
//!         over a jumper wire. Build with `--features external-loopback`.
//!
//! # Wiring (external_loopback only)
//!
//! ```text
//! JP2 pin 9  (MOSI / EMMC_DATA0 / D16)  ─── jumper ───  JP2 pin 8  (MISO / EMMC_DATA1 / D17)
//! ```
//! ⚠️  Both pads are 1.8 V — never connect to 3.3 V or 5 V signals.
//! ⚠️  Do not use with an eMMC/SD add-on attached.

#![no_std]
#![no_main]

use cortex_m_rt as _;
use defmt_serial as _;
use panic_probe as _;
use static_cell::StaticCell;

use cxd56_hal::clocks::{Clock, Hp};
use cxd56_hal::{pac, uart::Uart};

static SERIAL: StaticCell<Uart<'static, pac::Uart1>> = StaticCell::new();
static CLOCK: StaticCell<Clock<Hp>> = StaticCell::new();

/// Pattern that exercises all-zeros, all-ones, alternating bits, and ASCII.
const PATTERN: [u8; 8] = [0xA5, 0x5A, 0x00, 0xFF, b'R', b'U', b'S', b'T'];

#[defmt_test::tests]
mod tests {
    use defmt::assert_eq;
    use embedded_hal::spi::SpiBus;

    use cxd56_hal::clocks::{Config, RccExt};
    use cxd56_hal::gpio::pins::Parts;
    use cxd56_hal::pac;
    use cxd56_hal::spi::{Spi, Spi5Pins, SpiConfig};
    use cxd56_hal::uart::{Uart, Uart1Pins};

    struct State {
        clock: &'static cxd56_hal::clocks::Clock,
    }

    #[init]
    fn init() -> State {
        let pac = pac::Peripherals::take().unwrap();
        let crg = pac.crg.constrain(Config::default());
        let clock = crate::CLOCK.init(crg.into_hp_clock().expect("lock Hp"));

        // UART1 for defmt console output. COM clock is Fixed → Uart<'static, Uart1>.
        let parts = Parts::new(pac.topreg);
        let uart1_pins = Uart1Pins {
            tx: parts.gp_spi0_cs_x,
            rx: parts.gp_spi0_sck,
        };
        let uart = Uart::new(pac.uart1, uart1_pins, Default::default(), clock)
            .expect("uart1 init failed");
        defmt_serial::defmt_serial(crate::SERIAL.init(uart));

        State { clock }
    }

    /// [1/2] Internal LBM — SSPCR1.LBM = 1, no external wiring required.
    #[test]
    fn internal_lbm(state: &mut State) {
        let stolen = unsafe { pac::Peripherals::steal() };
        let parts = Parts::new(stolen.topreg);
        let pins = Spi5Pins { sck: parts.gp_emmc_clk, csn: parts.gp_emmc_cmd, mosi: parts.gp_emmc_data0, miso: parts.gp_emmc_data1 };
        let mut spi =
            Spi::new(stolen.spi5, pins, SpiConfig { loopback: true, ..SpiConfig::default() }, state.clock)
                .expect("Spi::new failed");

        let tx = crate::PATTERN;
        let mut rx = crate::PATTERN;
        spi.transfer_in_place(&mut rx).expect("transfer failed");

        for i in 0..tx.len() {
            assert_eq!(rx[i], tx[i], "byte {} mismatch", i);
        }
    }

    /// [2/2] External pad loopback — SSPCR1.LBM = 0, MOSI ↔ MISO jumper required.
    /// Build with `--features external-loopback` and wire JP2-9 ↔ JP2-8.
    #[cfg(feature = "external-loopback")]
    #[test]
    fn external_loopback(state: &mut State) {
        let stolen = unsafe { pac::Peripherals::steal() };
        let parts = Parts::new(stolen.topreg);
        let pins = Spi5Pins { sck: parts.gp_emmc_clk, csn: parts.gp_emmc_cmd, mosi: parts.gp_emmc_data0, miso: parts.gp_emmc_data1 };
        let mut spi =
            Spi::new(stolen.spi5, pins, SpiConfig { loopback: false, ..SpiConfig::default() }, state.clock)
                .expect("Spi::new failed");

        let tx = crate::PATTERN;
        let mut rx = crate::PATTERN;
        spi.transfer_in_place(&mut rx).expect("transfer failed");

        for i in 0..tx.len() {
            assert_eq!(rx[i], tx[i], "byte {} mismatch", i);
        }
    }
}
