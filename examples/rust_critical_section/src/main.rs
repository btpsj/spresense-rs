//! Cross-core `critical_section` contention test.
//!
//! Validates that the SPH-based `critical_section` implementation in
//! `cxd56-hal` provides real mutual exclusion across two APP cores.
//!
//! # Test kernel
//!
//! Both `Core0` and `Core1` each increment a shared `COUNTER` **N** times
//! using a deliberate non-atomic read-modify-write:
//!
//! ```no_run
//! critical_section::with(|_| {
//!     let v = COUNTER.load(Relaxed);
//!     COUNTER.store(v + 1, Relaxed);
//! });
//! ```
//!
//! The separate `load` + `store` (not `fetch_add`) means that without mutual
//! exclusion a lost update occurs whenever both cores read the same value
//! concurrently.  If the lock holds, the final count equals exactly `2 * N`.
//!
//! # Reporting
//!
//! - UART1 (115200 8N1) prints the result.
//! - LED0 (`gp_i2s1_bck`) lights on PASS; LED1 (`gp_i2s1_lrck`) lights on
//!   FAIL — readable without a serial console.

#![no_std]
#![no_main]

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use cortex_m::{Peripherals, asm};
use cortex_m_rt::entry;
use embedded_hal::delay::DelayNs;
use panic_halt as _;

use cxd56_hal::{delay::Delay, gpio::{Level, pins}};
use cxd56_hal::multicore::{Core, ack_boot, spawn};
use cxd56_hal::pac;
use cxd56_hal::uart::Uart;
use cxd56_hal::{
    clocks::{Config, RccExt},
    uart::Uart1Pins,
};

/// Number of increments each core performs. 100 000 per core → 200 000 total.
const N: u32 = 100_000;

// ---------------------------------------------------------------------------
// Shared state (visible to both cores via ADDRCONV-replicated SRAM).
// ---------------------------------------------------------------------------

/// The shared counter incremented by both cores inside `critical_section`.
static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Set by each core when its N iterations are complete.
static CORE0_DONE: AtomicBool = AtomicBool::new(false);
static CORE1_DONE: AtomicBool = AtomicBool::new(false);

/// Worker stack — must live in shared RAM so Core1 can use it.
/// `align(32)` matches the RP2040 convention and leaves room for a future
/// MPU stack-guard region (minimum granularity 32 bytes on Cortex-M4).
#[repr(C, align(32))]
struct Stack<const N: usize>([usize; N]);
static mut CORE1_STACK: Stack<2048> = Stack([0; 2048]); // 2048 words = 8 KiB

// ---------------------------------------------------------------------------
// Core0 entry
// ---------------------------------------------------------------------------

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();
    let core = Peripherals::take().unwrap();

    let crg = pac.crg.constrain(Config::default());
    let clocks = crg.into_clock();

    let mut delay = Delay::new(core.SYST, &clocks);

    let pins = pins::Parts::new(pac.topreg);
    let mut led0 = pins.gp_i2s1_bck.into_output(Level::Low);
    let mut led1 = pins.gp_i2s1_lrck.into_output(Level::Low);

    let uart1_pins = Uart1Pins {
        tx: pins.gp_spi0_cs_x,
        rx: pins.gp_spi0_sck,
    };
    let mut uart = Uart::new(pac.uart1, uart1_pins, Default::default(), &clocks).unwrap();

    let _ = writeln!(uart, "critical_section contention test: N={N} per core");

    // Spawn Core1.  It will `ack_boot()` and then race against us.
    let stack_top = (core::ptr::addr_of_mut!(CORE1_STACK) as usize
        + core::mem::size_of::<Stack<2048>>()) as u32;
    // SAFETY: `stack_top` is the top of a uniquely-owned, 8-byte-aligned stack
    // in shared RAM; `core1_main` is a valid `extern "C"` entry that never
    // returns; called once from the main core only.
    unsafe { spawn(Core::Core1, stack_top, core1_main).unwrap() };

    // Core0 runs its N iterations concurrently with Core1.
    run_iterations();
    CORE0_DONE.store(true, Ordering::Release);

    // Wait for Core1 to finish.
    while !CORE1_DONE.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }

    let total = COUNTER.load(Ordering::Relaxed);
    let expected = 2 * N;

    if total == expected {
        let _ = writeln!(uart, "PASS: counter={total} == 2*N={expected}");
        // Brief blink then hold LED0 on to signal pass.
        for _ in 0..3 {
            led0.set_high();
            delay.delay_ms(200);
            led0.set_low();
            delay.delay_ms(200);
        }
        led0.set_high();
    } else {
        let _ = writeln!(
            uart,
            "FAIL: counter={total} != 2*N={expected} (lost {} updates)",
            expected.saturating_sub(total)
        );
        led1.set_high();
    }

    loop {
        core::hint::spin_loop();
    }
}

// ---------------------------------------------------------------------------
// Core1 entry
// ---------------------------------------------------------------------------

extern "C" fn core1_main() -> ! {
    ack_boot();
    enable_fpu();

    run_iterations();
    CORE1_DONE.store(true, Ordering::Release);

    loop {
        core::hint::spin_loop();
    }
}

// ---------------------------------------------------------------------------
// Shared kernel
// ---------------------------------------------------------------------------

/// Increment `COUNTER` N times via a non-atomic RMW inside `critical_section`.
///
/// The load + store are intentionally separate (not `fetch_add`) so that the
/// lock — not the atomic — is what prevents lost updates.
fn run_iterations() {
    for _ in 0..N {
        critical_section::with(|_| {
            let v = COUNTER.load(Ordering::Relaxed);
            COUNTER.store(v + 1, Ordering::Relaxed);
        });
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Enable the FPU (CP10/CP11) on this core.  Required on worker cores because
/// cortex-m-rt only enables it during Core0's reset handler.
#[inline]
fn enable_fpu() {
    const CPACR: *mut u32 = 0xE000_ED88 as *mut u32;
    unsafe {
        let v = core::ptr::read_volatile(CPACR) | (0b1111 << 20);
        core::ptr::write_volatile(CPACR, v);
    }
    asm::dsb();
    asm::isb();
}
