//! Profile-aware DW_apb_i2c driver.
//!
//! Mirrors [`crate::i2c`] but takes the [`Clock`] abstraction instead of the
//! older [`crate::clocks::Clocks`] snapshot, and is **generic over the PAC I2C
//! token** via the sealed [`I2cPeriph`] trait. Currently only [`pac::I2c0`] is
//! implemented; I2C1 and I2C2 require PAC regeneration and additional
//! clock-enable work (see the `Follow-ups` section of the implementation plan).
//!
//! # Lifetime — why there is none
//!
//! All I2C clock sources are **[`Fixed`](crate::clocks::Fixed)**: I2C0/1 use `SCU`
//! (RCOSC/XOSC/RTC-derived) and I2C2 uses `COM` (SYS-bus-derived). Neither
//! tracks `appsmp`, the clock changed by [`Clock::request_perf`]. A programmed
//! SCL frequency remains correct across HP↔LP transitions, so there is no need
//! to hold a `&Clock` borrow after construction.
//!
//! # Driver size
//!
//! [`I2c<T>`] holds the consumed PAC token and the GPIO pin bundle. The SCL
//! timing registers (`*_SCL_HCNT`/`LCNT`) persist across the
//! disable/enable cycle inside [`begin_transfer`], so the frequency need only be
//! applied once in [`I2c::new`] rather than per-transaction (mirroring
//! `cxd56_i2c_setfrequency` caching behaviour in the NuttX driver).

use crate::clocks::{Clock, ClockError, PeripheralId};
use crate::gpio::GpioPin;
use crate::pac;
use crate::regs::topreg;
use embedded_hal::i2c::{ErrorKind, ErrorType, NoAcknowledgeSource, Operation};
use fugit::Hertz;
use thiserror::Error;

/// Errors from the generic I2C driver.
///
/// Wraps [`ClockError`] so it is not re-exported from this module's public API.
#[derive(Debug, Error)]
pub enum I2cError {
    /// Address not acknowledged (no device at that address).
    #[error("address not acknowledged")]
    NoAck,
    /// Bus arbitration lost.
    #[error("bus arbitration lost")]
    Arbitration,
    /// Operation timed out waiting for FIFO or controller.
    #[error("operation timed out")]
    Timeout,
    /// Clock gate enable failed.
    #[error("clock gate error: {0}")]
    Clock(#[from] ClockError),
}

impl embedded_hal::i2c::Error for I2cError {
    fn kind(&self) -> ErrorKind {
        match self {
            I2cError::NoAck => ErrorKind::NoAcknowledge(NoAcknowledgeSource::Unknown),
            I2cError::Arbitration => ErrorKind::ArbitrationLoss,
            I2cError::Timeout => ErrorKind::Other,
            I2cError::Clock(_) => ErrorKind::Other,
        }
    }
}

pub struct I2cConfig {
    pub freq: Hertz<u32>,
}

impl Default for I2cConfig {
    fn default() -> Self {
        Self {
            freq: Hertz::<u32>::kHz(400),
        }
    }
}

// ============================================================================
// Sealed peripheral trait
// ============================================================================

mod sealed {
    use core::ops::Deref;
    use fugit::Hertz;

    use super::pac;
    use crate::clocks::{Clock, PeripheralId};

    /// Sealed marker trait. Implemented only for PAC I2C tokens within this
    /// crate so downstream code cannot introduce unsound implementations.
    pub trait Sealed: Deref<Target = pac::i2c0::RegisterBlock> {
        /// Clock-gate identifier used by [`PeripheralId::enable`] /
        /// [`PeripheralId::disable`].
        const ID: PeripheralId;

        /// Route this port's pads to the I2C function.
        fn pinmux();

        /// Restore this port's pads to Func0 (GPIO).
        fn unpinmux();

        /// Sample this port's base clock from `clock`.
        ///
        /// All I2C clocks are [`Fixed`](crate::clocks::Fixed) and therefore
        /// `Copy`, so the `clock` borrow ends here and does not need to be
        /// retained.
        fn base_hz(clock: &Clock) -> Hertz<u32>;
    }
}

/// GPIO tokens for the I2C0 pads.
///
/// Consumed by [`I2c::new`] to enforce at the type level that no other code
/// can drive these pads as GPIO while the I2C bus is live. Returned by
/// [`I2c::free`] with the pads restored to Func0 (GPIO).
///
/// | Field | Spresense pad | Arduino | Header  |
/// |-------|--------------|---------|---------|
/// | `scl` | I2C0_BCK     | D15     | JP2-11  |
/// | `sda` | I2C0_BDT     | D14     | JP2-12  |
pub struct I2c0Pins {
    pub scl: GpioPin<pac::topreg::GpI2c0Bck>,
    pub sda: GpioPin<pac::topreg::GpI2c0Bdt>,
}

/// Maps a PAC I2C token to its clock-gate, pin-mux, and base clock.
///
/// Sealed — implemented only for [`pac::I2c0`] today (and future
/// `pac::I2c1`/`pac::I2c2` once those tokens are added to the PAC).
pub trait I2cPeriph: sealed::Sealed {
    /// GPIO pin bundle consumed at construction and returned by [`I2c::free`].
    type Pins;
}

// ----------------------------------------------------------------------------
// pac::I2c0 — SCU_I2C0 (SCU clock, base 0x0418_D400, IRQ 17)
// ----------------------------------------------------------------------------

impl sealed::Sealed for pac::I2c0 {
    const ID: PeripheralId = PeripheralId::I2c0;

    fn pinmux() {
        i2c0_pinmux();
    }

    fn unpinmux() {
        i2c0_unpinmux();
    }

    fn base_hz(clock: &Clock) -> Hertz<u32> {
        // SCU clock is Fixed — never changes with request_perf.
        clock.scu.hz()
    }
}

impl I2cPeriph for pac::I2c0 {
    type Pins = I2c0Pins;
}

// ============================================================================
// Private register helpers (adapted from i2c.rs — private there, so copied)
// ============================================================================

fn i2c0_unpinmux() {
    topreg()
        .iocsys_iomd1()
        .modify(|_, w| unsafe { w.i2c0().bits(0) });
}

// I2C0_BCK = SCL, I2C0_BDT = SDA — TOPREG offsets 0x8b0/0x8b4.
// Both pads: 2 mA drive (LOWEMI=1), pull-up (PUN=1), input enabled (ENZI=1)
// for open-drain operation. IOCSYS_IOMD1[19:18] = 1 (Func1 → I2C0).
fn i2c0_pinmux() {
    topreg()
        .io_i2c0_bck()
        .write(|w| w.lowemi().set_bit().pun().set_bit().enzi().set_bit());
    topreg()
        .io_i2c0_bdt()
        .write(|w| w.lowemi().set_bit().pun().set_bit().enzi().set_bit());
    topreg()
        .iocsys_iomd1()
        .modify(|_, w| unsafe { w.i2c0().bits(1) });
}

// Disable the controller and wait for IC_ENABLE_STATUS.IC_EN to clear.
// The DW_apb_i2c manual requires this before changing IC_CON or IC_TAR.
fn ic_disable(i2c: &pac::i2c0::RegisterBlock) -> Result<(), I2cError> {
    i2c.ic_enable().write(|w| unsafe { w.bits(0) });
    let mut retry = 25_000u32;
    while i2c.ic_enable_status().read().ic_en().bit_is_set() {
        retry = retry.checked_sub(1).ok_or(I2cError::Timeout)?;
    }
    Ok(())
}

// Compute SCL high/low counts from the base clock and a target frequency.
// Mirrors cxd56_i2c_setfrequency (cxd56_i2c.c). Controller must be disabled.
//
// Called once during init — the `*CNT` registers persist across begin_transfer's
// IC_ENABLE toggle, so re-applying per-transaction is unnecessary when the base
// clock is Fixed (which it always is for I2C).
fn set_frequency(i2c: &pac::i2c0::RegisterBlock, base_hz: u32, target: Hertz<u32>) {
    let target_hz = target.to_Hz();
    let (t_high, t_low, spklen, use_fs_regs): (u64, u64, u64, bool) = if target_hz <= 200_000 {
        (40, 47, 4, false) // standard-speed
    } else {
        (6, 13, 1, true) // fast-speed
    };

    let base_khz = base_hz as u64 / 1_000;
    let hcnt = ((base_khz * t_high).div_ceil(1_000_000))
        .saturating_sub(spklen + 7)
        .max(1) as u32;
    let lcnt = ((base_khz * t_low).div_ceil(1_000_000)).max(1) as u32;

    if use_fs_regs {
        i2c.ic_fs_scl_hcnt().write(|w| unsafe { w.bits(hcnt) });
        i2c.ic_fs_scl_lcnt().write(|w| unsafe { w.bits(lcnt) });
    } else {
        i2c.ic_ss_scl_hcnt().write(|w| unsafe { w.bits(hcnt) });
        i2c.ic_ss_scl_lcnt().write(|w| unsafe { w.bits(lcnt) });
    }
}

// ============================================================================
// I2c<T> driver struct
// ============================================================================

/// Generic DW_apb_i2c master driver.
///
/// `T` is a PAC I2C token (currently only [`pac::I2c0`]).
///
/// Constructed via [`I2c::new`]; implements [`embedded_hal::i2c::I2c`].
pub struct I2c<T: I2cPeriph> {
    i2c: T,
    pins: T::Pins,
}

impl<T: I2cPeriph> I2c<T> {
    /// Enable the peripheral, set up pin-mux, program SCL timing, and return
    /// the driver.
    ///
    /// `pins` enforces at the type level that no other code can use these pads
    /// as GPIO while the bus is live. Call [`I2c::free`] to release them.
    ///
    /// `clock` is borrowed only to read `T::base_hz` — a [`Fixed`](crate::clocks::Fixed)
    /// value that does not change with [`Clock::request_perf`]. The borrow ends
    /// at this call; calling `request_perf` later is always allowed.
    pub fn new(i2c: T, pins: T::Pins, clock: &Clock, config: I2cConfig) -> Result<Self, I2cError> {
        T::ID.enable()?;
        T::pinmux();

        // `&i2c` coerces to `&pac::i2c0::RegisterBlock` via Deref.
        ic_disable(&i2c)?;

        // Mask all interrupts (we poll status registers, not IRQs).
        i2c.ic_intr_mask().write(|w| unsafe { w.bits(0) });
        // Read CLR_INTR to clear any pending interrupts.
        let _ = i2c.ic_clr_intr().read();

        // RX threshold: effectively "drain manually". TX threshold: 0 = when FIFO empty.
        i2c.ic_rx_tl().write(|w| unsafe { w.bits(0xff) });
        i2c.ic_tx_tl().write(|w| unsafe { w.bits(0) });

        // SDA hold = 1 cycle (minimum).
        i2c.ic_sda_hold().write(|w| unsafe { w.bits(1) });

        // IC_CON: master mode, SPEED=FS (2), restart enable, slave disable,
        // TX_EMPTY_CTRL, RX_FIFO_FULL_HLD_CTRL. SPEED field value 2 is used
        // for both SS and FS — the actual timing comes from the hcnt/lcnt
        // registers. Mirrors NuttX cxd56_i2c.c init.
        i2c.ic_con()
            .write(|w| unsafe { w.bits(1 | (2 << 1) | (1 << 5) | (1 << 6) | (1 << 8) | (1 << 9)) });

        // Apply frequency once. `base_hz` and `config` are not retained — the
        // *CNT registers persist across begin_transfer's IC_ENABLE toggle, and
        // the base clock is Fixed, so this value never needs updating.
        let base_hz = T::base_hz(clock).to_Hz();
        set_frequency(&i2c, base_hz, config.freq);

        Ok(Self { i2c, pins })
    }

    /// Disable the I2C controller, restore the pads to GPIO (Func0), and return
    /// the consumed PAC token and GPIO pin bundle.
    pub fn free(self) -> (T, T::Pins) {
        T::ID.disable().ok();
        T::unpinmux();
        let md = core::mem::ManuallyDrop::new(self);
        // SAFETY: Each field is moved out exactly once through a raw pointer.
        // ManuallyDrop prevents the struct-level Drop (which would call
        // disable() a second time) from running.
        let i2c = unsafe { core::ptr::read(&md.i2c) };
        let pins = unsafe { core::ptr::read(&md.pins) };
        (i2c, pins)
    }

    // Prepare the controller for a transaction: disable, set target address,
    // re-enable. Frequency registers are not re-applied (see above).
    fn begin_transfer(&mut self, addr: u8) -> Result<(), I2cError> {
        ic_disable(&self.i2c)?;
        self.i2c
            .ic_tar()
            .write(|w| unsafe { w.bits(addr as u32 & 0x7f) });
        self.i2c.ic_enable().write(|w| unsafe { w.bits(1) });
        Ok(())
    }

    // Check IC_RAW_INTR_STAT for TX_ABRT and translate to an I2cError.
    // Clears the abort latch via CLR_TX_ABRT and CLR_INTR.
    fn check_abort(&self) -> Result<(), I2cError> {
        let raw = self.i2c.ic_raw_intr_stat().read();
        if raw.tx_abrt().bit_is_set() {
            let src = self.i2c.ic_tx_abrt_source().read();
            let _ = self.i2c.ic_clr_tx_abrt().read();
            let _ = self.i2c.ic_clr_intr().read();
            if src.abrt_arb_lost().bit_is_set() {
                return Err(I2cError::Arbitration);
            }
            // NOACK on address or data → NoAck
            return Err(I2cError::NoAck);
        }
        Ok(())
    }

    fn write_bytes(&mut self, buf: &[u8]) -> Result<(), I2cError> {
        let len = buf.len();
        for (i, &byte) in buf.iter().enumerate() {
            // Poll TX FIFO not-full with timeout.
            let mut retry = 25_000u32;
            while !self.i2c.ic_status().read().tfnf().bit_is_set() {
                self.check_abort()?;
                retry = retry.checked_sub(1).ok_or(I2cError::Timeout)?;
            }
            let stop = if i == len - 1 { 1u32 << 9 } else { 0 };
            self.i2c
                .ic_data_cmd()
                .write(|w| unsafe { w.bits(byte as u32 | stop) });
        }

        // Wait for STOP_DET — the transaction has actually completed on the bus.
        // TX_EMPTY/TFE assert as soon as the byte leaves the FIFO for the shift
        // register, which is *before* the address/data (N)ACK is known; a write
        // to a NACKing address would otherwise drain the FIFO and return Ok
        // before the abort latches. STOP_DET (the STOP we requested on the last
        // byte, or the STOP the controller issues on abort) is the real
        // completion signal. check_abort still runs each poll to catch the NACK.
        let mut retry = 100_000u32;
        loop {
            self.check_abort()?;
            if self.i2c.ic_raw_intr_stat().read().stop_det().bit_is_set() {
                break;
            }
            retry = retry.checked_sub(1).ok_or(I2cError::Timeout)?;
        }
        // Final check in case the abort and STOP latched on the same poll.
        self.check_abort()?;

        let _ = self.i2c.ic_clr_stop_det().read();
        let _ = self.i2c.ic_clr_intr().read();
        Ok(())
    }

    fn read_bytes(&mut self, buf: &mut [u8]) -> Result<(), I2cError> {
        let len = buf.len();
        for (i, slot) in buf.iter_mut().enumerate() {
            let stop = if i == len - 1 { 1u32 << 9 } else { 0 };
            // CMD_READ (bit 8) queues a read command.
            self.i2c
                .ic_data_cmd()
                .write(|w| unsafe { w.bits((1 << 8) | stop) });

            // Poll RX FIFO not-empty with timeout.
            let mut retry = 25_000u32;
            while !self.i2c.ic_status().read().rfne().bit_is_set() {
                self.check_abort()?;
                retry = retry.checked_sub(1).ok_or(I2cError::Timeout)?;
            }
            *slot = (self.i2c.ic_data_cmd().read().bits() & 0xff) as u8;
        }

        self.check_abort()?;
        let _ = self.i2c.ic_clr_intr().read();
        Ok(())
    }
}

impl<T: I2cPeriph> Drop for I2c<T> {
    fn drop(&mut self) {
        T::ID.disable().ok();
    }
}

impl<T: I2cPeriph> ErrorType for I2c<T> {
    type Error = I2cError;
}

impl<T: I2cPeriph> embedded_hal::i2c::I2c for I2c<T> {
    fn transaction(&mut self, addr: u8, ops: &mut [Operation<'_>]) -> Result<(), Self::Error> {
        self.begin_transfer(addr)?;
        for op in ops {
            match op {
                Operation::Write(buf) => self.write_bytes(buf)?,
                Operation::Read(buf) => self.read_bytes(buf)?,
            }
        }
        Ok(())
    }
}
