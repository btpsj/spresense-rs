//! Onboard GNSS demo: boots `gnssfw`, tracks GPS + GLONASS, and reports each
//! positioning epoch three ways over the UART1 defmt console — a
//! human-readable fix line, a per-satellite table, and standard NMEA
//! `$GPGGA`/`$GPRMC` sentences (emitted as defmt strings so the binary
//! defmt framing stays intact; the decoded lines paste into any NMEA tool).

#![no_std]
#![no_main]

use core::fmt::Write;

use cortex_m_rt::entry;
use defmt::{info, warn};
use defmt_serial as _;
use heapless::String;
use panic_probe as _;
use static_cell::StaticCell;

use cxd56_hal::gnss::{
    Gnss, GnssError, OperationMode, PositionData, SatelliteSystems, StartMode,
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
        report(pos);
    }
}

/// One epoch, three views: fix summary, satellite table, NMEA sentences.
fn report(pos: &PositionData) {
    let rx = &pos.receiver;
    let fixed = rx.pos_fixmode >= 2 && rx.pos_dataexist != 0;

    if fixed {
        info!(
            "fix {=str} lat={=f64} lon={=f64} alt={=f64} m vel={=f32} m/s dir={=f32} deg hdop={=f32} sv used {=u8}/{=u8}",
            if rx.pos_fixmode == 3 { "3D" } else { "2D" },
            rx.latitude,
            rx.longitude,
            rx.altitude,
            rx.velocity,
            rx.direction,
            rx.pos_dop.hdop,
            rx.numsv_calcpos,
            rx.numsv,
        );
        info!(
            "utc {=u16}-{=u8:02}-{=u8:02} {=u8:02}:{=u8:02}:{=u8:02}.{=u32:06}",
            rx.date.year, rx.date.month, rx.date.day,
            rx.time.hour, rx.time.minute, rx.time.sec, rx.time.usec,
        );
    } else {
        info!(
            "no fix yet: visible={=u8} tracking={=u8}",
            rx.numsv, rx.numsv_tracking,
        );
    }

    // Per-satellite table (svid / system / elevation / azimuth / C-over-N0 /
    // tracked+used flags).
    let count = (pos.svcount as usize).min(pos.sv.len());
    info!("sv  sys      el  az   c/n0  flags ({=usize} tracked)", count);
    for sv in &pos.sv[..count] {
        info!(
            "{=u8:03} {=str} {=u8:02}  {=i16:03}  {=f32}  {=str}{=str}",
            sv.svid,
            system_name(sv.system),
            sv.elevation,
            sv.azimuth,
            sv.siglevel,
            if sv.stat & 0x01 != 0 { "T" } else { "-" },
            if sv.stat & 0x02 != 0 { "P" } else { "-" },
        );
    }

    // NMEA emission — decoded defmt output yields standard sentences.
    let mut line: String<128> = String::new();
    if gga(&mut line, pos, fixed).is_ok() {
        info!("{=str}", line.as_str());
    }
    line.clear();
    if rmc(&mut line, pos, fixed).is_ok() {
        info!("{=str}", line.as_str());
    }
}

fn system_name(bits: u16) -> &'static str {
    // Fixed width — defmt has no alignment hints, so pad here.
    match bits {
        0x01 => "GPS     ",
        0x02 => "GLONASS ",
        0x04 => "SBAS    ",
        0x08 => "QZSS L1C",
        0x10 => "IMES    ",
        0x20 => "QZSS L1S",
        0x40 => "BeiDou  ",
        0x80 => "Galileo ",
        _ => "?       ",
    }
}

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
