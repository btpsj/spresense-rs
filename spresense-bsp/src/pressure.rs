use cxd56_hal::gpio::{GpioPin, Input};
use cxd56_hal::pac::topreg::GpI2s0Lrck;
use embedded_hal::i2c::I2c;
use lps22hh_rs::blocking;
use lps22hh_rs::blocking::register::OnState;
use st_mems_bus::blocking::i2c::I2cBus;

pub use lps22hh_rs::blocking::{Error as PressureError, I2CAddress};
pub use lps22hh_rs::blocking::prelude::Odr;

/// I2C address of the LPS22HH on the CommonSense board (CS/SDO tied to VDD_1.8V → 0x5D).
pub const ADDRESS: I2CAddress = I2CAddress::AddressH;

/// Expected WHO_AM_I response.
pub const WHO_AM_I: u8 = lps22hh_rs::blocking::ID;

/// Pressure and temperature reading converted to physical units.
pub struct Sample {
    /// Absolute pressure in hectopascals (hPa).
    pub pressure_hpa: f32,
    /// Temperature in degrees Celsius.
    pub temperature_celsius: f32,
}

/// Driver for the LPS22HH absolute pressure sensor on the CommonSense addon board.
///
/// Generic over the I2C bus — pass a bare `cxd56_hal::i2c::I2c0` for exclusive
/// access, or an `embedded_hal_bus::i2c::RefCellDevice` to share the bus with
/// other CommonSense drivers (HTS221, LIS2MDL, LSM6DSOX, etc.).
///
/// Construct with [`Pressure::new`], passing:
/// - an I2C bus handle implementing `embedded_hal::i2c::I2c`
/// - `pins.gp_i2s0_lrck` from [`cxd56_hal::gpio::pins::Parts`] — the INT/DRDY
///   line routed to sensor board J1 pin 7 (main-board J1 pin 7, Arduino I2S_LRCK)
pub struct Pressure<I: I2c> {
    inner: blocking::Lps22hh<I2cBus<I>, OnState>,
    /// INT/DRDY output — sensor J1 pin 7, main-board J1 pin 7, `gp_i2s0_lrck`.
    pub drdy: Input<GpI2s0Lrck>,
}

impl<I: I2c> Pressure<I> {
    /// Initialise the LPS22HH.
    ///
    /// Sequence: software reset → poll until complete → verify WHO_AM_I →
    /// enable block data update → set ODR to 10 Hz low-noise.
    ///
    /// Returns `Err` on any I2C error or if WHO_AM_I does not match [`WHO_AM_I`].
    pub fn new(i2c: I, drdy_pin: GpioPin<GpI2s0Lrck>) -> Result<Self, PressureError<I::Error>> {
        let mut sensor = blocking::Lps22hh::new_i2c(i2c, ADDRESS);

        sensor.reset_set(1)?;
        while sensor.reset_get()? != 0 {}

        let id = sensor.device_id_get()?;
        if id != WHO_AM_I {
            return Err(PressureError::UnexpectedValue);
        }

        sensor.block_data_update_set(1)?;
        sensor.data_rate_set(Odr::_10hzLowNoise)?;

        Ok(Self {
            inner: sensor,
            drdy: drdy_pin.into_input(),
        })
    }

    /// Read a pressure and temperature sample.
    ///
    /// Returns `Ok(None)` if new data is not yet available from either channel.
    pub fn read(&mut self) -> Result<Option<Sample>, PressureError<I::Error>> {
        if self.inner.press_flag_data_ready_get()? == 0
            || self.inner.temp_flag_data_ready_get()? == 0
        {
            return Ok(None);
        }

        let raw_p = self.inner.pressure_raw_get()?;
        let raw_t = self.inner.temperature_raw_get()?;

        Ok(Some(Sample {
            pressure_hpa: blocking::from_lsb_to_hpa(raw_p),
            temperature_celsius: blocking::from_lsb_to_celsius(raw_t),
        }))
    }

    /// Returns `true` if a new pressure measurement is available.
    pub fn pressure_ready(&mut self) -> Result<bool, PressureError<I::Error>> {
        self.inner.press_flag_data_ready_get().map(|v| v != 0)
    }

    /// Access the underlying `lps22hh-rs` driver for advanced configuration.
    pub fn driver(&mut self) -> &mut blocking::Lps22hh<I2cBus<I>, OnState> {
        &mut self.inner
    }
}
