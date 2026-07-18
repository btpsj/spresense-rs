//! embassy-time multi-timer demo on the `cxd56-hal` embassy time driver.
//!
//! The point of the embassy-time backing is **concurrency**: many `Timer`s in flight
//! at once, which the single-in-flight `async_delay` could not do. Phase 1 awaits four
//! one-shot `Timer`s together (50/100/150/250 ms) and shows the whole batch finishes
//! in ~250 ms — not the ~550 ms a sequential delay would take. Phase 2 shows repeated
//! re-arming with a periodic 200 ms tick (using `Instant`/`Duration`).
//!
//! The backend is feature-selected, mirroring `cxd56-hal`'s `time-driver-*`:
//!   * default `time-rtc`  — RTC0 alarm 0, perf-invariant, `tick-hz-32_768` (1:1).
//!   * `time-timer`        — SP804 `TIMER0`/`TIMER1`, the perf-dependent base clock
//!     software-rescaled to `tick-hz-1_000_000`. Build with
//!     `--no-default-features --features time-timer`.
//!
//! Add `--features low-power` to drop to the LP operating point first; the timings
//! below are **identical** at HP and LP under either backend (the RTC is
//! perf-invariant; the SP804 driver re-samples the base clock at `init`) — that
//! cross-operating-point match is the validation.
//!
//! A tiny in-file `block_on`/`join` (no executor crate) drives the futures, exactly
//! like the `rust_gpio_wait` examples — `embassy-time` `Timer`s are plain futures, so
//! an Embassy executor would work equally; this just keeps the example dependency-light.
//!
//! # Expected output (115 200 baud on UART1)
//!
//! ```text
//! embassy-time demo — APP CPU = 97500000 Hz, backing: RTC0 alarm 0 (32768 ticks/s)
//! phase 1: 4 one-shot timers awaited concurrently
//!   #0 requested 150000 us -> fired at ~150000 us
//!   #1 requested  50000 us -> fired at ~50000 us
//!   #2 requested 250000 us -> fired at ~250000 us
//!   #3 requested 100000 us -> fired at ~100000 us
//!   total wall time ~250000 us (sequential would be ~550000 us)
//! phase 2: periodic 200 ms ticks
//!   tick 1 at +200000 us
//!   ... (5 ticks)
//! embassy-time demo complete
//! ```
//!
//! CXD5602 GPIO is 1.8 V — never wire its pins to 3.3/5 V.

#![no_std]
#![no_main]

use core::cell::Cell;
use core::fmt::Write;

use cortex_m_rt::entry;
use panic_halt as _;

use embassy_time::{Duration, Instant, Timer};

use cxd56_hal::clocks::{Config, RccExt};
use cxd56_hal::gpio::pins::Parts;
use cxd56_hal::pac::{self, interrupt};
use cxd56_hal::time;
use cxd56_hal::uart::{Uart, Uart1Pins};

// Forward the active backing's timer interrupt(s) to the driver — the library cannot
// define the vectors itself. RTC backing: the single alarm IRQ. SP804 backing: TIMER0
// counts the free-running overflow (the monotonic base), TIMER1 is the one-shot alarm.
#[cfg(feature = "time-rtc")]
#[interrupt]
fn RTC0_A0() {
    time::on_interrupt();
}
#[cfg(feature = "time-timer")]
#[interrupt]
fn TIMER0() {
    time::on_overflow_interrupt();
}
#[cfg(feature = "time-timer")]
#[interrupt]
fn TIMER1() {
    time::on_alarm_interrupt();
}

/// One concurrent one-shot timer: sleep `after_ms`, then record the real elapsed
/// microseconds (vs `start`) into `slot`. Several of these awaited together prove the
/// driver keeps multiple timers in flight at once.
async fn fire(slot: &Cell<u64>, after_ms: u64, start: Instant) {
    Timer::after(Duration::from_millis(after_ms)).await;
    slot.set((Instant::now() - start).as_micros());
}

#[entry]
fn main() -> ! {
    let dp = pac::Peripherals::take().unwrap();

    // Construction locks the operating point: HP by default, or the LP point with
    // the `low-power` feature — so the demo can be flashed at both (the timings
    // below must match across the two).
    #[cfg(not(feature = "low-power"))]
    let clock = dp.crg.constrain(Config::default()).into_hp_clock().expect("lock Hp");
    #[cfg(feature = "low-power")]
    let clock = dp.crg.constrain(Config::default()).into_lp_clock().expect("lock Lp");

    let parts = Parts::new(dp.topreg);
    let uart1_pins = Uart1Pins {
        tx: parts.gp_spi0_cs_x,
        rx: parts.gp_spi0_sck,
    };
    let mut uart =
        Uart::new(dp.uart1, uart1_pins, Default::default(), &clock).expect("uart1 init failed");

    // Bring up the embassy time driver: opens its interrupt path and (on the SP804
    // backing) samples the base clock it counts at — *after* any perf switch. Must
    // precede any `Timer`/`Instant` use.
    time::init(&clock);

    let appsmp_hz = clock.freeze().appsmp.to_Hz();
    let _ = writeln!(
        uart,
        "embassy-time demo — APP CPU = {} Hz, backing: {} ({} ticks/s)",
        appsmp_hz,
        time::BACKING_NAME,
        time::tick_hz(),
    );

    rt::block_on(async {
        // --- Phase 1: four one-shot timers awaited *concurrently* ---
        let start = Instant::now();
        let (e0, e1, e2, e3) = (Cell::new(0), Cell::new(0), Cell::new(0), Cell::new(0));
        rt::join(
            rt::join(fire(&e0, 150, start), fire(&e1, 50, start)),
            rt::join(fire(&e2, 250, start), fire(&e3, 100, start)),
        )
        .await;
        let total = (Instant::now() - start).as_micros();
        let _ = writeln!(uart, "phase 1: 4 one-shot timers awaited concurrently");
        let _ = writeln!(uart, "  #0 requested 150000 us -> fired at {} us", e0.get());
        let _ = writeln!(uart, "  #1 requested  50000 us -> fired at {} us", e1.get());
        let _ = writeln!(uart, "  #2 requested 250000 us -> fired at {} us", e2.get());
        let _ = writeln!(uart, "  #3 requested 100000 us -> fired at {} us", e3.get());
        let _ = writeln!(
            uart,
            "  total wall time {total} us (sequential would be ~550000 us)"
        );

        // --- Phase 2: periodic re-arming via `Timer::after` in a loop ---
        let _ = writeln!(uart, "phase 2: periodic 200 ms ticks");
        let mut last = Instant::now();
        for n in 1..=5u32 {
            Timer::after(Duration::from_millis(200)).await;
            let now = Instant::now();
            let _ = writeln!(uart, "  tick {n} at +{} us", (now - last).as_micros());
            last = now;
        }
    });

    let _ = writeln!(uart, "embassy-time demo complete");
    loop {
        cortex_m::asm::wfi();
    }
}

/// Minimal in-file async runtime: a `block_on` that genuinely sleeps the core in `WFE`
/// between polls (its waker sets the ARM event via `SEV`, so a wake from the timer ISR
/// makes the next `WFE` fall through), plus a binary `join`. Kept local — no executor
/// crate — exactly like the `rust_gpio_wait` examples.
mod rt {
    use core::future::{Future, poll_fn};
    use core::pin::pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake, drop_noop);
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    fn wake(_: *const ()) {
        cortex_m::asm::sev();
    }
    fn drop_noop(_: *const ()) {}

    fn make_waker() -> Waker {
        // SAFETY: the vtable functions ignore the data pointer, so the null pointer is sound.
        unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
    }

    /// Drive `fut` to completion, sleeping in `WFE` between polls.
    pub fn block_on<F: Future>(fut: F) -> F::Output {
        let mut fut = pin!(fut);
        let waker = make_waker();
        let mut cx = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(val) = fut.as_mut().poll(&mut cx) {
                return val;
            }
            cortex_m::asm::wfe();
        }
    }

    /// Poll two futures concurrently until both complete.
    pub async fn join<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
        let mut a = pin!(a);
        let mut b = pin!(b);
        let mut ao: Option<A::Output> = None;
        let mut bo: Option<B::Output> = None;
        poll_fn(|cx| {
            if ao.is_none() {
                if let Poll::Ready(v) = a.as_mut().poll(cx) {
                    ao = Some(v);
                }
            }
            if bo.is_none() {
                if let Poll::Ready(v) = b.as_mut().poll(cx) {
                    bo = Some(v);
                }
            }
            if ao.is_some() && bo.is_some() {
                Poll::Ready((ao.take().unwrap(), bo.take().unwrap()))
            } else {
                Poll::Pending
            }
        })
        .await
    }
}
