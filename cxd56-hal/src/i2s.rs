//! Profile-aware I2S0 driver (CXD5602 audio subsystem).
//!
//! Brings up I2S0 — the first I2S port of the audio block, exposed on the
//! Spresense main-board headers (JP1/JP2, Arduino D26/D25/D19/D18) — following
//! the same conventions as [`crate::i2c`]: generic over a sealed port
//! marker, consuming the four `gp_i2s0_*` GPIO pins and the [`pac::Audio`]
//! token, and configured against a [`Clock`].
//!
//! # Scope
//!
//! The driver enables the audio MCLK clock domain, routes the four I2S0 pads
//! to the I2S0 function, configures the I2S0 port (master/slave, format,
//! sample-rate class, data-path enables) and powers the digital path
//! (codec DSP + SRC1). The port sequence mirrors `cxd56_i2s0_config`
//! (`nuttx/arch/arm/src/cxd56xx/cxd56_audio_driver.c:382`); the power-up
//! mirrors `cxd56_audio_ac_reg_poweron_codec`/`poweron_i2s`
//! (`nuttx/boards/arm/cxd56xx/drivers/audio/cxd56_audio_ac_reg.c`).
//!
//! Sample I/O is DMA-only — the CXD5602 I2S has no CPU-accessible PCM FIFO.
//! [`I2s::write_16_blocking`], [`I2s::read_16_blocking`] and
//! [`I2s::transfer_16_blocking`] drive one-shot transfers through the audio
//! DMA (BCA). With an external `DATA_OUT`→`DATA_IN` jumper (Arduino D18→D19)
//! the duplex transfer forms a loopback — see `examples/rust_i2s0_loopback`.
//!
//! # Lifetime — why there is none
//!
//! The I2S0 timing reference is the **audio MCLK**, which on the Spresense main
//! board is the **external audio crystal** ([`AudMclk::Ext`]). That is a
//! [`Fixed`](crate::clocks::Fixed) clock — it does not track `appsmp`, the
//! quantity changed by [`Clock::request_perf`]. Master BCK/LRCK therefore stay
//! correct across HP↔LP transitions, so (like [`crate::i2c::I2c`]) there is
//! no need to hold a `&Clock` borrow after construction.

use core::marker::PhantomData;

use crate::clocks::{AudMclk, Clock, ClockError, audio_clock_disable, audio_clock_enable};
use crate::gpio::GpioPin;
use crate::pac;
use crate::regs::topreg;
use thiserror::Error;

/// Errors from the I2S driver.
#[derive(Debug, Error)]
pub enum I2sError {
    /// Audio clock-domain enable failed.
    #[error("clock gate error: {0}")]
    Clock(#[from] ClockError),
    /// The hardware rejected the DMA channel setup (monitor error code).
    #[error("DMA channel setup error: {0}")]
    DmaChannel(u8),
    /// A DMA transfer did not complete within the poll budget.
    #[error("DMA timeout")]
    DmaTimeout,
    /// The DMA reported a transfer (bus) error.
    #[error("DMA transfer error")]
    Dma,
}

/// Serial data format (`I2S_CTRL.I2S0_FMT` / DIF1).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum I2sFormat {
    /// Standard I2S (MSB delayed one BCK after LRCK edge).
    I2s,
    /// Left-justified. Per the User Manual this also requires `LR_SWAP1`, which
    /// this bring-up driver does not configure.
    LeftJustified,
}

/// Sample-rate class. Selects the SRC band, `HI_RES_MODE`, and `I2SALL_DATARATE`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum I2sRate {
    /// Up to 48 kHz (`SRC1 = UT48K`, normal resolution).
    R48k,
    /// 192 kHz (`SRC1 = UT192K`, hi-res).
    R192k,
}

/// Which serial data paths to enable.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum I2sDir {
    /// Transmit only (`SDOUT1`).
    Tx,
    /// Receive only (`SDIN1`).
    Rx,
    /// Both directions.
    Duplex,
}

/// I2S0 configuration. [`Default`] is master / I2S / 48 kHz / duplex.
#[derive(Copy, Clone, Debug)]
pub struct I2sConfig {
    /// `true` = master (CXD5602 drives BCK/LRCK), `false` = slave.
    pub master: bool,
    /// Serial data format.
    pub format: I2sFormat,
    /// Sample-rate class.
    pub rate: I2sRate,
    /// Enabled data direction(s).
    pub direction: I2sDir,
}

impl Default for I2sConfig {
    fn default() -> Self {
        Self {
            master: true,
            format: I2sFormat::I2s,
            rate: I2sRate::R48k,
            direction: I2sDir::Duplex,
        }
    }
}

/// BCK/LRCK pad rates read back from the I2S0 block — see
/// [`I2s::frame_clocks`](crate::i2s::I2s::frame_clocks).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FrameClocks {
    /// LRCK (word/frame clock) at the pad, in Hz.
    pub lrck_hz: u32,
    /// BCK (bit clock) at the pad, in Hz (= `lrck_hz * 64`).
    pub bck_hz: u32,
    /// `true` when I2S0 drives the BCK/LRCK pads (master mode); `false` = slave
    /// (the pads are inputs and the rates are the expected external source).
    pub is_master: bool,
    /// `true` when the block is in hi-res (192 kHz) mode.
    pub hires: bool,
}

/// GPIO tokens for the I2S0 pads.
///
/// Consumed by [`I2s::new`] to enforce at the type level that no other code can
/// drive these pads while I2S0 is live. Returned by [`I2s::free`].
///
/// | Field      | Spresense pad   | Arduino | Header |
/// |------------|-----------------|---------|--------|
/// | `bck`      | I2S0_BCK        | D26     | JP1-6  |
/// | `lrck`     | I2S0_LRCK       | D25     | JP1-7  |
/// | `data_in`  | I2S0_DATA_IN    | D19     | JP2-6  |
/// | `data_out` | I2S0_DATA_OUT   | D18     | JP2-7  |
pub struct I2s0Pins {
    pub bck: GpioPin<pac::topreg::GpI2s0Bck>,
    pub lrck: GpioPin<pac::topreg::GpI2s0Lrck>,
    pub data_in: GpioPin<pac::topreg::GpI2s0DataIn>,
    pub data_out: GpioPin<pac::topreg::GpI2s0DataOut>,
}

// ============================================================================
// Sealed port trait
// ============================================================================

mod sealed {
    pub trait Sealed {}
}

/// Maps an I2S port marker to its pins, pin-mux, and audio-block register setup.
///
/// Sealed — implemented only for [`I2s0`] today. I2S1 (the second port, on the
/// main-board LED pads) can be added as a second impl: both ports share the one
/// [`pac::Audio`] block, which is why the generic parameter is a *port* marker
/// rather than a PAC peripheral token.
pub trait I2sPort: sealed::Sealed {
    /// GPIO pin bundle consumed at construction and returned by [`I2s::free`].
    type Pins;

    /// Route this port's pads to the I2S function.
    fn pinmux();
    /// Restore this port's pads to Func0 (GPIO).
    fn unpinmux();
    /// Program the audio-block port registers from `config`.
    fn configure(audio: &pac::audio::RegisterBlock, config: &I2sConfig);
    /// Disable the port's data paths / clock output (reverses [`Self::configure`]).
    fn teardown(audio: &pac::audio::RegisterBlock);
}

/// Marker for the I2S0 port (audio "channel 1": `SD1`/`SDIN1`/`SDOUT1`).
pub struct I2s0;

impl sealed::Sealed for I2s0 {}

impl I2sPort for I2s0 {
    type Pins = I2s0Pins;

    fn pinmux() {
        i2s0_pinmux();
    }

    fn unpinmux() {
        i2s0_unpinmux();
    }

    fn configure(audio: &pac::audio::RegisterBlock, config: &I2sConfig) {
        i2s0_configure(audio, config);
    }

    fn teardown(audio: &pac::audio::RegisterBlock) {
        i2s0_teardown(audio);
    }
}

// ============================================================================
// I2S0 pin-mux + register helpers (mirror cxd56_i2s0_config)
// ============================================================================

// IO-cell pad config + IOCAPP_IOMD.I2S0 = Func1. The four I2S0 pads share the
// `IOCAPP_IOMD.i2s0` 2-bit mux field. BCK/LRCK/DATA_OUT are driven (master) /
// hi-Z (slave) by the audio block; DATA_IN enables its input buffer (ENZI).
fn i2s0_pinmux() {
    let t = topreg();
    t.io_i2s0_bck().write(|w| w.lowemi().set_bit());
    t.io_i2s0_lrck().write(|w| w.lowemi().set_bit());
    t.io_i2s0_data_out().write(|w| w.lowemi().set_bit());
    t.io_i2s0_data_in()
        .write(|w| w.lowemi().set_bit().enzi().set_bit());
    t.iocapp_iomd().modify(|_, w| unsafe { w.i2s0().bits(1) });
}

fn i2s0_unpinmux() {
    topreg()
        .iocapp_iomd()
        .modify(|_, w| unsafe { w.i2s0().bits(0) });
}

// Clear the codec/SRC DSP coefficient RAM and soft-reset the DSPs — mirrors
// `cxd56_audio_ac_reg_initdsp`. The audio block powers up with garbage in DSP
// RAM; without clearing it the digital path mangles samples (a loopback returns
// data that is non-zero but not bit-exact). Runs once before the port config.
fn ac_initdsp(audio: &pac::audio::RegisterBlock) {
    // Power every DSP block up so its RAM is writable.
    audio.ac_pdn().modify(|_, w| {
        w.pdn_dspb().clear_bit();
        w.pdn_dsps2().clear_bit();
        w.pdn_dsps1().clear_bit();
        w.pdn_dspc().clear_bit()
    });
    audio.test_ctrl().modify(|_, w| {
        w.dspram1_clr()
            .set_bit()
            .dspram2_clr()
            .set_bit()
            .dspram4_clr()
            .set_bit()
    });
    crate::clocks::pmu::delay_us(1_000); // >= 512 cycles @ 24.576 MHz
    audio.test_ctrl().modify(|_, w| {
        w.dspram1_clr()
            .clear_bit()
            .dspram2_clr()
            .clear_bit()
            .dspram4_clr()
            .clear_bit()
    });
    // Power the DSPs back down, then pulse the soft reset.
    audio.ac_pdn().modify(|_, w| {
        w.pdn_dspb().set_bit();
        w.pdn_dsps2().set_bit();
        w.pdn_dsps1().set_bit();
        w.pdn_dspc().set_bit()
    });
    audio.test_ctrl().modify(|_, w| w.s_reset().set_bit());
    audio.test_ctrl().modify(|_, w| w.s_reset().clear_bit());
}

// Pulse the DSP soft reset and wait 20 ms for the digital path to settle —
// mirrors `cxd56_audio_ac_reg_resetdsp`. Runs after the port is configured but
// before the data paths are enabled.
fn ac_resetdsp(audio: &pac::audio::RegisterBlock) {
    audio.test_ctrl().modify(|_, w| w.s_reset().set_bit());
    audio.test_ctrl().modify(|_, w| w.s_reset().clear_bit());
    crate::clocks::pmu::delay_us(20_000);
}

// Mirrors cxd56_i2s0_config(en=true) + cxd56_i2sclock_out + the digital-path
// power-up of cxd56_audio_ac_reg_poweron_codec / poweron_i2s0 / enable_dma.
fn i2s0_configure(audio: &pac::audio::RegisterBlock, config: &I2sConfig) {
    // AHB clocks: I2S0 DMA port + the audio AHB master (BCA fetch engine).
    audio
        .ahb_i2s_clken()
        .modify(|_, w| w.ahbi2s1_clken().set_bit());
    audio.ac_clken().modify(|_, w| w.mck_ahbmstr_en().set_bit());

    // Clear DSP RAM + soft-reset before configuring the path.
    ac_initdsp(audio);

    // Power up the digital path: codec DSP (DSPC), the I2S0 sample-rate
    // converter (DSPS1 = SRC1) and the output filter (DSPB). Without these the
    // port clocks run but no data moves.
    audio.ac_pdn().modify(|_, w| {
        w.pdn_dspc().clear_bit();
        w.pdn_dsps1().clear_bit();
        w.pdn_dspb().clear_bit()
    });

    // SRC band / resolution / data-rate from the requested sample-rate class.
    // SRC1 = 0 is a *prohibited* setting (User Manual §3.15) — the enabled path
    // always uses UT48K(1)/UT192K(3), never 0.
    let (src1, hires, datarate): (u8, bool, bool) = match config.rate {
        I2sRate::R48k => (1, false, false), // CXD56_AUDI2SSRC_UT48K
        I2sRate::R192k => (3, true, true),  // CXD56_AUDI2SSRC_UT192K
    };
    let left_justified = matches!(config.format, I2sFormat::LeftJustified);

    audio.i2s_ctrl().modify(|_, w| {
        w.sd1master().bit(config.master);
        w.i2s0_fmt().bit(left_justified);
        // SDCK_OUTENX is active-low (0 = BCK/LRCK output enable). Per the User
        // Manual §3.15.6.8.1 master mode wants 0; slave 1. (NuttX's
        // `cxd56_i2sclock_out` writes the inverse, but its own
        // `cxd56_audio_ac_reg_enable_i2s_bcklrckout` writes 0 — the UM polarity
        // is authoritative and matches the reset default of 0.)
        w.sdck_outenx().bit(!config.master);
        w.hi_res_mode().bit(hires);
        // Codec DSP downsampling rate + digital soft-ramp (poweron_codec
        // defaults).
        unsafe { w.dsr_rate().bits(1) };
        w.digsft().set_bit();
        // SRC1 input source = I2S0 DMA (via AU_DAT_SEL1); SRC1 band per rate.
        unsafe { w.src1in_sel().bits(0) };
        unsafe { w.src1().bits(src1) }
    });

    // L/R swap accompanies the left-justified format (poweron_i2s0).
    audio
        .lr_swap()
        .modify(|_, w| w.lr_swap1().bit(left_justified));

    // Route the I2S0 TX DMA bus interface (BUSIF1 = 4) into SRC1's down input.
    audio
        .ac_dat_sel()
        .modify(|_, w| unsafe { w.au_dat_sel1().bits(4) });

    // Bypass the SRC filter (raw I2S), let the SRC clock run, disable its auto
    // mute, and mask the BCK/LRCK error interrupt.
    audio.test_ctrl().modify(|_, w| {
        w.test_out_sel0().set_bit();
        w.halt_inhibit().clear_bit();
        w.arwphset().clear_bit()
    });
    audio
        .i2s_spclkerr()
        .modify(|_, w| w.m_spclkerr1().clear_bit());

    // Sample-rate class.
    audio
        .i2s_datarate()
        .modify(|_, w| w.i2sall_datarate().bit(datarate));

    // Settle the DSP path (20 ms) before opening the data paths.
    ac_resetdsp(audio);

    // Serial data-path enables + the BLF output filter block.
    let (tx, rx) = match config.direction {
        I2sDir::Tx => (true, false),
        I2sDir::Rx => (false, true),
        I2sDir::Duplex => (true, true),
    };
    audio
        .i2s_data_en()
        .modify(|_, w| w.sdout1_en().bit(tx).sdin1_en().bit(rx).blf_en().set_bit());

    // Master clock *pad* output enable (TOPREG IOOEN_APP, active-low): clear for
    // master so BCK/LRCK reach the pins; set (disable) for slave.
    topreg().iooen_app().modify(|_, w| {
        w.i2s0_bck().bit(!config.master);
        w.i2s0_lrck().bit(!config.master)
    });
}

fn i2s0_teardown(audio: &pac::audio::RegisterBlock) {
    audio.i2s_data_en().modify(|_, w| {
        w.sdout1_en().clear_bit();
        w.sdin1_en().clear_bit();
        w.blf_en().clear_bit()
    });
    // Disable the master clock pads (restore IOOEN_APP reset state for I2S0).
    topreg()
        .iooen_app()
        .modify(|_, w| w.i2s0_bck().set_bit().i2s0_lrck().set_bit());
    // Power the digital path back down and stop the clocks (reverse of
    // configure; DSPC is shared codec infrastructure but this driver is the
    // only audio user, so it goes down too).
    audio.ac_pdn().modify(|_, w| {
        w.pdn_dsps1().set_bit();
        w.pdn_dspb().set_bit();
        w.pdn_dspc().set_bit()
    });
    audio.ac_clken().modify(|_, w| w.mck_ahbmstr_en().clear_bit());
    audio
        .ahb_i2s_clken()
        .modify(|_, w| w.ahbi2s1_clken().clear_bit());
}

// ============================================================================
// I2s<P> driver
// ============================================================================

/// Generic I2S driver. `P` is a port marker (currently only [`I2s0`]).
///
/// Constructed via [`I2s::new`]; holds the consumed [`pac::Audio`] token and the
/// GPIO pin bundle until [`I2s::free`].
pub struct I2s<P: I2sPort> {
    audio: pac::Audio,
    pins: P::Pins,
    _port: PhantomData<P>,
}

impl<P: I2sPort> I2s<P> {
    /// Enable the audio MCLK domain, route the pads to I2S, and configure the
    /// port from `config`.
    ///
    /// `clock` is taken to mirror the other peripheral constructors and to require
    /// the clock tree to be brought up first; the I2S0 timing reference (audio
    /// MCLK) is [`Fixed`](crate::clocks::Fixed), so the borrow ends here and
    /// [`Clock::request_perf`] stays callable afterwards (no lifetime retained).
    ///
    /// `pins` enforces at the type level that no other code drives these pads
    /// while I2S0 is live. Call [`I2s::free`] to release them.
    pub fn new(
        audio: pac::Audio,
        pins: P::Pins,
        clock: &Clock,
        config: I2sConfig,
    ) -> Result<Self, I2sError> {
        let _ = clock;
        // Spresense main board: audio MCLK from the external audio crystal.
        audio_clock_enable(AudMclk::Ext, 0)?;
        P::pinmux();
        P::configure(&audio, &config);
        Ok(Self {
            audio,
            pins,
            _port: PhantomData,
        })
    }

    /// Disable the port, restore the pads to GPIO (Func0), drop the audio clock
    /// domain, and return the consumed [`pac::Audio`] token and GPIO pins.
    pub fn free(self) -> (pac::Audio, P::Pins) {
        P::teardown(&self.audio);
        P::unpinmux();
        audio_clock_disable().ok();
        let md = core::mem::ManuallyDrop::new(self);
        // SAFETY: each field is moved out exactly once via a raw pointer read.
        // ManuallyDrop suppresses the struct-level Drop (which would tear down
        // the clock a second time).
        let audio = unsafe { core::ptr::read(&md.audio) };
        let pins = unsafe { core::ptr::read(&md.pins) };
        (audio, pins)
    }
}

impl<P: I2sPort> Drop for I2s<P> {
    fn drop(&mut self) {
        P::teardown(&self.audio);
        audio_clock_disable().ok();
    }
}

// ============================================================================
// One-shot DMA transfers (I2S0)
// ============================================================================

/// Poll budget for blocking DMA waits. At 48 kHz a 4096-word transfer takes
/// ~85 ms; this spin count bounds the wait well past that at any CPU clock
/// without needing a timer.
const DMA_POLL_BUDGET: u32 = 50_000_000;

/// Samples are 16-bit stereo: one `u32` word = `(right << 16) | left`.
///
/// The audio DMA (BCA) sequences mirror NuttX `cxd56_audio_bca_reg.c`
/// (`enable_i2s1_out_fmt16` / `enable_i2s1_in_fmt16` / `start_dma`) with the
/// done/error status polled instead of interrupt-driven. BCA register names
/// say "I2S1" for this port — that is the audio block's channel-1 naming for
/// the I2S0 pins.
impl I2s<I2s0> {
    /// Transmit `tx` out `SDOUT1` as one DMA shot, blocking until done.
    pub fn write_16_blocking(&mut self, tx: &[u32]) -> Result<(), I2sError> {
        if tx.is_empty() {
            return Ok(());
        }
        self.tx_setup_start(tx);
        self.wait_tx_done()
    }

    /// Capture `rx.len()` samples from `SDIN1` as one DMA shot, blocking.
    pub fn read_16_blocking(&mut self, rx: &mut [u32]) -> Result<(), I2sError> {
        if rx.is_empty() {
            return Ok(());
        }
        self.rx_setup_start(rx);
        self.wait_rx_done()
    }

    /// Full-duplex one-shot: start capturing into `rx`, then transmit `tx`,
    /// and block until both DMAs complete.
    ///
    /// RX is armed first so it is already capturing when the first TX sample
    /// hits the wire — with an external `DATA_OUT`→`DATA_IN` jumper (Arduino
    /// D18→D19, JP2-7→JP2-6) in master mode this loops the transmitted frames
    /// back into `rx` (after the SRC group delay, so size `rx` larger than
    /// `tx` and search for the pattern).
    pub fn transfer_16_blocking(&mut self, tx: &[u32], rx: &mut [u32]) -> Result<(), I2sError> {
        if tx.is_empty() || rx.is_empty() {
            return Err(I2sError::DmaChannel(0));
        }
        self.rx_setup_start(rx);
        self.tx_setup_start(tx);
        self.wait_tx_done()?;
        self.wait_rx_done()
    }

    /// CXD5247 audio master clock feeding the I2S0 block: 24.576 MHz.
    ///
    /// This is the [`Fixed`](crate::clocks::Fixed) crystal-derived reference the
    /// companion's `POWER_ON_COMMON` starts; BCK/LRCK are integer divisions of
    /// it (User Manual §3.15.6, audio regdef "Fb = MCLK/512").
    pub const MCLK_HZ: u32 = 24_576_000;

    /// Master-mode BCK/LRCK **pad** rates, read back from the hardware.
    ///
    /// Returns `(lrck_hz, bck_hz)` derived deterministically from [`MCLK_HZ`]
    /// and the live `I2S_CTRL.HI_RES_MODE` bit: normal mode drives
    /// LRCK = MCLK/512 (48 kHz) and BCK = MCLK/8 (3.072 MHz, = LRCK×64); hi-res
    /// mode drives LRCK = MCLK/128 (192 kHz) and BCK = MCLK/2 (12.288 MHz).
    ///
    /// This is the authoritative on-chip view of the **pin** clock: unlike timing
    /// a capture DMA — which observes the SRC's *internal* rate (2× the frame
    /// clock in the 8K–48K SRC mode) rather than the pad — it computes the pad
    /// rate from the actual rate-mode register and the known MCLK. `is_master`
    /// reports whether I2S0 actually drives these pads (`SD1MASTER`); in slave
    /// mode the pads are inputs and the returned rates are the expected source.
    ///
    /// [`MCLK_HZ`]: Self::MCLK_HZ
    pub fn frame_clocks(&self) -> FrameClocks {
        let ctrl = self.audio.i2s_ctrl().read();
        let hires = ctrl.hi_res_mode().bit_is_set();
        let (lrck_div, bck_div) = if hires { (128, 2) } else { (512, 8) };
        FrameClocks {
            lrck_hz: Self::MCLK_HZ / lrck_div,
            bck_hz: Self::MCLK_HZ / bck_div,
            is_master: ctrl.sd1master().bit_is_set(),
            hires,
        }
    }

    fn tx_setup_start(&mut self, buf: &[u32]) {
        // The DMA reads `buf` from RAM; make sure the compiler has actually
        // committed the buffer contents before the trigger below.
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        let audio = &self.audio;
        // 16-bit packed samples; SD1 left <- DMA channel L, right <- DMA ch R.
        audio.i2s1_out_bitwt().write(|w| w.bitwt().set_bit());
        audio
            .i2s1_out_sd_sel()
            .write(|w| unsafe { w.sd1_l_sel().bits(0).sd1_r_sel().bits(1) });
        // Clear stale TX status (write-1-to-clear).
        audio
            .i2s1_int_ctrl()
            .write(|w| w.done_i2so().set_bit().err_i2so().set_bit());
        // Buffer address (bits [31:2] — the field holds byte address >> 2) and
        // length (samples - 1), then trigger.
        audio
            .i2s1_out_start_adr()
            .write(|w| unsafe { w.start_adr().bits(buf.as_ptr() as u32 >> 2) });
        audio
            .i2s1_out_sample_no()
            .write(|w| unsafe { w.sample_no().bits(buf.len() as u32 - 1) });
        audio
            .i2s1_out_rtd_trg()
            .write(|w| unsafe { w.rtd_trg().bits(1) });
    }

    fn rx_setup_start(&mut self, buf: &mut [u32]) {
        let audio = &self.audio;
        // 16-bit packed samples; DMA channel 1 <- SRC1L, channel 2 unused
        // (enable_i2s1_in_fmt16: in 16-bit mode channel 1 carries the packed
        // L/R pair).
        audio.i2s1_in_bitwt().write(|w| w.bitwt().set_bit());
        audio
            .i2s1_in_ch_sel()
            .write(|w| unsafe { w.ch1_sel().bits(0).ch2_sel().bits(2) });
        audio
            .i2s1_int_ctrl()
            .write(|w| w.done_i2si().set_bit().err_i2si().set_bit());
        audio
            .i2s1_in_start_adr()
            .write(|w| unsafe { w.start_adr().bits(buf.as_mut_ptr() as u32 >> 2) });
        audio
            .i2s1_in_sample_no()
            .write(|w| unsafe { w.sample_no().bits(buf.len() as u32 - 1) });
        audio
            .i2s1_in_rtd_trg()
            .write(|w| unsafe { w.rtd_trg().bits(1) });
    }

    fn wait_tx_done(&mut self) -> Result<(), I2sError> {
        let mon = self.audio.i2s1_out_mon().read().mon_error_setting().bits();
        if mon != 0 {
            return Err(I2sError::DmaChannel(mon));
        }
        let mut err_seen = false;
        for _ in 0..DMA_POLL_BUDGET {
            let sts = self.audio.i2s1_int_ctrl().read();
            if sts.err_i2so().bit_is_set() {
                // The first error after a start can be the silicon's spurious
                // start-glitch (NuttX masks + clears it once — its "error
                // interrupt workaround"); a repeat is a real bus error.
                if err_seen {
                    return Err(I2sError::Dma);
                }
                err_seen = true;
                self.audio.i2s1_int_ctrl().write(|w| w.err_i2so().set_bit());
            }
            if sts.done_i2so().bit_is_set() {
                self.audio
                    .i2s1_int_ctrl()
                    .write(|w| w.done_i2so().set_bit());
                return Ok(());
            }
        }
        Err(I2sError::DmaTimeout)
    }

    fn wait_rx_done(&mut self) -> Result<(), I2sError> {
        let mon = self.audio.i2s1_in_mon().read().mon_error_setting().bits();
        if mon != 0 {
            return Err(I2sError::DmaChannel(mon));
        }
        let mut err_seen = false;
        for _ in 0..DMA_POLL_BUDGET {
            let sts = self.audio.i2s1_int_ctrl().read();
            if sts.err_i2si().bit_is_set() {
                if err_seen {
                    return Err(I2sError::Dma);
                }
                err_seen = true;
                self.audio.i2s1_int_ctrl().write(|w| w.err_i2si().set_bit());
            }
            if sts.done_i2si().bit_is_set() {
                self.audio
                    .i2s1_int_ctrl()
                    .write(|w| w.done_i2si().set_bit());
                // The DMA wrote the caller's buffer behind the compiler's
                // back; order those stores before the caller reads it.
                core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
                return Ok(());
            }
        }
        Err(I2sError::DmaTimeout)
    }
}
