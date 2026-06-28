#![no_std]

pub mod led;

#[cfg(feature = "pwbimu")]
pub mod pwbimu;

pub use cxd56_hal::gpio::Level;
pub use led::Led;

use cxd56_hal::gpio::GpioPin;
use cxd56_hal::pac;

/// Top-level board handle. Gives named access to Spresense main board peripherals.
pub struct Board {
    pub led0: Led,
}

impl Board {
    /// Take the PAC peripherals (once) and configure the board.
    ///
    /// Returns `None` if called more than once. The application binary must
    /// supply a `critical-section` implementation (e.g. depend on `cortex-m`
    /// with the `critical-section-single-core` feature).
    pub fn take() -> Option<Self> {
        pac::Peripherals::take().map(|_p| Self::from_parts())
    }

    /// Configure the board from already-taken PAC peripherals.
    ///
    /// Useful when the caller needs to interact with the PAC peripherals
    /// before handing control to the BSP.
    pub fn from_peripherals(_p: pac::Peripherals) -> Self {
        Self::from_parts()
    }

    /// # Safety
    /// Caller must ensure no other code holds the PAC peripherals or has
    /// constructed another `Board`.
    pub unsafe fn steal() -> Self {
        Self::from_parts()
    }

    fn from_parts() -> Self {
        // Safety: we reach here only through take() (singleton) or steal()
        // (caller's responsibility). The TOPREG peripheral address is a
        // compile-time constant; we borrow it for 'static.
        let topreg = unsafe { &*pac::Topreg::PTR };
        let led0 = Led::new(unsafe { GpioPin::new(topreg.gp_i2s1_bck()) });
        Board { led0 }
    }
}
