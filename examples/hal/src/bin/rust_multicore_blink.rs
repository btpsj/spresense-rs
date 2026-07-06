//! Two-core independent LED blink — minimal multicore validation.
//!
//! `Core0` blinks LED0 (`gp_i2s1_bck`) at ~500 ms; it spawns `Core1`, which
//! blinks LED1 (`gp_i2s1_lrck`) at ~200 ms. Both are Spresense main-board LEDs
//! on **distinct** TOPREG registers, so the cores never touch the same word —
//! no semaphore or mailbox is needed. Two visibly out-of-phase blink rates prove
//! that two cores are each running their own loop (not one core toggling both).
//! This is the hardware bring-up test for `cxd56_hal::multicore::spawn`.
//!
//! Note what is **absent** compared to the old fn-pointer spawn: no `unsafe`,
//! no `static mut` stack, no manual FPU enable, no `ack_boot` — LED1 is
//! configured on `Core0` and simply *moved* into the worker closure, and the
//! HAL's boot shim handles the rest.

#![no_std]
#![no_main]

use cortex_m::asm;
use cortex_m_rt::entry;
use panic_halt as _;
use static_cell::ConstStaticCell;

use cxd56_hal::clocks::{Config, RccExt};
use cxd56_hal::gpio::{Level, pins};
use cxd56_hal::multicore::{Cores, Stack, spawn};
use cxd56_hal::pac;

/// ~156 MHz APP core clock → cycles per millisecond for `asm::delay` busy-waits.
const CYCLES_PER_MS: u32 = 156_000;

/// Worker stack (8 KiB). `ConstStaticCell` hands out the `&'static mut` that
/// `spawn` needs without any `unsafe`.
static CORE1_STACK: ConstStaticCell<Stack<8192>> = ConstStaticCell::new(Stack::new());

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();

    // Bring the clock tree up BEFORE spawning the worker.
    let crg = pac.crg.constrain(Config::default());
    let _clocks = crg.freeze();

    // Configure both LEDs on Core0; LED1 then moves into the worker closure
    // (a configured `Output` pin is `Send` — it owns only its own register).
    let pins = pins::Parts::new(pac.topreg);
    let mut led0 = pins.gp_i2s1_bck.into_output(Level::Low);
    let mut led1 = pins.gp_i2s1_lrck.into_output(Level::Low);

    // Start Core1 on its dedicated stack. `spawn` only returns once the worker
    // has booted and released the boot mailbox.
    let cores = Cores::take().unwrap();
    spawn(cores.core1, CORE1_STACK.take(), move || {
        // 200 ms period — deliberately different from Core0 so the two LEDs
        // are visibly out of phase, proving Core1 runs independently.
        loop {
            led1.set_high();
            asm::delay(200 * CYCLES_PER_MS);
            led1.set_low();
            asm::delay(200 * CYCLES_PER_MS);
        }
    })
    .unwrap();

    // Core0 blink loop — 500 ms period.
    loop {
        led0.set_high();
        asm::delay(500 * CYCLES_PER_MS);
        led0.set_low();
        asm::delay(500 * CYCLES_PER_MS);
    }
}
