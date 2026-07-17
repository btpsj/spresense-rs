mod monitor;
mod port;
mod test;

use std::{
    env, fs,
    io::{self, BufRead, BufReader, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use cargo_metadata::{Message, MetadataCommand, TargetKind};
use clap::{ArgGroup, Parser};
use flash_writer::FlashOptions;

const DEFAULT_TARGET: &str = "thumbv7em-none-eabihf";
/// SPK install name stamped into the package header. The bootloader boots the
/// entry named `nuttx`, and we flash with `set bootable` disabled, so the
/// program must be installed under this name to actually run. Override via --name.
const DEFAULT_SAVENAME: &str = "nuttx";

#[derive(Parser, Debug)]
#[command(name = "cargo-spresense-flash", bin_name = "cargo")]
enum CargoCli {
    SpresenseFlash(Cli),
}

#[derive(clap::Args, Debug)]
#[command(
    version,
    about = "Build and flash a Rust binary to a Sony Spresense board",
    group(ArgGroup::new("artifact").multiple(false).args(["bin", "example"]))
)]
struct Cli {
    /// Prebuilt ELF to package and flash. When given, the cargo build is skipped
    /// — this is how the tool is invoked as a `cargo run` runner. Omit it to
    /// build the current crate first.
    #[arg(value_name = "ELF")]
    elf: Option<PathBuf>,

    /// Binary target to build and flash (defaults to the crate's sole bin)
    #[arg(long, value_name = "NAME")]
    bin: Option<String>,

    /// Example target to build and flash
    #[arg(long, value_name = "NAME")]
    example: Option<String>,

    /// Build in release mode
    #[arg(long)]
    release: bool,

    /// Build with the given profile (overrides --release)
    #[arg(long, value_name = "NAME")]
    profile: Option<String>,

    /// Cargo features to activate (comma-separated)
    #[arg(long, value_name = "FEATURES")]
    features: Option<String>,

    /// Target triple
    #[arg(long, value_name = "TRIPLE", default_value = DEFAULT_TARGET)]
    target: String,

    /// Disable default Cargo features
    #[arg(long)]
    no_default_features: bool,

    /// Path to Cargo.toml of the project to build
    #[arg(long, value_name = "PATH")]
    manifest_path: Option<PathBuf>,

    /// Serial port device (auto-detected when omitted)
    #[arg(long, value_name = "DEV", env = "SPRESENSE_PORT")]
    port: Option<String>,

    /// XMODEM-phase baud rate
    #[arg(long, value_name = "RATE", default_value_t = 115200)]
    baud: u32,

    /// mkspk -c core selector
    #[arg(long, value_name = "N", default_value_t = 2)]
    core: u8,

    /// SPK install name stored in the package header [default: nuttx]
    #[arg(long, value_name = "NAME")]
    name: Option<String>,

    /// Skip cargo build and reuse an existing artifact on disk
    #[arg(long)]
    skip_build: bool,

    /// After flashing, open a serial monitor on the board's console output
    /// (decodes defmt frames when the ELF embeds defmt data)
    #[arg(long)]
    monitor: bool,

    /// After flashing, run the firmware as a test: reset the board, decode its
    /// defmt output, and exit 0 (pass) / 1 (fail) / 2 (timeout, no verdict).
    #[arg(long)]
    test: bool,

    /// Seconds to wait for a PASS/FAIL verdict in --test mode before timing out.
    #[arg(long, value_name = "SECS", default_value_t = 30)]
    test_timeout: u64,
}

fn main() -> Result<()> {
    env_logger::init();
    let CargoCli::SpresenseFlash(cli) = CargoCli::parse();
    log::debug!("{cli:?}");

    // Resolve the ELF to flash and the name to stamp into the SPK header.
    let (elf_src, artifact_name) = if let Some(elf) = cli.elf.clone() {
        // Runner mode: cargo already built this ELF, just package + flash it.
        let name = elf
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("rust")
            .to_string();
        (elf, name)
    } else {
        let (kind, name) = resolve_target(&cli)?;
        let elf = if cli.skip_build {
            find_existing_artifact(&cli, &kind, &name)?
        } else {
            build(&cli, &kind, &name)?
        };
        (elf, name)
    };

    // The on-disk SPK keeps the artifact's name for readability, but the install
    // name stamped into the header defaults to `nuttx` (what the bootloader boots).
    let savename = cli.name.as_deref().unwrap_or(DEFAULT_SAVENAME);

    // Staging directory for the generated SPK.
    let stage_dir = stage_dir(&artifact_name, &cli)?;
    fs::create_dir_all(&stage_dir)
        .with_context(|| format!("creating staging dir {}", stage_dir.display()))?;
    let staged_spk = stage_dir.join(format!("{artifact_name}.spk"));

    package(&elf_src, savename, &staged_spk, cli.core)?;

    let port = port::resolve(cli.port.as_deref())?;
    flash(&port, cli.baud, &staged_spk)?;

    if cli.test {
        // Decode the board's defmt stream against this ELF and turn the verdict
        // into a process exit code (0 pass / 1 fail / 2 timeout). Never returns.
        let outcome = test::run(&port, &elf_src, Duration::from_secs(cli.test_timeout))?;
        std::process::exit(outcome.exit_code());
    }

    if cli.monitor {
        monitor::run(&port, &elf_src)?;
    }

    Ok(())
}

/// Decide which target to build: an explicit `--bin`/`--example`, otherwise the
/// crate's sole bin. Prompts on a TTY (or errors) when the choice is ambiguous.
fn resolve_target(cli: &Cli) -> Result<(TargetKind, String)> {
    if let Some(name) = &cli.example {
        return Ok((TargetKind::Example, name.clone()));
    }
    if let Some(name) = &cli.bin {
        return Ok((TargetKind::Bin, name.clone()));
    }

    let mut mcmd = MetadataCommand::new();
    if let Some(mp) = &cli.manifest_path {
        mcmd.manifest_path(mp);
    }
    let meta = mcmd.no_deps().exec().context("cargo metadata failed")?;
    let pkg = meta
        .root_package()
        .context("no root package found — run inside a crate or pass --manifest-path")?;

    let bins: Vec<&str> = pkg
        .targets
        .iter()
        .filter(|t| t.kind.contains(&TargetKind::Bin))
        .map(|t| t.name.as_str())
        .collect();

    match bins.as_slice() {
        [one] => Ok((TargetKind::Bin, one.to_string())),
        [] => bail!("no bin target in {} — pass --bin or --example", pkg.name),
        many => {
            if io::stdin().is_terminal() {
                Ok((TargetKind::Bin, pick(many)?))
            } else {
                bail!(
                    "multiple bin targets ({}) — pass --bin to choose one",
                    many.join(", ")
                )
            }
        }
    }
}

/// Numbered interactive picker over target names, read from stdin.
fn pick(names: &[&str]) -> Result<String> {
    let mut stderr = io::stderr();
    writeln!(stderr, "Multiple bin targets found:")?;
    for (i, n) in names.iter().enumerate() {
        writeln!(stderr, "  [{i}] {n}")?;
    }
    let stdin = io::stdin();
    loop {
        write!(stderr, "Select a target [0-{}]: ", names.len() - 1)?;
        stderr.flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            bail!("no target selected (end of input)");
        }
        match line.trim().parse::<usize>() {
            Ok(i) if i < names.len() => return Ok(names[i].to_string()),
            _ => writeln!(stderr, "Invalid selection, try again.")?,
        }
    }
}

fn stage_dir(name: &str, cli: &Cli) -> Result<PathBuf> {
    // Place under <manifest>/target/spresense-flash/<name>/.
    // Fall back to cwd/target if metadata fails.
    let mut mcmd = MetadataCommand::new();
    if let Some(ref mp) = cli.manifest_path {
        mcmd.manifest_path(mp);
    }
    let target_dir = mcmd
        .no_deps()
        .exec()
        .map(|m| m.target_directory.into_std_path_buf())
        .unwrap_or_else(|_| PathBuf::from("target"));
    Ok(target_dir.join("spresense-flash").join(name))
}

fn build(cli: &Cli, kind: &TargetKind, name: &str) -> Result<PathBuf> {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(&cargo);
    cmd.arg("build")
        .arg("--target")
        .arg(&cli.target)
        .args(["--message-format", "json-render-diagnostics"]);

    if let Some(ref p) = cli.profile {
        cmd.args(["--profile", p]);
    } else if cli.release {
        cmd.arg("--release");
    }

    if let Some(ref features) = cli.features {
        cmd.args(["--features", features]);
    }

    if cli.no_default_features {
        cmd.arg("--no-default-features");
    }

    if let Some(ref mp) = cli.manifest_path {
        cmd.arg("--manifest-path").arg(mp);
    }

    match kind {
        TargetKind::Example => cmd.args(["--example", name]),
        _ => cmd.args(["--bin", name]),
    };

    cmd.stdout(Stdio::piped());

    let mut child = cmd.spawn().context("failed to spawn cargo build")?;
    let stdout = child.stdout.take().unwrap();

    let mut elf_path: Option<PathBuf> = None;
    for message in Message::parse_stream(BufReader::new(stdout)) {
        if let Message::CompilerArtifact(art) = message.context("reading cargo JSON stream")? {
            if art.target.name == name && art.target.kind.contains(kind) {
                if let Some(exe) = art.executable {
                    elf_path = Some(exe.into_std_path_buf());
                }
            }
        }
    }

    let status = child.wait().context("cargo build did not exit cleanly")?;
    if !status.success() {
        bail!("cargo build failed (exit {})", status);
    }

    elf_path.ok_or_else(|| {
        anyhow!("cargo build succeeded but no executable artifact found for '{name}'")
    })
}

fn find_existing_artifact(cli: &Cli, kind: &TargetKind, name: &str) -> Result<PathBuf> {
    let mut mcmd = MetadataCommand::new();
    if let Some(ref mp) = cli.manifest_path {
        mcmd.manifest_path(mp);
    }
    let meta = mcmd.no_deps().exec().context("cargo metadata failed")?;

    let profile_dir = if let Some(ref p) = cli.profile {
        p.as_str()
    } else if cli.release {
        "release"
    } else {
        "debug"
    };

    let base: PathBuf = meta
        .target_directory
        .as_std_path()
        .join(&cli.target)
        .join(profile_dir);
    let path = match kind {
        TargetKind::Example => base.join("examples").join(name),
        _ => base.join(name),
    };

    if !path.exists() {
        bail!(
            "artifact not found (--skip-build): {}\nRun without --skip-build to build first.",
            path.display()
        );
    }
    Ok(path)
}

/// Package an ELF into an SPK image in-process via the `mkspk` library.
fn package(elf: &Path, name: &str, spk: &Path, core: u8) -> Result<()> {
    log::info!(
        "Packaging {} -> {} (core {core})",
        elf.display(),
        spk.display()
    );
    let elf_bytes = fs::read(elf).with_context(|| format!("reading ELF {}", elf.display()))?;
    let spk_bytes = mkspk::pack_spk(&elf_bytes, name, core).map_err(|e| anyhow!("mkspk: {e}"))?;
    fs::write(spk, &spk_bytes).with_context(|| format!("writing SPK {}", spk.display()))?;
    Ok(())
}

/// Flash an SPK to the board in-process via the `flash-writer` library.
fn flash(port: &str, baud: u32, spk: &Path) -> Result<()> {
    log::info!("Flashing {} @ {baud} baud via {port}", spk.display());
    let opts = FlashOptions {
        port,
        packages: &[spk],
        dtr_reset: true,
        xmodem_baud: Some(baud),
        set_bootable: false,
        reboot: true,
    };
    flash_writer::flash(&opts).map_err(|e| anyhow!("flash_writer: {e}"))?;
    log::info!("Done. Board rebooting.");
    Ok(())
}
