# 0011. AP startup on x86_64: own MADT parse and trampoline, on `bootloader` 0.11

- **Status:** Accepted
- **Date:** 2026-11-26

## Context

The SMP milestone needs the application processors (APs) running before per-CPU state and the scheduler can use them. `bootloader` 0.11 starts no APs and has no SMP support, but it hands over the RSDP's address, maps all physical memory, and leaves the first MiB untouched for a startup trampoline. Limine starts APs itself, so if starting them with `bootloader` failed, x86_64 would move to Limine, as aarch64 already uses it ([0006](0006-aarch64-behind-x86_64.md)). The test laptop, a Lenovo 83DA, lists its 18 CPUs as Local x2APIC entries with APIC IDs from 0 to 66, followed by 30 disabled placeholders with ID 0xffffffff. A USB boot of the 83DA started all 17 of its APs, about 10 ms each.

## Decision

- **Loader.** x86_64 stays on `bootloader` 0.11.
- **Tables.** A forbid-unsafe crate, `lib/acpi`, checks the RSDP, the XSDT and the MADT, each passed in as a byte slice that the arch crate read through the physical-memory mapping, and lists the MADT's Local APIC and Local x2APIC entries. Two entries that may be started with one APIC ID are an error; disabled ones may repeat. Its tests run under Miri on hand-built tables and on the MADTs of QEMU's q35 and the 83DA.
- **Which CPUs.** Entries with Enabled set. Online-capable ones are logged and left off.
- **Trampoline.** The four highest free frames below 640 KiB hold the trampoline and the AP's own page tables, for the kernel's life. Those tables copy the kernel's upper half and identity map the low 2 MiB with one page, so the AP can turn paging on from 32-bit mode with a level 4 table below 4 GiB, jump to the upper half, and only then load the kernel's CR3. The kernel's own tables never gain a lower-half entry. The AP takes the BSP's CR0, CR4, EFER and PAT.
- **Sequence.** One AP at a time, in MADT order: INIT, 10 ms, startup IPI, 200 µs, a second startup IPI only if the AP hasn't checked in, then up to 250 ms. Delays are timed on the TSC. An AP that doesn't check in gets INIT again, so it can't run the trampoline with the next AP's data, and the boot goes on without it, with a warning.
- **Until per-CPU state.** An AP loads the kernel's GDT without a TSS and an IDT whose exception stubs record the vector and halt, enables its APIC in x2APIC mode, stores its APIC ID in its own atomic slot and halts with interrupts off. It takes no lock and logs nothing; the BSP logs for it.

## Consequences

- `Platform::start_cpus` is in the HAL; aarch64 starts nothing and returns 1.
- APs cost about 10 ms each at boot, nearly all of it the INIT wait.
- A trap on a parked AP is reported only while the BSP waits for an AP, and is put down to the AP being started then.
- Per-CPU state (item 4) replaces the park IDT and gives each AP a TSS, and the scheduler (item 6) gives the parked APs work.
