#![no_std]
#![no_main]

//! External loopback test for I2S0 on the Spresense main board.
//!
//! Wiring: one jumper from `I2S0_DATA_OUT` to `I2S0_DATA_IN` — physically
//! **JP2-7 (Arduino D18) → JP2-6 (Arduino D19)**, adjacent pins. BCK (D26) and
//! LRCK (D25) stay free for a scope.
//!
//! The test brings up the CXD5247 companion (audio MCLK), then I2S0 as master @
//! 48 kHz duplex, and repeatedly runs a full-duplex one-shot DMA transfer: a
//! sine tone is transmitted out `SDOUT1` while `SDIN1` is captured into a larger
//! RX buffer. With the jumper in place the tone loops back and the verifier
//! confirms it over the UART1 console (115200 8N1).
//!
//! # Why a tone, not a bit-exact pattern
//!
//! The I2S0 capture path runs through the audio block's sample-rate converter
//! (the RX DMA is sourced from `SRC1`), which is a filter — it strips DC and
//! rings on step edges, so a near-DC PCM pattern never returns bit-exact (and no
//! Spresense SDK example bypasses the SRC; its "bypass" is a Hi-Res sound-quality
//! mode, not a transparent pipe). A passband tone is what this datapath is
//! designed to carry, so the check is energy + periodicity, not a bit match:
//! [`analyze`] confirms the captured signal is periodic at the transmitted
//! tone's period (shift-, gain- and clip-invariant), which proves the tone made
//! the round trip. Without the jumper the line idles/floats and the periodicity
//! check fails — the negative control.

use core::fmt::Write;

use cortex_m::Peripherals;
use cortex_m::peripheral::DWT;
use cortex_m_rt::entry;
use embedded_hal::delay::DelayNs;
use fugit::ExtU32; // brings `.millis()` into scope
use panic_halt as _;

use cxd56_hal::clocks::{Config, RccExt};
use cxd56_hal::delay::Delay;
use cxd56_hal::gpio::pins::Parts;
use cxd56_hal::i2s::{I2s, I2s0, I2s0Pins, I2sConfig};
use cxd56_hal::pac;
use cxd56_hal::uart::{Uart, Uart1Pins};
use cxd56_hal::watchdog::Watchdog;

/// Samples per period of the TX sine. At the 48 kHz frame rate that is a
/// `48000 / SINE_PERIOD` Hz tone — well inside the SRC passband.
const SINE_PERIOD: usize = 16;
/// Tone frequency for the report (Hz).
const TONE_HZ: u32 = 48_000 / SINE_PERIOD as u32;
/// One period of a sine, amplitude ~1/4 full scale (i16).
const SINE: [i16; SINE_PERIOD] = [
    0, 3135, 5793, 7568, 8192, 7568, 5793, 3135, 0, -3135, -5793, -7568, -8192, -7568, -5793, -3135,
];
/// TX length in stereo samples (a whole number of sine periods).
const TX_LEN: usize = 256;
/// RX capture length — oversized to absorb DMA start skew + SRC group delay.
const RX_LEN: usize = 1024;

/// PASS thresholds for [`analyze`].
const MIN_MEAN_SQ: f32 = 1.0e4; // AC energy/sample => a real signal (RMS ~100+)
const MIN_AC_PERIOD: f32 = 0.5; // strong correlation at the tone period
const MAX_AC_HALF: f32 = -0.2; // anti-correlation at the half period (sinusoid)

/// 16-bit stereo TX word with the same sine sample in L and R.
fn sine_word(i: usize) -> u32 {
    let s = SINE[i % SINE_PERIOD] as u16 as u32;
    (s << 16) | s
}

/// Analyse the RX capture for the transmitted tone. Returns
/// `(mean_square, ac_period, ac_half)`:
/// - `mean_square` — AC energy per sample; large means a signal is present,
/// - `ac_period` — normalised autocorrelation at the tone period (~+1 for the
///   tone, ~0 for noise),
/// - `ac_half` — autocorrelation at the half period (~-1 for a sinusoid).
///
/// Autocorrelation is shift- and gain-invariant, so it recognises the tone
/// through the SRC's group delay and amplitude scaling, and even if the path
/// clips the sine (the fundamental period is preserved).
fn analyze(rx: &[u32]) -> (f32, f32, f32) {
    let n = rx.len();
    let l = |i: usize| (rx[i] & 0xffff) as i16 as f32;
    let mean = (0..n).map(|i| l(i)).sum::<f32>() / n as f32;
    let v = |i: usize| l(i) - mean;
    let energy: f32 = (0..n).map(|i| v(i) * v(i)).sum();
    if energy <= 0.0 {
        return (0.0, 0.0, 0.0);
    }
    let p = SINE_PERIOD;
    let ac_p: f32 = (0..n - p).map(|i| v(i) * v(i + p)).sum();
    let ac_h: f32 = (0..n - p / 2).map(|i| v(i) * v(i + p / 2)).sum();
    (energy / n as f32, ac_p / energy, ac_h / energy)
}

#[entry]
fn main() -> ! {
    let pac = pac::Peripherals::take().unwrap();
    let mut core = Peripherals::take().unwrap();

    let crg = pac.crg.constrain(Config::default());
    let clocks = crg.into_clock();

    // Enable the DWT cycle counter for the on-chip BCK/LRCK rate check (see the
    // loop). DWT CYCCNT ticks at the **core clock** (FCLK = `appsmp`, the same
    // clock SysTick/`Delay` use), *not* `cpu_baseclk` — that is the AHB/watchdog
    // clock, which is `appsmp / 2` here (GEAR_AHB ratio). Timing against
    // `cpu_baseclk` would read exactly 2x the true rate.
    core.DCB.enable_trace();
    core.DWT.enable_cycle_counter();
    let core_hz = clocks.appsmp().hz().to_Hz();

    let mut delay = Delay::new(core.SYST, &clocks);
    let parts = Parts::new(pac.topreg);

    // UART1 console for the verdict.
    let uart1_pins = Uart1Pins {
        tx: parts.gp_spi0_cs_x,
        rx: parts.gp_spi0_sck,
    };
    let mut uart = Uart::new(pac.uart1, uart1_pins, Default::default(), &clocks).unwrap();

    // Sign of life: confirms the board booted and UART1 works *before* we touch
    // the audio block. The audio AHB stalls hard (no fault) if its MCLK is not
    // running (User Manual §3.15.6.16), so without this the whole program looks
    // dead even though only the I2S bring-up is stuck.
    let _ = writeln!(uart, "boot: UART1 up; bringing up I2S0 (audio MCLK + codec DSP)...");

    // Turn a bring-up stall into a visible reboot loop instead of a silent hang.
    // The audio register access below cannot be guarded by a status read (no
    // documented MCLK-present register that is itself accessible without MCLK),
    // so we arm a watchdog: if `I2s::new` stalls the bus, the chip resets in
    // ~8 s and you see the "boot:" line repeat. Fed once it returns / per loop.
    let mut wdt = Watchdog::new(pac.wdog, 8000u32.millis(), &clocks).expect("wdog init failed");
    wdt.start();

    // Power on the CXD5247 before probing it: assert XRST, bring up AVDD/DVDD,
    // release XRST. CHECK_ID reads the companion's id over I2C and that fails
    // until the rails are up AND the audio section has been reset. Then give it
    // a moment to come alive.
    match cxd56_hal::audio_aca::cxd5247_power_on() {
        Ok(()) => {
            let _ = writeln!(uart, "aca: CXD5247 powered (AVDD/DVDD, XRST released)");
        }
        Err(e) => {
            let _ = writeln!(uart, "aca: CXD5247 power-on failed: {e:?}");
        }
    }
    delay.delay_ms(20);
    wdt.feed();

    // Gate: does the SYSIOP loader firmware expose the CXD5247 ACA module? The
    // 24.576 MHz audio MCLK comes from that companion chip; without it the audio
    // block has no clock and its registers stall the bus. This Far API probe
    // self-times-out (it cannot hang), so it is safe to run before bring-up.
    // PASS here is the prerequisite for the full CXD5247 MCLK bring-up.
    match cxd56_hal::audio_aca::check_id() {
        Ok(()) => {
            let _ = writeln!(uart, "aca: CHECK_ID ok — CXD5247 companion present");
        }
        Err(e) => {
            let _ = writeln!(uart, "aca: CHECK_ID failed: {e:?} — no ACA over Far API");
        }
    }
    wdt.feed();

    // Start the companion's 24.576 MHz oscillator (the audio MCLK). After this
    // the CXD5602 audio block has a running clock, so its registers no longer
    // stall the AHB. SET_SERDES is deliberately skipped: it configures the mic
    // serializer (irrelevant to I2S0 TX/RX) and asserts in firmware with no mics
    // wired.
    match cxd56_hal::audio_aca::power_on_common() {
        Ok(()) => {
            let _ = writeln!(uart, "aca: POWER_ON_COMMON ok — MCLK oscillator started");
        }
        Err(e) => {
            let _ = writeln!(uart, "aca: POWER_ON_COMMON failed: {e:?}");
        }
    }
    delay.delay_ms(10); // let the 24.576 MHz crystal oscillator stabilise
    wdt.feed();

    // Route the MCLK pad to its master-clock function so the companion's clock
    // actually reaches the CXD5602 audio block (board_audio_initialize's
    // PINCONFS_MCLK). Without this the audio AHB stalls despite POWER_ON_COMMON.
    cxd56_hal::audio_aca::mclk_pin_config();
    let _ = writeln!(uart, "clk: MCLK pad routed to audio block");
    wdt.feed();

    let i2s0_pins = I2s0Pins {
        bck: parts.gp_i2s0_bck,
        lrck: parts.gp_i2s0_lrck,
        data_in: parts.gp_i2s0_data_in,
        data_out: parts.gp_i2s0_data_out,
    };

    let mut i2s = match I2s::<I2s0>::new(pac.audio, i2s0_pins, &clocks, I2sConfig::default()) {
        Ok(i2s) => {
            wdt.feed();
            let _ = writeln!(
                uart,
                "I2S0 up: loopback test, jumper D18 (JP2-7, DATA_OUT) -> D19 (JP2-6, DATA_IN)"
            );
            i2s
        }
        Err(e) => {
            // A clean Err (not a stall) — report it and idle, feeding the dog so
            // the message stays on screen instead of triggering a reboot loop.
            let _ = writeln!(uart, "I2S0 bring-up failed: {e:?}");
            loop {
                wdt.feed();
                delay.delay_ms(1000);
            }
        }
    };

    let mut tx = [0u32; TX_LEN];
    for (i, w) in tx.iter_mut().enumerate() {
        *w = sine_word(i);
    }

    let mut rx = [0u32; RX_LEN];
    let mut iteration: u32 = 0;

    loop {
        iteration += 1;
        rx.fill(0);
        wdt.feed();

        // Time the transfer to observe the capture datapath rate. The RX DMA
        // fills from SRC1's output, whose memory-side rate is 2x the LRCK frame
        // clock in the 8K-48K SRC mode (the audio block oversamples internally),
        // so this measures ~2x LRCK — used below only as a liveness check that
        // the audio clock is actually running. The authoritative pad rate comes
        // from `i2s.frame_clocks()` (register readback), not from this timing.
        let t0 = DWT::cycle_count();
        let result = i2s.transfer_16_blocking(&tx, &mut rx);
        let elapsed = DWT::cycle_count().wrapping_sub(t0);

        match result {
            Ok(()) => {
                let (mean_sq, ac_period, ac_half) = analyze(&rx);
                let pass =
                    mean_sq >= MIN_MEAN_SQ && ac_period >= MIN_AC_PERIOD && ac_half <= MAX_AC_HALF;
                let verdict = if pass { "PASS" } else { "FAIL" };
                let _ = writeln!(
                    uart,
                    "#{iteration}: {verdict} - {TONE_HZ} Hz tone looped back \
                     (mean_sq {mean_sq:.0}, ac@period {ac_period:.2}, ac@half {ac_half:.2})"
                );

                // BCK/LRCK verification (first pass). The pad rates are read back
                // deterministically from the audio block's rate-mode register and
                // the known 24.576 MHz MCLK (LRCK = MCLK/512, BCK = MCLK/8 in the
                // 48 kHz mode) — this is the authoritative on-chip view of the
                // *pin* clock. The DMA timing below is only a liveness check: it
                // observes the SRC's internal rate (~2x LRCK, the audio block's
                // oversampling), confirming the audio clock tree is actually
                // running. The scope hint is the physical pad confirmation.
                if iteration == 1 {
                    let fc = i2s.frame_clocks();
                    let pin_ok = fc.is_master && fc.lrck_hz == 48_000 && !fc.hires;
                    let _ = writeln!(
                        uart,
                        "clk: LRCK = {} Hz, BCK = {}.{:03} MHz at the pads \
                         (MCLK 24.576 MHz / 512, / 8; {}) - {}",
                        fc.lrck_hz,
                        fc.bck_hz / 1_000_000,
                        (fc.bck_hz % 1_000_000) / 1000,
                        if fc.is_master { "master" } else { "slave" },
                        if pin_ok { "PASS" } else { "FAIL" }
                    );

                    // Liveness: internal datapath rate from the DMA timing. Expect
                    // ~2x LRCK (SRC oversample); a 0/garbage value would mean the
                    // audio clock is not running even though registers look right.
                    let secs = elapsed as f32 / core_hz as f32;
                    let internal = RX_LEN as f32 / secs;
                    let expect = 2.0 * fc.lrck_hz as f32;
                    let live = (internal - expect).abs() <= expect * 0.10; // +/-10%
                    let _ = writeln!(
                        uart,
                        "clk: SRC datapath ~= {internal:.0} Hz (~2x LRCK, internal \
                         oversample) - clock {}",
                        if live { "live" } else { "SUSPECT" }
                    );
                    let _ = writeln!(
                        uart,
                        "clk: scope D26 = BCK (3.072 MHz), D25 = LRCK (48 kHz) to confirm the pads"
                    );
                }
            }
            Err(e) => {
                let _ = writeln!(uart, "#{iteration}: DMA error: {e:?}");
            }
        }

        delay.delay_ms(2000);
    }
}
