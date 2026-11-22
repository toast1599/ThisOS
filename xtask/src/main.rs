mod banned;
mod build;
mod ci;
mod clean;
mod cmd;
mod flash;
mod font;
mod limine;
mod lsblk;
mod qemu;
mod ramdisk;
mod test;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use thisos_report::{self as report, Mode, Name, Settings};

use crate::build::{Arch, Kernel};

/// Build, run and test tasks for ThisOS.
#[derive(Parser)]
#[command(bin_name = "cargo xtask")]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Who the output is for: spinners, bars and colors for someone at a
    /// terminal, or ASCII lines for CI and scripts. Interactive when
    /// standard error is a terminal and CI isn't set.
    #[arg(long, global = true, value_enum)]
    output: Option<Output>,
    /// The characters interactive mode draws with, overriding
    /// THISOS_GLYPHS and the guess from the locale and terminal.
    #[arg(long, global = true, value_enum)]
    glyphs: Option<Glyphs>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Output {
    Interactive,
    Machine,
}

#[derive(Clone, Copy, ValueEnum)]
enum Glyphs {
    Unicode,
    Ascii,
}

#[derive(Subcommand)]
enum Command {
    /// Build the kernel and write target/thisos.img, then print its path and
    /// build ID.
    Build {
        /// The architecture to build for.
        #[arg(long, value_enum, default_value = "x86_64")]
        arch: Arch,
        /// Use the release profile.
        #[arg(long)]
        release: bool,
        #[command(flatten)]
        tests: Tests,
    },
    /// Build, then boot the image in QEMU.
    Run {
        /// The architecture to build for.
        #[arg(long, value_enum, default_value = "x86_64")]
        arch: Arch,
        /// Use the release profile.
        #[arg(long)]
        release: bool,
        /// Start QEMU's gdb stub on TCP port 1234, with the CPU paused.
        #[arg(long)]
        gdb: bool,
    },
    /// Build, then write the image to a USB stick, verify it and power the
    /// stick off.
    Flash {
        /// Use the release profile.
        #[arg(long)]
        release: bool,
        /// The stick to write, when more than one USB disk is plugged in.
        #[arg(long, value_name = "PATH")]
        device: Option<String>,
        #[command(flatten)]
        tests: Tests,
    },
    /// Run the host tests, then the kernel's tests in QEMU.
    Test {
        /// The architecture to build for.
        #[arg(long, value_enum, default_value = "x86_64")]
        arch: Arch,
    },
    /// Run locally everything CI runs.
    Ci,
    /// Empty cargo's target directories, the root workspace's and
    /// xtask's, but for the OVMF and Limine downloads.
    Clean {
        /// Remove the OVMF and Limine downloads too.
        #[arg(long)]
        all: bool,
        /// Give only the total freed, on the summary line.
        #[arg(short, long)]
        summary: bool,
    },
    /// Rasterize the console font into lib/console/src/font.
    Font,
    /// Boot a kernel test ELF in QEMU; cargo's runner for `xtask test`.
    #[command(hide = true)]
    QemuTest {
        #[arg(long, value_enum)]
        arch: Arch,
        #[arg(long)]
        ramdisk: Option<PathBuf>,
        elf: PathBuf,
    },
}

/// Whether an image boots the kernel's tests in place of the kernel.
#[derive(Args)]
struct Tests {
    /// Build the kernel library's test binary, which holds every kernel
    /// test, into target/thisos-tests.img instead.
    #[arg(long)]
    tests: bool,
}

impl Tests {
    fn kernel(&self) -> Kernel {
        if self.tests {
            Kernel::Tests
        } else {
            Kernel::Main
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // The runner cargo starts for each kernel test binary: its output is
    // the output of the step that runs `cargo test`.
    if let Command::QemuTest { arch, ramdisk, elf } = &cli.command {
        return thisos_report::finish(&qemu::test(*arch, ramdisk.as_deref(), elf));
    }
    let mode = cli.output.map(|output| match output {
        Output::Interactive => Mode::Interactive,
        Output::Machine => Mode::Machine,
    });
    let glyphs = cli.glyphs.map(|glyphs| match glyphs {
        Glyphs::Unicode => report::Glyphs::Unicode,
        Glyphs::Ascii => report::Glyphs::Ascii,
    });
    let result = Settings::detect(mode, glyphs)
        .and_then(report::init)
        .and_then(|()| run(cli.command));
    report::finish(&result)
}

fn run(command: Command) -> Result<()> {
    let logs = build::target_dir()?.join("xtask-logs");
    match command {
        Command::Build {
            arch,
            release,
            tests,
        } => {
            report::start(
                &format!("build {}", arch.name()),
                names(build::steps(tests.kernel())),
                &logs,
            )?;
            let image = build::build(arch, release, tests.kernel())?;
            report::suspend(|| println!("{}\n{}", image.path.display(), image.id));
            Ok(())
        }
        Command::Run { arch, release, gdb } => {
            report::start(&format!("run {}", arch.name()), names(qemu::steps()), &logs)?;
            qemu::run(arch, release, gdb)
        }
        Command::Flash {
            release,
            device,
            tests,
        } => {
            report::start("flash", names(flash::STEPS), &logs)?;
            flash::flash(release, tests.kernel(), device.as_deref())
        }
        Command::Test { arch } => {
            let steps = test::steps(arch)?;
            let plan = steps.iter().map(|step| step.name.clone()).collect();
            report::start(&format!("test {}", arch.name()), plan, &logs)?;
            report::all(steps)
        }
        Command::Ci => {
            let steps = ci::steps()?;
            let plan = steps.iter().map(|step| step.name.clone()).collect();
            report::start("ci", plan, &logs)?;
            report::all(steps)
        }
        Command::Clean { all, summary } => {
            let dirs = clean::dirs()?;
            let plan = names(dirs.iter().map(|(name, _)| name.clone()));
            report::start("clean", plan, &logs)?;
            clean::clean(&dirs, all, summary)
        }
        Command::Font => {
            report::start("font", names(font::steps()), &logs)?;
            font::font()
        }
        Command::QemuTest { .. } => unreachable!("main runs qemu-test itself"),
    }
}

/// The plan of a command whose steps aren't grouped.
fn names<S: Into<Name>>(steps: impl IntoIterator<Item = S>) -> Vec<Name> {
    steps.into_iter().map(Into::into).collect()
}

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the repository root")
}
