//! CXD5602PWBIMU add-on board — dual-bus driver.
//!
//! The CXD5602PWBIMU is *not* a bare IMU chip addressed directly. The board
//! fronts the sensor with one or more on-board PSoC microcontrollers, and the
//! host talks to them over **two buses at once**:
//!
//! * **I2C @ 400 kHz — the control plane.** Identity, full-scale range, ODR,
//!   interrupt enable, FIFO threshold, output enable, calibration, mode.
//! * **SPI (+DMA) — the data plane.** The high-rate 6-axis sample stream,
//!   triggered by `DRDY`.
//!
//! # The driver is host-agnostic
//!
//! [`Pwbimu`] cares only that it is handed something implementing the
//! `embedded-hal` bus and pin traits: an I2C master ([`embedded_hal::i2c::I2c`]),
//! a SPI bus ([`embedded_hal::spi::SpiBus`]), and four GPIOs — power, reset,
//! chip-select ([`OutputPin`](embedded_hal::digital::OutputPin)) and DRDY
//! ([`InputPin`](embedded_hal::digital::InputPin)). It has **no** dependency on
//! the CXD56 HAL, on I2C0/SPI5, or on any particular pad assignment, so the same
//! driver works whether the add-on is plugged straight into a Spresense or wired
//! to a custom PCB on a different I2C/SPI combination (or even a different chip).
//!
//! On a Spresense the add-on plugs directly into the main board, which is the
//! common case: [`spresense`] is a convenience constructor that consumes the
//! CXD56 HAL's I2C0/SPI5 buses and the fixed add-on GPIOs and hands back a
//! [`SpresensePwbimu`]. The wiring it assumes is the table in
//! [Spresense direct-connection wiring](#spresense-direct-connection-wiring).
//!
//! [`Pwbimu`] is a single handle over **both** buses: it owns the I2C control
//! plane (power sequencing, `whoami`, ODR / full-scale / output-enable) *and* the
//! SPI data plane (the DRDY-gated 6-axis sample read), so from one object you
//! can power_on and stream [`ImuSample`]s.
//!
//! # Reading axis data
//!
//! 1. [`Pwbimu::new`] → a handle in the [`Off`] state.
//! 2. [`Pwbimu::power_on`] — sequence the board up **and probe its identity**;
//!    on success returns it in [`Idle`], on no-answer returns [`NotResponding`].
//! 3. [`Pwbimu::whoami`] (optional) — read the full identity for logging.
//! 4. [`Pwbimu::configure`] — enable the DRDY line, set ODR and full-scale range.
//! 5. [`Pwbimu::enable`] — start the 6-axis stream; returns it in [`Streaming`].
//! 6. [`Pwbimu::read_sample_blocking`] — wait for DRDY, read one [`ImuSample`].
//!
//! The lifecycle is tracked in the type system (see [`state`]): each step is only
//! reachable in the right state, so e.g. reading a sample before [`enable`] or
//! configuring before [`power_on`] is a **compile error**, not a runtime hang.
//! The transitions ([`power_on`], [`enable`], [`disable`]) consume the handle and
//! return it in its new state, so the idiom is `let imu = imu.power_on(&mut d)?;`.
//! [`power_on`] *earns* the [`Idle`] state by confirming the board answered; use
//! [`power_on_unchecked`] to drive the lines open-loop without a probe.
//!
//! Steps 2–5 ride the I2C control plane; step 6 the SPI data plane — but all are
//! methods on the same [`Pwbimu`], and all fallible ones share one error type
//! ([`Error`]). The PSoC emits each sample as IEEE-754 floats already scaled to
//! g / dps, so no conversion is needed host-side.
//!
//! [`enable`]: Pwbimu::enable
//! [`disable`]: Pwbimu::disable
//! [`power_on`]: Pwbimu::power_on
//! [`power_on_unchecked`]: Pwbimu::power_on_unchecked
//!
//! # The board is powered off until you sequence it
//!
//! You cannot just open the I2C bus and read a register: the board's power rail
//! and reset (`XRST`) are gated off out of reset. The PSoCs only answer on I2C
//! after the power → reset-pulse → 150 ms-settle sequence performed by
//! [`Pwbimu::power_on`] (mirroring the NuttX implementation `cxd5602pwbimu_open`). Call it once
//! after construction, before any register access.
//!
//! # Identity / `whoami`
//!
//! There is no register literally named `WHOAMI`; the NuttX driver uses
//! [`reg::FW_VER`] (`0x10`) as the presence/identity probe. A successful read
//! returning a plausible firmware-version byte means the primary PSoC at I2C
//! `0x10` answered — that is the `whoami` succeeding. See [`Pwbimu::whoami`].
//!
//! # Spresense direct-connection wiring
//!
//! When the add-on is plugged straight into a Spresense main board, the signals
//! land on these pads — this is the wiring [`spresense`] consumes. A custom PCB
//! is free to route them differently and build a [`Pwbimu`] from whatever buses
//! and pins it exposes.
//!
//! | Signal           | CXD5602 pad   | Arduino | CXD56 HAL pin              |
//! |------------------|---------------|---------|----------------------------|
//! | Power enable     | `EMMC_DATA2`  | D20     | `gp_emmc_data2`            |
//! | Reset (`XRST`)   | `I2S0_BCK`    | D26     | `gp_i2s0_bck`             |
//! | I2C SCL          | `I2C0_BCK`    | D15     | `gp_i2c0_bck` (via the bus) |
//! | I2C SDA          | `I2C0_BDT`    | D14     | `gp_i2c0_bdt` (via the bus) |
//! | SPI SCK          | `EMMC_CLK`    | D23     | (via the SPI5 bus)         |
//! | SPI MOSI         | `EMMC_DATA0`  | D16     | (via the SPI5 bus)         |
//! | SPI MISO         | `EMMC_DATA1`  | D17     | (via the SPI5 bus)         |
//! | SPI chip-select  | `I2S0_DATA_IN`| D19     | `gp_i2s0_data_in`          |
//! | Data ready (DRDY)| `EMMC_DATA3`  | D21     | `gp_emmc_data3`            |
//!
//! Note the IMU's chip-select is a plain **GPIO** (driven by [`Pwbimu`] around
//! each sample read), *not* the SPI peripheral's hardware frame-select — on
//! Spresense the PL022 frame-select on `EMMC_CMD` is unused by the add-on.

use core::marker::PhantomData;

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::{InputPin, OutputPin};
use embedded_hal::i2c::I2c;
use embedded_hal::spi::SpiBus;

/// Identity / control registers read over I2C (NuttX `cxd5602pwbimu` register map).
pub mod reg {
    /// Firmware version — the de-facto `whoami` (the driver's presence probe).
    pub const FW_VER: u8 = 0x10;
    /// Hardware revision.
    pub const HW_REVISION: u8 = 0x11;
    /// Hardware unique ID.
    pub const HW_UNIQUE_ID: u8 = 0x12;
    /// Full-scale range: accel in the high nibble, gyro in the low nibble.
    pub const FSR: u8 = 0x13;
    /// Output data rate (sample frequency).
    pub const ODR: u8 = 0x14;
    /// Data-ready interrupt enable — write 1 so the PSoC drives the DRDY line.
    pub const INTR_ENABLE: u8 = 0x15;
    /// 6-axis output enable: 1 = start streaming, 0 = stop.
    pub const OUTPUT_ENABLE: u8 = 0x18;
}

/// Default I2C address of the primary PSoC (`slaveid` 0).
pub const ADDR_PRIMARY: u8 = 0x10;
/// Base I2C address of the secondary PSoC bank (DIP-switch selectable).
pub const ADDR_SECONDARY: u8 = 0x30;

/// Power-rail settle after asserting the power pin (NuttX `up_mdelay(2)` in the
/// board `power()` callback).
const POWER_SETTLE_MS: u32 = 2;
/// Reset (`XRST`) low-pulse width (NuttX `up_udelay(20)`).
const RESET_PULSE_US: u32 = 20;
/// PSoC firmware boot time after reset release (NuttX `up_mdelay(150)`).
const BOOT_SETTLE_MS: u32 = 150;

/// Chip-select edge guard around an SPI sample read (NuttX board `csx`
/// callback: `up_udelay(5)` on each edge).
const CSX_GUARD_US: u32 = 5;

/// Bytes per SPI sample — `sizeof(cxd5602pwbimu_data_t)`: `timestamp` (u32) +
/// `temp` + `gx,gy,gz` + `ax,ay,az` (7 × f32).
pub const SAMPLE_BYTES: usize = 32;

/// SPI read flag — high bit set in the first transmitted byte (NuttX `recv`).
const SPI_READ_FLAG: u8 = 0x80;

/// Output data rate / sample frequency, written to [`reg::ODR`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Odr {
    Hz15,
    Hz30,
    Hz60,
    Hz120,
    Hz240,
    Hz480,
    Hz960,
    Hz1920,
}

impl Odr {
    /// Register encoding (`ODR_*`, 0x00–0x07).
    pub fn code(self) -> u8 {
        self as u8
    }

    /// The nominal sample rate in Hz.
    pub fn hz(self) -> u32 {
        match self {
            Odr::Hz15 => 15,
            Odr::Hz30 => 30,
            Odr::Hz60 => 60,
            Odr::Hz120 => 120,
            Odr::Hz240 => 240,
            Odr::Hz480 => 480,
            Odr::Hz960 => 960,
            Odr::Hz1920 => 1920,
        }
    }
}

/// Accelerometer full-scale range (±g). High nibble of [`reg::FSR`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelRange {
    G2,
    G4,
    G8,
    G16,
}

impl AccelRange {
    /// Register encoding (`FSR_ACCEL_*`, pre-shift, 0x00–0x03).
    pub fn code(self) -> u8 {
        self as u8
    }
}

/// Gyroscope full-scale range (±dps). Low nibble of [`reg::FSR`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GyroRange {
    Dps125,
    Dps250,
    Dps500,
    Dps1000,
    Dps2000,
    Dps4000,
}

impl GyroRange {
    /// Register encoding (`FSR_GYRO_*`, 0x00–0x05).
    pub fn code(self) -> u8 {
        self as u8
    }
}

/// [`Pwbimu::whoami`] — the three identity registers returned from i2c whoami
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// [`reg::FW_VER`] — firmware version.
    pub fw_ver: u8,
    /// [`reg::HW_REVISION`] — hardware revision.
    pub hw_rev: u8,
    /// [`reg::HW_UNIQUE_ID`] — hardware unique ID.
    pub hw_uid: u8,
}

/// Error from a [`Pwbimu`] operation, tagged by which resource produced it.
///
/// Control-plane methods ([`whoami`](Pwbimu::whoami),
/// [`configure`](Pwbimu::configure), [`enable`](Pwbimu::enable), …) yield
/// [`Error::I2c`]; the data-plane read [`read_sample`](Pwbimu::read_sample) yields
/// [`Error::Spi`] on a bus fault, [`Error::Pin`] on a chip-select / DRDY GPIO
/// fault, or [`Error::SampleNotReady`] when no sample is waiting. A single,
/// self-contained error type means the whole bring-up-to-stream flow can be
/// `?`-chained on one `Result` — with no `nb` in callers' signatures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<I2cErr, SpiErr, PinErr> {
    /// An error on the I2C control plane.
    I2c(I2cErr),
    /// An error on the SPI data plane.
    Spi(SpiErr),
    /// An error driving or reading one of the board's GPIOs (chip-select / DRDY).
    Pin(PinErr),
    /// No sample was ready: DRDY was not asserted when
    /// [`read_sample`](Pwbimu::read_sample) was called. Not a fault — the caller
    /// should retry (or wait). Kept distinct from a bus error so the two can be
    /// handled differently.
    SampleNotReady,
}

/// The [`Error`] type shared by every fallible [`Pwbimu`] method, written in
/// terms of the I2C bus `I`, the SPI bus `S`, and the shared GPIO error `E` (so
/// `BusError<I, S, E>` instead of the spelled-out `Error<I::Error, S::Error, E>`).
pub type BusError<I, S, E> = Error<
    <I as embedded_hal::i2c::ErrorType>::Error,
    <S as embedded_hal::spi::ErrorType>::Error,
    E,
>;

/// Returned by [`Pwbimu::power_on`] when the board does not answer the identity
/// probe after the power-up sequence.
///
/// The board has been powered back down to [`Off`] (rail off, reset asserted), so
/// [`imu`](Self::imu) is ready to retry [`power_on`](Pwbimu::power_on) — e.g.
/// after re-seating the add-on — or to drop. [`source`](Self::source) is the
/// underlying bus error from the failed identity read.
pub struct NotResponding<I: I2c, S: SpiBus, PWR, RST, CSX, DRDY, E> {
    /// The handle, powered back down to [`Off`] and ready to retry.
    pub imu: Pwbimu<I, S, PWR, RST, CSX, DRDY, Off>,
    /// The bus error from the failed identity read ([`reg::FW_VER`]).
    pub source: BusError<I, S, E>,
}

/// Lifecycle states for [`Pwbimu`]'s typestate parameter — see [`state`].
mod sealed {
    pub trait State {}
}

/// The board lifecycle as zero-sized typestate markers.
///
/// [`Pwbimu`]'s last type parameter is one of these, and each state exposes
/// only the methods legal in it, so the bring-up order is enforced at compile
/// time: you cannot read a sample before [`enable`](super::Pwbimu::enable), or
/// configure before [`power_on`](super::Pwbimu::power_on). The markers are
/// zero-sized and carried as [`PhantomData`](core::marker::PhantomData), so they
/// cost nothing at runtime.
///
/// ```text
///   new ─▶ Off ─power_on()─▶ Idle ─enable()─▶ Streaming
///                             ▲                   │
///                             └─── disable() ─────┘
///   (power_off() returns any state to Off)
/// ```
pub mod state {
    /// Just constructed: power rail off, `XRST` asserted. Only
    /// [`power_on`](super::Pwbimu::power_on) (and [`power_off`](super::Pwbimu::power_off)) exist.
    pub enum Off {}
    /// Powered and booted, output stopped: identity / [`configure`](super::Pwbimu::configure)
    /// available, plus [`enable`](super::Pwbimu::enable) to start streaming.
    pub enum Idle {}
    /// Output running: [`read_sample`](super::Pwbimu::read_sample) /
    /// [`read_sample_blocking`](super::Pwbimu::read_sample_blocking) available, plus
    /// [`disable`](super::Pwbimu::disable) to stop.
    pub enum Streaming {}

    impl super::sealed::State for Off {}
    impl super::sealed::State for Idle {}
    impl super::sealed::State for Streaming {}
}

pub use state::{Idle, Off, Streaming};

/// CXD5602PWBIMU board handle — both buses in one object, with the board
/// lifecycle tracked in the type system.
///
/// Owns everything the add-on board needs, and is generic over all of it: the
/// I2C bus `I` (any [`embedded_hal::i2c::I2c`] master) for the control plane, the
/// SPI bus `S` (any [`embedded_hal::spi::SpiBus`]) for the data plane, and the
/// board's four control GPIOs — `PWR`, `RST`, `CSX`
/// ([`OutputPin`](embedded_hal::digital::OutputPin)) and `DRDY`
/// ([`InputPin`](embedded_hal::digital::InputPin)). Because it is written against
/// the `embedded-hal` traits only, it has no dependency on the CXD56 HAL or on a
/// fixed pad map; on a Spresense the buses/pins are typically I2C0/SPI5 and the
/// add-on pins, built via [`spresense`] / [`SpresensePwbimu`], but any other
/// routing works just as well.
///
/// The `St` type parameter is a [`state`] marker that gates which methods exist;
/// the lifecycle is `new` → [`Off`] → [`power_on`](Self::power_on) → [`Idle`] →
/// [`enable`](Self::enable) → [`Streaming`]. The state transitions **consume**
/// `self` and return the handle in its new state, so the wrong-order call (read
/// before enable, configure before power-on) is a compile error rather than a
/// runtime hang or I2C NACK. All fallible methods share the [`Error`] type.
pub struct Pwbimu<I, S, PWR, RST, CSX, DRDY, St> {
    i2c: I,
    spi: S,
    power: PWR,
    reset: RST,
    csx: CSX,
    drdy: DRDY,
    addr: u8,
    _state: PhantomData<St>,
}

/// The buses and (already-configured) GPIOs a [`Pwbimu`] takes ownership of.
///
/// The pins must already be set to the right *mode*: `power`, `reset` and `csx`
/// as outputs, `drdy` as an input (with a pull-down, since DRDY is active-high).
/// `embedded-hal`'s pin traits have no notion of pull configuration, so that —
/// like the choice of which physical pads to use — is the caller's job;
/// [`Pwbimu::new`] then drives the outputs to the safe [`Off`] levels. On
/// Spresense, [`spresense`] does all of this for you from raw HAL pins.
pub struct PwbImuParts<I, S, PWR, RST, CSX, DRDY> {
    pub i2c: I,
    pub spi: S,
    pub power: PWR,
    pub reset: RST,
    pub csx: CSX,
    pub drdy: DRDY,
}

// --- state-agnostic: construction-independent accessors + power-down ---------

impl<I, S, PWR, RST, CSX, DRDY, St: sealed::State> Pwbimu<I, S, PWR, RST, CSX, DRDY, St> {
    /// Move the handle (all owned buses and pins) into a different typestate.
    fn into_state<New: sealed::State>(self) -> Pwbimu<I, S, PWR, RST, CSX, DRDY, New> {
        Pwbimu {
            i2c: self.i2c,
            spi: self.spi,
            power: self.power,
            reset: self.reset,
            csx: self.csx,
            drdy: self.drdy,
            addr: self.addr,
            _state: PhantomData,
        }
    }

    /// Select the PSoC I2C slave address (e.g. [`ADDR_PRIMARY`] / [`ADDR_SECONDARY`]).
    ///
    /// Available in any state (it only changes which slave subsequent control
    /// writes target); set it before [`configure`](Self::configure).
    pub fn with_address(mut self, addr: u8) -> Self {
        self.addr = addr;
        self
    }

    /// The currently selected I2C slave address.
    pub fn address(&self) -> u8 {
        self.addr
    }

    /// Borrow the underlying I2C bus.
    ///
    /// An escape hatch for control-plane registers not wrapped here. It bypasses
    /// the typestate, so prefer the dedicated methods — a raw write to
    /// [`reg::OUTPUT_ENABLE`] here will desync the tracked state.
    pub fn i2c(&mut self) -> &mut I {
        &mut self.i2c
    }

    /// Borrow the underlying SPI bus. Like [`i2c`](Self::i2c), an escape hatch
    /// that bypasses the typestate.
    pub fn spi(&mut self) -> &mut S {
        &mut self.spi
    }
}

impl<I, S, PWR, RST, CSX, DRDY, St: sealed::State> Pwbimu<I, S, PWR, RST, CSX, DRDY, St>
where
    PWR: OutputPin,
    RST: OutputPin,
{
    /// Cut the power rail and assert reset, returning the handle to [`Off`].
    ///
    /// Drives `XRST` low (reset asserted) then the power pin low. From here you
    /// can re-sequence with [`power_on`](Self::power_on). Available in any state
    /// — including [`Streaming`], where it is the hard way to stop (it kills the
    /// rail rather than clearing `OUTPUT_ENABLE` like [`disable`](Self::disable)).
    ///
    /// Powering down is best-effort: a GPIO write fault here is ignored, since
    /// this also runs on the error/cleanup path and there is nothing useful to do
    /// about it.
    pub fn power_off(mut self) -> Pwbimu<I, S, PWR, RST, CSX, DRDY, Off> {
        let _ = self.reset.set_low();
        let _ = self.power.set_low();
        self.into_state()
    }
}

// --- Off: construction + power-up --------------------------------------------

impl<I, S, PWR, RST, CSX, DRDY> Pwbimu<I, S, PWR, RST, CSX, DRDY, Off>
where
    PWR: OutputPin,
    RST: OutputPin,
    CSX: OutputPin,
{
    /// Consume both buses and the four board GPIOs and configure the board in its
    /// powered-off, reset-asserted, stream-idle state.
    ///
    /// The power pin is driven **low** (rail off) and `XRST` **low** (reset
    /// asserted, since it is active-low); the SPI chip-select is driven **high**
    /// (deasserted, also active-low). DRDY is taken as-is — the caller is
    /// responsible for having configured it as a pull-down input (active-high
    /// data-ready, matching the NuttX board config). Nothing answers on either
    /// bus yet; call [`power_on`](Self::power_on) to sequence the board up.
    /// Defaults to the primary PSoC address ([`ADDR_PRIMARY`]); use
    /// [`with_address`](Self::with_address) to select another.
    ///
    /// The SPI bus must already be configured for `MODE_0`, 8 bits, 1 MHz.
    /// Driving the outputs to their idle levels is best-effort; a GPIO write
    /// fault is ignored here (the subsequent [`power_on`](Self::power_on) probe
    /// will surface a genuinely dead pin).
    pub fn new(parts: PwbImuParts<I, S, PWR, RST, CSX, DRDY>) -> Self {
        let mut this = Self {
            i2c: parts.i2c,
            spi: parts.spi,
            power: parts.power,
            reset: parts.reset,
            csx: parts.csx,
            drdy: parts.drdy,
            addr: ADDR_PRIMARY,
            _state: PhantomData,
        };
        let _ = this.power.set_low();
        let _ = this.reset.set_low();
        let _ = this.csx.set_high();
        this
    }

    /// Sequence the board up **without** confirming it answered, advancing the
    /// handle from [`Off`] to [`Idle`].
    ///
    /// Mirrors NuttX `cxd5602pwbimu_open`: drive power **high** and let the rail
    /// settle (2 ms), pulse `XRST` low for 20 µs, then release it high and wait
    /// 150 ms for the PSoC firmware to boot.
    ///
    /// This is open-loop — it only drives GPIOs and delays, so it cannot fail
    /// (GPIO write faults are ignored), but the resulting [`Idle`] is an
    /// *unverified* claim: nothing has checked that a board is actually present
    /// and responding. Prefer [`power_on`](Self::power_on), which runs this same
    /// sequence and then probes the identity register, so reaching [`Idle`]
    /// proves the PSoC answered. Use this only when you deliberately want to drive
    /// the lines without a bus probe (e.g. bring-up testing). To re-sequence a
    /// board that is already up, [`power_off`](Self::power_off) it back to
    /// [`Off`] first.
    pub fn power_on_unchecked<D: DelayNs>(
        mut self,
        delay: &mut D,
    ) -> Pwbimu<I, S, PWR, RST, CSX, DRDY, Idle> {
        // 1. Enable the power rail and let it settle.
        let _ = self.power.set_high();
        delay.delay_ms(POWER_SETTLE_MS);
        // 2. Assert reset (XRST active-low) for the reset pulse width.
        let _ = self.reset.set_low();
        delay.delay_us(RESET_PULSE_US);
        // 3. Release reset and wait for the PSoC firmware to boot.
        let _ = self.reset.set_high();
        delay.delay_ms(BOOT_SETTLE_MS);
        self.into_state()
    }
}

// --- Off (with buses): checked power-up --------------------------------------

impl<I, S, PWR, RST, CSX, DRDY, E> Pwbimu<I, S, PWR, RST, CSX, DRDY, Off>
where
    I: I2c,
    S: SpiBus,
    PWR: OutputPin<Error = E>,
    RST: OutputPin<Error = E>,
    CSX: OutputPin<Error = E>,
    DRDY: InputPin<Error = E>,
{
    /// Sequence the board up and confirm it responds, advancing the handle from
    /// [`Off`] to [`Idle`].
    ///
    /// Runs [`power_on_unchecked`](Self::power_on_unchecked), then reads the
    /// firmware-version register ([`reg::FW_VER`] — NuttX's presence probe). On
    /// success the board is in [`Idle`], and unlike `power_on_unchecked` that
    /// state is *earned*: the PSoC answered at least once, so an absent or
    /// mis-seated board is caught right here with a clear error instead of
    /// surfacing as a confusing failure several calls later in
    /// [`configure`](Pwbimu::configure).
    ///
    /// On a probe failure the board is powered back down to [`Off`] and handed
    /// back inside [`NotResponding`], ready to retry (e.g. after re-seating the
    /// add-on). Note this proves the board was alive *at power-on*, not
    /// permanently — every later control/data call still returns a [`Result`]
    /// that surfaces a board which has since died or been unplugged.
    // Six hardware generics threaded through both arms of the `Result` trip the
    // lint; the shape is unavoidable for a fully bus/pin-generic transition.
    #[allow(clippy::type_complexity)]
    pub fn power_on<D: DelayNs>(
        self,
        delay: &mut D,
    ) -> Result<
        Pwbimu<I, S, PWR, RST, CSX, DRDY, Idle>,
        NotResponding<I, S, PWR, RST, CSX, DRDY, E>,
    > {
        let mut imu = self.power_on_unchecked(delay);
        match imu.fw_version() {
            Ok(_) => Ok(imu),
            Err(source) => Err(NotResponding {
                imu: imu.power_off(),
                source,
            }),
        }
    }
}

// --- Idle: control plane (identity + configuration) --------------------------

impl<I, S, PWR, RST, CSX, DRDY, E> Pwbimu<I, S, PWR, RST, CSX, DRDY, Idle>
where
    I: I2c,
    S: SpiBus,
    DRDY: InputPin<Error = E>,
{
    /// Read `buf.len()` bytes starting at register `reg` (NuttX `getregsn`).
    ///
    /// Issues the standard register-pointer-then-read transaction: write the
    /// 1-byte register address with a repeated start (no STOP), then read `buf`.
    pub fn read_regs(&mut self, reg: u8, buf: &mut [u8]) -> Result<(), BusError<I, S, E>> {
        self.i2c
            .write_read(self.addr, &[reg], buf)
            .map_err(Error::I2c)
    }

    /// Read a single register.
    pub fn read_reg(&mut self, reg: u8) -> Result<u8, BusError<I, S, E>> {
        let mut buf = [0u8];
        self.read_regs(reg, &mut buf)?;
        Ok(buf[0])
    }

    /// Read [`reg::FW_VER`] — the firmware version / de-facto `whoami` byte.
    pub fn fw_version(&mut self) -> Result<u8, BusError<I, S, E>> {
        self.read_reg(reg::FW_VER)
    }

    /// Read [`reg::HW_REVISION`].
    pub fn hw_revision(&mut self) -> Result<u8, BusError<I, S, E>> {
        self.read_reg(reg::HW_REVISION)
    }

    /// Read [`reg::HW_UNIQUE_ID`].
    pub fn hw_unique_id(&mut self) -> Result<u8, BusError<I, S, E>> {
        self.read_reg(reg::HW_UNIQUE_ID)
    }

    /// Read all three identity registers in one call.
    ///
    /// A successful return (and a plausible `fw_ver`) means the PSoC at the
    /// selected [`address`](Self::address) answered — the `whoami` succeeded.
    pub fn whoami(&mut self) -> Result<Identity, BusError<I, S, E>> {
        Ok(Identity {
            fw_ver: self.read_reg(reg::FW_VER)?,
            hw_rev: self.read_reg(reg::HW_REVISION)?,
            hw_uid: self.read_reg(reg::HW_UNIQUE_ID)?,
        })
    }

    /// Write a single control register (NuttX `putreg8`): a 2-byte I2C write of
    /// `[reg, val]`.
    pub fn write_reg(&mut self, reg: u8, val: u8) -> Result<(), BusError<I, S, E>> {
        self.i2c.write(self.addr, &[reg, val]).map_err(Error::I2c)
    }

    /// Configure the board for sampling: enable the DRDY line and set the output
    /// data rate and full-scale ranges.
    ///
    /// Legal only in [`Idle`] — the PSoC accepts these writes only while the
    /// output is stopped (its `STATE_READY`), which the typestate guarantees.
    /// Writes [`reg::INTR_ENABLE`] = 1 (so DRDY pulses per sample), then
    /// [`reg::ODR`], then [`reg::FSR`] (`accel` in the high nibble, `gyro` in the
    /// low nibble). Stays in [`Idle`]; call [`enable`](Self::enable) to start.
    pub fn configure(
        &mut self,
        odr: Odr,
        accel: AccelRange,
        gyro: GyroRange,
    ) -> Result<(), BusError<I, S, E>> {
        self.write_reg(reg::INTR_ENABLE, 1)?;
        self.write_reg(reg::ODR, odr.code())?;
        self.write_reg(reg::FSR, (accel.code() << 4) | gyro.code())?;
        Ok(())
    }

    /// Start the 6-axis output stream ([`reg::OUTPUT_ENABLE`] = 1), advancing the
    /// handle from [`Idle`] to [`Streaming`].
    ///
    /// After this, DRDY pulses at the configured [`Odr`] and samples are read via
    /// [`read_sample`](Pwbimu::read_sample). On an I2C error the handle is
    /// consumed (not returned) — the board is left in an indeterminate state, so
    /// recover by reconstructing or power-cycling rather than retrying in place.
    #[allow(clippy::type_complexity)]
    pub fn enable(
        mut self,
    ) -> Result<Pwbimu<I, S, PWR, RST, CSX, DRDY, Streaming>, BusError<I, S, E>> {
        self.write_reg(reg::OUTPUT_ENABLE, 1)?;
        Ok(self.into_state())
    }
}

// --- Streaming: data plane ---------------------------------------------------

impl<I, S, PWR, RST, CSX, DRDY, E> Pwbimu<I, S, PWR, RST, CSX, DRDY, Streaming>
where
    I: I2c,
    S: SpiBus,
    CSX: OutputPin<Error = E>,
    DRDY: InputPin<Error = E>,
{
    /// Whether a fresh sample is ready (DRDY asserted high).
    ///
    /// A convenience predicate: a GPIO read fault is reported as "not ready"
    /// (`false`). The authoritative check is inside
    /// [`read_sample`](Self::read_sample), which surfaces a DRDY fault as
    /// [`Error::Pin`].
    pub fn data_ready(&mut self) -> bool {
        self.drdy.is_high().unwrap_or(false)
    }

    /// Read one sample over SPI if one is ready (NuttX `cxd5602pwbimu_recv`).
    ///
    /// **Non-blocking**: returns [`Error::SampleNotReady`] when DRDY is not
    /// asserted (no fresh sample), so the caller owns the wait policy. When a
    /// sample *is* ready it asserts the GPIO chip-select low (with the 5 µs setup
    /// guard), exchanges [`SAMPLE_BYTES`] bytes full-duplex — the first
    /// transmitted byte carries the read flag (`0x80`), and the bytes clocked back
    /// are the sample packet — then deasserts (with the 5 µs hold guard). `delay`
    /// supplies the chip-select edge guards. A bus fault surfaces as
    /// [`Error::Spi`]; a chip-select / DRDY GPIO fault as [`Error::Pin`].
    ///
    /// Because "not ready" and a real fault are distinct [`Error`] variants, a
    /// caller can poll and handle them differently — retry on not-ready, bail on
    /// a fault.
    ///
    /// ```ignore
    /// let sample = loop {
    ///     match imu.read_sample(&mut delay) {
    ///         Ok(s) => break s,
    ///         Err(Error::SampleNotReady) => {
    ///             // not ready yet: keep polling, sleep, do other work, or give
    ///             // up against your own deadline (returning your own timeout).
    ///             if deadline.expired() {
    ///                 return Err(MyError::Timeout);
    ///             }
    ///         }
    ///         Err(e) => return Err(e.into()),     // real bus fault — bail
    ///     }
    /// };
    /// ```
    ///
    /// To simply block until the next sample (no timeout), use
    /// [`read_sample_blocking`](Self::read_sample_blocking).
    pub fn read_sample<D: DelayNs>(
        &mut self,
        delay: &mut D,
    ) -> Result<ImuSample, BusError<I, S, E>> {
        if !self.drdy.is_high().map_err(Error::Pin)? {
            return Err(Error::SampleNotReady);
        }

        let mut buf = [0u8; SAMPLE_BYTES];
        buf[0] = SPI_READ_FLAG;

        self.csx.set_low().map_err(Error::Pin)?;
        delay.delay_us(CSX_GUARD_US);
        let xfer = self.spi.transfer_in_place(&mut buf);
        delay.delay_us(CSX_GUARD_US);
        let deassert = self.csx.set_high();
        xfer.map_err(Error::Spi)?;
        deassert.map_err(Error::Pin)?;

        Ok(ImuSample::from_bytes(&buf))
    }

    /// Block until a sample arrives, then return it.
    ///
    /// A convenience wrapper over the non-blocking
    /// [`read_sample`](Self::read_sample): it busy-waits for the PSoC to assert
    /// DRDY (paced by the configured [`Odr`]), retrying past every
    /// [`Error::SampleNotReady`]. It has **no timeout** — if DRDY never comes
    /// (e.g. a wedged board) it spins forever; when that matters, poll
    /// [`read_sample`](Self::read_sample) yourself against a deadline instead. A
    /// bus fault returns as-is. `delay` supplies the chip-select edge guards.
    pub fn read_sample_blocking<D: DelayNs>(
        &mut self,
        delay: &mut D,
    ) -> Result<ImuSample, BusError<I, S, E>> {
        loop {
            match self.read_sample(delay) {
                Err(Error::SampleNotReady) => continue,
                other => return other,
            }
        }
    }

    /// Stop the output stream ([`reg::OUTPUT_ENABLE`] = 0), returning the handle
    /// from [`Streaming`] to [`Idle`] (where it can be re-[`configure`](Pwbimu::configure)d).
    ///
    /// On an I2C error the handle is consumed (not returned); see
    /// [`enable`](Pwbimu::enable).
    #[allow(clippy::type_complexity)]
    pub fn disable(mut self) -> Result<Pwbimu<I, S, PWR, RST, CSX, DRDY, Idle>, BusError<I, S, E>> {
        self.i2c
            .write(self.addr, &[reg::OUTPUT_ENABLE, 0])
            .map_err(Error::I2c)?;
        Ok(self.into_state())
    }
}

/// One 6-axis sample, decoded from the 32-byte SPI packet.
///
/// The PSoC firmware emits the values as little-endian IEEE-754 floats already
/// scaled to physical units — accelerations in **g**, angular rates in
/// **dps** — so no host-side scaling against the configured [`AccelRange`] /
/// [`GyroRange`] is required.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImuSample {
    /// PSoC sample counter / timestamp.
    pub timestamp: u32,
    /// Die temperature (°C).
    pub temp: f32,
    /// Gyroscope `[x, y, z]` in dps.
    pub gyro: [f32; 3],
    /// Accelerometer `[x, y, z]` in g.
    pub accel: [f32; 3],
}

impl ImuSample {
    /// Decode a raw [`SAMPLE_BYTES`]-byte SPI packet (little-endian, field order
    /// matching NuttX `cxd5602pwbimu_data_s`: timestamp, temp, gyro, accel).
    pub fn from_bytes(b: &[u8; SAMPLE_BYTES]) -> Self {
        let f = |o: usize| f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        Self {
            timestamp: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            temp: f(4),
            gyro: [f(8), f(12), f(16)],
            accel: [f(20), f(24), f(28)],
        }
    }
}

// =============================================================================
// Spresense convenience: wire the add-on's direct connection to the CXD56 HAL.
// =============================================================================
//
// This is the BSP's job, not the driver's: it pins the generic `Pwbimu` to the
// concrete I2C0 / SPI5 buses and the fixed add-on GPIOs, and configures those
// pins (direction + DRDY pull-down) from raw HAL pins. Everything above is
// host-agnostic; only this section knows about the CXD56.

use cxd56_hal::gpio::{GpioPin, Input, Level, Output};
use cxd56_hal::i2c_alt::I2c as HalI2c;
use cxd56_hal::pac::topreg::{GpEmmcData2, GpEmmcData3, GpI2s0Bck, GpI2s0DataIn};
use cxd56_hal::pac::{I2c0, Spi5};
use cxd56_hal::spi_alt::Spi as HalSpi;

/// A [`Pwbimu`] wired to the Spresense add-on's direct connection: I2C0 + SPI5
/// and the fixed add-on GPIOs. Build one with [`spresense`].
pub type SpresensePwbimu<'clk, St> = Pwbimu<
    HalI2c<I2c0>,
    HalSpi<'clk, Spi5>,
    Output<GpEmmcData2>,
    Output<GpI2s0Bck>,
    Output<GpI2s0DataIn>,
    Input<GpEmmcData3>,
    St,
>;

/// Build a [`SpresensePwbimu`] from the CXD56 HAL's I2C0/SPI5 buses and the four
/// raw add-on GPIO pins, returning it in the [`Off`] state.
///
/// This is the convenience path for the common case — the add-on plugged
/// straight into a Spresense main board (see the wiring table in the
/// [module docs](crate::pwbimu)). It configures the pins for you: `power`,
/// `reset` and `csx` as outputs, `drdy` as a pull-down input. The arguments are
/// in pad order — power (`gp_emmc_data2`), reset (`gp_i2s0_bck`), chip-select
/// (`gp_i2s0_data_in`), DRDY (`gp_emmc_data3`) — and their distinct pin types
/// make a mis-ordering a compile error.
///
/// For any other routing (a custom PCB on, say, I2C1/SPI4) skip this and build a
/// [`Pwbimu`] directly with [`Pwbimu::new`].
pub fn spresense(
    i2c: HalI2c<I2c0>,
    spi: HalSpi<Spi5>,
    power: GpioPin<GpEmmcData2>,
    reset: GpioPin<GpI2s0Bck>,
    csx: GpioPin<GpI2s0DataIn>,
    drdy: GpioPin<GpEmmcData3>,
) -> SpresensePwbimu<Off> {
    Pwbimu::new(PwbImuParts {
        i2c,
        spi,
        power: power.into_output(Level::Low),
        reset: reset.into_output(Level::Low),
        csx: csx.into_output(Level::High),
        drdy: drdy.into_pull_down_input(),
    })
}
