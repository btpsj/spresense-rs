//! Supplies `memory.x` for configurations that do not link the svd2rust PAC.
//!
//! With the `hal` feature on, cxd56-pac-svd2rust's build script emits
//! memory.x + device.x (its `rt` feature is always on via `hal`); emitting a
//! second copy here would put two memory.x on the linker search path. Only the
//! embassy-pac-only configuration (gpio_embassy) needs this one.

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    if env::var_os("CARGO_FEATURE_HAL").is_none() {
        let out = &PathBuf::from(env::var_os("OUT_DIR").unwrap());
        File::create(out.join("memory.x"))
            .unwrap()
            .write_all(include_bytes!("memory.x"))
            .unwrap();
        println!("cargo:rustc-link-search={}", out.display());
    }
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=build.rs");
}
