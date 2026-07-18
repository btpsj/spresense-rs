//! Async one-shot tick delay backed by a hardware timer.
//!
//! This is the HAL's interrupt-driven async time source: a future that arms a
//! hardware compare/alarm, suspends, and is woken by that peripheral's interrupt
//! — so the core can `WFE`-sleep (and the executor run other tasks) for the whole
//! delay instead of busy-spinning a counter.
//!
//! # Swappable backing (`TickSource`)
//!
//! The timing peripheral sits behind the private [`TickSource`] seam and is
//! selected by a Cargo feature, mirroring embassy's `time-driver-*` convention.
//! Enable **exactly one**:
//!
//! * **`async-delay-rtc`** (default) — RTC0 **alarm channel 0**. The RTC runs at a
//!   fixed 32.768 kHz and is **perf-invariant and always-on**, so a delay is the
//!   same real time at every operating point and survives a clock change. Tick
//!   granularity is ~30.5 µs. This is the right default for low-power work, and it
//!   needs no clock setup.
//! * **`async-delay-timer`** — SP804 **`TIMER0`** (see [`crate::timer`]) in
//!   one-shot mode. It counts at the CPU/AHB base clock, so the resolution is
//!   sub-µs (≈6.4 ns/tick at HP) but the rate is **perf-dependent**: the backing
//!   must sample the base clock before first use via [`init`] or [`Delay::new`],
//!   and the operating point must not change while a delay is in flight (the same
//!   constraint [`Timer`](crate::timer::Timer) enforces with its `Clock` borrow).
//!
//! Both implement the same [`TickSource`]; nothing above the seam — the public
//! API, the GPIO edge-arm settle — changes between them. A delay is always
//! requested in real time ([`after_micros`] / the [`DelayNs`] impl); only the raw
//! [`after_ticks`] escape hatch is in source-specific ticks.
//!
//! # The app must forward the source interrupt
//!
//! Like the GPIO [`Wait`](crate::gpio::Wait) API, a library cannot define the
//! interrupt vector itself (the PAC weakly binds it to `DefaultHandler`), so the
//! application owns the handler and forwards it to [`on_interrupt`]. The vector is
//! the active backing's: **`RTC0_A0`** (rtc) or **`TIMER0`** (timer):
//!
//! ```ignore
//! #[interrupt]
//! fn RTC0_A0() { // or `TIMER0` with `async-delay-timer`
//!     cxd56_hal::async_delay::on_interrupt(cxd56_hal::pac::Interrupt::RTC0_A0);
//! }
//! ```
//!
//! This is required by anything that awaits a delay here — including the GPIO
//! edge-arm baseline settle (`wait_for_*_edge`, see [`crate::gpio`]). Without the
//! handler the alarm fires into `DefaultHandler` and the delay never completes.
//!
//! # Concurrency (v1)
//!
//! A single delay may be in flight at a time (one alarm channel, one waker). That
//! covers the edge-arm settle and simple apps; a multi-waiter deadline queue (or
//! the RTC's other two channels) is a future extension behind the same seam.

use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll, Waker};
use cortex_m::peripheral::NVIC;
use critical_section::Mutex;

use crate::clocks::{Clock, PerfState};
use crate::pac;

// =============================================================================
// Source seam
// =============================================================================

/// A hardware tick source that can wake the core after a one-shot interval.
///
/// Implemented by exactly one backing peripheral, chosen by the `async-delay-*`
/// feature and aliased to [`Source`]. All methods are static: the source is a
/// single global peripheral reached through its PAC pointer.
///
/// The seam is paradigm-neutral so both an absolute-compare source (the RTC
/// alarm) and a relative one-shot source (the SP804 down-counter) fit: [`arm`]
/// returns an opaque *token* the [`expired`] check is handed back, which a
/// compare source uses as its deadline and a flag source ignores.
///
/// [`arm`]: TickSource::arm
/// [`expired`]: TickSource::expired
trait TickSource {
    /// The interrupt the app forwards to [`on_interrupt`].
    const INTERRUPT: pac::Interrupt;

    /// Tick rate in Hz. Constant for fixed-rate sources (RTC); for the timer it
    /// is the perf-dependent base clock sampled by [`set_clock`](Self::set_clock).
    fn tick_hz() -> u32;

    /// Sample any perf-dependent input clock the source needs. No-op default for
    /// fixed-rate sources; the timer backing latches the CPU base clock here.
    fn set_clock<P: PerfState>(_clock: &Clock<P>) {}

    /// Arm a one-shot wake `ticks` from now and return a source-defined token the
    /// poll hands back to [`expired`](Self::expired): an absolute deadline (tick
    /// count) for a compare source, unused (`0`) for a flag source. The baseline
    /// is sampled inside, *after* any programming handshake, so programming
    /// latency does not shorten the delay.
    fn arm(ticks: u64) -> u64;

    /// Has the wake armed with `token` fired yet?
    fn expired(token: u64) -> bool;

    /// ISR side: silence the interrupt so it cannot re-enter, and latch expiry for
    /// flag sources. Defaults to [`disarm`](Self::disarm) — a compare source
    /// re-derives expiry from its free-running counter, so clearing the hardware
    /// flag there loses nothing.
    fn ack_irq() {
        Self::disarm();
    }

    /// Clear the pending wake and disarm. Idempotent; called from the poll when it
    /// observes expiry and as cleanup.
    fn disarm();
}

// =============================================================================
// RTC backing (alarm channel 0)
// =============================================================================

#[cfg(feature = "async-delay-rtc")]
mod rtc {
    use super::TickSource;
    use crate::pac;

    /// `SETALMPRECNT.ASET_BUSY` — a prior alarm-set is still reflecting.
    const ASET_BUSY: u32 = 1 << 16;
    /// `ALMOUTEN.ALM_BUSY` — a prior enable is still reflecting.
    const ALM_BUSY: u32 = 1 << 8;
    /// `ALMOUTEN.ALM_EN` — alarm match enable.
    const ALM_EN: u32 = 1 << 0;
    /// `ALMOUTEN.ALM_ERREN` — alarm error-output enable. Enabling it means a
    /// deadline accidentally set in the past raises the IRQ immediately (an error
    /// flag) instead of being lost — a free safety net (mirrors NuttX).
    const ALM_ERREN: u32 = 1 << 16;
    /// `ALMCLR`/`ALMFLG` channel-0 match + error flag bits.
    const ALM0_FLAGS: u32 = (1 << 0) | (1 << 16);

    pub(super) struct RtcTick;

    impl RtcTick {
        #[inline]
        fn rtc() -> &'static pac::rtc0::RegisterBlock {
            // SAFETY: RTC0 is the always-on clock peripheral; we issue only
            // single-register reads/writes to its alarm-0 and counter registers.
            unsafe { &*pac::Rtc0::PTR }
        }

        /// Current monotonic 47-bit RTC tick count.
        fn now() -> u64 {
            let rtc = Self::rtc();
            // Re-read the high half for a consistent (POSTCNT<<15)|PRECNT snapshot
            // across a PRECNT wrap; the 47-bit count is monotonic.
            loop {
                let hi = rtc.rtpostcnt().read().bits();
                let lo = rtc.rtprecnt().read().bits() & 0x7fff;
                if hi == rtc.rtpostcnt().read().bits() {
                    return ((hi as u64) << 15) | lo as u64;
                }
            }
        }
    }

    impl TickSource for RtcTick {
        const INTERRUPT: pac::Interrupt = pac::Interrupt::RTC0_A0;

        fn tick_hz() -> u32 {
            32_768
        }

        fn arm(ticks: u64) -> u64 {
            let rtc = Self::rtc();
            // SAFETY (all writes below): fixed alarm-0 MMIO; the values are the
            // documented compare/flag/enable encodings.
            // Drop any stale channel-0 flags, then wait for a previous set to land.
            rtc.almclr().write(|w| unsafe { w.bits(ALM0_FLAGS) });
            while rtc.setalmprecnt0().read().bits() & ASET_BUSY != 0 {}
            // Sample the baseline *here* (post-handshake) so the realized delay
            // counts the full `ticks` from now; write both compare halves, then
            // enable so the comparator only starts once both are set.
            let deadline = Self::now().wrapping_add(ticks);
            rtc.setalmpostcnt0()
                .write(|w| unsafe { w.bits((deadline >> 15) as u32) });
            rtc.setalmprecnt0()
                .write(|w| unsafe { w.bits((deadline & 0x7fff) as u32) });
            while rtc.almouten0().read().bits() & ALM_BUSY != 0 {}
            rtc.almouten0()
                .write(|w| unsafe { w.bits(ALM_EN | ALM_ERREN) });
            deadline
        }

        fn expired(deadline: u64) -> bool {
            // Time-based, so robust to a missed/early wake and to the ISR having
            // already cleared the alarm flag: the counter keeps advancing.
            Self::now() >= deadline
        }

        fn disarm() {
            let rtc = Self::rtc();
            // SAFETY: fixed alarm-0 MMIO; clear the flags and disable the channel.
            rtc.almclr().write(|w| unsafe { w.bits(ALM0_FLAGS) });
            rtc.almouten0().write(|w| unsafe { w.bits(0) });
        }
    }
}

// =============================================================================
// TIMER backing (SP804 TIMER0, one-shot)
// =============================================================================

#[cfg(feature = "async-delay-timer")]
mod timer {
    use super::TickSource;
    use crate::clocks::{Clock, PerfState};
    use crate::pac;
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    /// CPU/AHB base clock (Hz) the SP804 counts at, sampled by [`set_clock`] from
    /// the [`Clock`]. The async delay uses the /1 prescaler, so this is also the
    /// tick rate. `0` until set — awaiting a delay before then is a usage error
    /// (debug-asserted in [`tick_hz`]).
    ///
    /// [`set_clock`]: super::TickSource::set_clock
    /// [`tick_hz`]: super::TickSource::tick_hz
    static BASE_HZ: AtomicU32 = AtomicU32::new(0);
    /// Latched by the ISR ([`ack_irq`]) when the one-shot fires: the SP804's level
    /// interrupt must be cleared there (or it re-enters), which also clears the raw
    /// status, so completion is recorded here for a later poll to observe.
    ///
    /// [`ack_irq`]: super::TickSource::ack_irq
    static FIRED: AtomicBool = AtomicBool::new(false);

    pub(super) struct TimerTick;

    impl TimerTick {
        #[inline]
        fn regs() -> &'static pac::timer0::RegisterBlock {
            // SAFETY: TIMER0 is the reserved async-delay channel; we issue only
            // single-register accesses to its SP804 control/load/status registers.
            unsafe { &*pac::Timer0::PTR }
        }

        /// Raw (mask-independent) status: counter has reached zero since the clear.
        fn raw_pending() -> bool {
            Self::regs().ris().read().timer_interrupt().bit_is_set()
        }
    }

    impl TickSource for TimerTick {
        const INTERRUPT: pac::Interrupt = pac::Interrupt::TIMER0;

        fn tick_hz() -> u32 {
            let hz = BASE_HZ.load(Ordering::Relaxed);
            debug_assert!(
                hz != 0,
                "async-delay-timer: call async_delay::init or Delay::new (which \
                 sample the base clock) before awaiting a delay"
            );
            hz
        }

        fn set_clock<P: PerfState>(clock: &Clock<P>) {
            BASE_HZ.store(clock.cpu_baseclk().to_Hz(), Ordering::Relaxed);
            // Clear any latched state a boot stage may have left in the channel so
            // the first delay's `expired` does not read a stale raw-pending.
            Self::disarm();
        }

        fn arm(ticks: u64) -> u64 {
            let t = Self::regs();
            // One-shot LOAD must fit the 32-bit counter; clamp (our delays are
            // microseconds — far below the >27 s /1 range at any operating point —
            // and a zero LOAD would latch immediately, so clamp up to 1 too).
            let load = ticks.clamp(1, u32::MAX as u64) as u32;
            FIRED.store(false, Ordering::Release);
            // SAFETY (all writes): CONTROL=0 disables everything; the INTCLR strobe
            // and LOAD accept any value; the CONTROL bits set below are the SP804
            // one-shot / 32-bit / interrupt-enable / enable encodings.
            t.control().write(|w| unsafe { w.bits(0) });
            t.intclr().write(|w| unsafe { w.bits(1) });
            t.load().write(|w| unsafe { w.bits(load) });
            t.control().write(|w| {
                w.size().set_bit(); // 32-bit counter
                w.mode().set_bit(); // one-shot: halt at zero
                w.intenable().set_bit(); // raise IRQ on the zero-crossing
                w.enable().set_bit()
            });
            0 // flag source — the poll's token is unused
        }

        fn expired(_token: u64) -> bool {
            // `FIRED` covers the case where the ISR already serviced (and cleared)
            // the interrupt; the raw status covers a delay that elapsed during
            // programming or before the ISR ran.
            FIRED.load(Ordering::Acquire) || Self::raw_pending()
        }

        fn ack_irq() {
            FIRED.store(true, Ordering::Release);
            Self::disarm();
        }

        fn disarm() {
            let t = Self::regs();
            // SAFETY: stop the counter and clear the level-latched interrupt.
            t.control().write(|w| unsafe { w.bits(0) });
            t.intclr().write(|w| unsafe { w.bits(1) });
        }
    }
}

// =============================================================================
// Active source selection
// =============================================================================

/// The active tick source selected by the `async-delay-*` feature.
#[cfg(feature = "async-delay-rtc")]
type Source = rtc::RtcTick;
#[cfg(feature = "async-delay-timer")]
type Source = timer::TimerTick;

#[cfg(not(any(feature = "async-delay-rtc", feature = "async-delay-timer")))]
compile_error!(
    "enable exactly one async-delay source feature (async-delay-rtc or async-delay-timer)"
);
#[cfg(all(feature = "async-delay-rtc", feature = "async-delay-timer"))]
compile_error!("enable exactly one async-delay source feature, not both");

/// Human-readable name of the active backing, for diagnostics — `"RTC0 alarm 0"`
/// or `"SP804 TIMER0"`. Reflects what the **HAL** compiled, so printing it
/// confirms the `async-delay-*` feature actually propagated.
#[cfg(feature = "async-delay-rtc")]
pub const BACKING_NAME: &str = "RTC0 alarm 0";
/// Human-readable name of the active backing — see the rtc-feature variant.
#[cfg(feature = "async-delay-timer")]
pub const BACKING_NAME: &str = "SP804 TIMER0";

/// The interrupt the active backing uses — exactly what the app must forward to
/// [`on_interrupt`]. Exposed so an app can confirm (e.g. print) which source
/// compiled in.
pub const SOURCE_INTERRUPT: pac::Interrupt = Source::INTERRUPT;

// =============================================================================
// Waker + one-time INTC/NVIC setup
// =============================================================================

/// The single in-flight delay's waker, shared with the alarm ISR.
static WAKER: Mutex<RefCell<Option<Waker>>> = Mutex::new(RefCell::new(None));

fn register_waker(waker: &Waker) {
    critical_section::with(|cs| {
        let mut slot = WAKER.borrow(cs).borrow_mut();
        match &*slot {
            Some(existing) if existing.will_wake(waker) => {}
            _ => *slot = Some(waker.clone()),
        }
    });
}

fn wake() {
    let waker = critical_section::with(|cs| WAKER.borrow(cs).borrow_mut().take());
    if let Some(waker) = waker {
        waker.wake();
    }
}

/// Open the INTC gate and unmask the NVIC line for [`Source::INTERRUPT`] once.
///
/// Idempotent: the INTC set is a guarded read-modify-write of one bit and the
/// NVIC unmask is idempotent, so a redundant call is harmless. Called from
/// [`init`] / [`Delay::new`] and the first poll of a [`TickDelay`], so a delay
/// works whether or not the app constructed a [`Delay`] handle (it still must
/// forward the IRQ — and, on the timer backing, call [`init`] for the clock).
fn ensure_init() {
    static INITED: AtomicBool = AtomicBool::new(false);
    if INITED.load(Ordering::Acquire) {
        return;
    }
    crate::interrupt::enable(Source::INTERRUPT);
    // SAFETY: this HAL masks with `critical-section` (PRIMASK), not BASEPRI, so
    // unmasking a line cannot escape an in-progress critical section.
    unsafe { NVIC::unmask(Source::INTERRUPT) };
    INITED.store(true, Ordering::Release);
}

/// Sample the source's input clock and open its interrupt path.
///
/// Backing-agnostic, so a call site need not know which source is compiled in.
/// On the perf-invariant **RTC** backing this only opens the interrupt path (the
/// rate is fixed, and the first poll would open it anyway). On the **timer**
/// backing it *also samples the CPU base clock* the SP804 counts at — which is
/// **required** before awaiting a delay (including a GPIO edge-arm settle via the
/// free [`after_micros`]/[`after_ticks`] functions, which take no [`Clock`]).
///
/// [`Delay::new`] calls this for you; use `init` directly when you only use the
/// free-function delay API and do not want to reserve a [`Delay`] handle. The
/// operating point must not change between this call and an in-flight timer-backed
/// delay (the RTC backing is immune).
pub fn init<P: PerfState>(clock: &Clock<P>) {
    Source::set_clock(clock);
    ensure_init();
}

/// Convert a microsecond duration to source ticks, rounding up (a delay must be
/// *at least* the requested duration).
fn micros_to_ticks(us: u32) -> u64 {
    ((us as u64) * Source::tick_hz() as u64).div_ceil(1_000_000)
}

// =============================================================================
// Future + public API
// =============================================================================

/// Future that resolves after at least its requested number of source ticks.
///
/// Created by [`after_ticks`] / [`after_micros`] / [`Delay`]'s
/// [`DelayNs`](embedded_hal_async::delay::DelayNs) impl. On the first poll it arms
/// the source; thereafter it is woken by the source ISR (or any re-poll) and
/// completes once the source reports the interval has elapsed.
#[must_use = "a TickDelay does nothing unless awaited"]
pub struct TickDelay {
    ticks: u64,
    /// Source token from [`arm`](TickSource::arm), set on the first poll. `None`
    /// until armed.
    token: Option<u64>,
}

impl TickDelay {
    fn new(ticks: u64) -> Self {
        Self { ticks, token: None }
    }
}

impl Future for TickDelay {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        match this.token {
            // Armed: complete once the source reports expiry, else stay asleep.
            Some(token) => {
                if Source::expired(token) {
                    Source::disarm();
                    Poll::Ready(())
                } else {
                    register_waker(cx.waker());
                    Poll::Pending
                }
            }
            // First poll: open the gate, register the waker, then arm. Re-check
            // after arming so an interval that elapsed during programming does not
            // sleep waiting for a wake that already passed.
            None => {
                ensure_init();
                register_waker(cx.waker());
                let token = Source::arm(this.ticks);
                this.token = Some(token);
                if Source::expired(token) {
                    Source::disarm();
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

/// Await a one-shot delay of at least `ticks` **source** ticks — RTC ticks
/// (~30.5 µs) on the default backing, CPU-base-clock ticks (perf-dependent) on the
/// timer backing.
///
/// A raw escape hatch whose unit changes with the backing; prefer the real-time
/// [`after_micros`] / [`Delay`]'s [`DelayNs`](embedded_hal_async::delay::DelayNs)
/// for portable code. A free function so callers that do not want to dedicate the
/// source peripheral to a [`Delay`] handle (e.g. the GPIO edge-arm settle) can
/// still use it. The app must forward the source IRQ to [`on_interrupt`] and, on
/// the timer backing, have called [`init`] — see the [module docs](self).
pub fn after_ticks(ticks: u32) -> TickDelay {
    TickDelay::new(ticks as u64)
}

/// Await a one-shot delay of at least `us` microseconds, independent of the
/// backing (the duration is converted to source ticks at the source's current
/// rate). This is the portable free-function delay; see [`after_ticks`] for the
/// same caveats about IRQ forwarding and timer-backing [`init`].
pub fn after_micros(us: u32) -> TickDelay {
    TickDelay::new(micros_to_ticks(us))
}

/// Current tick rate (Hz) of the active backing: a fixed `32_768` on the RTC
/// backing, or the sampled CPU base clock on the timer backing (valid only after
/// [`init`] / [`Delay::new`]). Useful for confirming which source is live — the
/// timer backing reports the operating-point base clock (e.g. ~39 MHz at LP),
/// unmistakably different from the RTC's 32.768 kHz.
pub fn tick_hz() -> u32 {
    Source::tick_hz()
}

/// Async delay handle bound to the active source peripheral.
///
/// Owns the backing peripheral so the source is clearly reserved, and implements
/// the async [`DelayNs`](embedded_hal_async::delay::DelayNs) trait.
#[cfg(feature = "async-delay-rtc")]
pub struct Delay {
    _rtc: pac::Rtc0,
}

#[cfg(feature = "async-delay-rtc")]
impl Delay {
    /// Reserve the RTC as an async delay source and open its interrupt path. The
    /// RTC is perf-invariant, so the [`Clock`] is only sampled for API symmetry
    /// with the timer backing.
    pub fn new<P: PerfState>(rtc: pac::Rtc0, clock: &Clock<P>) -> Self {
        init(clock);
        Self { _rtc: rtc }
    }
}

/// Async delay handle bound to the active source peripheral.
///
/// Owns the backing peripheral so the source is clearly reserved, and implements
/// the async [`DelayNs`](embedded_hal_async::delay::DelayNs) trait.
#[cfg(feature = "async-delay-timer")]
pub struct Delay {
    _timer: pac::Timer0,
}

#[cfg(feature = "async-delay-timer")]
impl Delay {
    /// Reserve SP804 `TIMER0` as an async delay source, **sample the CPU base
    /// clock** it counts at (from `clock`), and open its interrupt path. The
    /// operating point must not change while a delay handed out here is in flight
    /// (the SP804 rate would rescale mid-count) — free the handle, change perf,
    /// then make a new one, exactly as [`Timer`](crate::timer::Timer) requires.
    pub fn new<P: PerfState>(timer: pac::Timer0, clock: &Clock<P>) -> Self {
        init(clock);
        Self { _timer: timer }
    }
}

// Source-independent methods (the active `Delay` struct, whichever feature
// selected it, gets these too).
impl Delay {
    /// Await a delay of at least `ticks` source ticks. See [`after_ticks`].
    pub fn after_ticks(&mut self, ticks: u32) -> TickDelay {
        TickDelay::new(ticks as u64)
    }

    /// Await a delay of at least `us` microseconds. See [`after_micros`].
    pub fn after_micros(&mut self, us: u32) -> TickDelay {
        TickDelay::new(micros_to_ticks(us))
    }
}

impl embedded_hal_async::delay::DelayNs for Delay {
    async fn delay_ns(&mut self, ns: u32) {
        // Ceiling-divide ns→ticks: DelayNs contracts a delay of *at least* `ns`.
        let ticks = ((ns as u64) * Source::tick_hz() as u64).div_ceil(1_000_000_000);
        TickDelay::new(ticks).await
    }
}

/// Source-interrupt entry point — forward the active source's vector here
/// (`RTC0_A0` with the default backing, `TIMER0` with `async-delay-timer`). See
/// the [module docs](self) for the one-line `#[interrupt]` handler. No-op for any
/// other interrupt.
pub fn on_interrupt(interrupt: pac::Interrupt) {
    if interrupt == Source::INTERRUPT {
        Source::ack_irq();
        wake();
    }
}
