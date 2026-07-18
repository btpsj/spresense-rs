//! External-crate GNSS demo: exercises the two crates.io no_std GNSS
//! libraries that survived the crate survey (see
//! `documentation/gnss-crate-evaluation.md`) against the live receiver.
//!
//! 1. **`nmea` (parser)** — every epoch's hand-assembled `$GPGGA`/`$GPRMC`
//!    is parsed straight back on target and the decoded fields are compared
//!    against the source [`PositionData`]: an independent round-trip
//!    validation of our generator. (Generation itself stays hand-rolled —
//!    no no_std NMEA *generator* exists on crates.io.)
//! 2. **`sguaba` (coordinates)** — the first valid fix becomes the origin of
//!    a local ENU frame; every later fix is converted WGS84 → ECEF → ENU and
//!    reported as east/north/up metres plus horizontal distance and bearing.
//!
//! Indoors the receiver never fixes; after [`DEMO_AFTER_FIXLESS_EPOCHS`]
//! fixless epochs the example runs one pass on canned positions (~8.9 km
//! apart in Tokyo) so a desk run still exercises both crates, then keeps
//! waiting for a real fix.

#![no_std]
#![no_main]

use core::fmt::Write;

use chrono::{Datelike, NaiveTime};
use cortex_m_rt::entry;
use defmt::{info, warn};
use defmt_serial as _;
use heapless::String;
use nmea::sentences::rmc::RmcStatusOfFix;
use nmea::sentences::{FixType, GgaData, RmcData};
use nmea::{parse_str, ParseResult};
use panic_probe as _;
use sguaba::{
    math::RigidBodyTransform,
    systems::{Ecef, Wgs84},
    Coordinate,
};
use static_cell::StaticCell;
use uom::si::angle::degree;
use uom::si::f64::{Angle, Length};
use uom::si::length::meter;

use cxd56_hal::gnss::{
    Date, Gnss, GnssError, OperationMode, PositionData, SatelliteSystems, StartMode, Time,
};
use cxd56_hal::pac;
use cxd56_hal::{
    clocks::{Clock, Config, Hp, RccExt},
    gpio::pins::Parts,
    uart::{Uart, Uart1Pins},
};

static SERIAL: StaticCell<Uart<'static, pac::Uart1>> = StaticCell::new();
static CLOCK: StaticCell<Clock<Hp>> = StaticCell::new();
// 5392 bytes — keep the epoch buffer off the (8 KiB default) stack.
static POS: StaticCell<PositionData> = StaticCell::new();

/// (lat deg, lon deg, alt m) — Tokyo waterfront, and Shibuya ~8.9 km away.
const DEMO_A: (f64, f64, f64) = (35.6297, 139.7940, 42.0);
const DEMO_B: (f64, f64, f64) = (35.6580, 139.7016, 40.0);
/// Fixless epochs (1 s cycle) before the canned desk demo runs once.
const DEMO_AFTER_FIXLESS_EPOCHS: u32 = 15;

/// Round-trip tolerances, sized to the sentence quantization: `(d)ddmm.mmmm`
/// steps in 1e-4 minute = 1.67e-6 degree increments, and the one-decimal
/// fields (altitude, HDOP, knots, course) in 0.1 steps.
const TOL_DEG: f64 = 5e-6;
const TOL_TENTH: f32 = 0.06;

// One marker type per claimed origin: the `unsafe` in `ecef_to_enu_at`
// asserts "this type's origin is at this WGS84 point", so the live
// reference and the desk demo must not share a type.
sguaba::system!(struct LocalEnu using ENU);
sguaba::system!(struct DemoEnu using ENU);

/// Convert a target position into `$frame` (whose origin `$t` claims) and
/// report east/north/up plus horizontal distance and bearing. A macro so both
/// frames share it without naming sguaba's internal trait bounds.
macro_rules! report_enu {
    ($t:expr, $target:expr, $frame:ty) => {{
        let local: Coordinate<$frame> = $t.transform(Coordinate::<Ecef>::from_wgs84($target));
        let e = local.enu_east().get::<meter>();
        let n = local.enu_north().get::<meter>();
        let u = local.enu_up().get::<meter>();
        // `None` exactly at the origin (the reference fix itself).
        let az = local
            .bearing_from_origin()
            .map(|b| {
                let a = b.azimuth().get::<degree>();
                if a < 0.0 { a + 360.0 } else { a }
            })
            .unwrap_or(0.0);
        info!(
            "nav: E={=f64} m N={=f64} m U={=f64} m | horiz={=f64} m bearing={=f64} deg",
            e,
            n,
            u,
            libm::hypot(e, n),
            az,
        );
    }};
}

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();

    let crg = pac.crg.constrain(Config::default());
    // Construction locks Perf::Hp — the point gnssfw's own bringup drives
    // the tree to, so booting the firmware later moves nothing under the
    // console's divisor. Gnss::boot requires the Clock<Hp> witness.
    let clock: &'static Clock<Hp> = CLOCK.init(crg.into_hp_clock().expect("lock Hp failed"));

    let parts = Parts::new(pac.topreg);
    let uart1_pins = Uart1Pins {
        tx: parts.gp_spi0_cs_x,
        rx: parts.gp_spi0_sck,
    };
    let uart =
        Uart::new(pac.uart1, uart1_pins, Default::default(), clock).expect("uart1 init failed");
    defmt_serial::defmt_serial(SERIAL.init(uart));

    info!("booting gnssfw on the GPS CPU...");
    let mut gnss = match Gnss::boot(clock) {
        Ok(g) => g,
        Err(e) => panic!("gnssfw boot failed: {:?}", e),
    };
    let (major, minor, build) = gnss.firmware_version();
    info!("gnssfw version {}.{}.{}", major, minor, build);

    gnss.select_systems(SatelliteSystems::GPS | SatelliteSystems::GLONASS)
        .expect("select satellite systems");
    gnss.set_operation(OperationMode::Normal, 1000)
        .expect("set positioning cycle");

    // Hot start degrades toward a cold start when the firmware has no stored
    // ephemeris/position, so it is always the right default.
    let mut gnss = match gnss.start(StartMode::Hot) {
        Ok(g) => g,
        Err((_, e)) => panic!("GNSS start failed: {:?}", e),
    };
    info!("positioning started; expect a first cold-sky fix in 35-120 s");

    let pos = POS.init(PositionData::zeroed());
    let mut nav: Option<RigidBodyTransform<Ecef, LocalEnu>> = None;
    let mut fixless_epochs: u32 = 0;
    let mut demo_done = false;
    loop {
        match gnss.wait_update(2_000) {
            Ok(()) => {}
            Err(GnssError::UpdateTimeout) => {
                warn!("no epoch notification within 2 s");
                continue;
            }
            Err(e) => panic!("GNSS wait failed: {:?}", e),
        }
        gnss.read_position(pos).expect("read position");
        let rx = &pos.receiver;
        if rx.pos_fixmode >= 2 && rx.pos_dataexist != 0 {
            fixless_epochs = 0;
            report_fix(pos);
            nmea_roundtrip(pos);
            let here = wgs(rx.latitude, rx.longitude, rx.altitude);
            if nav.is_none() {
                info!(
                    "nav reference set: lat={=f64} lon={=f64} alt={=f64} m",
                    rx.latitude, rx.longitude, rx.altitude,
                );
                // SAFETY: LocalEnu's origin is defined by this very call —
                // the first valid fix of this run. Constructed exactly once.
                nav = Some(unsafe { RigidBodyTransform::ecef_to_enu_at(&here) });
            }
            if let Some(t) = &nav {
                report_enu!(t, &here, LocalEnu);
            }
        } else {
            info!(
                "no fix yet: visible={=u8} tracking={=u8}",
                rx.numsv, rx.numsv_tracking,
            );
            fixless_epochs += 1;
            if !demo_done && fixless_epochs >= DEMO_AFTER_FIXLESS_EPOCHS {
                demo_done = true;
                desk_demo(pos);
            }
        }
    }
}

/// Condensed fix summary (`rust_gnss` additionally prints the per-satellite
/// table; this example focuses on the crate integrations).
fn report_fix(pos: &PositionData) {
    let rx = &pos.receiver;
    info!(
        "fix {=str} lat={=f64} lon={=f64} alt={=f64} m vel={=f32} m/s dir={=f32} deg sv used {=u8}/{=u8}",
        if rx.pos_fixmode == 3 { "3D" } else { "2D" },
        rx.latitude,
        rx.longitude,
        rx.altitude,
        rx.velocity,
        rx.direction,
        rx.numsv_calcpos,
        rx.numsv,
    );
}

/// One canned pass so a desk run (no sky view) still exercises both crates,
/// then the main loop goes back to waiting for a real fix.
fn desk_demo(pos: &mut PositionData) {
    info!(
        "=== no fix after {=u32} epochs - desk demo on canned positions ===",
        DEMO_AFTER_FIXLESS_EPOCHS,
    );
    set_canned(pos, DEMO_A);
    nmea_roundtrip(pos);
    let a = wgs(DEMO_A.0, DEMO_A.1, DEMO_A.2);
    // SAFETY: DemoEnu's origin is defined here as DEMO_A and the type is
    // used for nothing else (LocalEnu keeps the live-fix origin).
    let t = unsafe { RigidBodyTransform::ecef_to_enu_at(&a) };
    set_canned(pos, DEMO_B);
    nmea_roundtrip(pos);
    let b = wgs(DEMO_B.0, DEMO_B.1, DEMO_B.2);
    // Expect roughly E=-8.4 km N=+3.1 km, ~8.9 km horizontal @ ~291 deg.
    report_enu!(&t, &b, DemoEnu);
    info!(
        "cross-check: haversine surface distance {=f64} m",
        a.haversine_distance_on_surface(&b).get::<meter>(),
    );
    info!("=== desk demo done; still waiting for a real fix ===");
}

/// Build a sguaba WGS84 coordinate from firmware degrees/metres.
fn wgs(lat_deg: f64, lon_deg: f64, alt_m: f64) -> Wgs84 {
    Wgs84::builder()
        .latitude(Angle::new::<degree>(lat_deg))
        .expect("latitude in [-90, 90]")
        .longitude(Angle::new::<degree>(lon_deg))
        .altitude(Length::new::<meter>(alt_m))
        .build()
}

/// Poke a synthetic fix into the epoch buffer (it is overwritten by the next
/// `read_position`) so the canned demo drives the exact same code paths as a
/// real fix.
fn set_canned(pos: &mut PositionData, (lat, lon, alt): (f64, f64, f64)) {
    let rx = &mut pos.receiver;
    rx.pos_fixmode = 3;
    rx.pos_dataexist = 1;
    rx.latitude = lat;
    rx.longitude = lon;
    rx.altitude = alt;
    rx.geoid = 36.1;
    rx.velocity = 1.2;
    rx.direction = 291.0;
    rx.numsv = 9;
    rx.numsv_tracking = 9;
    rx.numsv_calcpos = 8;
    rx.pos_dop.hdop = 0.9;
    rx.date = Date {
        year: 2026,
        month: 7,
        day: 4,
    };
    rx.time = Time {
        hour: 12,
        minute: 34,
        sec: 56,
        usec: 0,
    };
}

/// Generate GGA/RMC from the fix, parse each straight back with the `nmea`
/// crate, and compare the decoded fields against the source data.
fn nmea_roundtrip(pos: &PositionData) {
    let mut line: String<128> = String::new();
    if gga(&mut line, pos, true).is_ok() {
        info!("{=str}", line.as_str());
        match parse_str(line.as_str()) {
            Ok(ParseResult::GGA(g)) => match check_gga(&g, pos) {
                Ok(()) => info!("GGA round-trip OK"),
                Err(field) => warn!("GGA round-trip MISMATCH: {=str}", field),
            },
            Ok(_) => warn!("nmea: GGA parsed as an unexpected sentence type"),
            Err(e) => warn!("nmea rejected GGA: {}", defmt::Debug2Format(&e)),
        }
    }
    line.clear();
    if rmc(&mut line, pos, true).is_ok() {
        info!("{=str}", line.as_str());
        match parse_str(line.as_str()) {
            Ok(ParseResult::RMC(r)) => match check_rmc(&r, pos) {
                Ok(()) => info!("RMC round-trip OK"),
                Err(field) => warn!("RMC round-trip MISMATCH: {=str}", field),
            },
            Ok(_) => warn!("nmea: RMC parsed as an unexpected sentence type"),
            Err(e) => warn!("nmea rejected RMC: {}", defmt::Debug2Format(&e)),
        }
    }
}

/// Expected `NaiveTime`: the generator truncates microseconds to hundredths,
/// so rebuild exactly what was printed.
fn expected_time(pos: &PositionData) -> Option<NaiveTime> {
    let t = &pos.receiver.time;
    NaiveTime::from_hms_micro_opt(
        t.hour as u32,
        t.minute as u32,
        t.sec as u32,
        (t.usec / 10_000) * 10_000,
    )
}

fn close_deg(got: Option<f64>, want: f64) -> bool {
    got.is_some_and(|v| libm::fabs(v - want) <= TOL_DEG)
}

fn close_tenth(got: Option<f32>, want: f32) -> bool {
    got.is_some_and(|v| libm::fabsf(v - want) <= TOL_TENTH)
}

/// First mismatching GGA field, if any. Each comparison allows exactly the
/// error the sentence quantization introduces, no more.
fn check_gga(g: &GgaData, pos: &PositionData) -> Result<(), &'static str> {
    let rx = &pos.receiver;
    if !close_deg(g.latitude, rx.latitude) {
        return Err("latitude");
    }
    if !close_deg(g.longitude, rx.longitude) {
        return Err("longitude");
    }
    if g.fix_time.is_none() || g.fix_time != expected_time(pos) {
        return Err("time");
    }
    if !matches!(g.fix_type, Some(FixType::Gps)) {
        return Err("fix type");
    }
    if g.fix_satellites != Some(rx.numsv_calcpos as u32) {
        return Err("satellite count");
    }
    if !close_tenth(g.hdop, rx.pos_dop.hdop) {
        return Err("hdop");
    }
    if !close_tenth(g.altitude, rx.altitude as f32) {
        return Err("altitude");
    }
    if !close_tenth(g.geoid_separation, rx.geoid as f32) {
        return Err("geoid");
    }
    Ok(())
}

/// First mismatching RMC field, if any.
fn check_rmc(r: &RmcData, pos: &PositionData) -> Result<(), &'static str> {
    let rx = &pos.receiver;
    if !close_deg(r.lat, rx.latitude) {
        return Err("latitude");
    }
    if !close_deg(r.lon, rx.longitude) {
        return Err("longitude");
    }
    if r.fix_time.is_none() || r.fix_time != expected_time(pos) {
        return Err("time");
    }
    if !matches!(r.status_of_fix, RmcStatusOfFix::Autonomous) {
        return Err("status");
    }
    if !close_tenth(r.speed_over_ground, rx.velocity * 1.943_844) {
        return Err("speed");
    }
    if !close_tenth(r.true_course, rx.direction) {
        return Err("course");
    }
    // Compare modulo the century so the parser's 2-digit-year pivot rule
    // stays out of the comparison.
    let date_ok = r.fix_date.is_some_and(|d| {
        d.day() == rx.date.day as u32
            && d.month() == rx.date.month as u32
            && d.year().unsigned_abs() % 100 == (rx.date.year % 100) as u32
    });
    if !date_ok {
        return Err("date");
    }
    Ok(())
}

// --- NMEA sentence generation, duplicated verbatim from rust_gnss (examples
// --- are self-contained crates). This is the code the parser validates.

/// Degrees → NMEA `(d)ddmm.mmmm` split: whole degrees and decimal minutes.
fn dm(deg: f64) -> (u32, f64) {
    let abs = if deg < 0.0 { -deg } else { deg };
    let d = abs as u32;
    (d, (abs - d as f64) * 60.0)
}

/// `$GPGGA` — fix, position, satellite count, HDOP, altitude/geoid.
fn gga(out: &mut String<128>, pos: &PositionData, fixed: bool) -> core::fmt::Result {
    let rx = &pos.receiver;
    let (lat_d, lat_m) = dm(rx.latitude);
    let (lon_d, lon_m) = dm(rx.longitude);
    write!(
        out,
        "$GPGGA,{:02}{:02}{:02}.{:02},{:02}{:07.4},{},{:03}{:07.4},{},{},{:02},{:.1},{:.1},M,{:.1},M,,",
        rx.time.hour,
        rx.time.minute,
        rx.time.sec,
        rx.time.usec / 10_000,
        lat_d,
        lat_m,
        if rx.latitude < 0.0 { 'S' } else { 'N' },
        lon_d,
        lon_m,
        if rx.longitude < 0.0 { 'W' } else { 'E' },
        if fixed { 1 } else { 0 },
        rx.numsv_calcpos,
        rx.pos_dop.hdop,
        rx.altitude,
        rx.geoid,
    )?;
    checksum(out)
}

/// `$GPRMC` — validity, position, speed [kn], course, date.
fn rmc(out: &mut String<128>, pos: &PositionData, fixed: bool) -> core::fmt::Result {
    let rx = &pos.receiver;
    let (lat_d, lat_m) = dm(rx.latitude);
    let (lon_d, lon_m) = dm(rx.longitude);
    write!(
        out,
        "$GPRMC,{:02}{:02}{:02}.{:02},{},{:02}{:07.4},{},{:03}{:07.4},{},{:.1},{:.1},{:02}{:02}{:02},,,{}",
        rx.time.hour,
        rx.time.minute,
        rx.time.sec,
        rx.time.usec / 10_000,
        if fixed { 'A' } else { 'V' },
        lat_d,
        lat_m,
        if rx.latitude < 0.0 { 'S' } else { 'N' },
        lon_d,
        lon_m,
        if rx.longitude < 0.0 { 'W' } else { 'E' },
        rx.velocity * 1.943_844,
        rx.direction,
        rx.date.day,
        rx.date.month,
        rx.date.year % 100,
        if fixed { 'A' } else { 'N' },
    )?;
    checksum(out)
}

/// Append the NMEA `*hh` checksum: XOR over everything between `$` and `*`.
fn checksum(out: &mut String<128>) -> core::fmt::Result {
    let sum = out.as_bytes()[1..].iter().fold(0u8, |a, b| a ^ b);
    write!(out, "*{:02X}", sum)
}
