//! On-hardware I2S0 loopback test (defmt-test framework).
//!
//! This is a `harness = false` integration test: build and run it with
//! `cargo test --release --test i2s`. `defmt-test` only emits its entry point
//! under `cfg(test)`, which is why it lives here rather than in `src/`.
//!
//! Two test cases:
//!
//!   [1/2] **clock_sanity** (no wiring): brings up the CXD5247 audio companion
//!         and I2S0 master, then reads back BCK/LRCK from the hardware registers
//!         and asserts they match the expected 48 kHz / 3.072 MHz values.
//!
//!   [2/2] **external_loopback** (feature-gated): transmits a 3 kHz sine tone
//!         on I2S0_DATA_OUT while simultaneously capturing on I2S0_DATA_IN, then
//!         confirms the tone's periodicity via autocorrelation. Build with
//!         `--features external-loopback` and wire the two adjacent pads.
//!
//! # Wiring (external_loopback only)
//!
//! ```text
//! JP2 pin 7  (DATA_OUT / I2S0_SDOUT1 / D18)  ─── jumper ───  JP2 pin 6  (DATA_IN / I2S0_SDIN1 / D19)
//! ```
//! ⚠️  Both pads are 1.8 V — never connect to 3.3 V or 5 V signals.
//!
//! # Why a tone, not a bit-exact pattern
//!
//! The I2S0 RX DMA is sourced from `SRC1` (the audio block's sample-rate
//! converter), which filters DC and rings on step edges. A passband sine is what
//! this datapath is designed to carry, so the check is energy + periodicity via
//! autocorrelation rather than a bit match — shift-, gain-, and clip-invariant,
//! proving the tone made the round trip despite SRC filtering.

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

// These constants and helpers are only referenced by the feature-gated
// `external_loopback` test, so gate them to suppress dead-code warnings.
#[cfg(feature = "external-loopback")]
const SINE_PERIOD: usize = 16;
#[cfg(feature = "external-loopback")]
const TX_LEN: usize = 256;
#[cfg(feature = "external-loopback")]
const RX_LEN: usize = 1024;

#[cfg(feature = "external-loopback")]
const SINE: [i16; SINE_PERIOD] = [
    0, 3135, 5793, 7568, 8192, 7568, 5793, 3135, 0, -3135, -5793, -7568, -8192, -7568, -5793,
    -3135,
];

#[cfg(feature = "external-loopback")]
const MIN_MEAN_SQ: f32 = 1.0e4;
#[cfg(feature = "external-loopback")]
const MIN_AC_PERIOD: f32 = 0.5;
#[cfg(feature = "external-loopback")]
const MAX_AC_HALF: f32 = -0.2;

#[cfg(feature = "external-loopback")]
fn sine_word(i: usize) -> u32 {
    let s = SINE[i % SINE_PERIOD] as u16 as u32;
    (s << 16) | s
}

#[cfg(feature = "external-loopback")]
/// Analyse the RX capture for the transmitted tone.
///
/// Returns `(mean_square, ac_period, ac_half)`:
/// - `mean_square` — AC energy per sample; large means a signal is present.
/// - `ac_period` — normalised autocorrelation at the tone period (~+1 for the tone).
/// - `ac_half` — autocorrelation at the half period (~-1 for a sinusoid).
fn analyze(rx: &[u32]) -> (f32, f32, f32) {
    let n = rx.len();
    let l = |i: usize| (rx[i] & 0xffff) as i16 as f32;
    let mean = (0..n).map(|i| l(i)).sum::<f32>() / n as f32;
    let v = |i: usize| l(i) - mean;
    let energy: f32 = (0..n).map(|i| v(i) * v(i)).sum();
    if energy <= 0.0 {
        return (0.0, 0.0, 0.0);
    }
    let p = SINE_PERIOD;
    let ac_p: f32 = (0..n - p).map(|i| v(i) * v(i + p)).sum();
    let ac_h: f32 = (0..n - p / 2).map(|i| v(i) * v(i + p / 2)).sum();
    (energy / n as f32, ac_p / energy, ac_h / energy)
}

#[defmt_test::tests]
mod tests {
    use embedded_hal::delay::DelayNs;
    use fugit::ExtU32;

    use cxd56_hal::clocks::{Config, RccExt};
    use cxd56_hal::delay::Delay;
    use cxd56_hal::gpio::pins::Parts;
    use cxd56_hal::i2s::{I2s, I2s0, I2s0Pins, I2sConfig};
    use cxd56_hal::pac;
    use cxd56_hal::uart::{Uart, Uart1Pins};
    use cxd56_hal::watchdog::Watchdog;

    pub struct State {
        pub i2s: I2s<I2s0>,
    }

    #[init]
    fn init() -> State {
        let pac = pac::Peripherals::take().unwrap();
        let core = cortex_m::Peripherals::take().unwrap();

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

        let mut delay = Delay::new(core.SYST, clock);

        // Arm a watchdog: the audio AHB stalls silently (no fault) if audio MCLK
        // is not running when its registers are accessed (User Manual §3.15.6.16).
        // The watchdog converts a silent hang into a visible reboot loop.
        let mut wdt = Watchdog::new(pac.wdog, 8000u32.millis(), clock)
            .unwrap_or_else(|_| defmt::panic!("watchdog init failed"));
        wdt.start();

        // Power on the CXD5247 companion (AVDD/DVDD rails + XRST) before probing it.
        cxd56_hal::audio_aca::cxd5247_power_on()
            .unwrap_or_else(|_| defmt::panic!("CXD5247 power-on failed"));
        delay.delay_ms(20);
        wdt.feed();

        // Verify the SYSIOP loader exposes the ACA module (the 24.576 MHz MCLK source).
        cxd56_hal::audio_aca::check_id()
            .unwrap_or_else(|_| defmt::panic!("ACA check_id failed — CXD5247 not present"));
        wdt.feed();

        // Start the 24.576 MHz oscillator. After this the audio block has a running
        // clock and its registers no longer stall the AHB.
        cxd56_hal::audio_aca::power_on_common()
            .unwrap_or_else(|_| defmt::panic!("ACA power_on_common failed"));
        delay.delay_ms(10);
        wdt.feed();

        // Route the MCLK pad to the audio block (board_audio_initialize's PINCONFS_MCLK).
        cxd56_hal::audio_aca::mclk_pin_config();

        let i2s0_pins = I2s0Pins {
            bck: parts.gp_i2s0_bck,
            lrck: parts.gp_i2s0_lrck,
            data_in: parts.gp_i2s0_data_in,
            data_out: parts.gp_i2s0_data_out,
        };
        let i2s = I2s::<I2s0>::new(pac.audio, i2s0_pins, clock, I2sConfig::default())
            .unwrap_or_else(|_| defmt::panic!("I2S0 init failed"));
        wdt.feed();

        // Bring-up succeeded — disarm the watchdog so it doesn't fire during tests.
        wdt.stop();

        State { i2s }
    }

    /// [1/2] Verify BCK/LRCK register readback after audio bring-up. No wiring required.
    #[test]
    fn clock_sanity(state: &mut State) {
        let fc = state.i2s.frame_clocks();
        defmt::assert!(fc.is_master, "I2S0 not in master mode");
        defmt::assert_eq!(fc.lrck_hz, 48_000, "LRCK {} Hz ≠ 48000", fc.lrck_hz);
        // BCK = MCLK (24.576 MHz) / 8
        defmt::assert_eq!(fc.bck_hz, 3_072_000, "BCK {} Hz ≠ 3072000", fc.bck_hz);
    }

    /// [2/2] Full-duplex external loopback — sine tone out DATA_OUT, back in via DATA_IN.
    /// Build with `--features external-loopback` and wire JP2-7 (D18) → JP2-6 (D19).
    #[cfg(feature = "external-loopback")]
    #[test]
    fn external_loopback(state: &mut State) {
        let mut tx = [0u32; crate::TX_LEN];
        for (i, w) in tx.iter_mut().enumerate() {
            *w = crate::sine_word(i);
        }

        let mut rx = [0u32; crate::RX_LEN];
        state
            .i2s
            .transfer_16_blocking(&tx, &mut rx)
            .unwrap_or_else(|_| defmt::panic!("I2S DMA transfer failed"));

        let (mean_sq, ac_period, ac_half) = crate::analyze(&rx);
        defmt::assert!(
            mean_sq >= crate::MIN_MEAN_SQ,
            "no signal: mean_sq {=f32} < {=f32}",
            mean_sq,
            crate::MIN_MEAN_SQ
        );
        defmt::assert!(
            ac_period >= crate::MIN_AC_PERIOD,
            "not periodic: ac@period {=f32} < {=f32}",
            ac_period,
            crate::MIN_AC_PERIOD
        );
        defmt::assert!(
            ac_half <= crate::MAX_AC_HALF,
            "not sinusoidal: ac@half {=f32} > {=f32}",
            ac_half,
            crate::MAX_AC_HALF
        );
    }
}
