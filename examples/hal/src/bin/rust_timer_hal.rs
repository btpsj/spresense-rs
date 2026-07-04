//! SP804 dual-timer demo — both channels at once.
//!
//! * **TIMER0**: 1 Hz periodic with the interrupt enabled at all three layers
//!   (peripheral mask, chip-level INTC gate, NVIC line). The
//!   `#[interrupt] fn TIMER0` handler clears the level-latched flag via
//!   `timer::clear_pending` (a `Timer` borrowing a stack-local `Clock` is not
//!   `'static`, so the ISR cannot own it) and bumps an atomic tick counter.
//! * **TIMER1**: free-running at /256 as an uptime reference, read with
//!   `elapsed()` from the main loop.
//!
//! The main loop sleeps in `wfi` and prints one line per TIMER0 tick. The
//! printed uptime deltas validate the reload math end-to-end: consecutive
//! lines should be ~1_000_000 µs apart.
//!
//! # Expected output (115 200 baud on UART1)
//!
//! ```text
//! sp804 demo: 1 Hz periodic IRQ on TIMER0, free-running uptime on TIMER1
//! tick 1 at 1000123 us
//! tick 2 at 2000165 us
//! ...
//! ```

#![no_std]
#![no_main]

use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering};

use cortex_m::peripheral::NVIC;
use cortex_m_rt::entry;
use fugit::ExtU32; // brings `.micros()` into scope
use panic_halt as _;

use cxd56_hal::clocks::{Config, RccExt};
use cxd56_hal::gpio::pins::Parts;
use cxd56_hal::interrupt as intc;
use cxd56_hal::pac::{self, interrupt};
use cxd56_hal::timer::{self, Prescaler, Timer};
use cxd56_hal::uart::{Uart, Uart1Pins};

/// Ticks counted by the TIMER0 ISR, read by `main`.
static TICKS: AtomicU32 = AtomicU32::new(0);

#[interrupt]
fn TIMER0() {
    // Level-latched interrupt: clear it first or the handler re-enters forever.
    timer::clear_pending::<pac::Timer0>();
    TICKS.fetch_add(1, Ordering::Relaxed);
}

#[entry]
fn main() -> ! {
    let dp = pac::Peripherals::take().unwrap();

    // Profile-aware clock (owns the CRG). The timers count at the
    // perf-dependent CPU base clock, so constructing them borrows `clock`,
    // keeping the operating point locked (`into_hp`/`into_lp` need
    // ownership) for their lifetime.
    let clock = dp.crg.constrain(Config::default()).into_hp_clock().expect("lock Hp");

    // UART1 console
    let parts = Parts::new(dp.topreg);
    let uart1_pins = Uart1Pins {
        tx: parts.gp_spi0_cs_x,
        rx: parts.gp_spi0_sck,
    };
    let mut uart =
        Uart::new(dp.uart1, uart1_pins, Default::default(), &clock).expect("uart1 init failed");

    // TIMER0: 1 Hz periodic. `start_periodic` leaves the interrupt masked.
    // Three layers must all opt in for the IRQ to reach the handler: the
    // peripheral mask, the chip-level INTC gate (which sits in front of the
    // NVIC — without it a real peripheral interrupt never pends), then the
    // NVIC line. Order between them is safe — RIS latches independently of the
    // masks, so no tick is lost.
    let mut tick = Timer::new(dp.timer0, &clock).expect("timer0 init failed");
    tick.start_periodic(1_000_000u32.micros())
        .expect("period out of range");
    tick.enable_interrupt();
    intc::enable(tick.interrupt());
    unsafe { NVIC::unmask(tick.interrupt()) };

    // TIMER1: independent free-running uptime reference (~1.6 µs/tick at HP).
    let mut uptime = Timer::new(dp.timer1, &clock).expect("timer1 init failed");
    uptime.start_free_running(Prescaler::Div256);

    let _ = writeln!(
        uart,
        "sp804 demo: 1 Hz periodic IRQ on TIMER0, free-running uptime on TIMER1"
    );

    let mut last = 0;
    loop {
        cortex_m::asm::wfi();
        let now = TICKS.load(Ordering::Relaxed);
        if now != last {
            last = now;
            let _ = writeln!(uart, "tick {now} at {} us", uptime.elapsed().to_micros());
        }
    }
}
