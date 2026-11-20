//! The raw `asm!` accesses to system registers, TLBs and caches, one
//! function per register or operation. Accesses through `aarch64-cpu` stay
//! where they are used.

use core::arch::asm;

/// Points `TTBR0_EL1` at the level 0 table at `table`, with ASID 0.
///
/// # Safety
///
/// While TTBR0's walks are on, the table must map nothing but frames it
/// holds, so that the lower half reaches no memory the kernel uses
/// otherwise.
pub(crate) unsafe fn write_ttbr0(table: u64) {
    // SAFETY: The caller guarantees what the table maps.
    unsafe { asm!("msr ttbr0_el1, {}", in(reg) table, options(nostack, preserves_flags)) };
}

/// Points `TTBR1_EL1` at the level 0 table at `table`.
///
/// # Safety
///
/// The table must map everything the kernel reaches, as the kernel expects
/// to reach it, executable where the kernel executes.
pub(crate) unsafe fn write_ttbr1(table: u64) {
    // SAFETY: The caller guarantees what the table maps.
    unsafe { asm!("msr ttbr1_el1, {}", in(reg) table, options(nostack, preserves_flags)) };
}

/// Writes `value` to `TCR_EL1`.
///
/// # Safety
///
/// Every field but EPD0 must keep its value. Turning TTBR0's walks on,
/// with EPD0 clear, needs TTBR0 to hold a table that `write_ttbr0`'s
/// contract covers.
pub(crate) unsafe fn write_tcr(value: u64) {
    // SAFETY: Only EPD0 changes, as the caller allows.
    unsafe { asm!("msr tcr_el1, {}", in(reg) value, options(nostack, preserves_flags)) };
}

/// Writes `value` to `MAIR_EL1`.
///
/// # Safety
///
/// Every attribute index a live mapping uses must keep its memory type, or
/// its TLB entries must be dropped before anything reaches it again.
pub(crate) unsafe fn write_mair(value: u64) {
    // SAFETY: The caller guarantees no live mapping changes type.
    unsafe { asm!("msr mair_el1, {}", in(reg) value, options(nostack, preserves_flags)) };
}

/// Sets PSTATE.PAN, and makes every exception taken to EL1 set it too.
///
/// # Safety
///
/// Nothing at EL1 may be using EL0 pages without clearing PAN itself.
pub(crate) unsafe fn enable_pan() {
    // SAFETY: Clearing SCTLR_EL1.SPAN (bit 23) only changes what an
    // exception does to PSTATE.PAN, and setting PAN only stops EL1 reaching
    // EL0 pages, which the caller guarantees nothing does.
    unsafe {
        asm!(
            "mrs {tmp}, sctlr_el1",
            "bic {tmp}, {tmp}, #(1 << 23)",
            "msr sctlr_el1, {tmp}",
            ".arch_extension pan",
            "msr pan, #1",
            "isb",
            tmp = out(reg) _,
            options(nostack, preserves_flags),
        );
    }
}

/// PSTATE.PAN as `mrs` reads it, in bit 22.
pub(crate) fn read_pan() -> u64 {
    let pan: u64;
    // SAFETY: Reading PSTATE.PAN into a register reads no memory and
    // changes no other state.
    unsafe {
        asm!(
            ".arch_extension pan",
            "mrs {}, pan",
            out(reg) pan,
            options(nomem, nostack, preserves_flags),
        );
    }
    pan
}

/// `CTR_EL0`, the cache type register.
pub(crate) fn read_ctr() -> u64 {
    let ctr: u64;
    // SAFETY: Reading CTR_EL0 into a register changes nothing.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags)) };
    ctr
}

/// Writes back and drops the data cache line that holds `addr`, with
/// `dc civac`, and doesn't wait for it.
///
/// # Safety
///
/// `addr` must be mapped.
pub(crate) unsafe fn dc_civac(addr: u64) {
    // SAFETY: Cleaning and invalidating a line writes back what the cache
    // holds of an address the caller guarantees is mapped, and changes no
    // memory's value.
    unsafe { asm!("dc civac, {}", in(reg) addr, options(nostack, preserves_flags)) };
}

/// Makes instruction fetches on every CPU see the stores to `addr`'s data
/// cache line: cleans it to the point of unification and invalidates the
/// instruction caches, waiting for both.
pub(crate) fn sync_icache(addr: u64) {
    // SAFETY: Cleaning the data cache line to the point of unification and
    // invalidating the instruction cache change no memory, only which copy
    // of it an instruction fetch sees.
    unsafe {
        asm!(
            "dc cvau, {}",
            "dsb ish",
            "ic ialluis",
            "dsb ish",
            "isb",
            in(reg) addr,
            options(nostack, preserves_flags),
        );
    }
}

/// Runs `tlbi <op>` on every CPU once the table stores before it are done,
/// and waits until the entries are gone. With a page address, the
/// instruction gets the page's number.
macro_rules! tlbi {
    ($op:literal) => {
        // SAFETY: TLB maintenance only makes the CPUs walk the tables
        // again; it reads and writes no memory the kernel uses.
        unsafe {
            asm!(
                "dsb ishst",
                concat!("tlbi ", $op),
                "dsb ish",
                "isb",
                options(nostack, preserves_flags),
            )
        }
    };
    ($op:literal, $page:expr) => {
        // SAFETY: As above.
        unsafe {
            asm!(
                "dsb ishst",
                concat!("tlbi ", $op, ", {}"),
                "dsb ish",
                "isb",
                in(reg) ($page >> 12) & ((1 << 44) - 1),
                options(nostack, preserves_flags),
            )
        }
    };
}

/// Drops every CPU's entries for `page` whatever their ASID, global ones
/// included, with `tlbi vaae1is`.
pub(crate) fn tlbi_vaae1is(page: u64) {
    tlbi!("vaae1is", page);
}

/// Drops every CPU's non-global entries for `page` in ASID 0, with
/// `tlbi vae1is`.
pub(crate) fn tlbi_vae1is(page: u64) {
    tlbi!("vae1is", page);
}

/// Drops every CPU's non-global entries of ASID 0, with `tlbi aside1is`.
pub(crate) fn tlbi_aside1is() {
    tlbi!("aside1is, xzr");
}

/// Drops every CPU's EL1 entries, global ones included, with
/// `tlbi vmalle1is`.
pub(crate) fn tlbi_vmalle1is() {
    tlbi!("vmalle1is");
}

/// Points `VBAR_EL1` at `table`, and makes the CPU take the next trap
/// through it.
///
/// # Safety
///
/// `table` must be a vector table, 2 KiB aligned, that every page table
/// the kernel runs on maps executable, and whose entries save and restore
/// every register they change.
pub(crate) unsafe fn write_vbar(table: *const u8) {
    // SAFETY: The caller guarantees the table.
    unsafe {
        asm!(
            "msr vbar_el1, {}",
            "isb",
            in(reg) table,
            options(nostack, preserves_flags),
        );
    }
}

/// Sets `ICC_SRE_EL1.SRE` (`S3_0_C12_C12_5`, bit 0), which switches the
/// GIC's CPU interface to its system registers if EL2 allows it.
pub(crate) fn enable_icc_sre() {
    // SAFETY: Setting SRE only switches the CPU interface to the system
    // registers that the gic module uses.
    unsafe {
        asm!(
            "mrs {tmp}, S3_0_C12_C12_5",
            "orr {tmp}, {tmp}, #1",
            "msr S3_0_C12_C12_5, {tmp}",
            "isb",
            tmp = out(reg) _,
            options(nostack, preserves_flags),
        );
    }
}

/// `ICC_SRE_EL1` (`S3_0_C12_C12_5`).
pub(crate) fn read_icc_sre() -> u64 {
    let sre: u64;
    // SAFETY: Reading ICC_SRE_EL1 changes nothing.
    unsafe {
        asm!("mrs {}, S3_0_C12_C12_5", out(reg) sre, options(nomem, nostack, preserves_flags));
    }
    sre
}

/// Writes `value` to `ICC_PMR_EL1` (`S3_0_C4_C6_0`), the priority an
/// interrupt needs above it to reach the CPU.
///
/// # Safety
///
/// The interrupts it lets through must be masked in DAIF, or have a
/// handler ready.
pub(crate) unsafe fn write_icc_pmr(value: u64) {
    // SAFETY: The caller guarantees the interrupts it lets through.
    unsafe {
        asm!("msr S3_0_C4_C6_0, {}", in(reg) value, options(nomem, nostack, preserves_flags));
    }
}

/// Writes 0 to `ICC_BPR1_EL1` (`S3_0_C12_C12_3`), so that every bit of a
/// group 1 priority decides which preempts which.
pub(crate) fn clear_icc_bpr1() {
    // SAFETY: The binary point only decides which priorities preempt each
    // other, and the kernel never nests interrupts.
    unsafe {
        asm!(
            "msr S3_0_C12_C12_3, xzr",
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Clears `ICC_CTLR_EL1.EOImode` (`S3_0_C12_C12_4`, bit 1), so that a write
/// to EOIR deactivates the interrupt too.
pub(crate) fn clear_icc_eoimode() {
    // SAFETY: EOImode only decides whether an EOIR write also deactivates,
    // which no memory depends on.
    unsafe {
        asm!(
            "mrs {tmp}, S3_0_C12_C12_4",
            "bic {tmp}, {tmp}, #2",
            "msr S3_0_C12_C12_4, {tmp}",
            tmp = out(reg) _,
            options(nostack, preserves_flags),
        );
    }
}

/// Sets `ICC_IGRPEN1_EL1` (`S3_0_C12_C12_7`), which lets group 1 interrupts
/// reach this CPU.
///
/// # Safety
///
/// They must be masked in DAIF, or have a handler ready.
pub(crate) unsafe fn enable_icc_group_1() {
    // SAFETY: The caller guarantees the interrupts it lets through.
    unsafe {
        asm!("msr S3_0_C12_C12_7, {}", in(reg) 1_u64, options(nomem, nostack, preserves_flags));
    }
}

/// Reads `ICC_IAR0_EL1` (`S3_0_C12_C8_0`), which acknowledges the
/// highest-priority pending group 0 interrupt and marks it active.
///
/// # Safety
///
/// The interrupt it returns must be ended through `write_icc_eoir0`.
pub(crate) unsafe fn read_icc_iar0() -> u64 {
    let iar: u64;
    // SAFETY: The caller ends the interrupt.
    unsafe { asm!("mrs {}, S3_0_C12_C8_0", out(reg) iar, options(nostack, preserves_flags)) };
    iar
}

/// As [`read_icc_iar0`], for group 1 with `ICC_IAR1_EL1`
/// (`S3_0_C12_C12_0`).
///
/// # Safety
///
/// The interrupt it returns must be ended through `write_icc_eoir1`.
pub(crate) unsafe fn read_icc_iar1() -> u64 {
    let iar: u64;
    // SAFETY: The caller ends the interrupt.
    unsafe { asm!("mrs {}, S3_0_C12_C12_0", out(reg) iar, options(nostack, preserves_flags)) };
    iar
}

/// Writes `intid` to `ICC_EOIR0_EL1` (`S3_0_C12_C8_1`), which ends the
/// group 0 interrupt.
///
/// # Safety
///
/// `intid` must be a group 0 interrupt that `read_icc_iar0` returned and
/// that is still active.
pub(crate) unsafe fn write_icc_eoir0(intid: u64) {
    // SAFETY: The caller guarantees the interrupt is active.
    unsafe { asm!("msr S3_0_C12_C8_1, {}", in(reg) intid, options(nostack, preserves_flags)) };
}

/// As [`write_icc_eoir0`], for group 1 with `ICC_EOIR1_EL1`
/// (`S3_0_C12_C12_1`).
///
/// # Safety
///
/// `intid` must be a group 1 interrupt that `read_icc_iar1` returned and
/// that is still active.
pub(crate) unsafe fn write_icc_eoir1(intid: u64) {
    // SAFETY: The caller guarantees the interrupt is active.
    unsafe { asm!("msr S3_0_C12_C12_1, {}", in(reg) intid, options(nostack, preserves_flags)) };
}
