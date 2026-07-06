//! [`HwMutex`] — a data-guarding cross-core mutex over an SPH hardware
//! semaphore.
//!
//! The blessed composition of [`Sph`] + `UnsafeCell`: a `static`-placeable
//! mutex whose guard hands out `&mut T` with the required memory barriers built
//! in, so downstream code shares data between APP cores without writing
//! `unsafe`. It is deliberately minimal — blocking [`lock`](HwMutex::lock) /
//! [`try_lock`](HwMutex::try_lock) with an RAII guard; anything fancier (async
//! handover via the SPH `RESERVE` interrupt, reader patterns, `RawMutex` trait
//! impls) is left to downstream crates over the raw [`Sph`] primitive.
//!
//! Compared to `critical_section::Mutex` under the `critical-section-impl`
//! feature, `HwMutex`:
//!
//! - works with **any** `critical_section` implementation in the binary (it
//!   never touches PRIMASK), so a binary on `critical-section-single-core` can
//!   still share data across cores;
//! - is per-slot: independent data on independent SPH slots does not contend,
//!   and interrupt latency is unaffected while a lock is held.
//!
//! # Same-core rules
//!
//! The SPH hardware tracks ownership per **core**, not per execution context
//! (see the [`sph`](super::sph) module docs). Concretely for `HwMutex`:
//!
//! - Locking a mutex this core already holds **panics** (a second guard would
//!   alias the first); [`try_lock`](HwMutex::try_lock) returns `None` instead.
//! - An interrupt handler must not lock a `HwMutex` its core's thread context
//!   can hold across the interrupt — the owner field cannot tell the two
//!   contexts apart. Use `critical-section-impl` for thread↔ISR sharing.
//! - Do not create two `HwMutex`es on the same slot `N`: they would be one
//!   hardware lock with two unrelated payloads (still sound — same-core
//!   nesting panics and cross-core use merely serializes spuriously — but
//!   never what you want).

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{Ordering, compiler_fence};

use super::sph::Sph;

/// A cross-core mutex guarding `T` with hardware semaphore `N` (`0..16`).
///
/// Place it in a `static` visible to every participating core (the single
/// combined image makes that any ordinary `static`):
///
/// ```no_run
/// use cxd56_hal::multicore::HwMutex;
///
/// static SHARED: HwMutex<0, u32> = HwMutex::new(0);
///
/// // On any core:
/// *SHARED.lock() += 1;
/// ```
///
/// `N` is validated at compile time exactly like [`Sph<N>`]: out of range, or
/// the slot reserved by `critical-section-impl`, fails to build.
pub struct HwMutex<const N: usize, T> {
    cell: UnsafeCell<T>,
}

// SAFETY: mutual exclusion is provided by SPH hardware semaphore `N` — the only
// sound cross-core lock on this chip (LDREX/STREX monitors are core-local) —
// with a DMB after acquire and before release ordering the protected Normal-
// memory accesses against the Device-memory lock operations. Sharing
// `&HwMutex<N, T>` between cores is therefore sound whenever `T` itself may be
// sent between them.
unsafe impl<const N: usize, T: Send> Sync for HwMutex<N, T> {}

impl<const N: usize, T> HwMutex<N, T> {
    /// Create a mutex guarding `value` with SPH slot `N`.
    pub const fn new(value: T) -> Self {
        // Compile-time validation of N (range + reserved slot) via the token.
        let _ = Sph::<N>::new();
        HwMutex {
            cell: UnsafeCell::new(value),
        }
    }

    /// Spin until the lock is acquired, then hand out the guard.
    ///
    /// Issues the acquire-side `DMB` before returning; the guard's drop issues
    /// the release-side `DMB` and unlocks.
    ///
    /// # Panics
    ///
    /// Panics if this core already holds slot `N` (same-core reentry — see the
    /// module docs).
    pub fn lock(&self) -> HwMutexGuard<'_, N, T> {
        Sph::<N>::new().lock();
        // Order the lock acquisition (Device) before the guard's data accesses
        // (Normal memory); mirrors critical_section_impl::acquire.
        cortex_m::asm::dmb();
        HwMutexGuard {
            mutex: self,
            _core_local: PhantomData,
        }
    }

    /// Attempt to acquire the lock without spinning.
    ///
    /// Returns `None` when another core holds it — or when **this** core does
    /// (reentry can never succeed; see the module docs).
    pub fn try_lock(&self) -> Option<HwMutexGuard<'_, N, T>> {
        if Sph::<N>::new().try_lock() {
            cortex_m::asm::dmb();
            Some(HwMutexGuard {
                mutex: self,
                _core_local: PhantomData,
            })
        } else {
            None
        }
    }

    /// Access the data without locking, through statically exclusive access.
    pub fn get_mut(&mut self) -> &mut T {
        self.cell.get_mut()
    }

    /// Consume the mutex, returning the guarded data.
    pub fn into_inner(self) -> T {
        self.cell.into_inner()
    }
}

/// RAII guard for [`HwMutex`]: derefs to `T`, unlocks on drop.
///
/// `!Send`: the SPH owner field names the acquiring core, so the release (and
/// its ordering guarantees) must come from the core that locked — the guard
/// cannot move to another core or into an `ISR`-shared `Mutex`.
pub struct HwMutexGuard<'a, const N: usize, T> {
    mutex: &'a HwMutex<N, T>,
    /// Pins the guard to the acquiring core (`*const ()` is `!Send`/`!Sync`).
    _core_local: PhantomData<*const ()>,
}

impl<const N: usize, T> Deref for HwMutexGuard<'_, N, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the guard exists ⟹ this core holds SPH `N` (acquired in
        // `lock`/`try_lock`, released only in `drop`, and the guard cannot
        // leave this core). Same-core double-guards are impossible (`lock`
        // panics, `try_lock` refuses), cross-core exclusion is the hardware's.
        unsafe { &*self.mutex.cell.get() }
    }
}

impl<const N: usize, T> DerefMut for HwMutexGuard<'_, N, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `Deref`, plus `&mut self` makes this the sole borrow.
        unsafe { &mut *self.mutex.cell.get() }
    }
}

impl<const N: usize, T> Drop for HwMutexGuard<'_, N, T> {
    fn drop(&mut self) {
        // Publish the guarded stores (Normal memory) before the unlock (Device
        // memory) is observable by another core; mirrors
        // critical_section_impl::release.
        compiler_fence(Ordering::SeqCst);
        cortex_m::asm::dmb();
        Sph::<N>::new().unlock();
    }
}
