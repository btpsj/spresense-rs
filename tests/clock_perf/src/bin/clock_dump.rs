//! Operating-point **ground-truth** dump (diagnostic, not a pass/fail test).
//!
//! Goal: find out what operating point a SYSIOP FREQLOCK→LV transition actually
//! reaches, register by register, so `request_perf(Perf::Lp)` can be made to
//! reach the **canonical LV** point from the User Manual (Table UART-792, XOSC
//! 26 MHz, Low Power Mode): SYSPLL 195.0 MHz (unchanged) · COM 32.5 MHz.
//!
//! Design notes (learned the hard way):
//!   * The LP console is unusable, so we **measure at LP but report at HP**:
//!     snapshot the clock registers into RAM during the excursion, return to
//!     HP, then print.
//!   * The PM handshake is driven **manually with bounded `try_recv`** rather
//!     than the HAL's blocking `request_perf`, so a missing/extra message can
//!     never hang us — we always make it back to print the data, and we log the
//!     exact message sequence (how many CLK_CHG pairs, trailing FREQLOCK?).
//!   * The console UART is built **last, from a fresh live snapshot** of the
//!     recovered clock, so its baud is correct no matter where we ended up.
//!
//! Run: `cargo run --release --bin clock_dump` (from tests/clock_perf).
//! No external jumper. CXD5602 GPIO is 1.8 V.

#![no_std]
#![no_main]

use cortex_m::asm;
use cortex_m_rt::entry;
use defmt_serial as _;
use panic_probe as _;
use static_cell::StaticCell;

use cxd56_hal::clocks::{Clock, Config, RccExt};
use cxd56_hal::gpio::pins::Parts;
use cxd56_hal::multicore::Mailbox;
use cxd56_hal::pac;
use cxd56_hal::uart::{Uart, Uart1Pins, UartConfig};

static SERIAL: StaticCell<Uart<'static, pac::Uart1>> = StaticCell::new();
// UART1 borrows the `Clock` for its lifetime (COM is a Dyn clock), so the
// `Clock` must outlive the `'static` UART stored in `SERIAL`.
static CLOCK: StaticCell<Clock> = StaticCell::new();

/// `APP_CKSEL` lives in topreg_sub (offset 0x418); read raw like `buses.rs`.
const APP_CKSEL_ADDR: usize = 0x0410_3418;

// PM CPU-FIFO protocol (mirrors the private constants in `clocks::pm`).
const PM_PROTO: u32 = 10;
const PM_BOOT: u16 = 0;
const PM_FREQLOCK: u16 = 1;
const PM_CLK_CHG_START: u16 = 2;
const PM_CLK_CHG_END: u16 = 3;

// FREQLOCK flag bits (arch/arm/include/cxd56xx/pm.h).
const FLAG_INITIALIZED: u32 = 0x8000;
const FLAG_HV: u32 = 0x0001;
const FLAG_LV: u32 = 0x4000;

/// SYSIOP reads this on BOOT to learn the APP's per-domain callback config.
/// All-zero == no callbacks (same as NuttX with no PM-registered drivers).
static PM_TABLE: [u32; 6] = [0; 6];

/// Pack a PM message to the SYSIOP (cpuid 0): `word0 = proto<<24 | pdata`.
fn pm_msg(pdata: u16, data: u32) -> [u32; 2] {
    [(PM_PROTO << 24) | (pdata as u32 & 0xffff), data]
}

/// One operating-point snapshot: the raw root-clock-tree registers plus the
/// HAL's decode of them (so we see both the silicon and what the HAL believes).
#[derive(Copy, Clone)]
struct Snap {
    cksel_root: u32,
    ckdiv_cpu_dsp_bus: u32,
    ckdiv_com: u32,
    sys_pll_ctrl2: u32,
    app_cksel: u32,
    syspll: u32,
    sys: u32,
    sys_ahb: u32,
    sys_apb: u32,
    com: u32,
    appsmp: u32,
    img_uart: u32,
    gps_cpu: u32,
}

fn capture(clock: &Clock) -> Snap {
    // SAFETY: fixed MMIO base, read-only access (same invariant as regs.rs).
    let t = unsafe { &*pac::Topreg::PTR };
    let c = clock.freeze(); // always reads registers live
    Snap {
        cksel_root: t.cksel_root().read().bits(),
        ckdiv_cpu_dsp_bus: t.ckdiv_cpu_dsp_bus().read().bits(),
        ckdiv_com: t.ckdiv_com().read().bits(),
        sys_pll_ctrl2: t.sys_pll_ctrl2().read().bits(),
        app_cksel: unsafe { core::ptr::read_volatile(APP_CKSEL_ADDR as *const u32) },
        syspll: c.syspll.to_Hz(),
        sys: c.sys.to_Hz(),
        sys_ahb: c.sys_ahb.to_Hz(),
        sys_apb: c.sys_apb.to_Hz(),
        com: c.com.to_Hz(),
        appsmp: c.appsmp.to_Hz(),
        img_uart: c.img_uart.to_Hz(),
        gps_cpu: c.gps_cpu.to_Hz(),
    }
}

fn print_snap(name: &str, s: &Snap) {
    defmt::println!(
        "{=str}: REG cksel_root={=u32:#x} ckdiv_cpu_dsp_bus={=u32:#x} ckdiv_com={=u32:#x} sys_pll_ctrl2={=u32:#x} app_cksel={=u32:#x}",
        name, s.cksel_root, s.ckdiv_cpu_dsp_bus, s.ckdiv_com, s.sys_pll_ctrl2, s.app_cksel
    );
    defmt::println!(
        "{=str}: CLK syspll={=u32} sys={=u32} sys_ahb={=u32} sys_apb={=u32} com={=u32} appsmp={=u32} img_uart={=u32} gps_cpu={=u32}",
        name, s.syspll, s.sys, s.sys_ahb, s.sys_apb, s.com, s.appsmp, s.img_uart, s.gps_cpu
    );
}

fn within(meas: u32, want: u32, tol_pct: u32) -> bool {
    (meas.abs_diff(want) as u64) * 100 <= want as u64 * tol_pct as u64
}

fn pm_name(pdata: u16) -> &'static str {
    match pdata {
        PM_BOOT => "BOOT",
        PM_FREQLOCK => "FREQLOCK",
        PM_CLK_CHG_START => "CLK_CHG_START",
        PM_CLK_CHG_END => "CLK_CHG_END",
        _ => "?",
    }
}

/// Drive a full SYSIOP voltage-mode change manually and log every message.
///
/// Sends `FREQLOCK(flag)`, then acks each `CLK_CHG_START`/`CLK_CHG_END` (so a
/// multi-step downshift can complete) until the trailing `FREQLOCK` reply
/// (NuttX's real completion signal) or the FIFO goes idle. Bounded by an
/// idle-spin budget so it can never hang. Returns `(count, saw_trailing)`.
fn run_freqlock(flag: u32, log: &mut [(u16, u32)]) -> (usize, bool) {
    Mailbox::send_blocking(pm_msg(PM_FREQLOCK, flag));
    let mut n = 0usize;
    let mut empties = 0u32;
    let mut saw_trailing = false;
    while n < log.len() && empties < 3_000_000 {
        match Mailbox::try_recv() {
            Some(w) => {
                empties = 0;
                if (w[0] >> 24) & 0xf == PM_PROTO {
                    let pdata = (w[0] & 0xffff) as u16;
                    log[n] = (pdata, w[1]);
                    n += 1;
                    match pdata {
                        PM_CLK_CHG_START => Mailbox::send_blocking(pm_msg(PM_CLK_CHG_START, 0)),
                        PM_CLK_CHG_END => Mailbox::send_blocking(pm_msg(PM_CLK_CHG_END, 0)),
                        PM_FREQLOCK => {
                            saw_trailing = true;
                            break;
                        }
                        _ => {}
                    }
                }
            }
            None => empties += 1,
        }
    }
    (n, saw_trailing)
}

fn print_log(label: &str, log: &[(u16, u32)], n: usize, saw_trailing: bool) {
    defmt::println!("{=str}: {=usize} msgs, trailing FREQLOCK={=bool}", label, n, saw_trailing);
    for (pdata, data) in log.iter().take(n) {
        defmt::println!("  pdata={=u16} ({=str}) data={=u32:#x}", *pdata, pm_name(*pdata), *data);
    }
}

#[entry]
fn main() -> ! {
    let dp = pac::Peripherals::take().unwrap();
    let clock = dp.crg.constrain(Config::default()).into_clock();

    // --- Excursion: capture HP, drop to LV, capture LP, return to HV. No UART
    //     and no printing here; everything goes into RAM. ----------------------
    let hp = capture(&clock);

    // Boot the PM protocol (SYSIOP learns the target-id table address), settle.
    Mailbox::send_blocking(pm_msg(PM_BOOT, PM_TABLE.as_ptr() as u32));
    asm::delay(200_000);

    let mut lp_log: [(u16, u32); 24] = [(0, 0); 24];
    let (lp_n, lp_tr) = run_freqlock(FLAG_INITIALIZED | FLAG_LV, &mut lp_log);
    let lp = capture(&clock);

    let mut hp_log: [(u16, u32); 24] = [(0, 0); 24];
    let (hp_n, hp_tr) = run_freqlock(FLAG_INITIALIZED | FLAG_HV, &mut hp_log);
    let hp_recover = capture(&clock);

    // --- Report: build the console from the *current* live clock (correct baud
    //     wherever we ended up), then print everything. -------------------------
    // Promote the clock to `'static` so the UART1 console (which borrows it)
    // can live in `SERIAL`.
    let clock = CLOCK.init(clock);
    let now = clock.freeze();
    let parts = Parts::new(dp.topreg);
    let uart1_pins = Uart1Pins { tx: parts.gp_spi0_cs_x, rx: parts.gp_spi0_sck };
    let uart1 = Uart::new(dp.uart1, uart1_pins, UartConfig::default(), clock).expect("uart1 init failed");
    defmt_serial::defmt_serial(SERIAL.init(uart1));

    defmt::println!(
        "clock_dump: ground truth (console built at com={=u32} Hz after HP->LV->HV)",
        now.com.to_Hz()
    );
    print_snap("HP_boot", &hp);
    print_snap("LP", &lp);
    print_snap("HP_recover", &hp_recover);
    print_log("downshift(LV)", &lp_log, lp_n, lp_tr);
    print_log("upshift(HV)", &hp_log, hp_n, hp_tr);

    // Canonical LV per User Manual Table UART-792: COM 32.5 MHz, SYSPLL 195 MHz.
    defmt::println!(
        "LP vs canonical: com={=u32} (want 32500000, ok={=bool}) | syspll={=u32} (want 195000000, ok={=bool})",
        lp.com, within(lp.com, 32_500_000, 5),
        lp.syspll, within(lp.syspll, 195_000_000, 5),
    );

    defmt::println!("TEST RESULT: PASS");
    loop {
        asm::wfi();
    }
}
