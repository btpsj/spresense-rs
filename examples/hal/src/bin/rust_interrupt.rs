//! Minimal NVIC interrupt demo — software-pended `EXDEVICE_0`.
//!
//! Demonstrates the `#[interrupt]` + NVIC pattern in three parts:
//!
//! 1. **Handler** (`#[interrupt] fn EXDEVICE_0`) — increments a shared atomic.
//! 2. **Enable** (`NVIC::unmask`) — gates the line so pending actually fires it.
//! 3. **Trigger** (`NVIC::pend`) — raises the interrupt from software; no
//!    external hardware, pin, or peripheral clock needed.
//!
//! The interrupt chosen (`EXDEVICE_0`, GPIO ext-int slot 0, IRQ 20) is
//! arbitrary: the HAL never configures GPIO interrupts, so this slot is
//! guaranteed idle and only fires when we explicitly pend it.
//!
//! # Expected output (115 200 baud on UART1)
//!
//! ```text
//! interrupt demo: software-pending EXDEVICE_0
//! main 0: handler ran 1 times
//! main 1: handler ran 2 times
//! ...
//! ```
//!
//! # Why `AtomicU32` and not a `Mutex<RefCell<_>>`?
//!
//! A plain integer incremented from an ISR requires only **atomicity**
//! (`fetch_add`), not mutual exclusion. For richer shared state (e.g. a queue
//! or option) use `critical_section::with` + `Mutex<RefCell<_>>` — the repo
//! already enables `critical-section-single-core` via cortex-m so that works.

#![no_std]
#![no_main]

use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering};

use cortex_m::peripheral::NVIC;
use cortex_m_rt::entry;
use panic_halt as _;

use cxd56_hal::clocks::{Config, RccExt};
use cxd56_hal::gpio::pins::Parts;
use cxd56_hal::pac::{self, interrupt, Interrupt};
use cxd56_hal::uart::{Uart, Uart1Pins};

/// Shared counter incremented by the ISR, read by `main`.
///
/// `Relaxed` ordering is sufficient for a single-core M4 counter: there is no
/// second CPU to observe reordering, and the hardware guarantees visibility
/// once we return from the ISR before `main` resumes.
static IRQ_COUNT: AtomicU32 = AtomicU32::new(0);

/// NVIC interrupt handler for `EXDEVICE_0` (GPIO ext-int slot 0, IRQ 20).
///
/// The `#[interrupt]` macro:
///   1. Validates that `EXDEVICE_0` is a real variant of the PAC `Interrupt`
///      enum (compile-time check via the injected `interrupt::EXDEVICE_0;`).
///   2. Emits `#[export_name = "EXDEVICE_0"] pub unsafe extern "C" fn ...`
///      which overrides the weak `DefaultHandler` in the vector table.
#[interrupt]
fn EXDEVICE_0() {
    IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[entry]
fn main() -> ! {
    let dp = pac::Peripherals::take().unwrap();

    let crg = dp.crg.constrain(Config::default());
    let clock = crg.into_hp_clock().expect("lock Hp");

    // UART1 console
    let parts = Parts::new(dp.topreg);
    let uart1_pins = Uart1Pins { tx: parts.gp_spi0_cs_x, rx: parts.gp_spi0_sck };
    let mut uart = Uart::new(dp.uart1, uart1_pins, Default::default(), &clock)
        .expect("uart1 init failed");

    // Enable EXDEVICE_0 in the NVIC.  Without unmask, `pend` sets the pending
    // bit but the NVIC never dispatches the handler.  cortex-m-rt leaves
    // PRIMASK clear at reset, so global interrupts are already enabled.
    unsafe { NVIC::unmask(Interrupt::EXDEVICE_0) };

    let _ = writeln!(uart, "interrupt demo: software-pending EXDEVICE_0");

    let mut n: u32 = 0;
    loop {
        // Raise the interrupt.  Because EXDEVICE_0 has lower priority than
        // the default (no priority configured = 0 = highest non-NMI), it fires
        // before `main` reaches the next instruction.
        NVIC::pend(Interrupt::EXDEVICE_0);

        // Brief busy-wait so the output is readable (~50 ms at 156 MHz).
        cortex_m::asm::delay(8_000_000);

        let count = IRQ_COUNT.load(Ordering::Relaxed);
        let _ = writeln!(uart, "main {n}: handler ran {count} times");
        n = n.wrapping_add(1);
    }
}
