use crate::pac;
use crate::regs;
use core::cell::RefCell;
use core::future::poll_fn;
use core::task::{Poll, Waker};
use cortex_m::peripheral::NVIC;
use critical_section::Mutex;
use thiserror::Error;

pub use embedded_hal_async::digital::Wait;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Low,
    High,
}

/// Input pull configuration for a pad's internal resistors.
///
/// Programmed via the per-pin TOPREG `IO_*` register, whose `PUN` (pull-up) and
/// `PDN` (pull-down) bits are both *active-low* (0 = that resistor enabled).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pull {
    /// No pull; the pin floats when undriven (`PUN=1, PDN=1`).
    Floating,
    /// Internal pull-up to the 1.8 V pad rail (`PUN=0, PDN=1`).
    Up,
    /// Internal pull-down to ground (`PUN=1, PDN=0`).
    Down,
    /// Bus keeper: both resistors on, weakly holding the last driven level
    /// (`PUN=0, PDN=0`). Matches NuttX `PINCONF_BUSKEEPER`.
    BusKeeper,
}

// Bit positions — identical layout for every TOPREG GP_* register.
const IN_BIT: u32 = 1 << 0;
const OUT_BIT: u32 = 1 << 8;
const DIR_BIT: u32 = 1 << 16; // active-low: 0 = drive output, 1 = high-Z input

// Bit positions — identical layout for every TOPREG IO_* (IOCELL) pad register.
// PUN/PDN are active-low: 0 = that pull resistor enabled. LOWEMI (bit 24, drive
// strength) is deliberately never written, so it keeps whatever value pinmux set.
const ENZI_BIT: u32 = 1 << 0; // input buffer: 1 = enabled
const PUN_BIT: u32 = 1 << 8; // pull-up:   0 = enabled, 1 = off
const PDN_BIT: u32 = 1 << 16; // pull-down: 0 = enabled, 1 = off

mod sealed {
    pub trait Sealed {
        fn read_bits(&self) -> u32;
        fn write_bits(&self, val: u32);
        fn read_io_bits(&self) -> u32;
        fn write_io_bits(&self, val: u32);
    }
}

/// Marker trait for svd2rust GPIO register accessors with the standard
/// IN[0] / OUT[8] / DIR[16] (active-low) field layout, paired with the pin's
/// `IO_*` (IOCELL) pad register for input-buffer and pull configuration.
///
/// `PIN` is the CXD5602 pin number (the `PIN_*` numbering from NuttX
/// `cxd56_pinconfig`), used to route the pad to an EXDEVICE interrupt slot.
pub trait PinReg: sealed::Sealed + 'static {
    const PIN: u8;
}

// Each pin has a GP_* data register (`$gp`, IN/OUT/DIR) and a matching IO_*
// pad register (`$io`, ENZI/PUN/PDN/LOWEMI). The GP_* one is owned by the
// `GpioPin`; the IO_* one is reached through the shared `regs::topreg()` block
// (its aliasing invariant is documented there) since it is not individually
// owned. Macros cannot case-convert `GpFoo` → `io_foo`, so the pairs are
// spelled out explicitly.
macro_rules! impl_pinreg {
    ($($gp:ident => $io:ident => $pin:literal),+ $(,)?) => {$(
        impl sealed::Sealed for pac::topreg::$gp {
            fn read_bits(&self) -> u32 {
                self.read().bits()
            }
            fn write_bits(&self, val: u32) {
                self.modify(|_, w| unsafe { w.bits(val) });
            }
            fn read_io_bits(&self) -> u32 {
                crate::regs::topreg().$io().read().bits()
            }
            fn write_io_bits(&self, val: u32) {
                crate::regs::topreg().$io().modify(|_, w| unsafe { w.bits(val) });
            }
        }
        impl PinReg for pac::topreg::$gp {
            const PIN: u8 = $pin;
        }
    )+};
}

// The third column is the CXD5602 pin number (NuttX `PIN_*` numbering), used to
// route the pad to an EXDEVICE interrupt slot.
impl_pinreg!(
    // UART1 console pads (SPI0_CS_X = TX, SPI0_SCK = RX); Func0 is GPIO.
    GpSpi0CsX => io_spi0_cs_x => 17,
    GpSpi0Sck => io_spi0_sck => 18,
    // Main-board LEDs + Arduino D14 (pre-existing)
    GpI2s1Bck => io_i2s1_bck => 97,
    GpI2s1Lrck => io_i2s1_lrck => 98,
    GpI2s1DataIn => io_i2s1_data_in => 99,
    GpI2s1DataOut => io_i2s1_data_out => 100,
    GpI2c4Bck => io_i2c4_bck => 1,
    // JP1 header
    GpUart2Txd => io_uart2_txd => 67,
    GpUart2Rxd => io_uart2_rxd => 68,
    GpUart2Rts => io_uart2_rts => 70,
    GpUart2Cts => io_uart2_cts => 69,
    GpI2s0Bck => io_i2s0_bck => 93,
    GpI2s0Lrck => io_i2s0_lrck => 94,
    GpEmmcCmd => io_emmc_cmd => 76,
    GpEmmcClk => io_emmc_clk => 75,
    GpSenIrqIn => io_sen_irq_in => 37,
    // JP2 header
    GpEmmcData3 => io_emmc_data3 => 80,
    GpEmmcData2 => io_emmc_data2 => 79,
    GpI2s0DataIn => io_i2s0_data_in => 95,
    GpI2s0DataOut => io_i2s0_data_out => 96,
    GpEmmcData1 => io_emmc_data1 => 78,
    GpEmmcData0 => io_emmc_data0 => 77,
    GpI2c0Bck => io_i2c0_bck => 44,
    GpI2c0Bdt => io_i2c0_bdt => 45,
);

/// Program a pad's `IO_*` register for input use: enable the input buffer
/// (ENZI) and select the requested pull, preserving LOWEMI (drive strength).
fn configure_input_pad<R: PinReg>(reg: &R, pull: Pull) {
    let mut raw = reg.read_io_bits() | ENZI_BIT;
    // PUN/PDN are active-low: clear a bit to enable that resistor.
    match pull {
        Pull::Floating => raw |= PUN_BIT | PDN_BIT,
        Pull::Up => {
            raw &= !PUN_BIT;
            raw |= PDN_BIT;
        }
        Pull::Down => {
            raw |= PUN_BIT;
            raw &= !PDN_BIT;
        }
        Pull::BusKeeper => raw &= !(PUN_BIT | PDN_BIT),
    }
    reg.write_io_bits(raw);
}

/// Unconfigured GPIO pin. Call [`into_output`](GpioPin::into_output) or
/// [`into_input`](GpioPin::into_input) to configure the direction.
pub struct GpioPin<R: PinReg> {
    reg: &'static R,
}

impl<R: PinReg> GpioPin<R> {
    /// # Safety
    /// Caller must ensure exclusive access to this pin register for the
    /// lifetime of the program (no other `GpioPin` or direct register access
    /// may exist simultaneously).
    pub unsafe fn new(reg: &'static R) -> Self {
        Self { reg }
    }

    /// Configure as a push-pull output driving `initial`.
    ///
    /// This intentionally leaves the pad's `IO_*` register untouched: the reset
    /// state already has both pulls off, the output driver works regardless of
    /// the input-buffer/pull bits, and rewriting it would clobber a deliberate
    /// drive-strength (LOWEMI) or pull setting. Pad config is orthogonal to
    /// direction, matching NuttX's split between `cxd56_gpio` and pinmux.
    pub fn into_output(self, initial: Level) -> Output<R> {
        let raw = self.reg.read_bits() & !DIR_BIT; // DIR=0 → drive output
        let raw = match initial {
            Level::High => raw | OUT_BIT,
            Level::Low => raw & !OUT_BIT,
        };
        self.reg.write_bits(raw);
        Output { reg: self.reg }
    }

    /// Configure as a high-Z input with the given pull.
    ///
    /// Enables the pad input buffer (ENZI) and programs the pull in the `IO_*`
    /// register, *then* releases the GP_* direction to high-Z — so the chosen
    /// pull is already active the instant the pin stops driving.
    pub fn into_input(self, pull: Pull) -> Input<R> {
        configure_input_pad(self.reg, pull);
        let raw = self.reg.read_bits() | DIR_BIT; // DIR=1 → high-Z input
        self.reg.write_bits(raw);
        Input { reg: self.reg }
    }

    /// Configure as a floating (no-pull) input. See [`into_input`](Self::into_input).
    pub fn into_floating_input(self) -> Input<R> {
        self.into_input(Pull::Floating)
    }

    /// Configure as an input with the internal pull-up enabled.
    pub fn into_pull_up_input(self) -> Input<R> {
        self.into_input(Pull::Up)
    }

    /// Configure as an input with the internal pull-down enabled.
    pub fn into_pull_down_input(self) -> Input<R> {
        self.into_input(Pull::Down)
    }
}

/// Push-pull output.
pub struct Output<R: PinReg> {
    reg: &'static R,
}

impl<R: PinReg> Output<R> {
    pub fn set_high(&mut self) {
        let raw = self.reg.read_bits() | OUT_BIT;
        self.reg.write_bits(raw);
    }

    pub fn set_low(&mut self) {
        let raw = self.reg.read_bits() & !OUT_BIT;
        self.reg.write_bits(raw);
    }

    pub fn set_level(&mut self, level: Level) {
        match level {
            Level::High => self.set_high(),
            Level::Low => self.set_low(),
        }
    }

    pub fn is_set_high(&self) -> bool {
        self.reg.read_bits() & OUT_BIT != 0
    }
}

impl<R: PinReg> embedded_hal::digital::ErrorType for Output<R> {
    type Error = core::convert::Infallible;
}

impl<R: PinReg> embedded_hal::digital::OutputPin for Output<R> {
    fn set_high(&mut self) -> Result<(), Self::Error> {
        Output::set_high(self);
        Ok(())
    }
    fn set_low(&mut self) -> Result<(), Self::Error> {
        Output::set_low(self);
        Ok(())
    }
}

impl<R: PinReg> embedded_hal::digital::StatefulOutputPin for Output<R> {
    fn is_set_high(&mut self) -> Result<bool, Self::Error> {
        Ok(Output::is_set_high(self))
    }
    fn is_set_low(&mut self) -> Result<bool, Self::Error> {
        Ok(!Output::is_set_high(self))
    }
}

/// High-Z input.
///
/// The pad's input buffer (ENZI) and pull (`PUN`/`PDN`, both active-low) are set
/// when the pin is configured via [`GpioPin::into_input`] and friends; change the
/// pull at runtime with [`set_pull`](Self::set_pull).
pub struct Input<R: PinReg> {
    reg: &'static R,
}

impl<R: PinReg> Input<R> {
    pub fn is_high(&self) -> bool {
        self.reg.read_bits() & IN_BIT != 0
    }

    pub fn is_low(&self) -> bool {
        !self.is_high()
    }

    pub fn get_level(&self) -> Level {
        if self.is_high() {
            Level::High
        } else {
            Level::Low
        }
    }

    /// Change the pad pull at runtime, leaving the input buffer enabled.
    ///
    /// Switching pull is not instantaneous: the weak (~100 kΩ) resistor charges
    /// the pin/trace capacitance, so allow a brief settle before sampling a pin
    /// that nothing else is driving.
    pub fn set_pull(&mut self, pull: Pull) {
        configure_input_pad(self.reg, pull);
    }
}

impl<R: PinReg> embedded_hal::digital::ErrorType for Input<R> {
    type Error = core::convert::Infallible;
}

impl<R: PinReg> embedded_hal::digital::InputPin for Input<R> {
    fn is_high(&mut self) -> Result<bool, Self::Error> {
        Ok(Input::is_high(self))
    }
    fn is_low(&mut self) -> Result<bool, Self::Error> {
        Ok(Input::is_low(self))
    }
}

// =============================================================================
// GPIO interrupts (EXDEVICE)
// =============================================================================
//
// The CXD5602 has 12 GPIO external-interrupt slots (EXDEVICE_0..11 → NVIC IRQ
// 20..31). A slot is bound to a pin through the TOPREG `IOCSYS/IOCAPP_INTSEL`
// mux, and its trigger is configured by the `PMU_WAKE_TRIG_*` registers. Edge
// modes use the PMU latch (TRIG0_RAW status, TRIG0_CLR to re-arm); level modes
// route the pad straight through. Encodings and register layout follow the NuttX
// driver `cxd56_gpioint.c`.
//
// The async `Wait` impl (embedded-hal-async) unmasks the NVIC line for the
// duration of a wait so the EXDEVICE handler dispatches; the handler — your
// one-line `#[interrupt]` forwarding to [`on_interrupt`] — masks the line and
// wakes the task. The NVIC pending bit captures the edge in hardware, so the brief
// PMU latch is never polled. For raw handler-driven use call `enable_interrupt`
// and clear the latch from your `#[interrupt]` with [`clear_interrupt`].

const MAX_SLOT: u8 = 12;
const MAX_SYS_SLOT: u8 = 6;
const SLOT_UNUSED: u8 = 0x3f;
/// First pin number of the APP domain; lower pin numbers are in the SYS domain.
const PIN_APP_BASE: u8 = 56;

/// Trigger condition for a GPIO interrupt.
///
/// Level modes assert while the pin holds the level; edge modes latch a single
/// transition (held until cleared). Values are the 3-bit `PMU_WAKE_TRIG_INTDET`
/// polarity encoding.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    LevelHigh,
    LevelLow,
    RisingEdge,
    FallingEdge,
    AnyEdge,
}

impl Trigger {
    /// 3-bit `INTDET` polarity field.
    fn intdet(self) -> u32 {
        match self {
            Trigger::LevelHigh => 2,
            Trigger::LevelLow => 3,
            Trigger::RisingEdge => 4,
            Trigger::FallingEdge => 5,
            Trigger::AnyEdge => 7,
        }
    }

    /// 3-bit `CPUINTSEL` route field. Levels route Through/Inverter (or Pmu with
    /// the noise filter); every edge mode uses the PMU latch.
    fn route(self, filter: bool) -> u32 {
        match self {
            Trigger::LevelHigh => {
                if filter {
                    2 // Pmu
                } else {
                    0 // Through
                }
            }
            Trigger::LevelLow => {
                if filter {
                    2 // Pmu
                } else {
                    1 // Inverter
                }
            }
            Trigger::RisingEdge | Trigger::FallingEdge | Trigger::AnyEdge => 3, // PmuLatch
        }
    }
}

/// Error returned by [`Input::into_interrupt`].
#[derive(Debug, Error)]
pub enum InterruptError {
    /// Every EXDEVICE slot in the pin's domain is taken (6 SYS, 6 APP).
    #[error("no free EXDEVICE slot for this pin's domain")]
    NoSlotAvailable,
}

/// Pin index stored in the INTSEL mux for `pin` (index within its domain).
fn intsel_value(pin: u8) -> u8 {
    if pin < PIN_APP_BASE {
        pin - 1
    } else {
        pin - PIN_APP_BASE
    }
}

/// Read the 6-bit pin index in INTSEL slot `slot` (0..12). One byte per slot:
/// SYS slots 0–3/4–5 in `IOCSYS_INTSEL0/1`, APP slots 6–9/10–11 in `IOCAPP_INTSEL0/1`.
fn intsel_read(slot: u8) -> u8 {
    let t = regs::topreg();
    let (bits, byte) = match slot {
        0..=3 => (t.iocsys_intsel0().read().bits(), slot),
        4..=5 => (t.iocsys_intsel1().read().bits(), slot - 4),
        6..=9 => (t.iocapp_intsel0().read().bits(), slot - 6),
        10..=11 => (t.iocapp_intsel1().read().bits(), slot - 10),
        _ => unreachable!(),
    };
    ((bits >> (byte as u32 * 8)) & 0xff) as u8
}

/// Write the 6-bit pin index `val` into INTSEL slot `slot`. Not interrupt-safe on
/// its own — callers run it under a critical section.
fn intsel_write(slot: u8, val: u8) {
    let t = regs::topreg();
    let val = val as u32;
    macro_rules! put {
        ($reg:ident, $byte:expr) => {{
            let shift = ($byte as u32) * 8;
            t.$reg()
                .modify(|r, w| unsafe { w.bits((r.bits() & !(0xffu32 << shift)) | (val << shift)) });
        }};
    }
    match slot {
        0..=3 => put!(iocsys_intsel0, slot),
        4..=5 => put!(iocsys_intsel1, slot - 4),
        6..=9 => put!(iocapp_intsel0, slot - 6),
        10..=11 => put!(iocapp_intsel1, slot - 10),
        _ => unreachable!(),
    }
}

/// Allocate (or reuse) an EXDEVICE slot for `pin`. Scans the pin's domain (SYS
/// slots 0–5, APP slots 6–11): reuses the slot if the pin is already mapped, else
/// claims the first free (`0x3f`) one. Runs under a critical section — the INTSEL
/// mux is shared across all interrupt pins.
fn allocate_slot(pin: u8) -> Result<u8, InterruptError> {
    let (lo, hi) = if pin < PIN_APP_BASE {
        (0, MAX_SYS_SLOT)
    } else {
        (MAX_SYS_SLOT, MAX_SLOT)
    };
    let want = intsel_value(pin);
    critical_section::with(|_| {
        let mut free: Option<u8> = None;
        for slot in lo..hi {
            let v = intsel_read(slot);
            if v == want {
                return Ok(slot); // already mapped → reuse
            }
            if v == SLOT_UNUSED && free.is_none() {
                free = Some(slot);
            }
        }
        match free {
            Some(slot) => {
                intsel_write(slot, want);
                Ok(slot)
            }
            None => Err(InterruptError::NoSlotAvailable),
        }
    })
}

/// EXDEVICE interrupt for `slot` (0..12).
fn slot_interrupt(slot: u8) -> pac::Interrupt {
    use pac::Interrupt::*;
    match slot {
        0 => EXDEVICE_0,
        1 => EXDEVICE_1,
        2 => EXDEVICE_2,
        3 => EXDEVICE_3,
        4 => EXDEVICE_4,
        5 => EXDEVICE_5,
        6 => EXDEVICE_6,
        7 => EXDEVICE_7,
        8 => EXDEVICE_8,
        9 => EXDEVICE_9,
        10 => EXDEVICE_10,
        11 => EXDEVICE_11,
        _ => unreachable!(),
    }
}

/// Slot index for an EXDEVICE interrupt, or `None` for any other line.
fn interrupt_slot(irq: pac::Interrupt) -> Option<u8> {
    use pac::Interrupt::*;
    Some(match irq {
        EXDEVICE_0 => 0,
        EXDEVICE_1 => 1,
        EXDEVICE_2 => 2,
        EXDEVICE_3 => 3,
        EXDEVICE_4 => 4,
        EXDEVICE_5 => 5,
        EXDEVICE_6 => 6,
        EXDEVICE_7 => 7,
        EXDEVICE_8 => 8,
        EXDEVICE_9 => 9,
        EXDEVICE_10 => 10,
        EXDEVICE_11 => 11,
        _ => return None,
    })
}

/// Real-time hold after (re)configuring **or** clearing an edge trigger so the PMU
/// detector can (re-)sample the pin's current level as its edge baseline before an
/// edge can arrive. Expressed as **wall-clock microseconds** (not CPU cycles)
/// because the detector samples on the fixed 32.768 kHz RTC clock — a fixed
/// CPU-cycle delay is too short at a high core clock (drops edges) and wastefully
/// long at a low one — and as a *duration* (not raw source ticks) so it holds the
/// same real time under any async-time backend (the embassy [`time`](crate::time)
/// driver or [`async_delay`](crate::async_delay)).
///
/// Sized from the user manual's "Time Interval for a Signal to be Able to Detect an
/// Event Again" (Table GPIO-31 / Figure GPIO-24): after a configure/clear an edge
/// trigger is *undetectable* for up to ~13 RTC cycles — (A) re-sample (2–5) + (B)
/// clear-complete (7–8). 488 µs is 16 RTC cycles, margin over the worst documented
/// case (edge with debounce). One-time per arm, dwarfed by the wait it precedes.
const EDGE_ARM_SETTLE_US: u32 = 488;

/// Await the edge baseline settle — [`EDGE_ARM_SETTLE_US`] of wall-clock time (the
/// 32.768 kHz RTC that clocks the PMU detector is perf-invariant, so the hold is a
/// fixed real time) — via the active interrupt-driven async-time backend: the embassy
/// [`time`](crate::time) driver (`time-driver-*`) or [`async_delay`](crate::async_delay)
/// (`async-delay-*`). The core `WFE`-sleeps and the executor runs other tasks for the
/// interval instead of busy-spinning a counter.
///
/// Because it awaits a real interrupt, an async `wait_for_*_edge` requires the app to
/// forward the backend's source IRQ(s) to its handler:
/// * `time-driver-rtc` / `async-delay-rtc` → `RTC0_A0`;
/// * `time-driver-timer` → `TIMER0` (overflow) **and** `TIMER1` (alarm);
/// * `async-delay-timer` → `TIMER0`.
///
/// See [`crate::time`] / [`crate::async_delay`] for the one-line `#[interrupt]` shape.
async fn edge_arm_settle() {
    #[cfg(any(feature = "time-driver-rtc", feature = "time-driver-timer"))]
    crate::time::after_micros(EDGE_ARM_SETTLE_US).await;
    #[cfg(not(any(feature = "time-driver-rtc", feature = "time-driver-timer")))]
    crate::async_delay::after_micros(EDGE_ARM_SETTLE_US).await;
}

/// Program polarity (`INTDET`), route (`CPUINTSEL`) and noise filter
/// (`NOISECUTEN`) for `slot`, under a critical section (read-modify-write of
/// shared per-bank registers).
///
/// This no longer performs the edge baseline settle itself. The async edge wait
/// ([`wait_edge_async`](InterruptInput::wait_edge_async)) settles *after* its
/// arming `clear_pending`, which subsumes a config-time settle — clearing the latch
/// re-establishes the detector's baseline anyway. Polled / raw-handler users and
/// real external sources get the baseline for free (the signal sits at its level
/// long before its edge), exactly as the NuttX driver relies on: `cxd56_gpioint.c`
/// has no settle either. See [`edge_arm_settle`] and [`crate::async_delay`].
fn set_gpioint_config(slot: u8, trigger: Trigger, filter: bool) {
    let t = regs::topreg();
    let intdet = trigger.intdet();
    let route = trigger.route(filter);
    critical_section::with(|_| {
        // Noise filter: bit (16 + slot) in NOISECUTEN0.
        let fbit = 1u32 << (16 + slot as u32);
        t.pmu_wake_trig_noisecuten0().modify(|r, w| unsafe {
            w.bits(if filter { r.bits() | fbit } else { r.bits() & !fbit })
        });

        // Polarity + route are 3-bit fields. Slots 0–3 live in INTDET0/CPUINTSEL0
        // at bit 16+slot*4; slots 4–11 in INTDET1/CPUINTSEL1 at bit (slot-4)*4.
        let shift = 16 + (slot as u32) * 4;
        if shift < 32 {
            t.pmu_wake_trig_intdet0()
                .modify(|r, w| unsafe { w.bits((r.bits() & !(0x7u32 << shift)) | (intdet << shift)) });
            t.pmu_wake_trig_cpuintsel0()
                .modify(|r, w| unsafe { w.bits((r.bits() & !(0x7u32 << shift)) | (route << shift)) });
        } else {
            let shift = shift - 32;
            t.pmu_wake_trig_intdet1()
                .modify(|r, w| unsafe { w.bits((r.bits() & !(0x7u32 << shift)) | (intdet << shift)) });
            t.pmu_wake_trig_cpuintsel1()
                .modify(|r, w| unsafe { w.bits((r.bits() & !(0x7u32 << shift)) | (route << shift)) });
        }
    });
}

/// Raw PMU edge-latch status for `slot` (bit 16+slot in `PMU_WAKE_TRIG0_RAW`).
/// Upstream of the INTC/NVIC, so it reflects a captured edge regardless of mask.
fn edge_latched(slot: u8) -> bool {
    regs::topreg_sub().pmu_wake_trig0_raw().read().bits() & (1u32 << (16 + slot as u32)) != 0
}

/// Iterations spent waiting for a `PMU_WAKE_TRIG0_CLR` write to land in the raw
/// latch. The clear propagates a few slow PMU-clock cycles after the write
/// (~3k CPU cycles measured on hardware); bounded so a still-asserted level
/// trigger cannot spin forever.
const CLEAR_SETTLE_ITERS: u32 = 20_000;

/// Clear the PMU edge latch for `slot` (write 1 to bit 16+slot in
/// `PMU_WAKE_TRIG0_CLR`) and wait for it to take effect.
///
/// The PMU is in a slow clock domain, so the raw latch ([`edge_latched`]) does
/// not read clear until a few PMU cycles after the write. Spinning until it does
/// makes the clear synchronous: an immediately following [`edge_latched`] read or
/// edge re-arm sees a clean latch (the polled `is_pending` API and the async
/// `Wait` arm/disarm), and an `#[interrupt]` handler's [`clear_interrupt`]
/// de-asserts the line before it returns so the (level-type) EXDEVICE request does
/// not re-fire.
fn clear_latch(slot: u8) {
    let sub = regs::topreg_sub();
    let bit = 1u32 << (16 + slot as u32);
    sub.pmu_wake_trig0_clr().write(|w| unsafe { w.bits(bit) });
    for _ in 0..CLEAR_SETTLE_ITERS {
        if sub.pmu_wake_trig0_raw().read().bits() & bit == 0 {
            break;
        }
    }
}

/// A `Waker` cell shared between an [`InterruptInput`] future and the EXDEVICE
/// interrupt handler. Hand-rolled (rather than pulling in `embassy-sync`) so the
/// base HAL stays runtime-free — it needs only `critical-section`, already a
/// dependency. One entry per slot in [`WAKERS`].
struct AtomicWaker {
    waker: Mutex<RefCell<Option<Waker>>>,
}

impl AtomicWaker {
    const fn new() -> Self {
        Self {
            waker: Mutex::new(RefCell::new(None)),
        }
    }

    /// Register `waker` as the task to wake for this slot, unless an equivalent one
    /// is already registered.
    fn register(&self, waker: &Waker) {
        critical_section::with(|cs| {
            let mut slot = self.waker.borrow(cs).borrow_mut();
            match &*slot {
                Some(existing) if existing.will_wake(waker) => {}
                _ => *slot = Some(waker.clone()),
            }
        });
    }

    /// Wake the registered task, if any. Taken out under the critical section but
    /// woken outside it (waking may run arbitrary executor code).
    fn wake(&self) {
        let waker = critical_section::with(|cs| self.waker.borrow(cs).borrow_mut().take());
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// One waker per EXDEVICE slot, linking a slot's [`on_interrupt`] call to the task
/// suspended in that slot's async `wait_for_*`.
static WAKERS: [AtomicWaker; MAX_SLOT as usize] =
    [const { AtomicWaker::new() }; MAX_SLOT as usize];

/// EXDEVICE interrupt entry point for the async [`Wait`] API. Call this from the
/// `#[interrupt]` handler of every EXDEVICE line you `wait_for_*` on:
///
/// ```ignore
/// #[interrupt]
/// fn EXDEVICE_6() {
///     cxd56_hal::gpio::on_interrupt(cxd56_hal::pac::Interrupt::EXDEVICE_6);
/// }
/// ```
///
/// A library cannot define the vector symbol itself — the PAC's `device.x` weakly
/// binds it to `DefaultHandler`, so an rlib's strong definition is dropped — so the
/// application owns the handler and forwards here. This masks the NVIC line (a
/// one-shot disarm so a still-latched edge or a held level cannot re-enter before
/// the future re-arms), then wakes the suspended task. The masked state is what the
/// future polls as "fired". No-op for a non-EXDEVICE interrupt.
pub fn on_interrupt(interrupt: pac::Interrupt) {
    if let Some(slot) = interrupt_slot(interrupt) {
        NVIC::mask(interrupt);
        WAKERS[slot as usize].wake();
    }
}

/// Clear the EXDEVICE edge latch for `interrupt` from within its `#[interrupt]`
/// handler. The NVIC pending bit is cleared by exception entry, but the PMU latch
/// must be cleared here or the line re-asserts after the handler returns. No-op for
/// a non-EXDEVICE interrupt. Mirrors [`crate::timer::clear_pending`].
pub fn clear_interrupt(interrupt: pac::Interrupt) {
    if let Some(slot) = interrupt_slot(interrupt) {
        clear_latch(slot);
    }
}

/// A GPIO input wired to an EXDEVICE interrupt slot.
///
/// Created by [`Input::into_interrupt`]. Implements the async
/// [`embedded_hal_async::digital::Wait`] trait (`wait_for_high` …
/// `wait_for_any_edge`): each waits by unmasking the NVIC line and suspending until
/// the EXDEVICE handler — your one-line `#[interrupt]` forwarding to
/// [`on_interrupt`] — wakes the task. A polled mode
/// ([`is_pending`](Self::is_pending) / [`clear_pending`](Self::clear_pending)) and a
/// raw handler-driven mode ([`enable_interrupt`](Self::enable_interrupt) with your
/// own `#[interrupt]` calling [`clear_interrupt`]) are also available. Reading the
/// pin ([`is_high`](Self::is_high) / [`is_low`](Self::is_low)) still works.
pub struct InterruptInput<R: PinReg> {
    input: Input<R>,
    slot: u8,
    irq: pac::Interrupt,
    filter: bool,
    trigger: Trigger,
}

impl<R: PinReg> Input<R> {
    /// Allocate an EXDEVICE slot for this pin and program `trigger`.
    ///
    /// `noise_filter` routes level triggers through the PMU noise filter (edge
    /// triggers are reliable without it, given the baseline settle in
    /// [`set_gpioint_config`]). The pad input buffer (ENZI) must already be
    /// enabled, exactly as for reading via
    /// [`Input`]. The chip-level INTC gate is opened here (see [`crate::interrupt`])
    /// so a latched edge reaches the NVIC; the NVIC line itself is left **masked**
    /// until a wait arms it. The INTC gate is required both for the async `Wait`
    /// dispatch and for handler-driven use via
    /// [`enable_interrupt`](InterruptInput::enable_interrupt);
    /// [`release`](InterruptInput::release) closes it again.
    ///
    /// Returns [`InterruptError::NoSlotAvailable`] if every slot in the pin's
    /// domain (SYS: pins < 56, APP: pins ≥ 56) is in use.
    pub fn into_interrupt(
        self,
        trigger: Trigger,
        noise_filter: bool,
    ) -> Result<InterruptInput<R>, InterruptError> {
        let slot = allocate_slot(R::PIN)?;
        let mut this = InterruptInput {
            input: self,
            slot,
            irq: slot_interrupt(slot),
            filter: noise_filter,
            trigger,
        };
        this.apply_trigger();
        this.clear_pending();
        // Open the INTC gate so a latched edge reaches the NVIC — needed for the
        // async `Wait` dispatch and for handler-driven use. The NVIC line stays
        // masked until a wait (or `enable_interrupt`) arms it.
        crate::interrupt::enable(this.irq);
        Ok(this)
    }
}

impl<R: PinReg> InterruptInput<R> {
    /// The EXDEVICE interrupt this pin's slot is wired to.
    pub fn interrupt(&self) -> pac::Interrupt {
        self.irq
    }

    /// The EXDEVICE slot index (0–11) allocated to this pin. Pass to
    /// [`crate::sleep::boot::gpio_mask`] to enable cold-sleep wake on this pin.
    pub fn slot(&self) -> u8 {
        self.slot
    }

    pub fn is_high(&self) -> bool {
        self.input.is_high()
    }

    pub fn is_low(&self) -> bool {
        self.input.is_low()
    }

    pub fn get_level(&self) -> Level {
        self.input.get_level()
    }

    /// The currently configured trigger.
    pub fn trigger(&self) -> Trigger {
        self.trigger
    }

    /// Reprogram the trigger condition.
    pub fn set_trigger(&mut self, trigger: Trigger) {
        self.trigger = trigger;
        self.apply_trigger();
    }

    fn apply_trigger(&self) {
        set_gpioint_config(self.slot, self.trigger, self.filter);
    }

    /// Unmask the NVIC line so the configured trigger dispatches your `#[interrupt]`
    /// handler. The INTC gate in front of the NVIC was already opened by
    /// [`into_interrupt`](Input::into_interrupt). The async `Wait` methods arm and
    /// disarm this themselves, so call this only for raw handler-driven use.
    pub fn enable_interrupt(&mut self) {
        // Safety: this HAL uses `critical-section` (PRIMASK), not BASEPRI priority
        // masking, so unmasking a line cannot escape an in-progress critical section.
        unsafe { NVIC::unmask(self.irq) };
    }

    /// Mask the NVIC line.
    pub fn disable_interrupt(&mut self) {
        NVIC::mask(self.irq);
    }

    /// Whether an edge has been latched since the last clear (raw PMU status,
    /// independent of the NVIC mask). Meaningful for edge triggers; for level
    /// triggers read [`is_high`](Self::is_high) / [`is_low`](Self::is_low).
    pub fn is_pending(&self) -> bool {
        edge_latched(self.slot)
    }

    /// Clear a latched edge and the NVIC pending bit.
    pub fn clear_pending(&mut self) {
        clear_latch(self.slot);
        NVIC::unpend(self.irq);
    }

    /// Arm this slot and await an edge — the implementation behind the async
    /// [`Wait`] edge methods. Reconfigures the trigger first if it differs, clears
    /// any stale latch, [settles](edge_arm_settle) the detector's baseline, and
    /// unmasks the NVIC line (the INTC gate was opened in `into_interrupt`), then
    /// suspends until [`on_interrupt`] masks the line and wakes us — the NVIC
    /// pending bit captures the edge in hardware, so the brief PMU latch is never
    /// polled. Leaves the line masked (the handler disarmed it) and the latch clear.
    ///
    /// The [`edge_arm_settle`] hold **after** the clear is essential (and is the
    /// sole settle — `set_gpioint_config` no longer settles on its own): clearing
    /// the latch makes the slow ~32 kHz PMU detector re-establish its edge baseline,
    /// and an edge that arrives before it has re-sampled the current level is
    /// **silently dropped** — a dropped edge never sets the interrupt, so the wait
    /// would hang forever. Measured on hardware at the LV operating point this
    /// happened to ~5% of edges driven immediately after arming; the settle
    /// eliminates it (0 of many hundreds). It is now an interrupt-driven async
    /// delay (the core `WFE`-sleeps it); the cost is one-time per wait, dwarfed by
    /// the wait itself.
    async fn wait_edge_async(&mut self, trigger: Trigger) {
        if self.trigger != trigger {
            self.set_trigger(trigger);
        }
        self.clear_pending();
        edge_arm_settle().await;
        self.enable_interrupt();
        self.armed_wait().await;
        self.clear_pending();
    }

    /// Await `level` — the implementation behind async `wait_for_high`/`wait_for_low`.
    /// Returns at once if the pin already reads it; otherwise arms a level trigger
    /// and suspends, re-arming if the level deasserted before we polled (the pin is
    /// the source of truth).
    async fn wait_level_async(&mut self, trigger: Trigger, level: Level) {
        if self.input.get_level() == level {
            return;
        }
        if self.trigger != trigger {
            self.set_trigger(trigger);
        }
        loop {
            self.clear_pending();
            self.enable_interrupt();
            self.armed_wait().await;
            if self.input.get_level() == level {
                break;
            }
        }
    }

    /// Suspend until [`on_interrupt`] masks this slot's NVIC line — the "fired"
    /// signal — and wakes the registered waker. Registers the waker *before* the
    /// masked-state check, so an edge that lands in the window still wakes us.
    async fn armed_wait(&self) {
        let slot = self.slot as usize;
        let irq = self.irq;
        poll_fn(|cx| {
            WAKERS[slot].register(cx.waker());
            if NVIC::is_enabled(irq) {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
    }

    /// Release the EXDEVICE slot and return the plain [`Input`]. Masks the NVIC
    /// line, closes the INTC gate, frees the slot in the mux, parks its trigger at
    /// level-high (the unused-slot convention) and clears any latch.
    pub fn release(mut self) -> Input<R> {
        self.disable_interrupt();
        // Close the INTC gate opened by `into_interrupt` (mirror of its `enable`).
        crate::interrupt::disable(self.irq);
        set_gpioint_config(self.slot, Trigger::LevelHigh, false);
        critical_section::with(|_| intsel_write(self.slot, SLOT_UNUSED));
        self.clear_pending();
        self.input
    }
}

impl<R: PinReg> embedded_hal::digital::ErrorType for InterruptInput<R> {
    type Error = core::convert::Infallible;
}

impl<R: PinReg> embedded_hal::digital::InputPin for InterruptInput<R> {
    fn is_high(&mut self) -> Result<bool, Self::Error> {
        Ok(self.input.is_high())
    }
    fn is_low(&mut self) -> Result<bool, Self::Error> {
        Ok(self.input.is_low())
    }
}

impl<R: PinReg> embedded_hal_async::digital::Wait for InterruptInput<R> {
    async fn wait_for_high(&mut self) -> Result<(), Self::Error> {
        self.wait_level_async(Trigger::LevelHigh, Level::High).await;
        Ok(())
    }
    async fn wait_for_low(&mut self) -> Result<(), Self::Error> {
        self.wait_level_async(Trigger::LevelLow, Level::Low).await;
        Ok(())
    }
    async fn wait_for_rising_edge(&mut self) -> Result<(), Self::Error> {
        self.wait_edge_async(Trigger::RisingEdge).await;
        Ok(())
    }
    async fn wait_for_falling_edge(&mut self) -> Result<(), Self::Error> {
        self.wait_edge_async(Trigger::FallingEdge).await;
        Ok(())
    }
    async fn wait_for_any_edge(&mut self) -> Result<(), Self::Error> {
        self.wait_edge_async(Trigger::AnyEdge).await;
        Ok(())
    }
}

/// Per-port split structs.
///
/// Mirrors the convention used by `stm32f4xx-hal` (`gpioa::Parts`),
/// `nrf-hal` (`p0::Parts`), etc. Construct via [`Parts::new`], which
/// consumes the corresponding PAC singleton — proving exclusive access
/// so no `unsafe` is needed at the call site.
pub mod pins {
    use super::GpioPin;
    use crate::pac;

    /// GPIO pins accessible via the TOPREG GP_* registers.
    ///
    /// The four `gp_i2s1_*` pins drive the Spresense main-board LEDs
    /// (`gp_i2s1_bck` = LED0, `gp_i2s1_lrck` = LED1, `gp_i2s1_data_in` = LED2,
    /// `gp_i2s1_data_out` = LED3). The remaining fields are the digital GPIO
    /// pins broken out on the main-board headers JP1 and JP2; field names follow
    /// the CXD5602 signal name, and the doc comments give the Arduino pin label.
    /// Pins default to mode0 (GPIO); selecting an alternate function (UART2,
    /// I2S0, SPI5, I2C0, …) is done via the `IOCAPP_IOMD` / `IOCSYS_IOMD1`
    /// mode-mux registers, which share a group across all pins of a peripheral.
    pub struct Parts {
        /// SPI0_CS_X — UART1_TX / on-board console.
        /// Func0 = GPIO, Func1 = UART1, Func2 = SPI0.
        pub gp_spi0_cs_x: GpioPin<pac::topreg::GpSpi0CsX>,
        /// SPI0_SCK — UART1_RX / on-board console.
        /// Func0 = GPIO, Func1 = UART1, Func2 = SPI0.
        pub gp_spi0_sck: GpioPin<pac::topreg::GpSpi0Sck>,
        pub gp_i2s1_bck: GpioPin<pac::topreg::GpI2s1Bck>,
        pub gp_i2s1_lrck: GpioPin<pac::topreg::GpI2s1Lrck>,
        pub gp_i2s1_data_in: GpioPin<pac::topreg::GpI2s1DataIn>,
        pub gp_i2s1_data_out: GpioPin<pac::topreg::GpI2s1DataOut>,
        pub gp_i2c4_bck: GpioPin<pac::topreg::GpI2c4Bck>,

        // --- JP1 header ---
        /// JP1 pin 2 — UART2_TX (Arduino D01).
        pub gp_uart2_txd: GpioPin<pac::topreg::GpUart2Txd>,
        /// JP1 pin 3 — UART2_RX (Arduino D00).
        pub gp_uart2_rxd: GpioPin<pac::topreg::GpUart2Rxd>,
        /// JP1 pin 4 — UART2_RTS (Arduino D28).
        pub gp_uart2_rts: GpioPin<pac::topreg::GpUart2Rts>,
        /// JP1 pin 5 — UART2_CTS (Arduino D27).
        pub gp_uart2_cts: GpioPin<pac::topreg::GpUart2Cts>,
        /// JP1 pin 6 — I2S0_BCK (Arduino D26).
        pub gp_i2s0_bck: GpioPin<pac::topreg::GpI2s0Bck>,
        /// JP1 pin 7 — I2S0_LRCK (Arduino D25).
        pub gp_i2s0_lrck: GpioPin<pac::topreg::GpI2s0Lrck>,
        /// JP1 pin 8 — SPI5_CS_X / EMMC_CMD (Arduino D24).
        pub gp_emmc_cmd: GpioPin<pac::topreg::GpEmmcCmd>,
        /// JP1 pin 9 — SPI5_SCK / EMMC_CLK (Arduino D23).
        pub gp_emmc_clk: GpioPin<pac::topreg::GpEmmcClk>,
        /// JP1 pin 12 — SEN_IRQ (Arduino D22).
        pub gp_sen_irq_in: GpioPin<pac::topreg::GpSenIrqIn>,

        // --- JP2 header ---
        /// JP2 pin 4 — GPIO / EMMC_DATA3 (Arduino D21).
        pub gp_emmc_data3: GpioPin<pac::topreg::GpEmmcData3>,
        /// JP2 pin 5 — GPIO / EMMC_DATA2 (Arduino D20).
        pub gp_emmc_data2: GpioPin<pac::topreg::GpEmmcData2>,
        /// JP2 pin 6 — I2S0_DATA_IN (Arduino D19).
        pub gp_i2s0_data_in: GpioPin<pac::topreg::GpI2s0DataIn>,
        /// JP2 pin 7 — I2S0_DATA_OUT (Arduino D18).
        pub gp_i2s0_data_out: GpioPin<pac::topreg::GpI2s0DataOut>,
        /// JP2 pin 8 — SPI5_MISO / EMMC_DATA1 (Arduino D17).
        pub gp_emmc_data1: GpioPin<pac::topreg::GpEmmcData1>,
        /// JP2 pin 9 — SPI5_MOSI / EMMC_DATA0 (Arduino D16).
        pub gp_emmc_data0: GpioPin<pac::topreg::GpEmmcData0>,
        /// JP2 pin 11 — I2C0_SCL / I2C0_BCK (Arduino D15).
        pub gp_i2c0_bck: GpioPin<pac::topreg::GpI2c0Bck>,
        /// JP2 pin 12 — I2C0_SDA / I2C0_BDT (Arduino D14).
        pub gp_i2c0_bdt: GpioPin<pac::topreg::GpI2c0Bdt>,
    }

    impl Parts {
        pub fn new(_topreg: pac::Topreg) -> Self {
            // Safety: ownership of `pac::Topreg` — obtainable only via
            // `pac::Peripherals::take()` — guarantees no other code holds
            // a reference to this register block. The accessor in
            // `crate::regs::topreg()` documents the general aliasing invariant.
            let block = crate::regs::topreg();
            Self {
                gp_spi0_cs_x: unsafe { GpioPin::new(block.gp_spi0_cs_x()) },
                gp_spi0_sck: unsafe { GpioPin::new(block.gp_spi0_sck()) },
                gp_i2s1_bck: unsafe { GpioPin::new(block.gp_i2s1_bck()) },
                gp_i2s1_lrck: unsafe { GpioPin::new(block.gp_i2s1_lrck()) },
                gp_i2s1_data_in: unsafe { GpioPin::new(block.gp_i2s1_data_in()) },
                gp_i2s1_data_out: unsafe { GpioPin::new(block.gp_i2s1_data_out()) },
                gp_i2c4_bck: unsafe { GpioPin::new(block.gp_i2c4_bck()) },

                // JP1 header
                gp_uart2_txd: unsafe { GpioPin::new(block.gp_uart2_txd()) },
                gp_uart2_rxd: unsafe { GpioPin::new(block.gp_uart2_rxd()) },
                gp_uart2_rts: unsafe { GpioPin::new(block.gp_uart2_rts()) },
                gp_uart2_cts: unsafe { GpioPin::new(block.gp_uart2_cts()) },
                gp_i2s0_bck: unsafe { GpioPin::new(block.gp_i2s0_bck()) },
                gp_i2s0_lrck: unsafe { GpioPin::new(block.gp_i2s0_lrck()) },
                gp_emmc_cmd: unsafe { GpioPin::new(block.gp_emmc_cmd()) },
                gp_emmc_clk: unsafe { GpioPin::new(block.gp_emmc_clk()) },
                gp_sen_irq_in: unsafe { GpioPin::new(block.gp_sen_irq_in()) },

                // JP2 header
                gp_emmc_data3: unsafe { GpioPin::new(block.gp_emmc_data3()) },
                gp_emmc_data2: unsafe { GpioPin::new(block.gp_emmc_data2()) },
                gp_i2s0_data_in: unsafe { GpioPin::new(block.gp_i2s0_data_in()) },
                gp_i2s0_data_out: unsafe { GpioPin::new(block.gp_i2s0_data_out()) },
                gp_emmc_data1: unsafe { GpioPin::new(block.gp_emmc_data1()) },
                gp_emmc_data0: unsafe { GpioPin::new(block.gp_emmc_data0()) },
                gp_i2c0_bck: unsafe { GpioPin::new(block.gp_i2c0_bck()) },
                gp_i2c0_bdt: unsafe { GpioPin::new(block.gp_i2c0_bdt()) },
            }
        }
    }
}
