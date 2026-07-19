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
* **rust_burn_sine flips shared deps to alloc**: burn's store stack enables
  `memchr/std` (and so alloc) on the same memchr that `nmea` → `nom` links
  into the GNSS bins — co-selecting them demands a global allocator the
  allocator-less GNSS bins cannot provide.

So examples are grouped into members by link-level configuration:

| member | bins | configuration |
|---|---|---|
| `hal` | rust_adc_read, rust_blink_hal, rust_blink_leds, rust_echo, rust_hello_uart, rust_hello_defmt, rust_i2c_lis2mdl, rust_i2c_scan, rust_i2s0, rust_i2s0_loopback, rust_interrupt, rust_multicore_blink, rust_sd_spi, rust_timer_hal, rust_uart2_gear, rust_uart_split, rust_watchdog_hal, rust_blink_bsp, rust_pwbimu1 | cxd56-hal + spresense-bsp, RTC embassy time driver |
| `embassy-time` | rust_embassy_time | cxd56-hal `time-driver-*` (selectable, see below) |
| `pac-svd2rust` | rust_blink | svd2rust PAC direct, no HAL |
| `rust_burn_sine` | rust_burn_sine | burn (ONNX model codegen at build time) on cxd56-hal |
| `gnss` | rust_gnss, rust_gnss_nav | cxd56-hal + the crates.io nmea/sguaba stack (must not meet rust_burn_sine) |
| `async-delay` | rust_gpio_wait, rust_gpio_wait_lp, rust_sleep | cxd56-hal `async-delay-*` (selectable, see below) |
| `critical-section` | rust_critical_section | SPH-based critical_section impl (no single-core impl) |
| `embassy-pac` | rust_blink_embassy, rust_hello_uart_embassy | embassy-cxd56 (chiptool PAC) |
| `pac-chiptool` | rust_blink_chiptool | chiptool PAC direct, no HAL |

The first four members are mutually compatible and form the workspace's
`default-members`: their 22 bins build and run bare, as above. The remaining
five conflict with that set by design, so select the member explicitly:

```bash
cargo run --release -p examples-gnss             --bin rust_gnss
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
cargo build --release -p examples-hal -p examples-embassy-time -p examples-pac-svd2rust -p examples-gnss
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

The shared runner's monitor parses the flashed ELF and, when it embeds defmt
data, DTR-resets the board and live-decodes the stream itself — so a plain
`cargo run` shows readable output. The `socat | defmt-print` pipeline above
is the runner-less alternative.

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

### rust_gnss

Boots Sony's `gnssfw` firmware on the CXD5602's GPS CPU over the cxd56-hal
GNSS driver, tracks GPS + GLONASS at a 1 s cycle, and reports every
positioning epoch on the UART1 defmt console: a human-readable fix line with
the UTC timestamp, a per-satellite table (SVID, system, elevation, azimuth,
C/N0, tracked/used flags), and `$GPGGA`/`$GPRMC` sentences with valid
checksums — the decoded lines paste into any NMEA viewer.

Requirements: Sony's standard firmware set in SPI flash (`loader.espk`,
`gnssfw.espk` — `Gnss::boot` fails cleanly with `Firmware(-2)` without it);
the onboard chip antenna is used, nothing to wire. Indoors the epoch reports
and satellite table still run, but a first fix needs sky view (roughly
35–120 s from cold; later runs fix faster via Backup-SRAM). Build with
`DEFMT_LOG=info` or nothing prints:

```bash
DEFMT_LOG=info cargo run --release -p examples-gnss --bin rust_gnss
```

The `rust-lld: address ... of section .text is not a multiple of alignment
(8)` link warning is expected and benign — `.text` starts right after the
0x21c-byte vector table and merely contains an 8-aligned f64 literal pool.

### rust_gnss_nav

The same GNSS bring-up plus the two crates.io no_std GNSS libraries that
survived the survey in
[`documentation/gnss-crate-evaluation.md`](../documentation/gnss-crate-evaluation.md):

* **`nmea` (parser) — on-target round-trip validation.** Every epoch's
  hand-assembled `$GPGGA`/`$GPRMC` sentence is parsed straight back by the
  `nmea` crate and each decoded field is compared against the source fix
  within the sentence quantization — an independent check of the hand-rolled
  generator (which stays: no no_std NMEA *generator* exists on crates.io).
* **`sguaba` (coordinates) — local ENU navigation.** The first valid fix
  becomes the origin of a local East/North/Up frame; every later fix is
  reported as E/N/U metres plus horizontal distance and bearing from that
  reference.
* **Desk fallback.** Indoors, after 15 fixless epochs, one pass over two
  canned Tokyo positions ~8.9 km apart exercises both crates with exact
  expected numbers, then keeps waiting for a real fix — the whole dependency
  stack validates with zero sky view:

```text
INFO  === no fix after 15 epochs - desk demo on canned positions ===
INFO  $GPGGA,123456.00,3537.7820,N,13947.6400,E,1,08,0.9,42.0,M,36.1,M,,*5D
INFO  GGA round-trip OK
INFO  $GPRMC,123456.00,A,3537.7820,N,13947.6400,E,2.3,291.0,040726,,,A*50
INFO  RMC round-trip OK
INFO  $GPGGA,123456.00,3539.4800,N,13942.0960,E,1,08,0.9,40.0,M,36.1,M,,*58
INFO  GGA round-trip OK
INFO  $GPRMC,123456.00,A,3539.4800,N,13942.0960,E,2.3,291.0,040726,,,A*57
INFO  RMC round-trip OK
INFO  nav: E=-8367.0... m N=3143.9... m U=-8.3... m | horiz=8938.2... m bearing=290.6... deg
INFO  cross-check: haversine surface distance 8922.9... m
INFO  === desk demo done; still waiting for a real fix ===
```

(The sentences and checksums are exact — the canned data is fixed; floats
print at full precision, truncated here with `...`. The ENU horizontal
distance is a tangent-plane chord and the haversine a curved surface
distance, so they differ by construction; `U` is negative because the far
point drops below the tangent plane.) On a sky run each fixed epoch adds a
condensed fix line, both sentences with round-trip verdicts, and a `nav:`
line that starts at zero on the reference fix and then tracks movement.

Run with `DEFMT_LOG=info cargo run --release -p examples-gnss --bin
rust_gnss_nav`; same firmware/antenna requirements and benign link warning
as `rust_gnss`. All f64 math is soft-float (the M4F FPU is f32-only) —
microseconds per 1 Hz epoch, and +66.7 KiB of flash text over `rust_gnss`
for the two crates combined (measured in the evaluation doc).
