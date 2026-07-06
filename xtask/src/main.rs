//! Repo automation tasks. Run via the `cargo xtask` alias (see .cargo/config.toml).
//!
//! Subcommands:
//!   test-all [NAMES...]   Sequentially flash every on-hardware test (across all
//!                         feature combinations) and print a pass/fail summary.
//!                         Optionally restrict to NAMES (test names below).
//!
//! Flags for `test-all`:
//!   --no-external-loopback   Skip variants that need the external-loopback jumpers
//!                            (uart D01<->D00, spi JP2-9<->JP2-8).
//!
//! Feature matrix covered (all targets of the unified tests/ crate):
//!   clock_perf            (default features)
//!   uart_peripheral       internal, external-loopback
//!   embassy_time          rtc, rtc+low-power, timer, timer+low-power
//!   spi_loopback          internal, external-loopback
//!   gpio_levels           backing-rtc, backing-timer
//!   gpio_levels_embassy   (embassy-pac: chiptool PAC, no cxd56-hal)
//!   multicore             default, low-power (spawn/mailbox/HwMutex; low-power
//!                         adds the RX dispatcher under a live FREQLOCK; no wiring)
//!
//! Every variant flashes via the `cargo-spresense-flash ... --test` runner (see
//! tests/.cargo/config.toml), which exits 0=PASS / 1=FAIL / 2=TIMEOUT. This
//! does NOT abort on first failure; it records each verdict and prints a summary.
//!
//! Full passing assumes the wiring rig is in place: JP2 pin4->1.8V, JP2 pin5->GND,
//! JP1 D28<->D27, uart JP1 D01<->D00, spi JP2-9<->JP2-8, and D22 left unconnected.
//! The board's USB console must be the only Spresense on the bus, or set
//! SPRESENSE_PORT to pick the port.

use std::path::{Path, PathBuf};
use std::process::{exit, Command};

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("test-all") => {
            let rest: Vec<String> = args.collect();
            test_all(&rest);
        }
        Some("help") | Some("--help") | Some("-h") | None => {
            print_help();
        }
        Some(other) => {
            eprintln!("unknown task: {other}\n");
            print_help();
            exit(2);
        }
    }
}

fn print_help() {
    eprintln!(
        "cargo xtask <task>\n\n\
         tasks:\n  \
           test-all [NAMES...]   flash every on-hardware test (all feature combos)\n                        \
           and summarize pass/fail; NAMES restricts by test name\n\n\
         test-all flags:\n  \
           --no-external-loopback   skip variants needing the external-loopback jumpers\n"
    );
}

/// One feature combination of a test crate.
struct Variant {
    /// Short tag distinguishing this combination (empty for featureless tests).
    label: &'static str,
    /// Args passed to `cargo` (e.g. `run --release ...` or `test --test x ...`).
    args: Vec<&'static str>,
    /// Whether this variant requires the external-loopback jumpers.
    ext_loopback: bool,
}

/// One on-hardware test and every feature combination to exercise.
struct Test {
    /// Filter key for the `test-all NAMES...` restriction (kept equal to the
    /// old per-test directory names).
    name: &'static str,
    variants: Vec<Variant>,
}

fn no_feat(label: &'static str, args: Vec<&'static str>) -> Variant {
    Variant {
        label,
        args,
        ext_loopback: false,
    }
}

fn test_table() -> Vec<Test> {
    vec![
        Test {
            name: "clock_perf",
            variants: vec![no_feat("", vec!["run", "--release", "--bin", "clock_perf"])],
        },
        Test {
            name: "gnss_smoke",
            variants: vec![no_feat("", vec!["run", "--release", "--bin", "gnss_smoke"])],
        },
        Test {
            name: "uart_peripheral",
            variants: vec![
                no_feat(
                    "internal",
                    vec!["run", "--release", "--bin", "uart_peripheral"],
                ),
                Variant {
                    label: "external-loopback",
                    args: vec![
                        "run", "--release", "--bin", "uart_peripheral",
                        "--features", "external-loopback",
                    ],
                    ext_loopback: true,
                },
            ],
        },
        Test {
            name: "embassy_time",
            variants: vec![
                // time-rtc and time-timer are mutually exclusive backends; pick the
                // timer one with --no-default-features. low-power is orthogonal.
                no_feat("rtc", vec!["test", "--release", "--test", "time"]),
                no_feat(
                    "rtc,low-power",
                    vec!["test", "--release", "--test", "time", "--features", "low-power"],
                ),
                no_feat(
                    "timer",
                    vec![
                        "test", "--release", "--test", "time",
                        "--no-default-features", "--features", "time-timer",
                    ],
                ),
                no_feat(
                    "timer,low-power",
                    vec![
                        "test", "--release", "--test", "time",
                        "--no-default-features", "--features", "time-timer,low-power",
                    ],
                ),
            ],
        },
        Test {
            name: "spi_loopback",
            variants: vec![
                no_feat("internal", vec!["test", "--release", "--test", "spi"]),
                Variant {
                    label: "external-loopback",
                    args: vec![
                        "test", "--release", "--test", "spi",
                        "--features", "external-loopback",
                    ],
                    ext_loopback: true,
                },
            ],
        },
        Test {
            name: "gpio_levels",
            variants: vec![
                // backing-rtc and backing-timer are mutually exclusive, and both
                // conflict with the default time-rtc backend.
                no_feat(
                    "backing-rtc",
                    vec![
                        "test", "--release", "--test", "gpio",
                        "--no-default-features", "--features", "backing-rtc",
                    ],
                ),
                no_feat(
                    "backing-timer",
                    vec![
                        "test", "--release", "--test", "gpio",
                        "--no-default-features", "--features", "backing-timer",
                    ],
                ),
            ],
        },
        Test {
            name: "gpio_levels_embassy",
            variants: vec![no_feat(
                "",
                vec![
                    "test", "--release", "--test", "gpio_embassy",
                    "--no-default-features", "--features", "embassy-pac",
                ],
            )],
        },
        Test {
            name: "multicore",
            variants: vec![
                no_feat("", vec!["test", "--release", "--test", "multicore"]),
                no_feat(
                    "low-power",
                    vec![
                        "test", "--release", "--test", "multicore",
                        "--features", "low-power",
                    ],
                ),
            ],
        },
    ]
}

fn test_all(opts: &[String]) {
    let mut no_external_loopback = false;
    let mut filter: Vec<String> = Vec::new();
    for opt in opts {
        match opt.as_str() {
            "--no-external-loopback" => no_external_loopback = true,
            s if s.starts_with('-') => {
                eprintln!("unknown flag: {s}");
                exit(2);
            }
            name => filter.push(name.to_string()),
        }
    }

    let tests_root = workspace_root().join("tests");
    let mut summary: Vec<String> = Vec::new();

    for test in test_table() {
        if !filter.is_empty() && !filter.iter().any(|f| f == test.name) {
            continue;
        }
        for variant in &test.variants {
            if no_external_loopback && variant.ext_loopback {
                continue;
            }
            let name = if variant.label.is_empty() {
                test.name.to_string()
            } else {
                format!("{} [{}]", test.name, variant.label)
            };
            let cmd_str = format!("cargo {}", variant.args.join(" "));
            println!("================ {name} : {cmd_str} ================");

            let status = Command::new(env_cargo())
                .args(&variant.args)
                .current_dir(&tests_root)
                .status();

            let verdict = match status {
                Ok(s) if s.success() => "PASS".to_string(),
                Ok(s) => match s.code() {
                    Some(c) => format!("FAIL({c})"),
                    None => "FAIL(sig)".to_string(),
                },
                Err(e) => format!("ERR({e})"),
            };
            summary.push(format!("{verdict:<10} {name}"));
        }
    }

    if summary.is_empty() {
        eprintln!("no tests matched filter: {filter:?}");
        exit(2);
    }

    println!("================ SUMMARY ================");
    for line in &summary {
        println!("{line}");
    }

    if summary.iter().any(|l| !l.starts_with("PASS")) {
        exit(1);
    }
}

/// `cargo` honors CARGO when re-invoking itself.
fn env_cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

/// The xtask crate lives at `<root>/xtask`, so the root is its parent.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask crate must live under the workspace root")
        .to_path_buf()
}
