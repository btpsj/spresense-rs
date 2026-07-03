//! Deep & cold sleep — the CXD5602's firmware-delegated low-power states.
//!
//! Unlike the runtime power knobs ([`crate::clocks`] operating points, PMU domain
//! gating) which keep the APP core alive, these two states power most of the chip
//! **off** and resume by a full cold boot on a configured wake event — microamps
//! instead of milliamps.
//!
//! ```text
//!   deep sleep  — SRAM/most domains off, GNSS/SCU stopped. Wakes on the PMIC
//!                 buttons (WKUPS/WKUPL), RTC, or USB attach.
//!   cold sleep  — deepest state. Adds GPIO (EXDEVICE 0–11), SCU sensor, PMIC, and
//!                 RTC-alarm wake sources.
//! ```
//!
//! Only **Backup SRAM** (`0x0440_0000`, 64 KiB) survives either state; normal SRAM
//! is lost, so resume is indistinguishable from a power-on reset and the firmware
//! re-runs your program from the reset vector. Read [`boot_cause`] early in `main`
//! to discover *why* the chip booted.
//!
//! # How entry works — it is all firmware
//!
//! NuttX's `up_pm_sleep()` is trivial: it calls `fw_pm_deepsleep(NULL)` /
//! `fw_pm_coldsleep(NULL)` and spins. Those are [Far API](crate::farapi) routines
//! in the SYSIOP loader firmware, which performs every power-domain shutdown itself.
//! The APP core only has to (1) configure wake sources via the `bootmask` word in
//! Backup SRAM, then (2) make the Far API call. [`deep_sleep`] / [`cold_sleep`]
//! never return — the chip powers down mid-call.
//!
//! # Configuring wake sources
//!
//! Wake events are gated by the `bootmask` word (see [`set_boot_mask`] /
//! [`clear_boot_mask`] and the [`boot`] bit constants). For a **GPIO** wake from
//! cold sleep, first arm the pin's EXDEVICE interrupt with the normal
//! [`crate::gpio`] API (which programs the PMU wake-trigger detector), then enable
//! its `bootmask` bit:
//!
//! ```ignore
//! let pin = input.into_interrupt(Trigger::FallingEdge, false)?;
//! sleep::set_boot_mask(sleep::boot::gpio_mask(pin.slot()));
//! sleep::cold_sleep();
//! ```
//!
//! For an **RTC-alarm** wake, set [`boot::DEEP_RTC`] / [`boot::COLD_RTC_ALM0`] etc.
//! and program the RTC alarm comparator separately.
//!
//! # Not yet wired
//!
//! WKUPL "long-press" emergency recovery additionally needs PMIC register `0x38` to
//! be written, which the current [`crate::pmic`] (GPO / load-switch only) does not
//! expose — so [`boot::DEEP_WKUPL`] arms the mask bit but not the PMIC side.

use crate::farapi;

// --- Backup SRAM words (cxd5602_backupmem.h `backup_info_t`) -----------------
//
// Accessed by raw volatile read/write, the same idiom [`crate::clocks::sources`]
// uses for the RCOSC cache at `0x0440_0000`.

/// `BKUP->bootcause` — written by ROM/firmware on each boot with the wake reason.
const BOOTCAUSE_ADDR: usize = 0x0440_0020;
/// `BKUP->bootmask` — read-modify-written by us to enable/disable wake sources.
const BOOTMASK_ADDR: usize = 0x0440_0024;

// --- Far API: `power_mgr` module (modid 0) ----------------------------------
//
// `api_id = stub_slot * 4 + 4`, the same rule that gives `fw_pm_pmiccontrol`
// (slot 26) its 108 in [`crate::pmic`]. `fw_pm_coldsleep` is slot 19,
// `fw_pm_deepsleep` slot 20 in `cxd56_farapistub.S`.
const POWER_MGR_MODID: i32 = 0;
const FW_PM_COLDSLEEP_API_ID: i32 = 80;
const FW_PM_DEEPSLEEP_API_ID: i32 = 84;

/// Boot-cause / boot-mask bits (`arch/arm/include/cxd56xx/pm.h` `PM_BOOT_*`).
///
/// The same bits name a *wake reason* in [`boot_cause`] and a *wake source to
/// enable* in [`set_boot_mask`]. `COLD_*` bits are only honoured from cold sleep;
/// `DEEP_*` bits from both deep and cold sleep.
pub mod boot {
    /// Normal power-on reset (value 0 — the default boot cause).
    pub const POR_NORMAL: u32 = 0x0000_0000;
    /// Power-on after dead battery.
    pub const POR_DEADBATT: u32 = 0x0000_0001;
    /// Watchdog reboot.
    pub const WDT_REBOOT: u32 = 0x0000_0002;
    /// Watchdog reset.
    pub const WDT_RESET: u32 = 0x0000_0004;
    /// PMIC long-press button (deep/cold). See the module note: PMIC side not wired.
    pub const DEEP_WKUPL: u32 = 0x0000_0008;
    /// PMIC short-press button (deep/cold).
    pub const DEEP_WKUPS: u32 = 0x0000_0010;
    /// RTC wake (deep/cold).
    pub const DEEP_RTC: u32 = 0x0000_0020;
    /// USB attach (deep/cold).
    pub const DEEP_USB_ATTACH: u32 = 0x0000_0040;
    /// Other firmware-defined deep-sleep cause.
    pub const DEEP_OTHERS: u32 = 0x0000_0080;
    /// SCU sensor interrupt (cold only).
    pub const COLD_SCU_INT: u32 = 0x0000_0100;
    /// RTC alarm 0 (cold only).
    pub const COLD_RTC_ALM0: u32 = 0x0000_0200;
    /// RTC alarm 1 (cold only).
    pub const COLD_RTC_ALM1: u32 = 0x0000_0400;
    /// RTC alarm 2 (cold only).
    pub const COLD_RTC_ALM2: u32 = 0x0000_0800;
    /// RTC alarm error (cold only).
    pub const COLD_RTC_ALMERR: u32 = 0x0000_1000;
    /// Any GPIO / EXDEVICE pin (cold only). Bits 16–27 = EXDEVICE 0–11; build a
    /// single-pin mask with [`gpio_mask`].
    pub const COLD_GPIO: u32 = 0x0fff_0000;
    /// SCU sensor interrupt, second source (cold only).
    pub const COLD_SEN_INT: u32 = 0x1000_0000;
    /// PMIC interrupt (cold only).
    pub const COLD_PMIC_INT: u32 = 0x2000_0000;
    /// USB detach (cold only).
    pub const COLD_USB_DETACH: u32 = 0x4000_0000;
    /// USB attach (cold only).
    pub const COLD_USB_ATTACH: u32 = 0x8000_0000;

    /// Boot-mask bit for a single GPIO wake, by EXDEVICE slot (0–11). Mirrors
    /// NuttX `PM_BOOT_GPIO_MASK`. Get the slot from
    /// [`InterruptInput::slot`](crate::gpio::InterruptInput::slot).
    pub const fn gpio_mask(exdevice_slot: u8) -> u32 {
        1u32 << (exdevice_slot as u32 + 16)
    }
}

/// Bits that can never be cleared from the boot mask (NuttX `NON_MASKABLE_BOOTMASK`).
const NON_MASKABLE: u32 = boot::POR_NORMAL
    | boot::POR_DEADBATT
    | boot::WDT_REBOOT
    | boot::WDT_RESET
    | boot::DEEP_USB_ATTACH
    | boot::DEEP_OTHERS;

/// Deep sleep must keep at least one of these enabled (NuttX `DEEP_PROHIBIT_BOOTMASK`),
/// otherwise the chip could never wake from deep sleep.
const DEEP_PROHIBIT: u32 = boot::DEEP_WKUPS | boot::DEEP_RTC;

/// Why the chip last booted — the `BKUP->bootcause` word. Compare against the
/// [`boot`] bits (e.g. `boot_cause() & boot::COLD_GPIO != 0`). A value of `0`
/// ([`boot::POR_NORMAL`]) is a cold power-on, not a sleep wake.
#[inline]
pub fn boot_cause() -> u32 {
    // SAFETY: read-only access to the always-powered Backup SRAM region.
    unsafe { core::ptr::read_volatile(BOOTCAUSE_ADDR as *const u32) }
}

/// The current `BKUP->bootmask` — the set of enabled wake sources.
#[inline]
pub fn boot_mask() -> u32 {
    // SAFETY: read-only access to the always-powered Backup SRAM region.
    unsafe { core::ptr::read_volatile(BOOTMASK_ADDR as *const u32) }
}

#[inline]
fn write_boot_mask(value: u32) {
    // SAFETY: word-aligned write to the always-powered Backup SRAM region.
    unsafe { core::ptr::write_volatile(BOOTMASK_ADDR as *mut u32, value) }
}

/// Enable the given wake sources (OR `mask` into the boot mask). Returns the
/// updated mask. Mirrors NuttX `up_pm_set_bootmask`.
pub fn set_boot_mask(mask: u32) -> u32 {
    critical_section::with(|_| {
        let new = boot_mask() | mask;
        write_boot_mask(new);
        new
    })
}

/// Disable the given wake sources (clear `mask` from the boot mask), enforcing the
/// same guard rails as NuttX `up_pm_clr_bootmask`:
///
/// - [`NON_MASKABLE`] bits are silently kept.
/// - Deep sleep must retain at least one of [`boot::DEEP_WKUPS`] / [`boot::DEEP_RTC`];
///   a request that would clear the last of the two is ignored for those bits.
///
/// Returns the updated mask.
pub fn clear_boot_mask(mask: u32) -> u32 {
    critical_section::with(|_| {
        // Never clear non-maskable causes.
        let mut mask = mask & !NON_MASKABLE;

        // Refuse to disable both deep-sleep wake sources at once.
        let current = boot_mask();
        if (current & !mask) & DEEP_PROHIBIT == 0 {
            mask &= !DEEP_PROHIBIT;
        }

        let new = current & !mask;
        write_boot_mask(new);
        new
    })
}

/// Enter **deep sleep**. Configure wake sources with [`set_boot_mask`] first; at
/// least one of [`boot::DEEP_WKUPS`] / [`boot::DEEP_RTC`] must be enabled. The chip
/// powers down inside the firmware call, so this never returns — it resumes by a
/// cold boot, after which [`boot_cause`] reports a `DEEP_*` reason.
pub fn deep_sleep() -> ! {
    enter(FW_PM_DEEPSLEEP_API_ID)
}

/// Enter **cold sleep**, the deepest state (adds GPIO/SCU/PMIC/RTC-alarm wake
/// sources). Configure wake sources with [`set_boot_mask`] first. Never returns;
/// resumes by a cold boot, after which [`boot_cause`] reports a `COLD_*`/`DEEP_*`
/// reason.
pub fn cold_sleep() -> ! {
    enter(FW_PM_COLDSLEEP_API_ID)
}

/// Issue the sleep Far API call. The firmware powers the chip off mid-call, so the
/// call does not complete; the trailing `dsb` + halt loop exactly mirrors NuttX's
/// `__asm("dsb"); for(;;);` and is what makes this `-> !` regardless of whether the
/// (never-arriving) completion event times out.
fn enter(api_id: i32) -> ! {
    // NuttX passes NULL; our `call` wants a buffer. A zeroed 4-word arg matches the
    // r0–r3 the asm stub pushes; the firmware ignores it for sleep.
    let mut arg = [0u32; 4];
    let _ = farapi::call(POWER_MGR_MODID, api_id, &mut arg, farapi::DEFAULT_POLL_BUDGET);

    cortex_m::asm::dsb();
    loop {
        cortex_m::asm::wfi();
    }
}
