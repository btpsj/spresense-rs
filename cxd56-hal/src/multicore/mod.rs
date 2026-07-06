//! Multicore support for the CXD5602 APP-domain Cortex-M4F cluster.
//!
//! Targets the **single combined-image** model: all cores run from one binary
//! with multiple entry points (embassy-rp style — no ELF loader / ASMP runtime
//! loader). The pieces:
//!
//! - [`cpu`] — each core's identity ([`Core`], [`current`]).
//! - [`spawn`] — bring up worker cores on ordinary closures
//!   ([`spawn`], [`Cores`], [`Stack`]).
//! - [`sph`] — hardware-semaphore cross-core lock primitive ([`Sph`]). The
//!   Cortex-M4 `LDREX`/`STREX` monitors do not work across cores, so the SPH
//!   block is the only sound cross-core mutual-exclusion primitive.
//! - [`hw_mutex`] — [`HwMutex`], the data-guarding cross-core mutex composed
//!   from an [`Sph`] slot + the required memory barriers.
//! - [`mailbox`] — inter-core messages over the CPU FIFO: the raw two-word
//!   [`Mailbox`] plus the typed, interrupt-driven async [`Inbox`]/[`Outbox`]
//!   channel ([`Message`], `FIFO_FROM`/`FIFO_TO` wakers via
//!   [`on_rx_interrupt`]/[`on_tx_interrupt`]).

pub mod cpu;
pub mod hw_mutex;
pub mod mailbox;
pub mod spawn;
pub mod sph;

pub use cpu::{Core, current};
pub use hw_mutex::{HwMutex, HwMutexGuard};
pub use mailbox::{Full, Inbox, Mailbox, Message, Outbox, on_rx_interrupt, on_tx_interrupt};
pub use spawn::{Cores, SpawnError, Stack, Worker, spawn};
pub use sph::{COUNT, RESERVED_CS_ID, Sph};
