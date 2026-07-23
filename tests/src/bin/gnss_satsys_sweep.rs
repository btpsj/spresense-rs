//! GNSS satellite-system combination sweep — **desk-runnable, no sky view needed**.
//!
//! [`SatelliteSystems`] is an 8-bit mask, so 256 distinct values can reach
//! `fw_gd_selectsatellitesystem`. Some come back as `GnssError::Firmware(n)`,
//! and nothing in the source tree says which: NuttX forwards the mask without
//! validating it (`cxd56_gnss.c:513`), Sony's headers only enumerate the bits
//! (`gnss_type.h:53-61`), and the rule — if there is one — lives inside the
//! closed `gnssfw` image. This bin asks the firmware directly.
//!
//! Every mask is probed with `select_systems` followed by a `systems()`
//! read-back, all inside one firmware boot (no `start`, so the post-`BOOTCOMP`
//! settle window never applies). The read-back matters as much as the return
//! code: a firmware that accepts a mask and then silently reports a *different*
//! one has rejected it too, just without saying so.
//!
//! The sweep runs twice — ascending, then descending. Each probe leaves the
//! firmware holding the mask it just set, so a mask's verdict could in
//! principle depend on its predecessor; reversing the order changes every
//! predecessor, and any per-mask divergence between the passes is proof that
//! the table is not a per-combination truth. After a rejection, a baseline
//! canary re-selects a known-good mask to confirm the firmware is still
//! answering — this stack has form for sticky refusals (the `-60` START
//! refusal in `gnss/mod.rs`), and one poisoning mask would otherwise corrupt
//! every row after it.
//!
//! **Rejections are data, not test failures.** `TEST RESULT: PASS` means the
//! instrument was sound — boot succeeded, both passes completed, and they
//! agree — so the printed table can be believed. `FAIL` means it cannot.
//!
//! Requires Sony's standard firmware set in SPI flash (`gnssfw`); a board
//! without it fails at boot with `Firmware(-2)`.
//!
//! Run: `cargo run --release --bin gnss_satsys_sweep` (from tests/).

#![no_std]
#![no_main]

use cortex_m::asm;
use cortex_m_rt::entry;
use defmt::Debug2Format;
use defmt_serial as _;
use panic_probe as _;
use static_cell::StaticCell;

use cxd56_hal::clocks::{Clock, Config, Hp, RccExt};
use cxd56_hal::gnss::{Gnss, GnssError, Idle, SatelliteSystems};
use cxd56_hal::gpio::pins::Parts;
use cxd56_hal::pac;
use cxd56_hal::uart::{Uart, Uart1Pins};

static SERIAL: StaticCell<Uart<'static, pac::Uart1>> = StaticCell::new();
static CLOCK: StaticCell<Clock<Hp>> = StaticCell::new();
/// 2 KiB per pass — well past what belongs on the 8 KiB default stack.
static PASSES: StaticCell<[[Outcome; MASKS]; 2]> = StaticCell::new();

/// Every value an 8-bit `SatelliteSystems` can take.
const MASKS: usize = 256;

/// Known-good mask for the canary and the final restore: the pair `gnss_smoke`
/// and `rust_gnss` have always used on this hardware.
const BASELINE: SatelliteSystems = SatelliteSystems::GPS.union(SatelliteSystems::GLONASS);

/// One glyph per bit, in bit order — GPS, GLONASS (RINEX `R`), SBAS, QZSS L1CA,
/// IMES, QZSS L1S, BeiDou, Galileo (RINEX `E`).
const GLYPHS: [u8; 8] = *b"GRSQILBE";

/// What the firmware did with one mask.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Outcome {
    /// Selected, and `systems()` read back exactly what was asked for.
    Accepted,
    /// Selected, but the firmware reports a different mask — a silent refusal.
    Coerced(u32),
    /// `select_systems` returned `Firmware(n)`: an outright refusal.
    Rejected(i32),
    /// Selected, but the `systems()` read-back itself returned `Firmware(n)`.
    ReadbackErr(i32),
    /// A non-`Firmware` driver error (transport/state) — not a firmware verdict.
    Driver,
    /// Never probed (the pass aborted before reaching it).
    Skipped,
}

impl Outcome {
    /// A firmware refusal, the thing this sweep is looking for.
    fn is_rejected(self) -> bool {
        matches!(self, Outcome::Rejected(_))
    }

    /// The firmware took the mask (whether or not it kept every bit).
    fn is_usable(self) -> bool {
        matches!(self, Outcome::Accepted | Outcome::Coerced(_))
    }

    /// Grid character.
    fn glyph(self) -> u8 {
        match self {
            Outcome::Accepted => b'.',
            Outcome::Coerced(_) => b'~',
            Outcome::Rejected(_) => b'E',
            Outcome::ReadbackErr(_) => b'R',
            Outcome::Driver => b'!',
            Outcome::Skipped => b'?',
        }
    }
}

/// Render a mask as a fixed-width `GRSQILBE` string, `-` for each clear bit.
fn names(mask: u32, buf: &mut [u8; 8]) -> &str {
    for (i, glyph) in GLYPHS.iter().enumerate() {
        buf[i] = if mask & (1 << i) != 0 { *glyph } else { b'-' };
    }
    // Always ASCII by construction.
    core::str::from_utf8(buf).unwrap_or("????????")
}

/// Standard errno names for the negated codes the firmware returns. It is free
/// to use codes of its own (the START path answers `-60`), hence the fallback.
fn errno_name(ret: i32) -> &'static str {
    match -ret {
        1 => "EPERM",
        2 => "ENOENT",
        5 => "EIO",
        12 => "ENOMEM",
        16 => "EBUSY",
        22 => "EINVAL",
        38 => "ENOSYS",
        _ => "firmware-specific",
    }
}

/// One report line: mask, glyphs, verdict.
fn describe(mask: u32, outcome: Outcome) {
    let mut buf = [0u8; 8];
    let sys = names(mask, &mut buf);
    match outcome {
        Outcome::Accepted => {
            defmt::println!("    0x{=u32:02x} {=str}  accepted", mask, sys);
        }
        Outcome::Coerced(got) => {
            let mut got_buf = [0u8; 8];
            defmt::println!(
                "    0x{=u32:02x} {=str}  COERCED -> 0x{=u32:02x} {=str}",
                mask,
                sys,
                got,
                names(got, &mut got_buf),
            );
        }
        Outcome::Rejected(ret) => {
            defmt::println!(
                "    0x{=u32:02x} {=str}  REJECTED {=i32} ({=str})",
                mask,
                sys,
                ret,
                errno_name(ret),
            );
        }
        Outcome::ReadbackErr(ret) => {
            defmt::println!(
                "    0x{=u32:02x} {=str}  read-back error {=i32} ({=str})",
                mask,
                sys,
                ret,
                errno_name(ret),
            );
        }
        Outcome::Driver => {
            defmt::println!("    0x{=u32:02x} {=str}  driver error", mask, sys);
        }
        Outcome::Skipped => {
            defmt::println!("    0x{=u32:02x} {=str}  skipped", mask, sys);
        }
    }
}

/// Holds the booted firmware across a pass, and can replace it if a mask
/// leaves it unresponsive.
struct Sweeper<'clk> {
    /// `Option` only so `shutdown` (which consumes) can run mid-sweep; it is
    /// `Some` everywhere outside [`Sweeper::reboot`].
    gnss: Option<Gnss<'clk, Idle>>,
    clock: &'clk Clock<Hp>,
    /// How many times a rejection left the firmware failing its canary.
    canary_failures: u32,
    /// How many times the firmware had to be re-booted to keep going.
    reboots: u32,
}

impl<'clk> Sweeper<'clk> {
    /// Is there still a firmware to talk to? False only after a failed
    /// [`Sweeper::reboot`], which ends the sweep.
    fn alive(&self) -> bool {
        self.gnss.is_some()
    }

    /// Select one mask and read it back.
    fn probe(&mut self, mask: u32) -> Outcome {
        let gnss = self.gnss.as_mut().expect("firmware present");
        match gnss.select_systems(SatelliteSystems::from_bits(mask)) {
            Ok(()) => match gnss.systems() {
                Ok(got) if got.bits() == mask => Outcome::Accepted,
                Ok(got) => Outcome::Coerced(got.bits()),
                Err(GnssError::Firmware(ret)) => Outcome::ReadbackErr(ret),
                Err(_) => Outcome::Driver,
            },
            Err(GnssError::Firmware(ret)) => Outcome::Rejected(ret),
            Err(_) => Outcome::Driver,
        }
    }

    /// Is the firmware still taking a mask it is known to accept?
    fn canary(&mut self) -> bool {
        let gnss = self.gnss.as_mut().expect("firmware present");
        match gnss.select_systems(BASELINE).and_then(|()| gnss.systems()) {
            Ok(got) => got == BASELINE,
            Err(_) => false,
        }
    }

    /// Cold-sleep the GPS CPU and load `gnssfw` again. The shutdown verdict is
    /// ignored: we are here precisely because the firmware stopped answering.
    fn reboot(&mut self) -> bool {
        if let Some(gnss) = self.gnss.take() {
            let _ = gnss.shutdown();
        }
        match Gnss::boot(self.clock) {
            Ok(gnss) => {
                self.gnss = Some(gnss);
                self.reboots += 1;
                true
            }
            Err(e) => {
                defmt::println!("    re-boot FAILED ({})", Debug2Format(&e));
                false
            }
        }
    }
}

/// Probe all 256 masks in one direction. Returns `false` if the firmware died
/// unrecoverably (the rest of `out` stays [`Outcome::Skipped`]).
fn run_pass(sweeper: &mut Sweeper, out: &mut [Outcome; MASKS], descending: bool) -> bool {
    for step in 0..MASKS {
        let mask = if descending { MASKS - 1 - step } else { step };
        let outcome = sweeper.probe(mask as u32);
        out[mask] = outcome;

        // Only an error can have poisoned the firmware, so only an error is
        // worth an extra round-trip to check.
        if !outcome.is_usable() && !sweeper.canary() {
            sweeper.canary_failures += 1;
            defmt::println!(
                "    !! canary failed after mask 0x{=u32:02x}; re-booting gnssfw",
                mask as u32,
            );
            if !sweeper.reboot() {
                return false;
            }
        }

        if step % 64 == 63 {
            defmt::println!("    ...{=usize}/{=usize} probed", step + 1, MASKS);
        }
    }
    true
}

/// The 16x16 acceptance grid: row = high nibble, column = low nibble.
fn print_grid(out: &[Outcome; MASKS]) {
    defmt::println!(
        "  legend: '.' accepted  '~' coerced  'E' rejected  'R' read-back err  '!' driver  '?' skipped"
    );
    defmt::println!("       0123456789abcdef");
    let mut row = [0u8; 16];
    for hi in 0..16usize {
        for (lo, cell) in row.iter_mut().enumerate() {
            *cell = out[hi * 16 + lo].glyph();
        }
        defmt::println!(
            "  0x{=usize:x}_ {=str}",
            hi,
            core::str::from_utf8(&row).unwrap_or("????????????????"),
        );
    }
}

/// Everything derived from a completed pass: counts, the single/pair rows, and
/// the lattice edges that turn 256 verdicts into the rule behind them.
fn print_analysis(out: &[Outcome; MASKS]) {
    let mut accepted = 0u32;
    let mut coerced = 0u32;
    let mut rejected = 0u32;
    let mut other = 0u32;
    for outcome in out.iter() {
        match outcome {
            Outcome::Accepted => accepted += 1,
            Outcome::Coerced(_) => coerced += 1,
            Outcome::Rejected(_) => rejected += 1,
            _ => other += 1,
        }
    }
    defmt::println!(
        "  {=u32} accepted, {=u32} coerced, {=u32} rejected, {=u32} other (of {=usize})",
        accepted,
        coerced,
        rejected,
        other,
        MASKS,
    );

    defmt::println!("[analysis] single systems");
    for bit in 0..8usize {
        describe((1 << bit) as u32, out[1 << bit]);
    }

    defmt::println!("[analysis] rejected pairs (of the 28 two-system masks)");
    let mut any = false;
    for i in 0..8usize {
        for j in (i + 1)..8usize {
            let mask = (1 << i) | (1 << j);
            if out[mask].is_rejected() {
                describe(mask as u32, out[mask]);
                any = true;
            }
        }
    }
    if !any {
        defmt::println!("    none");
    }

    // A rejected mask whose every one-bit-smaller subset is accepted: the
    // smallest combinations the firmware refuses, i.e. the rule itself.
    defmt::println!("[analysis] minimal rejected combinations");
    any = false;
    for mask in 1..MASKS {
        if !out[mask].is_rejected() {
            continue;
        }
        let mut minimal = true;
        for bit in 0..8usize {
            if mask & (1 << bit) != 0 && out[mask & !(1 << bit)].is_rejected() {
                minimal = false;
                break;
            }
        }
        if minimal {
            describe(mask as u32, out[mask]);
            any = true;
        }
    }
    if !any {
        defmt::println!("    none — no mask was rejected");
    }

    // An accepted mask that cannot be grown: the largest usable constellation
    // sets, which is what an application actually wants to know.
    defmt::println!("[analysis] maximal accepted combinations");
    any = false;
    for mask in 0..MASKS {
        if !out[mask].is_usable() {
            continue;
        }
        let mut maximal = true;
        for bit in 0..8usize {
            if mask & (1 << bit) == 0 && !out[mask | (1 << bit)].is_rejected() {
                maximal = false;
                break;
            }
        }
        if maximal {
            describe(mask as u32, out[mask]);
            any = true;
        }
    }
    if !any {
        defmt::println!("    none");
    }

    defmt::println!("[analysis] coerced masks (accepted, but read back different)");
    any = false;
    for (mask, outcome) in out.iter().enumerate() {
        if matches!(outcome, Outcome::Coerced(_)) {
            describe(mask as u32, *outcome);
            any = true;
        }
    }
    if !any {
        defmt::println!("    none");
    }
}

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();

    let crg = pac.crg.constrain(Config::default());
    // Construction locks Perf::Hp — the point gnssfw's own bringup drives the
    // tree to, so booting the firmware later moves nothing under the console's
    // divisor. Gnss::boot requires the Clock<Hp> witness.
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

    defmt::println!("[0] gnss_satsys_sweep: console up, booting gnssfw...");

    let gnss = match Gnss::boot(clock) {
        Ok(g) => g,
        Err(e) => {
            defmt::println!("[1] boot: FAILED ({})", Debug2Format(&e));
            defmt::println!("TEST RESULT: FAIL");
            halt();
        }
    };
    let mut sweeper = Sweeper {
        gnss: Some(gnss),
        clock,
        canary_failures: 0,
        reboots: 0,
    };
    let (major, minor, build) = sweeper.gnss.as_ref().expect("booted").firmware_version();
    defmt::println!(
        "[1] boot: ok — gnssfw version {=u8}.{=u8}.{=u32}",
        major,
        minor,
        build,
    );

    // If the mask every other GNSS bin uses does not go through, the fault is
    // in the instrument (or the transport), and no sweep result would mean
    // anything.
    if !sweeper.canary() {
        defmt::println!("[2] baseline GPS|GLONASS: FAILED — instrument unusable");
        defmt::println!("TEST RESULT: FAIL");
        halt();
    }
    defmt::println!("[2] baseline GPS|GLONASS: ok");

    let passes = PASSES.init([[Outcome::Skipped; MASKS]; 2]);

    defmt::println!("[3] pass 1: sweeping 0x00..0xff ascending");
    let pass1_ok = run_pass(&mut sweeper, &mut passes[0], false);
    defmt::println!(
        "[3] pass 1 -> {=str}",
        if pass1_ok { "complete" } else { "ABORTED" }
    );

    defmt::println!("[4] pass 2: sweeping 0xff..0x00 descending");
    let pass2_ok = pass1_ok && run_pass(&mut sweeper, &mut passes[1], true);
    defmt::println!(
        "[4] pass 2 -> {=str}",
        if pass2_ok { "complete" } else { "ABORTED" }
    );

    // Values outside the eight named bits. If the firmware swallows
    // 0xffff_ffff it is not range-checking at all, which means every rejection
    // above is a genuine constellation rule rather than a bounds test.
    defmt::println!("[5] out-of-range bit probe");
    const OUT_OF_RANGE: [u32; 5] = [1 << 8, 1 << 15, 1 << 31, 0xffff_ffff, 0x0000_0101];
    for probe in OUT_OF_RANGE {
        if !sweeper.alive() {
            defmt::println!("    skipped — no firmware to probe");
            break;
        }
        let outcome = sweeper.probe(probe);
        let mut buf = [0u8; 8];
        match outcome {
            Outcome::Accepted => {
                defmt::println!("    0x{=u32:08x} accepted verbatim", probe);
            }
            Outcome::Coerced(got) => {
                defmt::println!(
                    "    0x{=u32:08x} accepted, read back 0x{=u32:08x} {=str}",
                    probe,
                    got,
                    names(got, &mut buf),
                );
            }
            Outcome::Rejected(ret) => {
                defmt::println!(
                    "    0x{=u32:08x} REJECTED {=i32} ({=str})",
                    probe,
                    ret,
                    errno_name(ret),
                );
            }
            Outcome::ReadbackErr(ret) => {
                defmt::println!("    0x{=u32:08x} read-back error {=i32}", probe, ret);
            }
            Outcome::Driver => defmt::println!("    0x{=u32:08x} driver error", probe),
            Outcome::Skipped => {}
        }
        if !outcome.is_usable() && !sweeper.canary() {
            sweeper.canary_failures += 1;
            defmt::println!("    !! canary failed; re-booting gnssfw");
            if !sweeper.reboot() {
                break;
            }
        }
    }

    // The two passes visit every mask from a different predecessor. Agreement
    // is what makes a single table meaningful.
    let mut divergences = 0u32;
    if pass2_ok {
        defmt::println!("[6] pass-1 vs pass-2 divergences");
        for (mask, (&first, &second)) in passes[0].iter().zip(passes[1].iter()).enumerate() {
            if first != second {
                divergences += 1;
                defmt::println!("    mask 0x{=usize:02x}:", mask);
                describe(mask as u32, first);
                describe(mask as u32, second);
            }
        }
        if divergences == 0 {
            defmt::println!("    none — the sweep is order-independent");
        }
    } else {
        defmt::println!("[6] pass-1 vs pass-2: not comparable (a pass aborted)");
    }

    defmt::println!("[7] acceptance grid (pass 1)");
    print_grid(&passes[0]);
    print_analysis(&passes[0]);

    if sweeper.canary_failures != 0 {
        defmt::println!(
            "[8] firmware went unresponsive {=u32} time(s); {=u32} re-boot(s)",
            sweeper.canary_failures,
            sweeper.reboots,
        );
    }

    // Leave a sane mask behind: pass 2 ends on 0x00 (SAT_NONE) and the
    // firmware keeps its configuration in Backup SRAM across boots, so without
    // this the next rust_gnss run would start with no constellations selected.
    let restored = sweeper.alive() && sweeper.canary();
    defmt::println!(
        "[9] restore baseline -> {=str}",
        if restored { "ok" } else { "FAILED" },
    );
    let down_ok = match sweeper.gnss.take() {
        Some(gnss) => gnss.shutdown().is_ok(),
        None => false,
    };
    defmt::println!(
        "[10] shutdown -> {=str}",
        if down_ok { "ok" } else { "FAILED" }
    );

    // Rejections are the point of the sweep, never a failure. What fails is an
    // instrument whose table cannot be trusted.
    let all_ok = pass1_ok && pass2_ok && divergences == 0 && restored && down_ok;
    defmt::println!("TEST RESULT: {=str}", if all_ok { "PASS" } else { "FAIL" });
    halt();
}

fn halt() -> ! {
    loop {
        asm::wfi();
    }
}
