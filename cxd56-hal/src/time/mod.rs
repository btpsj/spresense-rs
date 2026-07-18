//! Async time via the embassy [`Driver`](embassy_time_driver::Driver) trait: a
//! hardware timer plus an [`embassy_time_queue_utils`] software queue, so an
//! arbitrary number of `embassy_time::Timer`s can be in flight at once — unlike the
//! single-in-flight [`async_delay`](crate::async_delay).
//!
//! # Backing (`time-driver-*` feature)
//!
//! Enable **exactly one**:
//!
//! * **`time-driver-rtc`** (default) — RTC0 alarm 0. Perf-invariant, always-on,
//!   32.768 kHz; the 47-bit counter *is* the monotonic `now()`, 1:1 with
//!   `tick-hz-32_768` (no rescale, no overflow handling), ~30.5 µs granularity. The
//!   safe default and right for low-power work.
//! * **`time-driver-timer`** — SP804 `TIMER0`/`TIMER1`. `TIMER0` free-runs as the
//!   monotonic base, a software *period* counter extends it across 32-bit wraps;
//!   `TIMER1` is the one-shot alarm (the SP804 has no compare register, so a
//!   continuous `now()` plus an arbitrary alarm needs both halves). The SP804 counts
//!   the **perf-dependent** CPU base clock, so `now()`/alarms are software-rescaled
//!   to `tick-hz-1_000_000` (~1 µs). [`init`] must sample the base clock first, and
//!   the operating point must not change while the driver is live.
//!
//! # App wiring
//!
//! 1. Depend on `embassy-time` with the matching `tick-hz-*` and a `generic-queue-N`
//!    (and an executor **without** `integrated-timers`).
//! 2. Call [`init`] once after clock/perf setup.
//! 3. Forward the backing's interrupt vector(s) — the library cannot define them
//!    (the PAC weakly binds them to `DefaultHandler`), so the app owns the handler:
//!    * rtc: `RTC0_A0` → [`on_interrupt`].
//!    * timer: `TIMER0` → [`on_overflow_interrupt`], `TIMER1` → [`on_alarm_interrupt`].
//!
//! Without the forwarder(s) an alarm fires into `DefaultHandler` and timers never
//! complete (the same contract as [`crate::gpio`]'s `Wait` and [`crate::async_delay`]).

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::clocks::{Clock, PerfState};

#[cfg(feature = "time-driver-rtc")]
mod rtc;
#[cfg(feature = "time-driver-timer")]
mod timer;

#[cfg(feature = "time-driver-rtc")]
pub use rtc::on_interrupt;
#[cfg(feature = "time-driver-timer")]
pub use timer::{on_alarm_interrupt, on_overflow_interrupt};

/// Initialize the active time driver: open its interrupt path and — on the SP804
/// backing — sample the (perf-dependent) base clock the counter runs at. Call once
/// after clock/perf setup, before awaiting any `embassy_time` delay or a GPIO edge
/// wait. `clock` is used only by the SP804 backing (the RTC is perf-invariant).
pub fn init<P: PerfState>(clock: &Clock<P>) {
    #[cfg(feature = "time-driver-rtc")]
    {
        let _ = clock;
        rtc::init();
    }
    #[cfg(feature = "time-driver-timer")]
    timer::init(clock);
}

/// Human-readable name of the active backing — `"RTC0 alarm 0"` or `"SP804
/// TIMER0/1"`. Reflects what the **HAL** compiled, so printing it confirms the
/// `time-driver-*` feature reached `cxd56-hal`.
#[cfg(feature = "time-driver-rtc")]
pub const BACKING_NAME: &str = "RTC0 alarm 0";
/// Human-readable name of the active backing — see the rtc-feature variant.
#[cfg(feature = "time-driver-timer")]
pub const BACKING_NAME: &str = "SP804 TIMER0/1";

/// The driver's embassy tick rate ([`embassy_time_driver::TICK_HZ`]) — `32_768` on
/// the RTC backing, `1_000_000` on the SP804 backing.
pub const fn tick_hz() -> u64 {
    embassy_time_driver::TICK_HZ
}

/// µs → driver ticks at the compile-time `TICK_HZ`, rounding up (a delay is *at
/// least* `us`).
const fn micros_to_ticks(us: u32) -> u64 {
    ((us as u64) * embassy_time_driver::TICK_HZ).div_ceil(1_000_000)
}

/// Await at least `us` microseconds on the active driver. The GPIO edge-arm settle
/// uses this, so the HAL needs no dependency on the app-facing `embassy-time` crate.
/// Backing-agnostic: built on [`embassy_time_driver::now`]/[`schedule_wake`].
///
/// [`schedule_wake`]: embassy_time_driver::schedule_wake
pub(crate) fn after_micros(us: u32) -> Settle {
    Settle { ticks: micros_to_ticks(us), deadline: None }
}

/// Future for [`after_micros`]: latches an absolute deadline on the first poll, then
/// completes once `now()` reaches it (woken by the driver ISR via the shared queue).
pub(crate) struct Settle {
    ticks: u64,
    deadline: Option<u64>,
}

impl Future for Settle {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let deadline = match this.deadline {
            Some(d) => d,
            None => {
                let d = embassy_time_driver::now().wrapping_add(this.ticks);
                this.deadline = Some(d);
                d
            }
        };
        if embassy_time_driver::now() >= deadline {
            Poll::Ready(())
        } else {
            embassy_time_driver::schedule_wake(deadline, cx.waker());
            Poll::Pending
        }
    }
}
