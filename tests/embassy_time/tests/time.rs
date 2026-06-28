//! On-hardware test for the `cxd56-hal` embassy time driver.
//!
//! `harness = false` integration test (run with `cargo test --release --test time`).
//! It validates the driver against an **independent oracle** — the always-on 32.768
//! kHz RTC counter read directly (never via embassy-time), so it is valid ground
//! truth even when the SP804 is the embassy backing:
//!
//!   * `now_is_monotonic`    — `Instant::now()` never goes backwards (exercises the
//!     SP804 overflow/wrap-fold under `time-timer`) and does advance.
//!   * `elapsed_matches_oracle` — a 100 ms `Timer` elapses ~100 ms by the RTC oracle.
//!   * `concurrent_ordered`  — three out-of-order timers awaited together fire in
//!     deadline order, each at ~its delay, with total wall time ≈ the longest (not
//!     the sum) — i.e. multiple timers in flight at once (impossible with the
//!     single-in-flight `async_delay`), ordered by the software queue.
//!
//! Backend feature-selected like the HAL's `time-driver-*`: default `time-rtc`
//! (`tick-hz-32_768`) or `time-timer` (SP804, `tick-hz-1_000_000`,
//! `--no-default-features --features time-timer`); add `--features low-power` to run
//! at the LP operating point — every test must pass identically at HP and LP.

#![no_std]
#![no_main]

use cortex_m_rt as _; // reset handler that calls the defmt-test-generated `main`
use defmt_serial as _;
use panic_probe as _;
use static_cell::StaticCell;

use cxd56_hal::pac::{self, interrupt};
use cxd56_hal::time;
use cxd56_hal::uart::Uart;

// `defmt-test` owns the module body, so keep the logger cell at crate root and reach
// it via `crate::SERIAL`.
static SERIAL: StaticCell<Uart<'static, pac::Uart1>> = StaticCell::new();

// Forward the active backing's timer interrupt(s) to the driver. RTC: one alarm IRQ.
// SP804: TIMER0 counts the free-running overflow, TIMER1 is the one-shot alarm.
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

/// Independent ground-truth clock: the always-on 32.768 kHz RTC counter, read
/// directly (never via embassy-time), so it is a valid oracle even when the SP804 is
/// the embassy backing.
fn oracle_ticks() -> u64 {
    let rtc = unsafe { &*pac::Rtc0::PTR };
    loop {
        let hi = rtc.rtpostcnt().read().bits();
        let lo = rtc.rtprecnt().read().bits() & 0x7fff;
        if hi == rtc.rtpostcnt().read().bits() {
            return ((hi as u64) << 15) | lo as u64;
        }
    }
}

/// Oracle ticks → microseconds (the RTC counts at 32.768 kHz).
fn oracle_us(ticks: u64) -> u64 {
    ticks * 1_000_000 / 32_768
}

/// Minimal in-file async runtime: a `block_on` that sleeps the core in `WFE` between
/// polls (its waker sets `SEV`, so a wake from the timer ISR makes the next `WFE`
/// fall through), plus a binary `join`. Same pattern as the `rust_gpio_wait` examples.
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

/// One concurrent one-shot timer for `concurrent_ordered`: sleep `after_ms`, then
/// record real elapsed microseconds (vs `start`) into `slot`.
async fn fire(slot: &core::cell::Cell<u64>, after_ms: u64, start: embassy_time::Instant) {
    embassy_time::Timer::after(embassy_time::Duration::from_millis(after_ms)).await;
    slot.set((embassy_time::Instant::now() - start).as_micros());
}

#[defmt_test::tests]
mod tests {
    use core::cell::Cell;

    use defmt::assert;
    use embassy_time::{Duration, Instant, Timer};

    #[cfg(feature = "low-power")]
    use cxd56_hal::clocks::Perf;
    use cxd56_hal::clocks::{Config, RccExt};
    use cxd56_hal::gpio::pins::Parts;
    use cxd56_hal::pac;
    use cxd56_hal::uart::{Uart, Uart1Pins, UartConfig};

    struct Ctx;

    #[init]
    fn init() -> Ctx {
        let pac = pac::Peripherals::take().unwrap();
        #[allow(unused_mut)]
        let mut clock = pac.crg.constrain(Config::default()).into_clock();
        #[cfg(feature = "low-power")]
        clock
            .request_perf(Perf::Lp)
            .expect("failed to enter low-power operating point");

        // Bring up the embassy time driver (samples the base clock on the SP804
        // backing, opens the interrupt path) before any Timer/Instant use.
        crate::time::init(&clock);
        let clocks = clock.freeze();

        // Install the defmt-over-UART1 logger before any test logs a frame.
        let parts = Parts::new(pac.topreg);
        let uart1_pins = Uart1Pins { tx: parts.gp_spi0_cs_x, rx: parts.gp_spi0_sck };
        let uart =
            Uart::new(pac.uart1, uart1_pins, UartConfig::default(), &clock).expect("uart1 init failed");
        defmt_serial::defmt_serial(crate::SERIAL.init(uart));

        // Confirm which backing compiled in — the name + rate come from the HAL, so
        // this validates the `time-*` feature reached `cxd56-hal`.
        defmt::info!(
            "embassy-time backing: {} ({} ticks/s), APP CPU {} Hz",
            crate::time::BACKING_NAME,
            crate::time::tick_hz(),
            clocks.appsmp.to_Hz(),
        );
        Ctx
    }

    /// `Instant::now()` must be monotonic non-decreasing — and actually advance. The
    /// tight loop exercises the SP804 overflow wrap-fold under the `time-timer` backing.
    #[test]
    fn now_is_monotonic(_cx: &mut Ctx) {
        let mut prev = Instant::now();
        for _ in 0..200_000 {
            let now = Instant::now();
            assert!(now >= prev, "Instant::now() went backwards");
            prev = now;
        }
        let start = Instant::now();
        while Instant::now() == start {}
        assert!(Instant::now() > start, "Instant::now() did not advance");
    }

    /// A `Timer::after(100 ms)` must elapse ~100 ms by the independent RTC oracle: at
    /// least the requested time (embassy rounds the duration up), and not grossly more.
    #[test]
    fn elapsed_matches_oracle(_cx: &mut Ctx) {
        let t0 = crate::oracle_ticks();
        crate::rt::block_on(Timer::after(Duration::from_millis(100)));
        let dt = crate::oracle_us(crate::oracle_ticks() - t0);
        defmt::info!("100 ms timer measured {} us by the RTC oracle", dt);
        assert!(dt >= 99_000, "timer too short: {} us", dt);
        assert!(dt <= 110_000, "timer too long: {} us", dt);
    }

    /// Three out-of-order timers awaited together must fire in deadline order, each at
    /// ~its own delay, with total wall time ≈ the longest (not the sum) — i.e. several
    /// timers are genuinely in flight at once and the queue orders them.
    #[test]
    fn concurrent_ordered(_cx: &mut Ctx) {
        let start = Instant::now();
        let a = Cell::new(0u64);
        let b = Cell::new(0u64);
        let c = Cell::new(0u64);
        crate::rt::block_on(crate::rt::join(
            crate::fire(&a, 150, start),
            crate::rt::join(crate::fire(&b, 50, start), crate::fire(&c, 250, start)),
        ));
        let total = (Instant::now() - start).as_micros();
        defmt::info!(
            "fired: a(150ms)={} b(50ms)={} c(250ms)={} us, total {} us",
            a.get(),
            b.get(),
            c.get(),
            total
        );
        // Deadline order: b (50) < a (150) < c (250).
        assert!(
            b.get() < a.get() && a.get() < c.get(),
            "timers fired out of deadline order"
        );
        // Each at least its requested delay (embassy rounds up).
        assert!(b.get() >= 49_000, "b too short: {} us", b.get());
        assert!(a.get() >= 149_000, "a too short: {} us", a.get());
        assert!(c.get() >= 249_000, "c too short: {} us", c.get());
        // Concurrency: total ≈ the longest (250 ms), not the sum (450 ms).
        assert!(total <= 300_000, "timers not concurrent: total {} us", total);
    }
}
