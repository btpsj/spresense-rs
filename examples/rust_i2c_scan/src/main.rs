#![no_std]
#![no_main]

use core::fmt::Write;

use cortex_m_rt::entry;
use embedded_hal::i2c::I2c;
use panic_halt as _;

use cxd56_hal::uart::Uart;
use cxd56_hal::{
    clocks::{Config, RccExt},
    i2c::I2c0Pins,
};
use cxd56_hal::{gpio::pins::Parts, i2c::I2c as HalI2c, uart::Uart1Pins};
use cxd56_hal::{i2c::I2cConfig, pac};

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();
    let crg = pac.crg.constrain(Config::default());
    let clock = crg.into_hp_clock().expect("lock Hp");

    // UART1 for console output. COM is a Dyn clock → the UART borrows `clock`.
    let parts = Parts::new(pac.topreg);
    let uart1_pins = Uart1Pins {
        tx: parts.gp_spi0_cs_x,
        rx: parts.gp_spi0_sck,
    };
    let i2c0_pins = I2c0Pins {
        scl: parts.gp_i2c0_bck,
        sda: parts.gp_i2c0_bdt,
    };
    let mut uart =
        Uart::new(pac.uart1, uart1_pins, Default::default(), &clock).expect("uart1 init failed");
    let mut i2c = HalI2c::new(pac.i2c0, i2c0_pins, &clock, I2cConfig::default()).unwrap();

    writeln!(uart, "I2C0 scan (0x03..0x77):").ok();

    let mut found = 0u32;
    for addr in 0x03u8..=0x77 {
        // Probe with a 1-byte read. A zero-length write does NOT work on the
        // DW_apb_i2c controller: an empty buffer never writes IC_DATA_CMD, so no
        // START/address is ever put on the bus and the transfer reports success
        // for every address. A read queues a CMD_READ, forcing START+address;
        // an unanswered address raises TX_ABRT → I2cError::NoAck.
        let mut b = [0u8; 1];
        if i2c.read(addr, &mut b).is_ok() {
            writeln!(uart, "  found 0x{:02x}", addr).ok();
            found += 1;
            continue;
        }
        // Fallback for command-first parts (e.g. Sensirion SGP40 at 0x59) that
        // NACK a bare read until a command is issued. A 1-byte write puts
        // START+address+one data byte on the bus; the part ACKs the address even
        // though the lone byte is just an opcode it accepts or ignores. Mark it
        // "(w)" so a read-only hit and a write-only hit are distinguishable.
        if i2c.write(addr, &[0x00]).is_ok() {
            writeln!(uart, "  found 0x{:02x} (w)", addr).ok();
            found += 1;
        }
    }

    writeln!(uart, "{} device(s) found.", found).ok();

    loop {
        cortex_m::asm::wfe();
    }
}
