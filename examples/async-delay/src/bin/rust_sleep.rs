//! Cold-sleep wake-on-GPIO demo.
//!
//! Demonstrates the [`cxd56_hal::sleep`] API: read why the chip booted, arm a GPIO
//! as a cold-sleep wake source, then power the chip down with
//! [`sleep::cold_sleep`]. Cold sleep loses normal RAM and resumes by a **full cold
//! boot**, so each wake re-runs `main` from the top — the program therefore loops
//! forever: *boot → report cause → arm wake → sleep → (pin event) → boot → …*.
//!
//! # Wiring
//!
//! Console on **UART1** at 115 200 baud (the CP2102N USB serial). The wake pin is
//! **D27 (`gp_uart2_cts`) on JP1**, configured as a pulled-up input that wakes on a
//! falling edge — momentarily short **D27 to GND** to wake the board from cold
//! sleep. (CXD5602 GPIO is 1.8 V — never wire it to 3.3/5 V.)
//!
//! # Expected output
//!
//! First power-on:
//! ```text
//! cold-sleep demo
//! boot cause: 0x00000000 (power-on reset)
//! wake source armed: D27 (EXDEVICE_6), boot mask = 0x00400000
//! entering cold sleep — short D27 to GND to wake
//! ```
//! After shorting D27 to GND, the board cold-boots and prints:
//! ```text
//! cold-sleep demo
//! boot cause: 0x00400000 (woke from GPIO)
//! ...
//! ```

#![no_std]
#![no_main]

use core::fmt::Write;

use cortex_m_rt::entry;
use panic_halt as _;

use cxd56_hal::clocks::{Config, RccExt};
use cxd56_hal::gpio::{Trigger, pins::Parts};
use cxd56_hal::pac;
use cxd56_hal::sleep::{self, boot};
use cxd56_hal::uart::{Uart, Uart1Pins};

#[entry]
fn main() -> ! {
    let dp = pac::Peripherals::take().unwrap();

    let clock = dp.crg.constrain(Config::default()).into_hp_clock().expect("lock Hp");

    let parts = Parts::new(dp.topreg);
    let uart1_pins = Uart1Pins {
        tx: parts.gp_spi0_cs_x,
        rx: parts.gp_spi0_sck,
    };
    let mut uart =
        Uart::new(dp.uart1, uart1_pins, Default::default(), &clock).expect("uart1 init failed");

    // Read the wake reason BEFORE doing anything that might overwrite it. A value
    // of 0 (`POR_NORMAL`) is a fresh power-on; the GPIO bit means we woke from cold
    // sleep on the armed pin.
    let cause = sleep::boot_cause();
    let _ = writeln!(uart, "cold-sleep demo");
    let _ = writeln!(uart, "boot cause: {:#010x} ({})", cause, describe_cause(cause));

    // Arm D27 (gp_uart2_cts, APP domain → first free APP slot = EXDEVICE_6) as a
    // pulled-up falling-edge interrupt. `into_interrupt` programs the PMU wake-trigger
    // detector — exactly what cold sleep monitors — so we only need to additionally
    // enable this pin's bit in the boot mask.
    let irq_in = parts
        .gp_uart2_cts
        .into_pull_up_input()
        .into_interrupt(Trigger::FallingEdge, false)
        .expect("no free EXDEVICE slot");

    let slot = irq_in.slot();
    let mask = sleep::set_boot_mask(boot::gpio_mask(slot));
    let _ = writeln!(
        uart,
        "wake source armed: D27 ({:?}), boot mask = {:#010x}",
        irq_in.interrupt(),
        mask
    );

    let _ = writeln!(uart, "entering cold sleep — short D27 to GND to wake");

    // Let the UART FIFO drain before the chip powers off mid-byte. The Uart writer
    // returns once bytes are queued, not transmitted, so spin out the in-flight TX.
    cortex_m::asm::delay(20_000_000);

    // Never returns: the firmware powers the chip down here. A pin event triggers a
    // cold boot, which re-enters `main` from the top.
    sleep::cold_sleep();
}

/// Human-readable summary of a boot-cause word for the demo output.
fn describe_cause(cause: u32) -> &'static str {
    if cause == boot::POR_NORMAL {
        "power-on reset"
    } else if cause & boot::COLD_GPIO != 0 {
        "woke from GPIO"
    } else if cause & boot::DEEP_RTC != 0 || cause & boot::COLD_RTC_ALM0 != 0 {
        "woke from RTC"
    } else if cause & boot::DEEP_WKUPS != 0 {
        "woke from PMIC button"
    } else if cause & boot::WDT_RESET != 0 || cause & boot::WDT_REBOOT != 0 {
        "watchdog"
    } else {
        "other"
    }
}
