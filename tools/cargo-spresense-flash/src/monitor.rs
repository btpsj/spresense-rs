//! Serial monitor: stream the board's UART output to stdout.
//!
//! When the flashed ELF embeds defmt data (firmwares linked with `-Tdefmt.x`),
//! the board's output is binary defmt frames, so the monitor decodes them
//! against the ELF — a raw dump would be unreadable. Otherwise it is a plain
//! byte pump. Press Ctrl-C to exit (the default SIGINT handler terminates the
//! process).

use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use defmt_decoder::{DecodeError, Table};

/// Console baud rate. The board runs its UART console at 115200 regardless of
/// the XMODEM transfer rate used while flashing.
const MONITOR_BAUD: u32 = 115_200;

/// Monitor `port` until interrupted: decoded defmt frames when `elf` carries
/// defmt data, raw bytes otherwise.
pub fn run(port: &str, elf: &Path) -> Result<()> {
    let elf_bytes =
        std::fs::read(elf).with_context(|| format!("reading ELF {}", elf.display()))?;
    let table = Table::parse(&elf_bytes)
        .map_err(|e| anyhow!("parsing defmt data from {}: {e}", elf.display()))?;
    match table {
        Some(table) => run_defmt(port, &table),
        None => run_raw(port),
    }
}

/// Decode the board's defmt stream live against `table`. Resets the board
/// first: the post-flash reboot already started the program, so its earliest
/// frames would otherwise be emitted before the port opens and lost.
fn run_defmt(port: &str, table: &Table) -> Result<()> {
    let mut serial = open(port)?;

    // Reset the board (DTR False→True→False, like flash-writer::serial::pulse_dtr).
    let _ = serial.write_data_terminal_ready(false);
    let _ = serial.write_data_terminal_ready(true);
    let _ = serial.write_data_terminal_ready(false);

    log::info!("Monitoring {port} @ {MONITOR_BAUD} baud (decoding defmt) — press Ctrl-C to exit");

    // rzcobs (defmt's default framing) can resync after junk — e.g. the ROM
    // banner printed before our program starts. `raw` cannot, so bail there.
    let recoverable = table.encoding().can_recover();
    let mut stream = table.new_stream_decoder();

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut buf = [0u8; 1024];
    loop {
        let n = match serial.read(&mut buf) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::TimedOut => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e).with_context(|| format!("reading from {port}")),
        };
        stream.received(&buf[..n]);

        loop {
            match stream.decode() {
                Ok(frame) => {
                    match frame.level() {
                        Some(level) => writeln!(out, "[{level:?}] {}", frame.display_message())?,
                        None => writeln!(out, "{}", frame.display_message())?,
                    }
                    out.flush()?;
                }
                Err(DecodeError::UnexpectedEof) => break,
                Err(DecodeError::Malformed) if recoverable => continue,
                Err(DecodeError::Malformed) => {
                    return Err(anyhow!(
                        "malformed defmt frame (unrecoverable encoding) — does the ELF match \
                         the firmware on the board?"
                    ));
                }
            }
        }
    }
}

/// Copy everything the port emits to stdout, unmodified.
fn run_raw(port: &str) -> Result<()> {
    let mut serial = open(port)?;

    log::info!("Monitoring {port} @ {MONITOR_BAUD} baud — press Ctrl-C to exit");

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut buf = [0u8; 1024];
    loop {
        match serial.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                out.write_all(&buf[..n])?;
                out.flush()?;
            }
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e).with_context(|| format!("reading from {port}")),
        }
    }
}

fn open(port: &str) -> Result<Box<dyn serialport::SerialPort>> {
    serialport::new(port, MONITOR_BAUD)
        .timeout(Duration::from_millis(100))
        .open()
        .with_context(|| format!("opening {port} for monitoring"))
}
