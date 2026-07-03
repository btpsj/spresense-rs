//! PWM channels 0 and 1 (CXD5602 SCU-domain PWM peripheral).
//!
//! The CXD5602 has a 4-channel PWM block at `0x0419_5600` clocked by the SCU
//! base clock (the same source as I²C0 and the LP-ADC). Each channel has a
//! 16-bit period counter, a 16-bit off-period register, and a prescaler
//! (divisor = 2^N, N = 0–8) that extends the range to very low frequencies.
//!
//! This module exposes channels 0 and 1 through a single [`PwmBlock`] that
//! consumes the [`pac::Pwm`] token for exclusive ownership of the hardware
//! block, then [`split`](PwmBlock::split)s into two independent channel structs.
//!
//! # Frequency and duty-cycle math
//!
//! Ported from `convert_freq2period()` in `cxd56_pwm.c`:
//!
//! * **period** is the number of SCU-clock ticks (after prescaling) per PWM cycle.
//! * **offperiod** is the number of those ticks the output spends LOW.
//! * On-time = `period − offperiod`; duty cycle = `(period − offperiod) / period`.
//!
//! The hardware special cases: `duty = 0` → output stays LOW (EN=0);
//! `duty = 0xFFFF` → output stays HIGH (PARAM=1, EN=1).
//!
//! # Clock borrow
//!
//! Unlike [`Timer`](crate::timer::Timer), PWM does **not** borrow [`Clock`] for
//! a lifetime: the SCU clock is fixed at boot and does not change with the HP/LP
//! operating point, so the frequency is sampled once and stored as `u32`.
//!
//! # Pinmux
//!
//! [`PwmBlock::new`] configures both PWM0 and PWM1 pins for Func1 (PWMA mode)
//! — 4 mA drive, input disabled, floating — by writing `IOCSYS_IOMD1.PWMA` and
//! the `IO_PWM0`/`IO_PWM1` IOCELL registers. Calling `new` is sufficient; no
//! separate pin-type argument is needed.
//!
//! # Example
//!
//! ```ignore
//! let pwm = PwmBlock::new(p.pwm, &clocks)?;
//! let (mut ch0, _ch1) = pwm.split();
//! ch0.configure(1_000, 0x8000)?; // 1 kHz, 50 % duty
//! ```

use thiserror::Error;

use crate::clocks::Clocks;
use crate::pac;
use crate::regs::topreg;

// ── register block shorthand ─────────────────────────────────────────────────

type Rb = pac::pwm::RegisterBlock;

#[inline(always)]
fn rb(ptr: *const Rb) -> &'static Rb {
    // SAFETY: ptr comes from pac::Pwm::PTR (fixed MMIO base, always valid);
    // all register accesses go through VolatileCell, so aliased references are fine.
    unsafe { &*ptr }
}

// ── errors ────────────────────────────────────────────────────────────────────

/// Errors from PWM construction and configuration.
#[derive(Debug, Error)]
pub enum PwmError {
    /// SCU base clock reads as zero (SCU not running or clock tree not sampled).
    #[error("SCU base clock is zero")]
    ClockUnavailable,
    /// Requested frequency is zero or exceeds `(scu_hz + 1) / 2`.
    #[error("frequency out of range")]
    FreqOutOfRange,
    /// Duty cycle is zero or 0xFFFF without calling the constant-level path —
    /// use `set_const_low` / `set_const_high` for those extremes.
    ///
    /// **Note:** [`configure`](Pwm::configure) and [`set_duty`](Pwm::set_duty)
    /// handle 0 and 0xFFFF as special cases internally; this variant is only
    /// returned when the period math would produce an invalid `offperiod` value.
    #[error("duty value would produce invalid off-period")]
    DutyOutOfRange,
}

impl embedded_hal::pwm::Error for PwmError {
    fn kind(&self) -> embedded_hal::pwm::ErrorKind {
        embedded_hal::pwm::ErrorKind::Other
    }
}

// ── frequency / duty computation ─────────────────────────────────────────────

/// Convert `(freq_hz, duty_u16, scu_hz)` → `(PARAM register value, PHASE register value)`.
///
/// Mirrors `convert_freq2period()` in `cxd56_pwm.c` exactly, including the
/// rounding and prescale selection logic.  Returns `Err` if frequency or duty
/// are out of the hardware's valid range.
fn compute_param_phase(freq_hz: u32, duty: u16, scu_hz: u32) -> Result<(u32, u32), PwmError> {
    if freq_hz == 0 || freq_hz > (scu_hz.wrapping_add(1)) >> 1 {
        return Err(PwmError::FreqOutOfRange);
    }

    // Prescale selection: find the smallest N (0–8) such that the period count
    // fits in a 16-bit register.  The NuttX driver skips the prescale search
    // when freq is "high" relative to scu_hz (specifically when
    // `(freq << 8) >= (scu_hz >> 8)`); replicate that gate exactly.
    let mut prescale: u32 = 0;
    if (freq_hz << 8) < (scu_hz >> 8) {
        for n in 1u32..=8 {
            if freq_hz > (scu_hz >> n) / 65535 {
                prescale = n;
                break;
            }
        }
    }

    let period: u32 = if prescale > 0 {
        (((scu_hz as u64 * 10) >> prescale) as u32 / freq_hz + 5) / 10
    } else {
        (scu_hz * 10 / freq_hz - 5) / 10
    };
    let period = period.min(0xffff);

    let offperiod: u32 = if prescale > 0 {
        let op = ((0x10000u64 - duty as u64) * period as u64
            + (1u64 << (16 - prescale))) >> 16;
        if op < 2 {
            return Err(PwmError::DutyOutOfRange);
        }
        op as u32
    } else {
        ((((0x10000u64 - duty as u64) * (period as u64 + 1)) + 0x8000) >> 16) as u32
    };

    let offperiod = offperiod.min(period);

    let param = (period & 0xffff) | ((offperiod & 0xffff) << 16);
    let phase = prescale << 16;
    Ok((param, phase))
}

// ── pinmux ────────────────────────────────────────────────────────────────────

/// Configure both PWM0 and PWM1 pads for Func1 (PWMA output mode).
///
/// From `board_pinconfig.h`: `PINCONF(PIN_PWM0, mode=1, ENZI=0, 4mA=1, pull=0)`
/// → LOWEMI=0 (4 mA), PDN=1 (no pulldown), PUN=1 (no pullup), ENZI=0 (output only).
/// The IOCSYS_IOMD1.PWMA field selects Func1 for both pins in the PWMA group.
///
/// Reference: `cxd56_pinconfig.c` GROUP_PWMA=20, `cxd56_pwm.c` `pwm_pin_config()`.
fn pwm_pinmux() {
    let tr = topreg();
    // 4 mA (LOWEMI=0), no pulldown (PDN=1), no pullup (PUN=1), input disabled (ENZI=0).
    tr.io_pwm0().write(|w| {
        w.lowemi().clear_bit().pdn().set_bit().pun().set_bit().enzi().clear_bit()
    });
    tr.io_pwm1().write(|w| {
        w.lowemi().clear_bit().pdn().set_bit().pun().set_bit().enzi().clear_bit()
    });
    // IOCSYS_IOMD1.PWMA = 0b01 → Func1 (PWMA output for both PWM0 and PWM1).
    // SAFETY: 0b01 is a valid 2-bit PWMA field value.
    tr.iocsys_iomd1()
        .modify(|_, w| unsafe { w.pwma().bits(1) });
}

// ── PwmBlock ──────────────────────────────────────────────────────────────────

/// Owner of the hardware PWM block.
///
/// Consumes [`pac::Pwm`] for exclusive access; call [`split`](Self::split) to
/// obtain per-channel [`Pwm`] structs.  The block is disabled (both channels
/// EN=0) at construction and restored to that state on drop.
pub struct PwmBlock {
    ptr: *const Rb,
    scu_hz: u32,
}

// SAFETY: the raw pointer is the fixed MMIO base; there is no thread-local state.
unsafe impl Send for PwmBlock {}

impl PwmBlock {
    /// Configure the PWM pins and sample the SCU clock.
    ///
    /// Returns [`PwmError::ClockUnavailable`] if `clocks.scu` reads as zero.
    pub fn new(pwm: pac::Pwm, clocks: &Clocks) -> Result<Self, PwmError> {
        let scu_hz = clocks.scu.to_Hz();
        if scu_hz == 0 {
            return Err(PwmError::ClockUnavailable);
        }
        let ptr = pac::Pwm::PTR;
        // Disable both channels before touching the mux.
        rb(ptr).ch0_en().write(|w| w.enable().clear_bit());
        rb(ptr).ch1_en().write(|w| w.enable().clear_bit());
        pwm_pinmux();
        // Consume the token — it is logically owned via `ptr` from here.
        core::mem::forget(pwm);
        Ok(Self { ptr, scu_hz })
    }

    /// Split the block into two independent channel drivers.
    ///
    /// The channel structs are `'static` (raw pointer, no borrow of `self`).
    /// The original `PwmBlock` is consumed — call `unsplit` or drop both
    /// channels to stop the hardware.
    pub fn split(self) -> (Pwm<0>, Pwm<1>) {
        let ptr = self.ptr;
        let scu_hz = self.scu_hz;
        core::mem::forget(self); // suppress PwmBlock::drop
        (
            Pwm { ptr, scu_hz, freq_hz: 0 },
            Pwm { ptr, scu_hz, freq_hz: 0 },
        )
    }
}

impl Drop for PwmBlock {
    fn drop(&mut self) {
        rb(self.ptr).ch0_en().write(|w| w.enable().clear_bit());
        rb(self.ptr).ch1_en().write(|w| w.enable().clear_bit());
    }
}

// ── Pwm<CH> ──────────────────────────────────────────────────────────────────

/// A single PWM channel (`CH = 0` or `CH = 1`).
///
/// Created by [`PwmBlock::split`]; drives the underlying hardware directly via
/// the raw MMIO pointer inherited from [`PwmBlock`].
///
/// The channel is stopped (`EN=0`, output LOW) when dropped.
pub struct Pwm<const CH: u8> {
    ptr: *const Rb,
    scu_hz: u32,
    /// Frequency of the last successful [`configure`](Self::configure) call.
    /// Stored so [`set_duty`](Self::set_duty) can reuse it without a separate
    /// frequency argument.  `0` means "not yet configured."
    freq_hz: u32,
}

// SAFETY: same as PwmBlock.
unsafe impl<const CH: u8> Send for Pwm<CH> {}

// ── register accessors ────────────────────────────────────────────────────────

// Shared implementation via const-generic dispatch.  The accessor names
// (`ch0_param`, `ch1_param`, …) differ per channel, so we use `const CH`
// to select at monomorphisation time.

impl<const CH: u8> Pwm<CH> {
    fn en_read(&self) -> u32 {
        match CH {
            0 => rb(self.ptr).ch0_en().read().bits(),
            _ => rb(self.ptr).ch1_en().read().bits(),
        }
    }

    fn en_write(&self, val: u32) {
        match CH {
            0 => { rb(self.ptr).ch0_en().write(|w| unsafe { w.bits(val) }); }
            _ => { rb(self.ptr).ch1_en().write(|w| unsafe { w.bits(val) }); }
        }
    }

    fn param_write(&self, val: u32) {
        match CH {
            0 => { rb(self.ptr).ch0_param().write(|w| unsafe { w.bits(val) }); }
            _ => { rb(self.ptr).ch1_param().write(|w| unsafe { w.bits(val) }); }
        }
    }

    fn phase_write(&self, val: u32) {
        match CH {
            0 => { rb(self.ptr).ch0_phase().write(|w| unsafe { w.bits(val) }); }
            _ => { rb(self.ptr).ch1_phase().write(|w| unsafe { w.bits(val) }); }
        }
    }

    fn is_enabled(&self) -> bool {
        self.en_read() & 1 != 0
    }

    // ── public API ───────────────────────────────────────────────────────────

    /// Configure and start the PWM output.
    ///
    /// `freq_hz` is the output frequency in Hz; `duty` is the on-time fraction
    /// as a `u16` (0 = always LOW, 0xFFFF = always HIGH, 0x8000 ≈ 50 %).
    ///
    /// Special cases from `cxd56_pwm.c`:
    /// * `duty == 0` → stop the channel (output LOW) without error.
    /// * `duty == 0xFFFF` → drive the output HIGH (PARAM=1, EN=1).
    /// * Already running → update PARAM only (dynamic duty change, no glitch).
    ///
    /// Stores `freq_hz` for subsequent [`set_duty`](Self::set_duty) calls.
    pub fn configure(&mut self, freq_hz: u32, duty: u16) -> Result<(), PwmError> {
        if duty == 0 {
            self.en_write(0);
            self.freq_hz = freq_hz;
            return Ok(());
        }
        if duty == 0xffff {
            self.param_write(1);
            self.en_write(1);
            self.freq_hz = freq_hz;
            return Ok(());
        }

        let (param, phase) = compute_param_phase(freq_hz, duty, self.scu_hz)?;

        if self.is_enabled() {
            // Dynamic duty update: only write PARAM; no stop/restart needed.
            self.param_write(param);
        } else {
            self.en_write(0);
            self.param_write(param);
            self.phase_write(phase);
            self.en_write(1);
        }
        self.freq_hz = freq_hz;
        Ok(())
    }

    /// Change the duty cycle without changing the frequency.
    ///
    /// Equivalent to `configure(self.freq_hz, duty)`.  Requires at least one
    /// prior call to [`configure`](Self::configure) to set a valid frequency.
    pub fn set_duty(&mut self, duty: u16) -> Result<(), PwmError> {
        let f = self.freq_hz;
        if f == 0 {
            return Err(PwmError::FreqOutOfRange);
        }
        self.configure(f, duty)
    }

    /// Stop the channel — write EN=0 (output goes LOW).
    pub fn stop(&mut self) {
        self.en_write(0);
    }

    /// Whether the channel is currently outputting PWM pulses.
    pub fn is_running(&self) -> bool {
        self.is_enabled()
    }

    /// The SCU clock frequency this channel was constructed with.
    pub fn scu_hz(&self) -> u32 {
        self.scu_hz
    }
}

impl<const CH: u8> Drop for Pwm<CH> {
    fn drop(&mut self) {
        self.en_write(0);
    }
}

// ── embedded-hal SetDutyCycle ─────────────────────────────────────────────────

impl<const CH: u8> embedded_hal::pwm::ErrorType for Pwm<CH> {
    type Error = PwmError;
}

/// Duty is expressed as a fraction of `max_duty_cycle()` (= 65535 = 0xFFFF).
///
/// `set_duty_cycle(0)` drives the output permanently LOW;
/// `set_duty_cycle(65535)` drives it permanently HIGH.
/// Values in between require a prior call to [`configure`](Pwm::configure) to
/// set the frequency; if none has been made, `set_duty_cycle` returns
/// [`PwmError::FreqOutOfRange`].
impl<const CH: u8> embedded_hal::pwm::SetDutyCycle for Pwm<CH> {
    fn max_duty_cycle(&self) -> u16 {
        0xffff
    }

    fn set_duty_cycle(&mut self, duty: u16) -> Result<(), PwmError> {
        self.set_duty(duty)
    }
}
