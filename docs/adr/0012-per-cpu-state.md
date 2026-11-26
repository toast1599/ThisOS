# 0012. Per-CPU state through GS, and the CPU count read at runtime

- **Status:** Accepted
- **Date:** 2026-11-26

## Context

Until now x86_64 kept one TSS, one set of IST stacks, one GDT, and the SYSCALL stub's scratch for the user stack pointer in a static, which held only while the kernel ran on one CPU. The lock code's CPU index was a constant 0. [0011](0011-ap-startup-x86_64.md) starts the APs, which then need state of their own before the scheduler can give them threads. The SMP milestone's plan writes shared code for any number of CPUs, read at runtime, with aarch64 on one CPU until there is aarch64 hardware ([0006](0006-aarch64-behind-x86_64.md)).

## Decision

- **Block.** Each CPU has a block in a static array of `MAX_CPUS` (64): its index, the SYSCALL scratch, the stack top a SYSCALL starts on, the running thread's stack range for backtraces, a running probe's saved stack pointer, its TSS and its GDT. The boot CPU is index 0; APs take the next index in MADT order as they check in.
- **GS.** In kernel code, GS's base is the running CPU's block, so the CPU index is one GS-relative load. User code's base waits in `IA32_KERNEL_GS_BASE`. The SYSCALL stub swaps the two with SWAPGS on entry and before SYSRET. The IDT stubs swap when the saved CS is ring 3. The stubs for the IST vectors, #DB, NMI, #DF and #MC, read `IA32_GS_BASE` and swap only when it isn't an upper-half address, because these can arrive in ring 0 before the SYSCALL stub's SWAPGS. That test holds because FSGSBASE stays off and no syscall sets a GS base: user code changes its base only by loading a selector, which gives a descriptor's base of 0. The thread switch reloads the GS selector between two SWAPGS, since a selector load clears the active base on Intel.
- **Per CPU.** Each CPU has its own GDT, whose TSS descriptor names its own TSS, and four guarded IST stacks: the boot CPU's moved from boot stacks as before, an AP's from the kernel's stack allocator before its INIT. All CPUs share one IDT.
- **One setup function.** The boot CPU and each AP run the same `init_cpu`: descriptor tables, GS bases, CR0, CR4, EFER and the SYSCALL MSRs, written whole rather than copied. CR0 is PE, MP, ET, NE, WP, EM and PG, with AM clear, so RFLAGS.AC means only SMAP's user access. CR4 is PAE, MCE, SMEP and SMAP, PGE, and UMIP where CPUID reports it. An AP also takes the PAT the boot CPU wrote and puts its APIC in x2APIC mode. The boot CPU still turns global pages on in `paging::take` and writes the PAT in `pat::write_combine`, which run later.
- **Count.** `cpu::count()` is the number `Platform::start_cpus` returns, 1 until then. aarch64 returns index 0 and count 1.
- **HAL.** No calls for IPIs or shootdowns yet; the scheduler and TLB shootdown items add them with their callers.

## Consequences

- A parked AP takes traps on the kernel's IDT and its own IST stacks, so a fault there reaches the trap handler, which panics naming the CPU.
- An NMI, #MC or #DB costs one RDMSR more on entry.
- CPUs without UMIP let user code read the addresses of the GDT and TSS through SGDT and STR; the ring-3 test logs a warning there instead of failing.
- The status cells stay sized for the count at boot, before the APs start; the scheduler item sizes them for every CPU.
