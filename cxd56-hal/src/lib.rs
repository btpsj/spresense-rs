#![no_std]

pub use cxd56_pac_svd2rust as pac;

// Exactly one async-time backend (see Cargo.toml `[features]`). The `time-driver-*`
// pair implement embassy's `Driver` (concurrent timers via a queue); the
// `async-delay-*` pair are the hand-rolled single-in-flight path. Pick a non-default
// one with `--no-default-features`.
#[cfg(any(
    all(feature = "time-driver-rtc", feature = "time-driver-timer"),
    all(feature = "time-driver-rtc", feature = "async-delay-rtc"),
    all(feature = "time-driver-rtc", feature = "async-delay-timer"),
    all(feature = "time-driver-timer", feature = "async-delay-rtc"),
    all(feature = "time-driver-timer", feature = "async-delay-timer"),
    all(feature = "async-delay-rtc", feature = "async-delay-timer"),
))]
compile_error!(
    "enable exactly ONE async-time backend (time-driver-rtc, time-driver-timer, \
     async-delay-rtc, or async-delay-timer) — use --no-default-features to pick a \
     non-default one"
);
#[cfg(not(any(
    feature = "time-driver-rtc",
    feature = "time-driver-timer",
    feature = "async-delay-rtc",
    feature = "async-delay-timer",
)))]
compile_error!("enable exactly one async-time backend (time-driver-rtc is the default)");

#[cfg(any(feature = "async-delay-rtc", feature = "async-delay-timer"))]
pub mod async_delay;
#[cfg(any(feature = "time-driver-rtc", feature = "time-driver-timer"))]
pub mod time;
pub mod clocks;
#[cfg(feature = "critical-section-impl")]
pub mod critical_section_impl;
pub mod delay;
pub mod delay_alt;
pub mod farapi;
pub mod gpio;
pub mod i2c;
pub mod adc;
pub mod audio_aca;
pub mod i2c_alt;
pub mod i2s_alt;
pub mod interrupt;
pub mod multicore;
pub mod pmic;
pub mod pwm;
pub mod sleep;
pub mod spi_alt;
pub(crate) mod regs;
pub mod timer;
pub mod uart;
pub mod uart_alt;
pub mod watchdog;
