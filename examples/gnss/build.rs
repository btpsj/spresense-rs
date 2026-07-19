//! Passes `defmt.x` to the linker — both GNSS bins log via defmt. Kept out
//! of the shared .cargo/config.toml rustflags for the same reason as in
//! `hal`: not every workspace bin links defmt.

fn main() {
    for bin in ["rust_gnss", "rust_gnss_nav"] {
        println!("cargo:rustc-link-arg-bin={bin}=-Tdefmt.x");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
