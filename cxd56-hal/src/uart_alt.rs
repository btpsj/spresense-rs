use arm_pl011_uart as pl011;
use core::fmt;
use core::marker::PhantomData;
use core::ptr::NonNull;
use embedded_hal_nb::nb;
use embedded_hal_nb::serial::ErrorType;
use embedded_io;
use fugit::Hertz;
use thiserror::Error;

use crate::clocks::{Clock, ClockError, PeripheralId};
use crate::gpio::GpioPin;
use crate::pac;
use crate::regs::topreg;

pub enum WordLength {
    Five,
    Six,
    Seven,
    Eight,
}

pub enum StopBits {
    One,
    Two,
}

pub enum Parity {
    None,
    Even,
    Odd,
}

pub struct UartConfig {
    pub baud_rate: u32,
    pub word_length: WordLength,
    pub stop_bits: StopBits,
    pub parity: Parity,
    pub loopback: bool,
}

impl Default for UartConfig {
    fn default() -> Self {
        Self {
            baud_rate: 115_200,
            word_length: WordLength::Eight,
            stop_bits: StopBits::One,
            parity: Parity::None,
            loopback: false,
        }
    }
}

pub(crate) fn uart1_pinmux() {
    // TXD: 2mA drive (LOWEMI=1), float (PDN=1, PUN=1), input disabled (ENZI=0).
    topreg().io_spi0_cs_x().write(|w| {
        w.lowemi()
            .set_bit()
            .pdn()
            .set_bit()
            .pun()
            .set_bit()
            .enzi()
            .clear_bit()
    });
    // RXD: same but input enabled (ENZI=1).
    topreg().io_spi0_sck().write(|w| {
        w.lowemi()
            .set_bit()
            .pdn()
            .set_bit()
            .pun()
            .set_bit()
            .enzi()
            .set_bit()
    });
    // SPI0A[13:12] = Func1 → UART1.  FieldWriter<2> needs unsafe bits().
    topreg()
        .iocsys_iomd0()
        .modify(|_, w| unsafe { w.spi0a().bits(1) });
}

// UART2 — IMG-domain UART connected to the extension-board JP1 header (pins 2-5).
// TXD = P1n_00 (pin 67): IO_UART2_TXD (TOPREG+0x90c), IOCAPP_IOMD UART2=1.
// RXD = P1n_01 (pin 68): IO_UART2_RXD (TOPREG+0x910), input enabled.
// Reference: CXD5602 user manual §3.1 pp.66,71-74; cxd5602_pinconfig.h:356-357,577.
pub(crate) fn uart2_pinmux() {
    // TXD: 2mA drive (LOWEMI=1), float (PDN=1, PUN=1), input disabled (ENZI=0).
    topreg().io_uart2_txd().write(|w| {
        w.lowemi()
            .set_bit()
            .pdn()
            .set_bit()
            .pun()
            .set_bit()
            .enzi()
            .clear_bit()
    });
    // RXD: same but input enabled (ENZI=1).
    topreg().io_uart2_rxd().write(|w| {
        w.lowemi()
            .set_bit()
            .pdn()
            .set_bit()
            .pun()
            .set_bit()
            .enzi()
            .set_bit()
    });
    // UART2[3:2] = Func1 → UART2.  FieldWriter<2> needs unsafe bits().
    topreg()
        .iocapp_iomd()
        .modify(|_, w| unsafe { w.uart2().bits(1) });
}

/// Restore SPI0A mux to Func0 (GPIO) — undoes `uart1_pinmux()`.
fn uart1_unpinmux() {
    topreg()
        .iocsys_iomd0()
        .modify(|_, w| unsafe { w.spi0a().bits(0) });
}

/// Restore UART2 mux to Func0 (GPIO) — undoes `uart2_pinmux()`.
fn uart2_unpinmux() {
    topreg()
        .iocapp_iomd()
        .modify(|_, w| unsafe { w.uart2().bits(0) });
}

// Map our config types to the external driver's LineConfig.
fn line_config(config: &UartConfig) -> pl011::LineConfig {
    pl011::LineConfig {
        data_bits: match config.word_length {
            WordLength::Five => pl011::DataBits::Bits5,
            WordLength::Six => pl011::DataBits::Bits6,
            WordLength::Seven => pl011::DataBits::Bits7,
            WordLength::Eight => pl011::DataBits::Bits8,
        },
        parity: match config.parity {
            Parity::None => pl011::Parity::None,
            Parity::Even => pl011::Parity::Even,
            Parity::Odd => pl011::Parity::Odd,
        },
        stop_bits: match config.stop_bits {
            StopBits::One => pl011::StopBits::One,
            StopBits::Two => pl011::StopBits::Two,
        },
    }
}

mod sealed {
    use fugit::Hertz;

    use super::pac;
    use crate::clocks::{Clock, PeripheralId};

    pub trait Sealed {
        const ID: PeripheralId;
        /// Route this UART's signals to the board pins.
        fn pinmux();
        /// Restore the pin-mux back to Func0 (GPIO) — called by [`Uart::free`].
        fn unpinmux();
        /// Register base, type-erased to `uart1::RegisterBlock`.
        ///
        /// Used to derive the `PL011Registers` pointer for the external driver.
        /// Both UART1 and UART2 share the same PL011 register layout over the
        /// 0x1000-byte MMIO window; the cast of UART2's base address is sound on
        /// the same argument as the former direct PAC-cast approach.
        fn regs() -> *const pac::uart1::RegisterBlock;

        /// Sample this peripheral's base clock. Returns a `Copy` [`Hertz`] so
        /// the borrow of `clock` ends here; any lifetime tie lives in `Output`.
        fn base_hz(clock: &Clock) -> Hertz<u32>;
    }
}

/// TX/RX GPIO tokens for UART1 (SPI0_CS_X / SPI0_SCK pads, Func1 = UART1).
///
/// Constructed from [`gpio::pins::Parts`](crate::gpio::pins::Parts):
/// ```ignore
/// let parts = cxd56_hal::gpio::pins::Parts::new(pac.topreg);
/// let pins = Uart1Pins { tx: parts.gp_spi0_cs_x, rx: parts.gp_spi0_sck };
/// ```
pub struct Uart1Pins {
    pub tx: GpioPin<pac::topreg::GpSpi0CsX>,
    pub rx: GpioPin<pac::topreg::GpSpi0Sck>,
}

/// TX/RX GPIO tokens for UART2 (P1n_00 / P1n_01 pads, Func1 = UART2).
///
/// Constructed from [`gpio::pins::Parts`](crate::gpio::pins::Parts):
/// ```ignore
/// let parts = cxd56_hal::gpio::pins::Parts::new(pac.topreg);
/// let pins = Uart2Pins { tx: parts.gp_uart2_txd, rx: parts.gp_uart2_rxd };
/// ```
pub struct Uart2Pins {
    pub tx: GpioPin<pac::topreg::GpUart2Txd>,
    pub rx: GpioPin<pac::topreg::GpUart2Rxd>,
}

/// Maps a PAC UART token to its HAL wiring: clock-gate id, register base,
/// pin-mux routine, and the associated GPIO pin token types.
///
/// Sealed — implemented only for [`pac::Uart1`] and [`pac::Uart2`] within
/// this crate. Downstream code cannot add new implementors.
pub trait UartPeriph: sealed::Sealed {
    /// The type returned by [`Uart::new`] and how its lifetime relates to the
    /// [`Clock`] borrow:
    ///
    /// - `pac::Uart1`: `Output<'clk> = Uart<'clk, pac::Uart1>` — COM is a Dyn
    ///   clock that tracks the operating point; the UART borrows `Clock` for
    ///   `'clk`, blocking [`Clock::request_perf`] until dropped.
    /// - `pac::Uart2`: `Output<'clk> = Uart<'clk, pac::Uart2>` — IMG_UART is
    ///   a Dyn clock; the UART borrows `Clock` for `'clk`, blocking
    ///   [`Clock::request_perf`] until dropped.
    type Output<'clk>;

    /// The TX/RX GPIO pin tokens consumed by [`Uart::new`] and returned by
    /// [`Uart::free`]. [`Uart1Pins`] for UART1, [`Uart2Pins`] for UART2.
    type Pins;

    /// The TX-side GPIO pin token held by [`UartTx`] after [`Uart::split`].
    type TxPin;

    /// The RX-side GPIO pin token held by [`UartRx`] after [`Uart::split`].
    type RxPin;

    /// Wrap an initialised inner driver in the correctly-lifetimed driver
    /// struct. Bodies are textually identical across impls; only the return
    /// *type* (and therefore the `'clk` propagation) differs.
    #[doc(hidden)]
    fn wrap<'clk>(inner: pl011::Uart<'static>, uart: Self, pins: Self::Pins) -> Self::Output<'clk>;

    /// Decompose the pin pair into its TX and RX halves. Trivial struct
    /// destructuring; only the concrete pin types differ across impls.
    #[doc(hidden)]
    fn split_pins(pins: Self::Pins) -> (Self::TxPin, Self::RxPin);

    /// Inverse of [`split_pins`](UartPeriph::split_pins): recombine the TX and
    /// RX pin tokens into the original pin pair.
    #[doc(hidden)]
    fn join_pins(tx: Self::TxPin, rx: Self::RxPin) -> Self::Pins;
}

impl sealed::Sealed for pac::Uart1 {
    const ID: PeripheralId = PeripheralId::Uart1;

    fn pinmux() {
        uart1_pinmux()
    }

    fn unpinmux() {
        uart1_unpinmux()
    }

    fn regs() -> *const pac::uart1::RegisterBlock {
        pac::Uart1::PTR
    }

    fn base_hz(clock: &Clock) -> Hertz<u32> {
        clock.com().hz()
    }
}
impl UartPeriph for pac::Uart1 {
    // COM is Dyn — Output<'clk> = Uart<'clk, _>; the returned Uart borrows the
    // Clock for 'clk, blocking Clock::request_perf (which would change COM and
    // invalidate the baud divisor) until dropped.
    type Output<'clk> = Uart<'clk, pac::Uart1>;
    type Pins = Uart1Pins;
    type TxPin = GpioPin<pac::topreg::GpSpi0CsX>;
    type RxPin = GpioPin<pac::topreg::GpSpi0Sck>;

    fn wrap<'clk>(inner: pl011::Uart<'static>, uart: Self, pins: Self::Pins) -> Self::Output<'clk> {
        Uart {
            inner,
            uart,
            pins,
            _life: PhantomData,
        }
    }

    fn split_pins(pins: Self::Pins) -> (Self::TxPin, Self::RxPin) {
        (pins.tx, pins.rx)
    }

    fn join_pins(tx: Self::TxPin, rx: Self::RxPin) -> Self::Pins {
        Uart1Pins { tx, rx }
    }
}

impl sealed::Sealed for pac::Uart2 {
    const ID: PeripheralId = PeripheralId::ImgUart;
    fn pinmux() {
        uart2_pinmux()
    }
    fn unpinmux() {
        uart2_unpinmux()
    }
    fn regs() -> *const pac::uart1::RegisterBlock {
        pac::Uart2::PTR as *const _
    }
    fn base_hz(clock: &Clock) -> Hertz<u32> {
        clock.img_uart().hz()
    }
}
impl UartPeriph for pac::Uart2 {
    // IMG_UART is Dyn — Output<'clk> = Uart<'clk, _>; the returned Uart
    // borrows the Clock for 'clk, blocking Clock::request_perf until dropped.
    type Output<'clk> = Uart<'clk, pac::Uart2>;
    type Pins = Uart2Pins;
    type TxPin = GpioPin<pac::topreg::GpUart2Txd>;
    type RxPin = GpioPin<pac::topreg::GpUart2Rxd>;

    fn wrap<'clk>(inner: pl011::Uart<'static>, uart: Self, pins: Self::Pins) -> Self::Output<'clk> {
        Uart {
            inner,
            uart,
            pins,
            _life: PhantomData,
        }
    }

    fn split_pins(pins: Self::Pins) -> (Self::TxPin, Self::RxPin) {
        (pins.tx, pins.rx)
    }

    fn join_pins(tx: Self::TxPin, rx: Self::RxPin) -> Self::Pins {
        Uart2Pins { tx, rx }
    }
}

/// Errors from the profile-aware UART driver.
///
/// Wraps foreign error types so they are not re-exported through this
/// module's public API. Use [`core::error::Error::source`] to inspect the
/// underlying cause.
#[derive(Debug, Error)]
pub enum UartError {
    /// Clock gate enable or disable failed.
    #[error("clock gate error: {0}")]
    Clock(#[from] ClockError),
    /// PL011 driver error (baud divisor overflow, or RX overrun/parity/framing/break).
    #[error("pl011 error: {0}")]
    Pl011(#[from] pl011::Error),
}

impl embedded_hal_nb::serial::Error for UartError {
    fn kind(&self) -> embedded_hal_nb::serial::ErrorKind {
        match self {
            UartError::Pl011(e) => embedded_hal_nb::serial::Error::kind(e),
            UartError::Clock(_) => embedded_hal_nb::serial::ErrorKind::Other,
        }
    }
}

impl embedded_io::Error for UartError {
    fn kind(&self) -> embedded_io::ErrorKind {
        match self {
            UartError::Pl011(e) => embedded_io::Error::kind(e),
            UartError::Clock(_) => embedded_io::ErrorKind::Other,
        }
    }
}

/// Generic, profile-aware PL011 UART driver backed by [`arm_pl011_uart::Uart`].
///
/// `U` is the PAC UART token (`pac::Uart1` or `pac::Uart2`). Consuming the
/// token at construction ensures exclusive hardware ownership. The TX/RX
/// [`GpioPin`] tokens (`U::Pins`) are also consumed, making it a compile-time
/// error to use the same pads as GPIO while the UART is active.
///
/// The lifetime `'clk` is tied to the [`Clock`](crate::clocks::Clock) for both
/// UARTs, because both base clocks track the APP operating point:
/// - [`pac::Uart1`] — UART1 uses the dynamic COM clock (derived from the SYS
///   tree / SYSPLL), so the UART borrows the `Clock`, preventing
///   [`Clock::request_perf`] from changing COM and invalidating the baud
///   divisor until dropped.
/// - [`pac::Uart2`] — UART2 uses the dynamic IMG_UART clock, so the UART
///   borrows the `Clock`, preventing [`Clock::request_perf`] until dropped.
///
/// Use [`Uart::new`] to construct the driver — `U` is inferred from the PAC
/// token you pass. The return type and its lifetime are determined by `U` via
/// [`UartPeriph::Output`]. To reclaim the GPIO pins, call [`Uart::free`].
pub struct Uart<'clk, U: UartPeriph> {
    inner: pl011::Uart<'static>,
    uart: U,
    pins: U::Pins,
    _life: PhantomData<&'clk ()>,
}

// Exclusive peripheral ownership — the PAC token was consumed at construction,
// matching the PAC `Periph` Send impl. Both concrete `U::Pins` types
// (`Uart1Pins`, `Uart2Pins`) are Send; the sealed trait prevents other impls.
unsafe impl<U: UartPeriph> Send for Uart<'_, U> {}

impl<'clk, U: UartPeriph> Uart<'clk, U> {
    /// Enable the peripheral, route its pads to UART function, and return the
    /// correctly-lifetimed driver.
    ///
    /// Consumes both the PAC token `uart` (exclusive hardware ownership) and
    /// the `pins` GPIO tokens (`U::Pins`) to prevent using the same pads as
    /// GPIO while the UART is active. Pin-mux is applied **internally** —
    /// callers do not need to configure the pads. To reclaim the GPIO tokens
    /// and restore the pads to GPIO function, call [`Uart::free`].
    ///
    /// The return type is resolved per token via [`UartPeriph::Output`]:
    ///
    /// - `U = pac::Uart1` → [`Uart<'a, pac::Uart1>`]: COM is Dyn; the returned
    ///   `Uart` borrows `clock` for `'a`, preventing [`Clock::request_perf`]
    ///   (needs `&mut Clock`) from changing COM until dropped.
    /// - `U = pac::Uart2` → [`Uart<'a, pac::Uart2>`]: IMG_UART is Dyn; the
    ///   returned `Uart` borrows `clock` for `'a`, preventing
    ///   [`Clock::request_perf`] (needs `&mut Clock`) until dropped.
    ///
    /// # Example
    /// ```ignore
    /// let parts = cxd56_hal::gpio::pins::Parts::new(pac.topreg);
    /// let pins = Uart1Pins { tx: parts.gp_spi0_cs_x, rx: parts.gp_spi0_sck };
    /// let uart = Uart::new(pac.uart1, pins, Default::default(), &clock)?;
    /// ```
    #[allow(clippy::new_ret_no_self)] // intentional: returns U::Output<'a> (branded by the clock-lifetime GAT)
    pub fn new<'a>(
        uart: U,
        pins: U::Pins,
        config: UartConfig,
        clock: &'a Clock,
    ) -> Result<U::Output<'a>, UartError> {
        let hz = U::base_hz(clock);
        U::ID.enable()?;
        U::pinmux();
        // SAFETY: `U::regs()` returns the fixed, properly-aligned base address of
        // this UART's PL011 register block. We consumed the PAC token `U` above,
        // ensuring there is no other alias to this peripheral for the program's
        // lifetime. Both UART1 and UART2 expose the same PL011 register layout
        // over the 0x1000-byte MMIO window (`PL011Registers` is
        // `#[repr(C, align(4))]`); casting UART2's base address is sound on the
        // same argument as the former direct PAC-cast. The `'static` lifetime
        // reflects that the MMIO address is valid for the entire program.
        let ptr = NonNull::new(U::regs() as *mut pl011::PL011Registers)
            .expect("PL011 base address is null");
        let mut inner = pl011::Uart::new(unsafe { pl011::UniqueMmioPointer::new(ptr) });
        inner.enable(line_config(&config), config.baud_rate, hz.to_Hz())?;
        // Apply PL011 internal loopback (UARTCR.LBE) from config — mirrors
        // spi_alt's config-driven LBM. The external pl011 driver exposes no
        // loopback API, so write the bit directly. Must follow `enable`, which
        // writes UARTCR; `modify` preserves the enable/TXE/RXE bits just set.
        if config.loopback {
            // SAFETY: `U::regs()` is the fixed, properly-aligned PL011 base for
            // this peripheral (same argument as the `inner` pointer above); we
            // consumed the PAC token `uart`, so there is no other alias.
            unsafe { &*U::regs() }.cr().modify(|_, w| w.lbe().set_bit());
        }
        Ok(U::wrap(inner, uart, pins))
    }

    /// Transmit one byte, blocking until the TX FIFO has room.
    #[inline]
    pub fn write_byte(&mut self, byte: u8) {
        while self.inner.is_tx_fifo_full() {}
        self.inner.write_word(byte);
    }

    /// Read one byte if the RX FIFO is non-empty, otherwise return `None`.
    ///
    /// Returns `None` on both an empty FIFO and RX errors (overrun/parity/
    /// framing/break); use the [`embedded_hal_nb`] or [`embedded_io`] trait
    /// impls to observe error detail.
    #[inline]
    pub fn read_byte(&mut self) -> Option<u8> {
        self.inner.read_word().ok().flatten()
    }

    /// Block until the TX FIFO and shift register are empty.
    #[inline]
    pub fn flush(&mut self) {
        while self.inner.is_busy() {}
    }

    /// Disable the UART, restore the TX/RX pads to GPIO function (Func0), and
    /// return the consumed GPIO pin tokens so they can be used by other code.
    ///
    /// The returned `U` is the (zero-sized) PAC peripheral token; the returned
    /// `U::Pins` contains the TX and RX [`GpioPin`] tokens in their
    /// unconfigured state, ready to be passed to [`GpioPin::into_output`] /
    /// [`GpioPin::into_input`] or to another peripheral driver.
    ///
    /// # Note on `Drop`
    ///
    /// Calling `free` consumes `self` and prevents [`Drop`] from running — the
    /// clock is gated off here before the struct is dismantled.
    pub fn free(mut self) -> (U, U::Pins) {
        // Disable the peripheral and restore pads before dismantling the struct.
        // Clear UARTCR while the clock is live (matches Drop / spi_alt), then
        // gate. ManuallyDrop prevents Drop from running a second disable().
        self.inner.disable();
        U::ID.disable().ok();
        U::unpinmux();
        let mut md = core::mem::ManuallyDrop::new(self);
        // SAFETY: Each field is read exactly once through a raw pointer and
        // `md` is never used again after this point, so there is no
        // double-move or use-after-read. `inner` is explicitly dropped via
        // drop_in_place to run its destructor (mirroring normal field-drop
        // order); `uart` and `pins` are moved out by value. ManuallyDrop
        // ensures the struct-level Drop (which would call `disable()` again)
        // does not run.
        let uart = unsafe { core::ptr::read(&md.uart) };
        let pins = unsafe { core::ptr::read(&md.pins) };
        unsafe { core::ptr::drop_in_place(&mut md.inner) };
        (uart, pins)
    }

    /// Split the driver into independent [`UartTx`] and [`UartRx`] halves so
    /// the transmit and receive directions can be owned by different contexts
    /// (e.g. RX in an interrupt handler, TX in the main loop), or so a
    /// one-direction peripheral can be expressed by simply dropping the unused
    /// half.
    ///
    /// The UART stays enabled and configured: `split` only divides *software*
    /// ownership of the already-running peripheral. The two halves partition
    /// the PL011 register set — TX drives the data register's transmit FIFO and
    /// polls `TXFF`/`BUSY`; RX drains the receive FIFO and polls `RXFE`; neither
    /// touches the shared control/format registers — so they operate without
    /// coordination.
    ///
    /// # Note on `Drop` and the clock
    ///
    /// The halves have no [`Drop`]: the peripheral clock (and, for UART2, the
    /// [`Clock`] borrow encoded in `'clk`) stays held for as long as *either*
    /// half is alive. To gate the clock off and reclaim the GPIO pins, first
    /// recombine the halves with [`join`](Uart::join), then call
    /// [`free`](Uart::free) on the result.
    ///
    /// Pin-mux is unchanged — on this SoC the function mux routes both pads as a
    /// group, so the RX pad remains wired to the UART even if you keep only the
    /// TX half.
    pub fn split(self) -> (UartTx<'clk, U>, UartRx<'clk, U>) {
        // Keep the clock and pin-mux active — only dismantle the wrapper. The
        // ManuallyDrop suppresses Uart's Drop (which would gate the clock off).
        let mut md = core::mem::ManuallyDrop::new(self);
        // SAFETY: Each field is read exactly once through a raw pointer and
        // `md` is never used again, so there is no double-move or
        // use-after-read. `inner` is dropped in place (it owns only the MMIO
        // pointer and has no destructor of consequence); `uart` and `pins` are
        // moved out by value. ManuallyDrop ensures the struct-level Drop does
        // not run, so the clock stays enabled for the halves.
        let uart = unsafe { core::ptr::read(&md.uart) };
        let pins = unsafe { core::ptr::read(&md.pins) };
        unsafe { core::ptr::drop_in_place(&mut md.inner) };
        let (tx_pin, rx_pin) = U::split_pins(pins);
        (
            UartTx {
                uart,
                pin: tx_pin,
                _life: PhantomData,
            },
            UartRx {
                pin: rx_pin,
                _life: PhantomData,
            },
        )
    }

    /// Recombine [`UartTx`] and [`UartRx`] halves produced by [`split`](Uart::split)
    /// back into a full driver, restoring access to the whole-UART operations
    /// and enabling [`free`](Uart::free).
    ///
    /// Consuming both halves restores exclusive ownership of the peripheral, so
    /// rebuilding the single `pl011::Uart` wrapper is sound again. No hardware
    /// is reconfigured — the UART keeps the line settings and baud rate from the
    /// original [`new`](Uart::new).
    pub fn join(tx: UartTx<'clk, U>, rx: UartRx<'clk, U>) -> U::Output<'clk> {
        // Move the pin and peripheral tokens out of each half by field access
        // (neither half has a Drop impl, so partial moves are fine).
        let pins = U::join_pins(tx.pin, rx.pin);
        // SAFETY: identical to `Uart::new` — `U::regs()` is the fixed,
        // 'static-valid PL011 MMIO base. Both halves were just consumed, so no
        // other accessor to this peripheral remains and uniqueness is restored.
        // `pl011::Uart::new` only stores the pointer; it performs no register
        // writes, so the already-configured UART is left untouched.
        let ptr = NonNull::new(U::regs() as *mut pl011::PL011Registers)
            .expect("PL011 base address is null");
        let inner = pl011::Uart::new(unsafe { pl011::UniqueMmioPointer::new(ptr) });
        U::wrap(inner, tx.uart, pins)
    }
}

/// Transmit half of a [`Uart`] produced by [`Uart::split`].
///
/// Owns the PAC peripheral token (needed to [`Uart::join`]) and the TX GPIO
/// pin. Accesses only the PL011 transmit path — pushing bytes into the data
/// register's TX FIFO and polling `TXFF`/`BUSY` — never the shared
/// control/format registers, so it coexists with a live [`UartRx`].
///
/// Implements [`fmt::Write`], [`embedded_hal_nb::serial::Write`], and
/// [`embedded_io::Write`].
pub struct UartTx<'clk, U: UartPeriph> {
    uart: U,
    pin: U::TxPin,
    _life: PhantomData<&'clk ()>,
}

/// Receive half of a [`Uart`] produced by [`Uart::split`].
///
/// Owns the RX GPIO pin. Accesses only the PL011 receive path — draining the
/// data register's RX FIFO and polling `RXFE` — never the shared
/// control/format registers, so it coexists with a live [`UartTx`].
///
/// Implements [`embedded_hal_nb::serial::Read`] and [`embedded_io::Read`],
/// reporting the same overrun/break/parity/framing errors as the unsplit
/// [`Uart`].
pub struct UartRx<'clk, U: UartPeriph> {
    pin: U::RxPin,
    _life: PhantomData<&'clk ()>,
}

// Exclusive ownership of one direction of the peripheral. The TX half holds the
// PAC token (Send, matching the PAC `Periph` Send impl) plus a Send pin token;
// the RX half holds only a Send pin token. The split partitions the register
// set (TX FIFO / RX FIFO, both via independent volatile accesses), so handing a
// half to another execution context is sound on the same argument as the
// unsplit `Uart`'s Send impl.
unsafe impl<U: UartPeriph> Send for UartTx<'_, U> {}
unsafe impl<U: UartPeriph> Send for UartRx<'_, U> {}

impl<U: UartPeriph> UartTx<'_, U> {
    /// Shared `&'static` view of this UART's PL011 register block.
    ///
    /// SAFETY: `U::regs()` is the fixed, 'static-valid MMIO base for this
    /// peripheral (see [`Uart::new`]). The TX half only ever issues volatile
    /// writes to the data register and volatile reads of the flag register,
    /// which the RX half never writes, so the shared `&'static` view does not
    /// enable a conflicting access.
    #[inline]
    fn regs(&self) -> &'static pac::uart1::RegisterBlock {
        unsafe { &*U::regs() }
    }

    /// Transmit one byte, blocking until the TX FIFO has room.
    #[inline]
    pub fn write_byte(&mut self, byte: u8) {
        while self.regs().fr().read().txff().bit_is_set() {}
        self.regs().dr().write(|w| unsafe { w.bits(byte as u32) });
    }

    /// Block until the TX FIFO and shift register are empty.
    #[inline]
    pub fn flush(&mut self) {
        while self.regs().fr().read().busy().bit_is_set() {}
    }
}

impl<U: UartPeriph> UartRx<'_, U> {
    /// Shared `&'static` view of this UART's PL011 register block.
    ///
    /// SAFETY: as for [`UartTx::regs`], but the RX half only issues volatile
    /// reads of the data and flag registers; reading the data register pops the
    /// RX FIFO, an operation independent of the TX half's data-register writes.
    #[inline]
    fn regs(&self) -> &'static pac::uart1::RegisterBlock {
        unsafe { &*U::regs() }
    }

    /// Non-blocking read of a single byte, replicating
    /// [`arm_pl011_uart::Uart::read_word`]: `Ok(None)` when the RX FIFO is
    /// empty, `Err` on overrun/break/parity/framing (checked in that order).
    fn read_word(&mut self) -> Result<Option<u8>, pl011::Error> {
        let regs = self.regs();
        if regs.fr().read().rxfe().bit_is_set() {
            return Ok(None);
        }
        // Single volatile read latches the data byte and its error flags.
        let dr = regs.dr().read();
        if dr.oe().is_error() {
            Err(pl011::Error::Overrun)
        } else if dr.be().is_error() {
            Err(pl011::Error::Break)
        } else if dr.pe().is_error() {
            Err(pl011::Error::Parity)
        } else if dr.fe().is_error() {
            Err(pl011::Error::Framing)
        } else {
            Ok(Some(dr.bits() as u8))
        }
    }

    /// Read one byte if the RX FIFO is non-empty, otherwise return `None`.
    ///
    /// Returns `None` on both an empty FIFO and RX errors (overrun/parity/
    /// framing/break); use the [`embedded_hal_nb`] or [`embedded_io`] trait
    /// impls to observe error detail.
    #[inline]
    pub fn read_byte(&mut self) -> Option<u8> {
        self.read_word().ok().flatten()
    }
}

impl<U: UartPeriph> fmt::Write for UartTx<'_, U> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            self.write_byte(byte);
        }
        Ok(())
    }
}

impl<U: UartPeriph> ErrorType for UartTx<'_, U> {
    type Error = UartError;
}

impl<U: UartPeriph> embedded_hal_nb::serial::Write<u8> for UartTx<'_, U> {
    fn write(&mut self, word: u8) -> nb::Result<(), Self::Error> {
        if self.regs().fr().read().txff().bit_is_set() {
            Err(nb::Error::WouldBlock)
        } else {
            self.regs().dr().write(|w| unsafe { w.bits(word as u32) });
            Ok(())
        }
    }

    fn flush(&mut self) -> nb::Result<(), Self::Error> {
        if self.regs().fr().read().busy().bit_is_set() {
            Err(nb::Error::WouldBlock)
        } else {
            Ok(())
        }
    }
}

impl<U: UartPeriph> embedded_io::ErrorType for UartTx<'_, U> {
    type Error = UartError;
}

impl<U: UartPeriph> embedded_io::Write for UartTx<'_, U> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        for &byte in buf {
            self.write_byte(byte);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        while self.regs().fr().read().busy().bit_is_set() {}
        Ok(())
    }
}

impl<U: UartPeriph> ErrorType for UartRx<'_, U> {
    type Error = UartError;
}

impl<U: UartPeriph> embedded_hal_nb::serial::Read<u8> for UartRx<'_, U> {
    fn read(&mut self) -> nb::Result<u8, Self::Error> {
        match self.read_word() {
            Ok(Some(byte)) => Ok(byte),
            Ok(None) => Err(nb::Error::WouldBlock),
            Err(e) => Err(nb::Error::Other(UartError::Pl011(e))),
        }
    }
}

impl<U: UartPeriph> embedded_io::ErrorType for UartRx<'_, U> {
    type Error = UartError;
}

impl<U: UartPeriph> embedded_io::Read for UartRx<'_, U> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        if buf.is_empty() {
            return Ok(0);
        }
        // Block until at least one byte is available; the caller retries for
        // more. Mirrors `arm_pl011_uart`'s embedded-io Read impl.
        loop {
            match self.read_word() {
                Ok(Some(byte)) => {
                    buf[0] = byte;
                    return Ok(1);
                }
                Ok(None) => continue,
                Err(e) => return Err(UartError::Pl011(e)),
            }
        }
    }
}

impl<U: UartPeriph> Drop for Uart<'_, U> {
    fn drop(&mut self) {
        // Clear UARTCR (LBE/UARTEN/TXE/RXE) while the clock is still live, then
        // gate it — mirrors spi_alt's Drop so internal loopback never persists.
        self.inner.disable();
        U::ID.disable().ok();
    }
}

impl<U: UartPeriph> fmt::Write for Uart<'_, U> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        fmt::Write::write_str(&mut self.inner, s)
    }
}

impl<U: UartPeriph> ErrorType for Uart<'_, U> {
    type Error = UartError;
}

impl<U: UartPeriph> embedded_hal_nb::serial::Read<u8> for Uart<'_, U> {
    fn read(&mut self) -> nb::Result<u8, Self::Error> {
        embedded_hal_nb::serial::Read::read(&mut self.inner).map_err(|e| e.map(UartError::from))
    }
}

impl<U: UartPeriph> embedded_hal_nb::serial::Write<u8> for Uart<'_, U> {
    fn write(&mut self, word: u8) -> nb::Result<(), Self::Error> {
        embedded_hal_nb::serial::Write::write(&mut self.inner, word)
            .map_err(|e| e.map(UartError::from))
    }

    fn flush(&mut self) -> nb::Result<(), Self::Error> {
        embedded_hal_nb::serial::Write::flush(&mut self.inner).map_err(|e| e.map(UartError::from))
    }
}

impl<U: UartPeriph> embedded_io::ErrorType for Uart<'_, U> {
    type Error = UartError;
}

impl<U: UartPeriph> embedded_io::Write for Uart<'_, U> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        embedded_io::Write::write(&mut self.inner, buf).map_err(UartError::from)
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        embedded_io::Write::flush(&mut self.inner).map_err(UartError::from)
    }
}

impl<U: UartPeriph> embedded_io::Read for Uart<'_, U> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        embedded_io::Read::read(&mut self.inner, buf).map_err(UartError::from)
    }
}
