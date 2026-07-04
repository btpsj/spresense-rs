//! Test-runner monitor: reset the board, decode its `defmt` output over UART,
//! and turn the result into a process exit code.
//!
//! Unlike [`crate::monitor`] (a raw byte pump), this resets the board so the run
//! is captured from boot, decodes the `defmt` frame stream against the test ELF,
//! prints each decoded line live, and watches for verdict markers:
//!
//!   * PASS — `all tests passed!` (the `defmt-test` completion line) or
//!     `TEST RESULT: PASS` (the plain-`defmt` sentinel used by the bins in `tests/`)
//!   * FAIL — `TEST RESULT: FAIL`, a panic (`panicked at`), or `test failed`
//!     (a `defmt-test` assertion failure)
//!
//! Exit codes: `0` pass, `1` fail, `2` timed out with no verdict.

use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use defmt_decoder::{DecodeError, Table};

/// Console baud rate — the board's UART console runs at 115200 regardless of the
/// XMODEM rate used while flashing (matches [`crate::monitor`]).
const MONITOR_BAUD: u32 = 115_200;

/// Verdict of a test run, mapped to a process exit code by the caller.
pub enum Outcome {
    Pass,
    Fail,
    Timeout,
}

impl Outcome {
    pub fn exit_code(&self) -> i32 {
        match self {
            Outcome::Pass => 0,
            Outcome::Fail => 1,
            Outcome::Timeout => 2,
        }
    }
}

/// Open `port`, reset the board, decode the `defmt` stream produced by the
/// firmware at `elf`, and return the verdict (or time out after `timeout`).
pub fn run(port: &str, elf: &Path, timeout: Duration) -> Result<Outcome> {
    // The interned format strings live in the ELF; without them defmt frames
    // can't be decoded, so a `--test` firmware must log via defmt.
    let elf_bytes =
        std::fs::read(elf).with_context(|| format!("reading ELF {}", elf.display()))?;
    let table = Table::parse(&elf_bytes)
        .map_err(|e| anyhow!("parsing defmt data from {}: {e}", elf.display()))?
        .ok_or_else(|| {
            anyhow!(
                "no defmt data in {} — a --test firmware must log via defmt (see tests/README.md)",
                elf.display()
            )
        })?;

    let mut serial = serialport::new(port, MONITOR_BAUD)
        .timeout(Duration::from_millis(100))
        .open()
        .with_context(|| format!("opening {port} for testing"))?;

    // Reset the board (DTR False→True→False, like flash-writer::serial::pulse_dtr)
    // so we capture the test from boot instead of racing the program that the
    // post-flash reboot already started.
    let _ = serial.write_data_terminal_ready(false);
    let _ = serial.write_data_terminal_ready(true);
    let _ = serial.write_data_terminal_ready(false);

    log::info!(
        "Testing on {port} @ {MONITOR_BAUD} baud (timeout {}s) — press Ctrl-C to abort",
        timeout.as_secs()
    );

    // rzcobs (defmt's default framing) can resync after junk — e.g. the ROM
    // banner printed before our program starts. `raw` cannot, so bail there.
    let recoverable = table.encoding().can_recover();
    let mut stream = table.new_stream_decoder();

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut buf = [0u8; 1024];
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
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
                    let msg = frame.display_message().to_string();
                    // Echo the decoded line so the user sees live progress.
                    match frame.level() {
                        Some(level) => writeln!(out, "[{level:?}] {msg}")?,
                        None => writeln!(out, "{msg}")?,
                    }
                    out.flush()?;

                    if is_pass(&msg) {
                        log::info!("PASS — verdict matched");
                        return Ok(Outcome::Pass);
                    }
                    if is_fail(&msg) {
                        log::error!("FAIL — verdict matched");
                        return Ok(Outcome::Fail);
                    }
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

    log::error!(
        "TIMEOUT — no PASS/FAIL verdict within {}s",
        timeout.as_secs()
    );
    Ok(Outcome::Timeout)
}

/// Success markers: `defmt-test`'s completion line, or the plain-style sentinel.
fn is_pass(msg: &str) -> bool {
    msg.contains("all tests passed!") || msg.contains("TEST RESULT: PASS")
}

/// Failure markers: plain-style sentinel, a panic, or a `defmt-test` failure.
fn is_fail(msg: &str) -> bool {
    msg.contains("TEST RESULT: FAIL") || msg.contains("panicked at") || msg.contains("test failed")
}
