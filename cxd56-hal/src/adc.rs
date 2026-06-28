//! Blob-free LPADC driver for the CXD5602.
//!
//! Reads 10-bit samples directly from the hardware ADCIF FIFO Read Ports
//! (UM §3.9.9.4.3, §3.21), bypassing the iSoP firmware and SCU sequencer.
//! Only the lower-tier hardware FIFOs (`SCU_ADCIF_FIFO L0..L3`) are used;
//! the iSoP is never released from reset.
//!
//! # Supported channels
//!
//! | Variant | CXD5602 signal | Arduino header |
//! |---|---|---|
//! | [`LpAdcChannel::Ch2`] | SEN_AIN4 | A2 |
//! | [`LpAdcChannel::Ch3`] | SEN_AIN5 | A3 |
//!
//! # Voltage range
//!
//! **LPADC full-scale ≈ 0.7 V (VDDA_LPADC).** Never connect A2/A3 to the
//! 1.8 V or 3.3 V rail — over-range damages the analog front-end.
//!
//! # Usage
//!
//! ```no_run
//! let mut adc = Adc::new(LpAdc, dp.scu_adcif, AdcConfig::default(), &clock)?;
//! let raw = adc.read(LpAdcChannel::Ch2); // 0..=1023
//! ```

use core::marker::PhantomData;

use fugit::Hertz;
use thiserror::Error;

use crate::clocks::pmu;
use crate::clocks::{Clock, ClockError, PeripheralId};
use crate::pac;

// ============================================================================
// Public types
// ============================================================================

/// LPADC input channels available on the Spresense Arduino header.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LpAdcChannel {
    /// SEN_AIN4 — Arduino A2
    Ch2 = 2,
    /// SEN_AIN5 — Arduino A3
    Ch3 = 3,
}

/// Configuration applied once at [`Adc::new`].
#[derive(Clone, Debug)]
pub struct AdcConfig {
    /// Decimation multiplier written to `SAMP_RATIO[11:8]` in the per-channel
    /// `LPADC_D*` registers. 0 = one sample per LPADC clock (fastest); each
    /// increment of 1 doubles the effective sample period.
    pub sample_ratio: u8,
}

impl Default for AdcConfig {
    fn default() -> Self {
        Self { sample_ratio: 0 }
    }
}

/// Errors returned by the ADC driver.
#[derive(Debug, Error)]
pub enum AdcError {
    #[error("clock gate error: {0}")]
    Clock(#[from] ClockError),
}

// ============================================================================
// Sealed peripheral trait
// ============================================================================

mod sealed {
    use fugit::Hertz;

    use crate::clocks::{Clock, PeripheralId};

    pub trait Sealed {
        const CLOCK_ID: PeripheralId;
        /// Sample the peripheral's base clock. [`Fixed`](crate::clocks::Fixed)
        /// for LPADC, so the borrow of `clock` ends at the call site.
        fn base_hz(clock: &Clock) -> Hertz<u32>;
    }
}

/// Maps an ADC kind-marker to its clock gate and base clock.
///
/// Sealed — only [`LpAdc`] is implemented today.
pub trait AdcPeriph: sealed::Sealed {
    /// Return type from [`Adc::new`], parameterised by the lifetime of the
    /// `&Clock` borrow. Fixed-clock implementations set this to
    /// `Adc<'static, Self>` to signal that no borrow is retained.
    type Output<'clk>;

    #[doc(hidden)]
    fn wrap<'clk>(adcif: pac::ScuAdcif, sample_hz: u32) -> Self::Output<'clk>;
}

// ============================================================================
// LpAdc kind-marker
// ============================================================================

/// Kind-marker for the Low-Power ADC block.
///
/// Pass as the first argument to [`Adc::new`]; `T` is then inferred without a
/// turbofish (mirroring the `i2c` token-inference pattern).
pub struct LpAdc;

impl sealed::Sealed for LpAdc {
    const CLOCK_ID: PeripheralId = PeripheralId::LpAdc;

    fn base_hz(clock: &Clock) -> Hertz<u32> {
        // lpadc is Fixed — never changes with Clock::request_perf.
        clock.lpadc.hz()
    }
}

impl AdcPeriph for LpAdc {
    /// LPADC clock is Fixed ⇒ no `&Clock` borrow is retained after `new`.
    type Output<'clk> = Adc<'static, LpAdc>;

    fn wrap<'clk>(adcif: pac::ScuAdcif, sample_hz: u32) -> Self::Output<'clk> {
        Adc {
            adcif,
            sample_hz,
            _kind: PhantomData,
            _life: PhantomData,
        }
    }
}

// ============================================================================
// Adc<'clk, T> driver struct
// ============================================================================

/// Generic ADCIF driver.
///
/// `T` selects the ADC block ([`LpAdc`] today). `'clk` propagates the
/// `&Clock` borrow lifetime for Dyn-clocked blocks; for Fixed-clock blocks
/// (LPADC) it is always `'static`.
pub struct Adc<'clk, T: AdcPeriph> {
    adcif: pac::ScuAdcif,
    /// LPADC effective sample rate in Hz (base_hz / (sample_ratio + 1)).
    /// Used to compute the timed-poll delay in [`read`](Self::read).
    sample_hz: u32,
    _kind: PhantomData<T>,
    _life: PhantomData<&'clk ()>,
}

impl<'clk, T: AdcPeriph> Adc<'clk, T> {
    /// Enable the ADCIF, configure LPADC channels 2 and 3, and return the
    /// driver.
    ///
    /// `clock` is borrowed only to read `T::base_hz`. For LPADC the source is
    /// [`Fixed`](crate::clocks::Fixed) and the borrow ends at this call;
    /// [`Clock::request_perf`] may be called freely after construction.
    pub fn new<'a>(
        _kind: T,
        adcif: pac::ScuAdcif,
        config: AdcConfig,
        clock: &'a Clock,
    ) -> Result<T::Output<'a>, AdcError> {
        let base_hz = T::base_hz(clock).to_Hz();
        T::CLOCK_ID.enable()?;
        lpadc_init(&adcif, &config);
        // sample_hz = base_hz / (sample_ratio + 1); cap at 1 to avoid divide-by-zero.
        let sample_hz = base_hz / (config.sample_ratio as u32 + 1);
        Ok(T::wrap(adcif, sample_hz.max(1)))
    }
}

impl<'clk> Adc<'clk, LpAdc> {
    /// Read one fresh 10-bit sample (0..=1023) from `ch`.
    ///
    /// Waits two sample periods before popping the FIFO Read Port (UM
    /// §3.21.14.1: "maximum one sampling period of delay until the first
    /// sample"). The LPADC runs in all-four mode so both Ch2 and Ch3 fill
    /// their FIFOs continuously at `sample_hz`.
    ///
    /// Return value is right-justified: multiply by `VDDA_LPADC / 1023` for
    /// voltage (full-scale ≈ 0.7 V).
    pub fn read(&mut self, ch: LpAdcChannel) -> u16 {
        let wait_us = 2_000_000 / self.sample_hz;
        pmu::delay_us(wait_us);
        self.adcif.lpadc_fifo(ch as usize).read().data().bits()
    }

    /// Fill `out` with consecutive samples from `ch`, paced one per sample
    /// period.
    pub fn read_into(&mut self, ch: LpAdcChannel, out: &mut [u16]) {
        for slot in out.iter_mut() {
            *slot = self.read(ch);
        }
    }
}

impl<T: AdcPeriph> Drop for Adc<'_, T> {
    fn drop(&mut self) {
        // Disable the analog front-end before gating the clock.
        self.adcif.lpadc_a0().write(|w| unsafe { w.bits(0) });
        T::CLOCK_ID.disable().ok();
    }
}

// ============================================================================
// ADCIF bring-up
// ============================================================================

/// Configure and enable the LPADC for channels 2 and 3.
///
/// Sequence mirrors the NuttX `cxd56_lpadc_start` / `cxd56_adc_setparams`
/// calls: reset pulse → per-channel SAMP_RATIO+FIFO_WATERMARK → FIFO_EN →
/// channel-select mode → ADC enable.
fn lpadc_init(adcif: &pac::ScuAdcif, config: &AdcConfig) {
    // Ensure ADCIF clock/power register is on (reset value = 1 = enabled).
    adcif.scu_adcif_ckpower().write(|w| unsafe { w.bits(1) });

    // SW_RESET pulse: assert then deassert.
    adcif.lpadc_d0().write(|w| unsafe { w.bits(1) }); // SW_RESET=1
    adcif.lpadc_d0().write(|w| unsafe { w.bits(0) }); // SW_RESET=0

    let ratio = config.sample_ratio as u32;

    // Per-channel config (D5 = ch2, D6 = ch3):
    //   SAMP_RATIO[11:8] = ratio, FIFO_WATERMARK[3:0] = 1.
    // FIFO_WATERMARK=1 declares "1 word minimum depth before the FIFO is
    // considered non-empty"; this field must be ≥1 for reads to make sense.
    adcif
        .lpadc_d5()
        .write(|w| unsafe { w.bits((ratio << 8) | 1) });
    adcif
        .lpadc_d6()
        .write(|w| unsafe { w.bits((ratio << 8) | 1) });

    // Enable hardware FIFOs for ch2 (bit 2) and ch3 (bit 3).
    adcif.lpadc_d2().write(|w| unsafe { w.bits(0b_1100) });

    // All-four channel mode (LV_CH_SEL_MODE = 5 = AllFour) so ch2 and ch3
    // fill their FIFOs independently and continuously.
    adcif.lpadc_a1().write(|w| unsafe { w.bits(5) });

    // Enable the analog front-end (LV_ADC_EN = 1).
    adcif.lpadc_a0().write(|w| unsafe { w.bits(1) });
}
