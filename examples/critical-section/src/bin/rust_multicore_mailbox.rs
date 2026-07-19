//! Async inter-core mailbox demo — `Core1` streams ticks, `Core0` awaits them.
//!
//! `Core0` spawns `Core1` with a closure that takes its own `Outbox` and sends
//! a proto-14 [`Message`] every ~100 ms (blocking `try_send` + busy-wait —
//! workers need no executor). `Core0` arms its `Inbox` (`FIFO_FROM` interrupt)
//! and drives two futures **concurrently** with the same tiny in-file
//! `block_on`/`join` as `rust_embassy_time`:
//!
//! - an async receive loop, `inbox.recv().await`, printing each tick;
//! - an `embassy_time::Timer` ticker on the HAL's RTC time driver.
//!
//! The interleaved output proves the mailbox futures and embassy-time futures
//! coexist on one executor-less core, woken by two different interrupts
//! (`FIFO_FROM` and `RTC0_A0`).
//!
//! After the demo stops receiving, `Core1` keeps sending until the 8-deep
//! inbox ring and then the hardware FIFO fill, at which point its `try_send`
//! spins on `Full` forever — that quiet stop *is* the end-to-end back-pressure
//! working (nothing is dropped, the sender is simply held off).
//!
//! Uses `critical-section-impl` (the SPH cross-core lock), so `cortex-m` must
//! not enable `critical-section-single-core`.
//!
//! # Expected output (115 200 baud on UART1)
//!
//! ```text
//! multicore mailbox demo — Core1 ticks every ~100 ms, embassy ticker every 200 ms
//! mailbox tick #1 from Core1 (data=100) at ~100 ms
//! timer tick 1 at ~200 ms
//! mailbox tick #2 from Core1 (data=200) at ~200 ms
//! ... (8 mailbox ticks and 5 timer ticks, interleaved)
//! demo complete: 8 mailbox ticks + 5 timer ticks
//! ```
//!
//! CXD5602 GPIO is 1.8 V — never wire its pins to 3.3/5 V.

#![no_std]
#![no_main]

use core::cell::RefCell;
use core::fmt::Write;

use cortex_m::asm;
use cortex_m_rt::entry;
use panic_halt as _;
use static_cell::ConstStaticCell;

use embassy_time::{Duration, Instant, Timer};

use cxd56_hal::clocks::{Config, RccExt};
use cxd56_hal::gpio::pins::Parts;
use cxd56_hal::multicore::{self, Core, Cores, Message, Outbox, Stack, spawn};
use cxd56_hal::pac::{self, interrupt};
use cxd56_hal::time;
use cxd56_hal::uart::{Uart, Uart1Pins};

// The application owns the vectors and forwards them (a library rlib cannot):
// FIFO_FROM feeds Core0's async Inbox, RTC0_A0 feeds the embassy time driver.
#[interrupt]
fn FIFO_FROM() {
    multicore::on_rx_interrupt();
}
#[interrupt]
fn RTC0_A0() {
    time::on_interrupt();
}

/// ~156 MHz APP clock at the locked HP operating point → cycles per
/// millisecond for the worker's busy-wait.
const CYCLES_PER_MS: u32 = 156_000;
/// Worker send period in milliseconds.
const TICK_MS: u32 = 100;
/// Mailbox ticks Core0 awaits before finishing the demo.
const MAILBOX_TICKS: u16 = 8;
/// Embassy timer ticks in the concurrent task.
const TIMER_TICKS: u32 = 5;

/// Worker stack (8 KiB); `ConstStaticCell` yields the `&'static mut` safely.
static CORE1_STACK: ConstStaticCell<Stack<8192>> = ConstStaticCell::new(Stack::new());

#[entry]
fn main() -> ! {
    let dp = pac::Peripherals::take().unwrap();

    let clock = dp.crg.constrain(Config::default()).into_hp_clock().expect("lock Hp");

    let parts = Parts::new(dp.topreg);
    let uart1_pins = Uart1Pins {
        tx: parts.gp_spi0_cs_x,
        rx: parts.gp_spi0_sck,
    };
    let uart =
        Uart::new(dp.uart1, uart1_pins, Default::default(), &clock).expect("uart1 init failed");
    // Both concurrent tasks print; a RefCell arbitrates the short exclusive
    // borrows (single-threaded executor — never actually contended).
    let uart = RefCell::new(uart);

    // Bring up the embassy time driver (RTC backing) and this core's Inbox
    // BEFORE spawning the sender, so no early tick can go unserviced.
    time::init(&clock);
    let mut inbox = multicore::Inbox::take().expect("core0 inbox");

    let _ = writeln!(
        uart.borrow_mut(),
        "multicore mailbox demo — Core1 ticks every ~{TICK_MS} ms, embassy ticker every 200 ms",
    );

    // Core1: take its own Outbox and stream ticks addressed to Core0.
    let cores = Cores::take().unwrap();
    spawn(cores.core1, CORE1_STACK.take(), move || {
        let mut outbox = Outbox::take().expect("core1 outbox");
        let mut seq: u16 = 0;
        loop {
            seq = seq.wrapping_add(1);
            let msg = Message {
                peer: Core::Core0,
                msgid: 0,
                pdata: seq,
                data: seq as u32 * TICK_MS, // nominal elapsed ms
            };
            // Blocking send: spins only when Core0's ring + HW FIFO are full,
            // i.e. the demo is over and back-pressure has reached us.
            while outbox.try_send(msg).is_err() {
                core::hint::spin_loop();
            }
            asm::delay(TICK_MS * CYCLES_PER_MS);
        }
    })
    .unwrap();

    let start = Instant::now();

    rt::block_on(rt::join(
        // Task A: await mailbox ticks from Core1.
        async {
            for _ in 0..MAILBOX_TICKS {
                let msg = inbox.recv().await;
                let _ = writeln!(
                    uart.borrow_mut(),
                    "mailbox tick #{} from {:?} (data={}) at ~{} ms",
                    msg.pdata,
                    msg.peer,
                    msg.data,
                    (Instant::now() - start).as_millis(),
                );
            }
        },
        // Task B: an embassy-time ticker running concurrently on the same core.
        async {
            for n in 1..=TIMER_TICKS {
                Timer::after(Duration::from_millis(200)).await;
                let _ = writeln!(
                    uart.borrow_mut(),
                    "timer tick {n} at ~{} ms",
                    (Instant::now() - start).as_millis(),
                );
            }
        },
    ));

    let _ = writeln!(
        uart.borrow_mut(),
        "demo complete: {MAILBOX_TICKS} mailbox ticks + {TIMER_TICKS} timer ticks",
    );

    loop {
        asm::wfi();
    }
}

/// Minimal in-file async runtime — copied from `rust_embassy_time`: a
/// `block_on` that sleeps in `WFE` between polls (its waker is `SEV`, and any
/// interrupt also wakes `WFE`), plus a binary `join`. No executor crate.
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
