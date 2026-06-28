#![no_std]
#![no_main]

//! Validate the **SPI5 hardware peripheral** by bringing up the micro SD card on
//! the SensiEDGE CommonSense board over SPI.
//!
//! The SD card is the only SPI device on the board (SCK=EMMC_CLK, CS_X=EMMC_CMD,
//! MOSI=EMMC_DATA0, MISO=EMMC_DATA1, all on SPI5). Unlike most Rust HALs, CS here
//! cannot be a separate GPIO — the four pads share one mux group and CS is driven
//! by the PL022 hardware frame-select. We rely on the fact that the frame-select
//! stays asserted LOW for the whole of a single FIFO-fed burst (CXD5602 User
//! Manual, SPI5 "continuous transfer"), so each SD command + its response window
//! is issued as **one** `transfer_in_place` call and CS is held across it. See
//! `DEVELOPMENT.md` for the full rationale.
//!
//! Output is printed over UART1 (the on-board USB console), like `rust_i2c_scan`.

use core::fmt::Write;

use cortex_m_rt::entry;
use embedded_hal::spi::{SpiBus, MODE_0};
use fugit::Hertz;
use panic_halt as _;

use cxd56_hal::spi::{Spi, Spi5Pins, SpiConfig};
use cxd56_hal::uart::{Uart, Uart1Pins};
use cxd56_hal::{
    clocks::{Config, RccExt},
    gpio::pins::Parts,
    pac,
};

/// Response window: dummy `0xFF` bytes clocked after a command to capture the
/// reply. Ncr (command→response latency) is ≤ 8 bytes; the extra room covers the
/// 4 trailing bytes of an R3/R7 response.
const RESP_WINDOW: usize = 14;

/// Issue a 6-byte SD command as a single FIFO-fed burst — CS is held LOW for the
/// whole call — then scan the response window. Returns the index of the R1 byte
/// in `buf`; the R3/R7 trailing bytes (if any) follow at `buf[idx + 1..]`.
///
/// `buf` must be at least `6 + RESP_WINDOW` long. CRC is only checked by the card
/// for CMD0/CMD8 in SPI mode; other commands pass a dummy CRC with the end bit set.
fn send_cmd<S: SpiBus<u8>>(spi: &mut S, cmd: u8, arg: u32, crc: u8, buf: &mut [u8]) -> Option<usize> {
    buf.fill(0xFF);
    buf[0] = 0x40 | cmd; // start bit (0) + transmission bit (1) + command index
    buf[1] = (arg >> 24) as u8;
    buf[2] = (arg >> 16) as u8;
    buf[3] = (arg >> 8) as u8;
    buf[4] = arg as u8;
    buf[5] = crc;
    // buf[6..] stays 0xFF — the bytes clocked out to read the response back.
    spi.transfer_in_place(buf).ok()?;
    // R1 is the first byte after the command with its MSB (start bit) cleared.
    (6..buf.len()).find(|&i| buf[i] & 0x80 == 0)
}

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();
    let crg = pac.crg.constrain(Config::default());
    let clock = crg.into_clock();

    // UART1 for console output. COM is a Dyn clock → the UART borrows `clock`.
    let parts = Parts::new(pac.topreg);
    let uart1_pins = Uart1Pins {
        tx: parts.gp_spi0_cs_x,
        rx: parts.gp_spi0_sck,
    };
    let mut uart =
        Uart::new(pac.uart1, uart1_pins, Default::default(), &clock).expect("uart1 init failed");

    // SPI5 at the SD-card init clock (400 kHz, mode 0). Pins are the EMMC-A group.
    let spi5_pins = Spi5Pins {
        sck: parts.gp_emmc_clk,
        csn: parts.gp_emmc_cmd,
        mosi: parts.gp_emmc_data0,
        miso: parts.gp_emmc_data1,
    };
    let cfg = SpiConfig {
        frequency: Hertz::<u32>::Hz(400_000),
        mode: MODE_0,
        ..SpiConfig::default()
    };
    let mut spi = Spi::new(pac.spi5, spi5_pins, cfg, &clock).expect("spi5 init failed");

    writeln!(uart, "SPI5 SD-card validation").ok();

    let mut buf = [0xFF_u8; 6 + RESP_WINDOW];

    // 1) Power-up: clock ≥ 74 cycles to let the card's SPI engine wake up.
    spi.write(&[0xFF; 10]).ok();

    // 2) CMD0 GO_IDLE_STATE → R1 = 0x01 (idle). This single exchange already
    //    proves command-out, clock, and response-in over SPI5 with CS held.
    let r1 = send_cmd(&mut spi, 0, 0, 0x95, &mut buf).map(|i| buf[i]);
    writeln!(uart, "CMD0  GO_IDLE_STATE  -> R1={:#04x}", r1.unwrap_or(0xFF)).ok();
    if r1 != Some(0x01) {
        writeln!(uart, "  no valid idle response - is a card inserted? FAIL").ok();
        halt();
    }
    writeln!(uart, "  idle state OK").ok();

    // 3) CMD8 SEND_IF_COND (arg 0x1AA, CRC 0x87) → R7; echo 0xAA back ⇒ SD v2.
    let v2 = match send_cmd(&mut spi, 8, 0x0000_01AA, 0x87, &mut buf) {
        Some(i) if buf[i] == 0x01 => {
            let voltage = buf.get(i + 3).copied().unwrap_or(0) & 0x0F;
            let echo = buf.get(i + 4).copied().unwrap_or(0);
            writeln!(uart, "CMD8  SEND_IF_COND   -> R7 voltage={:#03x} echo={:#04x}", voltage, echo).ok();
            echo == 0xAA && voltage == 0x1
        }
        Some(i) => {
            writeln!(uart, "CMD8  SEND_IF_COND   -> R1={:#04x} (v1 card / illegal cmd)", buf[i]).ok();
            false
        }
        None => {
            writeln!(uart, "CMD8  SEND_IF_COND   -> no response").ok();
            false
        }
    };

    // 4) ACMD41 init loop: CMD55 (APP_CMD) + ACMD41 (HCS bit when v2) until R1=0x00.
    let acmd41_arg = if v2 { 0x4000_0000 } else { 0 };
    let mut ready = false;
    for _ in 0..50_000u32 {
        send_cmd(&mut spi, 55, 0, 0x01, &mut buf);
        if let Some(i) = send_cmd(&mut spi, 41, acmd41_arg, 0x01, &mut buf) {
            if buf[i] == 0x00 {
                ready = true;
                break;
            }
        }
    }
    writeln!(uart, "ACMD41 SD_SEND_OP_COND -> {}", if ready { "ready" } else { "TIMED OUT" }).ok();
    if !ready {
        writeln!(uart, "  card never left idle. FAIL").ok();
        halt();
    }

    // 5) CMD58 READ_OCR → R3; CCS (OCR bit 30) distinguishes SDHC/SDXC from SDSC.
    if let Some(i) = send_cmd(&mut spi, 58, 0, 0x01, &mut buf) {
        let b = |k: usize| buf.get(i + 1 + k).copied().unwrap_or(0);
        let ocr = u32::from_be_bytes([b(0), b(1), b(2), b(3)]);
        let ccs = ocr & (1 << 30) != 0;
        writeln!(
            uart,
            "CMD58 READ_OCR       -> OCR={:#010x}  type={}",
            ocr,
            if ccs { "SDHC/SDXC (block addressed)" } else { "SDSC (byte addressed)" }
        )
        .ok();
    } else {
        writeln!(uart, "CMD58 READ_OCR       -> no response").ok();
    }

    writeln!(uart, "SPI5 SD validation: PASS").ok();
    halt();
}

/// Park the core; nothing more to do once the report is printed.
fn halt() -> ! {
    loop {
        cortex_m::asm::wfe();
    }
}
