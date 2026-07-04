# On-hardware test harness

Flash a test firmware to the Spresense, capture its `defmt` output over the
UART1 console, and turn the result into a **process exit code** — so tests pass
or fail deterministically (and can be wired into CI later).

There is no debug probe on this board (SWD is disabled; the bootloader is
proprietary), so the result channel is the UART, and `defmt` frames are decoded
on the host. The harness lives in the `cargo-spresense-flash` tool as a `--test`
mode; the firmware tests are the targets of the single `cxd56-hw-tests` crate
rooted here.

## How it works

```
cargo run / cargo test           (from tests/)
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

`cargo xtask test-all` runs every variant below in sequence and prints a
pass/fail summary (`--no-external-loopback` skips the jumper-dependent ones).

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

## Crate layout and feature matrix

All tests live in one standalone package (`cxd56-hw-tests`): plain-`defmt` bins
in `src/bin/` (report `TEST RESULT: PASS/FAIL` from `main`) and `defmt-test`
integration tests in `tests/` (`harness = false`; report `all tests passed!`).

`cxd56-hal` enforces **exactly one** async-time backend per build
(`time-driver-rtc`/`time-driver-timer`/`async-delay-rtc`/`async-delay-timer`),
so one manifest cannot default every target at once. The crate defaults
(`hal` + `time-rtc`) serve the bins and the `time`/`spi`/`i2s` tests; the
others select their backend (or the embassy PAC) explicitly. Each target
declares `required-features`, so a missing selection is a clear cargo error,
not a rustc one.

Run everything from `tests/`:

| Target | Kind | Command | Wiring |
|--------|------|---------|--------|
| `uart_peripheral` | bin | `cargo run --release --bin uart_peripheral` | none |
| ↳ external loopback | | `… --features external-loopback` | JP1 D01↔D00 |
| `clock_perf` | bin | `cargo run --release --bin clock_perf` | none |
| `clock_dump` | bin (diagnostic) | `cargo run --release --bin clock_dump` | none |
| `time` | defmt-test | `cargo test --release --test time` | none |
| ↳ SP804 backing | | `… --no-default-features --features time-timer` | none |
| ↳ low-power | | `… --features low-power` (combines with either backing) | none |
| `gpio` | defmt-test | `cargo test --release --test gpio --no-default-features --features backing-rtc` (or `backing-timer`) | see below |
| `gpio_embassy` | defmt-test | `cargo test --release --test gpio_embassy --no-default-features --features embassy-pac` | JP2-4→1.8V, JP2-5→GND |
| `pwm` | defmt-test | `cargo test --release --test pwm --no-default-features --features backing-rtc` | PWM0↔D22 |
| `spi` | defmt-test | `cargo test --release --test spi` | none |
| ↳ external loopback | | `… --features external-loopback` | JP2-9↔JP2-8 |
| `i2s` | defmt-test | `cargo test --release --test i2s` | none (CXD5247 required) |
| ↳ external loopback | | `… --features external-loopback` | JP2-7↔JP2-6 |

**Never run a bare `cargo test`**: it would also build the lib/bin unit-test
targets, which need libtest (and a panic handler) that don't exist on a no_std
target. Always select a test by name with `--test <t>`.

## The tests

### `uart_peripheral` — UART peripheral (plain `defmt`, bin)

Three sub-tests run in `main`, each logged with `defmt::println!`, ending with
the `TEST RESULT` verdict line.

| # | Sub-test | Wiring | What it checks |
|---|----------|--------|----------------|
| 1 | `console_uart1` | none | UART1 console + `defmt-serial` come up (reaching the host over defmt *is* the assertion) |
| 2 | `uart2_internal_loopback` | none | UART2 in PL011 loopback (`UARTCR.LBE`): write a byte pattern, read it back, assert equal |
| 3 | `uart2_external_loopback` | jumper **JP1 D01↔D00** | same over the real pads; gated behind `--features external-loopback` |

Sub-test 2 exercises UART2, which lives in the IMG power/clock domain (no other
example uses it). If `Uart2::new` can't bring it up, you'll get
`uart2_internal_loopback: FAIL: Uart2::new failed` and an overall FAIL — the
console sub-test still passes, so the failure is reported, not hung.

### `clock_perf` — operating-point round-trip verification (bin)

Verifies `Clock::request_perf` reaches a correct, **in-spec** operating point in
**both** directions via the multi-step SYSIOP FREQLOCK handshake (ack every
`CLK_CHG_START`/`CLK_CHG_END` pair — 3 each way on CXD5602 — and complete on the
trailing `FREQLOCK` reply, not the first `CLK_CHG_END`).

The SP804 timer counts at `cpu_baseclk` (a *perf-dependent* clock). The RTC is a
free-running 32.768 kHz counter on the always-on crystal, **invariant** across
operating points. Counting the timer against the RTC over a fixed real-time
window recovers the *real* `cpu_baseclk`, compared to the HAL's belief at each
point. Because the LP console runs at a different COM than HP, **all measurement
happens with no printing** (captured to RAM); the verdict is printed once over
the restored-HP console.

- `[1] hp_boot`   — measured ≈ believed `cpu_baseclk` at boot (HP).
- `[2] lp`        — same after `request_perf(Lp)` (the downshift took, and the
  readback matches reality at LP).
- `[3] cache`     — after `request_perf(Lp)`, the cached `clock.com` (the `Fixed`
  field the UART driver reads) equals live `freeze().com` (the `resample_dyn`
  refresh).
- `[4] hp_recover`— measured ≈ believed back at HP (the LP→HP round-trip).
- `[5] changed`   — LP `cpu_baseclk` is clearly below HP's (physical proof the
  clock moved, not just the readback).

> Why measure at LP but print at HP? A UART sized for one COM garbles at another,
> and `defmt_serial` returns before bytes leave the FIFO — so printing across a
> perf change corrupts the line and desyncs the decoder. Measuring into RAM and
> reporting once at HP sidesteps both.

`clock_dump` is the companion diagnostic (not a pass/fail test): it snapshots
the raw root-clock-tree registers across an HP→LV→HV excursion and dumps them
with the PM FIFO message log, building the console from the *live* clock so the
baud is correct wherever the excursion ended.

### `time` — embassy time driver (defmt-test)

Validates the HAL's `embassy_time_driver::Driver` against an **independent
oracle** — the always-on 32.768 kHz RTC counter read directly (never via
embassy-time), so it is valid ground truth even when the SP804 is the backing:

- `now_is_monotonic` — `Instant::now()` never goes backwards (exercises the
  SP804 overflow/wrap-fold under `time-timer`) and does advance.
- `elapsed_matches_oracle` — a 100 ms `Timer` elapses ~100 ms by the RTC oracle.
- `concurrent_ordered` — three out-of-order timers awaited together fire in
  deadline order, with total wall time ≈ the longest (not the sum) — multiple
  timers in flight at once, ordered by the software queue.

Backend feature-selected like the HAL's `time-driver-*`: default `time-rtc`
(`tick-hz-32_768`) or `time-timer` (SP804, `tick-hz-1_000_000`). Add
`--features low-power` to run at the LP operating point — every test must pass
identically at HP and LP.

### `gpio` — GPIO levels (defmt-test)

Four kinds of check:

- **Input levels** — a GPIO input tied to 1.8V reads High, one tied to GND reads Low.
- **Output loopback** — an output pin shorted to a floating input pin: driving the
  output High/Low makes the input read High/Low, proving the output driver works.
- **Internal pulls** — an unconnected pin reads High with its internal pull-up
  enabled and Low with its pull-down, proving the `IO_*` pad pull config.
- **EXDEVICE interrupts** — the same loopback pin, configured as a GPIO interrupt:
  level `wait_for_high/low` return when the driven level is already present, and
  the PMU edge latch (`is_pending`) catches a rising/falling/any edge driven on
  the line, which `wait_for_rising_edge/falling_edge/any_edge` then consume.

The `backing-rtc`/`backing-timer` feature selects which peripheral backs the
HAL's `async_delay` (used by the edge-arm settle inside `wait_for_*_edge`); run
both variants to cover both backends.

#### Wiring (do this first)

CXD5602 GPIO is **1.8 V** — wire to the board's 1.8V rail only, never 3.3/5 V.

| Pin | Header | Wire to | Expected |
|-----|--------|---------|----------|
| `gp_emmc_data3` / D21 | JP2 pin 4 | **1.8V** | High |
| `gp_emmc_data2` / D20 | JP2 pin 5 | **GND**  | Low  |
| `gp_uart2_rts` / D28 ↔ `gp_uart2_cts` / D27 | JP1 pin 4 ↔ pin 5 | **short the two pins together** (jumper) | out High→in High, out Low→in Low |
| `gp_sen_irq_in` / D22 | JP1 pin 12 | **leave unconnected** | pull-up→High, pull-down→Low |

`tests/gpio.rs`'s `#[init]` brings up the UART1/`defmt-serial` logger and
configures the pins: `into_floating_input()` enables each input pad's buffer
(`ENZI`) and sets its pull, while `into_output()` drops `DIR` to enable D28's
driver. It then hands the pins to the tests as shared state — D27 as an
`InterruptInput` (EXDEVICE slot 6), which still reads its level via
`is_high`/`is_low`. A wrong reading panics; `panic-probe` emits a `panicked at …`
frame the harness reports as FAIL. Swap the D21/D20 wires (or pull the D27↔D28
jumper) to see it fail.

### `gpio_embassy` — GPIO levels via the embassy HAL (defmt-test)

The **embassy-HAL** variant of the `gpio` test's level checks. Same `defmt-test`
shape and the same D21/D20 wiring (only the first two rows above), but driven
entirely through `embassy-cxd56` — the build depends only on that crate
(chiptool PAC), **not** `cxd56-hal` or `cxd56-pac-svd2rust`. That isolation is
the point of the `embassy-pac` feature: it swaps the optional HAL dependency
out of the graph entirely (and `build.rs` supplies `memory.x`, which normally
comes from the svd2rust PAC's `rt` feature).

Unlike the cxd56-hal version there is no manual `IO_*` pad poke:
`Input::new(.., Pull::None)` enables the input buffer (ENZI) and floats the pad
itself.

> **Note on `COM_HZ`:** the blocking driver re-initialises the console and
> recomputes the baud from `COM_HZ`. If that value doesn't match the board's
> real COM clock, the serial output is garbled and the harness can't decode it
> (timeout). Validate the value with `examples/rust_hello_uart_embassy` (clean
> greeting at 115200) and keep the two in sync.

### `pwm` — PWM0 loopback (defmt-test)

Configures PWM0 and reads the signal back via a GPIO interrupt input
(**D22 / SEN_IRQ_IN**, JP1 pin 12 — jumper it to the PWM0 output pin; both are
1.8 V). Each frequency/duty test measures rising→falling→rising edge timestamps
against the always-on 32.768 kHz RTC counter to recover the actual output
frequency and duty cycle (±8 % / ±6 pp tolerances). Uses `async_delay`, so the
`backing-rtc`/`backing-timer` selection applies as for `gpio`.

### `spi` — SPI5 loopback (defmt-test)

Two test cases: an internal loopback (no wiring, `SSPCR1.LBM = 1`) and an
external pad loopback (feature-gated).

External wiring: **JP2 pin 9 (MOSI / D16) ↔ JP2 pin 8 (MISO / D17)** — 1.8 V pads.

### `i2s` — I2S0 loopback (defmt-test)

Two test cases: a clock-register sanity check (no wiring; requires the CXD5247
audio companion to be present) and a full-duplex sine-tone loopback (feature-gated).

The audio bring-up (CXD5247 power-on, 24.576 MHz MCLK oscillator, I2S0 master
at 48 kHz) happens once in `#[init]`. A watchdog is armed during init so a
missing CXD5247 produces a visible reboot loop rather than a silent hang.

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
- Add a `tests/<name>.rs` (defmt-test style) plus a `[[test]]` block in
  `Cargo.toml` (`harness = false` + the right `required-features`), or a
  `src/bin/<name>.rs` for a plain-`defmt` bin — then add its variants to the
  `test_table()` in `xtask/src/main.rs` so `cargo xtask test-all` covers it.
  The shared `.cargo/config.toml` (runner + `-Tlink.x -Tdefmt.x`) is what makes
  `cargo run`/`cargo test` flash-and-verify.
- If the test needs a HAL async-time backend, route it through the existing
  `time-*`/`backing-*` features — never enable two backends in one build.
- Plain-style firmware ends with `TEST RESULT: PASS`/`FAIL`; `defmt-test` ends
  with `all tests passed!`. The harness understands both.

## Notes

- After completion, a `defmt-test` firmware halts via a semihosting exit that
  HardFaults on bare metal — harmless, because the UART hardware has already
  drained the verdict line by then. Plain-style firmware halts cleanly in `wfi`.
