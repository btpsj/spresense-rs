//! PM ICC protocol — request CPU/bus operating-point changes from the SYSIOP.
//!
//! The CXD5602 SYSIOP (Cortex-M0+ running `loader.espk`) owns the PLL, root
//! mux, and bus dividers. The User Manual (§3.5.1 p.167, §3.13.3.3 p.974)
//! explicitly states they are "controlled by the API — use as RO registers."
//! The APP core requests operating-point changes over the ICC (Inter-Core
//! Comms) CPU-FIFO protocol, exactly as NuttX `cxd56_powermgr.c` does.
//!
//! Wire protocol (mirrors `cxd56_powermgr.c` / `cxd56_icc.h`):
//!
//! ```text
//! APP → SYSIOP: BOOT (once at first call, with target-id table addr)
//! APP → SYSIOP: FREQLOCK(flag)   — request HV (HP) or LV (LP) voltage mode
//! repeat (3× each way, measured on CXD5602):
//!   SYSIOP → APP: CLK_CHG_START  — about to change one step; quiesce
//!   APP → SYSIOP: CLK_CHG_START  — ack (ret=0)
//!   [SYSIOP changes PLL/mux/dividers and/or PMIC voltage via CXD5247]
//!   SYSIOP → APP: CLK_CHG_END    — that step's clocks stable
//!   APP → SYSIOP: CLK_CHG_END    — ack (ret=0)
//! SYSIOP → APP: FREQLOCK(flag)   — trailing reply: whole transition complete
//! [request_perf returns; resample_dyn re-reads the new rates]
//! ```
//!
//! # Limitations
//!
//! This implementation **polls** the CPU FIFO (blocking). The handshake runs
//! with `FIFO_FROM` masked (`multicore::mailbox::with_rx_claimed`), so an
//! armed async [`Inbox`](crate::multicore::Inbox) cannot steal a protocol
//! reply, and any user proto-14 message the loop pulls is stashed into that
//! inbox instead of dropped — an async `recv` in flight across a
//! [`request_perf`] just resumes afterwards. Messages of *other* protocols
//! received during the handshake are still dropped, and raw
//! [`Mailbox`] polling must not run concurrently on this core.
//!
//! # References
//!
//! - NuttX: `arch/arm/src/cxd56xx/cxd56_powermgr.c`
//! - NuttX: `arch/arm/src/cxd56xx/cxd56_powermgr.h` (PM_CLOCK_* / PM_DOMAIN_* constants)
//! - NuttX: `arch/arm/include/cxd56xx/pm.h` (PM_CPUFREQLOCK_FLAG_*)
//! - NuttX: `arch/arm/src/cxd56xx/cxd56_icc.h` (CXD56_PROTO_PM, message layout)

use crate::multicore::{Mailbox, mailbox};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// CPU/bus performance level.
///
/// Switching between levels adjusts the APP CPU frequency AND the core voltage
/// (via the PMIC CXD5247 on the SYSIOP I2C bus); both transitions are
/// orchestrated by the SYSIOP loader firmware.
///
/// Operating points (XOSC = 26 MHz). User Manual Table APP-807/808 lists the
/// LP APP clock as 39 MHz (SYSPLL-195 tap) *or* 31.2 MHz (SYSPLL-156 tap);
/// measured on CXD5602, the SYSIOP drives SYSPLL to 156 MHz and uses the 31.2 MHz
/// tap. The *boot* default (SYSPLL 195 / APP 97.5 MHz) is a third point, distinct
/// from both — the first `request_perf` in either direction moves off it.
/// - **`Hp`**: APP CPU ~156 MHz, VDD_CORE ≈ 1.0 V (HV), COM 39 MHz
/// - **`Lp`**: APP CPU ~31.2 MHz, VDD_CORE ≈ 0.7 V (LV), COM 31.2 MHz
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Perf {
    /// High performance: ~156 MHz @ ~1.0 V (HV).
    Hp,
    /// Low power: ~31.2 MHz @ ~0.7 V (LV).
    Lp,
}

/// Error from a PM operating-point change.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PmError {
    /// The SYSIOP returned a non-zero status in `CLK_CHG_END`, indicating a
    /// firmware-side callback failure during the transition.
    ClockChangeFailed(u32),
}

// --- Protocol constants (cxd56_icc.h, cxd56_powermgr.h, pm.h) ---------------

// CXD56_PROTO_PM
const PROTO_PM: u32 = 10;

// The SYSIOP's CPU-FIFO cpuid / routing nibble.
// CXD56_PM_SYS_CPU = 0 (cxd56_powermgr.h)
const CPUID_SYSIOP: u32 = 0;

// PM message IDs carried in word0[15:0] (protodata field).
// MSGID_* (cxd56_powermgr.c:70-76 / cxd56_powermgr.h:35-41)
const MSGID_BOOT: u32 = 0;
const MSGID_FREQLOCK: u32 = 1;
const MSGID_CLK_CHG_START: u32 = 2;
const MSGID_CLK_CHG_END: u32 = 3;

// FREQLOCK flag bits (arch/arm/include/cxd56xx/pm.h:70-72).
const FLAG_INITIALIZED: u32 = 0x8000; // always set; == PM_CPUFREQLOCK_FLAG_HOLD
const FLAG_HV: u32 = 0x0001; // PM_CPUFREQLOCK_FLAG_HV — request high voltage / HP
const FLAG_LV: u32 = 0x4000; // PM_CPUFREQLOCK_FLAG_LV — request low voltage / LP

// --- Boot handshake ----------------------------------------------------------

// Placeholder cxd56_pm_target_id_s (6 × u32, all zero).
// The SYSIOP reads this address on BOOT to learn the APP's domain-callback
// configuration. Zero means no per-domain callbacks are registered; the
// CLK_CHG handshake still proceeds normally.
static TARGET_ID_TABLE: [u32; 6] = [0; 6];

static BOOTED: AtomicBool = AtomicBool::new(false);

/// Last voltage-mode FREQLOCK flag sent to the SYSIOP. The SYSIOP runs the
/// CLK_CHG handshake only on a *change* of flag, so re-requesting the current
/// flag gets no handshake and would block the recv loop forever — we skip it
/// (mirroring `cxd56_pm_checkfreqlock`'s `if (g_freqlock_flag != flag)` guard).
///
/// Seeded to the SYSIOP's **boot** flag: `FLAG_INITIALIZED` alone, with neither
/// HV nor LV — the unconstrained default. Confirmed on CXD5602: boot runs SYSPLL
/// 195 / SYS 97.5 / COM 48.75 / APP 97.5 MHz, which is *neither* the FREQLOCK-HV
/// point (SYSPLL 156 / APP 156 / COM 39) nor the FREQLOCK-LV point (APP/COM 31.2).
/// So the first request in *either* direction is a real change that runs a
/// handshake — and no first request is ever mistaken for a no-op (which, with the
/// blocking recv below, would hang).
static LAST_FLAG: AtomicU32 = AtomicU32::new(FLAG_INITIALIZED);

fn boot_once() {
    if BOOTED
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        // cxd56_pm_bootup: cxd56_pmsendmsg(MSGID_BOOT, &g_target_id_table)
        send_pm(MSGID_BOOT, TARGET_ID_TABLE.as_ptr() as u32);
    }
}

// --- Wire-format helpers -----------------------------------------------------

// CPU-FIFO word layout (iccmsg_msg_s, little-endian):
//   word0[31:28] = cpuid  (4 bits) — destination on TX; sender on RX
//   word0[27:24] = proto  (4 bits) — CXD56_PROTO_PM = 10
//   word0[23:16] = msgid  (8 bits) — unused by PM (always 0)
//   word0[15:0]  = pdata (16 bits) — PM message ID (FREQLOCK/CLK_CHG/BOOT)
//   word1        = data  (32 bits) — payload (flag / ret / table ptr)

fn pack_pm(proto_data: u32, data: u32) -> [u32; 2] {
    let word0 = (CPUID_SYSIOP << 28) | (PROTO_PM << 24) | (proto_data & 0xffff);
    [word0, data]
}

// Returns Some((proto_data, data)) if the received words are a PM message.
fn unpack_pm(words: [u32; 2]) -> Option<(u32, u32)> {
    if (words[0] >> 24) & 0xf == PROTO_PM {
        Some((words[0] & 0xffff, words[1]))
    } else {
        None
    }
}

fn send_pm(proto_data: u32, data: u32) {
    Mailbox::send_blocking(pack_pm(proto_data, data));
}

// --- Public API --------------------------------------------------------------

/// Request a CPU/bus operating-point (voltage-mode) change from the SYSIOP.
///
/// Sends `FREQLOCK(flag)` over the ICC CPU-FIFO, then drives the SYSIOP handshake
/// to completion. A voltage-mode change is **multi-step** on this silicon: the
/// SYSIOP sends several `CLK_CHG_START`/`CLK_CHG_END` pairs (3 each way, measured),
/// each of which we ack, while it reconfigures the PLL/dividers + PMIC core
/// voltage one step at a time; it then sends a trailing **`FREQLOCK` reply** that
/// marks the whole transition complete. The call **blocks** until that trailing
/// `FREQLOCK`.
///
/// Acking only the first `CLK_CHG_END` and returning (an earlier version) strands
/// the chip between steps at an out-of-spec operating point — the real cause of
/// the garbled LP console, *not* the clock readback.
///
/// `FREQLOCK` selects the **voltage mode** (HV ≈ 1.0 V / LV ≈ 0.7 V); the APP CPU
/// *and* the SYSIOP bus clocks move with it — including COM, which feeds
/// UART1/SPI0/I2C2 (User Manual Tables UART-791/792: COM 48.75 MHz HV → 32.5 MHz
/// LV). Re-read rates with [`crate::clocks::Crg::freeze`] after `Ok(())`, and
/// rebuild any COM-bus peripheral so its divisor matches the new clock.
///
/// Requesting the mode already in effect is a no-op: the SYSIOP runs no handshake,
/// so the blocking recv would never return. We track the last mode (seeded to the
/// unconstrained boot flag) and skip such requests — mirroring
/// `cxd56_pm_checkfreqlock`'s `if (g_freqlock_flag != flag)` guard.
///
/// # Errors
///
/// Returns [`PmError::ClockChangeFailed`] if `CLK_CHG_END` carries a non-zero
/// status (a firmware-side callback failure).
pub(crate) fn request_perf(perf: Perf) -> Result<(), PmError> {
    boot_once();

    let flag = FLAG_INITIALIZED
        | match perf {
            Perf::Hp => FLAG_HV,
            Perf::Lp => FLAG_LV,
        };

    // Skip a no-op (unchanged voltage mode): the SYSIOP runs no handshake for it,
    // so the recv loop below would block forever.
    if LAST_FLAG.swap(flag, Ordering::Relaxed) == flag {
        return Ok(());
    }

    // Drive the handshake to completion with the RX interrupt claimed: the
    // async mailbox ISR must not steal a protocol reply mid-transition. A
    // voltage-mode change is **multi-step** on this silicon: the SYSIOP sends
    // several CLK_CHG_START/END pairs (3 each way, measured on CXD5602), each
    // of which we ack, while it reconfigures the PLL/dividers and PMIC core
    // voltage one step at a time. It then sends a trailing FREQLOCK reply that
    // marks the whole transition complete — that is the completion signal,
    // mirroring NuttX (`cxd56_pmmsghandler` posts the freqlock-wait semaphore
    // only on MSGID_FREQLOCK, after the maintask has acked every CLK_CHG
    // pair). Returning on the first CLK_CHG_END instead would strand the chip
    // between steps at an out-of-spec operating point.
    mailbox::with_rx_claimed(|| {
        // cxd56_pmsendmsg(MSGID_FREQLOCK, flag)
        send_pm(MSGID_FREQLOCK, flag);

        loop {
            let words = Mailbox::recv_blocking();
            let Some((proto_data, data)) = unpack_pm(words) else {
                // Not PM traffic: route user proto-14 messages into the local
                // Inbox instead of dropping them (other protos still drop).
                mailbox::stash_user(words);
                continue;
            };
            match proto_data {
                // cxd56_pmsendmsg(MSGID_CLK_CHG_*, 0 /*ret=OK*/) — ack each step.
                MSGID_CLK_CHG_START => send_pm(MSGID_CLK_CHG_START, 0),
                MSGID_CLK_CHG_END => send_pm(MSGID_CLK_CHG_END, 0),
                // Trailing reply: the whole transition is done.
                MSGID_FREQLOCK => {
                    return if data != 0 {
                        Err(PmError::ClockChangeFailed(data))
                    } else {
                        Ok(())
                    };
                }
                _ => {}
            }
        }
    })
}
