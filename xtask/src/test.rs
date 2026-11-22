//! Running the tests.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use thisos_report::{Name, Step};

use crate::build::{self, Arch, Kernel};
use crate::cmd::{cargo, exec, on_host};
use crate::root;

/// The host tests of every `lib/*` crate, `thisos-abi`, `thisos-rt` and
/// both arch crates, then the kernel's tests for `arch` in QEMU, a step
/// each.
pub fn steps(arch: Arch) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    for manifest in libs()? {
        steps.push(Step::new(
            Name::grouped("host tests", crate_dir(&manifest)),
            move || {
                exec(on_host(
                    cargo().arg("test").arg("--manifest-path").arg(&manifest),
                ))
            },
        ));
    }
    steps.push(qemu_step(arch));
    Ok(steps)
}

/// The kernel's tests for `arch` in QEMU, as a step.
pub fn qemu_step(arch: Arch) -> Step {
    Step::new(Name::grouped("QEMU tests", arch.name()), move || qemu(arch))
}

/// `lib/console` for `lib/console/Cargo.toml`, as step names show a crate.
pub fn crate_dir(manifest: &Path) -> String {
    let dir = manifest.parent().unwrap_or(manifest);
    dir.strip_prefix(root())
        .unwrap_or(dir)
        .display()
        .to_string()
}

/// Runs the kernel's tests for `arch` in QEMU.
fn qemu(arch: Arch) -> Result<()> {
    // Set here rather than in .cargo/config.toml: cargo starts the runner in
    // kernel/, where the xtask alias's relative manifest path doesn't resolve.
    let xtask = std::env::current_exe().context("finding the xtask executable")?;
    let xtask = xtask.to_str().context("the xtask path isn't UTF-8")?;
    ensure!(
        !xtask.contains(char::is_whitespace),
        "cargo splits its runner on whitespace, and the xtask path {xtask:?} has some"
    );
    let runner = format!(
        "CARGO_TARGET_{}_RUNNER",
        arch.target().to_uppercase().replace('-', "_")
    );
    let mut command = format!("{xtask} qemu-test --arch {}", arch.name());
    if let Some(ramdisk) = arch.ramdisk(false)? {
        let ramdisk = ramdisk.to_str().context("the ramdisk path isn't UTF-8")?;
        ensure!(
            !ramdisk.contains(char::is_whitespace),
            "cargo splits its runner on whitespace, and the ramdisk path {ramdisk:?} has some"
        );
        command = format!("{command} --ramdisk {ramdisk}");
    }
    exec(
        cargo()
            .env(runner, command)
            // The same kind as build --tests, so a QEMU run logs `tests` like a
            // laptop test image. It doesn't spare a rebuild when switching
            // between the two: the ID has the time to the minute, and the
            // kernel's option_env! recompiles whenever it changes.
            .env("THISOS_BUILD", build::build_env(false, Kernel::Tests)?)
            .args(["test", "-p", "thisos-kernel", "--target", arch.target()]),
    )
}

/// The manifests of the crates that build and test on the host: the
/// `lib/*` crates, `thisos-abi`, `thisos-rt`, whose tests leave out its
/// entry point, panic handler and syscalls, and the two arch crates, whose
/// tests cover their page tables over a fake CPU.
pub fn libs() -> Result<Vec<PathBuf>> {
    let dir = root().join("lib");
    let mut manifests = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let manifest = entry
            .with_context(|| format!("reading {}", dir.display()))?
            .path()
            .join("Cargo.toml");
        if manifest.is_file() {
            manifests.push(manifest);
        }
    }
    manifests.sort();
    manifests.push(root().join("abi/Cargo.toml"));
    manifests.push(root().join("user/rt/Cargo.toml"));
    manifests.push(root().join(ARCH_X86_64));
    manifests.push(root().join(ARCH_AARCH64));
    Ok(manifests)
}

/// The x86_64 arch crate's manifest, relative to the root.
pub const ARCH_X86_64: &str = "arch/x86_64/Cargo.toml";

/// The aarch64 arch crate's manifest, relative to the root.
pub const ARCH_AARCH64: &str = "arch/aarch64/Cargo.toml";
