//! Onboard GNSS — the CXD5602's positioning engine.
//!
//! # Architecture: firmware, not registers
//!
//! The GNSS baseband has no APP-core register interface. It is Sony's
//! `gnssfw` firmware running on the GPS CPU (CPU 1), loaded from SPI flash on
//! demand by the SYSIOP loader — the same `loader.espk` that already services
//! this HAL's PMIC/sleep Far API calls. Any board provisioned with Sony's
//! standard bootloader set has `gnssfw` in flash; this driver ships no
//! firmware and no C, only the wire protocol, ported from NuttX's open shim
//! (`arch/arm/src/cxd56xx/cxd56_gnss.c` and friends).
//!
//! Everything travels over the CPU-FIFO mailbox ([`crate::multicore::Mailbox`])
//! in two forms:
//!
//! * **Far API RPCs** ([`crate::farapi::call_to`]) — `fw_pm_*` calls to the
//!   SYSIOP (load/boot/sleep the GPS CPU) and `fw_gd_*` calls to the GNSS
//!   firmware itself (configuration and data readout).
//! * **Signal messages** — fire-and-forget words in both directions: the
//!   firmware notifies boot completion / data ready / backup-data requests,
//!   and the driver sends start/stop commands and backup-request replies.
//!
//! # Driver shape
//!
//! [`Gnss`] is a typestate singleton: configuration RPCs exist only on
//! `Gnss<Idle>` (the firmware requires idle mode for them) and data readout
//! only on `Gnss<Running>`. All waits are blocking polls, like the rest of
//! this HAL's inter-core code; do not issue other CPU-FIFO traffic (Far API
//! calls from other drivers, [`crate::clocks::pm::request_perf`])
//! concurrently with GNSS operations.
//!
//! ```ignore
//! let gnss = Gnss::boot()?;                       // load + boot gnssfw
//! let mut gnss = gnss.start(StartMode::Hot).map_err(|(_, e)| e)?;
//! let mut pos = PositionData::zeroed();
//! loop {
//!     gnss.wait_update(2_000)?;                   // one positioning epoch
//!     gnss.read_position(&mut pos)?;
//! }
//! ```

pub mod types;

pub use types::{
    Date, Dop, MAX_SV_NUM, OperationMode, PositionData, Receiver, SatelliteSystems, StartMode,
    Sv, SvPos, SvVel, Time, Var,
};

use thiserror::Error;

/// Error from the GNSS driver.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Error)]
pub enum GnssError {
    /// The GNSS singleton is already taken ([`Gnss::boot`] called while a
    /// previous instance is alive).
    #[error("GNSS already taken")]
    Taken,
    /// The RTC is not running — the GNSS firmware requires a stable RTC.
    #[error("RTC not running")]
    RtcNotReady,
    /// A Far API round-trip timed out (module missing from the loaded
    /// firmware, or the GPS CPU stopped responding).
    #[error("Far API transport timeout")]
    Timeout,
    /// `gnssfw` booted but never signalled boot completion.
    #[error("gnssfw boot did not signal BOOTCOMP")]
    BootTimeout,
    /// The firmware returned an error (a negated errno value).
    #[error("firmware returned {0}")]
    Firmware(i32),
    /// No position notification arrived within the caller's timeout.
    #[error("timed out waiting for a position update")]
    UpdateTimeout,
}
