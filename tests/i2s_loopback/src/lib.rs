//! Support crate for the `i2s_loopback` on-hardware test.
//!
//! The actual test is the `harness = false` integration test in `tests/i2s.rs`
//! (run with `cargo test --release --test i2s`). This library target exists
//! only to give the package something to build; it intentionally has no code.
#![no_std]
