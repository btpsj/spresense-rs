//! On-hardware PWM loopback test.
//!
//! Configures PWM0 and reads back the signal via a GPIO interrupt input to
//! measure the actual output frequency and duty cycle.
//!
//! # Wiring required before running
//!
//! Connect a jumper wire between:
//!   * **PWM0 output** — the PWM0 pin on the Spresense extension connector
//!   * **D22 / SEN_IRQ_IN** (JP1 pin 12) — used as the GPIO interrupt input
//!
//! Both pins are 1.8 V; do not connect to 3.3 V or 5 V.
//!
//! # Measurement technique
//!
//! Each frequency/duty test calls [`measure`], which:
//! 1. Waits for a rising edge → records always-on 32.768 kHz RTC counter T1.
//! 2. Waits for the falling edge → records T2 (end of HIGH phase).
//! 3. Waits for the next rising edge → records T3 (end of period).
//!
//! ```text
//! period_us   = (T3 − T1) × 1_000_000 / 32_768
//! duty_pct    = (T2 − T1) × 100 / (T3 − T1)
//! ```
//!
//! At 1 kHz one PWM period = 32.768 RTC ticks, giving ≈ 3% resolution —
//! well within the ±8 % / ±6 pp tolerances used in the assertions.
//!
//! # Interrupt forwarding
//!
//! `SEN_IRQ_IN` (pin 37) is in the SYS domain. With no other SYS-domain pins
//! configured for interrupt in this test, `into_interrupt` allocates it to
//! SYS slot 0 → **EXDEVICE_0**.

#![no_std]
#![no_main]

use cortex_m_rt as _;
use cxd56_hal::{async_delay, gpio::Wait, pac, uart::Uart};
use defmt_serial as _;
use panic_probe as _;
use static_cell::StaticCell;

use cxd56_hal::pac::{Interrupt, interrupt};

static SERIAL: StaticCell<Uart<'static, pac::Uart1>> = StaticCell::new();
// UART1 borrows the `Clock` for its lifetime (COM is a Dyn clock), so the
// `Clock` must outlive the `'static` UART stored in `SERIAL`.
static CLOCK: StaticCell<cxd56_hal::clocks::Clock<cxd56_hal::clocks::Hp>> = StaticCell::new();

/// Forward the D22 / SEN_IRQ_IN EXDEVICE interrupt to the GPIO async runtime.
/// SEN_IRQ_IN (pin 37) is the first SYS-domain pin mapped here → slot 0.
#[interrupt]
fn EXDEVICE_0() {
    cxd56_hal::gpio::on_interrupt(Interrupt::EXDEVICE_0);
}

/// Forward the async-delay IRQ (needed by the edge-arm settle inside
/// `wait_for_*_edge`).
#[cfg(feature = "backing-rtc")]
#[interrupt]
fn RTC0_A0() {
    async_delay::on_interrupt(Interrupt::RTC0_A0);
}
#[cfg(feature = "backing-timer")]
#[interrupt]
fn TIMER0() {
    async_delay::on_interrupt(Interrupt::TIMER0);
}

// ── minimal async runtime ─────────────────────────────────────────────────────

mod rt {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake, drop_noop);
    fn clone(_: *const ()) -> RawWaker { RawWaker::new(core::ptr::null(), &VTABLE) }
    fn wake(_: *const ()) { cortex_m::asm::sev(); }
    fn drop_noop(_: *const ()) {}

    fn make_waker() -> Waker {
        unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
    }

    pub fn block_on<F: Future>(fut: F) -> F::Output {
        let mut fut = pin!(fut);
        let waker = make_waker();
        let mut cx = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(val) = fut.as_mut().poll(&mut cx) {
                return val;
            }
            cortex_m::asm::wfe();
        }
    }

}

// ── RTC timestamp helper ──────────────────────────────────────────────────────

/// Read the always-on 47-bit RTC counter (32.768 kHz).
fn rtc_now() -> u64 {
    let rtc = unsafe { &*pac::Rtc0::PTR };
    loop {
        let hi = rtc.rtpostcnt().read().bits();
        let lo = rtc.rtprecnt().read().bits() & 0x7fff;
        if hi == rtc.rtpostcnt().read().bits() {
            return ((hi as u64) << 15) | lo as u64;
        }
    }
}

// ── measurement helper ────────────────────────────────────────────────────────

/// Measured PWM characteristics from one complete cycle.
struct Measurement {
    /// Measured period in microseconds.
    period_us: u32,
    /// Measured duty cycle as an integer percentage.
    duty_pct: u32,
}

/// Capture one complete PWM cycle by waiting for three edges on `gpio`.
///
/// Timestamps are taken from the always-on 32.768 kHz RTC counter.
/// Returns immediately once the third edge (second rising) fires.
async fn measure(gpio: &mut cxd56_hal::gpio::InterruptInput<pac::topreg::GpSenIrqIn>)
    -> Measurement
{
    // T1 — first rising edge (start of period).
    gpio.wait_for_rising_edge().await.unwrap();
    let t1 = rtc_now();

    // T2 — falling edge (end of HIGH phase).
    gpio.wait_for_falling_edge().await.unwrap();
    let t2 = rtc_now();

    // T3 — second rising edge (end of period).
    gpio.wait_for_rising_edge().await.unwrap();
    let t3 = rtc_now();

    // ticks are monotonically increasing; all subtractions are safe.
    let period_ticks = (t3 - t1) as u32;
    let high_ticks   = (t2 - t1) as u32;

    // RTC runs at 32_768 Hz → each tick is 1_000_000 / 32_768 ≈ 30.5 µs.
    let period_us = (period_ticks as u64 * 1_000_000 / 32_768) as u32;
    let duty_pct  = (high_ticks * 100).checked_div(period_ticks).unwrap_or(0);

    Measurement { period_us, duty_pct }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[defmt_test::tests]
mod tests {
    use defmt::assert;

    use cxd56_hal::clocks::{Config, RccExt};
    use cxd56_hal::gpio::{InterruptInput, Trigger, Wait};
    use cxd56_hal::gpio::pins::Parts;
    use cxd56_hal::pwm::PwmBlock;
    use cxd56_hal::{async_delay, pac};
    use cxd56_hal::uart::{Uart, Uart1Pins};

    struct State {
        pwm: cxd56_hal::pwm::Pwm<0>,
        sense: InterruptInput<pac::topreg::GpSenIrqIn>,
    }

    #[init]
    fn init() -> State {
        let pac = pac::Peripherals::take().unwrap();
        // Promote the clock to `'static` so the UART1 console (which borrows it)
        // can be stored in the `'static` `SERIAL` cell.
        let clock = crate::CLOCK.init(pac.crg.constrain(Config::default()).into_hp_clock().expect("lock Hp"));

        async_delay::init(clock);

        let parts = Parts::new(pac.topreg);
        let uart1_pins = Uart1Pins { tx: parts.gp_spi0_cs_x, rx: parts.gp_spi0_sck };
        let uart = Uart::new(pac.uart1, uart1_pins, Default::default(), clock)
            .expect("uart1 init");
        defmt_serial::defmt_serial(crate::SERIAL.init(uart));

        let clocks = clock.freeze();
        let pwm_block = PwmBlock::new(pac.pwm, &clocks).expect("pwm init");
        let (ch0, _ch1) = pwm_block.split();

        // D22 / SEN_IRQ_IN as the loopback GPIO input.  With no other SYS
        // interrupt pins configured, this allocates to SYS slot 0 → EXDEVICE_0.
        let sense = parts
            .gp_sen_irq_in
            .into_floating_input()
            .into_interrupt(Trigger::RisingEdge, false)
            .expect("no free EXDEVICE slot");

        State { pwm: ch0, sense }
    }

    /// duty=0 → output stays LOW.
    #[test]
    fn const_low(s: &mut State) {
        s.pwm.configure(1_000, 0).unwrap();
        cortex_m::asm::delay(200_000); // settle
        assert!(s.sense.is_low(), "duty=0 should hold output LOW");
    }

    /// duty=0xFFFF → output stays HIGH.
    #[test]
    fn const_high(s: &mut State) {
        s.pwm.configure(1_000, 0xffff).unwrap();
        cortex_m::asm::delay(200_000);
        assert!(s.sense.is_high(), "duty=0xFFFF should hold output HIGH");
    }

    /// 1 kHz, 50 % duty. Period ≈ 1000 µs, duty ≈ 50 %.
    #[test]
    fn freq_1khz_50pct(s: &mut State) {
        s.pwm.configure(1_000, 0x8000).unwrap();
        let m = crate::rt::block_on(crate::measure(&mut s.sense));
        let period_err = (m.period_us as i32 - 1_000).unsigned_abs();
        let duty_err   = (m.duty_pct  as i32 - 50).unsigned_abs();
        defmt::info!("1kHz/50%: period={}µs duty={}%", m.period_us, m.duty_pct);
        assert!(period_err <= 80, "1kHz period {}µs outside ±8% tolerance", m.period_us);
        assert!(duty_err   <= 6,  "50% duty {}% outside ±6pp tolerance",    m.duty_pct);
    }

    /// 1 kHz, 25 % duty.
    #[test]
    fn freq_1khz_25pct(s: &mut State) {
        s.pwm.configure(1_000, 0x4000).unwrap();
        let m = crate::rt::block_on(crate::measure(&mut s.sense));
        let period_err = (m.period_us as i32 - 1_000).unsigned_abs();
        let duty_err   = (m.duty_pct  as i32 - 25).unsigned_abs();
        defmt::info!("1kHz/25%: period={}µs duty={}%", m.period_us, m.duty_pct);
        assert!(period_err <= 80, "1kHz period {}µs outside ±8% tolerance", m.period_us);
        assert!(duty_err   <= 6,  "25% duty {}% outside ±6pp tolerance",    m.duty_pct);
    }

    /// Dynamic duty update while running — only PARAM written, no stop/restart.
    #[test]
    fn dynamic_duty_update(s: &mut State) {
        s.pwm.configure(1_000, 0x8000).unwrap(); // 50 %
        let m1 = crate::rt::block_on(crate::measure(&mut s.sense));

        s.pwm.set_duty(0x4000).unwrap(); // → 25 %, channel stays enabled
        // Discard the first possibly-partial cycle after the PARAM update.
        crate::rt::block_on(async { s.sense.wait_for_rising_edge().await });
        let m2 = crate::rt::block_on(crate::measure(&mut s.sense));

        defmt::info!("dynamic update: before={}% after={}%", m1.duty_pct, m2.duty_pct);
        let period_err = (m2.period_us as i32 - 1_000).unsigned_abs();
        let duty_err   = (m2.duty_pct  as i32 - 25).unsigned_abs();
        assert!(period_err <= 80, "freq changed during duty update: {}µs", m2.period_us);
        assert!(duty_err   <= 6,  "duty after update {}% not ≈25%",        m2.duty_pct);
    }

    /// stop() drives EN=0 → output goes LOW.
    #[test]
    fn stop_goes_low(s: &mut State) {
        s.pwm.configure(1_000, 0x8000).unwrap();
        cortex_m::asm::delay(100_000);
        s.pwm.stop();
        cortex_m::asm::delay(200_000);
        assert!(s.sense.is_low(), "stop() should drive output LOW");
    }
}
