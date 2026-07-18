//! On-hardware verification that the operating-point transitions
//! (`into_hp_clock` at construction, then `into_lp`/`into_hp`) reach a
//! correct, in-spec operating point in **both** directions via the multi-step
//! SYSIOP FREQLOCK handshake (the fix: ack every CLK_CHG pair, complete on
//! the trailing FREQLOCK — 3 pairs each way on CXD5602).
//!
//! Method: the SP804 timer counts at `cpu_baseclk` (a perf-dependent clock); the
//! RTC is a free-running 32.768 kHz counter on the always-on crystal, invariant
//! across operating points. Counting the timer against the RTC over a fixed
//! real-time window recovers the *real* `cpu_baseclk`, compared to the HAL's
//! belief at each point.
//!
//! The LP console runs at a different COM than HP, so a single UART sized for one
//! would garble the other. We therefore **measure at every operating point with
//! no printing** (results captured to RAM), then build the console from the
//! restored-HP clock and print the verdict once.
//!
//! Checks (all at the operating point named):
//!   [1] hp_boot   — measured ≈ believed `cpu_baseclk` at the HP point that
//!                   construction (`into_hp_clock`) locks.
//!   [2] lp        — measured ≈ believed after `into_lp()` (downshift took
//!                   *and* the HAL's belief matches reality at LP).
//!   [3] cache     — after `into_lp()`, the cached `clock.com` (the `Fixed`
//!                   field `uart` reads) equals live `freeze().com`
//!                   (the `resample_dyn` refresh).
//!   [4] hp_recover— measured ≈ believed back at HP (the LP→HP round-trip).
//!   [5] changed   — LP `cpu_baseclk` is clearly below HP's (physical proof the
//!                   clock actually moved, not just the readback).
//!
//! Ends with `TEST RESULT: PASS`/`FAIL`. No external jumper. CXD5602 GPIO is 1.8 V.

#![no_std]
#![no_main]

use cortex_m::asm;
use cortex_m_rt::entry;
use defmt_serial as _;
use panic_probe as _;
use static_cell::StaticCell;

use cxd56_hal::clocks::{Clock, Config, Hp, PerfState, RccExt};
use cxd56_hal::gpio::pins::Parts;
use cxd56_hal::pac;
use cxd56_hal::timer::{Prescaler, Timer};
use cxd56_hal::uart::{Uart, Uart1Pins, UartConfig};

static SERIAL: StaticCell<Uart<'static, pac::Uart1>> = StaticCell::new();
// UART1 borrows the `Clock` for its lifetime (COM is a Dyn clock), so the
// `Clock` must outlive the `'static` UART stored in `SERIAL`.
static CLOCK: StaticCell<Clock<Hp>> = StaticCell::new();

const RTC_HZ: u64 = 32_768;
const WINDOW_TICKS: u64 = RTC_HZ / 4; // 250 ms
const TOL_PCT: u32 = 8;

/// Free-running 32.768 kHz RTC counter, `(RTPOSTCNT<<15)|RTPRECNT`.
fn rtc_ticks(rtc: &pac::Rtc0) -> u64 {
    loop {
        let hi0 = rtc.rtpostcnt().read().bits();
        let lo = rtc.rtprecnt().read().bits() & 0x7fff;
        let hi1 = rtc.rtpostcnt().read().bits();
        if hi0 == hi1 {
            return ((hi0 as u64) << 15) | lo as u64;
        }
    }
}

/// Real cpu_baseclk (Hz) via SP804 ticks over a fixed RTC window.
fn measure<P: PerfState>(clock: &Clock<P>, tok: pac::Timer0, rtc: &pac::Rtc0) -> (u32, pac::Timer0) {
    let mut timer = Timer::new(tok, clock).expect("cpu base clock unavailable");
    timer.start_free_running(Prescaler::Div1);
    let r0 = rtc_ticks(rtc);
    let c0 = timer.counter();
    while rtc_ticks(rtc) - r0 < WINDOW_TICKS {}
    let c1 = timer.counter();
    let r1 = rtc_ticks(rtc);
    let measured = (c0.wrapping_sub(c1) as u64 * RTC_HZ / (r1 - r0)) as u32;
    (measured, timer.free())
}

fn within(meas: u32, believed: u32, tol_pct: u32) -> bool {
    (meas.abs_diff(believed) as u64) * 100 <= believed as u64 * tol_pct as u64
}

fn verdict(ok: bool) -> &'static str {
    if ok { "PASS" } else { "FAIL" }
}

#[entry]
fn main() -> ! {
    let dp = pac::Peripherals::take().unwrap();
    let clock = dp.crg.constrain(Config::default()).into_hp_clock().expect("lock Hp");
    let rtc = dp.rtc0;
    let mut tok = dp.timer0;

    // --- Measure across HP -> LP -> HP with NO printing (capture to RAM). ------

    // [1] HP (locked at construction).
    let believed_hp = clock.cpu_baseclk().to_Hz();
    let (meas_hp, t) = measure(&clock, tok, &rtc);
    tok = t;

    // -> LP. into_lp drives the full multi-step handshake; resample_dyn
    // refreshes the cached COM the console will be sized from.
    let clock = clock.into_lp().expect("into_lp failed");
    let cached_com_lp = clock.com().hz().to_Hz();
    let live_com_lp = clock.freeze().com.to_Hz();
    let believed_lp = clock.cpu_baseclk().to_Hz();
    let (meas_lp, t) = measure(&clock, tok, &rtc);
    tok = t;

    // -> HP (recover).
    let clock = clock.into_hp().expect("into_hp failed");
    let believed_hp2 = clock.cpu_baseclk().to_Hz();
    let (meas_hp2, _t) = measure(&clock, tok, &rtc);

    // --- Report over the restored-HP console. ---------------------------------
    // Promote the clock to `'static` so the UART1 console (which borrows it)
    // can live in `SERIAL`.
    let clock = CLOCK.init(clock);
    let parts = Parts::new(dp.topreg);
    let uart1_pins = Uart1Pins { tx: parts.gp_spi0_cs_x, rx: parts.gp_spi0_sck };
    let uart1 = Uart::new(dp.uart1, uart1_pins, UartConfig::default(), clock).expect("uart1 init failed");
    defmt_serial::defmt_serial(SERIAL.init(uart1));

    defmt::println!("clock_perf: into_lp/into_hp operating-point round-trip (HP->LP->HP)");
    let mut all_ok = true;

    let hp_ok = within(meas_hp, believed_hp, TOL_PCT);
    all_ok &= hp_ok;
    defmt::println!(
        "[1] hp_boot:    cpu_base believed={=u32} measured={=u32} -> {=str}",
        believed_hp, meas_hp, verdict(hp_ok)
    );

    let lp_ok = within(meas_lp, believed_lp, TOL_PCT);
    all_ok &= lp_ok;
    defmt::println!(
        "[2] lp:         cpu_base believed={=u32} measured={=u32} -> {=str}",
        believed_lp, meas_lp, verdict(lp_ok)
    );

    let cache_ok = cached_com_lp == live_com_lp;
    all_ok &= cache_ok;
    defmt::println!(
        "[3] cache:      cached_com={=u32} live_com={=u32} -> {=str}",
        cached_com_lp, live_com_lp, verdict(cache_ok)
    );

    let recover_ok = within(meas_hp2, believed_hp2, TOL_PCT);
    all_ok &= recover_ok;
    defmt::println!(
        "[4] hp_recover: cpu_base believed={=u32} measured={=u32} -> {=str}",
        believed_hp2, meas_hp2, verdict(recover_ok)
    );

    // LP must be clearly below HP — 156 vs 31.2 MHz APP is ~5x, so a 2x margin is
    // ample and tolerant of the exact tap the SYSIOP picks.
    let changed_ok = (meas_lp as u64) * 2 < meas_hp2 as u64;
    all_ok &= changed_ok;
    defmt::println!(
        "[5] changed:    lp={=u32} < hp/2={=u32} -> {=str}",
        meas_lp, meas_hp2 / 2, verdict(changed_ok)
    );

    defmt::println!("TEST RESULT: {=str}", verdict(all_ok));
    loop {
        asm::wfi();
    }
}
