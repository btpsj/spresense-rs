use super::peripheral::{GearError, PeripheralId};
use super::pm::{self, Perf, PmError};
use super::{Clocks, Crg, pmu};
use fugit::Hertz;

/// A perf-independent clock sample. `Copy` — safe to hold after the
/// originating [`Clock`] is dropped.
#[derive(Copy, Clone)]
pub struct Fixed(pub Hertz<u32>);

impl Fixed {
    pub fn hz(&self) -> Hertz<u32> {
        self.0
    }
}

/// A perf-dependent clock sample. Not `Copy`; no public constructor.
/// Must be borrowed from a live [`Clock`], keeping the `Clock` borrow
/// intact.
pub struct Dyn(Hertz<u32>);

impl Dyn {
    pub fn hz(&self) -> Hertz<u32> {
        self.0
    }
}

/// Owned typed clock snapshot. Consumes the [`Crg`] peripheral.
///
/// Obtained via [`Crg::into_clock`]. While this value (or any peripheral
/// borrowing a [`Dyn`] field from it) is alive, the `Clock` cannot be
/// borrowed mutably, preventing [`request_perf`](Clock::request_perf) from
/// silently invalidating a peripheral's baud/gear configuration.
///
/// [`Fixed`] fields are `pub` and `Copy` (no borrow) and hold only genuinely
/// perf-invariant clocks (`xosc`/`rcosc`/`rtc`/`scu`/`hpadc`/`lpadc`). Every
/// perf-dependent clock is a [`Dyn`] field, accessible only by reference via the
/// accessor methods, tying its lifetime to `&self` so the borrow checker blocks
/// [`request_perf`](Clock::request_perf) while a peripheral depends on one.
///
/// The perf-dependent set is the SYSIOP-tree `syspll`/`sys`/`sys_ahb`/`sys_apb`/
/// `sys_sfc`/`com`/`pmui2c`/`gps_cpu`/`gps_ahb` (User Manual SYSIOP-825/826,
/// UART-791/792) plus the APP-domain `appsmp`/`usb`/`sdio`/`img_*`. They are
/// refreshed on each [`request_perf`](Clock::request_perf) / `set_gear` (via the
/// private `resample_dyn`); because they are `Dyn`, a peripheral built from one
/// (e.g. an `uart_alt` UART1 sized from `com`) holds the `Clock` borrow and
/// cannot outlive the rate it was configured against.
pub struct Clock {
    crg: Crg,
    // `Copy` snapshots (no borrow). Perf-INVARIANT only:
    // xosc/rcosc/rtc/scu/hpadc/lpadc.
    pub xosc: Fixed,
    pub rcosc: Fixed,
    pub rtc: Fixed,
    pub scu: Fixed,
    pub hpadc: Fixed,
    pub lpadc: Fixed,
    // Perf-dependent — private to prevent move-out that would decouple the
    // borrow from the owning `Clock`. Access via `&self` methods below.
    // SYSIOP tree (refreshed by `request_perf`/`set_gear`; `gps_*` derive from
    // `sys`, `pmui2c` from `sys_apb`):
    syspll: Dyn,
    sys: Dyn,
    sys_ahb: Dyn,
    sys_apb: Dyn,
    sys_sfc: Dyn,
    com: Dyn,
    pmui2c: Dyn,
    gps_cpu: Dyn,
    gps_ahb: Dyn,
    // APP domain:
    appsmp: Dyn,
    usb: Dyn,
    sdio: Dyn,
    img_uart: Dyn,
    img_spi: Dyn,
    img_wspi: Dyn,
    img_vsync: Dyn,
}

impl Clock {
    /// Consume `crg` and sample all clocks.
    pub fn new(crg: Crg) -> Self {
        let c = Clocks::sample(crg.cfg);
        Self {
            crg,
            xosc: Fixed(c.xosc),
            rcosc: Fixed(c.rcosc),
            rtc: Fixed(c.rtc),
            scu: Fixed(c.scu),
            hpadc: Fixed(c.hpadc),
            lpadc: Fixed(c.lpadc),
            syspll: Dyn(c.syspll),
            sys: Dyn(c.sys),
            sys_ahb: Dyn(c.sys_ahb),
            sys_apb: Dyn(c.sys_apb),
            sys_sfc: Dyn(c.sys_sfc),
            com: Dyn(c.com),
            pmui2c: Dyn(c.pmui2c),
            gps_cpu: Dyn(c.gps_cpu),
            gps_ahb: Dyn(c.gps_ahb),
            appsmp: Dyn(c.appsmp),
            usb: Dyn(c.usb),
            sdio: Dyn(c.sdio),
            img_uart: Dyn(c.img_uart),
            img_spi: Dyn(c.img_spi),
            img_wspi: Dyn(c.img_wspi),
            img_vsync: Dyn(c.img_vsync),
        }
    }

    /// Request a CPU/bus operating-point change from the SYSIOP loader firmware.
    ///
    /// Drives the ICC `FREQLOCK` → `CLK_CHG_START` / `CLK_CHG_END` handshake
    /// and updates the dynamic clock fields once the new operating point is
    /// stable.
    ///
    /// Operating points (XOSC = 26 MHz, User Manual Table APP-807/808):
    /// - [`Perf::Hp`]: APP CPU ~156 MHz, VDD_CORE = 1.0 V
    /// - [`Perf::Lp`]: APP CPU  ~39 MHz, VDD_CORE = 0.7 V
    ///
    /// This call **blocks** (polls the CPU FIFO) until the SYSIOP confirms the
    /// transition. While any peripheral holds a borrow of a [`Dyn`] field from
    /// this `Clock`, the borrow checker prevents calling `request_perf` (which
    /// requires `&mut self`).
    pub fn request_perf(&mut self, perf: Perf) -> Result<(), PmError> {
        pm::request_perf(perf)?;
        self.resample_dyn();
        Ok(())
    }

    /// Set the APP-local gear divider for `id`, making its base clock
    /// `appsmp / divisor`, and re-sample the dynamic clock fields.
    ///
    /// Valid for [`PeripheralId::ImgUart`] (UART2), [`PeripheralId::Spi4`],
    /// [`PeripheralId::Spi5`], [`PeripheralId::Usb`], and
    /// [`PeripheralId::Sdio`]; `divisor` must lie in `1..=max` (`0x7f` for
    /// `ImgUart`/`Spi4`, `0xf` for `Spi5`, `0x3` for `Usb`/`Sdio`). The
    /// initial divisors come from [`Config::gear`](super::Config::gear).
    ///
    /// Requires `&mut self`: while any peripheral driver borrows a [`Dyn`]
    /// field from this `Clock` (e.g. a live UART2 or SPI5), the borrow
    /// checker rejects this call — the same protection that keeps
    /// [`request_perf`](Clock::request_perf) from invalidating a driver's
    /// baud/divisor math. Reconstruct the driver afterwards so it computes
    /// its divisors from the new rate.
    ///
    /// # Caveats
    /// `Usb` requires a specific base clock to function and `Sdio`'s divider
    /// bounds the card clock — override their defaults only if you know the
    /// resulting rate is valid.
    pub fn set_gear(&mut self, id: PeripheralId, divisor: u32) -> Result<(), GearError> {
        id.set_gear(divisor)?;
        self.resample_dyn();
        Ok(())
    }

    /// Set an SPI port's gear divider so the resulting SCK frequency is **at
    /// most** `maxfreq`, and re-sample the dynamic clock fields. Valid for
    /// [`PeripheralId::Spi4`] and [`PeripheralId::Spi5`]. Mirrors NuttX's
    /// `cxd56_spi_clock_gear_adjust`.
    ///
    /// Requires `&mut self` — see [`set_gear`](Clock::set_gear) for the
    /// borrow-checker protection this provides.
    pub fn set_spi_gear(&mut self, port: PeripheralId, maxfreq: Hertz<u32>) -> Result<(), GearError> {
        let appsmp = self.appsmp.hz();
        port.set_spi_gear(appsmp, maxfreq)?;
        self.resample_dyn();
        Ok(())
    }

    /// Re-sample the perf-dependent fields after an operation that changes
    /// them (operating-point change, gear rewrite).
    ///
    /// Besides the [`Dyn`] APP-domain clocks, the SYSIOP-tree clocks move with
    /// the voltage mode too — the operating point reconfigures SYSPLL and the
    /// SYS dividers (User Manual SYSIOP-825/826 & UART-791/792: COM 48.75 MHz HP
    /// → 32.5 MHz LP). They are typed [`Fixed`] for ergonomics but are *not*
    /// perf-invariant, so refresh their cached snapshots here; otherwise a
    /// freshly-built COM-bus peripheral (e.g. a `uart` UART1, whose baud
    /// divisor is computed from `self.com`) would use the stale boot rate after
    /// a perf change. The always-on/sensor clocks (`xosc`/`rcosc`/`rtc`/`scu`/
    /// `hpadc`/`lpadc`) are genuinely perf-invariant and need no refresh.
    fn resample_dyn(&mut self) {
        let c = Clocks::sample(self.crg.cfg);
        // Perf-dependent SYSIOP-tree clocks (`gps_*` derive from `sys`,
        // `pmui2c` from `sys_apb`).
        self.syspll  = Dyn(c.syspll);
        self.sys     = Dyn(c.sys);
        self.sys_ahb = Dyn(c.sys_ahb);
        self.sys_apb = Dyn(c.sys_apb);
        self.sys_sfc = Dyn(c.sys_sfc);
        self.com     = Dyn(c.com);
        self.pmui2c  = Dyn(c.pmui2c);
        self.gps_cpu = Dyn(c.gps_cpu);
        self.gps_ahb = Dyn(c.gps_ahb);
        // Perf-dependent APP-domain clocks.
        self.appsmp    = Dyn(c.appsmp);
        self.usb       = Dyn(c.usb);
        self.sdio      = Dyn(c.sdio);
        self.img_uart  = Dyn(c.img_uart);
        self.img_spi   = Dyn(c.img_spi);
        self.img_wspi  = Dyn(c.img_wspi);
        self.img_vsync = Dyn(c.img_vsync);
    }

    /// Snapshot every readable clock. Cheap; delegates to the owned `Crg`.
    pub fn freeze(&self) -> Clocks {
        self.crg.freeze()
    }

    /// Access the raw PMU sequencer (escape hatch for SCU firmware load etc.).
    pub fn pmu(&mut self) -> pmu::PmuCtl<'_> {
        self.crg.pmu()
    }

    pub fn appsmp(&self) -> &Dyn {
        &self.appsmp
    }

    /// CPU/AHB base clock — the watchdog (SP805) timer's clock source.
    ///
    /// Derived from the perf-dependent [`appsmp`](Self::appsmp) clock via the
    /// AHB gear ratio (mirrors `cxd56_get_cpu_baseclk`). Returns a `Copy`
    /// [`Hertz`]; callers that need this value to stay valid across an
    /// operating-point change should hold the `Clock` borrow — see
    /// [`watchdog::Watchdog`](crate::watchdog::Watchdog).
    pub fn cpu_baseclk(&self) -> Hertz<u32> {
        Hertz::<u32>::Hz(super::buses::cpu_baseclk_hz(self.appsmp.hz().to_Hz()))
    }

    pub fn usb(&self) -> &Dyn {
        &self.usb
    }
    pub fn sdio(&self) -> &Dyn {
        &self.sdio
    }
    pub fn img_uart(&self) -> &Dyn {
        &self.img_uart
    }
    pub fn img_spi(&self) -> &Dyn {
        &self.img_spi
    }
    pub fn img_wspi(&self) -> &Dyn {
        &self.img_wspi
    }
    pub fn img_vsync(&self) -> &Dyn {
        &self.img_vsync
    }

    // SYSIOP-tree perf-dependent clocks. Borrowing one (e.g. `com` for an
    // `uart_alt` UART1) ties the peripheral to the `Clock` lifetime, blocking
    // `request_perf`/`set_gear` until it is dropped.
    pub fn syspll(&self) -> &Dyn {
        &self.syspll
    }
    pub fn sys(&self) -> &Dyn {
        &self.sys
    }
    pub fn sys_ahb(&self) -> &Dyn {
        &self.sys_ahb
    }
    pub fn sys_apb(&self) -> &Dyn {
        &self.sys_apb
    }
    pub fn sys_sfc(&self) -> &Dyn {
        &self.sys_sfc
    }
    pub fn com(&self) -> &Dyn {
        &self.com
    }
    pub fn pmui2c(&self) -> &Dyn {
        &self.pmui2c
    }
    pub fn gps_cpu(&self) -> &Dyn {
        &self.gps_cpu
    }
    pub fn gps_ahb(&self) -> &Dyn {
        &self.gps_ahb
    }
}
