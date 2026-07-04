#![no_std]
#![no_main]

use cortex_m::asm;
use cortex_m_rt::entry;
use panic_halt as _;

use spresense_bsp::Board;

// ~0.5 s at 153.6 MHz
const DELAY_CYCLES: u32 = 76_800_000;

#[entry]
fn main() -> ! {
    let mut board = Board::take().unwrap();

    loop {
        board.led0.on();
        asm::delay(DELAY_CYCLES);
        board.led0.off();
        asm::delay(DELAY_CYCLES);
    }
}
