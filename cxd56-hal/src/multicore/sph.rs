//! Hardware semaphores (SPH) — the cross-core lock primitive.
//!
//! The Cortex-M4 `LDREX`/`STREX` exclusive monitors are local to each core and
//! do **not** provide mutual exclusion *across* cores, so `core::sync::atomic`
//! compare-exchange cannot build a cross-core lock. The CXD5602 instead exposes
//! 16 hardware test-and-set semaphores at `0x4600_c800` (the [`pac::Sph`]
//! peripheral). Each is a 16-byte slot with a write-only `REQ` command register
//! and a read-only `STS` status register. Mirrors `cxd56_sph.c`.
//!
//! These are global hardware shared by every core, so the driver accesses them
//! through [`pac::Sph::PTR`] rather than owning a singleton handle.
//!
//! [`Sph<N>`] is a zero-size, const-generic token; the index lives in the type
//! and is validated at compile time. `Sph` is the raw lock primitive; two
//! consumers with the barrier discipline built in sit on top of it:
//! [`Sph::with`] (scoped section) and [`HwMutex`](super::hw_mutex::HwMutex)
//! (data-guarding mutex). The bare [`try_lock`](Sph::try_lock)/
//! [`lock`](Sph::lock)/[`unlock`](Sph::unlock) operations imply **no memory
//! barrier**: a consumer that guards Normal-memory data through them must add a
//! `cortex_m::asm::dmb()` after locking and before unlocking (Normal-vs-Device
//! accesses may otherwise reorder on multi-core ARMv7-M — the
//! `critical_section_impl` module is the reference pattern).
//!
//! # Same-core rules
//!
//! The hardware tracks ownership per **core** (the raw ADSP id), not per
//! execution context, and it silently ignores a redundant `LOCK` from the
//! owning core. Two rules follow:
//!
//! - The lock is **not reentrant**: [`try_lock`](Sph::try_lock) reports `false`
//!   and [`lock`](Sph::lock)/[`with`](Sph::with) panic if this core already
//!   holds the slot (a "second acquisition" would let two owners alias).
//! - A same-core **interrupt handler must not contend** for a slot its thread
//!   context holds across the interrupt: the owner field cannot distinguish the
//!   two contexts, so the thread could observe a false win while the handler
//!   holds the lock. For thread↔ISR sharing use the `critical-section-impl`
//!   feature (which masks interrupts); SPH slots are for *cross-core* exclusion.

use super::cpu;
use crate::pac;
use core::marker::PhantomData;
use core::sync::atomic::{Ordering, compiler_fence};

/// Number of hardware semaphores.
pub const COUNT: usize = 16;

/// SPH index reserved by the `critical-section` impl (mirrors rp2040 Spinlock31).
///
/// Rejected at compile time by [`Sph`] when the `critical-section-impl` feature
/// is enabled; the impl itself reaches the slot through the unchecked
/// [`raw_try_lock`]/[`raw_unlock`] helpers.
pub const RESERVED_CS_ID: usize = 15;

// REQ command field (`REQ[1:0]`).
const REQ_UNLOCK: u32 = 0;
const REQ_LOCK: u32 = 1;
#[allow(dead_code)]
const REQ_RESERVE: u32 = 2;
#[allow(dead_code)]
const REQ_INTRCLR: u32 = 3;

#[inline]
fn regs() -> &'static pac::sph::RegisterBlock {
    // SAFETY: SPH is a memory-mapped peripheral shared by all cores; we only
    // issue single-register reads/writes with no aliasing requirements.
    unsafe { &*pac::Sph::PTR }
}

/// Test-and-set semaphore `n` without spinning. Returns `true` iff THIS core now
/// holds it.
///
/// Unchecked in the index — callers must ensure `n < COUNT`. This is the
/// privileged path used by both [`Sph::try_lock`] and the `critical_section`
/// impl (which needs [`RESERVED_CS_ID`], the slot [`Sph`] rejects).
#[inline]
pub(crate) fn raw_try_lock(n: usize) -> bool {
    let sph = regs();
    sph.req(n).write(|w| unsafe { w.bits(REQ_LOCK) });
    // The owner field records the ADSP master id (= core index + 2). If the
    // semaphore was free, our LOCK request set the owner to us; if it was
    // already held, the request is ignored and the owner is unchanged.
    sph.sts(n).read().lock_owner().bits() == cpu::raw_pid()
}

/// Release semaphore `n`. Only meaningful if this core currently holds it.
/// Unchecked in the index.
#[inline]
pub(crate) fn raw_unlock(n: usize) {
    regs().req(n).write(|w| unsafe { w.bits(REQ_UNLOCK) });
}

/// The raw ADSP id of the core currently holding semaphore `n`, or `None` if it
/// is idle. Unchecked in the index.
#[inline]
pub(crate) fn raw_owner(n: usize) -> Option<u8> {
    let s = regs().sts(n).read();
    if s.state().bits() == 0 {
        None
    } else {
        Some(s.lock_owner().bits())
    }
}

/// A handle to hardware semaphore `N` (`0..16`).
///
/// `Sph<N>` is a zero-size, `Copy` token; the index lives in the type. Multiple
/// cores — and multiple call sites on one core — may hold a token to the same
/// semaphore; mutual exclusion is enforced by the hardware, not by Rust
/// ownership.
///
/// `N` is validated at **compile time**: any use of `Sph::<N>` fails to compile
/// if `N >= 16`, or if `N` equals [`RESERVED_CS_ID`] while the
/// `critical-section-impl` feature is enabled.
#[derive(Copy, Clone, Debug)]
pub struct Sph<const N: usize>(PhantomData<()>);

impl<const N: usize> Sph<N> {
    /// Compile-time index check. Every associated fn below binds it
    /// (`let () = Self::VALID;`), forcing evaluation at monomorphization so an
    /// invalid `N` is a hard compile error rather than a runtime panic.
    const VALID: () = {
        assert!(N < COUNT, "SPH index out of range (must be < 16)");
        #[cfg(feature = "critical-section-impl")]
        assert!(
            N != RESERVED_CS_ID,
            "this SPH index is reserved by critical-section-impl; pick another"
        );
    };

    /// Bind to hardware semaphore `N`.
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        let () = Self::VALID;
        Sph(PhantomData)
    }

    /// Attempt to acquire the semaphore without spinning.
    ///
    /// Issues a `LOCK` request and checks whether this core won arbitration by
    /// comparing the recorded owner against this core's raw ADSP id. Returns
    /// `true` iff this core acquired the lock **with this call**.
    ///
    /// The hardware silently ignores a redundant `LOCK` from a core that already
    /// holds the slot (the owner field would still name this core), so `try_lock`
    /// first reads the owner and returns `false` when this core already holds the
    /// semaphore: the lock is not reentrant, and reporting a re-lock as a win
    /// would let two acquisitions alias. Only this core can make itself the
    /// owner, so the pre-check cannot race with other cores.
    #[inline]
    pub fn try_lock(self) -> bool {
        let () = Self::VALID;
        if raw_owner(N) == Some(cpu::raw_pid()) {
            return false;
        }
        raw_try_lock(N)
    }

    /// Spin until the semaphore is acquired.
    ///
    /// # Panics
    ///
    /// Panics if this core already holds the semaphore: a same-core re-lock can
    /// never succeed (see [`try_lock`](Self::try_lock)), so spinning on it would
    /// hang forever — the panic makes the reentry bug loud instead. See the
    /// module-level same-core rules for the interrupt-handler corollary.
    #[inline]
    pub fn lock(self) {
        let () = Self::VALID;
        if raw_owner(N) == Some(cpu::raw_pid()) {
            panic!("reentrant SPH lock: this core already holds the semaphore");
        }
        while !raw_try_lock(N) {
            core::hint::spin_loop();
        }
    }

    /// Release the semaphore. Only meaningful if this core currently holds it.
    ///
    /// No memory barrier is implied — see the module docs; [`with`](Self::with)
    /// is the barrier-correct scoped alternative.
    #[inline]
    pub fn unlock(self) {
        let () = Self::VALID;
        raw_unlock(N);
    }

    /// Run `f` while holding the semaphore, with the memory barriers a
    /// data-guarding consumer needs issued internally.
    ///
    /// Acquires the lock (spinning), issues the `DMB` that orders the lock
    /// acquisition before `f`'s protected accesses, runs `f`, then (on scope
    /// exit, including a panic with unwinding) issues the `DMB` that publishes
    /// `f`'s stores before the unlock is observable — the same barrier
    /// discipline as `critical_section_impl` (per ARM DAI0321A the cache-free
    /// reordering exemption does not apply to multi-core parts).
    ///
    /// Unlike `critical_section::with`, interrupts stay **enabled**: this is a
    /// purely cross-core lock, and the module-level same-core rules apply.
    ///
    /// # Panics
    ///
    /// Panics on same-core reentry (see [`lock`](Self::lock)).
    #[inline]
    pub fn with<R>(self, f: impl FnOnce() -> R) -> R {
        self.lock();
        // Order the lock acquisition (Device access) before the protected
        // accesses (Normal memory); mirrors critical_section_impl::acquire.
        cortex_m::asm::dmb();
        let _unlock = UnlockOnDrop::<N>(());
        f()
    }

    /// The raw ADSP id of the core currently holding the lock, or `None` if the
    /// semaphore is idle.
    #[inline]
    pub fn owner(self) -> Option<u8> {
        let () = Self::VALID;
        raw_owner(N)
    }
}

/// Releases semaphore `N` on drop, publishing the protected stores first.
///
/// Backs [`Sph::with`] so the unlock (and its barrier) also runs when `f`
/// panics under an unwinding panic handler.
struct UnlockOnDrop<const N: usize>(());

impl<const N: usize> Drop for UnlockOnDrop<N> {
    fn drop(&mut self) {
        // Make the protected stores (Normal memory) globally visible before the
        // unlock (Device memory) is observed by another core. `compiler_fence`
        // stops compiler reordering; the `dmb` is the hardware barrier (mirrors
        // critical_section_impl::release).
        compiler_fence(Ordering::SeqCst);
        cortex_m::asm::dmb();
        raw_unlock(N);
    }
}
