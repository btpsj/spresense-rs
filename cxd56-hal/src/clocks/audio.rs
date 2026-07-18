//! Audio (MCLK) clock-domain bring-up for the I2S / audio subsystem.
//!
//! Mirrors `cxd56_audio_clock_enable` / `cxd56_audio_clock_disable`
//! (`nuttx/arch/arm/src/cxd56xx/cxd56_clock.c:837-871`). This configures the
//! *audio MCLK* and the audio AHB gate/reset:
//!
//! ```text
//! enable_pwd(PDID_APP_AUD)                      -> pmu::enable_domain(AppAudio)
//! APP_CKSEL.AUD_MCLK = src                      -> topreg_sub().app_cksel()
//! if src == XOSC: APP_DIV = div                 -> topreg_sub().app_div()
//! APP_CKEN.MCLK = 1                             -> topreg_sub().app_cken()
//! CRG.RESET.XRS_AUD = 1   (release reset)       -> crg().reset()
//! CRG.CK_GATE_AHB.CK_GATE_AUD = 1 (ungate)      -> crg().ck_gate_ahb()
//! ```
//!
//! # Why this is direct MMIO (no ICC/FARAPI)
//!
//! Unlike the root clock tree (SYS PLL, `appsmp`, bus dividers — owned by the
//! SYSIOP M0+ and only reachable via the ICC `FREQLOCK` RPC), the audio MCLK
//! mux/gate is an APP-local control that NuttX programs with plain `modifyreg32`.
//! On the Spresense main board the MCLK source is the **external audio crystal**
//! ([`AudMclk::Ext`]), a fixed clock that never changes with the operating
//! point ([`crate::clocks::Clock`]) — which is why [`crate::i2s::I2s`] needs
//! no lifetime.

use super::peripheral::ClockError;
use super::pmu::{self, PmuDomain};
use crate::regs::{crg, topreg_sub};

/// Audio MCLK source — `APP_CKSEL.AUD_MCLK` (bits `[17:16]`).
///
/// Values match the User Manual / NuttX `AUD_MCLK_*` constants
/// (`hardware/cxd5602_topreg.h`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AudMclk {
    /// External audio XTAL — the Spresense main-board default
    /// (`cxd56_audio_analog.c:121` calls `cxd56_audio_clock_enable(AUD_MCLK_EXT, 0)`).
    Ext = 0,
    /// Internal XOSC, divided by `APP_DIV`.
    Xosc = 1,
    /// Internal RCOSC.
    Rcosc = 2,
}

/// Bring up the audio MCLK clock domain. Mirrors `cxd56_audio_clock_enable`.
///
/// `div` is written to `APP_DIV` only when `src == AudMclk::Xosc` (matching the
/// NuttX guard). Refcounted via the PMU domain, so repeated calls are safe.
pub fn audio_clock_enable(src: AudMclk, div: u8) -> Result<(), ClockError> {
    pmu::enable_domain(PmuDomain::AppAudio)?;
    topreg_sub()
        .app_cksel()
        .modify(|_, w| unsafe { w.aud_mclk().bits(src as u8) });
    if src == AudMclk::Xosc {
        topreg_sub()
            .app_div()
            .write(|w| unsafe { w.bits(div as u32) });
    }
    topreg_sub()
        .app_cken()
        .modify(|_, w| w.app_mclk().set_bit());
    crg().reset().modify(|_, w| w.xrs_aud().set_bit());
    crg().ck_gate_ahb().modify(|_, w| w.ck_gate_aud().set_bit());
    Ok(())
}

/// Tear down the audio MCLK clock domain. Mirrors `cxd56_audio_clock_disable`
/// (`cxd56_clock.c:867-871`) — reset, gate, MCLK off, then drop the PMU domain.
pub fn audio_clock_disable() -> Result<(), ClockError> {
    crg().reset().modify(|_, w| w.xrs_aud().clear_bit());
    crg()
        .ck_gate_ahb()
        .modify(|_, w| w.ck_gate_aud().clear_bit());
    topreg_sub()
        .app_cken()
        .modify(|_, w| w.app_mclk().clear_bit());
    pmu::disable_domain(PmuDomain::AppAudio)?;
    Ok(())
}
