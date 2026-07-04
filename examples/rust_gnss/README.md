# Onboard GNSS on the Spresense with Rust

Boots Sony's `gnssfw` firmware on the CXD5602's GPS CPU over the pure-Rust
[`cxd56-hal`](../../cxd56-hal/) GNSS driver, tracks GPS + GLONASS at a 1 s
cycle, and reports every positioning epoch over the UART1 defmt console in
three forms:

1. a human-readable fix line (fix mode, lat/lon/alt, speed, course, HDOP)
   plus the UTC timestamp,
2. a per-satellite table (SVID, system, elevation, azimuth, C/N0,
   tracked/used flags),
3. standard NMEA `$GPGGA` / `$GPRMC` sentences with valid checksums,
   assembled into `heapless` strings and emitted as defmt text — the decoded
   lines can be pasted into any NMEA validator or viewer.

## Requirements

- The board must have Sony's standard firmware set in SPI flash (`loader.espk`
  and `gnssfw.espk`, installed once via the stock bootloader-install
  procedure). The driver fails cleanly with `Firmware(-2)` from `Gnss::boot`
  if `gnssfw` is missing.
- Sky view: indoors you will see the epoch reports and the satellite table
  populate as satellites are found, but a first fix under open sky takes
  roughly 35–120 s from cold (the example requests a hot start, which the
  firmware degrades toward cold when it has nothing retained; later runs fix
  much faster thanks to Backup-SRAM data).
- The Spresense main board's onboard chip antenna is used; nothing to wire.

## Build, flash, observe

Build with `DEFMT_LOG` set or no printing will show:

```bash
DEFMT_LOG=info cargo run --release
```

(uses the `cargo-spresense-flash` runner — `cargo install --path
../../tools/cargo-spresense-flash` — which flashes and opens a monitor that
decodes the defmt stream against the ELF.)

A `rust-lld: address ... of section .text is not a multiple of alignment (8)`
warning at link time is expected and benign: `.text` starts right after the
0x21c-byte vector table and merely contains an 8-aligned f64 literal pool;
the linker still places every aligned item at a correct absolute address.

Expected decoded output once satellites are tracked:

```text
INFO  gnssfw version 2.0.1841
INFO  positioning started; expect a first cold-sky fix in 35-120 s
INFO  no fix yet: visible=5 tracking=3
...
INFO  fix 3D lat=35.629... lon=139.793... alt=52.3 m vel=0.1 m/s dir=213.0 deg hdop=1.2 sv used 7/11
INFO  utc 2026-07-04 03:15:42.000123
INFO  sv  sys      el  az   c/n0  flags (11 tracked)
INFO  008 GPS      64  201  42.5  TP
INFO  027 GLONASS  38  045  35.1  TP
...
INFO  $GPGGA,031542.00,3537.7405,N,13947.6208,E,1,07,1.2,52.3,M,36.1,M,,*5C
INFO  $GPRMC,031542.00,A,3537.7405,N,13947.6208,E,0.2,213.0,040726,,,A*4F
```

NOTE: the serial stream is binary defmt frames — use the runner's monitor (or
`socat ... | defmt-print -e <elf>`), not a plain terminal.

If a raw-NMEA UART stream (for gpsd/u-center) is ever wanted, the clean way
is a defmt-free variant of this example that writes the sentences straight to
the UART instead of `info!` — mixing raw bytes into the defmt stream would
corrupt its framing.
