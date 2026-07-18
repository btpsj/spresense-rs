//! SP804 `TIMER0`/`TIMER1` embassy time driver.
//!
//! The SP804 is a relative **down-counter** with no native monotonic `now()` and no
//! compare register, so a time driver needs the overflow-counter machinery other
//! Rust HALs use for narrow/down-counter timers:
//!
//! * **`TIMER0`** free-runs (counts `u32::MAX → 0 → wrap`) as the monotonic base; a
//!   software [`PERIOD`] counter, incremented on each wrap IRQ, extends it to 64-bit.
//! * **`TIMER1`** is the one-shot **alarm** (loaded with the ticks until the queue's
//!   next deadline). Two halves are required because the SP804 has no compare
//!   register — a continuous `now()` and an arbitrary alarm can't share one channel.
//!
//! The SP804 counts the **perf-dependent** CPU base clock ([`BASE_HZ`], sampled at
//! [`init`]), which lands on no clean embassy `tick-hz`, so `now()`/alarm values are
//! software-rescaled to `tick-hz-1_000_000` (~1 µs). The operating point must not
//! change while the driver is live (the rate is fixed at init) — the same constraint
//! [`crate::timer::Timer`] and the legacy `async-delay-timer` backing enforce.

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};
use core::task::Waker;

use cortex_m::peripheral::NVIC;
use critical_section::{CriticalSection, Mutex};
use embassy_time_driver::{Driver, TICK_HZ};
use embassy_time_queue_utils::Queue;

use crate::clocks::{Clock, PerfState};
use crate::pac;

// The SP804 rate is rescaled to this fixed embassy tick rate (microseconds).
const _: () = assert!(
    TICK_HZ == 1_000_000,
    "time-driver-timer requires the app's embassy-time tick rate to be tick-hz-1_000_000"
);

/// `TIMER0` wrap count — the high 32 bits of the hardware monotonic count. There is
/// no `AtomicU64` on this core, so [`hw_count`] composes the 64-bit value from this
/// plus the live counter under a critical section.
static PERIOD: AtomicU32 = AtomicU32::new(0);
/// SP804 input (CPU base) clock in Hz, sampled at [`init`]. Perf-dependent; `0` until
/// init (so `now()` returns 0 rather than dividing by zero, per the `Driver` contract).
static BASE_HZ: AtomicU32 = AtomicU32::new(0);

struct Sp804Driver {
    queue: Mutex<RefCell<Queue>>,
}

embassy_time_driver::time_driver_impl!(static DRIVER: Sp804Driver = Sp804Driver {
    queue: Mutex::new(RefCell::new(Queue::new())),
});

#[inline]
fn t0() -> &'static pac::timer0::RegisterBlock {
    // SAFETY: the `TIMER0` half of the SP804; single-register accesses only.
    unsafe { &*pac::Timer0::PTR }
}

#[inline]
fn t1() -> &'static pac::timer0::RegisterBlock {
    // SAFETY: the `TIMER1` half of the SP804 (identical register layout).
    unsafe { &*pac::Timer1::PTR }
}

/// Service a latched `TIMER0` wrap: if the raw interrupt is set, count the period and
/// clear it. **Must be called inside a critical section** so the RIS→`PERIOD`
/// transition is atomic w.r.t. concurrent readers and the overflow ISR — that makes
/// the wrap accounting correct regardless of interrupt priority. Idempotent (a wrap
/// already serviced by [`hw_count`] makes the ISR a no-op, and vice-versa).
fn update_period(_cs: CriticalSection) {
    if t0().ris().read().timer_interrupt().bit_is_set() {
        PERIOD.store(PERIOD.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        // SAFETY: write-1-to-clear the timer interrupt latch.
        t0().intclr().write(|w| unsafe { w.bits(1) });
    }
}

/// Hardware monotonic count = `(PERIOD << 32) | elapsed-in-period`. `TIMER0` counts
/// DOWN from `u32::MAX`, so elapsed = `u32::MAX - value`. Servicing the pending wrap
/// first (in the CS) keeps it from ever reading backward across a wrap.
fn hw_count() -> u64 {
    critical_section::with(|cs| {
        update_period(cs);
        let period = PERIOD.load(Ordering::Relaxed) as u64;
        let value = t0().value().read().bits();
        (period << 32) | (u32::MAX - value) as u64
    })
}

/// Hardware count rescaled to embassy ticks (`tick-hz-1_000_000`). Returns 0 before
/// [`init`] samples the base clock (the `Driver` contract forbids faulting).
fn now_ticks() -> u64 {
    let base = BASE_HZ.load(Ordering::Relaxed);
    if base == 0 {
        return 0;
    }
    ((hw_count() as u128 * TICK_HZ as u128) / base as u128) as u64
}

impl Sp804Driver {
    /// Arm `TIMER1` one-shot for absolute tick `at`, or disable it when the queue is
    /// empty (`u64::MAX`). Deadlines beyond the 32-bit range (~44 s at HP / ~137 s at
    /// LP) load the max and are re-armed in chunks by [`on_alarm_interrupt`].
    fn set_alarm(&self, _cs: CriticalSection, at: u64) {
        let t = t1();
        if at == u64::MAX {
            // SAFETY: stop the alarm timer and clear its latch.
            t.control().write(|w| unsafe { w.bits(0) });
            t.intclr().write(|w| unsafe { w.bits(1) });
            return;
        }
        let base = BASE_HZ.load(Ordering::Relaxed) as u128;
        let delta_ticks = at.saturating_sub(now_ticks()) as u128;
        let delta_hw = (delta_ticks * base / TICK_HZ as u128).min(u32::MAX as u128) as u32;
        // A zero LOAD would latch immediately and (with the deadline already due) the
        // queue drains on the next IRQ anyway, so clamp up to 1.
        let load = delta_hw.max(1);
        // SAFETY (all writes): `TIMER1` one-shot, 32-bit, interrupt-enabled — the
        // documented SP804 control encodings; INTCLR/LOAD accept any value.
        t.control().write(|w| unsafe { w.bits(0) });
        t.intclr().write(|w| unsafe { w.bits(1) });
        t.load().write(|w| unsafe { w.bits(load) });
        t.control().write(|w| {
            w.size().set_bit(); // 32-bit
            w.mode().set_bit(); // one-shot: halt at zero
            w.intenable().set_bit();
            w.enable().set_bit()
        });
    }
}

impl Driver for Sp804Driver {
    fn now(&self) -> u64 {
        now_ticks()
    }

    fn schedule_wake(&self, at: u64, waker: &Waker) {
        critical_section::with(|cs| {
            let mut queue = self.queue.borrow(cs).borrow_mut();
            if queue.schedule_wake(at, waker) {
                let next = queue.next_expiration(now_ticks());
                self.set_alarm(cs, next);
            }
        });
    }
}

/// Sample the (perf-dependent) base clock, start `TIMER0` free-running as the
/// monotonic base, leave `TIMER1` idle, and open both interrupt paths. Call once via
/// [`super::init`], after the operating point is set.
pub(super) fn init<P: PerfState>(clock: &Clock<P>) {
    BASE_HZ.store(clock.cpu_baseclk().to_Hz(), Ordering::Relaxed);
    PERIOD.store(0, Ordering::Relaxed);

    // TIMER0: free-running 32-bit (no mode/periodic), interrupt on each wrap.
    let t = t0();
    // SAFETY: documented SP804 control/load encodings; INTCLR accepts any value.
    t.control().write(|w| unsafe { w.bits(0) });
    t.intclr().write(|w| unsafe { w.bits(1) });
    t.load().write(|w| unsafe { w.bits(u32::MAX) });
    t.control().write(|w| {
        w.size().set_bit(); // 32-bit
        w.intenable().set_bit(); // IRQ on each zero-crossing (wrap)
        w.enable().set_bit()
    });

    // TIMER1: alarm — left disabled until the first schedule.
    // SAFETY: stop the alarm timer and clear its latch.
    t1().control().write(|w| unsafe { w.bits(0) });
    t1().intclr().write(|w| unsafe { w.bits(1) });

    crate::interrupt::enable(pac::Interrupt::TIMER0);
    crate::interrupt::enable(pac::Interrupt::TIMER1);
    // SAFETY: this HAL masks with critical-section (PRIMASK), not BASEPRI.
    unsafe {
        NVIC::unmask(pac::Interrupt::TIMER0);
        NVIC::unmask(pac::Interrupt::TIMER1);
    }
}

/// `TIMER0` (overflow) interrupt entry point — the app forwards `TIMER0` here. Counts
/// the wrap so `now()` keeps advancing even when it is not polled for a long time.
pub fn on_overflow_interrupt() {
    critical_section::with(update_period);
}

/// `TIMER1` (alarm) interrupt entry point — the app forwards `TIMER1` here. Wakes
/// expired timers and re-arms for the queue's next deadline.
pub fn on_alarm_interrupt() {
    critical_section::with(|cs| {
        // SAFETY: clear the alarm latch.
        t1().intclr().write(|w| unsafe { w.bits(1) });
        let mut queue = DRIVER.queue.borrow(cs).borrow_mut();
        let next = queue.next_expiration(now_ticks());
        DRIVER.set_alarm(cs, next);
    });
}
