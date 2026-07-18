//! On-hardware UART peripheral test (Option 1 — plain `defmt` + sentinel).
//!
//! Runs three sub-tests, logs each result over UART1 via `defmt-serial`, and
//! ends with a `TEST RESULT: PASS`/`FAIL` line that `cargo-spresense-flash
//! --test` matches to derive a process exit code. This is the lightweight,
//! framework-free style; see `../gpio_levels` for the `defmt-test` style.
//!
//!   [1/3] console_uart1         — UART1 console + defmt-serial come up (reaching
//!                                 the host over defmt *is* the assertion).
//!   [2/3] uart2_internal_loopback — UART2 in PL011 loopback (UARTCR.LBE): write a
//!                                 byte pattern, read it back, assert equal. No wiring.
//!   [3/3] uart2_external_loopback — same over the real JP1 pads with a D01↔D00
//!                                 jumper. Gated behind `--features external-loopback`.

#![no_std]
#![no_main]

use cortex_m::asm;
use cortex_m_rt::entry;
use defmt_serial as _;
use panic_probe as _;
use static_cell::StaticCell;

use cxd56_hal::{
    clocks::{Clock, Config, Hp, RccExt},
    uart::{Uart, Uart2Pins, UartConfig},
};
use cxd56_hal::{gpio::pins::Parts, pac, uart::Uart1Pins};

static SERIAL: StaticCell<Uart<'static, pac::Uart1>> = StaticCell::new();
// UART1 now borrows the `Clock` for its lifetime (COM is a Dyn clock), so the
// `Clock` must outlive the `'static` UART stored in `SERIAL`.
static CLOCK: StaticCell<Clock<Hp>> = StaticCell::new();

/// Transmit one byte and read it straight back (loopback), asserting a match.
fn echo_byte(uart: &mut Uart<'_, pac::Uart2>, expect: u8) -> Result<(), &'static str> {
    uart.write_byte(expect);
    uart.flush().map_err(|_| "TX flush timeout")?;
    // After flush() the byte has been clocked out and looped to RX; poll the RX
    // FIFO with a bounded spin so a wiring/clock fault fails fast instead of hanging.
    for _ in 0..1_000_000 {
        if let Some(got) = uart.read_byte() {
            return if got == expect {
                Ok(())
            } else {
                Err("byte mismatch")
            };
        }
    }
    Err("RX timeout")
}

/// Drive a UART2 instance through a fixed byte pattern via `echo_byte`.
fn run_pattern(uart: &mut Uart<'_, pac::Uart2>) -> Result<(), &'static str> {
    while uart.read_byte().is_some() {} // drain stale RX
    for &b in &[0xA5u8, 0x5A, 0x00, 0xFF, b'R', b'U', b'S', b'T'] {
        echo_byte(uart, b)?;
    }
    Ok(())
}

/// [2/3] UART2 self-test using on-chip loopback — needs no external wiring.
fn uart2_internal_loopback(
    uart2: pac::Uart2,
    pins: Uart2Pins,
    clocks: &Clock<Hp>,
) -> Result<(), &'static str> {
    let mut uart = Uart::new(
        uart2,
        pins,
        UartConfig {
            loopback: true,
            ..Default::default()
        },
        clocks,
    )
    .map_err(|_| "Uart2::new failed")?;
    run_pattern(&mut uart)
}

/// [3/3] UART2 over the real JP1 pads — requires a D01 (TXD) ↔ D00 (RXD) jumper.
#[cfg(feature = "external-loopback")]
fn uart2_external_loopback(
    uart2: pac::Uart2,
    pins: Uart2Pins,
    clocks: &Clock<Hp>,
) -> Result<(), &'static str> {
    let mut uart =
        Uart::new(uart2, pins, UartConfig::default(), clocks).map_err(|_| "Uart::new failed")?;
    // No LBE: bytes travel out TXD and back in RXD over the jumper wire.
    run_pattern(&mut uart)
}

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();
    let crg = pac.crg.constrain(Config::default());
    // Promote the clock to `'static` so the UART1 console (which borrows it)
    // can be stored in the `'static` `SERIAL` cell.
    let clock = CLOCK.init(crg.into_hp_clock().expect("lock Hp"));

    // UART1 for console output. COM is a Dyn clock → the UART borrows `clock`.
    let parts = Parts::new(pac.topreg);
    let uart1_pins = Uart1Pins {
        tx: parts.gp_spi0_cs_x,
        rx: parts.gp_spi0_sck,
    };
    let uart2_pins = Uart2Pins {
        tx: parts.gp_uart2_txd,
        rx: parts.gp_uart2_rxd,
    };
    let uart =
        Uart::new(pac.uart1, uart1_pins, Default::default(), clock).expect("uart1 init failed");

    defmt_serial::defmt_serial(crate::SERIAL.init(uart));

    defmt::println!("uart_peripheral: starting UART tests");
    let mut all_ok = true;

    // [1/3] Reaching here over defmt proves UART1 TX + the console path works.
    defmt::println!("[1/3] console_uart1: PASS (UART1 console up, defmt-serial logging)");

    // [2/3] UART2 internal loopback (no wiring).
    match uart2_internal_loopback(pac.uart2, uart2_pins, clock) {
        Ok(()) => defmt::println!("[2/3] uart2_internal_loopback: PASS"),
        Err(e) => {
            all_ok = false;
            defmt::println!("[2/3] uart2_internal_loopback: FAIL: {}", e);
        }
    }

    // [3/3] UART2 external loopback (needs a jumper; off by default).
    #[cfg(feature = "external-loopback")]
    {
        // Re-acquire UART2: the internal-loopback instance above already consumed
        // and dropped the PAC token. Safe here — single-threaded, no live UART2.
        let stolen = unsafe { pac::Peripherals::steal() };
        let uart2 = stolen.uart2;
        let parts = Parts::new(stolen.topreg);
        let uart2_pins = Uart2Pins {
            tx: parts.gp_uart2_txd,
            rx: parts.gp_uart2_rxd,
        };
        match uart2_external_loopback(uart2, uart2_pins, clock) {
            Ok(()) => defmt::println!("[3/3] uart2_external_loopback: PASS"),
            Err(e) => {
                all_ok = false;
                defmt::println!("[3/3] uart2_external_loopback: FAIL: {}", e);
            }
        }
    }
    #[cfg(not(feature = "external-loopback"))]
    defmt::println!(
        "[3/3] uart2_external_loopback: SKIPPED (--features external-loopback + jumper JP1 D01↔D00)"
    );

    defmt::println!("TEST RESULT: {=str}", if all_ok { "PASS" } else { "FAIL" });

    // Verdict delivered; halt cleanly (the UART FIFO has already drained the line).
    loop {
        asm::wfi();
    }
}
