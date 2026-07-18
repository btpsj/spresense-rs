#![no_std]
#![no_main]

extern crate alloc;

use burn::{backend::Flex, tensor::Tensor};
use core::fmt::Write;
use cortex_m::Peripherals;
use cortex_m_rt::entry;
use embedded_alloc::LlffHeap as Heap;
use embedded_hal::delay::DelayNs;
use panic_halt as _;
use rust_burn_sine::sine::Model;

use cxd56_hal::{
    clocks::{Config, RccExt},
    delay::Delay,
    gpio::pins::Parts,
    pac,
    uart::{Uart, Uart1Pins},
};

type Backend = Flex;
type BackendDevice = <Backend as burn::tensor::backend::BackendTypes>::Device;

#[global_allocator]
static HEAP: Heap = Heap::empty();

#[entry]
fn main() -> ! {
    // Heap must be initialised before any burn allocation.
    {
        use core::mem::MaybeUninit;
        // 100 KB matches the upstream Pico example; well within the 1.5 MB RAM.
        const HEAP_SIZE: usize = 100 * 1024;
        static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
        unsafe { HEAP.init(&raw mut HEAP_MEM as usize, HEAP_SIZE) }
    }

    let pac = pac::Peripherals::take().unwrap();
    let core = Peripherals::take().unwrap();

    let crg = pac.crg.constrain(Config::default());
    let clocks = crg.into_hp_clock().expect("lock Hp");

    let mut delay = Delay::new(core.SYST, &clocks);

    let parts = Parts::new(pac.topreg);
    let uart1_pins = Uart1Pins {
        tx: parts.gp_spi0_cs_x,
        rx: parts.gp_spi0_sck,
    };
    let mut uart = Uart::new(pac.uart1, uart1_pins, Default::default(), &clocks).unwrap();

    let device = BackendDevice::default();
    let model: Model<Backend> = Model::default();

    let mut input = 0.0f32;
    loop {
        if input > 2.0 {
            input = 0.0;
        }
        input += 0.05;

        let output = run_model(&model, &device, input);
        match output.into_data().as_slice::<f32>() {
            Ok(slice) => {
                let _ = writeln!(uart, "input: {:.3} - output: {:?}", input, slice);
            }
            Err(_) => core::panic!("output slice error"),
        }

        delay.delay_ms(50);
    }
}

fn run_model(model: &Model<Backend>, device: &BackendDevice, input: f32) -> Tensor<Backend, 2> {
    let input = Tensor::<Backend, 2>::from_floats([[input]], device);
    model.forward(input)
}
