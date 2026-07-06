//! Spawning APP-domain worker cores (single combined-image model).
//!
//! All cores run from one binary image with multiple entry points (embassy-rp
//! style — no ELF loader). [`spawn`] starts a worker on a caller-provided
//! [`Stack`] running an ordinary Rust **closure**:
//!
//! ```ignore
//! use cxd56_hal::multicore::{Cores, Stack, spawn};
//! use static_cell::ConstStaticCell;
//!
//! static CORE1_STACK: ConstStaticCell<Stack<8192>> = ConstStaticCell::new(Stack::new());
//!
//! let cores = Cores::take().unwrap();
//! let data = 42u32; // anything Send + 'static can move across
//! spawn(cores.core1, CORE1_STACK.take(), move || {
//!     let _ = data;
//!     loop { /* worker main loop */ }
//! })
//! .unwrap();
//! ```
//!
//! # Boot protocol (mirrors `up_cpu_start`, `cxd56_cpustart.c`)
//!
//! The main core brings a worker up by writing its initial stack pointer and
//! reset vector into the shared boot mailbox at `0x0d00_0000`/`0x0d00_0004`
//! (words 0/1 of the combined image's vector table — read by hardware only at
//! reset, so clobbering them is the NuttX-sanctioned protocol), replicating the
//! booting core's address-converter view, then releasing the worker's reset and
//! clock gate. The worker enters a per-`F` naked shim that enables its FPU
//! before any compiler-generated code can touch FPU registers, points its VTOR
//! at the shared vector table (so faults — and any `#[interrupt]` handlers a
//! worker unmasks through its per-core INTC/NVIC — dispatch correctly), moves
//! the closure out of the boot staging area, releases the spawner, runs the
//! closure, and parks in `wfe` if it returns.
//!
//! # How the closure crosses over
//!
//! [`spawn`] copies `F` into the top of the worker's [`Stack`] and crafts the
//! boot SP so that, per the AAPCS, the pointer to `F` arrives as a **stack**
//! argument: the worker-side entry takes two dummy `u64` parameters that
//! consume `r0`–`r3`, so parameters 3 and 4 are read from `[sp]`/`[sp+4]` —
//! exactly where spawn staged them. Entering at reset with a crafted SP is
//! indistinguishable from a jump-with-arguments (the rp2040-hal / embassy-rp
//! `core1_startup` mechanism, adapted to this chip's reset-vector boot). The
//! monomorphized shim address goes straight into the boot vector: no function
//! pointers are stored or dispatched at runtime.
//!
//! # Serialization and ownership
//!
//! The boot mailbox is a single shared location, so only one worker may be in
//! flight at a time; a freshly-started worker releases the spawner (mirroring
//! the `spin_unlock(&g_appdsp_boot)` handshake in `appdsp_boot`) before its
//! closure runs. Each worker core is represented by a [`Worker`] token consumed
//! by [`spawn`] — double-starting a core is unrepresentable. The tokens are
//! deliberately `!Send`: every spawn happens on the thread that called
//! [`Cores::take`], which closes the two-cores-spawning-concurrently race on
//! the boot mailbox by construction (a worker-spawns-worker topology is
//! intentionally unsupported).

use super::cpu::Core;
use crate::regs::crg;
use core::marker::PhantomData;
use core::mem::{align_of, size_of};
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

/// Base of the ADSP shared SRAM / boot mailbox (`CXD56_ADSP_RAM_BASE`).
///
/// Also where the combined image's vector table lives (`memory.x` places the
/// image at the RAM origin), which is why workers point their VTOR here.
const ADSP_RAM_BASE: usize = 0x0d00_0000;
/// Worker initial stack pointer (`VECTOR_ISTACK`).
const VECTOR_ISTACK: *mut u32 = ADSP_RAM_BASE as *mut u32;
/// Worker reset vector / entry point (`VECTOR_RESETV`).
const VECTOR_RESETV: *mut u32 = (ADSP_RAM_BASE + 4) as *mut u32;
/// Per-core address-converter table (APP-local view, `CXD56_ACNV_P0_DST0`).
/// 12 tiles of 4 bytes each; each core's table is `0x20` apart.
const ACNV_P0_DST0: usize = 0x0e01_2004;
const ACNV_TILES: usize = 12;
const ACNV_CPU_STRIDE: usize = 0x20;
/// This core's Vector Table Offset Register (core-private SCB).
const SCB_VTOR: *mut u32 = 0xe000_ed08 as *mut u32;
/// Minimum usable stack that must remain below the staged boot frame.
const MIN_HEADROOM: usize = 64;

/// Handshake flag: set by a freshly-booted worker, cleared by the spawner.
static BOOT_ACK: AtomicBool = AtomicBool::new(false);

/// Guards [`Cores::take`] (one-time construction of the worker tokens).
static CORES_TAKEN: AtomicBool = AtomicBool::new(false);

/// Stack memory for a worker core, `SIZE` **bytes**.
///
/// Place it in a `static` (the worker keeps using it after [`spawn`] returns);
/// `static_cell::ConstStaticCell` turns one into the `&'static mut` that
/// [`spawn`] takes without any `unsafe`:
///
/// ```ignore
/// # use cxd56_hal::multicore::Stack;
/// # use static_cell::ConstStaticCell;
/// static CORE1_STACK: ConstStaticCell<Stack<8192>> = ConstStaticCell::new(Stack::new());
/// let stack: &'static mut Stack<8192> = CORE1_STACK.take();
/// ```
///
/// Sizing note: [`spawn`] stages the closure's captures at the top of this
/// region before the worker moves them into its first frame, so budget
/// `size_of::<F>()` plus a 16-byte boot frame on top of the worker's real
/// stack need.
///
/// The 32-byte alignment matches the rp2040 convention and leaves room for a
/// future MPU stack-guard region (minimum granularity 32 bytes on Cortex-M4).
#[repr(C, align(32))]
pub struct Stack<const SIZE: usize> {
    mem: [u8; SIZE],
}

impl<const SIZE: usize> Stack<SIZE> {
    /// A zero-initialized stack (lands in `.bss`).
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        Stack { mem: [0; SIZE] }
    }
}

/// Ownership token for one worker core, consumed by [`spawn`].
///
/// Obtained from [`Cores::take`]; there is exactly one per core, so a core
/// cannot be double-started. `!Send` on purpose — see the module docs on
/// serialization.
#[derive(Debug)]
pub struct Worker {
    core: Core,
    /// `*const ()` is `!Send + !Sync`: pins all tokens (and therefore all
    /// spawns) to the thread that called [`Cores::take`].
    _not_send: PhantomData<*const ()>,
}

impl Worker {
    /// Which core this token starts.
    pub fn core(&self) -> Core {
        self.core
    }
}

/// The five spawnable worker cores (`Core0` is the main core and has no token).
#[derive(Debug)]
pub struct Cores {
    pub core1: Worker,
    pub core2: Worker,
    pub core3: Worker,
    pub core4: Worker,
    pub core5: Worker,
}

impl Cores {
    /// Take the worker-core tokens. Returns `None` after the first call.
    ///
    /// Cross-core races on the take flag are impossible by construction even
    /// under a single-core `critical_section` impl: no worker core exists
    /// until a winner has already taken the tokens and spawned it.
    pub fn take() -> Option<Cores> {
        critical_section::with(|_| {
            if CORES_TAKEN.load(Ordering::Relaxed) {
                return None;
            }
            CORES_TAKEN.store(true, Ordering::Relaxed);
            let worker = |core| Worker {
                core,
                _not_send: PhantomData,
            };
            Some(Cores {
                core1: worker(Core::Core1),
                core2: worker(Core::Core2),
                core3: worker(Core::Core3),
                core4: worker(Core::Core4),
                core5: worker(Core::Core5),
            })
        })
    }
}

/// Errors from [`spawn`].
///
/// Either way the [`Worker`] token and the stack are consumed: after
/// [`Timeout`](Self::Timeout) the core may still come up late and run the
/// closure (memory-safe — nothing else can ever alias the consumed stack or
/// captures — but hand the token back and a retry could double-boot the core);
/// and [`StackTooSmall`](Self::StackTooSmall) is a static sizing bug — grow
/// the [`Stack`] rather than retrying.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// The worker did not run the boot handshake within the spin budget.
    Timeout,
    /// `SIZE` cannot hold the closure + boot frame + [`MIN_HEADROOM`] bytes.
    StackTooSmall,
}

/// Start worker `worker` running `entry` on `stack`.
///
/// The closure and its captures move to the worker core, so `F: Send +
/// 'static` — exactly the bound for handing data to another thread. If the
/// closure returns, the worker parks in a `wfe` loop. See the module docs for
/// the boot protocol and serialization rules.
pub fn spawn<F, const SIZE: usize>(
    worker: Worker,
    stack: &'static mut Stack<SIZE>,
    entry: F,
) -> Result<(), SpawnError>
where
    F: FnOnce() + Send + 'static,
{
    let cpu = worker.core.index() as u32;
    let bit = 1u32 << (16 + cpu);

    // --- Stage the closure and the boot frame (no hardware touched yet, so
    // `entry` drops normally on the error path). Layout, top down:
    //   [f_dst .. f_dst + size_of::<F>())   the closure's captures
    //   [sp + 8 .. f_dst)                   alignment spare
    //   [sp] = f_dst, [sp + 4] = base       AAPCS stack arguments 3 and 4
    let base = stack.mem.as_mut_ptr() as usize;
    let top = base + SIZE;
    let f_dst = top
        .checked_sub(size_of::<F>())
        .ok_or(SpawnError::StackTooSmall)?
        & !(align_of::<F>() - 1);
    let sp = (f_dst & !7)
        .checked_sub(16)
        .ok_or(SpawnError::StackTooSmall)?;
    if f_dst < base || sp < base + MIN_HEADROOM {
        return Err(SpawnError::StackTooSmall);
    }
    // SAFETY: `f_dst` and `sp` lie inside the exclusively-borrowed `stack`
    // (checked above) and `f_dst` is aligned for `F`. `ptr::write` moves
    // `entry` in without reading or dropping the destination; the worker's
    // `startup` reads it back out exactly once.
    unsafe {
        ptr::write(f_dst as *mut F, entry);
        ptr::write(sp as *mut usize, f_dst);
        ptr::write((sp + 4) as *mut usize, base);
    }
    let entry_fn: unsafe extern "C" fn() -> ! = shim::<F>;
    let entry_word = entry_fn as *const () as usize as u32;

    // --- Boot the core. One critical section covers the whole shared-state
    // window: the boot-mailbox words + BOOT_ACK (shared with any concurrent
    // spawn attempt from an interrupt) and the CRG RESET/CK_GATE_AHB
    // read-modify-writes (registers also RMW'd by the clock-tree code). The
    // ACK spin stays outside so interrupts are only held off for the
    // microseconds of register work.
    critical_section::with(|_| {
        BOOT_ACK.store(false, Ordering::Release);

        // 1. Hold the worker in reset (active-low: clear bit 16+cpu).
        let r = crg().reset().read().bits();
        crg().reset().write(|w| unsafe { w.bits(r & !bit) });

        // 2. Write the worker's initial stack and reset vector into the boot
        //    mailbox. The Thumb bit survives the fn-pointer casts (the symbol
        //    address has bit 0 set), giving the required EPSR.T at reset.
        unsafe {
            ptr::write_volatile(VECTOR_ISTACK, sp as u32);
            ptr::write_volatile(VECTOR_RESETV, entry_word);
        }

        // 3. Clock supply, then stop (boot-prep pulse).
        let g = crg().ck_gate_ahb().read().bits();
        crg().ck_gate_ahb().write(|w| unsafe { w.bits(g | bit) });
        crg().ck_gate_ahb().write(|w| unsafe { w.bits(g & !bit) });

        // 4. Replicate the booting core's address-converter view to the worker
        //    so it sees the same flat memory map (single combined image).
        for i in 0..ACNV_TILES {
            let src = (ACNV_P0_DST0 + 4 * i) as *const u32;
            let dst = (ACNV_P0_DST0 + 4 * i + (cpu as usize) * ACNV_CPU_STRIDE) as *mut u32;
            unsafe { ptr::write_volatile(dst, ptr::read_volatile(src)) };
        }

        // The staged closure bytes, boot frame and boot words must be globally
        // observable before the worker leaves reset. A compiler fence alone
        // would not order the Normal-memory stores against the CRG Device
        // write on a multi-core ARMv7-M part (see critical_section_impl).
        cortex_m::asm::dmb();

        // 5. Release reset (set bit) and supply the clock.
        let r = crg().reset().read().bits();
        crg().reset().write(|w| unsafe { w.bits(r | bit) });
        let g = crg().ck_gate_ahb().read().bits();
        crg().ck_gate_ahb().write(|w| unsafe { w.bits(g | bit) });
    });

    // 6. Wait for the worker to report it has consumed the boot mailbox.
    let mut budget = 5_000_000u32;
    while !BOOT_ACK.load(Ordering::Acquire) {
        if budget == 0 {
            return Err(SpawnError::Timeout);
        }
        budget -= 1;
        core::hint::spin_loop();
    }

    Ok(())
}

/// Worker-side boot shim, monomorphized per closure type and entered directly
/// from reset via the boot vector.
///
/// Naked on purpose: it must enable this core's FPU (CPACR CP10/CP11) before
/// *any* compiler-generated code runs — a normal Rust prologue may `vpush`
/// callee-saved FPU registers, which faults while the FPU is off. The sequence
/// is literal-pool free (`movw`/`movt`), r0–r3 are dead (the AAPCS arguments
/// live on the crafted stack), and the tail `b` preserves SP for [`startup`]'s
/// argument loads.
#[unsafe(naked)]
unsafe extern "C" fn shim<F: FnOnce() + Send + 'static>() -> ! {
    core::arch::naked_asm!(
        "movw r0, #0xED88", // CPACR (0xE000ED88)
        "movt r0, #0xE000",
        "ldr r1, [r0]",
        "orr r1, r1, #0x00F00000", // CP10 | CP11 full access
        "str r1, [r0]",
        "dsb",
        "isb",
        "b {startup}",
        startup = sym startup::<F>,
    )
}

/// Worker-side Rust entry, reached from [`shim`] with the FPU already on.
///
/// The two `u64` parameters exist only to consume `r0`–`r3` so that `f` and
/// `_stack_base` are passed on the stack — i.e. read from the boot frame
/// [`spawn`] crafted (see the module docs).
extern "C" fn startup<F: FnOnce() + Send + 'static>(
    _r0r1: u64,
    _r2r3: u64,
    f: *mut F,
    _stack_base: *mut u8,
) -> ! {
    // Point this core's VTOR at the shared vector table so faults and any
    // interrupts this worker later unmasks dispatch correctly (mirrors
    // `up_irqinitialize` in `appdsp_boot`; VTOR resets to 0).
    // SAFETY: SCB_VTOR is this core's private register; the vector table at
    // ADSP_RAM_BASE is the running image's own.
    unsafe { SCB_VTOR.write_volatile(ADSP_RAM_BASE as u32) };
    cortex_m::asm::dsb();
    cortex_m::asm::isb();

    // Move the closure from the staging area at the top of our stack into this
    // frame, then release the boot mailbox for the next spawn. The staging
    // bytes are exclusively ours; only the ACK is shared, and it must come
    // after the read so a following spawn cannot be misordered around it.
    // SAFETY: `f` points at the `F` that `spawn` wrote for precisely this
    // core; it is read exactly once.
    let entry = unsafe { f.read() };
    BOOT_ACK.store(true, Ordering::Release);

    entry();

    // The closure returned: park this worker. Its stack and captures stay
    // borrowed forever (the token is gone), so there is nothing to release.
    loop {
        cortex_m::asm::wfe();
    }
}
