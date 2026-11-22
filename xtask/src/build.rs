//! Building the kernel and its disk image.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use thisos_report as report;

use crate::cmd::{cargo, output};
use crate::{limine, ramdisk, root};

/// The file in the image's FAT partition that holds the build ID.
pub const BUILD_FILE: &str = "thisos-build.txt";

/// The architecture a kernel is built for.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Arch {
    #[value(name = "x86_64")]
    X86_64,
    Aarch64,
}

impl Arch {
    /// Its name on xtask's command line.
    pub fn name(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        }
    }

    /// The Rust target the kernel builds for.
    pub fn target(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64-unknown-none",
            Arch::Aarch64 => "aarch64-unknown-none-softfloat",
        }
    }

    /// Writes the disk image that boots the kernel ELF at `elf` to `path`,
    /// with the file at `ramdisk` as the loader's ramdisk, and `id` in
    /// [`BUILD_FILE`], when they are `Some`.
    pub fn write_image(
        self,
        elf: &Path,
        ramdisk: Option<&Path>,
        id: Option<&str>,
        path: &Path,
    ) -> Result<()> {
        match self {
            Arch::X86_64 => {
                let mut builder = bootloader::DiskImageBuilder::new(elf.to_path_buf());
                if let Some(ramdisk) = ramdisk {
                    builder.set_ramdisk(ramdisk.to_path_buf());
                }
                if let Some(id) = id {
                    builder.set_file_contents(BUILD_FILE.into(), format!("{id}\n").into_bytes());
                }
                builder
                    .create_uefi_image(path)
                    .with_context(|| format!("writing {}", path.display()))
            }
            Arch::Aarch64 => limine::write_image(elf, ramdisk, id, path),
        }
    }

    /// Builds what the kernel's images carry as the loader's ramdisk and
    /// returns its path: a ustar archive of the programs in
    /// [`ramdisk::PROGRAMS`], built for this architecture, the same for
    /// every image.
    pub fn ramdisk(self, release: bool) -> Result<Option<PathBuf>> {
        let mut cargo_build = cargo();
        cargo_build.arg("build");
        for package in ramdisk::PROGRAMS {
            cargo_build.args(["-p", package]);
        }
        cargo_build.args(["--target", self.target()]);
        if release {
            cargo_build.arg("--release");
        }
        let elfs = executables(&mut cargo_build, &ramdisk::PROGRAMS, false)?;
        let mut files = Vec::new();
        for (package, elf) in ramdisk::PROGRAMS.into_iter().zip(&elfs) {
            let bytes = fs::read(elf).with_context(|| format!("reading {}", elf.display()))?;
            files.push((ramdisk::name(package), bytes));
        }
        let files: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(name, bytes)| (*name, bytes.as_slice()))
            .collect();
        let path = target_dir()?.join(format!("thisos{}-ramdisk.tar", self.suffix()));
        let file =
            fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        ramdisk::write(&files, io::BufWriter::new(file))?;
        Ok(Some(path))
    }

    /// What follows `thisos` in the names of its files in [`target_dir`]:
    /// nothing for x86_64, which came first.
    pub fn suffix(self) -> &'static str {
        match self {
            Arch::X86_64 => "",
            Arch::Aarch64 => "-aarch64",
        }
    }
}

/// What an image boots.
#[derive(Clone, Copy)]
pub enum Kernel {
    /// The kernel itself.
    Main,
    /// The kernel library's test binary, which holds every kernel test.
    Tests,
}

/// A disk image `build` wrote.
pub struct Image {
    pub path: PathBuf,
    /// `ThisOS <version> <commit> <profile> <time>`, as the kernel logs it.
    pub id: String,
}

/// The steps of [`build`], for a run of it alone.
pub fn steps(kernel: Kernel) -> Vec<String> {
    let kernel = match kernel {
        Kernel::Main => "kernel",
        Kernel::Tests => "kernel tests",
    };
    ["version", kernel, "ramdisk programs", "disk image"]
        .map(String::from)
        .into()
}

/// Builds `kernel` and writes its UEFI disk image, in the steps [`steps`]
/// names.
pub fn build(arch: Arch, release: bool, kernel: Kernel) -> Result<Image> {
    let (id, build) = report::step("version", || {
        let pkgid = output(cargo().args(["pkgid", "-p", "thisos-kernel"]))?;
        let version = pkgid
            .trim()
            .rsplit(['@', '#'])
            .next()
            .with_context(|| format!("no version in cargo pkgid output {pkgid:?}"))?;
        let build = build_env(release, kernel)?;
        Ok((format!("ThisOS {version} {build}"), build))
    })?;

    let (elf, path) = match kernel {
        Kernel::Main => report::step("kernel", || {
            let mut cargo_build = cargo();
            cargo_build.env("THISOS_BUILD", &build).args([
                "build",
                "-p",
                "thisos-kernel",
                "--target",
                arch.target(),
            ]);
            if release {
                cargo_build.arg("--release");
            }
            Ok((
                executable(&mut cargo_build, "thisos-kernel", false)?,
                target_dir()?.join(format!("thisos{}.img", arch.suffix())),
            ))
        })?,
        Kernel::Tests => report::step("kernel tests", || {
            Ok((
                test_elf(arch, release, &build)?,
                target_dir()?.join(format!("thisos{}-tests.img", arch.suffix())),
            ))
        })?,
    };

    let ramdisk = report::step("ramdisk programs", || arch.ramdisk(release))?;
    report::step("disk image", || {
        arch.write_image(&elf, ramdisk.as_deref(), Some(&id), &path)
    })?;
    Ok(Image { path, id })
}

/// `THISOS_BUILD` for a kernel build: `<commit> <profile>[ tests] <time>`.
/// The kernel logs "ThisOS <version> " followed by this.
pub fn build_env(release: bool, kernel: Kernel) -> Result<String> {
    let profile = if release { "release" } else { "debug" };
    let commit = output(
        Command::new("git")
            .current_dir(root())
            .args(["describe", "--always", "--dirty"]),
    )?;
    let time = output(Command::new("date").args(["-u", "+%Y-%m-%dT%H:%MZ"]))?;
    let kind = match kernel {
        Kernel::Main => "",
        Kernel::Tests => " tests",
    };
    Ok(format!("{} {profile}{kind} {}", commit.trim(), time.trim()))
}

/// Builds the kernel library's test binary and returns its path.
fn test_elf(arch: Arch, release: bool, build: &str) -> Result<PathBuf> {
    let mut cargo_test = cargo();
    cargo_test
        .env("THISOS_BUILD", build)
        .args(["test", "-p", "thisos-kernel", "--no-run", "--lib"])
        .args(["--target", arch.target()]);
    if release {
        cargo_test.arg("--release");
    }
    executable(&mut cargo_test, "thisos_kernel", true)
}

/// Runs `cargo_build`, a cargo build or test command, and returns the path
/// of the one executable it reports for the target named `name`, a test
/// binary when `test` is true.
fn executable(cargo_build: &mut Command, name: &str, test: bool) -> Result<PathBuf> {
    let [elf] = executables(cargo_build, &[name], test)?
        .try_into()
        .unwrap_or_else(|_| unreachable!("one path per name"));
    Ok(elf)
}

/// Runs `cargo_build` as [`executable`] does, and returns the path of the
/// one executable it reports for each target in `names`, in that order.
/// Cargo names them only in its JSON messages, and where it puts them
/// depends on `CARGO_TARGET_DIR` and cargo's config.
fn executables(cargo_build: &mut Command, names: &[&str], test: bool) -> Result<Vec<PathBuf>> {
    cargo_build.arg("--message-format=json-render-diagnostics");
    let messages = output(cargo_build)?;
    let mut elfs = vec![Vec::new(); names.len()];
    for line in messages.lines() {
        let message: Value =
            serde_json::from_str(line).with_context(|| format!("cargo message {line:?}"))?;
        if message["reason"] == "compiler-artifact"
            && message["profile"]["test"] == test
            && let Some(i) = names
                .iter()
                .position(|&name| message["target"]["name"] == name)
            && let Some(elf) = message["executable"].as_str()
        {
            elfs[i].push(PathBuf::from(elf));
        }
    }
    names
        .iter()
        .zip(elfs)
        .map(|(name, elfs)| match <[PathBuf; 1]>::try_from(elfs) {
            Ok([elf]) => Ok(elf),
            Err(elfs) => bail!("expected one executable for {name}, cargo built {elfs:?}"),
        })
        .collect()
}

/// Cargo's target directory, `<root>/target` unless `CARGO_TARGET_DIR` or a
/// cargo config moves it. xtask keeps its images and caches there too.
pub fn target_dir() -> Result<PathBuf> {
    // cargo metadata takes about 40 ms, and one run asks many times.
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    if let Some(dir) = DIR.get() {
        return Ok(dir.clone());
    }
    let metadata = output(cargo().args(["metadata", "--format-version", "1", "--no-deps"]))?;
    let metadata: Value = serde_json::from_str(&metadata).context("cargo metadata's output")?;
    let dir = metadata["target_directory"]
        .as_str()
        .context("cargo metadata gave no target_directory")?;
    Ok(DIR.get_or_init(|| PathBuf::from(dir)).clone())
}
