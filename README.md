# Rust on the Sony Spresense / CXD5602

Experimental efforts to bring bare-metal Rust to the Sony Spresense and CXD5602. Unofficial, and expect breakage across versions/commits.

The CXD5602 is a SoC (System on a chip) developed by Sony Semiconductor Solutions and released in 2019. Among its features are 6-core ARM Cortex-M4F cores, 1.5Mb SRAM, more. See the [technical manual](./documentation/p-28_CXD5602GG_technical_manual.pdf) for more details.

The Sony Spresense is the development board that breaks out the various peripherals and learn how to use the chip.

Primarily the SDK for this board and chip used Apache NuttX real-time operating system. However, this work focuses on developing some of the foundational Rust crates needed to allow for writing embedded Rust programs using libraries from the embedded Rust ecosystem.

## Quickstart

This mini-tutorial expects you to have already flashed the Sony bootloader and signed their EULA agreement. Without this, any Rust programs flashed will likely not work. If you haven't already, follow their instructions on the [spresense development website](https://developer.spresense.sony-semicon.com/development-guides/?page=sdk_set_up&lang=en#_flashing_bootloader).

```bash
git clone https://github.com/bsaintjo/spresense-rs
cd spresense-rs
# Installs the `cargo spresense-flash` subcommand (bundles mkspk + flash-writer).
# For more information about flashing, see the Flashing section.
cargo install --path tools/cargo-spresense-flash
cd examples
# Build, package, and flash in one step. The serial port is auto-detected; pass
# --port /dev/ttyUSB0 (or set SPRESENSE_PORT) to choose it explicitly.
cargo spresense-flash --release --bin rust_blink
```

The examples share one workspace whose `.cargo/config.toml` sets a `runner`,
so any example can be flashed with plain `cargo run` — which also opens a
serial monitor on the board's output:

```bash
cd examples
cargo run --release --bin rust_hello_uart    # build + flash + serial monitor
```

See [`examples/README.md`](examples/README.md) for the full example map (a few
examples additionally need `-p <member>` to select their build configuration).

![Rust Blinking on Sony Spresense](./assets/spresense.gif)

## Dependencies

All crates are tested on Linux.

- `rust` >= 1.91.1 for the tools, >=1.85.1 for the PAC/HAL

## Flashing

The process for flashing programs to the Spresense is slightly more difficult than typical Rust embedded development on ARM-based microcontroller boards. To my knowledge, there is no available CMSIS packs, and the bootloader is proprietary, so developing a workflow using `probe-rs` is not feasible without some reverse engineering work.

Instead, the Spresense SDK provides a small suite of tools that allow for taking any ELF binary, and packages it and flashes it to the Spresense. We can re-use this tooling to take any Rust ELF binary compiled for the target, and flash bare-metal Rust programs to run on the Spresense.

The first program involved in Spresense pipeline is [`mkspk`](https://github.com/SPRESENSE/nuttx/tree/a98b7ce090a0db5aa36c0feced496592735c5c57/tools/cxd56). This is a C program takes a core with the `-c` parameter, and three positional arguments. The path to the ELF binary, the SPK program header(?), and the path to the output which should end with `.spk`.

- _IMPORTANT_: always use `nuttx` as the SPK program header. I should have taken better notes, but from what I gather, this name is hardcoded in how the bootloader unpacks the program, and rejects the program (possibly silently) during the flashing stage.

The second program is [`flash.sh`](https://github.com/sonydevworld/spresense/blob/52e887edb9ad14eeeda266dd60a990620018c061/sdk/tools/flash.sh) which is a bash script that checks a few different things about the file and then based on the host OS, calls the corresponding `flash_writer` program, which is a Python program that is built into a binary using `pyinstaller`.

- _IMPORTANT_: if you just got the board and haven't flashed anything to it, you will need to flash the (secondary?) bootloader. If you haven't, while flashing (at least with the SDK version) you might see an error at the top regarding a new bootloader being required accepting a EULA agreement and flashing should fail. Follow the instructions from the documentation on the [Spresense Dev Website](https://developer.spresense.sony-semicon.com/development-guides/?page=sdk_set_up&lang=en#_flashing_bootloader). All work done in this set of crates is based on the SDK v3.4.3 or later.

The above points are mentioned to understand how the flash process works and illustrate that while straightforward, it can be slighly inconvienient. To that end, I've added two (LLM-translated) versions of [`mkspk`](./tools/mkspk/) and [`flash_writer`](./tools/flash-writer/) to make it simpler to have the tools needed to flash Rust programs to the Spresense. I've used these in my flashing workflow for the Rust examples here, and run integration tests to validate they produce correct output, but if you want to use the Spresense SDK flashing tools, here are the instructions for using them.

### Flashing with `cargo spresense-flash`

`tools/cargo-spresense-flash` wraps the whole pipeline as a Cargo subcommand. It
links `mkspk` and `flash-writer` as libraries (no separate `cargo install` of
those, nothing required on `PATH`), and behaves like `cargo build`/`cargo run`:

```bash
cargo install --path tools/cargo-spresense-flash
cd examples
cargo spresense-flash --release --bin rust_blink          # auto-detects the serial port
cargo spresense-flash --release --bin rust_blink --monitor   # ...then stream the board's UART output
```

- **Target**: like `cargo run`, it builds a crate's sole binary automatically;
  pass `--bin <name>`/`--example <name>` when ambiguous (always, in the
  examples workspace).
- **Serial port**: auto-detected from the attached USB serial adapters. With more
  than one it prompts; override with `--port` or `SPRESENSE_PORT`.
- **Install name**: the SPK header savename defaults to `nuttx` (the name the
  bootloader boots); override with `--name`.
- **`cargo run` runner**: add this to a project's `.cargo/config.toml` (the
  examples workspace ships it) so a plain `cargo run` builds, flashes, and
  (optionally) monitors:

  ```toml
  [target.'cfg(all(target_arch = "arm", target_os = "none"))']
  runner = "cargo-spresense-flash spresense-flash --monitor"
  ```

  `cargo run` builds the ELF and appends its path, so the prebuilt binary is
  flashed directly (no rebuild). Drop `--monitor` for examples without UART output.

The serial monitor is a plain read-only byte stream (press Ctrl-C to exit); it
does not yet decode `defmt` frames, so `defmt`-based examples will show encoded
bytes.

### Flashing using Spresense SDK tools

```bash
git clone --recursive https://github.com/sonydevworld/spresense.git
export SPRESENSE_SDK=$(realpath spresense)
git clone https://github.com/bsaintjo/spresense-rs
cd spresense-rs/examples
cargo build --release --bin rust_blink
"$SPRESENSE_SDK"/nuttx/tools/cxd56/mkspk -c 2 target/thumbv7em-none-eabihf/release/rust_blink nuttx target/rust_blink.spk
"$SPRESENSE_SDK"/sdk/tools/flash.sh -c /dev/ttyUSB0 target/rust_blink.spk
```

The SDK also provides a Docker container and `spresense-env.sh` with [instructions](https://github.com/sonydevworld/spresense/tree/master#using-docker) that can be useful if you have trouble setting up the dependencies.

## License

Most of the code is derived from either technical documentation and the Sony Spresense SDK. The SDK is (mostly) licensed Apache-2.0, and all the work here is licensed as Apache-2.0 or compatible. The SVDs are BSD-3-Clause and likewise the PAC here are licensed similarly. However the SDK `NOTICE` shows a variety of licensing and I make a best effort at making sure code in here follows with those. If there are any issues or missing licensing, please file an issue.
