//! Booting images in QEMU.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ovmf_prebuilt::{FileType, Prebuilt, Source};

use crate::build::{self, Arch, Kernel};
use crate::cmd::exec;
use crate::ramdisk;

const OVMF: Source = Source::EDK2_STABLE202605_R1;

/// How long one boot of a test binary may run before QEMU is killed. TCG
/// on a CI runner boots OVMF in well under this.
const TEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The statuses QEMU exits with when the kernel reports success, and
/// success with another panic-expecting test to boot for: through
/// isa-debug-exit, (0x10 << 1) | 1 and (0x12 << 1) | 1, and through
/// semihosting the same numbers, so that one constant covers both.
const TEST_SUCCESS: i32 = 33;
const TEST_SUCCESS_WITH_MORE: i32 = 37;

/// The ramdisk file that tells a test kernel which panic-expecting test to
/// run, by its place among them from 0.
const PANIC_TEST_FILE: &str = "panic-test";

/// Builds, then boots the image in QEMU with a window and serial on the
/// terminal.
pub fn run(arch: Arch, release: bool, gdb: bool) -> Result<()> {
    let image = build::build(arch, release, Kernel::Main)?;
    let vars = build::target_dir()?.join(format!("ovmf-vars{}.fd", arch.suffix()));
    let mut qemu = qemu(arch, &image.path, &vars)?;
    if gdb {
        qemu.args(["-s", "-S"]);
    }
    exec(&mut qemu)
}

/// Boots a kernel test ELF headless, with the file at `ramdisk` as the
/// loader's ramdisk, as cargo's runner, and fails unless the kernel reports
/// success before the timeout. A kernel test runs one panic-expecting test
/// per boot, so while the kernel reports another to run, this boots it
/// again with a copy of the ramdisk that names the next.
pub fn test(arch: Arch, ramdisk: Option<&Path>, elf: &Path) -> Result<()> {
    let mut panic_test = 0;
    while boot_test(arch, ramdisk, elf, panic_test)? {
        panic_test += 1;
        eprintln!(
            "booting {} again for panic test {panic_test}",
            elf.display()
        );
    }
    Ok(())
}

/// Boots a kernel test ELF to run panic-expecting test `panic_test`, and
/// returns whether the kernel reports another to run.
fn boot_test(arch: Arch, ramdisk: Option<&Path>, elf: &Path, panic_test: u32) -> Result<bool> {
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

    let mut qemu = qemu(arch, &image, &with_extension(elf, "vars.fd"))?;
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
    match status.code() {
        Some(TEST_SUCCESS) => Ok(false),
        Some(TEST_SUCCESS_WITH_MORE) => Ok(true),
        _ => bail!(
            "{} failed: QEMU exited with {status}, and only {TEST_SUCCESS} and \
             {TEST_SUCCESS_WITH_MORE} are passes",
            elf.display()
        ),
    }
}

/// A QEMU command that boots `image` under OVMF, with serial on standard
/// output. The firmware writes its variables, so each boot starts from a
/// fresh copy at `vars`.
fn qemu(arch: Arch, image: &Path, vars: &Path) -> Result<Command> {
    let firmware = match arch {
        Arch::X86_64 => ovmf_prebuilt::Arch::X64,
        Arch::Aarch64 => ovmf_prebuilt::Arch::Aarch64,
    };
    let ovmf = Prebuilt::fetch(OVMF, build::target_dir()?.join("ovmf")).context("fetching OVMF")?;
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
                // The kernel needs x2APIC, SMEP and SMAP, which TCG emulates
                // but its default qemu64 model leaves off.
                qemu.args(["-accel", "tcg", "-cpu", "qemu64,+x2apic,+smep,+smap"]);
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
    qemu.args(["-m", "512M", "-smp", "1"])
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
