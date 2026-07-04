//! Passes `defmt.x` to the linker for the one defmt-based bin. This cannot
//! live in the shared .cargo/config.toml rustflags: rust_burn_sine links
//! without defmt, so a workspace-wide -Tdefmt.x would fail there.

fn main() {
    println!("cargo:rustc-link-arg-bin=rust_hello_defmt=-Tdefmt.x");
    println!("cargo:rerun-if-changed=build.rs");
}
