//! Stream 6-axis IMU data from the CXD5602PWBIMU add-on board.
//!
//! The board is dual-bus: I2C0 @ 400 kHz is the control plane (identity, ODR,
//! full-scale range, output enable) and SPI5 is the data plane (the DRDY-gated
//! 6-axis sample stream). This example powers the board up, confirms it with a
//! `whoami`, configures it (60 Hz, ±4 g, ±500 dps), starts the stream, then
//! reads samples over SPI and prints accel (g) and gyro (dps) to UART1.
//!
//! Pins (fixed by the add-on board): power = EMMC_DATA2 (D20),
//! reset/XRST = I2S0_BCK (D26), I2C SCL/SDA = I2C0_BCK/BDT (D15/D14),
//! SPI SCK/MOSI/MISO = EMMC_CLK/DATA0/DATA1 (D23/D16/D17),
//! SPI chip-select = I2S0_DATA_IN (D19), DRDY = EMMC_DATA3 (D21).
//!
//! Status is mirrored on on-board LED0 (`gp_i2s1_bck`): **on** while bringing
//! the board up, **blinking** while streaming, **off** on failure.

#![no_std]
#![no_main]

use core::fmt::Write;

use cortex_m::Peripherals as CorePeripherals;
use cortex_m_rt::entry;
use panic_halt as _;

use cxd56_hal::{
    clocks::{Config, RccExt},
    delay_alt::Delay,
    gpio::{pins::Parts, Level, Output},
    i2c_alt::{I2c, I2c0Pins, I2cConfig},
    pac,
    pac::topreg::GpI2s1Bck,
    spi_alt::{Spi, Spi5Pins, SpiConfig},
    uart_alt::{Uart, Uart1Pins},
};

use spresense_bsp::pwbimu::{self, AccelRange, GyroRange, Odr};

/// Print a sample and toggle LED0 every this many samples (≈4 Hz at 60 Hz ODR,
/// so the LED blinks ~2 Hz — visibly "alive" without flooding the console).
const PRINT_EVERY: u32 = 15;

/// LED0 off, then spin forever — the failure indicator.
fn fail(led: &mut Output<GpI2s1Bck>) -> ! {
    led.set_low();
    loop {
        cortex_m::asm::wfe();
    }
}

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();
    let core = CorePeripherals::take().unwrap();

    let crg = pac.crg.constrain(Config::default());
    let clock = crg.into_clock();

    let parts = Parts::new(pac.topreg);

    // UART1 console.
    let uart1_pins = Uart1Pins {
        tx: parts.gp_spi0_cs_x,
        rx: parts.gp_spi0_sck,
    };
    let mut uart =
        Uart::new(pac.uart1, uart1_pins, Default::default(), &clock).expect("uart1 init failed");

    // I2C0 control plane @ 400 kHz.
    let i2c0_pins = I2c0Pins {
        scl: parts.gp_i2c0_bck,
        sda: parts.gp_i2c0_bdt,
    };
    let i2c =
        I2c::new(pac.i2c0, i2c0_pins, &clock, I2cConfig::default()).expect("i2c0 init failed");

    // SPI5 data plane @ MODE_0, 8-bit, 1 MHz (the defaults the IMU expects).
    let spi5_pins = Spi5Pins {
        sck: parts.gp_emmc_clk,
        csn: parts.gp_emmc_cmd,
        mosi: parts.gp_emmc_data0,
        miso: parts.gp_emmc_data1,
    };
    let spi =
        Spi::new(pac.spi5, spi5_pins, SpiConfig::default(), &clock).expect("spi5 init failed");

    let mut delay = Delay::new(core.SYST, &clock);

    // LED0 mirrors status: on = bringing up, blinking = streaming, off = fail.
    let mut led = parts.gp_i2s1_bck.into_output(Level::Low);
    led.set_high();

    writeln!(uart, "CXD5602PWBIMU: bringing up board").ok();

    // One handle over both buses: take the two bus objects plus the four board
    // GPIOs (power, reset, chip-select, DRDY). `spresense()` is the BSP
    // convenience for the add-on's direct connection — it wires I2C0/SPI5 and the
    // fixed add-on pins into the otherwise host-agnostic `Pwbimu` driver. The
    // board lifecycle is a typestate — this yields an `Off` handle, and each
    // transition consumes it and hands it back in the next state, so the bring-up
    // order is checked at compile time.
    let imu = pwbimu::spresense(
        i2c,
        spi,
        parts.gp_emmc_data2,
        parts.gp_i2s0_bck,
        parts.gp_i2s0_data_in,
        parts.gp_emmc_data3,
    );

    // Off -> Idle: sequence the board up and probe its identity. Reaching Idle
    // means the PSoC answered, so an absent/mis-seated board fails right here.
    let mut imu = match imu.power_on(&mut delay) {
        Ok(imu) => imu,
        Err(e) => {
            writeln!(uart, "power-on probe failed: {:?} — board not detected", e.source).ok();
            fail(&mut led);
        }
    };

    // Already confirmed present; read the full identity just to log it.
    if let Ok(id) = imu.whoami() {
        writeln!(
            uart,
            "whoami -> fw=0x{:02x} hwrev=0x{:02x} uid=0x{:02x}",
            id.fw_ver, id.hw_rev, id.hw_uid
        )
        .ok();
    }

    // Configure (DRDY + ODR + ranges) while still in Idle.
    if let Err(e) = imu.configure(Odr::Hz60, AccelRange::G4, GyroRange::Dps500) {
        writeln!(uart, "configure failed: {e:?}").ok();
        fail(&mut led);
    };

    // Idle -> Streaming: start the 6-axis output stream.
    let mut imu = match imu.enable() {
        Ok(streaming) => streaming,
        Err(e) => {
            writeln!(uart, "enable failed: {e:?}").ok();
            fail(&mut led);
        }
    };

    // This demo doesn't worry about a stalled board, so it just blocks for each
    // sample. If you need to bound the wait, poll the non-blocking
    // `imu.read_sample()` (returns `Err(Error::SampleNotReady)` until ready)
    // against your own deadline and surface your own timeout error.
    let mut count: u32 = 0;
    let mut led_on = true;
    loop {
        let sample = match imu.read_sample_blocking(&mut delay) {
            Ok(s) => s,
            Err(e) => {
                writeln!(uart, "sample read failed: {e:?}").ok();
                fail(&mut led);
            }
        };

        count = count.wrapping_add(1);
        if count.is_multiple_of(PRINT_EVERY) {
            led_on = !led_on;
            led.set_level(if led_on { Level::High } else { Level::Low });
            let [ax, ay, az] = sample.accel;
            let [gx, gy, gz] = sample.gyro;
            writeln!(
                uart,
                "acc[g]: {ax:>7.3} {ay:>7.3} {az:>7.3}  gyr[dps]: {gx:>8.2} {gy:>8.2} {gz:>8.2}  t={:.1}C",
                sample.temp
            )
            .ok();
        }
    }
}
