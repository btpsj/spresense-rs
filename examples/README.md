# Spresense Rust examples

One cargo workspace for every example: a single `Cargo.lock`,
`.cargo/config.toml`, and set of dependency versions
(`[workspace.dependencies]` in [`Cargo.toml`](Cargo.toml)). Each example is a
bin target at `<member>/src/bin/<name>.rs` — cargo infers the targets from the
filenames, so adding an example is dropping a file into the right member's
`src/bin/` (plus a dependency line if it needs a new crate).

## Running an example

```bash
cd examples
cargo run --release --bin rust_hello_uart   # build + flash + serial monitor
```

The shared runner (`cargo-spresense-flash spresense-flash --monitor`, see
[`.cargo/config.toml`](.cargo/config.toml)) builds the ELF, packages it,
flashes it over the bootloader serial port, and then streams the board's UART
output. Install it once with
`cargo install --path ../tools/cargo-spresense-flash`. `cargo build` /
`cargo spresense-flash` work the same way — just add `--bin <name>`.

Formerly the PAC blink examples flashed without a monitor; with the shared
runner every `cargo run` now opens one (Ctrl-C to exit).

## Members: why the workspace is split

Cargo unifies dependency features across every package it selects in one
invocation, and three axes here must never meet:

* **cxd56-hal allows exactly one async-time backend** per build
  (`time-driver-rtc|time-driver-timer|async-delay-rtc|async-delay-timer` —
  it `compile_error!`s otherwise).
* **The PACs must not share a package**: the svd2rust PAC's `rt` feature flips
  `cortex-m-rt/device` (and supplies `device.x`) for every co-selected bin,
  which breaks bins linked against the chiptool PAC or embassy-cxd56.
* **critical_section impls collide**: the SPH-based
  `cxd56-hal/critical-section-impl` and cortex-m's
  `critical-section-single-core` are duplicate symbols in one binary.

So examples are grouped into members by link-level configuration:

| member | bins | configuration |
|---|---|---|
| `hal` | rust_adc_read, rust_blink_hal, rust_blink_leds, rust_echo, rust_hello_uart, rust_hello_defmt, rust_i2c_lis2mdl, rust_i2c_scan, rust_i2s0, rust_i2s0_loopback, rust_interrupt, rust_multicore_blink, rust_sd_spi, rust_timer_hal, rust_uart2_gear, rust_uart_split, rust_watchdog_hal, rust_blink_bsp, rust_pwbimu1 | cxd56-hal + spresense-bsp, RTC embassy time driver |
| `embassy-time` | rust_embassy_time | cxd56-hal `time-driver-*` (selectable, see below) |
| `pac-svd2rust` | rust_blink | svd2rust PAC direct, no HAL |
| `rust_burn_sine` | rust_burn_sine | burn (ONNX model codegen at build time) on cxd56-hal |
| `async-delay` | rust_gpio_wait, rust_gpio_wait_lp, rust_sleep | cxd56-hal `async-delay-*` (selectable, see below) |
| `critical-section` | rust_critical_section | SPH-based critical_section impl (no single-core impl) |
| `embassy-pac` | rust_blink_embassy, rust_hello_uart_embassy | embassy-cxd56 (chiptool PAC) |
| `pac-chiptool` | rust_blink_chiptool | chiptool PAC direct, no HAL |

The first four members are mutually compatible and form the workspace's
`default-members`: their 22 bins build and run bare, as above. The last four
conflict with that set by design, so select the member explicitly:

```bash
cargo run --release -p examples-async-delay      --bin rust_gpio_wait
cargo run --release -p examples-critical-section --bin rust_critical_section
cargo run --release -p examples-embassy-pac      --bin rust_blink_embassy
cargo run --release -p examples-pac-chiptool     --bin rust_blink_chiptool
```

A bare `cargo build` builds the default members; `--workspace` can never work
(the constraints above are semantic, not structural).

### Backend variants

`async-delay` and `embassy-time` keep their backend-selection features:

```bash
# rust_gpio_wait / rust_gpio_wait_lp / rust_sleep on the SP804 TIMER backing
cargo build --release -p examples-async-delay --no-default-features --features backing-timer
# rust_embassy_time on the SP804 TIMER embassy driver (tick-hz-1_000_000)
cargo build --release -p examples-embassy-time --no-default-features --features time-timer
# rust_embassy_time: drop to the LP operating point first
cargo build --release -p examples-embassy-time --features low-power
```

### Building everything (what CI runs)

```bash
cargo build --release -p examples-hal -p examples-embassy-time -p examples-pac-svd2rust
cargo build --release -p examples-embassy-pac -p examples-pac-chiptool
cargo build --release -p examples-async-delay
cargo build --release -p examples-async-delay --no-default-features --features backing-timer
cargo build --release -p examples-embassy-time --no-default-features --features time-timer
cargo build --release -p examples-critical-section
cargo build --release -p rust_burn_sine
```

## Serial output

Most examples print on UART1 (the CP2102N USB console) at 115200 baud. The
runner's `--monitor` shows it directly; standalone alternatives:

```bash
picocom -b 115200 --imap lfcrlf --noreset /dev/ttyUSB1
# or
minicom -D /dev/ttyUSB1 -b 115200
```

Without `--noreset`, every `picocom` start resets the board.

## Example notes

### rust_hello_uart

Hello-world on cxd56-hal, and an empirical check that clock + UART bring-up
works. The onboard LED nearest the board's center blinks SOS on startup, then
the example prints `hello from spresense rust, n={n}` with a strobe blink per
line.

### rust_hello_defmt

The same hello-world shape, logging via [`defmt`] over UART1 with
`defmt-serial`. defmt frames are compact binary (interned strings), so an
ASCII terminal shows garbage — decode with `defmt-print`:

```bash
cargo install defmt-print
DEFMT_LOG=debug cargo build --release --bin rust_hello_defmt   # no DEFMT_LOG, no output
socat -u /dev/ttyUSB1,rawer,b115200 STDOUT \
  | defmt-print -e target/thumbv7em-none-eabihf/release/rust_hello_defmt
```

The shared runner's monitor does not decode defmt frames either — use the
`socat | defmt-print` pipeline to read them.

[`defmt`]: https://github.com/knurling-rs/defmt

### rust_echo

Reads up to 256 bytes from UART1 and echoes them back. Type into the serial
console; rapid input is buffered and echoed in one burst.

### rust_uart_split

Builds the UART1 driver, `split()`s it into independent halves, and runs a
byte-at-a-time echo (`uart split echo ready` on reset). The RX half implements
only `embedded_io::Read` and the TX half only `embedded_io::Write`, so the two
directions are owned by separate values — e.g. an interrupt handler takes RX
while the main loop keeps TX. The halves recombine with `Uart::join`, after
which `Uart::free` reclaims the pins and gates the clock.
