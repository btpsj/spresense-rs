//! Multicore support for the CXD5602 APP-domain Cortex-M4F cluster.
//!
//! Targets the **single combined-image** model: all cores run from one binary
//! with multiple entry points (embassy-rp style — no ELF loader / ASMP runtime
//! loader). The pieces:
//!
//! - [`cpu`] — each core's identity ([`Core`], [`current`]).
//! - [`spawn`] — bring up worker cores ([`spawn`], [`ack_boot`]).
//! - [`sph`] — hardware-semaphore cross-core lock primitive ([`Sph`]). The
//!   Cortex-M4 `LDREX`/`STREX` monitors do not work across cores, so the SPH
//!   block is the only sound cross-core mutual-exclusion primitive.
//! - [`hw_mutex`] — [`HwMutex`], the data-guarding cross-core mutex composed
//!   from an [`Sph`] slot + the required memory barriers.
//! - [`mailbox`] — two-word inter-core messages over the CPU FIFO ([`Mailbox`]).
//!
//! # Scope: polling/spinning only
//!
//! Every primitive here is blocking/polling. Interrupt-driven paths (an
//! embassy-async [`Mailbox`] receive waker on `FIFO_FROM`, or SPH release
//! wakeups) are simply **not** wired up yet — they are future work, not
//! blocked. The PAC `Interrupt` enum now carries the correct 0-based NVIC
//! numbers (cortex-m-rt places `__INTERRUPTS[n]` at vector slot `16 + n`;
//! see CLAUDE.md "Interrupt numbering"), so `#[interrupt]` handlers will land
//! in the right vector slot whenever someone implements them.

pub mod cpu;
pub mod hw_mutex;
pub mod mailbox;
pub mod spawn;
pub mod sph;

pub use cpu::{Core, current};
pub use hw_mutex::{HwMutex, HwMutexGuard};
pub use mailbox::{Full, Mailbox};
pub use spawn::{SpawnError, ack_boot, spawn};
pub use sph::{COUNT, RESERVED_CS_ID, Sph};
