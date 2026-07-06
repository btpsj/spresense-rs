//! FARAPI — remote-procedure calls into CXD5602 system firmware.
//!
//! Some CXD5602 peripherals are not driven by the APP core directly; instead
//! the APP core asks another core's firmware to run a routine on its behalf.
//! NuttX calls these `fw_*` stubs "Far API". Two firmwares answer them:
//!
//! * the SYSIOP Cortex-M0+ (running `loader.espk`, already flashed on the
//!   board) — PMIC, sleep, clock services. The audio analog companion
//!   (CXD5247 / ACA) bring-up — [`crate::audio_aca`] — is reached this way:
//!   `fw_as_acacontrol`.
//! * the GPS CPU (CPU 1, running `gnssfw`, loaded on demand) — every
//!   `fw_gd_*` routine used by the GNSS driver.
//!
//! # How NuttX does it, and what we replicate
//!
//! NuttX's `fw_*` symbols are tiny asm stubs (`cxd56_farapistub.S`) that capture
//! an API index from their own PC and jump to `farapi_main`
//! (`cxd56_farapi.c:181`). `farapi_main` fills a `farmsg` on the stack, hands
//! its **address** to the target core over the CPU-FIFO mailbox using the `MBX`
//! protocol, and blocks until that core signals completion with the `FLG`
//! protocol. The firmware reads the argument buffer the message points at, runs
//! the routine, and writes the return value back into the first word of that
//! buffer.
//!
//! This module is a faithful bare-metal port of `farapi_main` over the existing
//! [`Mailbox`]. The pieces NuttX gets from the linker — the per-module `modid`
//! (index into the `.modulelist` section), the destination `cpuno` (`0` for the
//! SYSIOP modules, `1` for the GNSS ones) and `mbxid` (statically `0`
//! everywhere) — we pass in as plain constants, so no special linker section or
//! loader-patched weak symbol is needed. See [`crate::audio_aca`] for the ACA
//! module's `modid`/api id and the GNSS driver for the GNSS module's.
//!
//! # Wire format (mirrors `cxd56_icc.c` `struct iccmsg_msg_s`, little-endian)
//!
//! ```text
//! word0: [31:28] cpuid  [27:24] proto  [23:16] msgid  [15:0] protodata
//! word1: data
//! ```
//!
//! # Polling, not interrupts
//!
//! Like [`crate::clocks::pm`], this polls the CPU FIFO — with `FIFO_FROM`
//! masked (`multicore::mailbox::with_rx_claimed`) for the bounded duration, so
//! an armed async [`Inbox`](crate::multicore::Inbox) cannot steal the
//! completion event. Stray user proto-14 messages pulled while waiting are
//! stashed into that inbox rather than dropped; [`call`] drops other unrelated
//! traffic, while [`call_to`] hands it to a caller sink, so protocols with
//! unsolicited firmware→app notifications (GNSS) don't lose events that land
//! while an RPC is in flight. Do not poll the raw [`Mailbox`] concurrently on
//! this core.
//!
//! # Timeout hazard
//!
//! A [`FarapiError::Timeout`] return means *we stopped waiting*, not that the
//! firmware stopped working: it may still read `arg` and the internal message
//! block later and write a completion through them. Callers whose `arg` points
//! at stack memory must treat a timeout as poisoning the transport (the HAL's
//! GNSS driver panics in that case); timeouts are only safely recoverable when
//! every word the firmware touches lives in `'static` (or otherwise immortal)
//! memory.

use crate::multicore::cpu;
use crate::multicore::{Mailbox, mailbox};

// --- ICC protocol ids (cxd56_icc.h) -----------------------------------------

const PROTO_MBX: u32 = 1;
const PROTO_FLG: u32 = 3;
/// The user mailbox protocol ([`crate::multicore`]); strays are stashed, not dropped.
const PROTO_USER: u32 = 14;

/// The SYSIOP core is CPU 0 — the `_modulelist_*` entries for the PMIC/sleep/
/// ACA-style modules in `cxd56_farapistub.S` all have `cpuno == 0`. The GNSS
/// modules have `cpuno == 1` and are reached via [`call_to`].
pub(crate) const CPUID_SYSIOP: u32 = 0;

/// `mbxid` is `0` for every module in `cxd56_farapistub.S`.
const MBXID: i16 = 0;

/// Low nibble of `flagid` — the "magic. not zero" 7 from `farapi_main`
/// (`api->flagid = (cpuid + 1) << 8 | 7`). The firmware echoes it back in the
/// completion `FLG` message, which is how we recognise our own done event.
const FLAG_MAGIC: u32 = 7;

/// Default completion budget. A Far API round-trip is sub-millisecond when the
/// target firmware exposes the module; this bounds the wait well past that so a
/// **missing** module (no reply) fails as [`FarapiError::Timeout`] instead of
/// hanging the core forever — the whole point of the ACA availability gate.
pub const DEFAULT_POLL_BUDGET: u32 = 5_000_000;

/// Error from a Far API call.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FarapiError {
    /// The target firmware never sent a completion event within the poll
    /// budget — typically the requested module is not present in the loaded
    /// firmware. See the module-level "Timeout hazard" note before recovering
    /// from this.
    Timeout,
}

/// NuttX `struct apimsg_s` (`cxd56_farapi.c:73`). `#[repr(C)]` so its layout
/// matches what the firmware expects.
#[repr(C)]
struct ApiMsg {
    id: i32,
    arg: *mut u32,
    mbxid: i16,
    flagid: i16,
    flagbitno: i32,
}

/// NuttX `struct farmsg_s` (`cxd56_farapi.c:94`). The leading `head.next`
/// pointer is part of the firmware-visible layout; we zero it.
#[repr(C)]
struct FarMsg {
    next: *mut u32,
    cpuid: i32,
    modid: i32,
    api: ApiMsg,
}

#[inline]
fn pack_word0(dest_cpuid: u32, proto: u32, msgid: u32, pdata: u32) -> u32 {
    ((dest_cpuid & 0xf) << 28) | ((proto & 0xf) << 24) | ((msgid & 0xff) << 16) | (pdata & 0xffff)
}

/// Issue one Far API call to a SYSIOP module and block until it completes.
///
/// `modid` is the module's index in the firmware module table; `api_id` is the
/// function index the NuttX asm stub would have derived from its PC; `arg` is
/// the argument buffer the firmware reads (`arg[0]` = command on entry) and
/// writes (`arg[0]` = return value on completion). `arg` must outlive the call
/// and hold at least the words the firmware touches (4 is always safe — it
/// mirrors the `r0-r3` the asm stub pushes).
///
/// On `Ok(())`, read the firmware return value from `arg[0]`.
pub fn call(modid: i32, api_id: i32, arg: &mut [u32], budget: u32) -> Result<(), FarapiError> {
    call_to(CPUID_SYSIOP, modid, api_id, arg, budget, |_| ())
}

/// Issue one Far API call to a module on `dest_cpu` and block until it
/// completes, handing any unrelated CPU-FIFO messages received while waiting to
/// `on_other`.
///
/// `dest_cpu` is the module's `cpuno` from `cxd56_farapistub.S` (`0` = SYSIOP,
/// `1` = GPS CPU); the other parameters are as for [`call`]. `on_other` exists
/// for protocols where the target firmware also sends unsolicited
/// notifications (GNSS `PROTO_MSG` traffic): dropping one of those on the floor
/// would lose a data-ready/boot event, so the sink lets the caller latch it and
/// act after the RPC returns.
///
/// `budget` bounds each of the three phases (request push, completion wait,
/// completion ack) separately, so the worst-case spin is `3 * budget`; the
/// pushes drain inbound traffic to `on_other` while the FIFO is full rather
/// than spinning blind.
pub fn call_to(
    dest_cpu: u32,
    modid: i32,
    api_id: i32,
    arg: &mut [u32],
    budget: u32,
    mut on_other: impl FnMut([u32; 2]),
) -> Result<(), FarapiError> {
    // `cpuid` of the *caller* — `getreg32(CPU_ID)` in NuttX, which is this
    // core's ADSP id (index + 2).
    let cpuid = cpu::raw_pid() as i32;

    let mut msg = FarMsg {
        next: core::ptr::null_mut(),
        cpuid,
        modid,
        api: ApiMsg {
            id: api_id,
            arg: arg.as_mut_ptr(),
            mbxid: MBXID,
            // api->flagid = (cpuid + 1) << 8 | 7
            flagid: (((cpuid + 1) << 8) | FLAG_MAGIC as i32) as i16,
            flagbitno: 0,
        },
    };

    // The target core reads `msg` and `arg` out of our RAM; a full data memory
    // barrier makes every field visible to it before we hand over the pointer
    // (a compiler fence alone does not order the writes for another master).
    cortex_m::asm::dmb();

    // Route a stray message pulled by one of the polling loops below: user
    // proto-14 traffic is stashed into this core's Inbox ring (an async recv
    // in flight across the call simply resumes afterwards); everything else
    // goes to the caller's sink (GNSS notifications).
    let mut route = |w: [u32; 2]| {
        if (w[0] >> 24) & 0xf == PROTO_USER {
            mailbox::stash_user(w);
        } else {
            on_other(w);
        }
    };

    // Run the request push + completion poll with the RX interrupt claimed, so
    // the async mailbox ISR cannot steal the FLG completion event out of the
    // hardware FIFO mid-handshake.
    mailbox::with_rx_claimed(|| {
        // Send request: cxd56_sendmsg(cpuno, PROTO_MBX, msgtype=4, pdata=1<<8|1,
        // &msg). msgid = msgtype << 4 = 0x40. Drain inbound while the push FIFO
        // is full instead of spinning blind — a blocking push can deadlock
        // against the far side pushing to us (hardware-verified on the GNSS
        // signal path). Nothing received before the request can be the
        // completion, so everything drained here is unrelated traffic.
        let req_w0 = pack_word0(dest_cpu, PROTO_MBX, 4 << 4, (1 << 8) | 1);
        let req = [req_w0, (&mut msg as *mut FarMsg) as u32];
        let mut sent = false;
        for _ in 0..budget {
            if Mailbox::try_send(req).is_ok() {
                sent = true;
                break;
            }
            if let Some(w) = Mailbox::try_recv() {
                route(w);
            }
            core::hint::spin_loop();
        }
        if !sent {
            return Err(FarapiError::Timeout);
        }

        // Wait for the FLG completion event (`pdata & 0xf == 7`), then
        // acknowledge it exactly as NuttX's `cxd56_farapidonehandler` does.
        // Only one RPC is ever in flight (blocking, single-core), so any FLG
        // with our magic is our completion regardless of which core sent it.
        for _ in 0..budget {
            let Some([w0, w1]) = Mailbox::try_recv() else {
                core::hint::spin_loop();
                continue;
            };
            let proto = (w0 >> 24) & 0xf;
            if proto != PROTO_FLG || (w0 & 0xf) != FLAG_MAGIC {
                // Unrelated FIFO traffic — stash or hand to the caller.
                route([w0, w1]);
                continue;
            }
            // Send event-flag response: cxd56_sendmsg(sender, PROTO_FLG,
            // msgtype=5, pdata = received & 0xff00, 0). msgid = 5 << 4 = 0x50.
            // Same drain-while-push discipline as the request.
            let sender = (w0 >> 28) & 0xf;
            let ack_w0 = pack_word0(sender, PROTO_FLG, 5 << 4, w0 & 0xff00);
            let ack = [ack_w0, 0];
            for _ in 0..budget {
                if Mailbox::try_send(ack).is_ok() {
                    // Order the firmware's writes to `arg` before the caller's
                    // reads.
                    cortex_m::asm::dmb();
                    return Ok(());
                }
                if let Some(w) = Mailbox::try_recv() {
                    route(w);
                }
                core::hint::spin_loop();
            }
            // The call itself completed but the completion ack could not be
            // pushed — the transport is dead; report it as the timeout it is.
            return Err(FarapiError::Timeout);
        }

        Err(FarapiError::Timeout)
    })
}
