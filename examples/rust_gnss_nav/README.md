# External-crate GNSS demo (`nmea` + `sguaba`)

Boots Sony's `gnssfw` on the CXD5602's GPS CPU via the pure-Rust
[`cxd56-hal`](../../cxd56-hal/) GNSS driver — the same bring-up as
[`rust_gnss`](../rust_gnss/) — and integrates the two crates.io no_std GNSS
libraries that survived the crate survey in
[`documentation/gnss-crate-evaluation.md`](../../documentation/gnss-crate-evaluation.md):

1. **`nmea` (parser) — on-target round-trip validation.** Every epoch's
   hand-assembled `$GPGGA`/`$GPRMC` sentence is parsed straight back by the
   `nmea` crate and each decoded field is compared against the source
   `PositionData` within the sentence quantization. A checksum, field-order,
   or hemisphere bug in the generator fails loudly against an independent
   implementation. (Generation itself stays hand-rolled: no no_std NMEA
   *generator* exists on crates.io.)
2. **`sguaba` (coordinates) — local ENU navigation.** The first valid fix
   becomes the origin of a local East/North/Up frame; every later fix is
   converted WGS84 → ECEF → ENU and reported as E/N/U metres plus horizontal
   distance and bearing from the reference point.
3. **Desk fallback.** Indoors the receiver never fixes, so after 15 fixless
   epochs the example runs one pass on two canned Tokyo positions ~8.9 km
   apart — the NMEA round-trip and the ENU pipeline both execute with
   known-good expected numbers, then it keeps waiting for a real fix.

## Requirements

Identical to `rust_gnss`: the stock firmware set (`loader.espk`,
`gnssfw.espk`) must be in SPI flash (`Gnss::boot` fails with `Firmware(-2)`
otherwise); the onboard chip antenna is used, nothing to wire. Sky view is
only needed for the live-fix path — the desk demo runs without it.

## Build, flash, observe

```bash
DEFMT_LOG=info cargo run --release
```

(uses the `cargo-spresense-flash` runner — `cargo install --path
../../tools/cargo-spresense-flash` — which flashes and opens a monitor that
decodes the defmt stream against the ELF.)

The `rust-lld: address ... of section .text is not a multiple of alignment
(8)` link-time warning is expected and benign (f64 literal pool after the
vector table), same as `rust_gnss`.

Expected desk run (indoors, no sky):

```text
INFO  gnssfw version 2.0.1841
INFO  positioning started; expect a first cold-sky fix in 35-120 s
INFO  no fix yet: visible=0 tracking=0
...                                        (15 of these, ~15 s)
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

(The sentences and checksums are exact — the canned data is fixed. defmt
prints the floats at full precision; the values above are truncated with
`...`. The ENU horizontal distance and the haversine cross-check differ by
construction — a tangent-plane chord vs the curved surface distance — and
both sit near 8.9 km at a ~291° bearing; `U` is negative because the far
point drops below the tangent plane over that range.)

On a sky run, each fixed epoch adds a condensed fix line, the two sentences
with their round-trip verdicts, and a `nav:` line whose E/N/U starts at zero
on the reference fix and then tracks movement relative to it:

```text
INFO  fix 3D lat=35.629... lon=139.793... alt=52.3 m vel=0.1 m/s dir=213.0 deg sv used 7/11
INFO  $GPGGA,031542.00,3537.7405,N,13947.6208,E,1,07,1.2,52.3,M,36.1,M,,*5C
INFO  GGA round-trip OK
INFO  $GPRMC,031542.00,A,3537.7405,N,13947.6208,E,0.2,213.0,040726,,,A*4F
INFO  RMC round-trip OK
INFO  nav reference set: lat=35.629... lon=139.793... alt=52.3 m
INFO  nav: E=0.0 m N=0.0 m U=0.0 m | horiz=0.0 m bearing=0.0 deg
```

NOTE: the serial stream is binary defmt frames — use the runner's monitor
(or `socat ... | defmt-print -e <elf>`), not a plain terminal.

## Dependency fine print

- `nmea` and `sguaba` are both `default-features = false` (their defaults
  include `std`); sguaba additionally needs its `libm` feature. `uom` is a
  direct dependency only to name `Angle`/`Length` (sguaba doesn't re-export
  it) and must stay semver-unified with sguaba's `uom = "0.38"`.
- `nmea`'s optional `defmt` feature stays **off**: it pins defmt 0.3 next to
  this repo's defmt 1.1.
- All f64 math is soft-float (the M4F FPU is f32-only) — microseconds per
  1 Hz epoch, and +66.7 KiB of flash text over `rust_gnss` for the two
  crates combined (measured in the evaluation doc).
