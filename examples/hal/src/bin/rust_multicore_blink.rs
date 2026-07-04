//! Two-core independent LED blink — minimal multicore validation.
//!
//! `Core0` blinks LED0 (`gp_i2s1_bck`) at ~500 ms; it spawns `Core1`, which
//! blinks LED1 (`gp_i2s1_lrck`) at ~200 ms. Both are Spresense main-board LEDs
//! on **distinct** TOPREG registers, so the cores never touch the same word —
//! no semaphore or mailbox is needed. Two visibly out-of-phase blink rates prove
//! that two cores are each running their own loop (not one core toggling both).
//! This is the hardware bring-up test for `cxd56_hal::multicore::spawn`.

#![no_std]
#![no_main]

use cortex_m::asm;
use cortex_m_rt::entry;
use panic_halt as _;

use cxd56_hal::clocks::{Config, RccExt};
use cxd56_hal::gpio::{pins, GpioPin, Level};
use cxd56_hal::multicore::{ack_boot, spawn, Core};
use cxd56_hal::pac;

/// ~156 MHz APP core clock → cycles per millisecond for `asm::delay` busy-waits.
const CYCLES_PER_MS: u32 = 156_000;

/// Worker stack size in `usize` words (2048 words = 8 KiB).
/// `align(32)` matches the RP2040 convention and leaves room for a future
/// MPU stack-guard region (minimum granularity 32 bytes on Cortex-M4).
const STACK_SIZE: usize = 2048;

#[repr(C, align(32))]
struct Stack<const N: usize>([usize; N]);
static mut CORE1_STACK: Stack<STACK_SIZE> = Stack([0; STACK_SIZE]);

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();

    // Bring the clock tree up BEFORE spawning the worker.
    let crg = pac.crg.constrain(Config::default());
    let _clocks = crg.freeze();

    // Core0 owns LED0 (gp_i2s1_bck). Core1 owns LED1 (gp_i2s1_lrck) — a distinct
    // register, left unconfigured here.
    let pins = pins::Parts::new(pac.topreg);
    let mut led0 = pins.gp_i2s1_bck.into_output(Level::Low);

    // Start Core1 on its dedicated stack. `core1_main` must call `ack_boot`
    // first and never return (see below).
    let stack_top = (core::ptr::addr_of_mut!(CORE1_STACK) as usize
        + core::mem::size_of::<Stack<STACK_SIZE>>()) as u32;
    // SAFETY: `stack_top` is the top of a uniquely-owned, 32-byte-aligned stack in
    // shared RAM; `core1_main` is a valid `extern "C"` worker entry that never
    // returns; called once, from the main core only.
    unsafe {
        spawn(Core::Core1, stack_top, core1_main).unwrap();
    }

    // Core0 blink loop — 500 ms period.
    loop {
        led0.set_high();
        asm::delay(500 * CYCLES_PER_MS);
        led0.set_low();
        asm::delay(500 * CYCLES_PER_MS);
    }
}

/// `Core1` entry point.
///
/// This runs as a raw `extern "C"` function with **no** cortex-m-rt runtime:
/// only `Core0` got the reset handler that zeroes `.bss` / copies `.data` /
/// enables the FPU. We rely on `Core0` having initialised the shared `.bss` and
/// `.data` (same image), and enable this core's own FPU defensively. No
/// interrupts are used, so the worker needs no vector table.
extern "C" fn core1_main() -> ! {
    // Release the boot mailbox so the spawner can proceed (boot handshake).
    ack_boot();
    enable_fpu();

    // Core1 owns LED1 (gp_i2s1_lrck) — re-derive its register directly, since the
    // `Topreg` singleton was consumed on Core0. This is the only handle to this
    // register, so the `GpioPin::new` exclusivity contract holds.
    let block: &'static _ = unsafe { &*pac::Topreg::PTR };
    // SAFETY: exclusive access — no other code touches gp_i2s1_lrck.
    let mut led1 = unsafe { GpioPin::new(block.gp_i2s1_lrck()) }.into_output(Level::Low);

    // 200 ms period — deliberately different from Core0 so the two LEDs are
    // visibly out of phase, proving Core1 runs independently.
    loop {
        led1.set_high();
        asm::delay(200 * CYCLES_PER_MS);
        led1.set_low();
        asm::delay(200 * CYCLES_PER_MS);
    }
}

/// Grant the FPU (CP10/CP11) full access via CPACR. CPACR (`0xE000_ED88`) is
/// core-private, so this enables the FPU on this worker only. Harmless if the
/// compiler emits no floating-point instructions.
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
