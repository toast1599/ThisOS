//! The entry point, the syscall instruction and the cycle counter on
//! aarch64, with ADR 0010's registers: `svc #0` with the number in x8, the
//! arguments in x0 to x5, the error back in x0 and the value in x1. The
//! kernel changes no other register.

use core::arch::asm;
#[cfg(not(test))]
use core::arch::naked_asm;

/// The `e_machine` of aarch64 ELF files.
pub const ELF_MACHINE: u16 = 183;

/// Where the kernel starts the program, with a 16-byte aligned stack, its
/// argument in x0, and every other register zeroed, x29 and x30 included,
/// which ends a backtrace's frame chain here.
#[cfg(not(test))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    naked_asm!("bl {start}", "udf #0", start = sym crate::start)
}

/// Ends the calling process with `status`.
pub fn exit(status: u64) -> ! {
    // SAFETY: By ADR 0010, exit reads only its number and argument
    // registers, touches no user memory and never returns.
    unsafe {
        asm!(
            "svc #0",
            in("x8") thisos_abi::Syscall::Exit.number(),
            in("x0") status,
            options(noreturn, nostack),
        );
    }
}

/// Makes syscall `number` with `args`, and returns its error code and
/// value.
///
/// # Safety
///
/// The syscall must read, write, map and unmap no memory of this process.
pub unsafe fn syscall_nomem(number: u64, args: [u64; 6]) -> (u64, u64) {
    let (code, value);
    // SAFETY: A syscall changes only x0 and x1, and the caller guarantees
    // it touches no memory.
    unsafe {
        asm!(
            "svc #0",
            in("x8") number,
            inlateout("x0") args[0] => code,
            inlateout("x1") args[1] => value,
            in("x2") args[2],
            in("x3") args[3],
            in("x4") args[4],
            in("x5") args[5],
            options(nostack, nomem),
        );
    }
    (code, value)
}

/// Makes syscall `number` with `args`, as [`syscall_nomem`] does, for a
/// syscall that reads memory.
///
/// # Safety
///
/// The syscall must read only memory the caller holds, and write, map and
/// unmap none.
pub unsafe fn syscall_reads(number: u64, args: [u64; 6]) -> (u64, u64) {
    let (code, value);
    // SAFETY: As in syscall_nomem, with the reads the caller allows.
    unsafe {
        asm!(
            "svc #0",
            in("x8") number,
            inlateout("x0") args[0] => code,
            inlateout("x1") args[1] => value,
            in("x2") args[2],
            in("x3") args[3],
            in("x4") args[4],
            in("x5") args[5],
            options(nostack, readonly),
        );
    }
    (code, value)
}

/// Makes syscall `number` with `args`, as [`syscall_nomem`] does, for a
/// syscall that writes or maps memory.
///
/// # Safety
///
/// The syscall must read and write only memory the caller holds, and map
/// only where nothing is mapped.
pub unsafe fn syscall_writes(number: u64, args: [u64; 6]) -> (u64, u64) {
    let (code, value);
    // SAFETY: As in syscall_nomem, with the accesses the caller allows.
    unsafe {
        asm!(
            "svc #0",
            in("x8") number,
            inlateout("x0") args[0] => code,
            inlateout("x1") args[1] => value,
            in("x2") args[2],
            in("x3") args[3],
            in("x4") args[4],
            in("x5") args[5],
            options(nostack),
        );
    }
    (code, value)
}

/// The virtual counter, which counts at [`cycles_hz`]. Without an `isb`
/// before it, a reading may come before earlier instructions finish.
pub fn cycles() -> u64 {
    let count;
    // SAFETY: Reading CNTVCT_EL0 touches no memory. The kernel sets
    // CNTKCTL_EL1.EL0VCTEN, so EL0 may read it; were it clear, the read
    // would trap, and the kernel would end the process.
    unsafe { asm!("mrs {}, cntvct_el0", out(reg) count, options(nomem, nostack, preserves_flags)) };
    count
}

/// How fast [`cycles`] counts: `CNTFRQ_EL0`, which the firmware sets, or
/// `None` if it left it 0.
pub fn cycles_hz() -> Option<u64> {
    let hz: u64;
    // SAFETY: As in cycles: CNTKCTL_EL1.EL0VCTEN lets EL0 read CNTFRQ_EL0
    // too.
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) hz, options(nomem, nostack, preserves_flags)) };
    (hz != 0).then_some(hz)
}
