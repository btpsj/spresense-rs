//! On-hardware GPIO level test driven entirely through the **embassy** HAL.
//!
//! Mirrors `tests/gpio_levels/` (same `defmt-test` shape, same wiring) but
//! depends only on `embassy-cxd56` — no `cxd56-hal` / `cxd56-pac-svd2rust`.
//! Asserts that a GPIO input tied to 1.8 V reads High and one tied to GND reads
//! Low. `harness = false` because `defmt-test` only emits its entry under
//! `cfg(test)`.

#![no_std]
#![no_main]

use cortex_m_rt as _;
use defmt_serial as _;
use panic_probe as _;
use static_cell::StaticCell;

/// UART1 (COM) base clock in Hz — the PL011 baud-divisor reference.
///
/// The blocking UART driver re-initialises the console and recomputes the baud
/// from this value, so it **must** match the board's real COM rate or the whole
/// defmt stream is garbled and the harness can't decode it (timeout → exit 2).
/// This matches `examples/rust_hello_uart_embassy`'s `COM_HZ`; confirm it there
/// (clean greeting at 115200) before trusting this test's verdict.
const COM_HZ: u32 = 48_000_000;

/// Holds the logger UART for `'static` so `defmt_serial` can keep using it.
static SERIAL: StaticCell<
    embassy_cxd56::uart::Uart<'static, embassy_cxd56::peripherals::UART1>,
> = StaticCell::new();

#[defmt_test::tests]
mod tests {
    use defmt::assert;
    use embassy_cxd56::gpio::{Input, Pull};
    use embassy_cxd56::uart::{Config, Uart};

    struct State {
        high: Input<'static>,
        low: Input<'static>,
    }

    #[init]
    fn init() -> State {
        let p = embassy_cxd56::init();

        // Bring up UART1 (the on-board CP2102N console) with the blocking PL011
        // driver and install it as the defmt logger. `Config::default()` is
        // 115200 8N1; only `src_clk_hz` must be supplied.
        let uart = Uart::new(
            p.UART1,
            p.SPI0_CS_X,
            p.SPI0_SCK,
            Config {
                src_clk_hz: crate::COM_HZ,
                ..Default::default()
            },
        )
        .unwrap();
        defmt_serial::defmt_serial(crate::SERIAL.init(uart));

        // `Input::new` flips DIR *and* enables the input buffer (ENZI) while
        // floating the pad (Pull::None → PUN=PDN=1), so — unlike the cxd56-hal
        // version — no manual `IO_*` pad poke is needed for `IN` to follow the
        // pin.
        State {
            high: Input::new(p.P1q_01, Pull::None), // D21 / GP_EMMC_DATA3 → 1.8V
            low: Input::new(p.P1q_00, Pull::None),  // D20 / GP_EMMC_DATA2 → GND
        }
    }

    #[test]
    fn data3_high_reads_high(s: &mut State) {
        assert!(
            s.high.is_high(),
            "D21 (P1q_01) tied to 1.8V should read High"
        );
    }

    #[test]
    fn data2_low_reads_low(s: &mut State) {
        assert!(s.low.is_low(), "D20 (P1q_00) tied to GND should read Low");
    }
}
