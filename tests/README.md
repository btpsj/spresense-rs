# On-hardware test harness

Flash a test firmware to the Spresense, capture its `defmt` output over the
UART1 console, and turn the result into a **process exit code** — so tests pass
or fail deterministically (and can be wired into CI later).

There is no debug probe on this board (SWD is disabled; the bootloader is
proprietary), so the result channel is the UART, and `defmt` frames are decoded
on the host. The harness lives in the `cargo-spresense-flash` tool as a `--test`
mode; the firmware tests are the standalone crates here.

## How it works

```
cargo run / cargo test           (inside a test crate)
  └─ runner = cargo-spresense-flash spresense-flash --test
       ├─ build ELF → mkspk → flash over serial (DTR reset, XMODEM, reboot)
       └─ --test: reopen serial, pulse DTR to reset, decode the defmt stream
                  against the ELF, print each line, and watch for a verdict:
            PASS  ← "all tests passed!"  (defmt-test)  or  "TEST RESULT: PASS"
            FAIL  ← "TEST RESULT: FAIL" | "panicked at" | "test failed"
          exit code: 0 pass · 1 fail · 2 timeout (no verdict within --test-timeout)
```

The harness embeds `defmt-decoder`, so you do **not** need `socat`/`defmt-print`
— decoded log lines stream to your terminal as the test runs.

## Prerequisites

- Install the harness-aware tool (once, and after any change to it):
  ```
  cargo install --path tools/cargo-spresense-flash --force
  ```
- Board connected over USB (the on-board CP2102N is auto-detected; override with
  `--port /dev/ttyUSBx` or `SPRESENSE_PORT`). Close any serial monitor first —
  only one process can own the port.
- The Sony bootloader/EULA must already be flashed (same prerequisite as the
  examples).

## The tests

### 1. `uart_peripheral/` — UART peripheral (plain `defmt`, Option 1)

Three sub-tests, reported with an explicit `TEST RESULT: PASS/FAIL` sentinel. No
wiring needed for the default two.

```
cd tests/uart_peripheral
cargo run --release                              # console smoke + UART2 internal loopback
cargo run --release --features external-loopback # + UART2 external loopback (needs a jumper)
echo $?                                           # 0 pass / 1 fail / 2 timeout
```

The external-loopback sub-test needs a jumper between **JP1 D01 (UART2_TXD)** and
**JP1 D00 (UART2_RXD)**.

### 2. `gpio_levels/` — GPIO levels (`defmt-test`, Option 2)

A `defmt-test` integration test. **Wire two JP2 pins first** (CXD5602 GPIO is
**1.8 V** — use the board 1.8V rail, never 3.3/5 V):

| Pin | Header | Wire to | Expected |
|-----|--------|---------|----------|
| `gp_emmc_data3` / D21 | JP2 pin 4 | **1.8V** | High |
| `gp_emmc_data2` / D20 | JP2 pin 5 | **GND**  | Low  |

```
cd tests/gpio_levels
cargo test --release --test gpio                  # cargo test exit status reflects pass/fail
```

(It's a test, not a `run`, because `defmt-test` only emits its entry point under
`cfg(test)`.)

### 3. `spi_loopback/` — SPI5 loopback (`defmt-test`)

Two test cases: an internal loopback (no wiring, `SSPCR1.LBM = 1`) and an
external pad loopback (feature-gated).

```
cd tests/spi_loopback
cargo test --release --test spi                            # internal LBM only
cargo test --release --test spi --features external-loopback # + MOSI↔MISO jumper
```

External wiring: **JP2 pin 9 (MOSI / D16) ↔ JP2 pin 8 (MISO / D17)** — 1.8 V pads.

### 4. `i2s_loopback/` — I2S0 loopback (`defmt-test`)

Two test cases: a clock-register sanity check (no wiring; requires the CXD5247
audio companion to be present) and a full-duplex sine-tone loopback (feature-gated).

The audio bring-up (CXD5247 power-on, 24.576 MHz MCLK oscillator, I2S0 master
at 48 kHz) happens once in `#[init]`. A watchdog is armed during init so a
missing CXD5247 produces a visible reboot loop rather than a silent hang.

```
cd tests/i2s_loopback
cargo test --release --test i2s                            # clock_sanity only
cargo test --release --test i2s --features external-loopback # + sine loopback
```

External wiring: **JP2 pin 7 (DATA_OUT / D18) ↔ JP2 pin 6 (DATA_IN / D19)** —
adjacent pins, 1.8 V. BCK (D26) and LRCK (D25) are free for a scope.

The loopback check uses autocorrelation rather than bit-exact comparison because
the RX DMA is sourced from the audio block's `SRC1` sample-rate converter, which
filters DC and rings on step edges. Energy + periodicity at the tone period proves
the signal made the round trip regardless of SRC gain or phase distortion.

## Writing more tests

- **Always emit verdicts with `defmt::println!`** (or `defmt-test`), never
  `info!`/`warn!` — defmt drops `info!` at compile time unless `DEFMT_LOG` is
  set, so an `info!` verdict would never reach the host and the harness would
  time out.
- Each test crate is standalone (its own `[workspace]`); `memory.x` comes free
  via the `cxd56-hal` `rt` feature. Copy a `.cargo/config.toml` from either
  crate — the `runner = "... --test"` and `-Tlink.x -Tdefmt.x` flags are what
  make `cargo run`/`cargo test` flash-and-verify.
- Plain-style firmware ends with `TEST RESULT: PASS`/`FAIL`; `defmt-test` ends
  with `all tests passed!`. The harness understands both.

## Notes

- After completion, a `defmt-test` firmware halts via a semihosting exit that
  HardFaults on bare metal — harmless, because the UART hardware has already
  drained the verdict line by then. Plain-style firmware halts cleanly in `wfi`.
- The repo workspace currently lists a `dw-apb-i2c` member that has no directory,
  so root-workspace cargo commands (including re-`cargo install` of the tool)
  fail until that crate is created or the member is removed. The standalone test
  crates here are unaffected.
