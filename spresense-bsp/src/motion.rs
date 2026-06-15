use cxd56_hal::gpio::{GpioPin, Input};
use cxd56_hal::pac::topreg::GpEmmcData2;
use embedded_hal::delay::DelayNs;
use embedded_hal::i2c::I2c;
use lsm6dsox::{Lsm6dsox, PrimaryRegister};

pub use lsm6dsox::{
    AccelerometerScale, DataRate, Error as MotionError, GyroscopeScale, InterruptCause,
    InterruptLine, InterruptSource, SlaveAddress, TapCfg, TapMode, TapSource,
};

/// I2C address of the LSM6DSOX on the CommonSense board (fixed, SDO/SA0 tied low → 0x6A).
pub const ADDRESS: SlaveAddress = SlaveAddress::Low;

/// Expected WHO_AM_I value for the LSM6DSOX.
pub const WHO_AM_I: u8 = 0x6C;

/// Acceleration reading in g (standard gravity).
pub struct Acceleration {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// Angular rate reading in degrees per second.
pub struct AngularRate {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// Driver for the LSM6DSOX 3D accelerometer + 3D gyroscope on the CommonSense
/// addon board.
///
/// Generic over the I2C bus — pass a bare `cxd56_hal::i2c::I2c0` for exclusive
/// access, or an `embedded_hal_bus::i2c::RefCellDevice` to share the bus with
/// other CommonSense drivers (HTS221, LIS2MDL, LPS22HH, etc.).
///
/// Construct with [`Motion::new`], passing:
/// - an I2C bus handle implementing `embedded_hal::i2c::I2c`
/// - `pins.gp_emmc_data2` from [`cxd56_hal::gpio::pins::Parts`] — the INT1
///   line routed to sensor board J2 pin 5 (main-board JP2 pin 5, Arduino D20)
/// - any `DelayNs` source (e.g. [`cxd56_hal::delay::Delay`]), required for the
///   software-reset sequence
///
/// INT2 is wired to port P1 of the PCA9538 I/O expander and is not represented
/// here; use the expander driver if interrupt-driven INT2 is needed.
pub struct Motion<I: I2c, D: DelayNs> {
    inner: Lsm6dsox<I, D>,
    /// INT1 interrupt input — sensor J2 pin 5, main-board JP2 pin 5, `gp_emmc_data2`.
    pub int1: Input<GpEmmcData2>,
}

impl<I: I2c, D: DelayNs> Motion<I, D> {
    /// Initialise the LSM6DSOX.
    ///
    /// Sequence: verify WHO_AM_I → software reset → disable I3C → enable block
    /// data update → power up accelerometer and gyroscope at 52 Hz with the
    /// default scales (±2 g, ±250 dps).
    ///
    /// Returns `Err` on any I2C error or if WHO_AM_I does not match [`WHO_AM_I`].
    pub fn new(
        i2c: I,
        int1_pin: GpioPin<GpEmmcData2>,
        delay: D,
    ) -> Result<Self, MotionError<I::Error>> {
        let mut sensor = Lsm6dsox::new(i2c, ADDRESS, delay);

        sensor.check_id().map_err(|_| MotionError::InvalidData)?;
        sensor.setup()?;

        sensor.set_accel_sample_rate(DataRate::Freq52Hz)?;
        sensor.set_gyro_sample_rate(DataRate::Freq52Hz)?;

        Ok(Self {
            inner: sensor,
            int1: int1_pin.into_input(),
        })
    }

    /// Read the latest acceleration sample in g.
    ///
    /// Returns `Ok(None)` if new data is not yet available.
    pub fn read_accel(&mut self) -> Result<Option<Acceleration>, MotionError<I::Error>> {
        use lsm6dsox::accelerometer::Accelerometer;
        match self.inner.accel_norm() {
            Ok(v) => Ok(Some(Acceleration {
                x: v.x,
                y: v.y,
                z: v.z,
            })),
            // Every error path in accel_norm attaches a cause, so into_cause
            // cannot panic here.
            Err(e) => match e.into_cause() {
                MotionError::NoDataReady => Ok(None),
                cause => Err(cause),
            },
        }
    }

    /// Read the latest angular rate sample in degrees per second.
    ///
    /// Returns `Ok(None)` if new data is not yet available.
    pub fn read_gyro(&mut self) -> Result<Option<AngularRate>, MotionError<I::Error>> {
        match self.inner.angular_rate() {
            // The driver encodes dps as rotational frequency (dps / 360),
            // so the hertz value maps back to dps exactly.
            Ok(r) => Ok(Some(AngularRate {
                x: (r.x.as_hertz() * 360.0) as f32,
                y: (r.y.as_hertz() * 360.0) as f32,
                z: (r.z.as_hertz() * 360.0) as f32,
            })),
            Err(MotionError::NoDataReady) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Read the embedded temperature sensor in degrees Celsius.
    ///
    /// Returns `Ok(None)` if new data is not yet available.
    pub fn temperature(&mut self) -> Result<Option<f32>, MotionError<I::Error>> {
        match self.inner.temperature() {
            Ok(t) => Ok(Some(t.as_celsius() as f32)),
            Err(MotionError::NoDataReady) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Returns `true` if a new accelerometer measurement is available.
    pub fn accel_ready(&mut self) -> Result<bool, MotionError<I::Error>> {
        self.status().map(|s| s & 0b0000_0001 != 0)
    }

    /// Returns `true` if a new gyroscope measurement is available.
    pub fn gyro_ready(&mut self) -> Result<bool, MotionError<I::Error>> {
        self.status().map(|s| s & 0b0000_0010 != 0)
    }

    // STATUS_REG: bit 0 = XLDA (accel), bit 1 = GDA (gyro), bit 2 = TDA (temp).
    // Safety: STATUS_REG is read-only; reading it cannot invalidate the
    // driver's configuration state, which is what register_access guards.
    fn status(&mut self) -> Result<u8, MotionError<I::Error>> {
        unsafe { self.inner.register_access() }.read_reg(PrimaryRegister::STATUS_REG)
    }

    /// Access the underlying `lsm6dsox` driver for advanced configuration
    /// (scales, data rates, tap detection, interrupts, FIFO, …).
    pub fn driver(&mut self) -> &mut Lsm6dsox<I, D> {
        &mut self.inner
    }

    /// Consume `Motion` and return the underlying I2C bus and delay.
    pub fn destroy(self) -> (I, D) {
        self.inner.destroy()
    }
}
