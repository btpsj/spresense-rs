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
//! only on `Gnss<Running>`. [`Gnss::boot`] takes `&'clk Clock<Hp>` and the
//! driver holds that borrow across every typestate until [`Gnss::shutdown`]:
//! booting `gnssfw` wakes the GNSS domain and the SYSIOP re-arbitrates the
//! shared clock tree, so the tree must already be parked at the HV point the
//! firmware drives it to (hardware-validated: a console built beforehand
//! survives boot untouched) — and while the firmware is live, an
//! APP-initiated operating-point change is unvalidated freqlock arbitration,
//! which the borrow turns into a compile error (`into_hp`/`into_lp` need
//! ownership of the `Clock`). All waits are blocking polls, like the rest of
//! this HAL's inter-core code; do not issue other CPU-FIFO traffic (Far API
//! calls from other drivers, raw PM traffic via
//! [`PerfControl`](crate::clocks::PerfControl) — which inherits this
//! contract by discipline) concurrently with GNSS operations. Dropping a
//! `Gnss` without [`Gnss::shutdown`] leaves the GPS CPU running and the
//! singleton taken, and releases the `Clock` borrow while the firmware may
//! still run — the Hp contract then continues by discipline only.
//!
//! # Panics
//!
//! Every Far API issued here **panics on transport timeout** instead of
//! returning an error, and a signal push that stays jammed past its spin
//! budget panics the same way. A timeout means we stopped waiting, not that the
//! firmware did: it may still read the request block and argument buffer —
//! both pointing into stack frames that are dead once we unwind — so the
//! transport is poisoned and no safe recovery exists. This never signals
//! "GNSS unavailable": a board without `gnssfw` in flash fails cleanly with
//! [`GnssError::Firmware`] from the load step, because the SYSIOP loader
//! itself always answers.
//!
//! ```ignore
//! let mut gnss = Gnss::boot(clock)?;              // clock: &Clock<Hp>
//! gnss.select_systems(SatelliteSystems::new(GpsFamily::GPS, Secondary::Glonass))?;
//! let mut gnss = gnss.start(StartMode::Hot).map_err(|(_, e)| e)?;
//! let mut pos = PositionData::zeroed();
//! loop {
//!     gnss.wait_update(2_000)?;                   // one positioning epoch
//!     gnss.read_position(&mut pos)?;
//! }
//! ```

pub mod types;

pub use types::{
    Date, Dop, GpsFamily, MAX_SV_NUM, OperationMode, PositionData, Receiver, SatelliteSystems,
    Secondary, StartMode, Sv, SvPos, SvVel, Time, Var,
};

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, Ordering};

use thiserror::Error;

use crate::clocks::{Clock, Hp};
use crate::farapi::{self, FarapiError};
use crate::multicore::{Mailbox, mailbox};
use crate::pac;

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
    /// `gnssfw` booted but never signalled boot completion.
    #[error("gnssfw boot did not signal BOOTCOMP")]
    BootTimeout,
    /// The firmware returned an error (a negated errno value). From
    /// [`Gnss::boot`], `-2` (`ENOENT`) means `gnssfw` is not in flash.
    #[error("firmware returned {0}")]
    Firmware(i32),
    /// A start/stop command was not acknowledged in time.
    #[error("start/stop command not acknowledged")]
    AckTimeout,
    /// No position notification arrived within the caller's timeout.
    #[error("timed out waiting for a position update")]
    UpdateTimeout,
}

// --- Far API routing ---------------------------------------------------------
//
// `modid` is the index of the module's `_modulelist_*` entry in
// `cxd56_farapistub.S`'s `.modulelist` section (file order): power_mgr=0,
// flash_mgr=1, rtc_mgr=2, gnss_pwr=3, aca=4, pinconfig=5, uart=6,
// update_mgr=7, gnss=8, gnss_geofence=9, gnss_pvtlog=10. The aca=4 and
// power_mgr=0 anchors are already proven on hardware by `audio_aca`/`pmic`.
//
// `api_id` is `slot * 4 + 4`: the stubs are 4-byte entries and Thumb
// `mov r12, pc` reads its own address + 4, from which the shared trampoline
// subtracts the table base. Hardware-proven anchors: `fw_pm_pmiccontrol`
// slot 26 → 108 (`pmic`), `fw_pm_coldsleep`/`fw_pm_deepsleep` slots 19/20 →
// 80/84 (`sleep`).

/// The GPS CPU — `cpuno` of the `gnss` module descriptor, `CXD56_GNSS_GPS_CPUID`.
const CPU_GPS: u32 = 1;

const MODID_PM: i32 = 0;
const MODID_GNSS: i32 = 8;

/// `fw_pm_startcpu(cpuid, wait)` — slot 15.
const API_PM_STARTCPU: i32 = 64;
/// `fw_pm_sleepcpu(cpuid, mode)` — slot 17.
const API_PM_SLEEPCPU: i32 = 72;
/// `fw_pm_loadimage(cpuid, filename)` — slot 22.
const API_PM_LOADIMAGE: i32 = 92;

/// `fw_gd_selectsatellitesystem(system)` — slot 2.
const API_GD_SELECTSATELLITESYSTEM: i32 = 12;
/// `fw_gd_getsatellitesystem(*system)` — slot 3.
const API_GD_GETSATELLITESYSTEM: i32 = 16;
/// `fw_gd_setoperationmode(mode, cycle)` — slot 6.
const API_GD_SETOPERATIONMODE: i32 = 28;
/// `fw_gd_getoperationmode(*mode, *cycle)` — slot 7.
const API_GD_GETOPERATIONMODE: i32 = 32;
/// `fw_gd_readbuffer(type, offset, buf, len)` — slot 35; returns copied size.
const API_GD_READBUFFER: i32 = 144;
/// `fw_gd_writebuffer(type, offset, buf, len)` — slot 36.
const API_GD_WRITEBUFFER: i32 = 148;
/// `fw_gd_setnotifymask(type, clear)` — slot 37.
const API_GD_SETNOTIFYMASK: i32 = 152;

/// GPS-CPU sleep modes (`PM_SLEEP_MODE_*`, `cxd56_gnss.c:94`).
const PM_SLEEP_COLD: u32 = 2;
const PM_SLEEP_HOT_ENABLE: u32 = 7;
const PM_SLEEP_HOT_DISABLE: u32 = 8;

/// The image name `fw_pm_loadimage` looks up in SPI flash.
static FW_NAME: [u8; 7] = *b"gnssfw\0";

/// Far API poll budget for `fw_pm_loadimage`: the SYSIOP streams the whole
/// ~450 KiB image out of SPI flash before completing, so this needs to be far
/// beyond [`farapi::DEFAULT_POLL_BUDGET`].
const LOADIMAGE_POLL_BUDGET: u32 = 50_000_000;

/// Spin budget for pushing one signal message while draining inbound traffic.
/// Far beyond any observed jam; expiring means the mailbox is dead.
const SIG_PUSH_BUDGET: u32 = 50_000_000;

/// `gnssfw` boot-completion window, mirroring NuttX's 5 s semaphore wait.
const BOOT_TIMEOUT_MS: u32 = 5_000;
/// Start/stop command acknowledge window.
const ACK_TIMEOUT_MS: u32 = 5_000;

/// Minimum `BOOTCOMP`-to-`START` settle, enforced by [`Gnss::start`].
///
/// Hardware-measured (2026-07-17): the firmware keeps initializing for
/// ~0.5–1 s *after* it signals `BOOTCOMP`. A `START` arriving inside that
/// window is refused with `-60` (the firmware's own timeout code) without
/// ever requesting CEP data — and the refusal is sticky: every later
/// `START` in the same firmware boot fails identically, so retrying cannot
/// recover it; only a firmware re-boot clears the state. START ≥ 1 s after
/// `BOOTCOMP` is granted instantly (measured pass/fail brackets: 0.5 s
/// fails, 1.0 s passes). 2 s is twice the worst observed pass threshold.
/// NuttX applications never trip this because their userland overhead
/// between `open()` and the start ioctl exceeds it naturally.
const START_SETTLE_MS: u32 = 2_000;

/// PMIC load-switch channel for the TCXO + GNSS LNA rail — `PMIC_LSW(4)`
/// (`board.h`: `POWER_TCXO`/`POWER_LNA`), `PMIC_GET_CH` extracts `1 << 4`.
///
/// This rail is **shared with the TCXO**, the running 26 MHz system reference
/// on the Spresense main board (`cxd56_power.c`: `g_used_tcxo = true`).
/// Powering it off kills every clock in the SoC mid-instruction (verified on
/// hardware). It must only ever be switched ON here; NuttX's
/// `board_lna_power_control(false)` likewise refuses to drop it while the
/// TCXO is in use.
const LSW_TCXO_LNA: u8 = 1 << 4;

/// `-ENOENT`, the value NuttX's backup/CEP restore paths report when there is
/// no stored data (we never have any: no filesystem).
const ENOENT: i32 = 2;

// --- Signal-message protocol (pure helpers, no I/O) --------------------------

mod proto {
    //! Wire format of the non-RPC signal traffic, mirrored from
    //! `cxd56_icc.c` (`struct iccmsg_msg_s`) and `cxd56_cpu1signal.h`:
    //!
    //! ```text
    //! word0: [31:28] cpuid  [27:24] proto  [23:16] msgid  [15:0] pdata
    //! word1: data
    //! ```
    //!
    //! Firmware→app notifications arrive with protocol `MSG` (0) — NuttX
    //! routes those to the per-CPU queue its `cxd56cpu1_worker` reads — and
    //! carry `dev = data & 0xff`, `value = data >> 8` in **word1**
    //! (`CXD56_CPU1_GET_DEV`/`GET_DATA`). App→firmware commands go out with
    //! protocol `GNSS` (13), the sigtype in word0's msgid byte
    //! (`cxd56_cpu1sigsend`), and the payload in word1.

    /// `CXD56_PROTO_MSG` — inbound notifications.
    const PROTO_MSG: u32 = 0;
    /// `CXD56_PROTO_GNSS` — outbound signal commands.
    const PROTO_GNSS: u32 = 13;

    // `CXD56_CPU1_DATA_TYPE_*` (dev byte / outbound sigtype).
    pub(super) const DEV_GNSS: u32 = 0;
    pub(super) const DEV_INFO: u32 = 6;
    pub(super) const DEV_CEP: u32 = 8;
    pub(super) const DEV_BKUPFILE: u32 = 10;
    pub(super) const DEV_CPUFIFOAPI: u32 = 13;

    // `CXD56_GNSS_NOTIFY_TYPE_*` (value when dev == DEV_GNSS).
    const NOTIFY_POSITION: i32 = 0;
    const NOTIFY_BOOTCOMP: i32 = 1;
    const NOTIFY_REQBKUPDAT: i32 = 2;
    const NOTIFY_REQCEPOPEN: i32 = 3;
    const NOTIFY_REQCEPCLOSE: i32 = 4;
    const NOTIFY_REQCEPDAT: i32 = 5;
    const NOTIFY_REQCEPBUFFREE: i32 = 6;

    // `CXD56_GNSS_GD_GNSS_*` (CPUFIFOAPI command byte; response value is the
    // firmware return code).
    pub(super) const CPUFIFOAPI_START: u32 = 0;
    pub(super) const CPUFIFOAPI_STOP: u32 = 1;

    /// A classified inbound CPU-FIFO message.
    #[derive(Copy, Clone, Debug)]
    pub(super) enum Event {
        /// One positioning epoch is ready (`NOTIFY_TYPE_POSITION`).
        Position,
        /// `gnssfw` finished initializing (`NOTIFY_TYPE_BOOTCOMP`).
        BootComp,
        /// The firmware asks for stored backup data (`NOTIFY_TYPE_REQBKUPDAT`).
        ReqBackupData,
        /// The firmware asks for CEP assist data (`NOTIFY_TYPE_REQCEPDAT`).
        ReqCepData,
        /// CEP file housekeeping (open/close/buffer-free) — nothing to do
        /// without a filesystem.
        CepHousekeeping,
        /// Acknowledge of a `CPUFIFOAPI` start/stop command, carrying the
        /// firmware return code.
        CpuFifoApiRet(i32),
        /// Anything else (stray traffic, other protocols).
        Other,
    }

    /// Classify an inbound message. Only protocol-`MSG` words from the GPS
    /// CPU are GNSS notifications; everything else is [`Event::Other`].
    pub(super) fn classify(w: [u32; 2]) -> Event {
        let sender = (w[0] >> 28) & 0xf;
        let protocol = (w[0] >> 24) & 0xf;
        if protocol != PROTO_MSG || sender != super::CPU_GPS {
            return Event::Other;
        }
        let dev = w[1] & 0xff;
        let value = (w[1] as i32) >> 8;
        match dev {
            DEV_GNSS => match value {
                NOTIFY_POSITION => Event::Position,
                NOTIFY_BOOTCOMP => Event::BootComp,
                NOTIFY_REQBKUPDAT => Event::ReqBackupData,
                NOTIFY_REQCEPDAT => Event::ReqCepData,
                NOTIFY_REQCEPOPEN | NOTIFY_REQCEPCLOSE | NOTIFY_REQCEPBUFFREE => {
                    Event::CepHousekeeping
                }
                _ => Event::Other,
            },
            DEV_CPUFIFOAPI => Event::CpuFifoApiRet(value),
            _ => Event::Other,
        }
    }

    /// Build an outbound signal command (`cxd56_cpu1sigsend(sigtype, data)`).
    pub(super) fn sig_words(sigtype: u32, data: u32) -> [u32; 2] {
        [
            (super::CPU_GPS << 28) | (PROTO_GNSS << 24) | ((sigtype & 0xff) << 16),
            data,
        ]
    }
}

// --- Shared info block --------------------------------------------------------

/// NuttX `struct cxd56_gnss_shared_info_s`. [`Gnss::boot`] registers this
/// block's **address** with the firmware (`fw_gd_writebuffer(INFO, ..)`);
/// the firmware then reads `retval` across the bus after each backup/CEP
/// request reply, so it must live in immortal memory and be written volatile.
#[repr(C)]
struct SharedInfo {
    retval: i32,
    argc: u32,
    argv: [u32; 6],
}

const _: () = assert!(core::mem::size_of::<SharedInfo>() == 32);

struct SharedInfoCell(UnsafeCell<SharedInfo>);

// SAFETY: written only by the blocking singleton driver on this core; the
// other reader is the GNSS firmware, which is sequenced by the signal
// protocol (write + dmb before the reply signal is sent).
unsafe impl Sync for SharedInfoCell {}

static SHARED_INFO: SharedInfoCell = SharedInfoCell(UnsafeCell::new(SharedInfo {
    retval: 0,
    argc: 0,
    argv: [0; 6],
}));

fn shared_info_set_retval(v: i32) {
    // SAFETY: sole writer (see SharedInfoCell); volatile because the consumer
    // is another master.
    unsafe { (&raw mut (*SHARED_INFO.0.get()).retval).write_volatile(v) };
    cortex_m::asm::dmb();
}

// --- RTC-based deadlines ------------------------------------------------------

const RTC_HZ: u64 = 32_768;

/// Current monotonic RTC tick count (same double-read idiom as `time::rtc`).
fn rtc_now() -> u64 {
    // SAFETY: RTC0 is the always-on clock peripheral; counter reads only.
    let rtc = unsafe { &*pac::Rtc0::PTR };
    loop {
        let hi = rtc.rtpostcnt().read().bits();
        let lo = rtc.rtprecnt().read().bits() & 0x7fff;
        if hi == rtc.rtpostcnt().read().bits() {
            return ((hi as u64) << 15) | lo as u64;
        }
    }
}

fn deadline_after_ms(ms: u32) -> u64 {
    rtc_now() + (ms as u64 * RTC_HZ).div_ceil(1000)
}

/// The GNSS firmware requires a stable RTC (NuttX gates open/start on
/// `g_rtc_enabled`). One RTC tick is ~30.5 µs; if the counter does not move
/// within this generous spin budget, the RTC is not running.
fn rtc_running() -> bool {
    let start = rtc_now();
    for _ in 0..2_000_000u32 {
        if rtc_now() != start {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

// --- Event latch + firmware request servicing ----------------------------------

/// Notifications observed while waiting for something else. Position/boot
/// flags stay latched until consumed; firmware *requests* are serviced by
/// [`Latch::service_requests`] as soon as the current wait step allows.
#[derive(Default)]
struct Latch {
    position: bool,
    bootcomp: bool,
    cpufifo_ret: Option<i32>,
    req_backup: bool,
    req_cep: bool,
}

impl Latch {
    fn absorb(&mut self, ev: proto::Event) {
        match ev {
            proto::Event::Position => self.position = true,
            proto::Event::BootComp => self.bootcomp = true,
            proto::Event::CpuFifoApiRet(ret) => self.cpufifo_ret = Some(ret),
            proto::Event::ReqBackupData => self.req_backup = true,
            proto::Event::ReqCepData => self.req_cep = true,
            proto::Event::CepHousekeeping | proto::Event::Other => {}
        }
    }

    /// Push one signal message without deadlocking against inbound traffic.
    ///
    /// A blind blocking push can jam forever: the push FIFO reports full
    /// while the firmware is itself pushing to us (hardware-verified — an
    /// instant `BKUPFILE` reply to `REQBKUPDAT` during boot never went
    /// through until the inbound side was drained). Drain into the latch
    /// while waiting; notifications absorbed here are handled by the
    /// caller's next `service_requests`/wait iteration.
    fn send_sig(&mut self, sigtype: u32, data: u32) {
        let words = proto::sig_words(sigtype, data);
        for _ in 0..SIG_PUSH_BUDGET {
            if Mailbox::try_send(words).is_ok() {
                return;
            }
            mailbox::drain_rx();
            if let Some(w) = mailbox::msg_try_recv() {
                self.absorb(proto::classify(w));
            }
            core::hint::spin_loop();
        }
        panic!("GNSS signal push jammed: transport poisoned");
    }

    /// Answer pending firmware requests. With no filesystem there is never
    /// stored data: mirror NuttX's no-file paths — report `-ENOENT` in the
    /// shared info block, then send the zero-length terminator signal
    /// (`cxd56_gnss_read_backup_file` err path / `cxd56_gnss_read_cep_file`).
    fn service_requests(&mut self) {
        if core::mem::take(&mut self.req_backup) {
            shared_info_set_retval(-ENOENT);
            self.send_sig(proto::DEV_BKUPFILE, 0);
        }
        if core::mem::take(&mut self.req_cep) {
            shared_info_set_retval(-ENOENT);
            self.send_sig(proto::DEV_CEP, 0);
        }
    }

    /// Drain the notification sink, then wait until `done(self)` or the deadline.
    fn wait_until(&mut self, deadline: u64, mut done: impl FnMut(&mut Self) -> bool) -> bool {
        loop {
            while let Some(w) = mailbox::msg_try_recv() {
                self.absorb(proto::classify(w));
            }
            self.service_requests();
            if done(self) {
                return true;
            }
            if rtc_now() >= deadline {
                return false;
            }
            core::hint::spin_loop();
        }
    }
}

/// One Far API round-trip with GNSS notification latching. Returns the
/// firmware return value (`arg[0]`); panics on transport timeout (see the
/// module-level "Panics" note).
fn rpc(latch: &mut Latch, dest_cpu: u32, modid: i32, api_id: i32, arg: &mut [u32; 4], budget: u32) -> i32 {
    let res = farapi::call_to(dest_cpu, modid, api_id, arg, budget);
    // Notifications that landed mid-RPC were parked in the dispatcher's MSG
    // sink; absorb them before acting on the latch.
    while let Some(w) = mailbox::msg_try_recv() {
        latch.absorb(proto::classify(w));
    }
    latch.service_requests();
    match res {
        Ok(()) => arg[0] as i32,
        Err(FarapiError::Timeout) => {
            panic!("GNSS Far API timeout (modid {modid}, api {api_id}): transport poisoned")
        }
    }
}

fn fw_result(ret: i32) -> Result<i32, GnssError> {
    if ret < 0 { Err(GnssError::Firmware(ret)) } else { Ok(ret) }
}

// --- The driver ----------------------------------------------------------------

mod sealed {
    /// Typestate marker for [`super::Gnss`].
    pub trait State {}
}

/// [`Gnss`] state: firmware booted, positioning stopped — configuration is
/// legal here.
pub struct Idle;
/// [`Gnss`] state: positioning in progress — data readout is legal here.
pub struct Running;

impl sealed::State for Idle {}
impl sealed::State for Running {}

static TAKEN: AtomicBool = AtomicBool::new(false);

/// The onboard GNSS engine (singleton). See the module docs for the model.
pub struct Gnss<'clk, S: sealed::State> {
    /// A position notification observed before the caller asked for it
    /// (delivered by the next [`Gnss::wait_update`]).
    position_pending: bool,
    /// RTC tick from which `START` may be sent (see [`START_SETTLE_MS`]).
    ready_at: u64,
    _state: PhantomData<S>,
    /// Ties the driver to the [`Clock`] borrow for its whole life — see the
    /// module docs ("Driver shape").
    _clk: PhantomData<&'clk ()>,
}

impl<'clk> Gnss<'clk, Idle> {
    /// Load `gnssfw` onto the GPS CPU, boot it, and service its restore
    /// requests until it reports ready (bounded by ~5 s like NuttX).
    ///
    /// Mirrors `cxd56_gnss_open`: `fw_pm_loadimage` → `fw_pm_startcpu` →
    /// `fw_pm_sleepcpu(HOT_DISABLE)` → wait `BOOTCOMP` → register the shared
    /// info block.
    ///
    /// Requires [`Clock<Hp>`](Clock): `gnssfw`'s bringup drives the SYSIOP
    /// tree to the HV point, so with the APP already locked there, boot
    /// provably moves nothing (hardware-validated — a console built
    /// beforehand keeps decoding) and every divisor sampled earlier stays
    /// correct. The returned driver holds the borrow until
    /// [`shutdown`](Self::shutdown), making operating-point changes compile
    /// errors while the firmware is live. `Hp` specifically, not `Lp`: the
    /// GNSS DSP does have a low-power mode, but boot-time arbitration
    /// between an APP LV lock and the waking GNSS domain is undocumented,
    /// and the `START_SETTLE_MS` window was bracketed at HV only — LV
    /// operation is future hardware-validation work.
    pub fn boot(clock: &'clk Clock<Hp>) -> Result<Self, GnssError> {
        let _ = clock;
        if TAKEN
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(GnssError::Taken);
        }
        Self::boot_inner().inspect_err(|_| TAKEN.store(false, Ordering::Release))
    }

    fn boot_inner() -> Result<Self, GnssError> {
        if !rtc_running() {
            return Err(GnssError::RtcNotReady);
        }

        let mut latch = Latch::default();

        // Purge any leftover firmware instance first. A previous application
        // reset mid-positioning leaves `gnssfw` running (the GPS CPU survives
        // an APP reboot) and auto-resuming — hardware-verified. Force it
        // through the shutdown path; the return codes are deliberately
        // ignored (they are errors when the CPU is already cold, the common
        // case).
        let _ = gps_cpu_off(&mut latch);

        // fw_pm_loadimage(GPS, "gnssfw") — the loader streams the image from
        // SPI flash; -ENOENT here means gnssfw was never provisioned.
        let mut arg = [CPU_GPS, FW_NAME.as_ptr() as u32, 0, 0];
        fw_result(rpc(
            &mut latch,
            farapi::CPUID_SYSIOP,
            MODID_PM,
            API_PM_LOADIMAGE,
            &mut arg,
            LOADIMAGE_POLL_BUDGET,
        ))?;

        // fw_pm_startcpu(GPS, wait=1)
        let mut arg = [CPU_GPS, 1, 0, 0];
        let ret = rpc(
            &mut latch,
            farapi::CPUID_SYSIOP,
            MODID_PM,
            API_PM_STARTCPU,
            &mut arg,
            farapi::DEFAULT_POLL_BUDGET,
        );
        if ret < 0 {
            gps_cpu_off(&mut latch);
            return Err(GnssError::Firmware(ret));
        }

        // Hot sleep off (NuttX default !CONFIG_CXD56_GNSS_HOT_SLEEP); return
        // value ignored, as in NuttX.
        let mut arg = [CPU_GPS, PM_SLEEP_HOT_DISABLE, 0, 0];
        let _ = rpc(
            &mut latch,
            farapi::CPUID_SYSIOP,
            MODID_PM,
            API_PM_SLEEPCPU,
            &mut arg,
            farapi::DEFAULT_POLL_BUDGET,
        );

        // The firmware now asks for its backup data (answered "none" by the
        // latch servicing) and then signals BOOTCOMP.
        let booted = latch.wait_until(deadline_after_ms(BOOT_TIMEOUT_MS), |l| {
            core::mem::take(&mut l.bootcomp)
        });
        if !booted {
            gps_cpu_off(&mut latch);
            return Err(GnssError::BootTimeout);
        }
        // Anchor for the START settle (see START_SETTLE_MS): the firmware
        // is not ready for START the moment it says BOOTCOMP.
        let ready_at = deadline_after_ms(START_SETTLE_MS);

        // Register the shared info block: fw_gd_writebuffer(INFO, 0, &info, 32).
        // The firmware keeps the *address* and reads `retval` across the bus
        // after each request reply, hence the immortal static.
        let mut arg = [
            proto::DEV_INFO,
            0,
            SHARED_INFO.0.get() as u32,
            core::mem::size_of::<SharedInfo>() as u32,
        ];
        let ret = rpc(
            &mut latch,
            CPU_GPS,
            MODID_GNSS,
            API_GD_WRITEBUFFER,
            &mut arg,
            farapi::DEFAULT_POLL_BUDGET,
        );
        if ret < 0 {
            gps_cpu_off(&mut latch);
            return Err(GnssError::Firmware(ret));
        }

        Ok(Gnss {
            position_pending: latch.position,
            ready_at,
            _state: PhantomData,
            _clk: PhantomData,
        })
    }

    /// Milliseconds until the post-`BOOTCOMP` settle elapses and
    /// [`start`](Self::start) can send `START` without waiting (0 = ready
    /// now).
    ///
    /// No readiness signal exists for the settle window: stock NuttX is
    /// oblivious to it, the wire is silent between `BOOTCOMP` and actual
    /// readiness, and the config RPCs succeed inside it — the deadline *is*
    /// the contract. Callers can schedule other work first, and an async
    /// wrapper can await the deadline (e.g. an embassy-time RTC timer)
    /// instead of blocking in `start`.
    pub fn settle_remaining_ms(&self) -> u32 {
        let now = rtc_now();
        if now >= self.ready_at {
            return 0;
        }
        ((self.ready_at - now) * 1000).div_ceil(RTC_HZ) as u32
    }

    /// Select the satellite systems used for positioning.
    ///
    /// A thin pass-through to `fw_gd_selectsatellitesystem`; the legal
    /// combinations are encoded in [`SatelliteSystems::new`], so a mask built
    /// that way is always accepted. A mask built with
    /// [`SatelliteSystems::from_bits`] is not checked here and can still come
    /// back [`GnssError::Firmware`]`(-22)` — see [`Secondary`] for the rule and
    /// the hardware measurement behind it.
    pub fn select_systems(&mut self, systems: SatelliteSystems) -> Result<(), GnssError> {
        let mut arg = [systems.bits(), 0, 0, 0];
        fw_result(self.gd_rpc(API_GD_SELECTSATELLITESYSTEM, &mut arg)).map(|_| ())
    }

    /// The currently selected satellite systems.
    pub fn systems(&mut self) -> Result<SatelliteSystems, GnssError> {
        let mut out: u32 = 0;
        let mut arg = [&raw mut out as u32, 0, 0, 0];
        fw_result(self.gd_rpc(API_GD_GETSATELLITESYSTEM, &mut arg))?;
        // Volatile: the firmware wrote it across the bus.
        Ok(SatelliteSystems::from_bits(unsafe {
            (&raw const out).read_volatile()
        }))
    }

    /// Set the positioning mode and output cycle. The firmware accepts cycles
    /// that are multiples of 1000 ms and rejects others with
    /// [`GnssError::Firmware`].
    pub fn set_operation(&mut self, mode: OperationMode, cycle_ms: u32) -> Result<(), GnssError> {
        let mut arg = [mode as u32, cycle_ms, 0, 0];
        fw_result(self.gd_rpc(API_GD_SETOPERATIONMODE, &mut arg)).map(|_| ())
    }

    /// The current positioning mode and output cycle in ms.
    pub fn operation(&mut self) -> Result<(OperationMode, u32), GnssError> {
        let mut mode: u32 = 0;
        let mut cycle: u32 = 0;
        let mut arg = [&raw mut mode as u32, &raw mut cycle as u32, 0, 0];
        fw_result(self.gd_rpc(API_GD_GETOPERATIONMODE, &mut arg))?;
        let mode = match unsafe { (&raw const mode).read_volatile() } {
            1 => OperationMode::Normal,
            other => return Err(GnssError::Firmware(-(other as i32))),
        };
        Ok((mode, unsafe { (&raw const cycle).read_volatile() }))
    }

    /// Power the TCXO/LNA rail and start positioning.
    ///
    /// Mirrors `cxd56_gnss_start`: rail on, then the `CPUFIFOAPI` start
    /// command, acknowledged by the firmware with a return code.
    pub fn start(mut self, mode: StartMode) -> Result<Gnss<'clk, Running>, (Self, GnssError)> {
        // Hold START until the post-BOOTCOMP settle has elapsed (see
        // START_SETTLE_MS: a premature START is refused with a sticky -60
        // that only a firmware re-boot clears). Firmware requests keep
        // being serviced while holding; no-op once the settle has passed.
        let mut latch = Latch::default();
        latch.wait_until(self.ready_at, |_| false);

        if let Err(FarapiError::Timeout) = crate::pmic::set_loadswitch(LSW_TCXO_LNA, true) {
            panic!("GNSS rail power Far API timeout: transport poisoned");
        }

        latch.send_sig(
            proto::DEV_CPUFIFOAPI,
            ((mode as u32) << 8) | proto::CPUFIFOAPI_START,
        );
        let acked = latch.wait_until(deadline_after_ms(ACK_TIMEOUT_MS), |l| {
            l.cpufifo_ret.is_some()
        });
        self.position_pending |= latch.position;

        let err = match latch.cpufifo_ret {
            Some(ret) if ret < 0 => GnssError::Firmware(ret),
            Some(_) => {
                return Ok(Gnss {
                    position_pending: self.position_pending,
                    ready_at: self.ready_at,
                    _state: PhantomData,
                    _clk: PhantomData,
                });
            }
            None => {
                debug_assert!(!acked);
                GnssError::AckTimeout
            }
        };
        // Deliberately no rail-off: LSW4 also powers the TCXO (see
        // `LSW_TCXO_LNA`) — dropping it would halt the whole SoC.
        Err((self, err))
    }

    /// Cold-sleep the GPS CPU and release the singleton (mirrors
    /// `cxd56_gnss_close`). The firmware's warm/hot-start data survives in
    /// Backup SRAM. Consuming `self` also releases the [`Clock`] borrow —
    /// operating-point transitions become legal again once the GPS CPU is
    /// cold.
    pub fn shutdown(self) -> Result<(), GnssError> {
        let mut latch = Latch::default();
        let ret = gps_cpu_off(&mut latch);
        TAKEN.store(false, Ordering::Release);
        fw_result(ret).map(|_| ())
    }
}

impl<'clk> Gnss<'clk, Running> {
    /// Block until the firmware signals a fresh positioning epoch, at most
    /// `timeout_ms`. Arms the one-shot notify mask (it self-clears with each
    /// notification, so every wait re-arms it, like NuttX's poll setup).
    pub fn wait_update(&mut self, timeout_ms: u32) -> Result<(), GnssError> {
        if core::mem::take(&mut self.position_pending) {
            return Ok(());
        }

        let mut arg = [proto::DEV_GNSS, 0 /* FALSE: don't mask */, 0, 0];
        fw_result(self.gd_rpc(API_GD_SETNOTIFYMASK, &mut arg))?;
        if core::mem::take(&mut self.position_pending) {
            return Ok(());
        }

        let mut latch = Latch::default();
        let got = latch.wait_until(deadline_after_ms(timeout_ms), |l| {
            core::mem::take(&mut l.position)
        });
        if got { Ok(()) } else { Err(GnssError::UpdateTimeout) }
    }

    /// Copy the latest positioning epoch out of the firmware
    /// (`fw_gd_readbuffer(GNSS, 0, out, size)`).
    pub fn read_position(&mut self, out: &mut PositionData) -> Result<(), GnssError> {
        let len = core::mem::size_of::<PositionData>();
        let mut arg = [proto::DEV_GNSS, 0, out as *mut PositionData as u32, len as u32];
        let copied = fw_result(self.gd_rpc(API_GD_READBUFFER, &mut arg))?;
        debug_assert_eq!(copied as usize, len);
        Ok(())
    }

    /// Stop positioning (mirrors `cxd56_gnss_stop`). The TCXO/LNA rail stays
    /// up — it is shared with the system reference clock (see
    /// [`LSW_TCXO_LNA`]), exactly as on stock Spresense.
    pub fn stop(mut self) -> Result<Gnss<'clk, Idle>, (Self, GnssError)> {
        let mut latch = Latch::default();
        latch.send_sig(proto::DEV_CPUFIFOAPI, proto::CPUFIFOAPI_STOP);
        latch.wait_until(deadline_after_ms(ACK_TIMEOUT_MS), |l| {
            l.cpufifo_ret.is_some()
        });
        self.position_pending |= latch.position;
        // Deliberately no rail-off: LSW4 also powers the TCXO (see
        // `LSW_TCXO_LNA`) — dropping it would halt the whole SoC.

        match latch.cpufifo_ret {
            Some(ret) if ret < 0 => Err((self, GnssError::Firmware(ret))),
            Some(_) => Ok(Gnss {
                position_pending: false,
                ready_at: self.ready_at,
                _state: PhantomData,
                _clk: PhantomData,
            }),
            None => Err((self, GnssError::AckTimeout)),
        }
    }
}

impl<'clk, S: sealed::State> Gnss<'clk, S> {
    /// `gnssfw` version, decoded from its Backup SRAM word exactly as NuttX's
    /// `CXD56_GNSS_IOCTL_GET_VERSION` does: `(major, minor, build)`.
    pub fn firmware_version(&self) -> (u8, u8, u32) {
        // SAFETY: Backup SRAM `BKUP->gnssfw_version` (0x0440_0010), read-only.
        let v = unsafe { (0x0440_0010 as *const u32).read_volatile() };
        (((v >> 28) & 0xf) as u8, ((v >> 20) & 0xff) as u8, v & 0xf_ffff)
    }

    /// One `fw_gd_*` RPC, latching any concurrent notifications into
    /// `position_pending`.
    fn gd_rpc(&mut self, api_id: i32, arg: &mut [u32; 4]) -> i32 {
        let mut latch = Latch::default();
        let ret = rpc(
            &mut latch,
            CPU_GPS,
            MODID_GNSS,
            api_id,
            arg,
            farapi::DEFAULT_POLL_BUDGET,
        );
        self.position_pending |= latch.position;
        ret
    }
}

/// NuttX `err1`/close path: hot-sleep re-enable, then cold sleep. Returns the
/// cold-sleep return code (the one NuttX propagates).
fn gps_cpu_off(latch: &mut Latch) -> i32 {
    let mut arg = [CPU_GPS, PM_SLEEP_HOT_ENABLE, 0, 0];
    let _ = rpc(
        latch,
        farapi::CPUID_SYSIOP,
        MODID_PM,
        API_PM_SLEEPCPU,
        &mut arg,
        farapi::DEFAULT_POLL_BUDGET,
    );
    let mut arg = [CPU_GPS, PM_SLEEP_COLD, 0, 0];
    rpc(
        latch,
        farapi::CPUID_SYSIOP,
        MODID_PM,
        API_PM_SLEEPCPU,
        &mut arg,
        farapi::DEFAULT_POLL_BUDGET,
    )
}
