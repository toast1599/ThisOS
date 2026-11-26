//! Booting images in QEMU.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use ovmf_prebuilt::{FileType, Prebuilt, Source};
use thisos_hal::{PANIC_TEST_FILE, TestEnd};
use thisos_report as report;

use crate::build::{self, Arch, Kernel};
use crate::cmd::attached;
use crate::ramdisk;

const OVMF: Source = Source::EDK2_STABLE202605_R1;

/// How long one boot of a test binary may run before QEMU is killed. TCG
/// on a CI runner boots OVMF in well under this.
const TEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The statuses QEMU exits with when the kernel reports success, and
/// success with another panic-expecting test to boot for.
const TEST_SUCCESS: u8 = TestEnd::Passed.status();
const TEST_SUCCESS_WITH_MORE: u8 = TestEnd::PassedWithMore.status();

/// The most boots of one test binary, one per panic-expecting test, far
/// more than the kernel has. A kernel that still asks for another is
/// broken, for instance by not finding [`PANIC_TEST_FILE`], and would
/// otherwise be booted for good.
const MAX_BOOTS: u32 = 64;

/// The steps of [`run`], for a run of it alone.
pub fn steps() -> Vec<String> {
    let mut steps = build::steps(Kernel::Main);
    steps.push("QEMU".into());
    steps
}

/// Builds, then boots the image in QEMU with a window and serial on the
/// terminal, in the steps [`steps`] names.
pub fn run(arch: Arch, release: bool, gdb: bool, cpus: u32) -> Result<()> {
    let image = build::build(arch, release, Kernel::Main)?;
    report::step("QEMU", || {
        let vars = build::target_dir()?.join(format!("ovmf-vars{}.fd", arch.suffix()));
        let mut qemu = qemu(arch, &image.path, &vars, cpus)?;
        if gdb {
            qemu.args(["-s", "-S"]);
        }
        attached(&mut qemu)
    })
}

/// Boots a kernel test ELF headless, with the file at `ramdisk` as the
/// loader's ramdisk, as cargo's runner, and fails unless the kernel reports
/// success before the timeout. A kernel test runs one panic-expecting test
/// per boot, so while the kernel reports another to run, this boots it
/// again with a copy of the ramdisk that names the next.
pub fn test(arch: Arch, ramdisk: Option<&Path>, cpus: u32, elf: &Path) -> Result<()> {
    boot_all(elf, |panic_test| {
        boot_test(arch, ramdisk, cpus, elf, panic_test)
    })
}

/// Calls `boot` for panic-expecting test 0, 1 and so on while it returns
/// that the kernel reports another to run, and fails after [`MAX_BOOTS`].
fn boot_all(elf: &Path, mut boot: impl FnMut(u32) -> Result<bool>) -> Result<()> {
    for panic_test in 0..MAX_BOOTS {
        if panic_test > 0 {
            eprintln!(
                "booting {} again for panic test {panic_test}",
                elf.display()
            );
        }
        if !boot(panic_test)? {
            return Ok(());
        }
    }
    bail!(
        "{} still asked for another panic test after {MAX_BOOTS} boots; \
         does the kernel read the {PANIC_TEST_FILE} file xtask writes?",
        elf.display()
    )
}

/// Boots a kernel test ELF to run panic-expecting test `panic_test`, and
/// returns whether the kernel reports another to run.
fn boot_test(
    arch: Arch,
    ramdisk: Option<&Path>,
    cpus: u32,
    elf: &Path,
    panic_test: u32,
) -> Result<bool> {
    let ramdisk = match (ramdisk, panic_test) {
        (ramdisk, 0) => ramdisk.map(Path::to_path_buf),
        (Some(ramdisk), _) => {
            let archive =
                fs::read(ramdisk).with_context(|| format!("reading {}", ramdisk.display()))?;
            let copy = with_extension(elf, "ramdisk.tar");
            let out =
                fs::File::create(&copy).with_context(|| format!("creating {}", copy.display()))?;
            ramdisk::with_file(
                &archive,
                PANIC_TEST_FILE,
                panic_test.to_string().as_bytes(),
                out,
            )?;
            Some(copy)
        }
        (None, _) => bail!(
            "{} has another panic test to run, but {} images carry no ramdisk to name it in",
            elf.display(),
            arch.name()
        ),
    };
    let image = with_extension(elf, "img");
    arch.write_image(elf, ramdisk.as_deref(), None, &image)?;

    let mut qemu = qemu(arch, &image, &with_extension(elf, "vars.fd"), cpus)?;
    qemu.args(["-display", "none"]);
    match arch {
        Arch::X86_64 => qemu.args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]),
        // target=native lets the guest open host files too, which only our
        // own test kernels get to do.
        Arch::Aarch64 => qemu.args(["-semihosting-config", "enable=on,target=native"]),
    };
    // QEMU would otherwise put the terminal in raw mode, and a kill on
    // timeout leaves it that way.
    qemu.stdin(Stdio::null());
    let mut child = qemu
        .spawn()
        .with_context(|| format!("running {}", qemu.get_program().display()))?;
    let deadline = Instant::now() + TEST_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for QEMU")? {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().context("killing QEMU")?;
            child.wait().context("waiting for QEMU")?;
            bail!(
                "{} timed out after {} s",
                elf.display(),
                TEST_TIMEOUT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(100));
    };
    match status.code().and_then(|code| u8::try_from(code).ok()) {
        Some(TEST_SUCCESS) => Ok(false),
        Some(TEST_SUCCESS_WITH_MORE) => Ok(true),
        _ => bail!(
            "{} failed: QEMU exited with {status}, and only {TEST_SUCCESS} and \
             {TEST_SUCCESS_WITH_MORE} are passes",
            elf.display()
        ),
    }
}

/// A QEMU command that boots `image` under OVMF on `cpus` CPUs, with
/// serial on standard output. The firmware writes its variables, so each
/// boot starts from a fresh copy at `vars`.
fn qemu(arch: Arch, image: &Path, vars: &Path, cpus: u32) -> Result<Command> {
    ensure!(
        cpus == 1 || arch == Arch::X86_64,
        "{} runs on one CPU (ADR 0006)",
        arch.name()
    );
    let firmware = match arch {
        Arch::X86_64 => ovmf_prebuilt::Arch::X64,
        Arch::Aarch64 => ovmf_prebuilt::Arch::Aarch64,
    };
    let ovmf = report::step("OVMF", || {
        Prebuilt::fetch(OVMF, build::target_dir()?.join("ovmf")).context("fetching OVMF")
    })?;
    let code = ovmf.get_file(firmware, FileType::Code);
    fs::copy(ovmf.get_file(firmware, FileType::Vars), vars)
        .with_context(|| format!("copying OVMF vars to {}", vars.display()))?;

    let mut qemu = match arch {
        Arch::X86_64 => {
            let mut qemu = Command::new("qemu-system-x86_64");
            qemu.args(["-machine", "q35"]);
            if kvm_usable() {
                qemu.args(["-accel", "kvm", "-cpu", "host"]);
            } else {
                // The kernel needs x2APIC, SMEP and SMAP, and uses UMIP where
                // the CPU has it, all of which TCG emulates but its default
                // qemu64 model leaves off.
                qemu.args(["-accel", "tcg", "-cpu", "qemu64,+x2apic,+smep,+smap,+umip"]);
            }
            qemu
        }
        Arch::Aarch64 => {
            // The host is x86_64, so aarch64 always runs under TCG. ramfb
            // is a plain framebuffer that edk2 hands on as GOP, with no
            // virtio. Without ACPI tables, edk2 hands Limine the device tree
            // instead, which the kernel finds its devices in.
            let mut qemu = Command::new("qemu-system-aarch64");
            qemu.args(["-machine", "virt,gic-version=3,acpi=off"])
                .args(["-cpu", "cortex-a76", "-device", "ramfb"]);
            qemu
        }
    };
    qemu.args(["-m", "512M", "-smp", &cpus.to_string()])
        .arg("-drive")
        .arg(format!(
            "if=pflash,format=raw,readonly=on,file={}",
            code.display()
        ))
        .arg("-drive")
        .arg(format!("if=pflash,format=raw,file={}", vars.display()))
        .args(["-device", "qemu-xhci,id=xhci"])
        .arg("-drive")
        .arg(format!(
            "if=none,id=stick,format=raw,file={}",
            image.display()
        ))
        .args(["-device", "usb-storage,bus=xhci.0,drive=stick"])
        .args(["-serial", "stdio", "-no-reboot"]);
    Ok(qemu)
}

fn with_extension(path: &Path, extension: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(extension);
    name.into()
}

fn kvm_usable() -> bool {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boots_each_panic_test_in_turn_until_none_is_left() {
        let mut booted = Vec::new();
        let result = boot_all(Path::new("kernel"), |panic_test| {
            booted.push(panic_test);
            Ok(panic_test < 2)
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(booted, [0, 1, 2]);
    }

    // A kernel that never finds the file would otherwise be booted for good.
    #[test]
    fn a_kernel_that_always_asks_for_another_fails_at_the_limit() {
        let mut boots = 0;
        let result = boot_all(Path::new("kernel"), |_| {
            boots += 1;
            Ok(true)
        });
        assert!(result.is_err());
        assert_eq!(boots, MAX_BOOTS);
    }
}
