use embedded_hal::delay::DelayNs;
use embedded_hal::i2c::I2c;
use lis2mdl_rs::blocking::prelude::{Md, OnState};
use lis2mdl_rs::blocking::{self, from_lsb_to_mgauss, I2CAddress};
use st_mems_bus::blocking::i2c::I2cBus;

/// I2C address of the LIS2MDL on the CommonSense board (fixed, CS pin tied to GND).
pub const ADDRESS: u8 = 0x1E;

/// Expected WHO_AM_I value for the LIS2MDL.
pub const WHO_AM_I: u8 = 0x40;

/// Calibrated magnetic field reading in milligauss.
pub struct MagneticField {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

pub use lis2mdl_rs::blocking::Error;

/// Driver for the LIS2MDL 3-axis magnetometer on the CommonSense addon board.
///
/// Generic over the I2C bus so the same peripheral can be shared across
/// multiple sensors via a bus-sharing wrapper (e.g. `embedded-hal-bus`
/// `RefCellDevice` or `MutexDevice`).
///
/// `D` is a delay source required for the software-reset sequence during
/// construction; it is retained in the struct for potential future resets.
pub struct Magnetometer<I: I2c, D: DelayNs> {
    inner: blocking::Lis2mdl<I2cBus<I>, D, OnState>,
}

impl<I: I2c, D: DelayNs> Magnetometer<I, D> {
    /// Initialise the LIS2MDL.
    ///
    /// Performs a software reset, enables block data update, and starts
    /// continuous-measurement mode.  Returns `Err` if the device does not
    /// respond or the WHO_AM_I value is unexpected.
    pub fn new(i2c: I, delay: D) -> Result<Self, Error<I::Error>> {
        let mut mag = blocking::Lis2mdl::new_i2c(i2c, I2CAddress::I2cAdd, delay);

        mag.sw_reset()?;
        mag.block_data_update_set(1)?;
        mag.operating_mode_set(Md::ContinuousMode)?;

        let id = mag.device_id_get()?;
        if id != WHO_AM_I {
            return Err(Error::UnexpectedValue);
        }

        Ok(Self { inner: mag })
    }

    /// Read the latest magnetic field sample.
    ///
    /// Returns x/y/z components in milligauss.
    pub fn read(&mut self) -> Result<MagneticField, Error<I::Error>> {
        let raw = self.inner.magnetic_raw_get()?;
        Ok(MagneticField {
            x: from_lsb_to_mgauss(raw[0]),
            y: from_lsb_to_mgauss(raw[1]),
            z: from_lsb_to_mgauss(raw[2]),
        })
    }

    /// Returns `true` if a new measurement is available.
    pub fn data_ready(&mut self) -> Result<bool, Error<I::Error>> {
        self.inner.mag_data_ready_get().map(|v| v != 0)
    }

    /// Access the underlying `lis2mdl-rs` driver for advanced configuration.
    pub fn driver(&mut self) -> &mut blocking::Lis2mdl<I2cBus<I>, D, OnState> {
        &mut self.inner
    }
}
