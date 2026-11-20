# 0010. Syscalls on aarch64: svc #0, x8 and x0 to x5, no SIMD state

- **Status:** Accepted
- **Date:** 2026-11-20

## Context

[0007](0007-syscall-abi-x86_64.md) set x86_64's syscall convention and left aarch64's for the port of user mode there ([0006](0006-aarch64-behind-x86_64.md)). aarch64 enters EL1 from EL0 through `svc`, whose 16-bit immediate the CPU reports in `ESR_EL1`, and saves nothing on a trap but the return address and PSTATE, so the kernel's trap entry saves every register itself. The kernel is built soft-float, as on x86_64, and saves no FP or SIMD registers when it switches threads. EL0 can also write `TPIDR_EL0` freely, which x86_64's user code can't do with FS or GS.

## Decision

- **Entry.** User code enters with `svc #0`. An `svc` with any other immediate is not a syscall: the kernel ends the thread, as for any other trap from user code, so the immediates stay free for a later use.
- **Number and arguments.** x8 holds the syscall's number, and x0 to x5 hold up to six arguments, in that order, as on Linux.
- **Result.** x0 holds 0 for success or an error code, and x1 holds the value, as RAX and RDX do on x86_64.
- **Kept registers.** Every general register but x0 and x1 has the same value after the syscall, and so have the condition flags and `SP_EL0`.
- **Numbers, codes and user space** are the same as x86_64's, in `thisos-abi`: user space ends at `0x7fff_ffff_f000` on both, although aarch64 has no SYSRET to keep a page from.
- **No SIMD state.** `CPACR_EL1.FPEN` traps FP and SIMD instructions at EL0 and EL1, and the kernel ends a user thread that runs one. User crates build for `aarch64-unknown-none-softfloat`, as the kernel does.
- **Thread pointer.** `TPIDR_EL0` belongs to the thread: a switch saves it and loads the next thread's, 0 for a new one, so one process can't read what another left there.

## Consequences

- `thisos-rt` has one syscall primitive per memory effect for each architecture, and its wrappers are the same on both.
- aarch64 loses the user addresses from `0x7fff_ffff_f000` to `1 << 48`.
- What 0007 says of floating point and `std` holds for aarch64 too: the kernel has to save FP and SIMD state per thread before milestone 7.
- `TPIDR_EL0` is there for thread-local storage once `thisos-rt` has some; x86_64 has no counterpart yet.
