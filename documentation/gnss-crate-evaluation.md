# External no_std GNSS-crate evaluation

2026-07-18 · toolchain `rustc 1.97.0 (2d8144b78 2026-07-07)` · target
`thumbv7em-none-eabihf` (CXD5602 APP, Cortex-M4F)

**Question**: can crates.io no_std GNSS/GPS libraries (`nmea`, `icao-wgs84`,
`gnss-rs`, `nav-types`, plus anything else the ecosystem offers) replace the
hand-rolled GNSS code in this repo, and can they be integrated in an example?

**Answer**: nothing hand-rolled is replaceable — but two crates are genuinely
usable and are now integrated in the `rust_gnss_nav` example
(`examples/gnss/src/bin/rust_gnss_nav.rs`): **`nmea`** (as an
on-target validator of our NMEA generator) and **`sguaba`** (local-frame
coordinate math, a capability the repo did not have). The other three named
crates **do not compile for this target as published**, each for the same
class of packaging bug: a dependency declared with default features that pull
in `std`.

## 1. What is actually hand-rolled here

- `cxd56-hal/src/gnss/` — firmware-ABI mirror structs (`#[repr(C)]`, every
  size/offset pinned by `const` asserts against NuttX
  `cxd56_gnss_positiondata_s`) plus the Far-API/ICC transport. **Categorically
  not replaceable**: no crates.io library speaks Sony's `gnssfw` protocol, and
  the structs are dictated byte-for-byte by the firmware.
- `examples/gnss/src/bin/rust_gnss.rs` — the *only* generic GNSS processing in the
  repo (~70 lines): `$GPGGA`/`$GPRMC` generation via `core::fmt::Write`, XOR
  checksum, degrees→degree/decimal-minute split, m/s→knots factor, and a
  constellation-bit→name match. The firmware already delivers geodetic
  lat/lon/alt in final units, so **no coordinate math existed anywhere**.

## 2. Verdicts

Method: published-manifest analysis (crates.io API reports each dependency's
`default_features` flag — authoritative, and in three cases different from the
crate's GitHub HEAD), then a scratch-crate
`cargo check --target thumbv7em-none-eabihf` probe per candidate exercising
the exact APIs the example needs.

| Crate | Version probed | Probe | Blocking issue |
|---|---|---|---|
| `nmea` (AeroRust) | 0.7.0 (Oct 2024) | **PASS** | — (adopted) |
| `sguaba` (helsing-ai) | 0.10.4 (Jul 2026) | **PASS** | — (adopted) |
| `icao-wgs84` (kenba) | 1.0.2 and 0.9.3 | FAIL | `once_cell` and (via `unit-sphere 1.0`) `nalgebra ^0.34`, both with default features → `std` |
| `gnss-rs` (nav-solutions) | 2.6.0 | FAIL | `hifitime ^4.2` and `lazy_static ^1` (no `spin_no_std`) and `thiserror ^2`, all with default features → `std` |
| `nav-types` | 0.5.2 (Jul 2023) | FAIL | `nalgebra ^0.32` with default features → `std`; fixed on unreleased git master only |

Probe failure signature (all three): the dependency itself fails to build,
e.g.

```
error[E0463]: can't find crate for `std`
error: could not compile `once_cell` (lib) due to 245 previous errors   # icao-wgs84 1.0.2 / 0.9.3
error: could not compile `wide` (lib) due to 1 previous error           # nalgebra-default path (icao-wgs84, nav-types)
error: could not compile `serde_core` (lib) due to 5827 previous errors # gnss-rs via hifitime defaults
error: could not compile `web-time` (lib) due to 3 previous errors      # gnss-rs via hifitime defaults
```

Notes per crate:

- **`nmea` 0.7.0** — no_std **without alloc**; per-sentence feature flags
  (we enable only `GGA`, `RMC`); deps declared correctly (`chrono`, `nom 7`,
  `arrayvec` all no-default-features; `num-traits` with `libm`; `heapless
  0.8` — the same version our examples already use). MSRV 1.70. It is a
  **parser only** — and so is every other NMEA crate surveyed (`nmea0183`
  [core-only, zero deps], `rustedbytes-nmea`, `nmea0183-parser`,
  `nmea-parser` [needs alloc]). **No no_std NMEA generator exists on
  crates.io**, so our generator stays and the parser validates it. Caveat:
  its optional `defmt` feature pins defmt **0.3** — must stay off next to our
  defmt 1.1.
- **`sguaba` 0.10.4** — WGS84/ECEF/NED/ENU coordinates, rigid-body
  transforms, compile-time frame safety (`system!` marker types; the
  observer-transform constructors are `unsafe` because they *claim* the
  marker's origin). Manifest is exemplary: `nalgebra 0.35` and `uom 0.38`
  both no-default-features, `libm` optional and wired through
  `nalgebra/libm`. Actively maintained (2–4 week cadence). It does not
  re-export `uom`, so naming `Angle`/`Length` needs a direct `uom` dep
  (kept semver-unified at `0.38`).
- **`icao-wgs84`** — the geodesic math itself is high quality
  (Karney-derived, validated against GeographicLib test data) and the README
  says "declared no_std", but the published manifests since at least 0.9.3
  cannot build for a real no-std target. Worth an upstream issue; revisit if
  fixed.
- **`gnss-rs`** — "no-std by default" is true only at the cargo-feature
  level, never tested against an embedded target. Its value here was
  marginal anyway (constellation naming — a 9-line match in our example).
- **`nav-types` 0.5.2** — the no_std fix (nalgebra no-default-features +
  `libm` feature) exists only on unreleased master; last release Jul 2023.
  A git-pin was rejected for durability; `sguaba` covers the same niche
  (and itself uses `nav-types` as a test oracle).
- Also surveyed, not adopted: `hifitime` (GPS timescales; no_std with
  default-features off — nothing here needs it: the firmware hands us UTC
  fields directly), `map_3d` (geodetic conversions, std-oriented).

## 3. Integration outcome (the `rust_gnss_nav` example)

- **nmea round-trip**: each epoch's generated `$GPGGA`/`$GPRMC` is parsed
  back by `nmea::parse_str` and every decoded field is compared against the
  source `PositionData` within the sentence quantization (1e-4 arcminute for
  lat/lon; 0.1 steps for the one-decimal fields; exact for
  time/date/status/count — the parser keeps fractional seconds via
  `from_hms_nano_opt`, and the 2-digit-year pivot is sidestepped by comparing
  `year % 100`). An independent on-target validator of our encoder.
- **sguaba ENU**: first valid fix becomes the origin of a local ENU frame;
  every later fix is reported as east/north/up metres + horizontal distance +
  bearing. A canned two-position desk demo (Tokyo pair, ~8.9 km @ ~291°,
  cross-checked against sguaba's haversine) runs once after 15 fixless
  epochs so an indoor run exercises both crates with known-good numbers.
- **Cost** (release, `opt-level = "s"`, fat LTO, `llvm-size`):

  | ELF | text | data | bss |
  |---|---|---|---|
  | `rust_gnss` (baseline) | 35,596 B | 28 B | 5,588 B |
  | `rust_gnss_nav` | 103,880 B | 28 B | 5,588 B |

  +66.7 KiB of flash for the parser + coordinate stack (nom, chrono naive
  types, nalgebra 3-vector/3×3 paths, uom wrappers, libm f64 soft-float
  routines — the M4F FPU is f32-only). Zero RAM delta. At the 1 Hz epoch
  rate the soft-f64 transform cost (dozens of libm calls) is microseconds
  per epoch at 156 MHz.

## 4. Conclusion

- **Keep** the hand-rolled NMEA generator (~70 lines) — there is nothing on
  crates.io that can generate sentences under no_std; it is now
  round-trip-validated on target by an independent parser.
- **Keep** the HAL GNSS driver and wire types — firmware-ABI, not a library
  concern.
- **Use `nmea`** wherever sentence *parsing* is needed (e.g. a future
  external-receiver example) — configure `default-features = false` with
  per-sentence features, never its `defmt` (0.3) feature.
- **Use `sguaba`** when coordinate math is needed —
  `default-features = false, features = ["libm"]`.
- The "declared no_std" claim of an embedded-adjacent crate is not evidence;
  the published manifest and a target-compile probe are. Three of the four
  candidates fail exactly there.

Revisit triggers: `nav-types` cutting a release with its master no_std fix;
`icao-wgs84` fixing `once_cell`/`unit-sphere` feature declarations (its
geodesics are better than great-circle approximations when that matters);
`nmea` gaining defmt 1.x support.

Probe sources: scratch crates + full logs preserved in the session job dir
(`gnss-crate-probes/`; not committed).
