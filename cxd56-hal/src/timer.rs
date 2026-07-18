//! General-purpose timers (Arm PrimeCell SP804).
//!
//! The CXD5602 exposes the two channels of an SP804 dual-input timer as
//! separate peripherals: TIMER0 at `0xe004_3000` and TIMER1 at `0xe004_3020`.
//! Both share the same register layout ([`pac::timer0::RegisterBlock`]) and are
//! fully independent — [`Timer`] is generic over the channel via the sealed
//! [`TimerPeriph`] trait, so `Timer<pac::Timer0>` and `Timer<pac::Timer1>` can
//! coexist. Each channel is a 32-bit down-counter with three modes:
//!
//! * **periodic** — reloads from LOAD on zero and keeps counting; the latched
//!   interrupt fires every period ([`start_periodic`](Timer::start_periodic)),
//! * **one-shot** — halts at zero with the interrupt latched
//!   ([`start_oneshot`](Timer::start_oneshot)),
//! * **free-running** — counts down from `u32::MAX` and wraps, for elapsed-time
//!   measurement ([`start_free_running`](Timer::start_free_running)).
//!
//! # Clocking and the `'clk` borrow
//!
//! The SP804 counts at the CPU/AHB base clock
//! ([`Clock::cpu_baseclk`](crate::clocks::Clock::cpu_baseclk)), which is
//! **perf-dependent** (HP ≈ 156 MHz / LP ≈ 39 MHz) — the same dynamic clock the
//! [`watchdog`](crate::watchdog) runs from, and what NuttX's `cxd56_timer.c`
//! samples via `cxd56_get_cpu_baseclk()`. An operating-point change while a
//! timer runs would silently rescale every period mid-count, so [`Timer`]
//! borrows the [`Clock`] for `'clk`: while a timer is alive, an
//! operating-point change ([`Clock::into_hp`](crate::clocks::Clock::into_hp) /
//! [`Clock::into_lp`](crate::clocks::Clock::into_lp), which need ownership of
//! the `Clock`) cannot happen. To change the operating point,
//! [`free`](Timer::free) the timer first, change perf, then create a new one.
//!
//! A `CONTROL.DIV` prescaler (/1, /16, /256) sits between the base clock and
//! the counter; [`start_periodic`]/[`start_oneshot`] pick the smallest divider
//! whose tick count fits the 32-bit counter, keeping the best resolution for
//! short periods (6.4 ns/tick at HP) while still reaching multi-minute ones.
//!
//! [`start_periodic`]: Timer::start_periodic
//! [`start_oneshot`]: Timer::start_oneshot
//!
//! # Interrupts
//!
//! Starting a timer always leaves the interrupt **masked**; opt in with
//! [`enable_interrupt`](Timer::enable_interrupt), open the chip-level INTC gate
//! with [`interrupt::enable`](crate::interrupt::enable), and unmask the NVIC
//! line ([`Timer::interrupt`] names it). All three layers are required — the
//! INTC sits in front of the NVIC, so without it a real peripheral interrupt
//! never reaches the core (see [`crate::interrupt`]). The order is safe in any
//! permutation: the raw
//! status (RIS) latches on every zero-crossing regardless of the mask, so a
//! tick that lands before the unmask is delivered, not lost. The interrupt is
//! **level-latched** — the handler must clear it via INTCLR or it re-enters
//! forever. Because a `Timer` borrowing a stack-local [`Clock`] is not
//! `'static` (it cannot live in a `Mutex<RefCell<…>>` for the handler to
//! reach), the module provides the free function [`clear_pending`] to do that
//! from an ISR without owning the `Timer`:
//!
//! ```ignore
//! #[interrupt]
//! fn TIMER0() {
//!     timer::clear_pending::<pac::Timer0>();
//!     TICKS.fetch_add(1, Ordering::Relaxed);
//! }
//! ```
//!
//! # Example
//! ```ignore
//! use cxd56_hal::timer::Timer;
//! use fugit::ExtU32; // for `.millis()`
//!
//! let mut t = Timer::new(p.timer0, &clock)?;
//! t.start_periodic(500u32.millis())?;
//! loop {
//!     nb::block!(t.wait()).unwrap(); // every 500 ms
//! }
//! ```

use core::convert::Infallible;
use core::marker::PhantomData;
use embedded_hal_nb::nb;
use fugit::{Hertz, MicrosDurationU32, MicrosDurationU64};
use thiserror::Error;

use crate::clocks::{Clock, PerfState};
use crate::pac;

/// Input-clock prescaler between the CPU base clock and the counter
/// (`CONTROL.DIV`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Prescaler {
    /// Count at the full CPU base clock.
    Div1,
    /// Divide the input clock by 16.
    Div16,
    /// Divide the input clock by 256.
    Div256,
}

impl Prescaler {
    const fn divisor(self) -> u32 {
        match self {
            Self::Div1 => 1,
            Self::Div16 => 16,
            Self::Div256 => 256,
        }
    }

    /// `CONTROL.DIV` field encoding.
    const fn bits(self) -> u8 {
        match self {
            Self::Div1 => 0b00,
            Self::Div16 => 0b01,
            Self::Div256 => 0b10,
        }
    }
}

/// Errors from [`Timer::new`] and the `start_*` methods.
#[derive(Debug, Error)]
pub enum TimerError {
    /// The CPU base clock reads as zero (clock not configured / gated).
    #[error("CPU base clock is zero")]
    ClockUnavailable,
    /// Requested duration is zero.
    #[error("duration must be non-zero")]
    ZeroDuration,
    /// Duration exceeds the 32-bit counter even at /256 prescale.
    #[error("duration exceeds the 32-bit counter even at /256 prescale")]
    DurationTooLong,
}

mod sealed {
    use crate::pac;

    /// Both channel tokens `Deref` to the shared `timer0::RegisterBlock`; the
    /// trait only has to supply what differs: the NVIC line and the MMIO base.
    pub trait Sealed: core::ops::Deref<Target = pac::timer0::RegisterBlock> {
        const INTERRUPT: pac::Interrupt;
        /// Register base, for owner-less INTCLR writes from ISRs
        /// ([`clear_pending`](super::clear_pending)).
        fn regs() -> *const pac::timer0::RegisterBlock;
    }
}

/// Maps a PAC timer token to its register block and NVIC line.
///
/// Sealed: implemented exactly for [`pac::Timer0`] and [`pac::Timer1`].
pub trait TimerPeriph: sealed::Sealed {}

impl sealed::Sealed for pac::Timer0 {
    const INTERRUPT: pac::Interrupt = pac::Interrupt::TIMER0;
    fn regs() -> *const pac::timer0::RegisterBlock {
        pac::Timer0::PTR
    }
}
impl TimerPeriph for pac::Timer0 {}

impl sealed::Sealed for pac::Timer1 {
    const INTERRUPT: pac::Interrupt = pac::Interrupt::TIMER1;
    fn regs() -> *const pac::timer0::RegisterBlock {
        pac::Timer1::PTR
    }
}
impl TimerPeriph for pac::Timer1 {}

/// Convert a microsecond duration at `freq_hz` into a LOAD value and the
/// smallest prescaler whose tick count fits the 32-bit counter.
///
/// Rounds to the nearest input-clock tick, then to the nearest prescaled tick
/// (round-to-nearest mirrors the watchdog's `timeout_to_load`). With a
/// [`MicrosDurationU32`] (max ≈ 4295 s) and a base clock ≤ 160 MHz, /256
/// always fits, so [`TimerError::DurationTooLong`] is defensive only.
fn compute_load(freq_hz: u32, us: u32) -> Result<(u32, Prescaler), TimerError> {
    if us == 0 {
        return Err(TimerError::ZeroDuration);
    }
    if freq_hz == 0 {
        return Err(TimerError::ClockUnavailable);
    }
    // Max ≈ 160e6 × 4.295e9 / 1e6 + … ≈ 6.9e11 — far below u64 overflow.
    let ticks = (freq_hz as u64 * us as u64 + 500_000) / 1_000_000;
    for p in [Prescaler::Div1, Prescaler::Div16, Prescaler::Div256] {
        let d = p.divisor() as u64;
        let load = (ticks + d / 2) / d;
        if load <= u32::MAX as u64 {
            // LOAD = 0 would latch the interrupt immediately and (periodic)
            // spin at the input clock; clamp the shortest request to one tick.
            return Ok(((load as u32).max(1), p));
        }
    }
    Err(TimerError::DurationTooLong)
}

/// SP804 timer channel driver.
///
/// Consumes the channel's PAC token (`pac::Timer0` / `pac::Timer1`) for
/// exclusive ownership and borrows the [`Clock`] for `'clk` (see the
/// [module docs](self) for the perf-lock rationale). Constructed stopped with
/// the interrupt masked and cleared; call a `start_*` method to run.
///
/// Unlike [`Watchdog`](crate::watchdog::Watchdog) no manual `Send` impl is
/// needed: every field (the ZST PAC token included) is already `Send`.
pub struct Timer<'clk, T: TimerPeriph> {
    /// Exclusive hardware ownership; `Deref`s to the register block.
    periph: T,
    /// CPU base clock sampled at construction — kept valid by the `'clk`
    /// borrow, which blocks the `into_hp`/`into_lp` transitions.
    freq: Hertz<u32>,
    /// Prescaler of the last `start_*` call, for [`elapsed`](Self::elapsed).
    div: Prescaler,
    /// LOAD value of the last `start_*` call, for [`elapsed`](Self::elapsed).
    load: u32,
    /// Ties this timer to the `Clock` borrow, blocking the
    /// `into_hp`/`into_lp` transitions.
    _clk: PhantomData<&'clk ()>,
}

impl<'clk, T: TimerPeriph> Timer<'clk, T> {
    /// Take ownership of a timer channel and force it into a known state:
    /// stopped, 32-bit, interrupt masked, no stale latched interrupt.
    ///
    /// CONTROL resets to `0x20` — INTENABLE set by the hardware! — and a prior
    /// boot stage may have left the channel running with RIS latched, so the
    /// whole register is written to zero (not `reset()`, which would re-set
    /// INTENABLE) and INTCLR is pulsed. After `new()` MIS is guaranteed clear,
    /// so unmasking the NVIC line at any later point cannot take a spurious
    /// interrupt.
    pub fn new<P: PerfState>(timer: T, clock: &'clk Clock<P>) -> Result<Self, TimerError> {
        let freq = clock.cpu_baseclk();
        if freq.to_Hz() == 0 {
            return Err(TimerError::ClockUnavailable);
        }
        // SAFETY: 0 is a valid CONTROL value (everything disabled); any value
        // may be written to INTCLR (write-one-ignored-data clear strobe).
        timer.control().write(|w| unsafe { w.bits(0) });
        timer.intclr().write(|w| unsafe { w.bits(1) });
        Ok(Self {
            periph: timer,
            freq,
            div: Prescaler::Div1,
            load: 0,
            _clk: PhantomData,
        })
    }

    /// Stop, reconfigure, and start the counter.
    ///
    /// SP804 programming order: disable while reconfiguring, drop any stale
    /// latched interrupt, load the counter, then enable with the new mode in a
    /// single CONTROL write. SIZE is always 32-bit — 16-bit mode only truncates
    /// the range (≈ 420 µs max at HP /1) with no benefit on this SoC, matching
    /// NuttX's hardwired `TIMERCTRL_SIZE_32BIT`. INTENABLE is always left
    /// clear; see the module docs for the masked-start contract.
    fn start(&mut self, load: u32, div: Prescaler, periodic: bool, oneshot: bool) {
        let t = &self.periph;
        // SAFETY: as in `new()` — CONTROL = 0 and INTCLR strobe are valid;
        // LOAD accepts any 32-bit count.
        t.control().write(|w| unsafe { w.bits(0) });
        t.intclr().write(|w| unsafe { w.bits(1) });
        t.load().write(|w| unsafe { w.bits(load) });
        t.control().write(|w| {
            w.size().set_bit();
            // SAFETY: `Prescaler::bits` only produces the defined encodings
            // 0b00/0b01/0b10 for the 2-bit DIV field.
            unsafe { w.div().bits(div.bits()) };
            if periodic {
                w.periodic().set_bit();
            }
            if oneshot {
                w.mode().set_bit();
            }
            w.enable().set_bit()
        });
        self.div = div;
        self.load = load;
    }

    /// Run the counter in periodic mode: it reloads on zero and latches the
    /// interrupt every `period`. Restarts (with a fresh phase) if already
    /// running. The interrupt is left masked; see
    /// [`enable_interrupt`](Self::enable_interrupt).
    pub fn start_periodic(&mut self, period: MicrosDurationU32) -> Result<(), TimerError> {
        let (load, div) = compute_load(self.freq.to_Hz(), period.to_micros())?;
        self.start(load, div, true, false);
        Ok(())
    }

    /// Run the counter in one-shot mode: it halts at zero with the interrupt
    /// latched. Re-arm by calling `start_oneshot` again.
    pub fn start_oneshot(&mut self, timeout: MicrosDurationU32) -> Result<(), TimerError> {
        let (load, div) = compute_load(self.freq.to_Hz(), timeout.to_micros())?;
        self.start(load, div, false, true);
        Ok(())
    }

    /// Run the counter free-running from `u32::MAX` as an elapsed-time
    /// reference (see [`elapsed`](Self::elapsed)); it wraps — and latches the
    /// interrupt — on each zero-crossing (at HP /256: every ≈ 7000 s).
    ///
    /// The prescaler is the caller's resolution/range trade-off, e.g.
    /// [`Prescaler::Div256`] gives ≈ 1.6 µs ticks at HP.
    pub fn start_free_running(&mut self, prescaler: Prescaler) {
        self.start(u32::MAX, prescaler, false, false);
    }

    /// Non-blocking wait for expiry: `Ok(())` once the (raw) interrupt has
    /// latched — clearing it in the process — and `WouldBlock` otherwise.
    /// Composes with `nb::block!` for a blocking wait.
    pub fn wait(&mut self) -> nb::Result<(), Infallible> {
        if self.periph.ris().read().timer_interrupt().bit_is_set() {
            self.clear_interrupt();
            Ok(())
        } else {
            Err(nb::Error::WouldBlock)
        }
    }

    /// Whether the counter has reached zero since the last clear (raw status,
    /// independent of the interrupt mask). Does not clear the latch.
    pub fn is_expired(&self) -> bool {
        self.periph.ris().read().timer_interrupt().bit_is_set()
    }

    /// Whether the interrupt is latched **and** unmasked — what the NVIC sees.
    pub fn is_pending(&self) -> bool {
        self.periph.mis().read().timer_interrupt().bit_is_set()
    }

    /// Unmask the interrupt at the peripheral (`CONTROL.INTENABLE`). The NVIC
    /// line must also be unmasked: `NVIC::unmask(timer.interrupt())`.
    pub fn enable_interrupt(&mut self) {
        self.periph.control().modify(|_, w| w.intenable().set_bit());
    }

    /// Mask the interrupt at the peripheral. The raw status keeps latching.
    pub fn disable_interrupt(&mut self) {
        self.periph
            .control()
            .modify(|_, w| w.intenable().clear_bit());
    }

    /// Clear the latched interrupt. From an ISR (where the `Timer` value is
    /// out of reach) use the free function [`clear_pending`] instead.
    pub fn clear_interrupt(&mut self) {
        // SAFETY: any value may be written to INTCLR (clear strobe).
        self.periph.intclr().write(|w| unsafe { w.bits(1) });
    }

    /// NVIC line of this channel — pass to `cortex_m::peripheral::NVIC::unmask`.
    pub const fn interrupt(&self) -> pac::Interrupt {
        T::INTERRUPT
    }

    /// Current counter value (counts down).
    pub fn counter(&self) -> u32 {
        self.periph.value().read().bits()
    }

    /// Time since the counter last (re)loaded. Free-running: time since start
    /// (wraps with the counter). Periodic: time into the current period.
    /// Meaningless before the first `start_*` call.
    pub fn elapsed(&self) -> MicrosDurationU64 {
        let ticks = self.load.wrapping_sub(self.counter()) as u64;
        // Exact u64 math — deliberately NOT `ticks / tick_hz()`: the prescaled
        // rate need not be integral (39 MHz / 256 = 152 343.75 Hz), and the
        // truncated `tick_hz` would accumulate error over long spans.
        let us = ticks * self.div.divisor() as u64 * 1_000_000 / self.freq.to_Hz() as u64;
        MicrosDurationU64::from_ticks(us)
    }

    /// Approximate counter tick rate of the current configuration
    /// (base clock / prescaler, **truncated** — see [`elapsed`](Self::elapsed)
    /// for why exact arithmetic avoids this value).
    pub fn tick_hz(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(self.freq.to_Hz() / self.div.divisor())
    }

    /// Stop the counter and clear any latched interrupt (CONTROL = 0 + INTCLR,
    /// mirroring NuttX `cxd56_timer.c` stop). The configuration is discarded;
    /// restart with a `start_*` method.
    pub fn stop(&mut self) {
        // SAFETY: as in `new()`.
        self.periph.control().write(|w| unsafe { w.bits(0) });
        self.periph.intclr().write(|w| unsafe { w.bits(1) });
    }

    /// Stop the channel and return the PAC token for reuse.
    ///
    /// Calling `free` consumes `self` and prevents [`Drop`] from running a
    /// second `stop()`.
    pub fn free(mut self) -> T {
        self.stop();
        let md = core::mem::ManuallyDrop::new(self);
        // SAFETY: `periph` is read exactly once and `md` is never used again,
        // so there is no double-move or use-after-read; ManuallyDrop suppresses
        // the struct-level Drop (which would stop the already-stopped timer).
        unsafe { core::ptr::read(&md.periph) }
    }
}

impl<T: TimerPeriph> Drop for Timer<'_, T> {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Blocking delay via one-shot countdown (`embedded_hal::delay::DelayNs`).
///
/// Each call **reconfigures the channel** (one-shot, /1, interrupt masked) and
/// busy-polls — do not interleave with a running periodic/free-running
/// configuration on the same instance. The tick count always fits /1:
/// `u32::MAX` ns ≈ 4.3 s × 160 MHz < 2³².
impl<T: TimerPeriph> embedded_hal::delay::DelayNs for Timer<'_, T> {
    fn delay_ns(&mut self, ns: u32) {
        // Ceiling division: DelayNs contracts a delay of *at least* `ns`.
        let ticks = (self.freq.to_Hz() as u64 * ns as u64).div_ceil(1_000_000_000);
        if ticks == 0 {
            return;
        }
        self.start(ticks as u32, Prescaler::Div1, false, true);
        while !self.is_expired() {}
        self.stop(); // also clears the latched interrupt
    }
}

/// Clear the latched (level) interrupt of channel `T` without owning the
/// [`Timer`].
///
/// Intended for `#[interrupt] fn TIMER0()` handlers, where the `Timer` value —
/// non-`'static` because of its [`Clock`] borrow — is unreachable (see the
/// [module docs](self)). Sound alongside a live owner: INTCLR is write-only
/// with no effect beyond dropping the latch, and the write is a single
/// volatile store.
pub fn clear_pending<T: TimerPeriph>() {
    // SAFETY: `T::regs()` is the channel's fixed, properly-aligned MMIO base,
    // valid for the program's lifetime; any value may be written to INTCLR.
    let rb = unsafe { &*T::regs() };
    rb.intclr().write(|w| unsafe { w.bits(1) });
}
