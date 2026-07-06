//! On-hardware multicore tests: closure spawn, typed async mailbox, HwMutex.
//!
//! Four cases (the fourth in `--features low-power` builds only), each
//! consuming one worker-core token:
//!
//! 1. `spawn_closure_captures` — a closure with moved captures runs on Core1
//!    (`spawn` returning `Ok` proves the boot handshake; the published value
//!    proves the capture crossed cores through the staged stack frame).
//! 2. `mailbox_echo_roundtrip` — Core2 runs its own `Inbox`/`Outbox` echo
//!    server, taking the `FIFO_FROM` interrupt **on the worker core** through
//!    its banked INTC/NVIC and the shared vector table; Core0 sends typed
//!    messages and awaits the replies with a minimal `WFE` `block_on`.
//! 3. `hw_mutex_contention` — Core0 and Core3 hammer one `HwMutex<0, u32>`
//!    with a deliberately non-atomic read-modify-write; an exact final count
//!    proves the SPH lock + DMB pair provide real cross-core exclusion.
//! 4. `dispatcher_survives_freqlock` (low-power builds) — `init` spawns Core4
//!    streaming typed ticks and locks the Lp operating point
//!    (`into_lp_clock`, the FREQLOCK handshake) mid-stream, before the UART
//!    console exists (the operating point moves the COM clock under UART1);
//!    the test asserts the recorded evidence: a gap-free tick sequence, with
//!    the console coming up at the settled Lp point as proof the handshake
//!    completed — the RX dispatcher routing PM replies and user datagrams
//!    off one hardware FIFO under live SYSIOP traffic.
//!
//! Cross-core signalling rules in this file (the tests crate links
//! `critical-section-single-core`, which is only core-local): cross-core data
//! goes through `HwMutex` (SPH-backed, PRIMASK-free), and flags use plain
//! atomic `store`/`load` with Release/Acquire — never `fetch_*`/`swap`
//! (LDREX/STREX are not cross-core coherent on this chip). The HAL's own
//! per-core mailbox state is core-local by hardware banking, so the
//! single-core `critical_section` inside it stays sound.

#![no_std]
#![no_main]

use cortex_m_rt as _;
use defmt_serial as _;
use panic_probe as _;
use static_cell::{ConstStaticCell, StaticCell};

use cxd56_hal::multicore::{self, Stack};
use cxd56_hal::pac::{self, interrupt};
use cxd56_hal::uart::Uart;

static SERIAL: StaticCell<Uart<'static, pac::Uart1>> = StaticCell::new();
// The operating point this build locks — Lp for the case-4 (low-power)
// build, Hp otherwise (the other cases are operating-point agnostic).
#[cfg(not(feature = "low-power"))]
static CLOCK: StaticCell<cxd56_hal::clocks::Clock<cxd56_hal::clocks::Hp>> = StaticCell::new();
#[cfg(feature = "low-power")]
static CLOCK: StaticCell<cxd56_hal::clocks::Clock<cxd56_hal::clocks::Lp>> = StaticCell::new();

static CORE1_STACK: ConstStaticCell<Stack<8192>> = ConstStaticCell::new(Stack::new());
static CORE2_STACK: ConstStaticCell<Stack<8192>> = ConstStaticCell::new(Stack::new());
static CORE3_STACK: ConstStaticCell<Stack<8192>> = ConstStaticCell::new(Stack::new());
#[cfg(feature = "low-power")]
static CORE4_STACK: ConstStaticCell<Stack<8192>> = ConstStaticCell::new(Stack::new());

// One forwarder serves every core that arms an Inbox: the vector table is
// shared, each core takes only the line it unmasked, and the handler drains
// the *calling* core's banked FIFO.
#[interrupt]
fn FIFO_FROM() {
    multicore::on_rx_interrupt();
}

/// Minimal `WFE` block_on (SEV waker; interrupts also wake WFE) for driving
/// the async `Inbox::recv` future without an executor crate.
fn block_on<F: core::future::Future>(fut: F) -> F::Output {
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| cortex_m::asm::sev(),
        |_| cortex_m::asm::sev(),
        |_| {},
    );
    // SAFETY: the vtable functions ignore the data pointer.
    let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    let mut fut = core::pin::pin!(fut);
    loop {
        if let Poll::Ready(val) = fut.as_mut().poll(&mut cx) {
            return val;
        }
        cortex_m::asm::wfe();
    }
}

#[defmt_test::tests]
mod tests {
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use defmt::{assert_eq, unwrap};

    use cxd56_hal::clocks::{Config, RccExt};
    use cxd56_hal::gpio::pins::Parts;
    use cxd56_hal::multicore::{self, Core, Cores, HwMutex, Inbox, Message, Outbox, Worker};
    use cxd56_hal::pac;
    use cxd56_hal::uart::{Uart, Uart1Pins};

    /// Case-4 stream parameters: Core4 sends `STREAM_TICKS` messages tagged
    /// `TICK_MSGID`; anything else on Core0's inbox is another case's traffic.
    #[cfg(feature = "low-power")]
    const TICK_MSGID: u8 = 0xA5;
    #[cfg(feature = "low-power")]
    const STREAM_TICKS: u16 = 6;

    /// Worker tokens carried across the cases (each `spawn` consumes one),
    /// Core0's typed endpoints, and — in low-power builds — the case-4
    /// evidence recorded in `init`.
    struct State {
        core1: Option<Worker>,
        core2: Option<Worker>,
        core3: Option<Worker>,
        inbox: Inbox,
        outbox: Outbox,
        #[cfg(feature = "low-power")]
        tick_seqs: [u16; 4],
    }

    #[init]
    fn init() -> State {
        let pac = unwrap!(pac::Peripherals::take());
        let crg = pac.crg.constrain(Config::default());
        // Non-LP builds lock the HP point up front (the other tests' default);
        // the low-power build locks Lp *mid-stream* below, so `crg` stays
        // unlocked until then.
        #[cfg(not(feature = "low-power"))]
        let clock = crg.into_hp_clock().expect("lock Hp");

        let cores = unwrap!(Cores::take());
        // Core0's typed endpoints, taken once and shared by the cases (the
        // take also arms FIFO_FROM through the file-scope forwarder).
        #[allow(unused_mut)]
        let mut inbox = unwrap!(Inbox::take());
        let outbox = unwrap!(Outbox::take());

        // Case 4 (low-power builds) runs BEFORE the console exists: the
        // operating-point change moves the COM clock under UART1, so the
        // transition happens first and the console is then configured
        // against the settled rates (the tests/time.rs pattern). Failures in
        // this phase are silent (no logger yet) and surface as the runner's
        // timeout; the assertions run with the console live, in
        // `dispatcher_survives_freqlock`.
        #[cfg(feature = "low-power")]
        let (clock, tick_seqs) = {
            // Core4: stream typed ticks, blind to what Core0 is doing.
            multicore::spawn(cores.core4, crate::CORE4_STACK.take(), || {
                let mut outbox = Outbox::take().expect("core4 outbox");
                for seq in 1..=STREAM_TICKS {
                    let msg = Message {
                        peer: Core::Core0,
                        msgid: TICK_MSGID,
                        pdata: seq,
                        data: 0,
                    };
                    while outbox.try_send(msg).is_err() {
                        core::hint::spin_loop();
                    }
                    // ~20 ms at the boot point, stretching ~3x once the APP
                    // domain drops to Lp — only the ordering matters.
                    cortex_m::asm::delay(20 * 97_500);
                }
                // Returning parks Core4; stragglers are filtered later.
            })
            .expect("spawn core4");

            let mut recv_tick = || loop {
                let msg = crate::block_on(inbox.recv());
                if msg.msgid == TICK_MSGID {
                    break msg.pdata;
                }
            };

            let mut seqs = [0u16; 4];
            seqs[0] = recv_tick();
            seqs[1] = recv_tick();
            // The FREQLOCK handshake runs while Core4 keeps streaming: PM
            // replies and user datagrams interleave on Core0's one RX FIFO,
            // and the dispatcher must route both without loss. A failed lock
            // consumes the boot tree (no clock, so no console): the panic is
            // silent here and surfaces as the runner's timeout.
            let clock = crg.into_lp_clock().expect("lock Lp under live user traffic");
            seqs[2] = recv_tick();
            seqs[3] = recv_tick();
            (clock, seqs)
        };

        // UART1 for defmt console output — configured after any
        // operating-point change so its divisor matches the settled COM rate.
        let clock = crate::CLOCK.init(clock);
        let parts = Parts::new(pac.topreg);
        let uart1_pins = Uart1Pins {
            tx: parts.gp_spi0_cs_x,
            rx: parts.gp_spi0_sck,
        };
        let uart = Uart::new(pac.uart1, uart1_pins, Default::default(), clock)
            .expect("uart1 init failed");
        defmt_serial::defmt_serial(crate::SERIAL.init(uart));

        State {
            core1: Some(cores.core1),
            core2: Some(cores.core2),
            core3: Some(cores.core3),
            inbox,
            outbox,
            #[cfg(feature = "low-power")]
            tick_seqs,
        }
    }

    #[test]
    fn spawn_closure_captures(state: &mut State) {
        static SEEN: AtomicU32 = AtomicU32::new(0);
        const MAGIC: u32 = 0xC0DE_CAFE;

        let payload = MAGIC; // moved into the closure, not a shared constant read
        let token = unwrap!(state.core1.take());
        multicore::spawn(token, crate::CORE1_STACK.take(), move || {
            SEEN.store(payload, Ordering::Release);
            // Returning parks Core1 in `wfe` — also exercises the park path.
        })
        .expect("spawn core1");

        let mut budget: u32 = 50_000_000;
        while SEEN.load(Ordering::Acquire) != MAGIC {
            budget = defmt::unwrap!(
                budget.checked_sub(1),
                "Core1 never published the captured value"
            );
            core::hint::spin_loop();
        }
    }

    #[test]
    fn mailbox_echo_roundtrip(state: &mut State) {
        // Core2: typed echo server on its own endpoints. Its Inbox arms
        // FIFO_FROM on Core2 (banked INTC + NVIC); the shared vector table's
        // forwarder then runs on Core2 and fills Core2's ring.
        let token = unwrap!(state.core2.take());
        multicore::spawn(token, crate::CORE2_STACK.take(), || {
            let mut inbox = Inbox::take().expect("core2 inbox");
            let mut outbox = Outbox::take().expect("core2 outbox");
            loop {
                if let Some(msg) = inbox.try_recv() {
                    let reply = Message {
                        peer: msg.peer, // sender == Core0; echo straight back
                        msgid: msg.msgid,
                        pdata: msg.pdata,
                        data: msg.data.wrapping_mul(2),
                    };
                    while outbox.try_send(reply).is_err() {
                        core::hint::spin_loop();
                    }
                }
                core::hint::spin_loop();
            }
        })
        .expect("spawn core2");

        // Core0: typed send + genuinely async receive, on the endpoints taken
        // in init. Replies are filtered by msgid — under the low-power
        // variant, stragglers from Core4's case-4 stream may still arrive.
        for i in 1..=4u16 {
            let request = Message {
                peer: Core::Core2,
                msgid: 7,
                pdata: i,
                data: u32::from(i) * 1000,
            };
            while state.outbox.try_send(request).is_err() {
                core::hint::spin_loop();
            }
            let reply = loop {
                let msg = crate::block_on(state.inbox.recv());
                if msg.msgid == 7 {
                    break msg;
                }
            };
            assert_eq!(reply.peer.index(), Core::Core2.index());
            assert_eq!(reply.msgid, 7);
            assert_eq!(reply.pdata, i);
            assert_eq!(reply.data, u32::from(i) * 2000);
        }
    }

    #[test]
    fn hw_mutex_contention(state: &mut State) {
        static SHARED: HwMutex<0, u32> = HwMutex::new(0);
        static CORE3_DONE: AtomicBool = AtomicBool::new(false);
        const N: u32 = 20_000;

        // Deliberately non-atomic read-modify-write: without real cross-core
        // exclusion (SPH + DMB), concurrent increments lose updates.
        fn hammer() {
            for _ in 0..N {
                let mut guard = SHARED.lock();
                let v = *guard;
                core::hint::spin_loop(); // widen the lost-update window
                *guard = v + 1;
            }
        }

        let token = unwrap!(state.core3.take());
        multicore::spawn(token, crate::CORE3_STACK.take(), || {
            hammer();
            CORE3_DONE.store(true, Ordering::Release);
        })
        .expect("spawn core3");

        hammer();

        let mut budget: u32 = 200_000_000;
        while !CORE3_DONE.load(Ordering::Acquire) {
            budget = defmt::unwrap!(budget.checked_sub(1), "Core3 never finished");
            core::hint::spin_loop();
        }
        assert_eq!(*SHARED.lock(), 2 * N);
    }

    /// Case 4 (`--features low-power` builds): assert the evidence `init`
    /// recorded while Core4 streamed ticks across a live `into_lp_clock`
    /// FREQLOCK handshake. That this assertion runs at all proves the
    /// handshake completed (PM replies routed to the PM sink, every CLK_CHG
    /// step acked — a failure has no `Clock`, so no console, and times out
    /// the runner), and the first four tick sequence numbers must be exactly
    /// 1..=4 — at that tick rate the 8-deep inbox ring never fills, so the
    /// dispatcher preserving every user datagram is deterministic, not
    /// probabilistic.
    #[cfg(feature = "low-power")]
    #[test]
    fn dispatcher_survives_freqlock(state: &mut State) {
        assert_eq!(state.tick_seqs, [1, 2, 3, 4]);
    }
}
