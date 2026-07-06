//! On-hardware multicore tests: closure spawn, typed async mailbox, HwMutex.
//!
//! Three cases, each consuming one worker-core token:
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
static CLOCK: StaticCell<cxd56_hal::clocks::Clock<cxd56_hal::clocks::Hp>> = StaticCell::new();

static CORE1_STACK: ConstStaticCell<Stack<8192>> = ConstStaticCell::new(Stack::new());
static CORE2_STACK: ConstStaticCell<Stack<8192>> = ConstStaticCell::new(Stack::new());
static CORE3_STACK: ConstStaticCell<Stack<8192>> = ConstStaticCell::new(Stack::new());

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

    /// Worker tokens carried across the cases; each `spawn` consumes one.
    struct State {
        core1: Option<Worker>,
        core2: Option<Worker>,
        core3: Option<Worker>,
    }

    #[init]
    fn init() -> State {
        let pac = unwrap!(pac::Peripherals::take());
        let crg = pac.crg.constrain(Config::default());
        let clock = crate::CLOCK.init(crg.into_hp_clock().expect("lock Hp"));

        // UART1 for defmt console output.
        let parts = Parts::new(pac.topreg);
        let uart1_pins = Uart1Pins {
            tx: parts.gp_spi0_cs_x,
            rx: parts.gp_spi0_sck,
        };
        let uart = Uart::new(pac.uart1, uart1_pins, Default::default(), clock)
            .expect("uart1 init failed");
        defmt_serial::defmt_serial(crate::SERIAL.init(uart));

        let cores = unwrap!(Cores::take());
        State {
            core1: Some(cores.core1),
            core2: Some(cores.core2),
            core3: Some(cores.core3),
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

        // Core0: typed send + genuinely async receive.
        let mut inbox = unwrap!(Inbox::take());
        let mut outbox = unwrap!(Outbox::take());
        for i in 1..=4u16 {
            let request = Message {
                peer: Core::Core2,
                msgid: 7,
                pdata: i,
                data: u32::from(i) * 1000,
            };
            while outbox.try_send(request).is_err() {
                core::hint::spin_loop();
            }
            let reply = crate::block_on(inbox.recv());
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
}
