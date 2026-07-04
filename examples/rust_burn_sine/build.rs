use burn_onnx::{LoadStrategy, ModelGen};
use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    // Copy memory.x to the linker search path so it is found at link time
    // even when building from a workspace or non-root directory.
    let out = &PathBuf::from(env::var_os("OUT_DIR").unwrap());
    File::create(out.join("memory.x"))
        .unwrap()
        .write_all(include_bytes!("memory.x"))
        .unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");

    generate_model();
}

fn generate_model() {
    ModelGen::new()
        .input("src/model/sine.onnx")
        .out_dir("model/")
        .load_strategy(LoadStrategy::Embedded)
        .run_from_script();
}
