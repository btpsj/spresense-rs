#![no_std]
#![forbid(unsafe_code)]
//! Registry-based clock-change propagation for the `cxd56-hal` shared-clock
//! (`from_ref`) path — the **RegistryPerf** approach.
//!
//! # What this is
//!
//! [`PerfControl::request_perf`] changes the CPU/bus operating point but leaves
//! every peripheral's baud/SCK divisor programmed for the *old* base clock. On
//! the borrow-checked `Clock` path the compiler forces you to drop peripherals
//! before changing perf; on the shared [`ClockRef`] path nothing does, so a byte
//! in flight across the transition is garbled and every later transfer runs at
//! the wrong rate until the divisor is rewritten.
//!
//! NuttX solves this with a power-manager callback registry: on a change it
//! calls every registered driver with `CLK_CHG_START` (before) to quiesce, then
//! `CLK_CHG_END` (after) to recompute the divisor. This crate is the Rust
//! analogue. [`RegistryPerf`] **owns** the [`PerfControl`] (making it the only
//! coherent way to change the operating point) plus a fixed set of
//! [`ClockSink`]s, and its [`request_perf`](RegistryPerf::request_perf) brackets
//! the change:
//!
//! 1. **`CLK_CHG_START`** — [`quiesce`](ClockSink::quiesce) every sink (drain TX
//!    / wait for the bus to go idle) *before* the clock moves. If any sink
//!    cannot drain, the change is aborted with the clock left untouched, so the
//!    frequency never moves with a transfer in flight.
//! 2. the operating-point change ([`PerfControl::request_perf`], which
//!    re-samples [`ClockRef`]).
//! 3. **`CLK_CHG_END`** — [`reconfigure`](ClockSink::reconfigure) every sink for
//!    the new rates.
//!
//! Because the sink set is fixed when the registry is built, you cannot forget
//! to bracket a peripheral at a call site (the gap the per-call move-through /
//! scoped strategies have): registration is once, coverage is total.
//!
//! # The cost
//!
//! The registry must reach each peripheral both for normal I/O (from the app)
//! and for reconfiguration (from `request_perf`) — two owners of the same
//! `&mut`. In Rust that means **interior mutability**: each sink stores its
//! peripheral in a [`critical_section::Mutex`]`<`[`RefCell`]`<…>>`,
//! costing one `RefCell` borrow + one critical section per access. That is the
//! price of this strategy; strategies that keep the peripheral exclusively owned
//! (drop+rebuild, or a generation-gated pull loop) avoid it — pick per
//! peripheral. This crate contains **no `unsafe`**: it is pure safe composition
//! of the HAL's [`flush`](cxd56_hal::uart::Uart::flush) /
//! [`reconfigure`](cxd56_hal::uart::Uart::reconfigure) primitives.
//!
//! # Concurrency
//!
//! [`RegistryPerf`] is `Send + !Sync` (inherited from [`PerfControl`], whose
//! `request_perf` is non-re-entrant), so a single owner — e.g. a power task —
//! drives operating-point changes while any context performs I/O through the
//! sinks. The per-access critical sections keep an I/O borrow and a reconfigure
//! borrow from overlapping. Each quiesce / reconfigure / `with` runs inside a
//! `critical_section` — on this chip a cross-core hardware semaphore — so it
//! briefly stalls the other core; the drains are bounded
//! ([`UartError`]`::Timeout` / [`SpiError`]`::Timeout`) so a wedged bus aborts the
//! change instead of hanging. One window stays the caller's responsibility:
//! between step 2 and step 3 the clock has already moved but a sink not yet
//! reconfigured is at a stale rate, so a *concurrent* transfer started from
//! another context in that window would use the wrong divisor. Trigger perf
//! changes from a point where the registered peripherals are not being driven
//! concurrently (as NuttX does from its PM thread).
//!
//! [`UartSink`], [`SpiSink`], and [`DelaySink`] are provided. A `Delay` is
//! recalibrated (it has no in-flight data to drain) rather than flushed on a
//! perf change. Note SysTick is **core-local**, so a registered `Delay` must be
//! installed and driven on the core it was built on — see [`DelaySink`].
//!
//! # Example
//!
//! ```ignore
//! use cxd56_hal::{pac, clocks::{ClockRef, Perf}, uart::{Uart, UartConfig}};
//! use cxd56_shared_clk::{RegistryPerf, UartSink, SpiSink};
//! use static_cell::StaticCell;
//!
//! static CLOCK: StaticCell<ClockRef> = StaticCell::new();
//! static UART1: UartSink<pac::Uart1> = UartSink::new();
//! static SPI5:  SpiSink<pac::Spi5>  = SpiSink::new();
//!
//! // ... obtain `crg`, `p` (PAC peripherals), pins ...
//! let clock: &'static ClockRef = CLOCK.init(ClockRef::from_crg(&crg));
//! let perf = crg.into_perf_control(clock);
//!
//! let ucfg = UartConfig::default();
//! let uart = Uart::from_ref(p.uart1, upins, ucfg.clone(), clock).expect("uart");
//! UART1.install(uart, ucfg);
//! // let spi = Spi::from_ref(p.spi5, spins, scfg.clone(), clock).expect("spi");
//! // SPI5.install(spi, scfg);
//!
//! let registry = RegistryPerf::new(perf, clock, [&UART1, &SPI5]);
//!
//! // Normal I/O from anywhere:
//! UART1.with(|u| u.write_byte(b'A'));
//!
//! // DVFS — every registered peripheral is quiesced + reconfigured around it:
//! registry.request_perf(Perf::Lp)?; // returns cxd56_shared_clk::Error
//! UART1.with(|u| u.write_byte(b'B')); // correct baud at the new operating point
//! # Ok::<(), cxd56_shared_clk::Error>(())
//! ```

use core::cell::RefCell;

use critical_section::Mutex;
use fugit::Hertz;

use cxd56_hal::clocks::{ClockRef, GearError, Perf, PerfControl, PeripheralId, PmError};
use cxd56_hal::delay::Delay;
use cxd56_hal::spi::{Spi, SpiConfig, SpiError, SpiPeriph};
use cxd56_hal::uart::{Uart, UartConfig, UartError, UartPeriph};

/// Error from a registered sink during a bracketed clock change.
#[derive(Debug)]
pub enum SinkError {
    /// A [`UartSink`] failed to quiesce or reconfigure.
    Uart(UartError),
    /// A [`SpiSink`] failed to quiesce or reconfigure.
    Spi(SpiError),
}

/// Error from [`RegistryPerf`].
#[derive(Debug)]
pub enum Error {
    /// The operating-point change itself failed. The clock is unchanged and no
    /// sink was reconfigured (the sinks were quiesced first, then left as-is).
    Perf(PmError),
    /// A gear change failed (see [`RegistryPerf::set_gear`] /
    /// [`set_spi_gear`](RegistryPerf::set_spi_gear)). Clock unchanged.
    Gear(GearError),
    /// A sink failed; `index` is its position in the registry's sink array.
    ///
    /// On a **quiesce** failure (before the change) the clock is left untouched.
    /// On a **reconfigure** failure (after the change) the remaining sinks are
    /// still reconfigured best-effort, so this reports the *first* such failure
    /// while the others move to the new rate.
    Sink { index: usize, error: SinkError },
}

impl From<UartError> for SinkError {
    fn from(e: UartError) -> Self {
        SinkError::Uart(e)
    }
}

impl From<SpiError> for SinkError {
    fn from(e: SpiError) -> Self {
        SinkError::Spi(e)
    }
}

impl core::fmt::Display for SinkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SinkError::Uart(e) => write!(f, "uart sink: {e}"),
            SinkError::Spi(e) => write!(f, "spi sink: {e}"),
        }
    }
}

impl core::error::Error for SinkError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            SinkError::Uart(e) => Some(e),
            SinkError::Spi(e) => Some(e),
        }
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            // PmError / GearError don't implement Display; fall back to Debug.
            Error::Perf(e) => write!(f, "operating-point change failed: {e:?}"),
            Error::Gear(e) => write!(f, "gear change failed: {e:?}"),
            Error::Sink { index, error } => write!(f, "sink #{index}: {error}"),
        }
    }
}

impl core::error::Error for Error {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            // PmError / GearError don't implement core::error::Error.
            Error::Sink { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// A peripheral the [`RegistryPerf`] can bracket across an operating-point
/// change.
///
/// Object-safe so heterogeneous peripherals share one registry. Both methods
/// take `&self`: the interior mutability lives inside the concrete sink
/// ([`UartSink`] / [`SpiSink`]), which is why a sink is `Send + Sync` and can be
/// referenced from a `'static` registry.
pub trait ClockSink: Send + Sync {
    /// Drain in-flight activity before the clock changes (the `CLK_CHG_START`
    /// half). Returning `Err` aborts the change with the clock untouched.
    fn quiesce(&self) -> Result<(), SinkError>;

    /// Reprogram divisors from the new rates in `clock` (the `CLK_CHG_END`
    /// half).
    fn reconfigure(&self, clock: &'static ClockRef) -> Result<(), SinkError>;
}

/// Stored UART plus the config needed to recompute its baud divisor, behind a
/// critical-section `RefCell`.
type UartSlot<U> = Mutex<RefCell<Option<(Uart<'static, U>, UartConfig)>>>;

/// Stored SPI plus the config needed to recompute its SCK divisor, behind a
/// critical-section `RefCell`.
type SpiSlot<S> = Mutex<RefCell<Option<(Spi<'static, S>, SpiConfig)>>>;

/// A registered UART. Holds the `'static` [`Uart`] and the [`UartConfig`] needed
/// to recompute its baud divisor, behind a critical-section `RefCell`.
pub struct UartSink<U: UartPeriph> {
    slot: UartSlot<U>,
}

impl<U: UartPeriph> UartSink<U> {
    /// Create an empty sink, suitable for a `static`. Install the peripheral at
    /// runtime with [`install`](Self::install).
    pub const fn new() -> Self {
        Self {
            slot: Mutex::new(RefCell::new(None)),
        }
    }

    /// Install (or replace) the UART and the config used to reconfigure it.
    pub fn install(&self, uart: Uart<'static, U>, config: UartConfig) {
        critical_section::with(|cs| {
            *self.slot.borrow(cs).borrow_mut() = Some((uart, config));
        });
    }

    /// Run `f` with mutable access to the UART, returning `None` if no UART has
    /// been installed.
    ///
    /// `f` runs inside a critical section (interrupts off, and on this chip the
    /// cross-core SPH held), so keep it short and **do not re-enter the same
    /// sink** from within `f` — that is a `RefCell` double-borrow panic. The
    /// return is `Option<R>`: if `f` returns a `Result` (e.g. I/O), the outer
    /// `Option` will not trip `must_use`, so handle the inner error.
    pub fn with<R>(&self, f: impl FnOnce(&mut Uart<'static, U>) -> R) -> Option<R> {
        critical_section::with(|cs| self.slot.borrow(cs).borrow_mut().as_mut().map(|(u, _)| f(u)))
    }
}

impl<U: UartPeriph> Default for UartSink<U> {
    fn default() -> Self {
        Self::new()
    }
}

impl<U: UartPeriph> ClockSink for UartSink<U> {
    fn quiesce(&self) -> Result<(), SinkError> {
        critical_section::with(|cs| {
            if let Some((u, _)) = self.slot.borrow(cs).borrow_mut().as_mut() {
                u.flush()?; // bounded drain; UartError::Timeout on a wedged bus
            }
            Ok(())
        })
    }

    fn reconfigure(&self, clock: &'static ClockRef) -> Result<(), SinkError> {
        critical_section::with(|cs| {
            match self.slot.borrow(cs).borrow_mut().as_mut() {
                Some((u, cfg)) => u.reconfigure(cfg, clock).map_err(SinkError::Uart),
                None => Ok(()),
            }
        })
    }
}

/// A registered SPI bus. Holds the `'static` [`Spi`] and the [`SpiConfig`] needed
/// to recompute its SCK divisor, behind a critical-section `RefCell`.
pub struct SpiSink<S: SpiPeriph + Send> {
    slot: SpiSlot<S>,
}

impl<S: SpiPeriph + Send> SpiSink<S> {
    /// Create an empty sink, suitable for a `static`. Install the peripheral at
    /// runtime with [`install`](Self::install).
    pub const fn new() -> Self {
        Self {
            slot: Mutex::new(RefCell::new(None)),
        }
    }

    /// Install (or replace) the SPI bus and the config used to reconfigure it.
    pub fn install(&self, spi: Spi<'static, S>, config: SpiConfig) {
        critical_section::with(|cs| {
            *self.slot.borrow(cs).borrow_mut() = Some((spi, config));
        });
    }

    /// Run `f` with mutable access to the SPI bus, returning `None` if none has
    /// been installed. Same critical-section / no-re-entry / `Option<R>` caveats
    /// as [`UartSink::with`].
    pub fn with<R>(&self, f: impl FnOnce(&mut Spi<'static, S>) -> R) -> Option<R> {
        critical_section::with(|cs| self.slot.borrow(cs).borrow_mut().as_mut().map(|(s, _)| f(s)))
    }
}

impl<S: SpiPeriph + Send> Default for SpiSink<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: SpiPeriph + Send> ClockSink for SpiSink<S> {
    fn quiesce(&self) -> Result<(), SinkError> {
        critical_section::with(|cs| match self.slot.borrow(cs).borrow_mut().as_mut() {
            Some((s, _)) => s.flush().map_err(SinkError::Spi),
            None => Ok(()),
        })
    }

    fn reconfigure(&self, clock: &'static ClockRef) -> Result<(), SinkError> {
        critical_section::with(|cs| match self.slot.borrow(cs).borrow_mut().as_mut() {
            Some((s, cfg)) => s.reconfigure(cfg, clock).map_err(SinkError::Spi),
            None => Ok(()),
        })
    }
}

/// Stored `Delay`, behind a critical-section `RefCell`. No config: a `Delay` is
/// recalibrated purely from the new core-clock rate.
type DelaySlot = Mutex<RefCell<Option<Delay<'static>>>>;

/// A registered SysTick [`Delay`].
///
/// **SysTick is core-local:** a `Delay` is calibrated against the SysTick of the
/// core it was built on, so install it, drive it via [`with`](Self::with), and
/// trigger the perf changes that reconfigure it all on that same core. For code
/// that owns a `Delay` directly it is often simpler to skip the registry and
/// refresh it inline with `delay = Delay::from_ref(delay.free(), clock)` after a
/// perf change — a `Delay` has no in-flight state to garble, only stale
/// calibration.
pub struct DelaySink {
    slot: DelaySlot,
}

impl DelaySink {
    /// Create an empty sink, suitable for a `static`. Install at runtime with
    /// [`install`](Self::install).
    pub const fn new() -> Self {
        Self {
            slot: Mutex::new(RefCell::new(None)),
        }
    }

    /// Install (or replace) the delay.
    pub fn install(&self, delay: Delay<'static>) {
        critical_section::with(|cs| {
            *self.slot.borrow(cs).borrow_mut() = Some(delay);
        });
    }

    /// Run `f` with mutable access to the delay, returning `None` if none has
    /// been installed. Same critical-section / no-re-entry caveats as
    /// [`UartSink::with`]; note a blocking delay holds interrupts off (and the
    /// cross-core SPH) for its full duration.
    pub fn with<R>(&self, f: impl FnOnce(&mut Delay<'static>) -> R) -> Option<R> {
        critical_section::with(|cs| self.slot.borrow(cs).borrow_mut().as_mut().map(f))
    }
}

impl Default for DelaySink {
    fn default() -> Self {
        Self::new()
    }
}

impl ClockSink for DelaySink {
    fn quiesce(&self) -> Result<(), SinkError> {
        Ok(()) // a Delay has no in-flight activity to drain
    }

    fn reconfigure(&self, clock: &'static ClockRef) -> Result<(), SinkError> {
        critical_section::with(|cs| {
            let mut slot = self.slot.borrow(cs).borrow_mut();
            if let Some(d) = slot.take() {
                // SysTick has no frequency setter: release SYST and rebuild.
                *slot = Some(Delay::from_ref(d.free(), clock));
            }
        });
        Ok(())
    }
}

/// Owns a [`PerfControl`] and a fixed set of [`ClockSink`]s, bracketing every
/// operating-point / gear change so registered peripherals are quiesced before
/// the clock moves and reconfigured after. See the [crate] docs.
///
/// `N` is the number of sinks, inferred from the array passed to [`new`](Self::new).
pub struct RegistryPerf<const N: usize> {
    inner: PerfControl,
    clock: &'static ClockRef,
    sinks: [&'static dyn ClockSink; N],
}

impl<const N: usize> RegistryPerf<N> {
    /// Take ownership of `perf` (so this becomes the sole coherent perf
    /// authority) and the `sinks` to bracket. `clock` is the same
    /// `&'static ClockRef` the sinks' peripherals and `perf` were built from.
    pub fn new(
        perf: PerfControl,
        clock: &'static ClockRef,
        sinks: [&'static dyn ClockSink; N],
    ) -> Self {
        Self {
            inner: perf,
            clock,
            sinks,
        }
    }

    /// Quiesce all sinks, run `op` (the actual clock change), reconfigure all
    /// sinks. If a sink cannot quiesce, `op` is not run and the clock is left
    /// untouched. After the change every sink is reconfigured even if one fails,
    /// so none is stranded at the stale rate; the first failure is returned.
    fn bracket(&self, op: impl FnOnce(&PerfControl) -> Result<(), Error>) -> Result<(), Error> {
        // CLK_CHG_START: drain every sink before the clock moves; abort (clock
        // untouched) if any can't quiesce.
        for (index, &s) in self.sinks.iter().enumerate() {
            s.quiesce().map_err(|error| Error::Sink { index, error })?;
        }
        // The actual operating-point / gear change (re-samples ClockRef).
        op(&self.inner)?;
        // CLK_CHG_END: reconfigure EVERY sink even if one fails (so none is left
        // stranded at the stale rate). `and` keeps the first error.
        let mut result = Ok(());
        for (index, &s) in self.sinks.iter().enumerate() {
            result = result.and(
                s.reconfigure(self.clock)
                    .map_err(|error| Error::Sink { index, error }),
            );
        }
        result
    }

    /// Change the CPU/bus operating point, bracketing all sinks. Replaces a bare
    /// [`PerfControl::request_perf`] so no registered peripheral garbles or is
    /// left at a stale rate.
    pub fn request_perf(&self, perf: Perf) -> Result<(), Error> {
        self.bracket(|inner| inner.request_perf(perf).map_err(Error::Perf))
    }

    /// Set an APP-local gear divider, bracketing all sinks (a gear change shifts
    /// `img_*` base clocks, so registered peripherals are reconfigured too).
    pub fn set_gear(&self, id: PeripheralId, divisor: u32) -> Result<(), Error> {
        self.bracket(|inner| inner.set_gear(id, divisor).map_err(Error::Gear))
    }

    /// Set an SPI port's gear to target `maxfreq`, bracketing all sinks.
    pub fn set_spi_gear(&self, port: PeripheralId, maxfreq: Hertz<u32>) -> Result<(), Error> {
        self.bracket(|inner| inner.set_spi_gear(port, maxfreq).map_err(Error::Gear))
    }
}
