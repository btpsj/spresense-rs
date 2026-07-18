//! Option-2 shared-clock path: [`ClockRef`] + [`PerfControl`].
//!
//! Use this when peripherals must be `'static` (e.g. embassy tasks) and the
//! operating point may change at runtime. The trade-off vs the borrow-checked
//! [`Clock`](super::profile::Clock) path: the borrow checker no longer prevents
//! [`PerfControl::request_perf`] while peripherals are alive — the caller is
//! responsible for quiescing in-flight operations first.
//!
//! # Entry point
//!
//! ```ignore
//! use static_cell::StaticCell;
//! use cxd56_hal::clocks::{ClockRef, Config};
//!
//! static CLOCK: StaticCell<ClockRef> = StaticCell::new();
//!
//! let crg = dp.crg.constrain(Config::default());
//! let clock_ref: &'static ClockRef = CLOCK.init(ClockRef::from_crg(&crg));
//! let perf_ctl = crg.into_perf_control(clock_ref);
//!
//! // All peripherals constructed with from_ref are 'static.
//! let uart = Uart::from_ref(pac.uart1, pins, UartConfig::default(), clock_ref)?;
//! ```

use core::cell::Cell;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicU32, Ordering};
use fugit::Hertz;

use super::peripheral::{GearError, PeripheralId};
use super::pm::{self, Perf, PmError};
use super::{Clocks, Config};

/// Shared clock reference for the Option-2 (`'static` peripheral) path.
///
/// Truly fixed clocks (`xosc`, `rcosc`, `rtc`, `scu`, `hpadc`, `lpadc`) are
/// plain `pub u32` fields — they never change. All perf-dependent clocks are
/// private `AtomicU32` fields accessed through accessor methods that do an
/// `Acquire` load. [`PerfControl::request_perf`] updates the atomics with
/// `Release` stores after each operating-point change.
///
/// `ClockRef` is `Send + Sync` — `u32` and `AtomicU32` are both `Send + Sync`.
///
/// Obtain a `&'static ClockRef` via [`ClockRef::from_crg`] + `StaticCell::init`
/// (or any other `'static` placement), then pass it to [`Crg::into_perf_control`]
/// and to peripheral `from_ref` constructors.
pub struct ClockRef {
    // Truly fixed — never change regardless of operating point.
    pub xosc:  u32,
    pub rcosc: u32,
    pub rtc:   u32,
    pub scu:   u32,
    pub hpadc: u32,
    pub lpadc: u32,
    // Perf-dependent — `Release` stores by PerfControl, `Acquire` loads by accessors.
    com:       AtomicU32,
    appsmp:    AtomicU32,
    syspll:    AtomicU32,
    sys:       AtomicU32,
    sys_ahb:   AtomicU32,
    sys_apb:   AtomicU32,
    sys_sfc:   AtomicU32,
    pmui2c:    AtomicU32,
    gps_cpu:   AtomicU32,
    gps_ahb:   AtomicU32,
    usb:       AtomicU32,
    sdio:      AtomicU32,
    img_uart:  AtomicU32,
    img_spi:   AtomicU32,
    img_wspi:  AtomicU32,
    img_vsync: AtomicU32,
    // Monotonic counter bumped once per `resample` (i.e. per applied perf/gear
    // change). The `Release` store in `resample` publishes the rate stores above
    // it; an `Acquire` load in `generation` that observes a new value is
    // guaranteed to also see the new rates. Lets `from_ref` peripherals detect a
    // stale divisor and call `reconfigure` (the pull model) without this struct
    // storing any per-peripheral state. (`gen` is a reserved keyword in edition
    // 2024, hence `gen_counter`.)
    gen_counter: AtomicU32,
}

impl ClockRef {
    /// Sample all clocks from `crg` and return an initialised `ClockRef`.
    ///
    /// Place the result in `'static` storage before use:
    ///
    /// ```ignore
    /// use static_cell::StaticCell;
    /// static CLOCK: StaticCell<ClockRef> = StaticCell::new();
    /// let clock_ref: &'static ClockRef = CLOCK.init(ClockRef::from_crg(&crg));
    /// let perf_ctl = crg.into_perf_control(clock_ref);
    /// ```
    pub fn from_crg(crg: &super::Crg) -> Self {
        Self::from_clocks(&Clocks::sample(crg.cfg))
    }

    pub(crate) fn from_clocks(c: &Clocks) -> Self {
        Self {
            xosc:  c.xosc.to_Hz(),
            rcosc: c.rcosc.to_Hz(),
            rtc:   c.rtc.to_Hz(),
            scu:   c.scu.to_Hz(),
            hpadc: c.hpadc.to_Hz(),
            lpadc: c.lpadc.to_Hz(),
            com:       AtomicU32::new(c.com.to_Hz()),
            appsmp:    AtomicU32::new(c.appsmp.to_Hz()),
            syspll:    AtomicU32::new(c.syspll.to_Hz()),
            sys:       AtomicU32::new(c.sys.to_Hz()),
            sys_ahb:   AtomicU32::new(c.sys_ahb.to_Hz()),
            sys_apb:   AtomicU32::new(c.sys_apb.to_Hz()),
            sys_sfc:   AtomicU32::new(c.sys_sfc.to_Hz()),
            pmui2c:    AtomicU32::new(c.pmui2c.to_Hz()),
            gps_cpu:   AtomicU32::new(c.gps_cpu.to_Hz()),
            gps_ahb:   AtomicU32::new(c.gps_ahb.to_Hz()),
            usb:       AtomicU32::new(c.usb.to_Hz()),
            sdio:      AtomicU32::new(c.sdio.to_Hz()),
            img_uart:  AtomicU32::new(c.img_uart.to_Hz()),
            img_spi:   AtomicU32::new(c.img_spi.to_Hz()),
            img_wspi:  AtomicU32::new(c.img_wspi.to_Hz()),
            img_vsync: AtomicU32::new(c.img_vsync.to_Hz()),
            gen_counter: AtomicU32::new(0),
        }
    }

    /// Update all perf-dependent atomics from a fresh `Clocks` sample.
    ///
    /// Called by [`PerfControl`] after each `request_perf` / `set_gear`. Uses
    /// `Release` ordering — pairs with the `Acquire` loads in the accessors below.
    pub(crate) fn resample(&self, c: &Clocks) {
        self.com.store(c.com.to_Hz(),             Ordering::Release);
        self.appsmp.store(c.appsmp.to_Hz(),       Ordering::Release);
        self.syspll.store(c.syspll.to_Hz(),       Ordering::Release);
        self.sys.store(c.sys.to_Hz(),             Ordering::Release);
        self.sys_ahb.store(c.sys_ahb.to_Hz(),     Ordering::Release);
        self.sys_apb.store(c.sys_apb.to_Hz(),     Ordering::Release);
        self.sys_sfc.store(c.sys_sfc.to_Hz(),     Ordering::Release);
        self.pmui2c.store(c.pmui2c.to_Hz(),       Ordering::Release);
        self.gps_cpu.store(c.gps_cpu.to_Hz(),     Ordering::Release);
        self.gps_ahb.store(c.gps_ahb.to_Hz(),     Ordering::Release);
        self.usb.store(c.usb.to_Hz(),             Ordering::Release);
        self.sdio.store(c.sdio.to_Hz(),           Ordering::Release);
        self.img_uart.store(c.img_uart.to_Hz(),   Ordering::Release);
        self.img_spi.store(c.img_spi.to_Hz(),     Ordering::Release);
        self.img_wspi.store(c.img_wspi.to_Hz(),   Ordering::Release);
        self.img_vsync.store(c.img_vsync.to_Hz(), Ordering::Release);
        // Bump last: this `Release` publishes every rate store above to any
        // `Acquire` reader of `generation`.
        self.gen_counter.fetch_add(1, Ordering::Release);
    }

    /// COM-bus clock (UART1 / SPI0 / I2C2). Perf-dependent; changes with `request_perf`.
    pub fn com(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(self.com.load(Ordering::Acquire))
    }
    /// APP Cortex-M4 clock. Perf-dependent.
    pub fn appsmp(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(self.appsmp.load(Ordering::Acquire))
    }
    /// System PLL output. Perf-dependent.
    pub fn syspll(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(self.syspll.load(Ordering::Acquire))
    }
    /// SYS/IOP root bus. Perf-dependent.
    pub fn sys(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(self.sys.load(Ordering::Acquire))
    }
    /// SYS AHB bus. Perf-dependent.
    pub fn sys_ahb(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(self.sys_ahb.load(Ordering::Acquire))
    }
    /// SYS APB bus. Perf-dependent.
    pub fn sys_apb(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(self.sys_apb.load(Ordering::Acquire))
    }
    /// SPI-flash controller clock. Perf-dependent.
    pub fn sys_sfc(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(self.sys_sfc.load(Ordering::Acquire))
    }
    /// PMU I2C (I2C4) clock. Perf-dependent (when sourced from `sys_apb`).
    pub fn pmui2c(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(self.pmui2c.load(Ordering::Acquire))
    }
    /// GPS CPU clock. Perf-dependent.
    pub fn gps_cpu(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(self.gps_cpu.load(Ordering::Acquire))
    }
    /// GPS AHB clock. Perf-dependent.
    pub fn gps_ahb(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(self.gps_ahb.load(Ordering::Acquire))
    }
    /// USB clock (gear-divided from `appsmp`). Perf-dependent.
    pub fn usb(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(self.usb.load(Ordering::Acquire))
    }
    /// SDIO clock (gear-divided from `appsmp`). Perf-dependent.
    pub fn sdio(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(self.sdio.load(Ordering::Acquire))
    }
    /// IMG-UART clock (UART2, gear-divided from `appsmp`). Perf-dependent.
    pub fn img_uart(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(self.img_uart.load(Ordering::Acquire))
    }
    /// IMG-SPI clock (SPI4, gear-divided from `appsmp`). Perf-dependent.
    pub fn img_spi(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(self.img_spi.load(Ordering::Acquire))
    }
    /// IMG-WSPI clock (SPI5, gear-divided from `appsmp`). Perf-dependent.
    pub fn img_wspi(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(self.img_wspi.load(Ordering::Acquire))
    }
    /// IMG VSYNC clock. Perf-dependent.
    pub fn img_vsync(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(self.img_vsync.load(Ordering::Acquire))
    }

    /// Monotonic generation counter, incremented once on **every** [`PerfControl`]
    /// mutation that resamples (`request_perf`, `set_gear`, `set_spi_gear`). It
    /// lets a peripheral built with `from_ref` detect that its cached baud/SCK
    /// divisor may be stale and call `reconfigure` — without this crate storing
    /// any per-peripheral state:
    ///
    /// ```ignore
    /// let mut last = clock.generation();
    /// // ... after perf_ctl.request_perf(...):
    /// let gen = clock.generation();
    /// if gen != last {
    ///     uart.reconfigure(&config, clock)?;
    ///     last = gen;
    /// }
    /// ```
    ///
    /// This is a single global epoch, not a per-clock signal: it bumps even for a
    /// change that doesn't move *this* peripheral's base clock (e.g. a `set_gear`
    /// on an unrelated port), so a gated `reconfigure` may occasionally rewrite an
    /// identical divisor — a cheap glitch-only no-op, not a correctness issue.
    /// Compare a specific rate accessor (e.g. [`com`](Self::com)) if you need
    /// per-clock granularity.
    pub fn generation(&self) -> u32 {
        self.gen_counter.load(Ordering::Acquire)
    }
}

/// Exclusive handle for requesting operating-point changes on the Option-2 path.
///
/// Uses `&self` (not `&mut self`) because [`ClockRef`] updates use `AtomicU32`
/// interior mutability. `!Sync` prevents concurrent `request_perf` calls from
/// different execution contexts — `pm::request_perf` is not re-entrant (it
/// blocks on the ICC CPU FIFO). `Send` — may be moved to a dedicated task or
/// interrupt handler.
///
/// # Safety contract
///
/// Unlike an owned [`Clock`](super::profile::Clock)'s consuming transitions
/// ([`into_hp`](super::profile::Clock::into_hp) /
/// [`into_lp`](super::profile::Clock::into_lp)), this method does **not**
/// have borrow-checker enforcement that all peripherals are quiesced. The
/// hardware clocks change during the ICC handshake inside this
/// call; any in-flight UART/SPI byte will be corrupted. The caller must ensure
/// all in-flight operations are complete (e.g. by calling `flush()`) before
/// invoking `request_perf`. After the call, baud/gear divisors cached in
/// peripheral drivers are stale — reconstruct them via `from_ref` to get
/// correct rates at the new operating point.
///
/// Garbled bytes are a correctness bug, not Rust undefined behaviour: no memory
/// safety invariants are violated.
pub struct PerfControl {
    shared: &'static ClockRef,
    cfg:    Config,
    /// Makes `PerfControl` `!Sync` while staying `Send`, on stable Rust (a
    /// negative `impl !Sync` is nightly-only). `Cell<()>` is `Send + !Sync`,
    /// and `PhantomData` inherits those marker traits without storing anything.
    /// This stops `&PerfControl` from being shared across execution contexts —
    /// so the non-re-entrant [`request_perf`](Self::request_perf) can't be
    /// called concurrently — while `Send` still lets the single owner be moved
    /// to a dedicated task or interrupt handler.
    _not_sync: PhantomData<Cell<()>>,
}

impl PerfControl {
    pub(crate) fn new(shared: &'static ClockRef, cfg: Config) -> Self {
        Self {
            shared,
            cfg,
            _not_sync: PhantomData,
        }
    }

    /// Request a CPU/bus operating-point change and update all [`ClockRef`] atomics.
    ///
    /// Drives the ICC `FREQLOCK` handshake to completion (blocking), then
    /// issues a `SeqCst` fence and re-samples all live HW clock registers into
    /// the `ClockRef` atomics. See the [`PerfControl`] doc for the safety
    /// contract around in-flight peripheral operations.
    pub fn request_perf(&self, perf: Perf) -> Result<(), PmError> {
        pm::request_perf(perf)?;
        // SeqCst fence: ensure the SYSIOP handshake completion (Release writes
        // in pm.rs via Mailbox) is visible to this core before we re-read the
        // clock registers.
        core::sync::atomic::fence(Ordering::SeqCst);
        self.shared.resample(&Clocks::sample(self.cfg));
        Ok(())
    }

    /// Set an APP-local gear divider and update the relevant [`ClockRef`] atomics.
    ///
    /// Mirrors [`Clock::set_gear`](super::profile::Clock::set_gear).
    pub fn set_gear(&self, id: PeripheralId, divisor: u32) -> Result<(), GearError> {
        id.set_gear(divisor)?;
        self.shared.resample(&Clocks::sample(self.cfg));
        Ok(())
    }

    /// Set an SPI port's gear divider to target a max frequency, and update atomics.
    ///
    /// Mirrors [`Clock::set_spi_gear`](super::profile::Clock::set_spi_gear).
    pub fn set_spi_gear(&self, port: PeripheralId, maxfreq: Hertz<u32>) -> Result<(), GearError> {
        let appsmp = self.shared.appsmp();
        port.set_spi_gear(appsmp, maxfreq)?;
        self.shared.resample(&Clocks::sample(self.cfg));
        Ok(())
    }
}
