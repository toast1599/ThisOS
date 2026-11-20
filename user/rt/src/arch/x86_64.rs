//! The entry point, the syscall instruction and the cycle counter on
//! x86_64, with ADR 0007's registers: the number in RAX, the arguments in
//! RDI, RSI, RDX, R10, R8 and R9, the error back in RAX and the value in
//! RDX. SYSCALL itself changes RCX and R11, and the kernel nothing else.

use core::arch::asm;
#[cfg(not(test))]
use core::arch::naked_asm;

/// The `e_machine` of x86_64 ELF files.
pub const ELF_MACHINE: u16 = 62;

/// Where the kernel starts the program, with a 16-byte aligned stack, its
/// argument in RDI, and every other register zeroed, RBP included, which
/// ends a backtrace's frame chain here. The call makes the stack aligned as
/// the System V ABI wants it at a function's entry, and leaves RDI as
/// `start`'s argument.
#[cfg(not(test))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    naked_asm!("call {start}", "ud2", start = sym crate::start)
}

/// Ends the calling process with `status`.
pub fn exit(status: u64) -> ! {
    // SAFETY: By ADR 0007, exit reads only its number and argument
    // registers, touches no user memory and never returns.
    unsafe {
        asm!(
            "syscall",
            in("rax") thisos_abi::Syscall::Exit.number(),
            in("rdi") status,
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
    // SAFETY: A syscall changes only RAX, RDX, RCX and R11, and the caller
    // guarantees it touches no memory.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number => code,
            in("rdi") args[0],
            in("rsi") args[1],
            inlateout("rdx") args[2] => value,
            in("r10") args[3],
            in("r8") args[4],
            in("r9") args[5],
            lateout("rcx") _,
            lateout("r11") _,
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
            "syscall",
            inlateout("rax") number => code,
            in("rdi") args[0],
            in("rsi") args[1],
            inlateout("rdx") args[2] => value,
            in("r10") args[3],
            in("r8") args[4],
            in("r9") args[5],
            lateout("rcx") _,
            lateout("r11") _,
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
            "syscall",
            inlateout("rax") number => code,
            in("rdi") args[0],
            in("rsi") args[1],
            inlateout("rdx") args[2] => value,
            in("r10") args[3],
            in("r8") args[4],
            in("r9") args[5],
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    (code, value)
}

/// The CPU's time-stamp counter, which counts at [`cycles_hz`]. Reading it
/// doesn't wait for earlier instructions to finish, which shifts a reading
/// by tens of cycles at most.
pub fn cycles() -> u64 {
    // SAFETY: RDTSC reads a counter into registers and touches no memory.
    // The kernel leaves CR4.TSD clear, so it works in ring 3; were it set,
    // RDTSC would raise #GP, and the kernel would end the process.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// How fast [`cycles`] counts: the crystal frequency in ECX of CPUID leaf
/// 0x15 times the ratio EBX / EAX, or `None` where the leaf doesn't give
/// all three, as under QEMU without KVM.
pub fn cycles_hz() -> Option<u64> {
    if core::arch::x86_64::__cpuid(0).eax < 0x15 {
        return None;
    }
    let leaf = core::arch::x86_64::__cpuid(0x15);
    if leaf.eax == 0 || leaf.ebx == 0 || leaf.ecx == 0 {
        return None;
    }
    Some(u64::from(leaf.ecx) * u64::from(leaf.ebx) / u64::from(leaf.eax))
}
