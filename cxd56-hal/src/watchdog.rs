//! Watchdog timer (Arm PrimeCell SP805).
//!
//! The CXD5602 watchdog is an SP805 at `0xe004_4000`. This module wraps the
//! trustedfirmware [`arm_sp805`] driver over the register block, owning the
//! [`pac::Wdog`] token for exclusive hardware access.
//!
//! # Clocking and timeout
//!
//! The SP805 counts down at the CPU/AHB base clock
//! ([`Clock::cpu_baseclk`](crate::clocks::Clock::cpu_baseclk)), which is
//! **perf-dependent** (HP ≈ 156 MHz / LP ≈ 39 MHz). The counter underflows
//! twice before resetting the chip — the first underflow asserts the (here
//! unhandled) early-warning interrupt and reloads, the second drives reset — so
//! the reload is `freq × timeout_ms / 2000` (mirrors `cxd56_settimeout` in NuttX
//! `cxd56_wdt.c`). The maximum timeout is 40 s.
//!
//! [`feed`](Watchdog::feed) reloads the counter **and** clears the early-warning
//! interrupt (via the SP805 enable sequence — the same write set NuttX's
//! `keepalive` uses), so feeding at any point within `timeout` reliably prevents
//! the reset; the chip resets only `timeout` after the last successful feed.
//!
//! Because the timeout depends on the operating point, [`Watchdog`] borrows the
//! [`Clock`] for `'clk`: while a watchdog is alive, an operating-point change
//! ([`Clock::into_hp`](crate::clocks::Clock::into_hp) /
//! [`Clock::into_lp`](crate::clocks::Clock::into_lp), which need ownership of
//! the `Clock`) cannot happen, guaranteeing the configured timeout stays
//! valid. To change the operating point, [`free`](Watchdog::free) the watchdog
//! first, change perf, then create a new one.
//!
//! # Example
//! ```ignore
//! use cxd56_hal::watchdog::Watchdog;
//! use fugit::ExtU32; // for `.millis()`
//!
//! let mut wdt = Watchdog::new(p.wdog, 2000u32.millis(), &clock)?;
//! wdt.start();
//! loop {
//!     do_work();
//!     wdt.feed(); // pet the dog before it bites
//! }
//! ```

use arm_sp805 as sp805;
use core::marker::PhantomData;
use core::ptr::NonNull;
use fugit::MillisDurationU32;
use thiserror::Error;

use crate::clocks::{Clock, PerfState};
use crate::pac;

/// Maximum supported timeout, mirroring NuttX `WDT_MAX_TIMEOUT` (40 s).
const WDT_MAX_TIMEOUT_MS: u32 = 40_000;

/// Errors from [`Watchdog::new`].
#[derive(Debug, Error)]
pub enum WatchdogError {
    /// Requested timeout is zero or exceeds the maximum (ms).
    #[error("timeout must be 1..={0} ms")]
    InvalidTimeout(u32),
    /// The computed reload value does not fit in the SP805's 32-bit counter.
    #[error("reload value overflows 32 bits")]
    ReloadOverflow,
    /// The CPU base clock reads as zero (clock not configured / gated).
    #[error("CPU base clock is zero")]
    ClockUnavailable,
}

/// Convert a millisecond timeout at `freq_hz` into an SP805 reload value.
///
/// Mirrors `cxd56_settimeout` (`cxd56_wdt.c:447-461`): round to the nearest
/// tick, then halve for the two-lap (interrupt-then-reset) behaviour.
fn timeout_to_load(freq_hz: u32, ms: u32) -> Result<u32, WatchdogError> {
    if ms == 0 || ms > WDT_MAX_TIMEOUT_MS {
        return Err(WatchdogError::InvalidTimeout(WDT_MAX_TIMEOUT_MS));
    }
    if freq_hz == 0 {
        return Err(WatchdogError::ClockUnavailable);
    }
    let reload = (freq_hz as u64 * ms as u64 + 500) / 1000 / 2;
    if reload >> 32 != 0 {
        return Err(WatchdogError::ReloadOverflow);
    }
    Ok(reload as u32)
}

/// SP805 watchdog driver, backed by [`arm_sp805::Watchdog`].
///
/// Consumes the [`pac::Wdog`] token for exclusive ownership and borrows the
/// [`Clock`] for `'clk` (see the [module docs](self) for the perf-lock
/// rationale). Constructed disabled; call [`start`](Self::start) to arm.
pub struct Watchdog<'clk> {
    /// The MMIO base is valid for the whole program, hence `'static`.
    inner: sp805::Watchdog<'static>,
    /// Exclusive hardware ownership.
    _wdog: pac::Wdog,
    /// Ties this watchdog to the `Clock` borrow, blocking the
    /// `into_hp`/`into_lp` transitions.
    _clk: PhantomData<&'clk ()>,
}

// Exclusive peripheral ownership — the `pac::Wdog` token was consumed at
// construction (matching the PAC `Periph` Send impl) and nothing else aliases
// the SP805 register block.
unsafe impl Send for Watchdog<'_> {}

impl<'clk> Watchdog<'clk> {
    /// Configure the watchdog with `timeout` and return it **disabled**.
    ///
    /// Consumes the [`pac::Wdog`] token and borrows `clock` for `'clk`. The
    /// reload value is computed from [`Clock::cpu_baseclk`] at this point and
    /// fixed for the watchdog's lifetime (the borrow prevents an operating-point
    /// change that would invalidate it). Call [`start`](Self::start) to arm.
    pub fn new<P: PerfState>(
        wdog: pac::Wdog,
        timeout: MillisDurationU32,
        clock: &'clk Clock<P>,
    ) -> Result<Self, WatchdogError> {
        let freq = clock.cpu_baseclk().to_Hz();
        let load = timeout_to_load(freq, timeout.to_millis())?;

        // SAFETY: `pac::Wdog::PTR` is the fixed, properly-aligned base address
        // of the SP805 register block; it is valid for the program's lifetime.
        // We consumed the `wdog` token above, so there is no other alias to this
        // peripheral. `arm_sp805::SP805Registers` describes the same 0x1000-byte
        // SP805 MMIO window as the PAC `wdog` block
        let ptr = NonNull::new(pac::Wdog::PTR as *mut sp805::SP805Registers)
            .expect("WDOG base address is null");
        let mut inner = sp805::Watchdog::new(unsafe { sp805::UniqueMmioPointer::new(ptr) }, load);
        // Force a known-disabled state: a prior boot stage may have left the
        // watchdog running, which would reset the board before `start()`.
        inner.disable();

        Ok(Self {
            inner,
            _wdog: wdog,
            _clk: PhantomData,
        })
    }

    /// Arm the watchdog: load the counter and enable interrupt + reset outputs.
    /// After this, the chip resets unless [`feed`](Self::feed) is called within
    /// the configured timeout.
    #[inline]
    pub fn start(&mut self) {
        self.inner.enable();
    }

    /// Reload the counter ("pet the dog") to postpone the reset.
    ///
    /// Delegates to the SP805 enable sequence, which reloads the counter *and*
    /// clears the early-warning interrupt — equivalent to NuttX's `keepalive`.
    /// This is why feeding any time within `timeout` (not merely within the
    /// first underflow lap) reliably prevents a reset. `arm_sp805::update`
    /// reloads only, so it is intentionally not used here.
    #[inline]
    pub fn feed(&mut self) {
        self.inner.enable();
    }

    /// Disable the watchdog (stops the counter; no reset will occur).
    #[inline]
    pub fn stop(&mut self) {
        self.inner.disable();
    }

    /// Disable the watchdog and return the [`pac::Wdog`] token for reuse.
    ///
    /// Calling `free` consumes `self` and prevents [`Drop`] from running a
    /// second `disable()`.
    pub fn free(mut self) -> pac::Wdog {
        // Stop before dismantling; ManuallyDrop then prevents Drop's disable().
        self.inner.disable();
        let mut md = core::mem::ManuallyDrop::new(self);
        // SAFETY: `_wdog` is read exactly once and `md` is never used again, so
        // there is no double-move or use-after-read. `inner` is dropped in place
        // to run its destructor; ManuallyDrop ensures the struct-level Drop
        // (which would call `disable()` again) does not run.
        let wdog = unsafe { core::ptr::read(&md._wdog) };
        unsafe { core::ptr::drop_in_place(&mut md.inner) };
        wdog
    }
}

impl Drop for Watchdog<'_> {
    fn drop(&mut self) {
        self.inner.disable();
    }
}
