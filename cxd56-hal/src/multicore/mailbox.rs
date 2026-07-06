//! Inter-core mailbox over the CPU FIFO — raw two-word exchange plus a typed,
//! interrupt-driven async channel.
//!
//! # Hardware
//!
//! The CXD5602 CPU FIFO ([`pac::CpuFifo`] at `0x4600_c400`) is a **per-core
//! banked** hardware mailbox carrying two-word (`[u32; 2]`) messages: every
//! core sees its own TX/RX pair at the same addresses, and the hardware routes
//! a pushed message to the RX FIFO of the core named in word 0 bits `[31:28]`
//! (replaced with the *sender's* id on delivery). Mirrors `cxd56_cpufifo.c`
//! (`cxd56_cfpush` / `cxd56_cfpull`).
//!
//! Two interrupt lines per core, both level conditions with **no
//! peripheral-level enable** (arming is purely the Sony INTC gate + NVIC
//! mask): `FIFO_FROM` = this core's RX FIFO is non-empty, `FIFO_TO` = this
//! core's TX FIFO is not full.
//!
//! # Protocol layout (ICC convention, `cxd56_icc.c`)
//!
//! Word 0 is `[31:28] cpuid | [27:24] proto | [23:16] msgid | [15:0] pdata`,
//! word 1 is a free 32-bit payload. The proto nibble multiplexes independent
//! protocols over the one FIFO: the SYSIOP power manager uses 10, FARAPI's
//! event flags use 3, and this module's typed [`Message`] channel claims
//! **14** — the one id the vendor table leaves undefined. (A future GNSS
//! driver adds protos 0/13 as further arms of the same dispatch.)
//!
//! # Two layers
//!
//! - [`Mailbox`] — the raw, polling `[u32; 2]` exchange. It is what the
//!   PM/FARAPI drivers speak (their protocols are inherently synchronous
//!   request/response), and it stays available for protocol experiments.
//!   It performs **no** proto demultiplexing: a raw receive takes whatever is
//!   at the head of this core's RX FIFO.
//! - [`Inbox`] / [`Outbox`] — per-core typed endpoints for proto-14
//!   [`Message`]s with `async` receive/send, built on the FIFO interrupts and
//!   hand-rolled wakers (no executor dependency; any waker works, including a
//!   simple `WFE`-based `block_on`). All waking is same-core by construction:
//!   a core's `Inbox` is filled by that core's own `FIFO_FROM` handler.
//!
//! # Arming (the usual three-layer opt-in)
//!
//! [`Inbox::take`] opens the INTC gate and unmasks `FIFO_FROM` in this core's
//! NVIC; [`Outbox::take`] opens only the INTC gate (`FIFO_TO` reads "not
//! full", i.e. almost always true — it is unmasked only while an async
//! [`send`](Outbox::send) actually waits). The third layer is the
//! application's, because a library rlib cannot own a vector:
//!
//! ```ignore
//! #[interrupt]
//! fn FIFO_FROM() {
//!     cxd56_hal::multicore::on_rx_interrupt();
//! }
//! #[interrupt]
//! fn FIFO_TO() {
//!     cxd56_hal::multicore::on_tx_interrupt();
//! }
//! ```
//!
//! Forward them on **every core** that uses the respective endpoint (the
//! vector table is shared; each core takes only the lines it unmasked).
//!
//! # Coexistence with the PM / FARAPI protocols on `Core0`
//!
//! `Core0`'s RX FIFO also carries the SYSIOP protocol replies. The polling
//! handshakes in `clocks::pm` and `farapi` therefore run under
//! `with_rx_claimed`: they mask `FIFO_FROM` for their (bounded) duration so
//! the RX interrupt cannot steal a protocol reply, and any proto-14 message
//! they pull meanwhile is routed into the local [`Inbox`] via `stash_user`
//! instead of being dropped — so an async `recv` in flight during a
//! `request_perf` merely resumes a little later, losing nothing. In the other
//! direction, [`on_rx_interrupt`] drops non-proto-14 traffic (an unsolicited
//! protocol message outside a handshake was dropped before this module
//! existed, too; a full per-proto dispatcher can grow out of that match arm
//! without changing this API).
//!
//! # Flow control
//!
//! [`on_rx_interrupt`] drains the hardware FIFO into a small per-core ring.
//! When the ring is full it masks `FIFO_FROM` and leaves the rest in the
//! hardware FIFO — senders then see [`Full`] and back-pressure propagates end
//! to end; popping a message re-unmasks the line (level semantics re-fire it
//! while backlog remains).

use core::cell::RefCell;
use core::future::poll_fn;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Poll, Waker};

use cortex_m::peripheral::NVIC;
use critical_section::Mutex;

use super::cpu::{self, Core};
use crate::pac;

/// Returned by [`Mailbox::try_send`] / [`Outbox::try_send`] when the transmit
/// FIFO is full.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Full;

/// Zero-sized handle to this core's raw CPU FIFO window.
#[derive(Copy, Clone, Debug)]
pub struct Mailbox;

impl Mailbox {
    #[inline]
    fn regs() -> &'static pac::cpu_fifo::RegisterBlock {
        // SAFETY: the CPU FIFO window is memory-mapped, per-core banked
        // hardware; we only issue single-register reads/writes, serialized
        // against this core's other contexts by the critical sections below.
        unsafe { &*pac::CpuFifo::PTR }
    }

    /// Pack a destination core id into word 0's routing nibble (`[31:28]`)
    /// together with a 28-bit payload (`[27:0]`).
    #[inline]
    pub const fn pack_word0(dest: Core, payload28: u32) -> u32 {
        ((dest.raw_pid() as u32) << 28) | (payload28 & 0x0fff_ffff)
    }

    /// Extract the sender's raw ADSP id from a received word 0.
    #[inline]
    pub const fn sender_raw_pid(word0: u32) -> u8 {
        ((word0 >> 28) & 0xf) as u8
    }

    /// `true` if the transmit FIFO is full (a [`Mailbox::try_send`] would fail).
    #[inline]
    pub fn is_tx_full() -> bool {
        Self::regs().fif_push_full().read().bits() != 0
    }

    /// `true` if a message is waiting in the receive FIFO.
    #[inline]
    pub fn is_rx_ready() -> bool {
        Self::regs().fif_pull_emp().read().bits() == 0
    }

    /// Try to enqueue a two-word message without blocking.
    ///
    /// The destination core must be encoded in `words[0]` bits `[31:28]` — see
    /// [`Mailbox::pack_word0`]. Returns [`Full`] if the transmit FIFO is full.
    ///
    /// The check + WRD0/WRD1/CMP sequence runs under `critical_section` so an
    /// interrupt handler pushing on the same core cannot interleave and
    /// corrupt both messages (NuttX wraps the same window in
    /// `enter_critical_section`). A single-core `critical_section` impl
    /// suffices: the FIFO window is banked per core, so only this core's own
    /// contexts can touch it.
    #[inline]
    pub fn try_send(words: [u32; 2]) -> Result<(), Full> {
        critical_section::with(|_| {
            let f = Self::regs();
            if f.fif_push_full().read().bits() != 0 {
                return Err(Full);
            }
            f.fif_push_wrd0().write(|w| unsafe { w.bits(words[0]) });
            f.fif_push_wrd1().write(|w| unsafe { w.bits(words[1]) });
            f.fif_push_cmp().write(|w| w.push_cmp().complete());
            Ok(())
        })
    }

    /// Try to dequeue a two-word message without blocking.
    ///
    /// On success, `words[0]` bits `[31:28]` hold the sender's raw id — see
    /// [`Mailbox::sender_raw_pid`]. Runs under `critical_section` for the same
    /// reason as [`try_send`](Self::try_send).
    ///
    /// Note this takes whatever is at the head of this core's RX FIFO,
    /// regardless of protocol — on `Core0`, do not poll this while an async
    /// [`Inbox`] is armed unless `FIFO_FROM` is masked (see the module's
    /// coexistence notes and `with_rx_claimed` in the module source).
    #[inline]
    pub fn try_recv() -> Option<[u32; 2]> {
        critical_section::with(|_| {
            let f = Self::regs();
            if f.fif_pull_emp().read().bits() != 0 {
                return None;
            }
            let w0 = f.fif_pull_wrd0().read().bits();
            let w1 = f.fif_pull_wrd1().read().bits();
            f.fif_pull_cmp().write(|w| w.pull_cmp().complete());
            Some([w0, w1])
        })
    }

    /// Spin until the message can be enqueued.
    #[inline]
    pub fn send_blocking(words: [u32; 2]) {
        while Self::try_send(words).is_err() {
            core::hint::spin_loop();
        }
    }

    /// Spin until a message is received.
    #[inline]
    pub fn recv_blocking() -> [u32; 2] {
        loop {
            if let Some(m) = Self::try_recv() {
                return m;
            }
            core::hint::spin_loop();
        }
    }
}

// ---------------------------------------------------------------------------
// Typed user channel (ICC proto 14)
// ---------------------------------------------------------------------------

/// The ICC protocol nibble this module's [`Message`] channel rides on — the
/// only id the vendor protocol table (`cxd56_icc.h`) leaves undefined.
const PROTO_USER: u32 = 14;

/// A typed inter-core message on the user protocol (ICC proto 14).
///
/// `peer` is the **destination** core when sending and the **sender** when
/// received (the hardware rewrites the routing nibble in flight). `msgid`,
/// `pdata` and `data` are free payload — 56 bits per message, mirroring the
/// ICC field split so raw traffic remains interpretable on the wire.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// Destination on send; sender on receive.
    pub peer: Core,
    /// Free 8-bit message tag (`word0[23:16]`).
    pub msgid: u8,
    /// Free 16-bit payload (`word0[15:0]`).
    pub pdata: u16,
    /// Free 32-bit payload (word 1).
    pub data: u32,
}

impl Message {
    const fn to_words(self) -> [u32; 2] {
        [
            ((self.peer.raw_pid() as u32) << 28)
                | (PROTO_USER << 24)
                | ((self.msgid as u32) << 16)
                | self.pdata as u32,
            self.data,
        ]
    }

    /// Decode a received raw message; `None` unless it is proto 14 from an
    /// APP core (SYSIOP/GNSS raw ids 0/1 never legitimately speak proto 14).
    fn from_words(words: [u32; 2]) -> Option<Message> {
        let w0 = words[0];
        if (w0 >> 24) & 0xf != PROTO_USER {
            return None;
        }
        let peer = Core::from_index((((w0 >> 28) & 0xf) as u8).wrapping_sub(2))?;
        Some(Message {
            peer,
            msgid: ((w0 >> 16) & 0xff) as u8,
            pdata: (w0 & 0xffff) as u16,
            data: words[1],
        })
    }
}

/// Depth of each core's software RX ring. Beyond it, backlog stays in the
/// hardware FIFO and back-pressures the senders (see the module docs).
const RX_RING_DEPTH: usize = 8;

const EMPTY_MSG: Message = Message {
    peer: Core::Core0,
    msgid: 0,
    pdata: 0,
    data: 0,
};

/// Fixed-capacity FIFO of decoded [`Message`]s, one per core, filled by
/// [`on_rx_interrupt`] / [`stash_user`] and drained by [`Inbox`].
struct RxRing {
    buf: [Message; RX_RING_DEPTH],
    /// Index of the oldest element.
    head: u8,
    len: u8,
    /// `FIFO_FROM` was masked because this ring filled up; the next pop must
    /// unmask it (and only then — an unconditional unmask would fight the
    /// PM/FARAPI [`with_rx_claimed`] window).
    masked_by_full: bool,
}

impl RxRing {
    const fn new() -> Self {
        RxRing {
            buf: [EMPTY_MSG; RX_RING_DEPTH],
            head: 0,
            len: 0,
            masked_by_full: false,
        }
    }

    fn is_full(&self) -> bool {
        self.len as usize == RX_RING_DEPTH
    }

    fn push(&mut self, msg: Message) {
        debug_assert!(!self.is_full());
        let tail = (self.head as usize + self.len as usize) % RX_RING_DEPTH;
        self.buf[tail] = msg;
        self.len += 1;
    }

    fn pop(&mut self) -> Option<Message> {
        if self.len == 0 {
            return None;
        }
        let msg = self.buf[self.head as usize];
        self.head = (self.head + 1) % RX_RING_DEPTH as u8;
        self.len -= 1;
        Some(msg)
    }
}

/// A `Waker` cell shared between an endpoint future and this core's FIFO
/// interrupt handler. Hand-rolled (rather than pulling in `embassy-sync`) so
/// the base HAL stays runtime-free — the same pattern as `gpio`'s wakers.
struct WakerCell {
    waker: Mutex<RefCell<Option<Waker>>>,
}

impl WakerCell {
    const fn new() -> Self {
        Self {
            waker: Mutex::new(RefCell::new(None)),
        }
    }

    /// Register `waker` as the task to wake, unless an equivalent one already is.
    fn register(&self, waker: &Waker) {
        critical_section::with(|cs| {
            let mut slot = self.waker.borrow(cs).borrow_mut();
            match &*slot {
                Some(existing) if existing.will_wake(waker) => {}
                _ => *slot = Some(waker.clone()),
            }
        });
    }

    /// Wake the registered task, if any. Taken under the critical section but
    /// woken outside it (waking may run arbitrary executor code).
    fn wake(&self) {
        let waker = critical_section::with(|cs| self.waker.borrow(cs).borrow_mut().take());
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// Per-core RX state: the decoded-message ring plus the receiver's waker.
struct RxState {
    ring: Mutex<RefCell<RxRing>>,
    waker: WakerCell,
}

impl RxState {
    const fn new() -> Self {
        RxState {
            ring: Mutex::new(RefCell::new(RxRing::new())),
            waker: WakerCell::new(),
        }
    }
}

const CORE_COUNT: usize = Core::COUNT as usize;

/// Indexed by `Core::index()`; each core's own contexts are the only ones that
/// touch its entry (the FIFO window and its interrupts are per-core banked),
/// so a single-core `critical_section` impl is sufficient protection.
static RX: [RxState; CORE_COUNT] = [const { RxState::new() }; CORE_COUNT];
static TX_WAKERS: [WakerCell; CORE_COUNT] = [const { WakerCell::new() }; CORE_COUNT];
static INBOX_TAKEN: [AtomicBool; CORE_COUNT] = [const { AtomicBool::new(false) }; CORE_COUNT];
static OUTBOX_TAKEN: [AtomicBool; CORE_COUNT] = [const { AtomicBool::new(false) }; CORE_COUNT];

/// Pop one decoded message from this core's ring, restoring `FIFO_FROM` if the
/// ring had back-pressured it shut.
fn pop_local(idx: usize) -> Option<Message> {
    critical_section::with(|cs| {
        let mut ring = RX[idx].ring.borrow(cs).borrow_mut();
        let msg = ring.pop();
        if msg.is_some() && ring.masked_by_full {
            ring.masked_by_full = false;
            // Level IRQ: re-fires immediately if hardware backlog remains.
            // SAFETY (non-priority-based masking): unmasking is what the armed
            // Inbox contract expects; handlers tolerate spurious entry.
            unsafe { NVIC::unmask(pac::Interrupt::FIFO_FROM) };
        }
        msg
    })
}

/// The receiving endpoint of this core's typed mailbox channel (ICC proto 14).
///
/// One per core via [`take`](Inbox::take); `!Send`, so the receive futures are
/// pinned to the core whose `FIFO_FROM` handler wakes them.
pub struct Inbox {
    idx: usize,
    /// `*const ()` is `!Send + !Sync`: this endpoint (and its futures) must
    /// stay on the core whose banked FIFO and NVIC it arms.
    _core_local: PhantomData<*const ()>,
}

impl Inbox {
    /// Take the calling core's receive endpoint and arm its RX interrupt path
    /// (INTC gate + NVIC unmask). Returns `None` after the first call on this
    /// core.
    ///
    /// The application must forward the vector — `#[interrupt] fn FIFO_FROM()
    /// { multicore::on_rx_interrupt() }` — on this core's side of the shared
    /// vector table (see the module docs); without it, an incoming message
    /// lands in `DefaultHandler`.
    pub fn take() -> Option<Inbox> {
        let idx = cpu::current().index() as usize;
        critical_section::with(|_| {
            if INBOX_TAKEN[idx].load(Ordering::Relaxed) {
                return None;
            }
            INBOX_TAKEN[idx].store(true, Ordering::Relaxed);
            Some(())
        })?;
        crate::interrupt::enable(pac::Interrupt::FIFO_FROM);
        // SAFETY (non-priority-based masking): opting the armed path in.
        unsafe { NVIC::unmask(pac::Interrupt::FIFO_FROM) };
        Some(Inbox {
            idx,
            _core_local: PhantomData,
        })
    }

    /// Dequeue a buffered message without waiting.
    pub fn try_recv(&mut self) -> Option<Message> {
        pop_local(self.idx)
    }

    /// Wait for the next message addressed to this core.
    ///
    /// `&mut self` keeps receives one-at-a-time (the endpoint holds a single
    /// waker slot). The future is `!Send` like the endpoint itself.
    pub async fn recv(&mut self) -> Message {
        let idx = self.idx;
        poll_fn(move |cx| {
            if let Some(msg) = pop_local(idx) {
                return Poll::Ready(msg);
            }
            RX[idx].waker.register(cx.waker());
            // Re-check after registering: a message that landed in between
            // would otherwise be missed until the next (never-coming) wake.
            match pop_local(idx) {
                Some(msg) => Poll::Ready(msg),
                None => Poll::Pending,
            }
        })
        .await
    }
}

/// The sending endpoint of this core's typed mailbox channel (ICC proto 14).
///
/// One per core via [`take`](Outbox::take); `!Send` for the same core-affinity
/// reasons as [`Inbox`].
pub struct Outbox {
    idx: usize,
    _core_local: PhantomData<*const ()>,
}

impl Outbox {
    /// Take the calling core's send endpoint. Opens the `FIFO_TO` INTC gate;
    /// the NVIC line stays masked except while an async
    /// [`send`](Outbox::send) is actually waiting ("TX not full" is a level
    /// condition that is true almost always). Returns `None` after the first
    /// call on this core.
    ///
    /// Async sends additionally need the vector forwarded — `#[interrupt] fn
    /// FIFO_TO() { multicore::on_tx_interrupt() }`. [`try_send`](Outbox::try_send)
    /// alone works without it.
    pub fn take() -> Option<Outbox> {
        let idx = cpu::current().index() as usize;
        critical_section::with(|_| {
            if OUTBOX_TAKEN[idx].load(Ordering::Relaxed) {
                return None;
            }
            OUTBOX_TAKEN[idx].store(true, Ordering::Relaxed);
            Some(())
        })?;
        crate::interrupt::enable(pac::Interrupt::FIFO_TO);
        Some(Outbox {
            idx,
            _core_local: PhantomData,
        })
    }

    /// Enqueue `msg` for `msg.peer` without blocking; [`Full`] if this core's
    /// TX FIFO is full (destination backlog — see the module's flow-control
    /// notes).
    pub fn try_send(&mut self, msg: Message) -> Result<(), Full> {
        Mailbox::try_send(msg.to_words())
    }

    /// Send `msg` to `msg.peer`, waiting (via `FIFO_TO`) while the TX FIFO is
    /// full.
    pub async fn send(&mut self, msg: Message) {
        let idx = self.idx;
        let words = msg.to_words();
        poll_fn(move |cx| {
            if Mailbox::try_send(words).is_ok() {
                return Poll::Ready(());
            }
            TX_WAKERS[idx].register(cx.waker());
            // Arm the level line only while actually waiting; the handler
            // masks it again after waking us.
            // SAFETY (non-priority-based masking): scoped to this wait.
            unsafe { NVIC::unmask(pac::Interrupt::FIFO_TO) };
            // Re-try after arming: if the FIFO drained in between, the IRQ
            // may already have fired-and-masked with a stale (taken) waker.
            match Mailbox::try_send(words) {
                Ok(()) => {
                    NVIC::mask(pac::Interrupt::FIFO_TO);
                    Poll::Ready(())
                }
                Err(Full) => Poll::Pending,
            }
        })
        .await
    }
}

/// RX interrupt entry point: forward `#[interrupt] fn FIFO_FROM()` here on
/// every core that armed an [`Inbox`].
///
/// Drains this core's hardware RX FIFO into its ring — proto-14 messages are
/// buffered, anything else is dropped (see the module's coexistence notes) —
/// then wakes the receiver. When the ring fills, the line is masked and the
/// backlog stays in hardware for end-to-end back-pressure; the next
/// [`Inbox::try_recv`]/[`recv`](Inbox::recv) pop unmasks it.
pub fn on_rx_interrupt() {
    let idx = cpu::current().index() as usize;
    let pushed = critical_section::with(|cs| {
        let mut ring = RX[idx].ring.borrow(cs).borrow_mut();
        let mut pushed = false;
        loop {
            if ring.is_full() {
                NVIC::mask(pac::Interrupt::FIFO_FROM);
                ring.masked_by_full = true;
                break;
            }
            let Some(words) = Mailbox::try_recv() else {
                break;
            };
            if let Some(msg) = Message::from_words(words) {
                ring.push(msg);
                pushed = true;
            }
        }
        pushed
    });
    if pushed {
        RX[idx].waker.wake();
    }
}

/// TX interrupt entry point: forward `#[interrupt] fn FIFO_TO()` here on every
/// core that uses async [`Outbox::send`].
///
/// "TX not full" is a level condition, so the handler's only job is to disarm
/// the line and hand control back to the waiting send future.
pub fn on_tx_interrupt() {
    NVIC::mask(pac::Interrupt::FIFO_TO);
    TX_WAKERS[cpu::current().index() as usize].wake();
}

