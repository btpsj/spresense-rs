//! GNSS wire types — `#[repr(C)]` mirrors of the `gnssfw` output structures.
//!
//! The GNSS firmware copies its positioning output into APP-core RAM via
//! `fw_gd_readbuffer`, laid out as NuttX's `struct cxd56_gnss_positiondata_s`
//! (`arch/arm/include/cxd56xx/gnss_type.h`). These mirrors reproduce that
//! layout field-for-field under the ARM EABI natural rules (no packing, no
//! bitfields in this subset); the `const` asserts at the bottom pin every
//! size and the load-bearing offsets so a drifted mirror fails to compile
//! instead of misreading a fix.
//!
//! Two field names differ from NuttX because of Rust keywords:
//! `cxd56_gnss_receiver_s.type` is [`Receiver::pos_type`] and
//! `cxd56_gnss_sv_s.type` is [`Sv::system`]; `receiver.priv` is
//! [`Receiver::internal`].

/// The GPS-interoperable L1 C/A family — freely combinable.
///
/// GPS, SBAS, QZSS and IMES all share the GPS L1 C/A signal structure, and the
/// firmware imposes no restriction across them: every one of the 32 subsets is
/// accepted, alone or beside any [`Secondary`]. Combine with `|`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GpsFamily(u32);

impl GpsFamily {
    /// No L1 C/A signal at all.
    ///
    /// Load-bearing, not decoration: it is the only way to ask for a bare
    /// [`Secondary`] constellation, and the firmware does accept GLONASS-only
    /// (`0x02`), BeiDou-only (`0x40`) and Galileo-only (`0x80`) masks.
    pub const NONE: Self = Self(0);
    /// GPS L1 C/A.
    pub const GPS: Self = Self(1 << 0);
    /// SBAS L1 C/A.
    pub const SBAS: Self = Self(1 << 2);
    /// QZSS L1 C/A.
    pub const QZSS_L1CA: Self = Self(1 << 3);
    /// IMES.
    pub const IMES: Self = Self(1 << 4);
    /// QZSS L1S.
    pub const QZSS_L1S: Self = Self(1 << 5);

    /// The raw `CXD56_GNSS_SAT_*` bits of this family subset.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// `self | other`, usable in `const` position.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Does `self` include every signal in `other`?
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl core::ops::BitOr for GpsFamily {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl core::ops::BitOrAssign for GpsFamily {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = self.union(rhs);
    }
}

/// The one non-GPS constellation the receiver may track alongside the
/// [`GpsFamily`] signals.
///
/// **Hardware-measured** (`tests/src/bin/gnss_satsys_sweep.rs`, gnssfw
/// 2.2.20596, 2026-07-23): GLONASS, BeiDou and Galileo are **mutually
/// exclusive**. `fw_gd_selectsatellitesystem` refuses any mask naming two of
/// them with `-22`/`EINVAL`; of the 256 possible masks exactly 128 are refused,
/// and the minimal refused set is precisely the three pairs `GLONASS|BeiDou`
/// (`0x42`), `GLONASS|Galileo` (`0x82`) and `BeiDou|Galileo` (`0xc0`) — so
/// every refusal is a mask containing one of them. Each of the three alone is
/// accepted.
///
/// Hence an enum rather than three more flags: the illegal state has no
/// spelling. NuttX does not validate this (`cxd56_gnss.c:513` forwards the
/// mask untouched) and neither does the firmware's range check — bits outside
/// the documented eight are accepted verbatim — so `EINVAL` from
/// [`Gnss::select_systems`](crate::gnss::Gnss::select_systems) always means
/// "illegal constellation combination", never "bit out of range".
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Secondary {
    /// GPS family only — leave the slot empty.
    None = 0,
    /// GLONASS L1OF.
    Glonass = 1 << 1,
    /// BeiDou B1I.
    BeiDou = 1 << 6,
    /// Galileo E1B/C.
    Galileo = 1 << 7,
}

/// Satellite-system selection mask (`CXD56_GNSS_SAT_*`, `gnss.h`).
///
/// Built by [`SatelliteSystems::new`] from a freely-combinable [`GpsFamily`]
/// subset plus at most one [`Secondary`] constellation — which is exactly what
/// the firmware permits. There is deliberately **no** `BitOr` here: two masks
/// cannot be merged, so no sequence of safe calls can name two mutually
/// exclusive constellations.
///
/// The same bit assignment appears in [`Receiver::svtype`] and [`Sv::system`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SatelliteSystems(u32);

impl SatelliteSystems {
    /// The systems to position with: any L1 C/A family subset, plus at most
    /// one [`Secondary`] constellation.
    ///
    /// ```ignore
    /// SatelliteSystems::new(GpsFamily::GPS | GpsFamily::QZSS_L1CA, Secondary::Glonass)
    /// SatelliteSystems::new(GpsFamily::GPS, Secondary::None)   // GPS alone
    /// SatelliteSystems::new(GpsFamily::NONE, Secondary::BeiDou) // BeiDou alone
    /// ```
    pub const fn new(family: GpsFamily, secondary: Secondary) -> Self {
        Self(family.bits() | secondary as u32)
    }

    /// The raw `CXD56_GNSS_SAT_*` bitmask.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Reconstruct from a raw firmware bitmask, **unvalidated**.
    ///
    /// The escape hatch out of the type-level rule described on [`Secondary`]:
    /// it will happily build a mask the firmware refuses. That is deliberate —
    /// [`Gnss::systems`](crate::gnss::Gnss::systems) decodes firmware
    /// read-backs through it, and `gnss_satsys_sweep` probes illegal masks with
    /// it on purpose. Prefer [`new`](Self::new) everywhere else.
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Does `self` include every system in `other`?
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

// Mask-encoding pins. [`GpsFamily`]'s consts and [`Secondary`]'s discriminants
// must keep matching `CXD56_GNSS_SAT_*` (`gnss_type.h:53-61`), and the two
// groups must stay disjoint — `new` fuses them with a bare OR, so an overlap
// would silently alias two systems onto one bit.
const _: () = {
    assert!(SatelliteSystems::new(GpsFamily::GPS, Secondary::None).bits() == 0x01);
    assert!(SatelliteSystems::new(GpsFamily::NONE, Secondary::Glonass).bits() == 0x02);
    assert!(SatelliteSystems::new(GpsFamily::SBAS, Secondary::None).bits() == 0x04);
    assert!(SatelliteSystems::new(GpsFamily::QZSS_L1CA, Secondary::None).bits() == 0x08);
    assert!(SatelliteSystems::new(GpsFamily::IMES, Secondary::None).bits() == 0x10);
    assert!(SatelliteSystems::new(GpsFamily::QZSS_L1S, Secondary::None).bits() == 0x20);
    assert!(SatelliteSystems::new(GpsFamily::NONE, Secondary::BeiDou).bits() == 0x40);
    assert!(SatelliteSystems::new(GpsFamily::NONE, Secondary::Galileo).bits() == 0x80);

    // The whole family plus each secondary: the three maximal masks the sweep
    // measured as accepted (`tests/src/bin/gnss_satsys_sweep.rs`).
    let all = GpsFamily::GPS
        .union(GpsFamily::SBAS)
        .union(GpsFamily::QZSS_L1CA)
        .union(GpsFamily::IMES)
        .union(GpsFamily::QZSS_L1S);
    assert!(SatelliteSystems::new(all, Secondary::Glonass).bits() == 0x3f);
    assert!(SatelliteSystems::new(all, Secondary::BeiDou).bits() == 0x7d);
    assert!(SatelliteSystems::new(all, Secondary::Galileo).bits() == 0xbd);
};

/// Positioning start mode (`CXD56_GNSS_STMOD_*`, `gnss.h:593`).
///
/// Which mode actually applies depends on what the firmware has retained
/// (ephemeris, almanac, last position, time): it degrades toward a cold
/// start when the requested data is missing, so `Hot` is always safe to ask
/// for. The intermediate `WARM_ACC2`/`HOT_ACC*` tuning variants exist in the
/// firmware but are outside this minimal surface.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum StartMode {
    /// No retained data is used.
    Cold = 0,
    /// Reuse almanac + rough position/time.
    Warm = 1,
    /// Reuse ephemeris — fastest time to first fix.
    Hot = 3,
}

/// Positioning operation mode (`fw_gd_setoperationmode` first argument).
///
/// The firmware also implements test/measurement modes (2, 4, 5); only the
/// normal positioning mode is exposed here.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum OperationMode {
    /// Normal positioning, output every configured cycle.
    Normal = 1,
}

/// UTC / GPS day (`struct cxd56_gnss_date_s`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Date {
    pub year: u16,
    pub month: u8,
    pub day: u8,
}

/// UTC / GPS time of day (`struct cxd56_gnss_time_s`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Time {
    pub hour: u8,
    pub minute: u8,
    pub sec: u8,
    /// Microseconds.
    pub usec: u32,
}

/// Dilution of precision (`struct cxd56_gnss_dop_s`).
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct Dop {
    /// Position DOP.
    pub pdop: f32,
    /// Horizontal DOP.
    pub hdop: f32,
    /// Vertical DOP.
    pub vdop: f32,
    /// Time DOP.
    pub tdop: f32,
    /// East-West DOP.
    pub ewdop: f32,
    /// North-South DOP.
    pub nsdop: f32,
    /// Stdev of semi-major axis.
    pub majdop: f32,
    /// Stdev of semi-minor axis.
    pub mindop: f32,
    /// Orientation of semi-major axis [deg].
    pub oridop: f32,
}

/// Position variance (`struct cxd56_gnss_var_s`).
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct Var {
    /// Horizontal.
    pub hvar: f32,
    /// Vertical.
    pub vvar: f32,
}

/// Satellite position, ECEF metres (`struct cxd56_gnss_pvt_sv_pos_s`).
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct SvPos {
    pub fx: f64,
    pub fy: f64,
    pub fz: f64,
}

/// Satellite velocity, ECEF m/s (`struct cxd56_gnss_pvt_sv_vel_s`).
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct SvVel {
    pub fvx: f32,
    pub fvy: f32,
    pub fvz: f32,
}

/// Receiver fix data (`struct cxd56_gnss_receiver_s`).
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct Receiver {
    /// Position type; 0: invalid, 1: GNSS, 2: IMES, 3: user set, 4: previous.
    /// (NuttX field name: `type`.)
    pub pos_type: u8,
    /// 0: SGPS, 1: DGPS.
    pub dgps: u8,
    /// 1: invalid, 2: 2D fix, 3: 3D fix.
    pub pos_fixmode: u8,
    /// 1: invalid, 2: 2D VZ, 3: 2D offset, 4: 3D, 5: 1D, 6: PRED.
    pub vel_fixmode: u8,
    /// Number of visible satellites.
    pub numsv: u8,
    /// Number of tracking satellites.
    pub numsv_tracking: u8,
    /// Number of satellites used to calculate the position.
    pub numsv_calcpos: u8,
    /// Number of satellites used to calculate the velocity.
    pub numsv_calcvel: u8,
    /// Assist usage bit field (`CXD56_GNSS_PVT_RECEIVER_ASSIST_*`):
    /// bit0 user set, bit1 CEP position, bit2 CEP velocity, bit3 AEP
    /// position, bit4 AEP velocity.
    pub assist: u8,
    /// 0: none, 1: position data exists.
    pub pos_dataexist: u8,
    /// Satellite systems in use — [`SatelliteSystems`] bit assignment.
    pub svtype: u16,
    /// Systems used for the position solution.
    pub pos_svtype: u16,
    /// Systems used for the velocity solution.
    pub vel_svtype: u16,
    /// Position source; 0: invalid, 1: GNSS, 2: IMES, 3: user set,
    /// 4: previous.
    pub possource: u32,
    /// TCXO offset [Hz].
    pub tcxo_offset: f32,
    /// DOPs of the position solution.
    pub pos_dop: Dop,
    /// Weighted DOPs of the velocity solution.
    pub vel_idx: Dop,
    /// Position accuracy variance.
    pub pos_accuracy: Var,
    /// Latitude [degree].
    pub latitude: f64,
    /// Longitude [degree].
    pub longitude: f64,
    /// Altitude [m].
    pub altitude: f64,
    /// Geoid height [m].
    pub geoid: f64,
    /// Speed over ground [m/s].
    pub velocity: f32,
    /// Course over ground [degree].
    pub direction: f32,
    /// Current day (UTC).
    pub date: Date,
    /// Current time (UTC).
    pub time: Time,
    /// Current day (GPS).
    pub gpsdate: Date,
    /// Current time (GPS).
    pub gpstime: Time,
    /// Receive time (UTC).
    pub receivetime: Time,
    /// For firmware internal use. (NuttX field name: `priv`.)
    pub internal: u32,
    /// Leap seconds [s].
    pub leap_sec: i8,
    /// Elapsed time from reset [ns].
    pub time_ns: u64,
    /// Elapsed time from the GPS epoch [ns].
    pub full_bias_ns: i64,
    /// Receiver extra (debug) data.
    pub extra: [u8; 568],
}

/// Per-satellite data (`struct cxd56_gnss_sv_s`).
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct Sv {
    /// This satellite's system — a single [`GpsFamily`]/[`Secondary`] bit.
    /// (NuttX field name: `type`.)
    pub system: u16,
    /// Satellite id (PRN).
    pub svid: u8,
    /// Usage bit field: bit0 tracking, bit1 positioning, bit2 calculating
    /// velocity, bit3 visible.
    pub stat: u8,
    /// Azimuth [degree].
    pub azimuth: i16,
    /// Carrier-phase status bit field: bit0 unknown, bit1 valid, bit2 reset,
    /// bit3 cycle slip.
    pub phase_stat: u8,
    /// bit0: almanac OK.
    pub nav_message_info: u8,
    /// Raw-measurement validity bit field: bit0 doppler, bit1 pseudorange,
    /// bit2 carrier phase, bit3 sv time, bit4 sv clock, bit5 sv pos/vel.
    pub raw_stat: u8,
    /// Measurement-invalidity cause bit field: bit0 not tracked, bit1 no
    /// signal, bit2 no ephemeris, bit3 clock not adjusted, bit4 before TOW
    /// locked, bit5 not supported, bit6 speed limit.
    pub invalid_cause: u8,
    /// Elevation [degree].
    pub elevation: u8,
    /// Frequency channel (GLONASS).
    pub freqchannel: i8,
    /// C/N0 [dB-Hz].
    pub siglevel: f32,
    /// Doppler frequency [Hz].
    pub doppler: f32,
    /// Pseudo range [m].
    pub pseudo_range: f64,
    /// Carrier phase [cycle].
    pub carrier_phase: f64,
    /// Received SV time [s].
    pub sv_time: f64,
    /// Time tracked [s].
    pub timetracked: f32,
    /// Satellite clock offset [m].
    pub svclockoffset: f32,
    /// Satellite clock drift [m/s].
    pub svclockdrift: f32,
    /// Satellite position (ECEF).
    pub svpos: SvPos,
    /// Satellite velocity (ECEF).
    pub svvel: SvVel,
    /// SV extra (debug) data.
    pub extra: [u8; 40],
}

/// Number of [`Sv`] slots in [`PositionData`] (`CXD56_GNSS_MAX_SV_NUM`).
pub const MAX_SV_NUM: usize = 32;

/// One positioning epoch (`struct cxd56_gnss_positiondata_s`) — the buffer
/// `fw_gd_readbuffer` fills. 5392 bytes; keep it in a `static` rather than on
/// a task stack.
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct PositionData {
    /// Timestamp of this epoch.
    pub data_timestamp: u64,
    /// 0: valid, negative: invalid. (Declared `uint32_t` in NuttX with
    /// documented "<0: invalid" semantics, so mirrored signed.)
    pub status: i32,
    /// Number of valid entries in `sv`.
    pub svcount: u32,
    /// Receiver fix data.
    pub receiver: Receiver,
    /// Per-satellite data; only `sv[..svcount]` is meaningful.
    pub sv: [Sv; MAX_SV_NUM],
}

impl PositionData {
    /// An all-zero buffer for `read_position` to fill.
    ///
    /// Every field is an integer, a float, or an aggregate of those, so the
    /// all-zero bit pattern is a valid value.
    pub const fn zeroed() -> Self {
        unsafe { core::mem::MaybeUninit::zeroed().assume_init() }
    }
}

// Layout pins — sizes for everything, offsets where padding is inserted or a
// miscount would silently shear every following field. A failure here means
// the mirror no longer matches `gnss_type.h` and must not be trusted.
const _: () = {
    use core::mem::{align_of, offset_of, size_of};

    assert!(size_of::<Date>() == 4);
    assert!(size_of::<Time>() == 8);
    assert!(size_of::<Dop>() == 36);
    assert!(size_of::<Var>() == 8);
    assert!(size_of::<SvPos>() == 24);
    assert!(size_of::<SvVel>() == 12);

    assert!(size_of::<Receiver>() == 768);
    assert!(align_of::<Receiver>() == 8);
    assert!(offset_of!(Receiver, svtype) == 10);
    assert!(offset_of!(Receiver, possource) == 16);
    assert!(offset_of!(Receiver, pos_dop) == 24);
    assert!(offset_of!(Receiver, latitude) == 104);
    assert!(offset_of!(Receiver, velocity) == 136);
    assert!(offset_of!(Receiver, date) == 144);
    assert!(offset_of!(Receiver, time) == 148);
    assert!(offset_of!(Receiver, receivetime) == 168);
    assert!(offset_of!(Receiver, leap_sec) == 180);
    assert!(offset_of!(Receiver, time_ns) == 184); // 3 pad bytes before
    assert!(offset_of!(Receiver, extra) == 200);

    assert!(size_of::<Sv>() == 144);
    assert!(align_of::<Sv>() == 8);
    assert!(offset_of!(Sv, siglevel) == 12);
    assert!(offset_of!(Sv, pseudo_range) == 24); // 4 pad bytes before
    assert!(offset_of!(Sv, timetracked) == 48);
    assert!(offset_of!(Sv, svpos) == 64); // 4 pad bytes before
    assert!(offset_of!(Sv, svvel) == 88);
    assert!(offset_of!(Sv, extra) == 100); // 4 pad bytes after

    assert!(size_of::<PositionData>() == 5392);
    assert!(offset_of!(PositionData, receiver) == 16);
    assert!(offset_of!(PositionData, sv) == 784);
};
