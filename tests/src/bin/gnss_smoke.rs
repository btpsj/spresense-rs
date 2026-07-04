//! GNSS bring-up smoke test — **desk-runnable, no sky view needed**.
//!
//! Exercises the whole non-RF surface of `cxd56_hal::gnss` against the real
//! firmware stack:
//!
//!   [1] `Gnss::boot` — SYSIOP loads `gnssfw` from flash, GPS CPU boots,
//!       backup-restore requests get answered, BOOTCOMP arrives.
//!   [2] firmware version word from Backup SRAM (logged; zero is not a
//!       failure, the word is firmware-managed).
//!   [3] satellite-system round-trip: `select_systems` then `systems()` must
//!       read back the same mask — a full RPC in/out through the GNSS CPU,
//!       the real proof the Far API routing (modid 8 / cpu 1) is right.
//!   [4] operation-mode round-trip: `set_operation(Normal, 1000)` /
//!       `operation()`.
//!   [5] cold start → two `wait_update` epochs (the firmware notifies every
//!       cycle even with no fix) → `read_position` → `stop`. Indoors the fix
//!       stays invalid; only the mechanics are asserted.
//!   [6] `shutdown` — GPS CPU back to cold sleep.
//!
//! Ends with `TEST RESULT: PASS`/`FAIL`. No external jumper. A board without
//! `gnssfw` provisioned fails step [1] with `Firmware(-2)`.
//!
//! Run: `cargo run --release --bin gnss_smoke` (from tests/).

#![no_std]
#![no_main]

use cortex_m::asm;
use cortex_m_rt::entry;
use defmt::Debug2Format;
use defmt_serial as _;
use panic_probe as _;
use static_cell::StaticCell;

use cxd56_hal::clocks::{Clock, Config, Hp, RccExt};
use cxd56_hal::gnss::{
    Gnss, GnssError, OperationMode, PositionData, SatelliteSystems, StartMode,
};
use cxd56_hal::gpio::pins::Parts;
use cxd56_hal::pac;
use cxd56_hal::uart::{Uart, Uart1Pins};

static SERIAL: StaticCell<Uart<'static, pac::Uart1>> = StaticCell::new();
static CLOCK: StaticCell<Clock<Hp>> = StaticCell::new();
static POS: StaticCell<PositionData> = StaticCell::new();

fn verdict(ok: bool) -> &'static str {
    if ok { "ok" } else { "FAILED" }
}

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();

    let crg = pac.crg.constrain(Config::default());
    // Construction locks Perf::Hp — the point gnssfw's own bringup drives
    // the tree to (hardware-verified), so booting the firmware later moves
    // nothing under the console's divisor. Gnss::boot requires the
    // Clock<Hp> witness.
    let clock: &'static Clock<Hp> = CLOCK.init(crg.into_hp_clock().expect("lock Hp failed"));
    let parts = Parts::new(pac.topreg);
    let uart = Uart::new(
        pac.uart1,
        Uart1Pins {
            tx: parts.gp_spi0_cs_x,
            rx: parts.gp_spi0_sck,
        },
        Default::default(),
        clock,
    )
    .expect("uart1 init failed");
    defmt_serial::defmt_serial(SERIAL.init(uart));

    let mut all_ok = true;

    // Liveness line before any GNSS traffic: separates "died inside boot"
    // from "console never worked" when things go wrong.
    defmt::println!("[0] gnss_smoke: console up, booting gnssfw...");

    // [1] boot: flash load + GPS CPU bring-up + BOOTCOMP.
    let mut gnss = match Gnss::boot(clock) {
        Ok(g) => {
            defmt::println!("[1] boot: ok");
            g
        }
        Err(e) => {
            defmt::println!("[1] boot: FAILED ({})", Debug2Format(&e));
            defmt::println!("TEST RESULT: FAIL");
            loop {
                asm::wfi();
            }
        }
    };

    // [2] version word (diagnostic only).
    let (major, minor, build) = gnss.firmware_version();
    defmt::println!("[2] gnssfw version {=u8}.{=u8}.{=u32}", major, minor, build);

    // [3] satellite-system round-trip through the GNSS CPU.
    let want = SatelliteSystems::GPS | SatelliteSystems::GLONASS;
    let sys_ok = match gnss.select_systems(want).and_then(|()| gnss.systems()) {
        Ok(got) => got == want,
        Err(e) => {
            defmt::println!("    error: {}", Debug2Format(&e));
            false
        }
    };
    all_ok &= sys_ok;
    defmt::println!("[3] satellite-system round-trip -> {=str}", verdict(sys_ok));

    // [4] operation-mode round-trip.
    let op_ok = match gnss
        .set_operation(OperationMode::Normal, 1000)
        .and_then(|()| gnss.operation())
    {
        Ok((mode, cycle)) => mode == OperationMode::Normal && cycle == 1000,
        Err(e) => {
            defmt::println!("    error: {}", Debug2Format(&e));
            false
        }
    };
    all_ok &= op_ok;
    defmt::println!("[4] operation-mode round-trip -> {=str}", verdict(op_ok));

    // [5] start / epoch notifications / readout / stop. Indoors there is no
    // fix; the firmware still notifies each 1 s epoch and serves the buffer.
    let mut epoch_ok = false;
    gnss = match gnss.start(StartMode::Cold) {
        Ok(mut running) => {
            let mut waits_ok = true;
            for i in 0..2u32 {
                match running.wait_update(10_000) {
                    Ok(()) => defmt::println!("    epoch {=u32} notified", i),
                    Err(GnssError::UpdateTimeout) => {
                        defmt::println!("    epoch {=u32}: no notification in 10 s", i);
                        waits_ok = false;
                    }
                    Err(e) => {
                        defmt::println!("    epoch {=u32}: {}", i, Debug2Format(&e));
                        waits_ok = false;
                    }
                }
            }
            let pos = POS.init(PositionData::zeroed());
            let read_ok = match running.read_position(pos) {
                Ok(()) => {
                    defmt::println!(
                        "    epoch: timestamp={=u64} status={=i32} sv={=u32} tracking={=u8}",
                        pos.data_timestamp,
                        pos.status,
                        pos.svcount,
                        pos.receiver.numsv_tracking,
                    );
                    true
                }
                Err(e) => {
                    defmt::println!("    read_position: {}", Debug2Format(&e));
                    false
                }
            };
            epoch_ok = waits_ok && read_ok;
            match running.stop() {
                Ok(idle) => idle,
                Err((_, e)) => {
                    defmt::println!("    stop: {}", Debug2Format(&e));
                    defmt::println!("TEST RESULT: FAIL");
                    loop {
                        asm::wfi();
                    }
                }
            }
        }
        Err((idle, e)) => {
            defmt::println!("    start: {}", Debug2Format(&e));
            idle
        }
    };
    all_ok &= epoch_ok;
    defmt::println!("[5] start/epochs/read/stop -> {=str}", verdict(epoch_ok));

    // [6] shutdown: GPS CPU to cold sleep, singleton released.
    let down_ok = gnss.shutdown().is_ok();
    all_ok &= down_ok;
    defmt::println!("[6] shutdown -> {=str}", verdict(down_ok));

    defmt::println!("TEST RESULT: {=str}", if all_ok { "PASS" } else { "FAIL" });
    loop {
        asm::wfi();
    }
}
