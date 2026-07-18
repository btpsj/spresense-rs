//! Read the LIS2MDL 3-axis magnetometer over I2C0 using the `lis2mdl-rs` driver.
//!
//! Hardware: SensiEDGE CommonSense sensor board on a Spresense main board. The
//! LIS2MDL is wired to I2C0 (SCL = I2C0_BCK, SDA = I2C0_BDT) at slave address
//! 0x1E (CommonSense datasheet, Fig. 7). Console output goes to UART1, which is
//! the CP2102N USB serial console.
//!
//! The configuration sequence mirrors ST's reference flow for this part: check
//! WHO_AM_I, soft reset, enable block-data-update, 10 Hz ODR, set/reset mode set
//! to cancel-every-ODR, temperature compensation, then continuous mode. We then
//! poll the data-ready flag and print each sample in milligauss.

#![no_std]
#![no_main]

use core::fmt::Write;

use cortex_m::Peripherals as CorePeripherals;
use cortex_m_rt::entry;
use panic_halt as _;

use cxd56_hal::delay::Delay;
use cxd56_hal::uart::{Uart, Uart1Pins};
use cxd56_hal::{
    clocks::{Config, RccExt},
    gpio::pins::Parts,
    i2c::{I2c as HalI2c, I2c0Pins, I2cConfig},
    pac,
};

use lis2mdl_rs::blocking as lis2mdl;
use lis2mdl::prelude::*;
use lis2mdl::{I2CAddress, Lis2mdl, from_lsb_to_mgauss};

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();
    let core = CorePeripherals::take().unwrap();

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

    // I2C0 at the default 400 kHz (the LIS2MDL supports fast mode).
    let i2c = HalI2c::new(pac.i2c0, i2c0_pins, &clock, I2cConfig::default()).unwrap();

    // SysTick-backed delay. The driver takes ownership of it; loop pacing comes
    // from polling the 10 Hz data-ready flag, so we don't need a separate delay.
    let delay = Delay::new(core.SYST, &clock);

    writeln!(uart, "LIS2MDL magnetometer (I2C0 @ 0x1E)").ok();

    let mut sensor = Lis2mdl::new_i2c(i2c, I2CAddress::I2cAdd, delay);

    // WHO_AM_I — confirm the part answers with the expected ID before configuring.
    match sensor.device_id_get() {
        Ok(id) => {
            writeln!(uart, "WHO_AM_I = 0x{id:02x} (expected 0x{:02x})", lis2mdl::ID).ok();
            if id != lis2mdl::ID {
                writeln!(uart, "unexpected device ID — halting").ok();
                loop {
                    cortex_m::asm::wfe();
                }
            }
        }
        Err(_) => {
            writeln!(uart, "I2C read of WHO_AM_I failed — halting").ok();
            loop {
                cortex_m::asm::wfe();
            }
        }
    }

    // Configure per ST's reference flow. `sw_reset` restores the default register
    // values and polls SOFT_RST until hardware clears it.
    sensor.sw_reset().unwrap();
    sensor.block_data_update_set(1).unwrap();
    sensor.data_rate_set(Odr::_10hz).unwrap();
    sensor
        .set_rst_mode_set(SetRst::SensOffCancEveryOdr)
        .unwrap();
    sensor.offset_temp_comp_set(1).unwrap();
    sensor.operating_mode_set(Md::ContinuousMode).unwrap();

    writeln!(uart, "configured; streaming at 10 Hz").ok();

    loop {
        // Wait for a fresh sample (paces the loop to the 10 Hz ODR).
        if sensor.mag_data_ready_get().unwrap_or(0) == 0 {
            continue;
        }

        match sensor.magnetic_raw_get() {
            Ok(raw) => {
                let x = from_lsb_to_mgauss(raw[0]);
                let y = from_lsb_to_mgauss(raw[1]);
                let z = from_lsb_to_mgauss(raw[2]);
                // Fixed-width (one decimal, right-justified) so columns stay aligned
                // as magnitudes change; tabs separate the columns. Full scale is
                // ±50000.0 mG, so 8 chars (incl. sign and ".0") always fits in 9.
                writeln!(uart, "mag:\tx={x:>9.1}\ty={y:>9.1}\tz={z:>9.1} mG").ok();
            }
            Err(_) => {
                writeln!(uart, "mag read error").ok();
            }
        }
    }
}
