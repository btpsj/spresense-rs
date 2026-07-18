//! Clock subsystem.
//!
//! # Architecture
//!
//! The CXD5602 SYSIOP (Cortex-M0+) owns the PLL, root mux, and bus dividers.
//! The User Manual (§3.5.1 p.167, §3.13.3.3 p.974) explicitly states those
//! registers are "controlled by the API — use as RO registers."  User code:
//!
//! - **Reads** clock rates via [`Crg::freeze`] (a re-samplable snapshot).
//! - **Controls** per-peripheral gates, software resets, and the APP-local M/N
//!   "gear" dividers (UART2, USB, SDIO, SPI4/5) directly via the APP-local CRG
//!   at `0x0201_1000`. The gear divider sets a peripheral's base clock to
//!   `appsmp / M`; choose the divisors in [`Config::gear`] at constrain time,
//!   retune at runtime with [`Clock::set_gear`] (raw divisor) or
//!   [`Clock::set_spi_gear`] (target a max SPI frequency), and read the live
//!   register back with [`PeripheralId::gear_divisor`].
//! - **Locks** the CPU/bus operating point (HV: APP 156 MHz / LV: APP
//!   31.2 MHz) at construction — [`Crg::into_hp_clock`] /
//!   [`Crg::into_lp_clock`] — over the SYSIOP's ICC CPU-FIFO FREQLOCK
//!   protocol, and moves it only by consuming transition
//!   ([`Clock::into_hp`] / [`Clock::into_lp`]). A [`Clock`] is never
//!   unconstrained; to observe the tree without locking it, sample via
//!   [`ClockRef::from_crg`].
//!
//! # Entry point
//!
//! ```ignore
//! let dp = pac::Peripherals::take().unwrap();
//!
//! // Pick UART2's base clock: appsmp/2 (≈78 MHz at HP) instead of the
//! // default appsmp/4 (≈39 MHz). The divisor is programmed into hardware
//! // when the peripheral is enabled; the snapshot reflects it from the start.
//! let cfg = Config {
//!     gear: GearConfig { img_uart: 2, ..Default::default() },
//!     ..Default::default()
//! };
//! let mut crg = dp.crg.constrain(cfg);
//! let clocks = crg.freeze();
//! PeripheralId::Uart1.enable()?;
//! ```

use crate::pac;
use fugit::Hertz;

pub mod audio;
pub mod buses;
pub mod gate;
pub mod peripheral;
pub mod pm;
pub mod profile;
pub mod pmu;
pub mod reset;
pub mod shared;
pub mod sources;

pub use audio::{AudMclk, audio_clock_disable, audio_clock_enable};
pub use peripheral::{ClockError, GearError, I2cPort, PeripheralId, SpiPort};
pub use pm::{PmError, Perf};
pub use profile::{Clock, Dyn, Fixed, Hp, Lp, PerfState};
pub use shared::{ClockRef, PerfControl};

/// Board-level clock configuration.
///
/// The XOSC frequency and the APP-local gear dividers are configurable —
/// the rest of the clock tree is set by boot ROM. Spresense main board uses
/// a 26 MHz crystal.
#[derive(Copy, Clone, Debug)]
pub struct Config {
    /// XOSC frequency. Spresense main = 26 MHz.
    pub xosc: Hertz<u32>,
    /// APP-local M/N gear divisors for the gear-divided peripherals.
    pub gear: GearConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            xosc: Hertz::<u32>::MHz(26),
            gear: GearConfig::default(),
        }
    }
}

/// APP-local gear divisors (the M field; base clock = `appsmp / M`).
///
/// Programmed into hardware when the peripheral is enabled
/// ([`PeripheralId::enable`]), and used to compute the gear-divided fields of
/// every [`Clocks`]/[`Clock`] snapshot — so a driver's baud/divisor math and
/// the hardware always agree. Defaults match NuttX's bring-up values.
/// Retune at runtime with [`Clock::set_gear`] / [`Clock::set_spi_gear`].
///
/// [`Crg::constrain`](RccExt::constrain) **panics** if a divisor is outside
/// its register range (listed per field).
#[derive(Copy, Clone, Debug)]
pub struct GearConfig {
    /// UART2 (IMG_UART) divisor, `1..=0x7f`. Default 4 (≈39 MHz at HP).
    /// PL011 max baud = base / 16.
    pub img_uart: u32,
    /// SPI4 (IMG_SPI) divisor, `1..=0x7f`. Default 2.
    pub img_spi: u32,
    /// SPI5 (IMG_WSPI) divisor, `1..=0xf`. Default 4.
    pub img_wspi: u32,
    /// USB divisor, `1..=0x3`. Default 2. USB requires a specific base clock
    /// to function — override only if you know the resulting rate is valid.
    pub usb: u32,
    /// SDIO divisor, `1..=0x3`. Default 2 — bounds the card clock.
    pub sdio: u32,
}

impl Default for GearConfig {
    fn default() -> Self {
        Self {
            img_uart: 4,
            img_spi: 2,
            img_wspi: 4,
            usb: 2,
            sdio: 2,
        }
    }
}

/// Extension trait — implemented for [`pac::Crg`].
pub trait RccExt {
    fn constrain(self, cfg: Config) -> Crg;
}

impl RccExt for pac::Crg {
    fn constrain(self, cfg: Config) -> Crg {
        Crg::new(self, cfg)
    }
}

/// Owns the CRG peripheral. Returned by [`RccExt::constrain`].
pub struct Crg {
    _crg: pac::Crg,
    cfg: Config,
}

impl Crg {
    pub(crate) fn new(crg: pac::Crg, cfg: Config) -> Self {
        sources::init_rcosc_cache();
        peripheral::seed_gear_config(cfg.gear);
        let appsmp = buses::appsmp_hz(cfg.xosc.to_Hz());
        pmu::set_appsmp_hz(appsmp);
        Self { _crg: crg, cfg }
    }

    /// Snapshot every readable clock. Cheap; call again after PMU changes
    /// that affect bus dividers.
    pub fn freeze(&self) -> Clocks {
        Clocks::sample(self.cfg)
    }

    /// Consume this `Crg`, **lock the operating point at HV** ([`Perf::Hp`]:
    /// APP CPU 156 MHz, VDD_CORE ≈ 1.0 V), and return the typed [`Clock`].
    ///
    /// Stock-default parity: NuttX with dynamic clock control disabled (the
    /// factory default) runs the maximum system clock permanently. Choose
    /// [`into_lp_clock`](Self::into_lp_clock) for the 0.7 V low-power point
    /// instead. There is deliberately no unconstrained variant — the
    /// cold-boot tree cannot be locked or reliably re-entered; observe it
    /// read-only via [`ClockRef::from_crg`] if needed.
    ///
    /// Sends the SYSIOP PM bring-up pair (`MSGID_BOOT` + `FREQLOCK`) exactly
    /// like stock board init, so the first lock always runs a real handshake
    /// — even after a warm reboot that inherited a previous operating point
    /// (the SYSIOP survives APP resets).
    pub fn into_hp_clock(self) -> Result<Clock<Hp>, PmError> {
        Clock::lock(self)
    }

    /// Consume this `Crg`, **lock the operating point at LV** ([`Perf::Lp`]:
    /// APP CPU 31.2 MHz, VDD_CORE ≈ 0.7 V — the low-power point), and return
    /// the typed [`Clock`]. COM and the other Dyn clocks drop with it, so
    /// UART/SPI/I2C divisors are computed from the slower base rates. See
    /// [`into_hp_clock`](Self::into_hp_clock) for the bring-up handshake
    /// notes.
    pub fn into_lp_clock(self) -> Result<Clock<Lp>, PmError> {
        Clock::lock(self)
    }

    /// Consume this `Crg` and produce a [`PerfControl`] bound to `shared`, the
    /// shared-clock (Option-2) path for `'static` peripherals.
    ///
    /// `shared` must have been initialised from this same `Crg` via
    /// [`ClockRef::from_crg`] and placed in `'static` storage. Unlike
    /// [`into_hp_clock`](Self::into_hp_clock), the returned [`PerfControl`] holds no
    /// borrow of the clock state — `shared` and `perf_ctl` are independent, so
    /// peripherals built with `from_ref(shared)` are `'static`. See
    /// [`PerfControl`] for the safety contract around runtime perf changes.
    ///
    /// ```ignore
    /// use static_cell::StaticCell;
    /// static CLOCK: StaticCell<ClockRef> = StaticCell::new();
    ///
    /// let crg = dp.crg.constrain(Config::default());
    /// let clock_ref: &'static ClockRef = CLOCK.init(ClockRef::from_crg(&crg));
    /// let perf_ctl = crg.into_perf_control(clock_ref);
    /// let uart = Uart::from_ref(pac.uart1, pins, Default::default(), clock_ref)?;
    /// ```
    pub fn into_perf_control(self, shared: &'static ClockRef) -> PerfControl {
        PerfControl::new(shared, self.cfg)
    }

    /// Access the raw PMU sequencer (escape hatch for SCU firmware load etc.).
    pub fn pmu(&mut self) -> pmu::PmuCtl<'_> {
        pmu::PmuCtl::new(self)
    }
}

/// Snapshot of every readable clock on the chip.
///
/// Most fields are derived from the current register state — caller can
/// re-call [`Crg::freeze`] after any operation that mutates a divider. The
/// gear-divided fields (`usb`, `sdio`, `img_uart`, `img_spi`, `img_wspi`)
/// are computed from the *configured* divisors ([`Config::gear`]) rather
/// than the live gear registers, so they report the rate the peripheral
/// runs at once enabled — even when sampled before
/// [`PeripheralId::enable`] programs the hardware.
#[derive(Copy, Clone, Debug)]
pub struct Clocks {
    pub xosc: Hertz<u32>,
    pub rcosc: Hertz<u32>,
    pub rtc: Hertz<u32>,
    pub syspll: Hertz<u32>,

    /// SYS/IOP root bus.
    pub sys: Hertz<u32>,
    pub sys_ahb: Hertz<u32>,
    pub sys_apb: Hertz<u32>,
    pub sys_sfc: Hertz<u32>,

    /// APP CPU (Cortex-M4 user core).
    pub appsmp: Hertz<u32>,

    pub scu: Hertz<u32>,
    pub com: Hertz<u32>,
    pub pmui2c: Hertz<u32>,
    pub usb: Hertz<u32>,
    pub sdio: Hertz<u32>,
    pub hpadc: Hertz<u32>,
    pub lpadc: Hertz<u32>,

    /// IMG-block peripherals on APP_SUB.
    pub img_uart: Hertz<u32>,
    pub img_spi: Hertz<u32>,
    pub img_wspi: Hertz<u32>,
    pub img_vsync: Hertz<u32>,

    pub gps_cpu: Hertz<u32>,
    pub gps_ahb: Hertz<u32>,
}

impl Clocks {
    pub(crate) fn sample(cfg: Config) -> Self {
        let xosc = cfg.xosc.to_Hz();
        let rcosc = sources::rcosc_hz();
        let rtc = sources::rtc_hz();
        let syspll = sources::syspll_hz(xosc);
        let sys = buses::sys_hz(rcosc, rtc, syspll, xosc);
        let sys_ahb = buses::sys_ahb_hz(sys);
        let sys_apb = buses::sys_apb_hz(sys_ahb);
        let sys_sfc = buses::sys_sfc_hz(sys);
        let appsmp = buses::appsmp_hz(xosc);
        let scu = buses::scu_hz(rcosc, rtc, xosc);
        let com = buses::com_hz(sys);
        let pmui2c = buses::pmui2c_hz(sys_apb, rtc, rcosc);
        let usb = buses::usb_hz(appsmp);
        let sdio = buses::sdio_hz(appsmp);
        let hpadc = buses::hpadc_hz(rcosc, rtc);
        let lpadc = buses::lpadc_hz(rcosc, rtc);
        let img_uart = buses::img_uart_hz(appsmp);
        let img_spi = buses::img_spi_hz(appsmp);
        let img_wspi = buses::img_wspi_hz(appsmp);
        let img_vsync = buses::img_vsync_hz(appsmp);
        let gps_cpu = buses::gps_cpu_hz(sys);
        let gps_ahb = buses::gps_ahb_hz(sys);

        Self {
            xosc: Hertz::<u32>::Hz(xosc),
            rcosc: Hertz::<u32>::Hz(rcosc),
            rtc: Hertz::<u32>::Hz(rtc),
            syspll: Hertz::<u32>::Hz(syspll),
            sys: Hertz::<u32>::Hz(sys),
            sys_ahb: Hertz::<u32>::Hz(sys_ahb),
            sys_apb: Hertz::<u32>::Hz(sys_apb),
            sys_sfc: Hertz::<u32>::Hz(sys_sfc),
            appsmp: Hertz::<u32>::Hz(appsmp),
            scu: Hertz::<u32>::Hz(scu),
            com: Hertz::<u32>::Hz(com),
            pmui2c: Hertz::<u32>::Hz(pmui2c),
            usb: Hertz::<u32>::Hz(usb),
            sdio: Hertz::<u32>::Hz(sdio),
            hpadc: Hertz::<u32>::Hz(hpadc),
            lpadc: Hertz::<u32>::Hz(lpadc),
            img_uart: Hertz::<u32>::Hz(img_uart),
            img_spi: Hertz::<u32>::Hz(img_spi),
            img_wspi: Hertz::<u32>::Hz(img_wspi),
            img_vsync: Hertz::<u32>::Hz(img_vsync),
            gps_cpu: Hertz::<u32>::Hz(gps_cpu),
            gps_ahb: Hertz::<u32>::Hz(gps_ahb),
        }
    }

    /// SPI port base clock. Mirrors `cxd56_get_spi_baseclock` (cxd56_clock.c:1733).
    pub fn spi(&self, port: SpiPort) -> Hertz<u32> {
        match port {
            SpiPort::Spi0 => self.com,
            SpiPort::Spi3 => self.scu,
            SpiPort::Spi4 => self.img_spi,
            SpiPort::Spi5 => self.img_wspi,
        }
    }

    /// I2C port base clock. Mirrors `cxd56_get_i2c_baseclock` (cxd56_clock.c:1766).
    pub fn i2c(&self, port: I2cPort) -> Hertz<u32> {
        match port {
            I2cPort::I2c0 | I2cPort::I2c1 => self.scu,
            I2cPort::I2c2 => self.com,
            I2cPort::I2c4 => self.pmui2c,
        }
    }
}
