use core::marker::PhantomData;

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

mod sealed {
    pub trait Sealed {}
}

/// Marker: operating point locked at [`Perf::Hp`] — HV voltage mode, APP CPU
/// 156 MHz, VDD_CORE ≈ 1.0 V. Stock-default parity: NuttX with dynamic clock
/// control disabled (the factory default) runs the maximum system clock
/// permanently.
pub struct Hp;
/// Marker: operating point locked at [`Perf::Lp`] — LV voltage mode, APP CPU
/// 31.2 MHz, VDD_CORE ≈ 0.7 V, the low-power point. COM and the other [`Dyn`]
/// clocks drop with it, so peripheral divisors are computed from the slower
/// base rates.
pub struct Lp;

impl sealed::Sealed for Hp {}
impl sealed::Sealed for Lp {}

/// The locked operating point of a [`Clock`] — implemented by [`Hp`] and
/// [`Lp`] only (sealed). There is deliberately no unconstrained state: the
/// 97.5 MHz cold-boot tree cannot be locked (the FREQLOCK protocol has only
/// the HV/LV targets) nor relied on (a warm reboot inherits whatever point
/// was last active — the SYSIOP survives APP resets); see [`Clock`].
pub trait PerfState: sealed::Sealed {
    /// The operating point this marker locks.
    const PERF: Perf;
}

impl PerfState for Hp {
    const PERF: Perf = Perf::Hp;
}
impl PerfState for Lp {
    const PERF: Perf = Perf::Lp;
}

/// Owned typed clock snapshot with a **locked operating point**. Consumes the
/// [`Crg`] peripheral.
///
/// Obtained via [`Crg::into_hp_clock`] / [`Crg::into_lp_clock`] — a `Clock`
/// always exists at a known, FREQLOCK-held operating point; the marker `P`
/// ([`Hp`]/[`Lp`]) is the compile-time witness of that lock. Moving the point
/// requires **ownership** ([`into_hp`](Self::into_hp) /
/// [`into_lp`](Self::into_lp)), so while this value — or any peripheral
/// borrowing from it — is alive, the operating point cannot change and no
/// baud/gear/divisor configuration can be silently invalidated. There is no
/// unconstrained variant (see [`PerfState`]); to observe the tree without
/// locking it (e.g. dumping the cold-boot state), sample read-only via
/// [`ClockRef::from_crg`](super::shared::ClockRef::from_crg).
///
/// [`Fixed`] fields are `pub` and `Copy` (no borrow) and hold only genuinely
/// perf-invariant clocks (`xosc`/`rcosc`/`rtc`/`scu`/`hpadc`/`lpadc`). Every
/// perf-dependent clock is a [`Dyn`] field, accessible only by reference via the
/// accessor methods, tying its lifetime to `&self` so the borrow checker blocks
/// the consuming transitions (and the `&mut` gear rewrites) while a peripheral
/// depends on one.
///
/// The perf-dependent set is the SYSIOP-tree `syspll`/`sys`/`sys_ahb`/`sys_apb`/
/// `sys_sfc`/`com`/`pmui2c`/`gps_cpu`/`gps_ahb` (User Manual SYSIOP-825/826,
/// UART-791/792) plus the APP-domain `appsmp`/`usb`/`sdio`/`img_*`. They are
/// refreshed on each transition / `set_gear` (via the private `resample_dyn`);
/// because they are `Dyn`, a peripheral built from one (e.g. a UART1 sized
/// from `com`) holds the `Clock` borrow and cannot outlive the rate it was
/// configured against.
///
/// Mapping to stock NuttX strategies: a held shared borrow ≈ a HOLD freqlock
/// (freeze during an operation); `Clock<Hp>`/`Clock<Lp>` ≈ the session HV/LV
/// freqlocks stock drivers take (USB, cameras, BT); the
/// [`ClockRef`](super::shared::ClockRef) generation/reconfigure pull model ≈
/// the CLK_CHG adapt callbacks. Scope of the lock's promise: the HAL's only
/// idle is WFI (no operating-point interaction) and cold/deep sleep restart
/// the program, so a live `Clock<P>` cannot silently go stale; a future
/// hot-sleep API must consume the typed clock or re-verify with
/// [`sampled_perf`](Self::sampled_perf) on wake (NuttX retains but never
/// re-sends its freqlock after `HOT_BOOT`).
pub struct Clock<P: PerfState> {
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
    // SYSIOP tree (refreshed by the `into_*` transitions/`set_gear`; `gps_*`
    // derive from `sys`, `pmui2c` from `sys_apb`):
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
    _p: PhantomData<P>,
}

impl<P: PerfState> Clock<P> {
    /// Lock the operating point at `P` (FREQLOCK handshake with the SYSIOP)
    /// and sample all clocks. Backs [`Crg::into_hp_clock`] /
    /// [`Crg::into_lp_clock`].
    pub(crate) fn lock(crg: Crg) -> Result<Self, PmError> {
        pm::request_perf(P::PERF)?;
        let clock = Self::sample_all(crg);
        debug_assert_eq!(clock.sampled_perf(), Some(P::PERF));
        Ok(clock)
    }

    /// Consume `crg` and sample all clocks (tree state as-is).
    fn sample_all(crg: Crg) -> Self {
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
            _p: PhantomData,
        }
    }

    /// Move to the HV operating point ([`Perf::Hp`]: APP CPU 156 MHz,
    /// VDD_CORE ≈ 1.0 V), consuming this clock and returning it re-typed and
    /// re-sampled.
    ///
    /// Drives the ICC `FREQLOCK` → `CLK_CHG_START` / `CLK_CHG_END` handshake —
    /// a voltage-mode change is **multi-step** on this silicon (3 CLK_CHG
    /// pairs plus a trailing `FREQLOCK`, measured on CXD5602) — and **blocks**
    /// (polls the CPU FIFO) until the SYSIOP confirms. A same-point call is a
    /// cheap no-op. Requiring ownership is the point: any live borrow (a UART
    /// sized from `com`, a timer on `cpu_baseclk`, ...) makes the transition a
    /// compile error, so no peripheral configuration survives an
    /// operating-point change.
    ///
    /// # Errors
    ///
    /// On [`PmError`] the `Clock` is consumed: a failed CLK_CHG handshake
    /// leaves the SYSIOP PM wedged mid-transition (hardware-observed; recovery
    /// is a power cycle), so no truthful operating point exists to re-type a
    /// survivor with.
    pub fn into_hp(self) -> Result<Clock<Hp>, PmError> {
        self.transition()
    }

    /// Move to the LV operating point ([`Perf::Lp`]: APP CPU 31.2 MHz,
    /// VDD_CORE ≈ 0.7 V — the low-power point), consuming this clock and
    /// returning it re-typed and re-sampled. See [`into_hp`](Self::into_hp)
    /// for the handshake, blocking, and consumed-on-error semantics.
    pub fn into_lp(self) -> Result<Clock<Lp>, PmError> {
        self.transition()
    }

    fn transition<Q: PerfState>(self) -> Result<Clock<Q>, PmError> {
        pm::request_perf(Q::PERF)?;
        let mut clock = self.retype::<Q>();
        clock.resample_dyn();
        debug_assert_eq!(clock.sampled_perf(), Some(Q::PERF));
        Ok(clock)
    }

    /// Re-brand without touching hardware (fields move; the held FREQLOCK is
    /// unchanged until `transition` runs the handshake).
    fn retype<Q: PerfState>(self) -> Clock<Q> {
        Clock {
            crg: self.crg,
            xosc: self.xosc,
            rcosc: self.rcosc,
            rtc: self.rtc,
            scu: self.scu,
            hpadc: self.hpadc,
            lpadc: self.lpadc,
            syspll: self.syspll,
            sys: self.sys,
            sys_ahb: self.sys_ahb,
            sys_apb: self.sys_apb,
            sys_sfc: self.sys_sfc,
            com: self.com,
            pmui2c: self.pmui2c,
            gps_cpu: self.gps_cpu,
            gps_ahb: self.gps_ahb,
            appsmp: self.appsmp,
            usb: self.usb,
            sdio: self.sdio,
            img_uart: self.img_uart,
            img_spi: self.img_spi,
            img_wspi: self.img_wspi,
            img_vsync: self.img_vsync,
            _p: PhantomData,
        }
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
    /// checker rejects this call — the same protection that keeps an
    /// operating-point transition ([`into_hp`](Self::into_hp) /
    /// [`into_lp`](Self::into_lp), which need full ownership) from
    /// invalidating a driver's baud/divisor math. Reconstruct the driver
    /// afterwards so it computes its divisors from the new rate.
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

    /// Classify the **live** clock tree by its operating-point signature: a
    /// fresh register sample (not the cached fields), mapped by the SYSPLL
    /// setting and the APP base clock. Signatures measured on CXD5602:
    ///
    /// | tree                | SYSPLL  | `appsmp` | result           |
    /// |---------------------|---------|----------|------------------|
    /// | FREQLOCK HV         | 156 MHz | 156 MHz  | `Some(Perf::Hp)` |
    /// | FREQLOCK LV         | 156 MHz | 31.2 MHz | `Some(Perf::Lp)` |
    /// | cold-boot (no lock) | 195 MHz | 97.5 MHz | `None`           |
    ///
    /// Wide bands (> 100 MHz / < 50 MHz) rather than exact taps, mirroring
    /// the tolerance `clock_perf` applies on hardware; `None` means "not a
    /// lockable operating point" — the unconstrained boot tree or an unknown
    /// state.
    ///
    /// This reads the *clock* half of the operating point only (the silicon
    /// has no voltage-mode readback register; the core rail lives behind a
    /// PMIC RPC), and it proves what the tree **is**, not that a `FREQLOCK`
    /// is *held*: a warm APP reboot inherits the previous operating point
    /// with no lock in force — the SYSIOP survives the reset.
    pub fn sampled_perf(&self) -> Option<Perf> {
        let c = Clocks::sample(self.crg.cfg);
        if c.syspll.to_Hz() != 156_000_000 {
            return None;
        }
        let appsmp = c.appsmp.to_Hz();
        if appsmp > 100_000_000 {
            Some(Perf::Hp)
        } else if appsmp < 50_000_000 {
            Some(Perf::Lp)
        } else {
            None
        }
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

    // SYSIOP-tree perf-dependent clocks. Borrowing one (e.g. `com` for a
    // `uart` UART1) ties the peripheral to the `Clock` lifetime, blocking
    // the `into_hp`/`into_lp` transitions and `set_gear` until it is dropped.
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
