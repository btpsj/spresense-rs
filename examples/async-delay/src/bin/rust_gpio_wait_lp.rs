//! Async GPIO interrupt (EXDEVICE) demo at the **low-power** operating point.
//!
//! Identical to the `rust_gpio_wait` example but locks the APP CPU at
//! [`Perf::Lp`](cxd56_hal::clocks::Perf::Lp) (31.2 MHz, VDD_CORE 0.7 V) from
//! construction, to confirm the async EXDEVICE edge handling still works at
//! the slowest core clock.
//!
//! The GPIO edge detector is clocked by the fixed 32.768 kHz RTC, so the HAL's
//! edge-arm baseline settle is a fixed *real* time (~488 µs, independent of the
//! core clock). That settle is now an **interrupt-driven async delay**
//! ([`cxd56_hal::async_delay`]) rather than a busy-wait, so the core `WFE`-sleeps
//! it instead of spinning. Awaiting it needs the async-delay source handler below,
//! alongside the `EXDEVICE_6` one — the same one-line forwarding pattern. The
//! source is feature-selected: the default `backing-rtc` (`RTC0_A0`, perf-invariant)
//! or `backing-timer` (SP804 `TIMER0`). The TIMER backing is the *interesting*
//! low-power case: the SP804 counts the perf-dependent CPU base clock, so this demo
//! exercises that the ~488 µs settle is held correctly even though the base clock is
//! sampled at the LP point — build it with
//! `--no-default-features --features backing-timer`.
//!
//! Wire **D28 (`gp_uart2_rts`) ↔ D27 (`gp_uart2_cts`)** on JP1, same as
//! `rust_gpio_wait`. Each edge wait is `join`ed with a task that drives the edge
//! only *after* the wait has finished arming: it paces itself with
//! [`rt::delay_ticks`], a cooperative RTC wait that outlasts the wait's baseline
//! settle, so the EXDEVICE ISR is what wakes the sleeping core. (The driver's pace
//! is poll-based rather than another alarm because the single-channel async delay
//! is already in use by the concurrent settle.)
//!
//! # Expected output (115 200 baud on UART1)
//!
//! ```text
//! gpio wait demo (LOW POWER) — APP CPU = 39000000 Hz, cts interrupt = EXDEVICE_6
//! async-delay backing: RTC0 alarm 0 (IRQ RTC0_A0, 32768 Hz)
//! phase A (async): 5 rising edges (expected 5)
//! phase B: wait_for_high  -> cts is_high=true
//! phase B: wait_for_low   -> cts is_low=true
//! phase B: wait_for_rising_edge  -> ok
//! phase B: wait_for_falling_edge -> ok
//! gpio wait demo complete
//! ```
//! (The exact APP CPU figure depends on the XOSC; 31.2 MHz at the LP point.)
//!
//! CXD5602 GPIO is 1.8 V — never wire these pins to 3.3/5 V.

#![no_std]
#![no_main]

use core::fmt::Write;

use cortex_m_rt::entry;
use panic_halt as _;

use cxd56_hal::async_delay::{self, Delay};
use cxd56_hal::clocks::{Config, RccExt};
use cxd56_hal::gpio::{self, Level, Trigger, Wait, pins::Parts};
use cxd56_hal::pac::{self, Interrupt, interrupt};
use cxd56_hal::uart::{Uart, Uart1Pins};

/// Baseline hold before arming an edge wait, via the public [`Delay`] — lets the
/// driven level settle so the PMU detector's baseline is unambiguous. A wall-clock
/// duration (~244 µs) so it is the same under either async-delay backing.
const BASELINE_US: u32 = 244;
/// In-`join` pacing for the loopback driver: wait this many RTC ticks before
/// driving the edge, so the concurrent `wait_for_*_edge` has finished arming first
/// (its ~488 µs baseline settle + NVIC unmask). At 32.768 kHz, 24 ticks ≈ 732 µs,
/// so it outlasts the settle regardless of backing. Reads the RTC counter directly
/// (see [`rt::delay_ticks`]), independent of the HAL's async-delay source.
const ARM_TICKS: u32 = 24;

/// CTS (pin 69, APP domain) takes the first free APP slot — slot 6 → `EXDEVICE_6`.
/// Forward the vector to the HAL, which masks the line and wakes the waiting task.
#[interrupt]
fn EXDEVICE_6() {
    gpio::on_interrupt(Interrupt::EXDEVICE_6);
}

/// Forward the async-delay source IRQ to the HAL, which disarms the source and
/// wakes the delaying task. Backs the edge-arm settle and the baseline holds, so it
/// is required by any async `wait_for_*_edge`. The vector matches the selected
/// backing: `RTC0_A0` (default) or `TIMER0`.
#[cfg(feature = "backing-rtc")]
#[interrupt]
fn RTC0_A0() {
    async_delay::on_interrupt(Interrupt::RTC0_A0);
}
#[cfg(feature = "backing-timer")]
#[interrupt]
fn TIMER0() {
    async_delay::on_interrupt(Interrupt::TIMER0);
}

#[entry]
fn main() -> ! {
    let dp = pac::Peripherals::take().unwrap();

    // Lock the low-power operating point *before* the UART is created, so the
    // UART computes its baud divisor from the LP clock. If this RPC to the SYSIOP
    // loader fails the board halts here (before any output) — no output on an
    // otherwise-correct setup means the perf switch itself failed.
    let clock = dp
        .crg
        .constrain(Config::default())
        .into_lp_clock()
        .expect("failed to enter low-power operating point");

    // `into_floating_input()` enables the CTS pad's input buffer (ENZI) and sets
    // the pull; the RTS output pad's driver is enabled by `into_output`.
    let parts = Parts::new(dp.topreg);

    // UART1 console (baud computed from the now-LP clock).
    let uart1_pins = Uart1Pins {
        tx: parts.gp_spi0_cs_x,
        rx: parts.gp_spi0_sck,
    };
    let mut uart =
        Uart::new(dp.uart1, uart1_pins, Default::default(), &clock).expect("uart1 init failed");

    // Async delay source the edge-arm settle and the baseline holds sleep on, and
    // which `Delay::new` opens the interrupt path for + samples the clock of. The
    // backing peripheral is feature-selected: RTC0 alarm 0 (perf-invariant) or
    // SP804 TIMER0, whose count rate `Delay::new` samples from `clock` *here* —
    // after the LP switch — so the settle holds the right real time at LP.
    #[cfg(feature = "backing-rtc")]
    let mut delay = Delay::new(dp.rtc0, &clock);
    #[cfg(feature = "backing-timer")]
    let mut delay = Delay::new(dp.timer0, &clock);

    // JP1 loopback: D28 (UART2_RTS) drives the line, D27 (UART2_CTS) reads it and
    // is wired to the EXDEVICE interrupt. Short the two header pins together.
    let mut driver = parts.gp_uart2_rts.into_output(Level::Low);
    let mut irq_in = parts
        .gp_uart2_cts
        .into_floating_input()
        .into_interrupt(Trigger::RisingEdge, false)
        .expect("no free EXDEVICE slot");

    let appsmp_hz = clock.freeze().appsmp.to_Hz();
    let _ = writeln!(
        uart,
        "gpio wait demo (LOW POWER) — APP CPU = {} Hz, cts interrupt = {:?}",
        appsmp_hz,
        irq_in.interrupt()
    );
    // Confirm the async-delay backing the edge-arm settle sleeps on. The name and
    // rate come from the HAL, so this validates the `backing-*` feature reached
    // `cxd56-hal`. With `--features backing-timer` this prints `SP804 TIMER0` at
    // the *LP* CPU base clock (31.2 MHz) — proof the SP804 sampled the slow clock
    // yet still holds the right ~488 µs settle — vs `RTC0 alarm 0` at 32768 Hz.
    let _ = writeln!(
        uart,
        "async-delay backing: {} (IRQ {:?}, {} Hz)",
        async_delay::BACKING_NAME,
        async_delay::SOURCE_INTERRUPT,
        async_delay::tick_hz(),
    );

    rt::block_on(async {
        // --- Phase A: count rising edges, each driven concurrently ---
        const PULSES: u32 = 5;
        for _ in 0..PULSES {
            driver.set_low(); // hold the low baseline …
            delay.after_micros(BASELINE_US).await; // … long enough to sample it
            rt::join(irq_in.wait_for_rising_edge(), async {
                rt::delay_ticks(ARM_TICKS).await; // let the wait finish arming first
                driver.set_high(); // low→high = rising edge → EXDEVICE_6
            })
            .await;
        }
        let _ = writeln!(uart, "phase A (async): {PULSES} rising edges (expected {PULSES})");

        // --- Phase B: each Wait method once ---
        driver.set_high();
        delay.after_micros(BASELINE_US).await;
        let _ = irq_in.wait_for_high().await; // already high → returns at once
        let _ = writeln!(
            uart,
            "phase B: wait_for_high  -> cts is_high={}",
            irq_in.is_high()
        );

        driver.set_low();
        delay.after_micros(BASELINE_US).await;
        let _ = irq_in.wait_for_low().await;
        let _ = writeln!(
            uart,
            "phase B: wait_for_low   -> cts is_low={}",
            irq_in.is_low()
        );

        // Rising edge: hold low baseline, then drive high once the wait has armed.
        driver.set_low();
        delay.after_micros(BASELINE_US).await;
        rt::join(irq_in.wait_for_rising_edge(), async {
            rt::delay_ticks(ARM_TICKS).await;
            driver.set_high();
        })
        .await;
        let _ = writeln!(uart, "phase B: wait_for_rising_edge  -> ok");

        // Falling edge: hold high baseline, then drive low once the wait has armed.
        driver.set_high();
        delay.after_micros(BASELINE_US).await;
        rt::join(irq_in.wait_for_falling_edge(), async {
            rt::delay_ticks(ARM_TICKS).await;
            driver.set_low();
        })
        .await;
        let _ = writeln!(uart, "phase B: wait_for_falling_edge -> ok");
    });

    let _ = writeln!(uart, "gpio wait demo complete");
    loop {
        cortex_m::asm::wfi();
    }
}

/// Minimal in-file async runtime: a `block_on` that genuinely sleeps the core in
/// `WFE` between polls, plus `join` and a cooperative `delay_ticks`. Kept local (no
/// executor crate) — `cxd56-hal` implements the standard `embedded-hal-async`
/// traits, so an Embassy executor would work equally; this just keeps the example
/// dependency-free.
mod rt {
    use core::future::{Future, poll_fn};
    use core::pin::pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    use cxd56_hal::pac;

    // A waker that just sets the ARM event register (SEV). Combined with WFE in
    // `block_on`, a wake from any context (including the EXDEVICE / RTC ISR) makes
    // the next WFE fall through, so no wakeup is lost.
    const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake, drop_noop);
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    fn wake(_: *const ()) {
        cortex_m::asm::sev();
    }
    fn drop_noop(_: *const ()) {}

    fn make_waker() -> Waker {
        // Safety: the vtable's functions ignore the data pointer entirely, so the
        // null pointer they are paired with here is sound.
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

    /// Cooperative poll-based wait of at least `ticks` RTC ticks. Reads the
    /// always-on 32.768 kHz counter and yields (re-polling) until the deadline.
    ///
    /// Used to pace the loopback driver so it drives the edge only *after* the
    /// concurrent `wait_for_*_edge` has finished arming (its baseline settle +
    /// NVIC unmask). It is independent of the HAL's single async-delay alarm
    /// channel — which the settle is already using — so the two can run at once.
    /// Test scaffolding: it busy-polls (the executor spins), unlike the HAL settle
    /// it paces against, which truly sleeps the core on the RTC alarm.
    pub async fn delay_ticks(ticks: u32) {
        // SAFETY: read-only MMIO of the always-on RTC counter.
        let rtc = unsafe { &*pac::Rtc0::PTR };
        let now = || -> u64 {
            // Re-read the high half for a consistent (POSTCNT<<15)|PRECNT snapshot
            // across a PRECNT wrap; the 47-bit count is monotonic.
            loop {
                let hi = rtc.rtpostcnt().read().bits();
                let lo = rtc.rtprecnt().read().bits() & 0x7fff;
                if hi == rtc.rtpostcnt().read().bits() {
                    return ((hi as u64) << 15) | lo as u64;
                }
            }
        };
        let deadline = now().wrapping_add(ticks as u64);
        poll_fn(|cx| {
            if now() >= deadline {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await
    }

    /// Poll two futures concurrently until both complete.
    pub async fn join<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
        let mut a = pin!(a);
        let mut b = pin!(b);
        let mut ao: Option<A::Output> = None;
        let mut bo: Option<B::Output> = None;
        poll_fn(|cx| {
            if ao.is_none()
                && let Poll::Ready(v) = a.as_mut().poll(cx)
            {
                ao = Some(v);
            }
            if bo.is_none()
                && let Poll::Ready(v) = b.as_mut().poll(cx)
            {
                bo = Some(v);
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
