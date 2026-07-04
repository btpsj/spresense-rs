#![no_std]
#![no_main]

use cortex_m::asm;
use cortex_m_rt::entry;
use panic_halt as _;

use cxd56_pac_chiptool::TOPREG;

// ~0.5 s at 153.6 MHz
const DELAY_CYCLES: u32 = 76_800_000;

#[entry]
fn main() -> ! {
    // Configure GP_I2S1_BCK (LED0) as output, initially low.
    // DIR is active-low: false = drive output.
    TOPREG.GP_I2S1_BCK().write(|w| {
        w.set_DIR(false);
        w.set_OUT(false);
    });

    loop {
        // Read GP_I2C4_BCK input level.
        let pin1_high = TOPREG.GP_I2C4_BCK().read().IN();

        // Mirror input level on LED0.
        TOPREG.GP_I2S1_BCK().modify(|w| w.set_OUT(pin1_high));
        asm::delay(DELAY_CYCLES);

        // Force LED off so there is always a visible blink.
        TOPREG.GP_I2S1_BCK().modify(|w| w.set_OUT(false));
        asm::delay(DELAY_CYCLES);
    }
}
