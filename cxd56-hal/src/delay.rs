use core::marker::PhantomData;

use cortex_m::delay::Delay as SystDelay;
use cortex_m::peripheral::SYST;
use embedded_hal::delay::DelayNs;

use crate::clocks::{Clock, ClockRef};

/// SysTick-backed delay implementing [`DelayNs`], calibrated against the APP
/// core (Cortex-M4) clock.
///
/// # Lifetime
///
/// SysTick counts the **core clock** — `cortex_m::delay::Delay` selects
/// `SystClkSource::Core`, and on the CXD5602 NuttX confirms this by setting
/// `NVIC_SYSTICK_CTRL_CLKSOURCE` to CCLK in `cxd56_timerisr.c`. That clock is
/// `appsmp`, a **[`Dyn`](crate::clocks::Dyn)** quantity that changes with the
/// operating point (HP ≈ 156 MHz / LP ≈ 39 MHz). A silent perf change
/// invalidates the SysTick calibration, making every delay ~4× too long or
/// too short.
///
/// `Delay<'clk>` borrows the [`Clock`] for `'clk`, so
/// [`Clock::request_perf`] (requires `&mut Clock`) cannot be called while a
/// `Delay` is alive — drop it (or call [`free`](Delay::free)), reconfigure the
/// operating point, then rebuild with the new rate.
pub struct Delay<'clk> {
    inner: SystDelay,
    _life: PhantomData<&'clk ()>,
}

impl<'clk> Delay<'clk> {
    /// Create a `Delay` calibrated to the current APP core frequency read from
    /// `clock`. Borrows `clock` for `'clk`, blocking [`Clock::request_perf`]
    /// until this `Delay` is dropped.
    ///
    /// ```ignore
    /// let mut clock = crg.into_clock();
    /// let delay = Delay::new(cp.SYST, &clock);
    /// // clock.request_perf(Perf::Lp)?;  // ← E0502 while `delay` is live
    /// drop(delay);
    /// clock.request_perf(Perf::Lp)?;     // ← now fine
    /// ```
    pub fn new(syst: SYST, clock: &'clk Clock) -> Self {
        Self {
            inner: SystDelay::new(syst, clock.appsmp().hz().to_Hz()),
            _life: PhantomData,
        }
    }

    /// Release the `SYST` peripheral, ending the `clock` borrow.
    ///
    /// Equivalent to `drop(self)` for the borrow-checker but returns the
    /// peripheral for reuse.
    pub fn free(self) -> SYST {
        self.inner.free()
    }
}

impl Delay<'static> {
    /// Create a `Delay` from an explicit core-clock frequency in Hz.
    ///
    /// Useful at very early boot before a [`Clock`] snapshot is available
    /// (e.g. using the known boot-ROM default of 156 MHz). Pins nothing — the
    /// caller owns the calibration's correctness across any later perf change.
    /// Parallels [`crate::delay::Delay::with_clock`].
    pub fn with_clock(syst: SYST, sysclk_hz: u32) -> Self {
        Self {
            inner: SystDelay::new(syst, sysclk_hz),
            _life: PhantomData,
        }
    }

    /// Create a `Delay` calibrated from a shared [`ClockRef`] (Option-2 path),
    /// always `'static`.
    ///
    /// Reads the current `appsmp` (APP core) frequency with an `Acquire` load
    /// and forwards to [`with_clock`](Self::with_clock). Holds no [`Clock`]
    /// borrow, so it does **not** block
    /// [`PerfControl::request_perf`](crate::clocks::PerfControl::request_perf).
    /// The calibration is fixed at construction: after a perf change the delay
    /// scaling is wrong — drop and rebuild with `from_ref` to recalibrate.
    pub fn from_ref(syst: SYST, clock: &'static ClockRef) -> Self {
        Self::with_clock(syst, clock.appsmp().to_Hz())
    }
}

impl DelayNs for Delay<'_> {
    #[inline]
    fn delay_ns(&mut self, ns: u32) {
        // SysTick resolution at 156 MHz is ~6.4 ns; round up to the next
        // microsecond so the delay is always at least `ns` nanoseconds.
        self.inner.delay_us(ns.div_ceil(1_000));
    }

    #[inline]
    fn delay_us(&mut self, us: u32) {
        self.inner.delay_us(us);
    }

    #[inline]
    fn delay_ms(&mut self, ms: u32) {
        self.inner.delay_ms(ms);
    }
}
